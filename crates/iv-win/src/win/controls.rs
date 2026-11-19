//! Small helpers over standard Win32 controls: creation at a DPI scale with the system font,
//! text, check boxes and trackbars.

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT};
use windows::Win32::UI::Controls::{
    BST_CHECKED, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, ICC_TAB_CLASSES, INITCOMMONCONTROLSEX,
    InitCommonControlsEx, TBM_SETPAGESIZE, TBM_SETPOS, TBM_SETRANGEMAX, TBM_SETRANGEMIN,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use super::wide;

pub fn init_common_controls() {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES | ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES | ICC_TAB_CLASSES,
    };
    unsafe {
        let _ = InitCommonControlsEx(&icc);
    }
}

/// The system's message font (what dialogs use) at `dpi`.
pub fn message_font(dpi: u32) -> HFONT {
    let mut m = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    unsafe {
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            m.cbSize,
            Some((&mut m as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        );
        if ok.is_err() {
            return HFONT::default();
        }
        CreateFontIndirectW(&m.lfMessageFont)
    }
}

/// `v` in 96-DPI units at `dpi`.
pub fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

/// Lays out controls in 96-DPI units on a window at `dpi`, remembering each control's 96-DPI
/// rectangle so the window can be laid out again when its DPI changes ([`Layout`]).
pub struct Builder {
    pub parent: HWND,
    pub dpi: u32,
    pub font: HFONT,
    placed: RefCell<Vec<(HWND, [i32; 4])>>,
}

impl Builder {
    /// Controls for `parent` at `dpi`, in the system message font at that DPI.
    pub fn new(parent: HWND, dpi: u32) -> Self {
        Self { parent, dpi, font: message_font(dpi), placed: RefCell::new(Vec::new()) }
    }

    pub fn px(&self, v: i32) -> i32 {
        scale(v, self.dpi)
    }

    /// Everything added so far, for laying it out again later.
    pub fn finish(self) -> Layout {
        Layout { items: self.placed.into_inner(), font: Cell::new(self.font) }
    }

    /// A child control at (x, y, w, h) in 96-DPI units.
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        class: PCWSTR,
        text: &str,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> windows::core::Result<HWND> {
        let text = wide(text);
        let style = WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE;
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR(text.as_ptr()),
                style,
                self.px(x),
                self.px(y),
                self.px(w),
                self.px(h),
                Some(self.parent),
                Some(HMENU(id as isize as *mut _)),
                None,
                None,
            )?;
            SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            self.placed.borrow_mut().push((hwnd, [x, y, w, h]));
            Ok(hwnd)
        }
    }
}

/// The controls of a tool window with their 96-DPI rectangles, and the font they use.
pub struct Layout {
    items: Vec<(HWND, [i32; 4])>,
    font: Cell<HFONT>,
}

impl Layout {
    /// Handles `WM_DPICHANGED` for `hwnd`: moves it to the rectangle Windows suggests, then
    /// places every control and sets its font at the new DPI (Windows scales neither for plain
    /// windows). Returns the new DPI.
    pub fn dpi_changed(&self, hwnd: HWND, wp: WPARAM, lp: LPARAM) -> u32 {
        let dpi = match ((wp.0 >> 16) & 0xffff) as u32 {
            0 => 96,
            d => d,
        };
        unsafe {
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let font = message_font(dpi);
            for (h, rect) in &self.items {
                let [x, y, w, hh] = rect.map(|v| scale(v, dpi));
                let _ = MoveWindow(*h, x, y, w, hh, true);
                SendMessageW(*h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
            let old = self.font.replace(font);
            if !old.is_invalid() {
                let _ = DeleteObject(old.into());
            }
        }
        dpi
    }
}

pub fn set_text(hwnd: HWND, text: &str) {
    let w = wide(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

pub fn text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn enable(hwnd: HWND, on: bool) {
    unsafe {
        let _ = EnableWindow(hwnd, on);
    }
}

pub fn set_checked(hwnd: HWND, on: bool) {
    unsafe {
        SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(usize::from(on))), None);
    }
}

pub fn checked(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 as u32 == BST_CHECKED.0 }
}

pub fn trackbar_init(hwnd: HWND, min: i32, max: i32, page: i32) {
    unsafe {
        SendMessageW(hwnd, TBM_SETRANGEMIN, Some(WPARAM(0)), Some(LPARAM(min as isize)));
        SendMessageW(hwnd, TBM_SETRANGEMAX, Some(WPARAM(1)), Some(LPARAM(max as isize)));
        SendMessageW(hwnd, TBM_SETPAGESIZE, None, Some(LPARAM(page as isize)));
    }
}

/// Moves a trackbar without notifying its parent.
pub fn trackbar_set(hwnd: HWND, pos: i32) {
    unsafe {
        SendMessageW(hwnd, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(pos as isize)));
    }
}

pub fn trackbar_get(hwnd: HWND) -> i32 {
    // TBM_GETPOS = WM_USER
    unsafe { SendMessageW(hwnd, WM_USER, None, None).0 as i32 }
}

#[cfg(test)]
mod tests {
    use super::scale;

    #[test]
    fn layout_units_scale_with_dpi() {
        assert_eq!(scale(260, 96), 260);
        assert_eq!(scale(260, 144), 390);
        assert_eq!(scale(26, 192), 52);
        assert_eq!(scale(-10, 120), -12);
    }
}
