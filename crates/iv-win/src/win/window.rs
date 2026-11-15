//! The main window: class, window procedure, painting and the message loop.
//!
//! The app lives in a thread-local cell. Modal dialogs it opens (file pickers, message boxes)
//! run nested message loops that call back into the window procedure while the app is
//! borrowed; those messages are answered without it (paint blits the last frame, the rest go
//! to Windows or wait in the queues the app drains afterwards).

use std::cell::RefCell;
use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, PAINTSTRUCT, SetDIBitsToDevice,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Shell::DragAcceptFiles;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use super::app::App;
use super::{controls, panel, settings_window};
use crate::keymap::Keymap;
use crate::raster::Frame;
use crate::settings::Settings;
use crate::trace::Trace;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// What the window shows; painting needs only this, so it works while the app is busy.
    static FRAME: RefCell<Frame> = RefCell::new(Frame::default());
}

pub struct Options {
    pub file: Option<PathBuf>,
    pub keymap: Keymap,
    pub settings: Settings,
    pub settings_path: Option<PathBuf>,
    pub trace: Trace,
    pub no_activate: bool,
}

/// Runs `f` on the app, then lets it drain its queues. `None` when the app is busy further up
/// the stack (inside a modal dialog) or gone.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        let app = guard.as_mut()?;
        let r = f(app);
        app.drain();
        Some(r)
    })
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        FRAME.with(|f| {
            let f = f.borrow();
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: f.width() as i32,
                    biHeight: -(f.height() as i32), // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            SetDIBitsToDevice(
                hdc,
                0,
                0,
                f.width(),
                f.height(),
                0,
                0,
                0,
                f.height(),
                f.pixels().as_ptr().cast(),
                &bmi,
                DIB_RGB_COLORS,
            );
        });
        let _ = EndPaint(hwnd, &ps);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            FRAME.with(|f| {
                if let Ok(mut frame) = f.try_borrow_mut() {
                    with_app(|app| app.render_if_dirty(&mut frame));
                }
            });
            paint(hwnd);
            LRESULT(0)
        }
        // every pixel is painted from the frame
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // the suggested rectangle keeps the window the same physical size on the new monitor
            let r = unsafe { &*(lp.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        _ => match with_app(|app| app.on_message(hwnd, msg, wp, lp)).flatten() {
            Some(r) => r,
            None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        },
    }
}

/// Hands keyboard navigation (Tab, Esc, arrow keys on controls) to the panel and settings
/// windows when a message is for them.
fn dialog_message(msg: &MSG) -> bool {
    for aux in [panel::hwnd(), settings_window::hwnd()].into_iter().flatten() {
        unsafe {
            if (msg.hwnd == aux || IsChild(aux, msg.hwnd).as_bool()) && IsDialogMessageW(aux, msg).as_bool() {
                return true;
            }
        }
    }
    false
}

/// Opens the window and runs until it closes. Returns the process exit code.
pub fn run(opts: Options) -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // the file dialogs need a single-threaded apartment on this thread
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
    controls::init_common_controls();

    let app = App::new(opts.keymap, opts.trace, opts.settings, opts.settings_path, opts.no_activate);
    APP.with(|a| *a.borrow_mut() = Some(app));
    // decoding starts now and overlaps window creation
    if let Some(file) = opts.file {
        with_app(|app| app.open(file));
    }

    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            lpszClassName: w!("iv-viewer"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("iv-viewer"),
            w!("image-viewer"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("cannot create the window: {e}");
                return 1;
            }
        }
    };
    super::set_main_window(hwnd);
    with_app(|app| {
        app.trace("window created");
        app.attach(hwnd);
    });
    unsafe {
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const i32).cast(), 4);
        DragAcceptFiles(hwnd, true);
        // with --no-activate the window never takes focus from what the user is doing
        let _ = ShowWindow(hwnd, if opts.no_activate { SW_SHOWNOACTIVATE } else { SW_SHOW });
    }
    // events that arrived before the window existed
    super::wake();

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if dialog_message(&msg) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.with(|a| a.borrow_mut().take());
    msg.wParam.0 as i32
}
