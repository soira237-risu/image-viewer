//! The right-click menu: every command, with its shortcut, as a native popup menu.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};
use windows::core::PCWSTR;

use super::wide;
use crate::commands::Command;
use crate::keymap::Keymap;

/// Menu layout: commands in groups; `None` is a separator.
const ITEMS: &[Option<(Command, &str)>] = &[
    Some((Command::FileOpen, "Open…")),
    Some((Command::FileSave, "Save")),
    Some((Command::FileSaveAs, "Save as…")),
    None,
    Some((Command::EditRotateCw, "Rotate right")),
    Some((Command::EditRotateCcw, "Rotate left")),
    Some((Command::EditFlipH, "Flip horizontally")),
    Some((Command::EditFlipV, "Flip vertically")),
    Some((Command::EditCrop, "Crop to selection")),
    Some((Command::EditUndo, "Undo")),
    Some((Command::EditRedo, "Redo")),
    None,
    Some((Command::ViewFit, "Fit to window")),
    Some((Command::ViewActualSize, "Actual size")),
    Some((Command::ViewZoomIn, "Zoom in")),
    Some((Command::ViewZoomOut, "Zoom out")),
    Some((Command::ViewToggleFullscreen, "Full screen")),
    Some((Command::ViewToggleSpread, "Two pages")),
    Some((Command::ViewToggleDirection, "Right to left")),
    None,
    Some((Command::ViewToggleEditPanel, "Edit panel")),
    Some((Command::AppSettings, "Settings")),
    None,
    Some((Command::AppQuit, "Quit")),
];

/// What the menu shows as checked or unavailable.
pub struct MenuState {
    pub fullscreen: bool,
    pub spread: Option<bool>,
    pub panel_open: bool,
    pub settings_open: bool,
    pub has_image: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub edited: bool,
    pub has_selection: bool,
}

/// Menu id of a command (ids start at 1; 0 means "nothing chosen").
fn id_of(cmd: Command) -> usize {
    Command::ALL.iter().position(|c| *c == cmd).expect("every command is in ALL") + 1
}

fn flags_for(cmd: Command, s: &MenuState) -> MENU_ITEM_FLAGS {
    let checked = match cmd {
        Command::ViewToggleFullscreen => s.fullscreen,
        Command::ViewToggleSpread => s.spread.is_some(),
        Command::ViewToggleDirection => s.spread == Some(true),
        Command::ViewToggleEditPanel => s.panel_open,
        Command::AppSettings => s.settings_open,
        _ => false,
    };
    let enabled = match cmd {
        Command::FileSave => s.edited,
        Command::FileSaveAs
        | Command::EditRotateCw
        | Command::EditRotateCcw
        | Command::EditFlipH
        | Command::EditFlipV
        | Command::ViewFit
        | Command::ViewActualSize
        | Command::ViewZoomIn
        | Command::ViewZoomOut => s.has_image,
        Command::EditCrop => s.has_selection,
        Command::EditUndo => s.can_undo,
        Command::EditRedo => s.can_redo,
        Command::ViewToggleDirection => s.spread.is_some(),
        _ => true,
    };
    let mut f = MF_STRING;
    if checked {
        f |= MF_CHECKED;
    }
    if !enabled {
        f |= MF_GRAYED;
    }
    f
}

/// Shows the menu at screen point `at` and returns the chosen command.
pub fn show(owner: HWND, at: (i32, i32), keymap: &Keymap, state: &MenuState) -> Option<Command> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for item in ITEMS {
            match item {
                Some((cmd, label)) => {
                    let text = match keymap.chord_for(*cmd) {
                        Some(chord) => format!("{label}\t{chord}"),
                        None => (*label).to_string(),
                    };
                    let w = wide(&text);
                    let _ = AppendMenuW(menu, flags_for(*cmd, state), id_of(*cmd), PCWSTR(w.as_ptr()));
                }
                None => {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
            }
        }
        // without this the menu does not close when clicking elsewhere
        let _ = SetForegroundWindow(owner);
        let chosen = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, at.0, at.1, owner, None).0 as usize;
        let _ = DestroyMenu(menu);
        Command::ALL.get(chosen.checked_sub(1)?).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_map_back_to_commands_and_every_command_but_escape_is_listed() {
        for cmd in Command::ALL {
            assert_eq!(Command::ALL[id_of(cmd) - 1], cmd);
        }
        let listed: Vec<Command> = ITEMS.iter().flatten().map(|(c, _)| *c).collect();
        for cmd in Command::ALL {
            let expected = !matches!(
                cmd,
                Command::Escape | Command::NavNext | Command::NavPrev | Command::NavFirst | Command::NavLast
            );
            assert_eq!(listed.contains(&cmd), expected, "{cmd:?}");
        }
    }
}
