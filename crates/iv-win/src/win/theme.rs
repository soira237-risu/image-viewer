//! The dark look of the viewer's own windows (the settings window), matching the viewer: a dark
//! title bar, dark backgrounds, and buttons, check boxes and tab titles drawn by the program
//! (the standard ones only come light). Plain GDI and documented messages: `WM_CTLCOLOR*` for
//! labels and fields, `NM_CUSTOMDRAW` for buttons and check boxes (common controls 6, which the
//! program's manifest asks for), a subclass painting the tab strip. Combo boxes and edit fields
//! take Windows' own dark theme (`DarkMode_CFD`, what the file dialogs use); a Windows without it
//! draws them as before.
//!
//! In high contrast mode none of this applies: the system colours the user chose are used.

use std::cell::Cell;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject, HBRUSH, HDC, HGDIOBJ, NULL_BRUSH, PAINTSTRUCT,
    PS_SOLID, Polyline, RoundRect, SelectObject, SetBkColor, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::Controls::{
    BST_CHECKED, CDDS_PREPAINT, CDIS_DISABLED, CDIS_FOCUS, CDIS_HOT, CDIS_SELECTED, CDRF_DODEFAULT, CDRF_SKIPDEFAULT,
    COMBOBOXINFO, GetComboBoxInfo, NMCUSTOMDRAW, SetWindowTheme, TCIF_TEXT, TCITEMW, TCM_GETCURSEL, TCM_GETITEMCOUNT,
    TCM_GETITEMRECT, TCM_GETITEMW,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BS_AUTOCHECKBOX, BS_CHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, BS_TYPEMASK, GWL_STYLE, GetClientRect,
    GetWindowLongW, GetWindowTextW, SM_CXMENUCHECK, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SendMessageW, SystemParametersInfoW, WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC,
    WM_ERASEBKGND, WM_GETFONT, WM_PAINT,
};
use windows::core::{PWSTR, w};

use super::controls;

/// The colours, as `0x00BBGGRR` like `COLORREF`. The window is a little lighter than the
/// viewer's default background (26, 26, 28), so the two stay apart when the settings window sits
/// on top of it.
pub mod colour {
    const fn rgb(r: u8, g: u8, b: u8) -> u32 {
        r as u32 | (g as u32) << 8 | (b as u32) << 16
    }
    pub const WINDOW: u32 = rgb(32, 32, 35);
    /// Buttons, fields and the chosen tab.
    pub const SURFACE: u32 = rgb(48, 48, 52);
    pub const SURFACE_HOT: u32 = rgb(60, 60, 66);
    pub const SURFACE_PRESSED: u32 = rgb(72, 72, 79);
    pub const BORDER: u32 = rgb(82, 82, 89);
    pub const TEXT: u32 = rgb(232, 232, 232);
    /// Text of controls that are turned off, and of the tabs not chosen.
    pub const TEXT_DIM: u32 = rgb(138, 138, 144);
    /// The chosen tab's underline, a ticked box, and the frame of the focused control.
    pub const ACCENT: u32 = rgb(42, 120, 214);
}

thread_local! {
    static WINDOW_BRUSH: Cell<isize> = const { Cell::new(0) };
    static SURFACE_BRUSH: Cell<isize> = const { Cell::new(0) };
}

/// A brush of `c`, made once per thread and kept for the life of the program.
fn brush(cell: &'static std::thread::LocalKey<Cell<isize>>, c: u32) -> HBRUSH {
    cell.with(|b| {
        if b.get() == 0 {
            b.set(unsafe { CreateSolidBrush(COLORREF(c)) }.0 as isize);
        }
        HBRUSH(b.get() as *mut _)
    })
}

/// The settings window's background brush.
pub fn window_brush() -> HBRUSH {
    brush(&WINDOW_BRUSH, colour::WINDOW)
}

fn surface_brush() -> HBRUSH {
    brush(&SURFACE_BRUSH, colour::SURFACE)
}

/// `HIGHCONTRASTW` (declared here: the `Win32_UI_Accessibility` feature would bring in far more).
#[repr(C)]
struct HighContrast {
    size: u32,
    flags: u32,
    scheme: PWSTR,
}

/// Whether the dark look applies: not in high contrast mode, whose colours the user chose.
pub fn enabled() -> bool {
    let mut hc = HighContrast { size: size_of::<HighContrast>() as u32, flags: 0, scheme: PWSTR::null() };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.size,
            Some((&mut hc as *mut HighContrast).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    // HCF_HIGHCONTRASTON
    !(ok.is_ok() && hc.flags & 1 != 0)
}

/// A dark title bar, like the viewer's.
pub fn dark_title_bar(hwnd: HWND) {
    let dark: i32 = 1;
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const i32).cast(), 4);
    }
}

/// Gives a combo box (and its drop-down list) or an edit field Windows' dark theme.
pub fn dark_control(hwnd: HWND) {
    unsafe {
        let _ = SetWindowTheme(hwnd, w!("DarkMode_CFD"), None);
        let mut info = COMBOBOXINFO { cbSize: size_of::<COMBOBOXINFO>() as u32, ..Default::default() };
        if GetComboBoxInfo(hwnd, &mut info).is_ok() && !info.hwndList.is_invalid() {
            let _ = SetWindowTheme(info.hwndList, w!("DarkMode_Explorer"), None);
        }
    }
}

/// The answer to a `WM_CTLCOLOR*` message from a child: light text on the window's background
/// (labels, check boxes) or on the field colour (edit fields, drop-down lists). `None` for other
/// messages.
pub fn ctl_color(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let hdc = HDC(wp.0 as *mut _);
    let child = HWND(lp.0 as *mut _);
    let enabled = unsafe { IsWindowEnabled(child) }.as_bool();
    let text = if enabled { colour::TEXT } else { colour::TEXT_DIM };
    let (back, brush) = match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => (colour::WINDOW, window_brush()),
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => (colour::SURFACE, surface_brush()),
        _ => return None,
    };
    unsafe {
        SetTextColor(hdc, COLORREF(text));
        SetBkColor(hdc, COLORREF(back));
    }
    Some(LRESULT(brush.0 as isize))
}

/// The window text of `hwnd`.
fn text_of(hwnd: HWND) -> Vec<u16> {
    let mut buf = vec![0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    buf.truncate(n);
    buf
}

/// Draws `text` in `r` with the control's font.
fn draw_text(hdc: HDC, hwnd: HWND, text: &mut [u16], r: &mut RECT, colour: u32, centre: bool) {
    unsafe {
        let font = SendMessageW(hwnd, WM_GETFONT, None, None);
        let old = (font.0 != 0).then(|| SelectObject(hdc, HGDIOBJ(font.0 as *mut _)));
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(colour));
        let align = if centre { DT_CENTER } else { DT_LEFT };
        DrawTextW(hdc, text, r, align | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        if let Some(old) = old {
            SelectObject(hdc, old);
        }
    }
}

/// A rounded rectangle filled with `fill` (`None`: not filled), outlined in `line`.
fn rounded(hdc: HDC, r: &RECT, fill: Option<u32>, line: u32, radius: i32, width: i32) {
    unsafe {
        let pen = CreatePen(PS_SOLID, width.max(1), COLORREF(line));
        let fill_brush = fill.map(|c| CreateSolidBrush(COLORREF(c)));
        let brush = fill_brush.map_or(HGDIOBJ(GetStockObject(NULL_BRUSH).0), |b| HGDIOBJ(b.0));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let old_brush = SelectObject(hdc, brush);
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(pen.0));
        if let Some(b) = fill_brush {
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
    }
}

/// Draws a push button or a check box in the dark look, answering its `NM_CUSTOMDRAW`; other
/// buttons and other drawing stages are left to Windows (`None`).
///
/// # Safety
/// `lp` is the `NMCUSTOMDRAW` of an `NM_CUSTOMDRAW` notification.
pub unsafe fn custom_draw_button(lp: LPARAM) -> Option<LRESULT> {
    let cd = unsafe { &*(lp.0 as *const NMCUSTOMDRAW) };
    if cd.dwDrawStage != CDDS_PREPAINT {
        return Some(LRESULT(CDRF_DODEFAULT as isize));
    }
    let hwnd = cd.hdr.hwndFrom;
    let kind = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & BS_TYPEMASK;
    let state = cd.uItemState;
    let (hot, pressed, disabled, focused) = (
        state.contains(CDIS_HOT),
        state.contains(CDIS_SELECTED),
        state.contains(CDIS_DISABLED),
        state.contains(CDIS_FOCUS),
    );
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let px = |v: i32| controls::scale(v, dpi);
    let hdc = cd.hdc;
    let rc = cd.rc;
    unsafe { FillRect(hdc, &rc, window_brush()) };
    let mut text = text_of(hwnd);
    let colour = if disabled { colour::TEXT_DIM } else { colour::TEXT };
    match kind {
        k if k == BS_PUSHBUTTON || k == BS_DEFPUSHBUTTON => {
            let fill = match () {
                _ if disabled => colour::WINDOW,
                _ if pressed => colour::SURFACE_PRESSED,
                _ if hot => colour::SURFACE_HOT,
                _ => colour::SURFACE,
            };
            let line = if focused { colour::ACCENT } else { colour::BORDER };
            let inner = RECT { left: rc.left + 1, top: rc.top + 1, right: rc.right - 1, bottom: rc.bottom - 1 };
            rounded(hdc, &inner, Some(fill), line, px(6), if focused { px(2) } else { 1 });
            let mut r = inner;
            draw_text(hdc, hwnd, &mut text, &mut r, colour, true);
        }
        k if k == BS_AUTOCHECKBOX || k == BS_CHECKBOX => {
            let checked = unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None) }.0 as u32 == BST_CHECKED.0;
            // the box is as wide as Windows' own, so labels keep the room they had
            let side = unsafe { GetSystemMetricsForDpi(SM_CXMENUCHECK, dpi) }.max(px(13));
            let top = rc.top + (rc.bottom - rc.top - side) / 2;
            let bx = RECT { left: rc.left + 1, top, right: rc.left + 1 + side - 2, bottom: top + side - 2 };
            let (fill, line) = match () {
                _ if disabled => (None, colour::TEXT_DIM),
                _ if checked => (Some(colour::ACCENT), colour::ACCENT),
                _ if hot || pressed => (Some(colour::SURFACE_HOT), colour::TEXT_DIM),
                _ => (Some(colour::SURFACE), colour::BORDER),
            };
            rounded(hdc, &bx, fill, line, px(4), 1);
            if checked {
                let (w, h) = (bx.right - bx.left, bx.bottom - bx.top);
                let tick = [
                    POINT { x: bx.left + w * 22 / 100, y: bx.top + h * 52 / 100 },
                    POINT { x: bx.left + w * 42 / 100, y: bx.top + h * 72 / 100 },
                    POINT { x: bx.left + w * 78 / 100, y: bx.top + h * 30 / 100 },
                ];
                unsafe {
                    let pen = CreatePen(PS_SOLID, px(2), COLORREF(colour::TEXT));
                    let old = SelectObject(hdc, HGDIOBJ(pen.0));
                    let _ = Polyline(hdc, &tick);
                    SelectObject(hdc, old);
                    let _ = DeleteObject(HGDIOBJ(pen.0));
                }
            }
            let mut r = RECT { left: rc.left + side + px(5), ..rc };
            draw_text(hdc, hwnd, &mut text, &mut r, colour, false);
            if focused {
                // a thin accent line under the label marks the focus
                let line = RECT { left: r.left, top: rc.bottom - px(2), right: rc.right, bottom: rc.bottom - px(1) };
                let b = unsafe { CreateSolidBrush(COLORREF(colour::ACCENT)) };
                unsafe {
                    FillRect(hdc, &line, b);
                    let _ = DeleteObject(HGDIOBJ(b.0));
                }
            }
        }
        _ => return Some(LRESULT(CDRF_DODEFAULT as isize)),
    }
    Some(LRESULT(CDRF_SKIPDEFAULT as isize))
}

/// Paints a tab control's strip in the dark look from then on: every title on the window's
/// background, the chosen one on the field colour with an accent line under it.
pub fn dark_tabs(tabs: HWND) {
    unsafe {
        let _ = SetWindowSubclass(tabs, Some(tabs_proc), 1, 0);
    }
}

unsafe extern "system" fn tabs_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            paint_tabs(hwnd, hdc);
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => unsafe { DefSubclassProc(hwnd, msg, wp, lp) },
    }
}

fn paint_tabs(tabs: HWND, hdc: HDC) {
    let dpi = unsafe { GetDpiForWindow(tabs) }.max(96);
    let px = |v: i32| controls::scale(v, dpi);
    let mut client = RECT::default();
    unsafe {
        let _ = GetClientRect(tabs, &mut client);
        FillRect(hdc, &client, window_brush());
    }
    let count = unsafe { SendMessageW(tabs, TCM_GETITEMCOUNT, None, None) }.0.max(0) as usize;
    let current = unsafe { SendMessageW(tabs, TCM_GETCURSEL, None, None) }.0;
    let focused = unsafe { GetFocus() } == tabs;
    let solid = |r: &RECT, c: u32| unsafe {
        let b = CreateSolidBrush(COLORREF(c));
        FillRect(hdc, r, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    };
    // the line under the strip
    solid(&RECT { top: client.bottom - 1, ..client }, colour::BORDER);
    for i in 0..count {
        let mut r = RECT::default();
        let mut buf = vec![0u16; 128];
        let mut item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: PWSTR(buf.as_mut_ptr()),
            cchTextMax: buf.len() as i32,
            ..Default::default()
        };
        unsafe {
            SendMessageW(tabs, TCM_GETITEMRECT, Some(WPARAM(i)), Some(LPARAM(&mut r as *mut RECT as isize)));
            SendMessageW(tabs, TCM_GETITEMW, Some(WPARAM(i)), Some(LPARAM(&mut item as *mut TCITEMW as isize)));
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let chosen = i as isize == current;
        // every title takes the full height of the strip, down to the line
        let mut cell = RECT { top: client.top, bottom: client.bottom - 1, ..r };
        if chosen {
            solid(&cell, colour::SURFACE);
            let under = px(if focused { 3 } else { 2 });
            solid(&RECT { top: cell.bottom - under, ..cell }, colour::ACCENT);
        }
        let colour = if chosen { colour::TEXT } else { colour::TEXT_DIM };
        draw_text(hdc, tabs, &mut buf[..len], &mut cell, colour, true);
    }
}

#[cfg(test)]
mod tests {
    use super::colour;

    /// Relative luminance (WCAG) of a `0x00BBGGRR` colour.
    fn luminance(c: u32) -> f64 {
        let lin = |v: u32| {
            let s = v as f64 / 255.0;
            if s <= 0.039_28 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(c & 0xff) + 0.7152 * lin((c >> 8) & 0xff) + 0.0722 * lin((c >> 16) & 0xff)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (l1, l2) = (luminance(a), luminance(b));
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }

    /// Text is readable on everything it is drawn on (WCAG AA: 4.5:1 for normal text, 3:1 for
    /// the dimmed text of controls that are off and the boxes' outlines).
    #[test]
    fn colours_keep_readable_contrast() {
        for back in [colour::WINDOW, colour::SURFACE, colour::SURFACE_HOT, colour::SURFACE_PRESSED] {
            assert!(contrast(colour::TEXT, back) >= 4.5, "text on {back:06x}: {:.2}", contrast(colour::TEXT, back));
        }
        for back in [colour::WINDOW, colour::SURFACE] {
            assert!(contrast(colour::TEXT_DIM, back) >= 3.0, "dim text on {back:06x}");
        }
        assert!(contrast(colour::TEXT, colour::ACCENT) >= 3.0, "the tick on a ticked box");
        assert!(contrast(colour::ACCENT, colour::WINDOW) >= 3.0, "the accent line on the window");
        // the window stays apart from the viewer's default background behind it
        let viewer = 26 | 26 << 8 | 28 << 16;
        assert!(luminance(colour::WINDOW) > luminance(viewer));
    }
}
