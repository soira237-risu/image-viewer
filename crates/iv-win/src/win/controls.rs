//! Small helpers over standard Win32 controls: creation at a DPI scale with the system font,
//! text, check boxes and trackbars.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, HFONT};
use windows::Win32::UI::Controls::{
    BST_CHECKED, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, TBM_SETPAGESIZE,
    TBM_SETPOS, TBM_SETRANGEMAX, TBM_SETRANGEMIN,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use super::wide;

pub fn init_common_controls() {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
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

/// Lays out controls in 96-DPI units on a window at `dpi`.
pub struct Builder {
    pub parent: HWND,
    pub dpi: u32,
    pub font: HFONT,
}

impl Builder {
    pub fn px(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
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
            Ok(hwnd)
        }
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
