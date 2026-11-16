//! The help window's contents (F1): every command with the keys bound to it in the keymap in
//! use (so rebound keys show as they really are), then the mouse actions.

use crate::commands::Command;
use crate::i18n::Strings;
use crate::keymap::{Chord, Keymap};
use crate::wheel::{WheelUse, wheel_use};

/// Commands in the order the help lists them: files, browsing, view, editing, windows.
const ORDER: [Command; 27] = [
    Command::FileOpen,
    Command::FileSave,
    Command::FileSaveAs,
    Command::FileFixExtension,
    Command::NavNext,
    Command::NavPrev,
    Command::NavFirst,
    Command::NavLast,
    Command::ViewFit,
    Command::ViewActualSize,
    Command::ViewZoomIn,
    Command::ViewZoomOut,
    Command::ViewToggleFullscreen,
    Command::ViewToggleSpread,
    Command::ViewToggleDirection,
    Command::EditRotateCw,
    Command::EditRotateCcw,
    Command::EditFlipH,
    Command::EditFlipV,
    Command::EditCrop,
    Command::EditUndo,
    Command::EditRedo,
    Command::ViewToggleEditPanel,
    Command::AppSettings,
    Command::AppHelp,
    Command::Escape,
    Command::AppQuit,
];

/// One line of the help: what happens, and how to make it happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub action: String,
    pub keys: String,
}

/// The help lines for `keymap` in the language of `t`. `wheel_navigates` is the setting of the
/// same name: it decides whether the plain wheel browses or zooms.
pub fn rows(keymap: &Keymap, t: &Strings, wheel_navigates: bool) -> Vec<Row> {
    // the plain wheel's command when the setting (not the keymap) gives it one
    let plain_wheel = |up: bool| match wheel_use(keymap, up, false, false, false, wheel_navigates) {
        WheelUse::Command(c) => Some(c),
        WheelUse::Zoom => None,
    };
    let mut out: Vec<Row> = ORDER
        .iter()
        .map(|&cmd| {
            let mut keys: Vec<String> = keymap.chords_for(cmd).iter().map(Chord::to_string).collect();
            for (up, name) in [(true, "WheelUp"), (false, "WheelDown")] {
                if plain_wheel(up) == Some(cmd) && !keys.iter().any(|k| k == name) {
                    keys.push(name.to_string());
                }
            }
            Row { action: t.command(cmd).to_string(), keys: keys.join(", ") }
        })
        .collect();
    let zoom_keys = match (plain_wheel(true).or(plain_wheel(false)), wheel_use(keymap, true, true, false, false, true))
    {
        (None, _) => t.mouse_wheel,
        (Some(_), WheelUse::Zoom) => t.mouse_ctrl_wheel,
        (Some(_), WheelUse::Command(_)) => "",
    };
    let mouse = [
        (t.pan, t.mouse_drag),
        (t.select_area, t.mouse_shift_drag),
        (t.fit_or_actual, t.mouse_double_click),
        (t.zoom_at_cursor, zoom_keys),
        (t.all_commands_menu, t.mouse_right_click),
        (t.open_by_drop, t.mouse_drop),
    ];
    out.extend(mouse.into_iter().map(|(action, keys)| Row { action: action.into(), keys: keys.into() }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{EN, KO};

    fn keys_of<'a>(rows: &'a [Row], action: &str) -> &'a str {
        &rows.iter().find(|r| r.action == action).unwrap_or_else(|| panic!("no row {action}")).keys
    }

    #[test]
    fn every_command_is_listed_once_with_its_keys() {
        let mut seen = std::collections::HashSet::new();
        for cmd in ORDER {
            assert!(seen.insert(cmd), "{cmd:?} twice");
        }
        assert_eq!(seen.len(), Command::ALL.len(), "a command is missing from the help");
        let rows = rows(&Keymap::defaults(), &EN, false);
        assert_eq!(keys_of(&rows, EN.next), "Right, Space, Mouse5, PageDown");
        assert_eq!(keys_of(&rows, EN.help), "F1");
        assert_eq!(keys_of(&rows, EN.zoom_at_cursor), EN.mouse_wheel);
    }

    #[test]
    fn rebinding_and_the_wheel_setting_show_up() {
        let mut map = Keymap::defaults();
        map.apply_toml("[bindings]\n\"Q\" = \"app.quit\"\n\"F\" = \"\"\n").unwrap();
        let rows = rows(&map, &KO, true);
        assert_eq!(keys_of(&rows, KO.quit), "Q, Ctrl+Q, Ctrl+W");
        assert_eq!(keys_of(&rows, KO.fit), "Ctrl+0");
        assert_eq!(keys_of(&rows, KO.previous), "Left, Mouse4, PageUp, Backspace, WheelUp");
        assert_eq!(keys_of(&rows, KO.zoom_at_cursor), KO.mouse_ctrl_wheel);
    }
}
