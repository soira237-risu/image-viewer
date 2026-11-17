//! The right-click menu: every command, with its shortcut, as a native popup menu.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};
use windows::core::PCWSTR;

use super::wide;
use crate::commands::Command;
use crate::i18n::Strings;
use crate::keymap::Keymap;

/// Menu layout: commands in groups; `None` is a separator.
const ITEMS: &[Option<Command>] = &[
    Some(Command::FileOpen),
    Some(Command::FileSave),
    Some(Command::FileSaveAs),
    Some(Command::FileFixExtension),
    None,
    Some(Command::EditCopyImage),
    Some(Command::FileCopyFile),
    Some(Command::FileCopyPath),
    Some(Command::FileShowInFolder),
    Some(Command::FileDelete),
    None,
    Some(Command::NavNext),
    Some(Command::NavPrev),
    Some(Command::NavFirst),
    Some(Command::NavLast),
    None,
    Some(Command::EditRotateCw),
    Some(Command::EditRotateCcw),
    Some(Command::EditFlipH),
    Some(Command::EditFlipV),
    Some(Command::EditCrop),
    Some(Command::EditUndo),
    Some(Command::EditRedo),
    None,
    Some(Command::ViewFit),
    Some(Command::ViewActualSize),
    Some(Command::ViewZoomIn),
    Some(Command::ViewZoomOut),
    Some(Command::ViewToggleFullscreen),
    Some(Command::ViewToggleSpread),
    Some(Command::ViewToggleDirection),
    None,
    Some(Command::ViewToggleEditPanel),
    Some(Command::AppSettings),
    Some(Command::AppHelp),
    Some(Command::AppAbout),
    None,
    Some(Command::AppQuit),
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
    /// A folder or archive with more than one image is open.
    pub can_navigate: bool,
    /// An image is current (its path can be copied or shown in Explorer).
    pub has_path: bool,
    /// The current image is a file of its own on disk, not a page inside an archive (it can be
    /// deleted or copied as a file).
    pub on_disk: bool,
    /// The current image is fully decoded (it can be copied as pixels).
    pub loaded: bool,
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
        | Command::FileFixExtension
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
        Command::NavNext | Command::NavPrev | Command::NavFirst | Command::NavLast => s.can_navigate,
        Command::FileCopyPath | Command::FileShowInFolder => s.has_path,
        Command::FileDelete | Command::FileCopyFile => s.on_disk,
        Command::EditCopyImage => s.loaded,
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
pub fn show(owner: HWND, at: (i32, i32), keymap: &Keymap, strings: &Strings, state: &MenuState) -> Option<Command> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for item in ITEMS {
            match item {
                Some(cmd) => {
                    let label = strings.command(*cmd);
                    let text = match keymap.chord_for(*cmd) {
                        Some(chord) => format!("{label}\t{chord}"),
                        None => label.to_string(),
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
    fn ids_map_back_to_commands_and_every_command_but_escape_is_listed_once() {
        for cmd in Command::ALL {
            assert_eq!(Command::ALL[id_of(cmd) - 1], cmd);
        }
        let listed: Vec<Command> = ITEMS.iter().flatten().copied().collect();
        let unique: std::collections::HashSet<_> = listed.iter().collect();
        assert_eq!(unique.len(), listed.len(), "a command is listed twice");
        for cmd in Command::ALL {
            let expected = cmd != Command::Escape;
            assert_eq!(listed.contains(&cmd), expected, "{cmd:?}");
        }
    }
}
