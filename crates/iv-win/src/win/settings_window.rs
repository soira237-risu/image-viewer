//! The settings window (F2): standard controls editing a copy of the settings. Every change is
//! reported with the whole new value; the app applies what is live and saves on request.

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, CreateSolidBrush, DeleteObject, HBRUSH, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_SUNKEN;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use super::controls::{self, Builder};
use super::{UiEvent, dialogs, post_ui};
use crate::settings::Settings;

const ID_PREFETCH: i32 = 100;
const ID_CACHE: i32 = 101;
const ID_WRAP: i32 = 102;
const ID_RTL: i32 = 103;
const ID_NEAREST: i32 = 104;
const ID_QUALITY: i32 = 105;
const ID_COLOUR: i32 = 106;
const ID_SAVE: i32 = 107;
const ID_DEFAULTS: i32 = 108;
const ID_CANCEL: i32 = 2;

const WIDTH: i32 = 340;
const HEIGHT: i32 = 300;

struct Window {
    hwnd: HWND,
    prefetch: HWND,
    cache: HWND,
    wrap: HWND,
    rtl: HWND,
    nearest: HWND,
    quality: HWND,
    swatch: HWND,
}

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static VALUE: RefCell<Settings> = RefCell::new(Settings::default());
    static SYNCING: Cell<bool> = const { Cell::new(false) };
    static SWATCH_BRUSH: Cell<isize> = const { Cell::new(0) };
}

pub fn hwnd() -> Option<HWND> {
    WINDOW.with(|w| w.borrow().as_ref().map(|w| w.hwnd))
}

fn create(owner: HWND) -> windows::core::Result<Window> {
    unsafe {
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("iv-settings"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&class);
        let dpi = GetDpiForWindow(owner).max(96);
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let ex = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
        let mut r = RECT { left: 0, top: 0, right: WIDTH * dpi as i32 / 96, bottom: HEIGHT * dpi as i32 / 96 };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let mut o = RECT::default();
        let _ = GetWindowRect(owner, &mut o);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let hwnd = CreateWindowExW(
            ex,
            w!("iv-settings"),
            w!("Settings"),
            style,
            (o.left + o.right - w) / 2,
            (o.top + o.bottom - h) / 2,
            w,
            h,
            Some(owner),
            None,
            Some(instance.into()),
            None,
        )?;
        let b = Builder { parent: hwnd, dpi, font: controls::message_font(dpi) };
        let label = |text: &str, y| b.add(w!("STATIC"), text, 0, 12, y + 3, 200, 18, 0);
        let field = |y, id, number: bool| {
            let mut style = ES_AUTOHSCROLL | WS_TABSTOP.0 as i32 | WS_BORDER.0 as i32;
            if number {
                style |= ES_NUMBER;
            }
            b.add(w!("EDIT"), "", style as u32, 220, y, 108, 24, id)
        };
        let check = |text: &str, y, id| {
            b.add(w!("BUTTON"), text, (BS_AUTOCHECKBOX | WS_TABSTOP.0 as i32) as u32, 12, y, 316, 22, id)
        };
        label("Images decoded ahead", 10)?;
        let prefetch = field(10, ID_PREFETCH, true)?;
        label("Cache memory (MiB)", 40)?;
        let cache = field(40, ID_CACHE, true)?;
        let wrap = check("Wrap around at the ends", 70, ID_WRAP)?;
        let rtl = check("Two pages read right to left", 96, ID_RTL)?;
        label("Sharp pixels above zoom (×)", 124)?;
        let nearest = field(124, ID_NEAREST, false)?;
        label("Save quality (1–100)", 154)?;
        let quality = field(154, ID_QUALITY, true)?;
        label("Background", 186)?;
        let swatch = b.add(w!("STATIC"), "", SS_SUNKEN.0, 220, 184, 40, 24, 0)?;
        let button = |text: &str, x, y, w, id| {
            b.add(w!("BUTTON"), text, (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32, x, y, w, 26, id)
        };
        button("Choose…", 266, 183, 62, ID_COLOUR)?;
        b.add(w!("STATIC"), "Cache memory applies after a restart.", 0, 12, 222, 316, 18, 0)?;
        button("Save", 136, 258, 92, ID_SAVE)?;
        button("Defaults", 236, 258, 92, ID_DEFAULTS)?;
        Ok(Window { hwnd, prefetch, cache, wrap, rtl, nearest, quality, swatch })
    }
}

fn set_swatch(colour: [u8; 3]) {
    let [r, g, b] = colour.map(u32::from);
    let brush = unsafe { CreateSolidBrush(COLORREF(r | g << 8 | b << 16)) };
    let old = SWATCH_BRUSH.with(|c| c.replace(brush.0 as isize));
    if old != 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(old as *mut _));
        }
    }
    WINDOW.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(w.swatch), None, true);
            }
        }
    });
}

/// Puts `s` into the fields without reporting it back.
fn fill(s: &Settings) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        SYNCING.with(|c| c.set(true));
        controls::set_text(w.prefetch, &s.prefetch_count.to_string());
        controls::set_text(w.cache, &s.cache_mib.to_string());
        controls::set_checked(w.wrap, s.wrap);
        controls::set_checked(w.rtl, s.spread_right_to_left);
        controls::set_text(w.nearest, &format!("{}", s.nearest_above_zoom));
        controls::set_text(w.quality, &s.save_quality.to_string());
        SYNCING.with(|c| c.set(false));
    });
    set_swatch(s.background);
}

/// Opens the window on a copy of `s`.
pub fn show(owner: HWND, s: &Settings, activate: bool) -> Result<(), String> {
    if hwnd().is_none() {
        let w = create(owner).map_err(|e| format!("cannot create the settings window: {e}"))?;
        WINDOW.with(|c| *c.borrow_mut() = Some(w));
    }
    VALUE.with(|v| *v.borrow_mut() = s.clone());
    fill(s);
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
        }
    }
    Ok(())
}

pub fn hide() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// The fields as settings; a field that does not parse keeps its previous value.
fn read(prev: &Settings) -> Settings {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return prev.clone() };
        let mut s = prev.clone();
        let num = |h: HWND| controls::text(h).trim().parse::<u64>().ok();
        if let Some(v) = num(w.prefetch) {
            s.prefetch_count = v.min(1 << 20) as usize;
        }
        if let Some(v) = num(w.cache) {
            s.cache_mib = v.min(u32::MAX as u64) as u32;
        }
        if let Some(v) = num(w.quality) {
            s.save_quality = v.min(255) as u8;
        }
        if let Ok(v) = controls::text(w.nearest).trim().parse::<f32>() {
            s.nearest_above_zoom = v;
        }
        s.wrap = controls::checked(w.wrap);
        s.spread_right_to_left = controls::checked(w.rtl);
        s
    })
}

fn changed(s: Settings) {
    VALUE.with(|v| *v.borrow_mut() = s.clone());
    post_ui(UiEvent::SettingsChanged(s));
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if !SYNCING.with(Cell::get) => {
            let (id, code) = ((wp.0 & 0xffff) as i32, ((wp.0 >> 16) & 0xffff) as u32);
            let current = VALUE.with(|v| v.borrow().clone());
            match (id, code) {
                (ID_PREFETCH | ID_CACHE | ID_NEAREST | ID_QUALITY, EN_CHANGE) | (ID_WRAP | ID_RTL, BN_CLICKED) => {
                    changed(read(&current));
                }
                (ID_COLOUR, BN_CLICKED) => {
                    if let Some(c) = dialogs::choose_color(hwnd, current.background) {
                        set_swatch(c);
                        changed(Settings { background: c, ..current });
                    }
                }
                (ID_DEFAULTS, BN_CLICKED) => {
                    let d = Settings::default();
                    fill(&d);
                    changed(d);
                }
                (ID_SAVE, BN_CLICKED) => post_ui(UiEvent::SettingsSave),
                (ID_CANCEL, _) => post_ui(UiEvent::SettingsClosed),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let is_swatch = WINDOW.with(|w| w.borrow().as_ref().is_some_and(|w| w.swatch.0 as isize == lp.0));
            let brush = SWATCH_BRUSH.with(Cell::get);
            if is_swatch && brush != 0 {
                return LRESULT(brush);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_CLOSE => {
            post_ui(UiEvent::SettingsClosed);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
