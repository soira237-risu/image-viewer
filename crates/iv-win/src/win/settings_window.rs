//! The settings window (F2): standard controls editing a copy of the settings. Every change is
//! reported with the whole new value; the app applies what is live and saves on request.

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, CreateSolidBrush, DeleteObject, HBRUSH, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_SUNKEN;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::controls::{self, Builder};
use super::{UiEvent, dialogs, post_ui, wide};
use crate::i18n::{Language, Strings};
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
const ID_LANGUAGE: i32 = 109;
const ID_WHEEL: i32 = 110;
const ID_CANCEL: i32 = 2;

const WIDTH: i32 = 360;
const HEIGHT: i32 = 360;

/// A label in the current language, so controls can be relabelled when it changes.
type Text = fn(&Strings) -> &'static str;

struct Window {
    hwnd: HWND,
    language: HWND,
    prefetch: HWND,
    cache: HWND,
    wrap: HWND,
    rtl: HWND,
    wheel: HWND,
    nearest: HWND,
    quality: HWND,
    swatch: HWND,
    /// Controls showing translated text.
    texts: Vec<(HWND, Text)>,
    /// Every control, for laying the window out again on a DPI change.
    layout: controls::Layout,
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

fn create(owner: HWND, s: &Strings) -> windows::core::Result<Window> {
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
        let title = wide(s.settings);
        let hwnd = CreateWindowExW(
            ex,
            w!("iv-settings"),
            PCWSTR(title.as_ptr()),
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
        let b = Builder::new(hwnd, dpi);
        let texts = RefCell::new(Vec::<(HWND, Text)>::new());
        let keep = |h: HWND, text: Text| {
            texts.borrow_mut().push((h, text));
            h
        };
        let label = |text: Text, y| -> windows::core::Result<HWND> {
            Ok(keep(b.add(w!("STATIC"), text(s), 0, 12, y + 3, 220, 18, 0)?, text))
        };
        let field = |y, id, number: bool| {
            let mut style = ES_AUTOHSCROLL | WS_TABSTOP.0 as i32 | WS_BORDER.0 as i32;
            if number {
                style |= ES_NUMBER;
            }
            b.add(w!("EDIT"), "", style as u32, 240, y, 108, 24, id)
        };
        let check = |text: Text, y, id| -> windows::core::Result<HWND> {
            let style = (BS_AUTOCHECKBOX | WS_TABSTOP.0 as i32) as u32;
            Ok(keep(b.add(w!("BUTTON"), text(s), style, 12, y, 336, 22, id)?, text))
        };
        let button = |text: Text, x, y, w, id| -> windows::core::Result<HWND> {
            let style = (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32;
            Ok(keep(b.add(w!("BUTTON"), text(s), style, x, y, w, 26, id)?, text))
        };
        label(|s| s.language, 10)?;
        let combo_style = (CBS_DROPDOWNLIST | WS_TABSTOP.0 as i32 | WS_VSCROLL.0 as i32) as u32;
        let language = b.add(w!("COMBOBOX"), "", combo_style, 240, 10, 108, 120, ID_LANGUAGE)?;
        for l in Language::ALL {
            let name = wide(l.native_name());
            SendMessageW(language, CB_ADDSTRING, None, Some(LPARAM(name.as_ptr() as isize)));
        }
        label(|s| s.prefetch_count, 44)?;
        let prefetch = field(44, ID_PREFETCH, true)?;
        label(|s| s.cache_memory, 74)?;
        let cache = field(74, ID_CACHE, true)?;
        let wrap = check(|s| s.wrap, 104, ID_WRAP)?;
        let rtl = check(|s| s.spread_rtl, 130, ID_RTL)?;
        let wheel = check(|s| s.wheel_navigates, 156, ID_WHEEL)?;
        label(|s| s.sharp_pixels, 184)?;
        let nearest = field(184, ID_NEAREST, false)?;
        label(|s| s.save_quality_range, 214)?;
        let quality = field(214, ID_QUALITY, true)?;
        label(|s| s.background, 246)?;
        let swatch = b.add(w!("STATIC"), "", SS_SUNKEN.0, 240, 244, 40, 24, 0)?;
        button(|s| s.choose, 286, 243, 62, ID_COLOUR)?;
        keep(b.add(w!("STATIC"), s.cache_note, 0, 12, 282, 336, 18, 0)?, |s| s.cache_note);
        button(|s| s.save, 156, 318, 92, ID_SAVE)?;
        button(|s| s.defaults, 256, 318, 92, ID_DEFAULTS)?;
        let texts = texts.into_inner();
        let layout = b.finish();
        Ok(Window { hwnd, language, prefetch, cache, wrap, rtl, wheel, nearest, quality, swatch, texts, layout })
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
        controls::set_checked(w.wheel, s.wheel_navigates);
        controls::set_text(w.nearest, &format!("{}", s.nearest_above_zoom));
        controls::set_text(w.quality, &s.save_quality.to_string());
        let index = Language::ALL.iter().position(|l| *l == s.language).unwrap_or(0);
        unsafe {
            SendMessageW(w.language, CB_SETCURSEL, Some(WPARAM(index)), None);
        }
        SYNCING.with(|c| c.set(false));
    });
    set_swatch(s.background);
}

/// Opens the window on a copy of `s`.
pub fn show(owner: HWND, s: &Settings, activate: bool) -> Result<(), String> {
    if hwnd().is_none() {
        let t = s.language.strings();
        let w =
            create(owner, t).map_err(|e| crate::i18n::fill(t.settings_window_error, &[("error", &e.to_string())]))?;
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

/// Relabels the window (if it exists) in another language.
pub fn set_language(s: &Strings) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        controls::set_text(w.hwnd, s.settings);
        for (h, text) in &w.texts {
            controls::set_text(*h, text(s));
        }
    });
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
        s.wheel_navigates = controls::checked(w.wheel);
        let index = unsafe { SendMessageW(w.language, CB_GETCURSEL, None, None).0 };
        if let Some(l) = usize::try_from(index).ok().and_then(|i| Language::ALL.get(i)) {
            s.language = *l;
        }
        s
    })
}

/// Reports `s`. The app uses it clamped into range, so the window keeps the clamped value too.
fn changed(s: Settings) {
    let s = s.clamped();
    VALUE.with(|v| *v.borrow_mut() = s.clone());
    post_ui(UiEvent::SettingsChanged(s));
}

/// When the user leaves a number field: shows the value in effect if the field says otherwise
/// (a number out of range was clamped, text that is not a number was ignored), so the window
/// never shows a value the app is not using. Not while typing: "1024" passes through "1".
fn refill_field(id: i32) {
    let v = VALUE.with(|v| v.borrow().clone());
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        let int =
            |h: HWND, value: u64| (h, controls::text(h).trim().parse::<u64>().ok() == Some(value), value.to_string());
        let (field, shows_value, text) = match id {
            ID_PREFETCH => int(w.prefetch, v.prefetch_count as u64),
            ID_CACHE => int(w.cache, u64::from(v.cache_mib)),
            ID_QUALITY => int(w.quality, u64::from(v.save_quality)),
            ID_NEAREST => {
                let zoom = v.nearest_above_zoom;
                let same = controls::text(w.nearest).trim().parse::<f32>().ok() == Some(zoom);
                (w.nearest, same, format!("{zoom}"))
            }
            _ => return,
        };
        if !shows_value {
            SYNCING.with(|c| c.set(true));
            controls::set_text(field, &text);
            SYNCING.with(|c| c.set(false));
        }
    });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if !SYNCING.with(Cell::get) => {
            let (id, code) = ((wp.0 & 0xffff) as i32, ((wp.0 >> 16) & 0xffff) as u32);
            let current = VALUE.with(|v| v.borrow().clone());
            match (id, code) {
                (ID_PREFETCH | ID_CACHE | ID_NEAREST | ID_QUALITY, EN_CHANGE)
                | (ID_WRAP | ID_RTL | ID_WHEEL, BN_CLICKED)
                | (ID_LANGUAGE, CBN_SELCHANGE) => {
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
                (ID_PREFETCH | ID_CACHE | ID_NEAREST | ID_QUALITY, EN_KILLFOCUS) => refill_field(id),
                (ID_SAVE, BN_CLICKED) => {
                    // what is written is what the window shows
                    fill(&current);
                    post_ui(UiEvent::SettingsSave);
                }
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
        WM_DPICHANGED => {
            WINDOW.with(|w| {
                if let Some(w) = w.borrow().as_ref() {
                    SYNCING.with(|c| c.set(true));
                    w.layout.dpi_changed(hwnd, wp, lp);
                    SYNCING.with(|c| c.set(false));
                }
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
