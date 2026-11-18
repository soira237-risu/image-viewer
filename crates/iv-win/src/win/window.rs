//! The main window: class, window procedure, painting and the message loop.
//!
//! The app lives in a thread-local cell. Modal dialogs it opens (file pickers, message boxes)
//! run nested message loops that call back into the window procedure while the app is
//! borrowed; those messages are answered without it (paint blits the last frame, a new client
//! size waits for the app's next drain, a close waits until the app is free, logoff and shutdown
//! are answered from what the app published (`session`), the rest go to Windows or wait in the
//! queues the app drains afterwards). Painting renders without draining and drains only after
//! the frame is released and the window validated, so a dialog opened while draining can paint
//! safely.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, GetMonitorInfoW, MONITOR_DEFAULTTONULL,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromRect, PAINTSTRUCT, SetDIBitsToDevice,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForSystem,
    MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::Ime::{HIMC, ImmAssociateContextEx};
use windows::Win32::UI::Shell::DragAcceptFiles;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::app::App;
use super::controls::scale;
use super::{controls, help, panel, session, settings_window};
use crate::keymap::Keymap;
use crate::raster::Frame;
use crate::settings::{Settings, WindowPlacement};
use crate::trace::Trace;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// What the window shows; painting needs only this, so it works while the app is busy.
    static FRAME: RefCell<Frame> = RefCell::new(Frame::default());
}

pub struct Options {
    pub file: Option<PathBuf>,
    pub keymap: Keymap,
    /// Entries of the keymap file that could not be used (shown once in the title bar).
    pub keymap_warnings: Vec<String>,
    pub settings: Settings,
    /// Why the settings file could not be read (its defaults are in use).
    pub settings_warning: Option<String>,
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
        // a close asked for while a modal dialog had the app, now that it is free
        if session::take_deferred_close() {
            app.quit();
            app.drain();
        }
        app.publish_session();
        Some(r)
    })
}

/// Brings the frame up to date with the app, without draining its queues: draining can open a
/// modal dialog, which must not happen while the frame is borrowed and the window unpainted.
fn render(frame: &mut Frame) {
    APP.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut()
            && let Some(app) = guard.as_mut()
        {
            app.apply_pending_size();
            app.render_if_dirty(frame);
        }
    });
}

/// Blits the frame. If the frame is busy (a paint nested inside another paint), the window is
/// only validated, so the nested paint neither fails nor repeats forever.
fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        FRAME.with(|f| {
            let Ok(f) = f.try_borrow() else { return };
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
                    render(&mut frame);
                }
            });
            paint(hwnd);
            // now that the frame is free and the window painted, events may open dialogs
            with_app(|_| ());
            LRESULT(0)
        }
        WM_SIZE => {
            // minimised: keep the last size
            if wp.0 as u32 != SIZE_MINIMIZED {
                let (w, h) = ((lp.0 as u32 & 0xffff).max(1), ((lp.0 as u32 >> 16) & 0xffff).max(1));
                // applied when the app next drains: right away if it is idle, else when whatever
                // has it busy (full screen's SetWindowPos, a dialog) returns
                super::set_pending_size((w, h));
                with_app(|_| ());
            }
            LRESULT(0)
        }
        // every pixel is painted from the frame
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // never left to DefWindowProc, which would destroy the window without asking about
        // unsaved edits or keeping the placement: while the app is busy in a modal dialog
        // (another program closing the window), the close waits until it is free
        WM_CLOSE => {
            if with_app(App::quit).is_none() {
                session::defer_close();
            }
            LRESULT(0)
        }
        // answered without the app, which may be busy in a modal dialog: see `session`
        WM_QUERYENDSESSION => LRESULT(isize::from(session::query_end(lp.0 as u32))),
        WM_ENDSESSION => {
            if wp.0 != 0 {
                session::end(hwnd);
            }
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
/// windows when a message is for them; Ctrl+Tab switches the settings window's tabs.
fn dialog_message(msg: &MSG) -> bool {
    let settings = settings_window::hwnd();
    for aux in [panel::hwnd(), settings, help::hwnd()].into_iter().flatten() {
        unsafe {
            if msg.hwnd != aux && !IsChild(aux, msg.hwnd).as_bool() {
                continue;
            }
            if Some(aux) == settings && settings_window::pre_translate(msg) {
                return true;
            }
            if IsDialogMessageW(aux, msg).as_bool() {
                return true;
            }
        }
    }
    false
}

/// The default window: 1280×800 logical pixels of client area.
const DEFAULT_CLIENT: (i32, i32) = (1280, 800);

/// A window of outer size `outer`, centred in the work area `work` and no larger than it.
fn centred_in(outer: (i32, i32), work: RECT) -> (i32, i32, i32, i32) {
    let (ww, wh) = (work.right - work.left, work.bottom - work.top);
    let (w, h) = (outer.0.min(ww), outer.1.min(wh));
    (work.left + (ww - w) / 2, work.top + (wh - h) / 2, w, h)
}

/// Whether enough of `r` lies in the work area `work` to see the window and drag it by its
/// title bar (the top edge must be on screen).
fn reachable(r: RECT, work: RECT) -> bool {
    let visible_w = r.right.min(work.right) - r.left.max(work.left);
    let visible_h = r.bottom.min(work.bottom) - r.top.max(work.top);
    visible_w >= 120 && visible_h >= 60 && r.top >= work.top - 16 && r.top <= work.bottom - 60
}

/// The default window rectangle on the primary monitor, scaled for its DPI.
fn default_rect(style: WINDOW_STYLE) -> (i32, i32, i32, i32) {
    unsafe {
        let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        let (mut dpi, mut dpi_y) = (0u32, 0u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y).is_err() || dpi == 0 {
            dpi = GetDpiForSystem().max(96);
        }
        let mut r = RECT { left: 0, top: 0, right: scale(DEFAULT_CLIENT.0, dpi), bottom: scale(DEFAULT_CLIENT.1, dpi) };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, WINDOW_EX_STYLE(0), dpi);
        let outer = (r.right - r.left, r.bottom - r.top);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return (CW_USEDEFAULT, CW_USEDEFAULT, outer.0, outer.1);
        }
        centred_in(outer, mi.rcWork)
    }
}

/// The saved restored rectangle, if it still lands on a connected monitor well enough to use.
/// The rectangle stays in workspace coordinates (what `SetWindowPlacement` takes); it is checked
/// against the monitors in screen coordinates, which differ by the primary monitor's work-area
/// offset when the taskbar sits at the top or the left.
fn visible_placement(p: WindowPlacement) -> Option<RECT> {
    let r = RECT { left: p.x, top: p.y, right: p.x + p.width, bottom: p.y + p.height };
    unsafe {
        let (dx, dy) = workspace_offset();
        let screen = RECT { left: r.left + dx, top: r.top + dy, right: r.right + dx, bottom: r.bottom + dy };
        let monitor = MonitorFromRect(&screen, MONITOR_DEFAULTTONULL);
        if monitor.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return None;
        }
        reachable(screen, mi.rcWork).then_some(r)
    }
}

/// Screen position of the workspace origin: the primary monitor's work area minus the monitor.
fn workspace_offset() -> (i32, i32) {
    unsafe {
        let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(primary, &mut mi).as_bool() {
            return (0, 0);
        }
        (mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top)
    }
}

/// The program's icon (resource 1, embedded by `build.rs`) at the size of the system metrics
/// `cx` and `cy`, picked from the sizes in the file so it is sharp; Windows' generic program
/// icon when the executable has none (a test binary).
fn app_icon(instance: HINSTANCE, cx: SYSTEM_METRICS_INDEX, cy: SYSTEM_METRICS_INDEX) -> HICON {
    unsafe {
        let (w, h) = (GetSystemMetrics(cx), GetSystemMetrics(cy));
        match LoadImageW(Some(instance), PCWSTR(1 as _), IMAGE_ICON, w, h, LR_DEFAULTCOLOR) {
            Ok(icon) => HICON(icon.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

/// Opens the window and runs until it closes. Returns the process exit code.
pub fn run(opts: Options) -> i32 {
    unsafe {
        // the manifest (res/app.manifest) already sets this, and then the call fails harmlessly;
        // it still applies when the executable runs without its manifest
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // the file dialogs need a single-threaded apartment on this thread
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
    controls::init_common_controls();

    let saved_place = opts.settings_path.as_deref().and_then(|p| WindowPlacement::load(&WindowPlacement::path_for(p)));
    let language = opts.settings.language;
    let startup_window = opts.settings.startup_window;
    let mut app = App::new(opts.keymap, opts.trace, opts.settings, opts.settings_path, opts.no_activate);
    // without a console these would go unseen: the title bar says them until the first command
    app.startup_warnings(opts.settings_warning, &opts.keymap_warnings);
    APP.with(|a| *a.borrow_mut() = Some(app));
    // decoding starts now and overlaps window creation
    if let Some(file) = opts.file {
        with_app(|app| app.open(file));
    }

    // born where it was last time (so it starts at that monitor's DPI), else centred on the
    // primary monitor at 1280×800 logical pixels
    let restore = saved_place.and_then(visible_placement);
    let was_maximized = saved_place.is_some_and(|p| p.maximized) && restore.is_some();
    let start = startup_window.start_state(was_maximized, opts.no_activate);
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: app_icon(instance.into(), SM_CXICON, SM_CYICON),
            hIconSm: app_icon(instance.into(), SM_CXSMICON, SM_CYSMICON),
            lpszClassName: w!("iv-viewer"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let (x, y, w, h) = match restore {
            Some(r) => (r.left, r.top, r.right - r.left, r.bottom - r.top),
            None => default_rect(WS_OVERLAPPEDWINDOW),
        };
        match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("iv-viewer"),
            w!("image-viewer"),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                // no window to put it in the title of: say it in a box, or nothing would show
                let t = language.strings();
                let message = crate::i18n::fill(t.window_failed, &[("error", &e.to_string())]);
                crate::diag::log(&message);
                super::dialogs::show_fatal("image-viewer", &message);
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
        // No input method on the viewer itself: in Hangul mode the Korean IME would swallow
        // every single-letter shortcut (R, F, D, ...). The panel's and settings' edit fields
        // are other windows and keep theirs.
        let ime_off = ImmAssociateContextEx(hwnd, HIMC::default(), 0).as_bool();
        with_app(|app| app.trace(&format!("input method {}", if ime_off { "off" } else { "still on" })));
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const i32).cast(), 4);
        DragAcceptFiles(hwnd, true);
        // with --no-activate the window never takes focus from what the user is doing
        let show = match (opts.no_activate, start.maximized) {
            (true, _) => SW_SHOWNOACTIVATE,
            (false, true) => SW_SHOWMAXIMIZED,
            (false, false) => SW_SHOW,
        };
        match restore {
            // exact restored rectangle (workspace coordinates) and the maximised state
            Some(r) => {
                let place = WINDOWPLACEMENT {
                    length: size_of::<WINDOWPLACEMENT>() as u32,
                    showCmd: show.0 as u32,
                    rcNormalPosition: r,
                    ptMinPosition: POINT { x: -1, y: -1 },
                    ptMaxPosition: POINT { x: -1, y: -1 },
                    ..Default::default()
                };
                if SetWindowPlacement(hwnd, &place).is_err() {
                    let _ = ShowWindow(hwnd, show);
                }
            }
            None => {
                let _ = ShowWindow(hwnd, show);
            }
        }
    }
    if start.fullscreen {
        with_app(App::start_fullscreen);
    }
    // a registration left pointing at a deleted older copy is repaired, beside the first frame
    with_app(|app| app.check_registration(true));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    #[test]
    fn default_window_is_centred_and_fits_the_work_area() {
        // 1280x800 at 150 % is 1920x1200 plus the frame: too big for a 1920x1040 work area
        let work = rect(0, 0, 1920, 1040);
        assert_eq!(centred_in((1950, 1250), work), (0, 0, 1920, 1040));
        // fits on a 4K monitor at 100 %: centred
        assert_eq!(centred_in((1300, 850), rect(0, 0, 3840, 2100)), (1270, 625, 1300, 850));
        // a secondary monitor left of the primary
        assert_eq!(centred_in((1000, 600), rect(-1920, 0, 0, 1080)), (-1460, 240, 1000, 600));
    }

    #[test]
    fn saved_placement_must_be_reachable() {
        let work = rect(0, 0, 1920, 1040);
        assert!(reachable(rect(100, 100, 1400, 900), work));
        assert!(!reachable(rect(1900, 100, 3000, 900), work), "only 20 px on screen");
        assert!(!reachable(rect(100, -400, 1400, 30), work), "title bar above the screen");
        assert!(!reachable(rect(100, 1020, 1400, 1800), work), "title bar under the taskbar");
    }
}
