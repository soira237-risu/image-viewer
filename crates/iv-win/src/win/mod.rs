//! The Win32 shell: window, input, dialogs and the native edit panel and settings windows.

pub mod app;
pub mod controls;
pub mod dialogs;
pub mod help;
pub mod keys;
pub mod menu;
pub mod panel;
pub mod settings_window;
pub mod window;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::panel::PanelAction;
use crate::settings::Settings;

/// Posted to the main window when something is waiting in its queues.
pub const WM_APP_WAKE: u32 = WM_APP + 1;

/// The main window, for threads that need to wake it (0 until it exists).
static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

pub fn set_main_window(hwnd: HWND) {
    MAIN_WINDOW.store(hwnd.0 as isize, Ordering::Release);
}

pub fn main_window() -> Option<HWND> {
    let h = MAIN_WINDOW.load(Ordering::Acquire);
    (h != 0).then_some(HWND(h as *mut _))
}

/// Asks the main window to drain its queues. Safe from any thread.
pub fn wake() {
    if let Some(hwnd) = main_window() {
        // a full queue only drops this wake; a later one picks the events up
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0)) };
    }
}

/// What the panel and settings windows report to the app (UI thread only).
#[derive(Debug)]
pub enum UiEvent {
    Panel(PanelAction),
    SettingsChanged(Settings),
    SettingsSave,
    SettingsClosed,
    HelpClosed,
}

thread_local! {
    static INBOX: RefCell<VecDeque<UiEvent>> = const { RefCell::new(VecDeque::new()) };
}

/// Queues `e` for the app and wakes it. Used from the panel's and settings window's window
/// procedures, which must not touch the app directly: it may be busy in a modal dialog.
pub fn post_ui(e: UiEvent) {
    INBOX.with(|q| q.borrow_mut().push_back(e));
    wake();
}

pub fn take_ui() -> Option<UiEvent> {
    INBOX.with(|q| q.borrow_mut().pop_front())
}

thread_local! {
    static PENDING_SIZE: Cell<Option<(u32, u32)>> = const { Cell::new(None) };
}

/// Records the main window's new client size. `WM_SIZE` often arrives while the app is busy
/// further up the stack (it is sent synchronously by the `SetWindowPos` that full screen does, or
/// during a modal dialog), so the size waits here and the app applies it when it next drains.
pub fn set_pending_size(size: (u32, u32)) {
    PENDING_SIZE.with(|s| s.set(Some(size)));
}

pub fn take_pending_size() -> Option<(u32, u32)> {
    PENDING_SIZE.with(Cell::take)
}

/// UTF-16, NUL-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Low and high 16-bit words of an LPARAM as signed coordinates.
pub fn point_from_lparam(lp: LPARAM) -> (i32, i32) {
    let v = lp.0 as u32;
    ((v & 0xffff) as i16 as i32, (v >> 16) as i16 as i32)
}
