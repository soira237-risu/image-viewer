//! Measures what a presenter costs: time to the first painted frame and process memory.
//!
//! ```text
//! cargo run --release -p iv-win --example backend_probe -- <bare|gdi|d2d> [image]
//! ```
//!
//! `bare` paints nothing (the floor of a Win32 window), `gdi` scales the image on the CPU and
//! blits it with `SetDIBitsToDevice`, `d2d` draws it through a Direct2D render target (which
//! loads the D3D11 driver). The window opens without taking focus and closes itself after 2 s.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use iv_core::{CancelToken, Source};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1CreateFactory,
    ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, BeginPaint, DIB_RGB_COLORS, EndPaint, FillRect, GetStockObject, HBRUSH,
    PAINTSTRUCT, SetDIBitsToDevice,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Bare,
    Gdi,
    D2d,
}

struct State {
    mode: Mode,
    started: Instant,
    /// BGRA pixels of the decoded image
    image: Option<(u32, u32, Vec<u8>)>,
    /// GDI: the image scaled to the last client size
    scaled: Option<(u32, u32, Vec<u8>)>,
    d2d: Option<(ID2D1HwndRenderTarget, ID2D1Bitmap)>,
    painted: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn memory() -> String {
    let mut pmc = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) }.expect("GetProcessMemoryInfo");
    format!(
        "working set {:.1} MB, private {:.1} MB, peak working set {:.1} MB",
        pmc.WorkingSetSize as f64 / 1048576.0,
        pmc.PagefileUsage as f64 / 1048576.0,
        pmc.PeakWorkingSetSize as f64 / 1048576.0
    )
}

/// Largest size with the image's aspect that fits `area`, never enlarged.
fn fit(img: (u32, u32), area: (u32, u32)) -> (u32, u32) {
    let s = (area.0 as f64 / img.0 as f64).min(area.1 as f64 / img.1 as f64).min(1.0);
    (((img.0 as f64 * s).round() as u32).max(1), ((img.1 as f64 * s).round() as u32).max(1))
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe { GetClientRect(hwnd, &mut r) }.expect("GetClientRect");
    ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
}

fn paint(hwnd: HWND, st: &mut State) {
    let area = client_size(hwnd);
    match st.mode {
        Mode::Bare | Mode::Gdi => unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
            FillRect(hdc, &ps.rcPaint, black);
            if let (Mode::Gdi, Some((w, h, px))) = (st.mode, &st.image) {
                let (fw, fh) = fit((*w, *h), area);
                if st.scaled.as_ref().is_none_or(|s| (s.0, s.1) != (fw, fh)) {
                    let src = ImageRef::new(*w, *h, px, PixelType::U8x4).expect("buffer size");
                    let mut dst = Image::new(fw, fh, PixelType::U8x4);
                    Resizer::new().resize(&src, &mut dst, &ResizeOptions::new()).expect("resize");
                    st.scaled = Some((fw, fh, dst.into_vec()));
                }
                let (sw, sh, spx) = st.scaled.as_ref().expect("just made");
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: *sw as i32,
                        biHeight: -(*sh as i32), // top-down rows
                        biPlanes: 1,
                        biBitCount: 32,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let (x, y) = ((area.0 - sw) / 2, (area.1 - sh) / 2);
                SetDIBitsToDevice(
                    hdc,
                    x as i32,
                    y as i32,
                    *sw,
                    *sh,
                    0,
                    0,
                    0,
                    *sh,
                    spx.as_ptr().cast(),
                    &bmi,
                    DIB_RGB_COLORS,
                );
            }
            let _ = EndPaint(hwnd, &ps);
        },
        Mode::D2d => unsafe {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            if st.d2d.is_none() {
                let factory: ID2D1Factory =
                    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).expect("D2D1CreateFactory");
                let rt = factory
                    .CreateHwndRenderTarget(
                        &D2D1_RENDER_TARGET_PROPERTIES::default(),
                        &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                            hwnd,
                            pixelSize: D2D_SIZE_U { width: area.0, height: area.1 },
                            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                        },
                    )
                    .expect("CreateHwndRenderTarget");
                let (w, h, px) = st.image.as_ref().expect("d2d needs an image");
                let bitmap = rt
                    .CreateBitmap(
                        D2D_SIZE_U { width: *w, height: *h },
                        Some(px.as_ptr().cast()),
                        w * 4,
                        &D2D1_BITMAP_PROPERTIES {
                            pixelFormat: D2D1_PIXEL_FORMAT {
                                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                alphaMode: D2D1_ALPHA_MODE_IGNORE,
                            },
                            dpiX: 96.0,
                            dpiY: 96.0,
                        },
                    )
                    .expect("CreateBitmap");
                st.d2d = Some((rt, bitmap));
            }
            let (rt, bitmap) = st.d2d.as_ref().expect("just made");
            let (w, h, _) = st.image.as_ref().expect("checked");
            let (fw, fh) = fit((*w, *h), area);
            let (x, y) = (((area.0 - fw) / 2) as f32, ((area.1 - fh) / 2) as f32);
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }));
            rt.DrawBitmap(
                bitmap,
                Some(&D2D_RECT_F { left: x, top: y, right: x + fw as f32, bottom: y + fh as f32 }),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
            rt.EndDraw(None, None).expect("EndDraw");
            let _ = EndPaint(hwnd, &ps);
        },
    }
    if !st.painted {
        st.painted = true;
        println!("first paint {:.1} ms after start; {}", st.started.elapsed().as_secs_f64() * 1000.0, memory());
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    paint(hwnd, st);
                }
            });
            LRESULT(0)
        }
        WM_TIMER => {
            println!("after 2 s: {}", memory());
            unsafe { DestroyWindow(hwnd) }.expect("DestroyWindow");
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn main() {
    let started = Instant::now();
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("bare") => Mode::Bare,
        Some("gdi") => Mode::Gdi,
        Some("d2d") => Mode::D2d,
        _ => {
            eprintln!("usage: backend_probe <bare|gdi|d2d> [image]");
            std::process::exit(2);
        }
    };
    let image = args.next().map(PathBuf::from).map(|path| {
        let d = iv_codec::default_registry()
            .decode(&Source::from_path(&path), &CancelToken::never())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut px = d.image.to_rgba8().into_owned();
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // RGBA → BGRA
        }
        println!(
            "decoded {}x{} in {:.1} ms",
            d.image.width(),
            d.image.height(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        (d.image.width(), d.image.height(), px)
    });
    if mode != Mode::Bare && image.is_none() {
        eprintln!("gdi and d2d need an image");
        std::process::exit(2);
    }
    STATE.with(|s| *s.borrow_mut() = Some(State { mode, started, image, scaled: None, d2d: None, painted: false }));

    unsafe {
        let instance = GetModuleHandleW(None).expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("iv-backend-probe"),
            hCursor: LoadCursorW(None, IDC_ARROW).expect("LoadCursorW"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("iv-backend-probe"),
            w!("backend probe"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .expect("CreateWindowExW");
        // never take focus from whatever the user is doing
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, 2000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
