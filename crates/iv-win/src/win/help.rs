//! The help window (F1): every command with the keys bound to it in the keymap in use, then the
//! mouse actions, in a two-column list. The contents come from [`crate::help::rows`].

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETCOLUMNW, LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVS_EX_DOUBLEBUFFER,
    LVS_EX_FULLROWSELECT, LVS_NOSORTHEADER, LVS_REPORT, LVS_SINGLESEL, WC_LISTVIEWW,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

use super::controls::{self, Builder, scale};
use super::{UiEvent, post_ui, wide};
use crate::help::Row;
use crate::i18n::{Strings, fill};

/// Client size and the action column's width, in 96-DPI units.
const WIDTH: i32 = 540;
const HEIGHT: i32 = 560;
const ACTION_COLUMN: i32 = 220;
const MARGIN: i32 = 10;
const ID_LIST: i32 = 100;
/// Esc in a window driven by `IsDialogMessageW`.
const ID_CANCEL: i32 = 2;

struct Window {
    hwnd: HWND,
    list: HWND,
    layout: controls::Layout,
}

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

pub fn hwnd() -> Option<HWND> {
    WINDOW.with(|w| w.borrow().as_ref().map(|w| w.hwnd))
}

fn create(owner: HWND, t: &Strings) -> windows::core::Result<Window> {
    unsafe {
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("iv-help"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&class);
        let dpi = GetDpiForWindow(owner).max(96);
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let ex = WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT;
        let mut r = RECT { left: 0, top: 0, right: scale(WIDTH, dpi), bottom: scale(HEIGHT, dpi) };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let mut o = RECT::default();
        let _ = GetWindowRect(owner, &mut o);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let title = wide(t.help);
        let hwnd = CreateWindowExW(
            ex,
            w!("iv-help"),
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
        let list_style = LVS_REPORT | LVS_SINGLESEL | LVS_NOSORTHEADER | WS_TABSTOP.0 | WS_BORDER.0 | WS_VSCROLL.0;
        let list =
            b.add(WC_LISTVIEWW, "", list_style, MARGIN, MARGIN, WIDTH - 2 * MARGIN, HEIGHT - 2 * MARGIN, ID_LIST)?;
        let ex_style = (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize;
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, Some(WPARAM(ex_style as usize)), Some(LPARAM(ex_style)));
        for (i, text) in [t.help_action, t.help_keys].into_iter().enumerate() {
            let mut text = wide(text);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH,
                cx: scale(ACTION_COLUMN, dpi),
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(i)), Some(LPARAM(&col as *const _ as isize)));
        }
        size_columns(list, dpi);
        Ok(Window { hwnd, list, layout: b.finish() })
    }
}

/// The action column at its design width; the keys column takes the rest.
fn size_columns(list: HWND, dpi: u32) {
    unsafe {
        let action = scale(ACTION_COLUMN, dpi);
        // the client area leaves out the vertical scroll bar, so no horizontal one appears
        let mut r = RECT::default();
        let _ = GetClientRect(list, &mut r);
        let keys = (r.right - r.left - action).max(scale(80, dpi));
        SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(0)), Some(LPARAM(action as isize)));
        SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(1)), Some(LPARAM(keys as isize)));
    }
}

/// Puts the title, column headings and `rows` into the window.
fn fill_window(w: &Window, t: &Strings, rows: &[Row]) {
    controls::set_text(w.hwnd, t.help);
    unsafe {
        for (i, text) in [t.help_action, t.help_keys].into_iter().enumerate() {
            let mut text = wide(text);
            let col = LVCOLUMNW { mask: LVCF_TEXT, pszText: PWSTR(text.as_mut_ptr()), ..Default::default() };
            SendMessageW(w.list, LVM_SETCOLUMNW, Some(WPARAM(i)), Some(LPARAM(&col as *const _ as isize)));
        }
        SendMessageW(w.list, LVM_DELETEALLITEMS, None, None);
        for (i, row) in rows.iter().enumerate() {
            let mut action = wide(&row.action);
            let mut keys = wide(&row.keys);
            let item = LVITEMW {
                mask: LVIF_TEXT,
                iItem: i as i32,
                iSubItem: 0,
                pszText: PWSTR(action.as_mut_ptr()),
                ..Default::default()
            };
            let at = SendMessageW(w.list, LVM_INSERTITEMW, None, Some(LPARAM(&item as *const _ as isize))).0;
            if at < 0 {
                continue;
            }
            let sub = LVITEMW { iSubItem: 1, pszText: PWSTR(keys.as_mut_ptr()), ..Default::default() };
            SendMessageW(w.list, LVM_SETITEMTEXTW, Some(WPARAM(at as usize)), Some(LPARAM(&sub as *const _ as isize)));
        }
        // after the rows: the keys column must leave room for the vertical scroll bar they bring
        size_columns(w.list, GetDpiForWindow(w.hwnd).max(96));
    }
}

/// Shows the window (creating it on first use) with `rows`. With `activate` false it does not
/// take focus.
pub fn show(owner: HWND, t: &Strings, rows: &[Row], activate: bool) -> Result<(), String> {
    if hwnd().is_none() {
        let w = create(owner, t).map_err(|e| fill(t.help_window_error, &[("error", &e.to_string())]))?;
        WINDOW.with(|c| *c.borrow_mut() = Some(w));
    }
    refresh(t, rows);
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
        }
    }
    Ok(())
}

/// Shows new contents (another language, a changed wheel setting) if the window exists.
pub fn refresh(t: &Strings, rows: &[Row]) {
    WINDOW.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            fill_window(w, t, rows);
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

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if (wp.0 & 0xffff) as i32 == ID_CANCEL => {
            post_ui(UiEvent::HelpClosed);
            LRESULT(0)
        }
        WM_CLOSE => {
            post_ui(UiEvent::HelpClosed);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            WINDOW.with(|w| {
                if let Some(w) = w.borrow().as_ref() {
                    let dpi = w.layout.dpi_changed(hwnd, wp, lp);
                    size_columns(w.list, dpi);
                }
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
