//! Logoff, shutdown and closing while the app is busy.
//!
//! At logoff or shutdown Windows asks every window whether the session may end
//! (`WM_QUERYENDSESSION`), then says that it does (`WM_ENDSESSION`); after that the process can
//! be ended at any moment, without `WM_CLOSE` and without unwinding (so `App`'s `Drop` never
//! runs). These messages, and a `WM_CLOSE` from another program, can arrive while the app is
//! borrowed by a modal dialog, so the window procedure answers them from what is kept here: what
//! would be lost (published by the app after everything it handles), the save thread, and where
//! the window placement goes.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::thread::JoinHandle;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::UI::WindowsAndMessaging::{
    ENDSESSION_CRITICAL, GetWindowPlacement, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, WINDOWPLACEMENT,
    WPF_RESTORETOMAXIMIZED,
};
use windows::core::PCWSTR;

use super::wide;
use crate::settings::WindowPlacement;

/// What ending the session now would lose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stake {
    Nothing,
    /// A save is running; it finishes in a moment.
    Saving,
    /// The current image has edits that are not saved.
    Unsaved,
}

/// Whether the session may end now (the answer to `WM_QUERYENDSESSION`, whose flags are
/// `flags`). A running save does not hold it: `WM_ENDSESSION` waits for the save to finish. Unsaved
/// edits hold an ordinary logoff or shutdown: Windows then lists the viewer with the reason given
/// to [`publish`], and the user either cancels and goes back to save them or ends the session
/// anyway. A critical shutdown cannot be held.
pub fn may_end(stake: Stake, flags: u32) -> bool {
    stake != Stake::Unsaved || flags & ENDSESSION_CRITICAL != 0
}

thread_local! {
    /// What the app last published, and the reason registered with Windows for it.
    static STAKE: RefCell<(Stake, String)> = const { RefCell::new((Stake::Nothing, String::new())) };
    static SAVE_THREAD: RefCell<Option<JoinHandle<()>>> = const { RefCell::new(None) };
    static CLOSE_DEFERRED: Cell<bool> = const { Cell::new(false) };
    /// The window state file, and the placement from before full screen while it is on.
    static PLACEMENT: RefCell<(Option<PathBuf>, Option<WINDOWPLACEMENT>)> = const { RefCell::new((None, None)) };
}

/// What ending the session would lose now, and why in words (shown by Windows on its "these apps
/// are preventing shutdown" screen). Registers or withdraws the reason only when it changed.
pub fn publish(hwnd: HWND, stake: Stake, reason: String) {
    STAKE.with(|s| {
        let mut s = s.borrow_mut();
        if s.0 == stake && s.1 == reason {
            return;
        }
        unsafe {
            if stake == Stake::Nothing {
                let _ = ShutdownBlockReasonDestroy(hwnd);
            } else {
                let w = wide(&reason);
                let _ = ShutdownBlockReasonCreate(hwnd, PCWSTR(w.as_ptr()));
            }
        }
        *s = (stake, reason);
    });
}

/// `WM_QUERYENDSESSION`: see [`may_end`].
pub fn query_end(flags: u32) -> bool {
    STAKE.with(|s| may_end(s.borrow().0, flags))
}

/// `WM_ENDSESSION` for a session that does end: keeps the window placement and lets a running
/// save finish, before Windows ends the process. No dialog may be shown here.
pub fn end(hwnd: HWND) {
    if let Some(Err(e)) = save_placement(hwnd) {
        crate::diag::log(&format!("window placement not saved: {e}"));
    }
    finish_save();
}

/// The save running on `thread`, to be waited for by [`finish_save`].
pub fn set_save_thread(thread: JoinHandle<()>) {
    SAVE_THREAD.with(|t| *t.borrow_mut() = Some(thread));
}

/// Waits for the running save, if any, to finish.
pub fn finish_save() {
    let thread = SAVE_THREAD.try_with(|t| t.borrow_mut().take()).ok().flatten();
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

/// A `WM_CLOSE` arrived while the app was busy in a modal dialog: it closes the window (through
/// the app's own quit, which asks about unsaved edits) once the app is free again.
pub fn defer_close() {
    CLOSE_DEFERRED.with(|c| c.set(true));
}

/// Takes a close request deferred by [`defer_close`].
pub fn take_deferred_close() -> bool {
    CLOSE_DEFERRED.with(Cell::take)
}

/// Where [`save_placement`] writes: next to the settings file (`None`: nowhere).
pub fn set_placement_file(settings_path: Option<PathBuf>) {
    PLACEMENT.with(|p| p.borrow_mut().0 = settings_path.map(|s| WindowPlacement::path_for(&s)));
}

/// Full screen is on (`Some`, with the placement from before it) or off.
pub fn set_fullscreen_placement(before: Option<WINDOWPLACEMENT>) {
    PLACEMENT.with(|p| p.borrow_mut().1 = before);
}

/// Writes where the window is (its restored rectangle and whether it is maximised) to the window
/// state file, so the next start opens it there. Full screen is not remembered: the placement
/// from before it is. `None` when there is no file to write to or no placement to read.
pub fn save_placement(hwnd: HWND) -> Option<Result<WindowPlacement, String>> {
    let (file, fullscreen) = PLACEMENT.with(|p| p.borrow().clone());
    let file = file?;
    let placement = match fullscreen {
        Some(p) => p,
        None => {
            let mut p = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            unsafe { GetWindowPlacement(hwnd, &mut p) }.ok()?;
            p
        }
    };
    let r = placement.rcNormalPosition;
    let maximized = placement.showCmd == SW_SHOWMAXIMIZED.0 as u32
        || placement.showCmd == SW_SHOWMINIMIZED.0 as u32 && placement.flags.contains(WPF_RESTORETOMAXIMIZED);
    let saved = WindowPlacement { x: r.left, y: r.top, width: r.right - r.left, height: r.bottom - r.top, maximized };
    Some(saved.save(&file).map(|()| saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{ENDSESSION_CLOSEAPP, ENDSESSION_LOGOFF};

    #[test]
    fn unsaved_edits_hold_the_session_unless_it_must_end() {
        for flags in [0, ENDSESSION_LOGOFF, ENDSESSION_CLOSEAPP] {
            assert!(!may_end(Stake::Unsaved, flags), "{flags:#x}");
            assert!(may_end(Stake::Nothing, flags), "{flags:#x}");
            // the save is waited for at WM_ENDSESSION instead
            assert!(may_end(Stake::Saving, flags), "{flags:#x}");
        }
        assert!(may_end(Stake::Unsaved, ENDSESSION_CRITICAL));
        assert!(may_end(Stake::Unsaved, ENDSESSION_CRITICAL | ENDSESSION_LOGOFF));
    }

    #[test]
    fn a_deferred_close_is_taken_once() {
        assert!(!take_deferred_close());
        defer_close();
        assert!(take_deferred_close());
        assert!(!take_deferred_close());
    }

    #[test]
    fn finishing_waits_for_the_save_thread() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        set_save_thread(std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            flag.store(true, std::sync::atomic::Ordering::Release);
        }));
        finish_save();
        assert!(done.load(std::sync::atomic::Ordering::Acquire));
        finish_save(); // nothing left to wait for
    }
}
