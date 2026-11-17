//! Every user-triggerable action, addressable by a stable string id. Key bindings, menus and
//! (later) scripts all go through [`Command`], so adding an action means adding a variant here
//! and handling it in one place.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    FileOpen,
    FileSave,
    FileSaveAs,
    /// Rename the file so its extension matches its content (a WebP named `.jpg`).
    FileFixExtension,
    /// Move the current file to the Recycle Bin (asking first) and show the next image.
    FileDelete,
    /// Put the file itself on the clipboard, for pasting in Explorer or a chat.
    FileCopyFile,
    /// Put the full path of the current file on the clipboard as text.
    FileCopyPath,
    /// Open the folder holding the current file in Explorer, with the file selected.
    FileShowInFolder,
    /// Put the image as shown (with its edits) on the clipboard.
    EditCopyImage,
    EditRotateCw,
    EditRotateCcw,
    EditFlipH,
    EditFlipV,
    /// Crop to the selection (Shift+drag).
    EditCrop,
    EditUndo,
    EditRedo,
    NavNext,
    NavPrev,
    NavFirst,
    NavLast,
    ViewFit,
    ViewActualSize,
    ViewZoomIn,
    ViewZoomOut,
    ViewToggleFullscreen,
    /// The edit panel: sliders, resize, buttons.
    ViewToggleEditPanel,
    /// Two pages side by side (comics).
    ViewToggleSpread,
    /// Two-page reading direction: right-to-left (manga) or left-to-right.
    ViewToggleDirection,
    /// The settings window.
    AppSettings,
    /// The keyboard and mouse help window.
    AppHelp,
    /// Name, version and where the settings and logs are.
    AppAbout,
    /// Clear the selection; else leave full screen; else quit.
    Escape,
    AppQuit,
}

impl Command {
    pub const ALL: [Command; 33] = [
        Command::FileOpen,
        Command::FileSave,
        Command::FileSaveAs,
        Command::FileFixExtension,
        Command::FileDelete,
        Command::FileCopyFile,
        Command::FileCopyPath,
        Command::FileShowInFolder,
        Command::EditCopyImage,
        Command::EditRotateCw,
        Command::EditRotateCcw,
        Command::EditFlipH,
        Command::EditFlipV,
        Command::EditCrop,
        Command::EditUndo,
        Command::EditRedo,
        Command::NavNext,
        Command::NavPrev,
        Command::NavFirst,
        Command::NavLast,
        Command::ViewFit,
        Command::ViewActualSize,
        Command::ViewZoomIn,
        Command::ViewZoomOut,
        Command::ViewToggleFullscreen,
        Command::ViewToggleEditPanel,
        Command::ViewToggleSpread,
        Command::ViewToggleDirection,
        Command::AppSettings,
        Command::AppHelp,
        Command::AppAbout,
        Command::Escape,
        Command::AppQuit,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Command::FileOpen => "file.open",
            Command::FileSave => "file.save",
            Command::FileSaveAs => "file.save_as",
            Command::FileFixExtension => "file.fix_extension",
            Command::FileDelete => "file.delete",
            Command::FileCopyFile => "file.copy_file",
            Command::FileCopyPath => "file.copy_path",
            Command::FileShowInFolder => "file.show_in_folder",
            Command::EditCopyImage => "edit.copy_image",
            Command::EditRotateCw => "edit.rotate_cw",
            Command::EditRotateCcw => "edit.rotate_ccw",
            Command::EditFlipH => "edit.flip_h",
            Command::EditFlipV => "edit.flip_v",
            Command::EditCrop => "edit.crop",
            Command::EditUndo => "edit.undo",
            Command::EditRedo => "edit.redo",
            Command::NavNext => "nav.next",
            Command::NavPrev => "nav.prev",
            Command::NavFirst => "nav.first",
            Command::NavLast => "nav.last",
            Command::ViewFit => "view.fit",
            Command::ViewActualSize => "view.actual_size",
            Command::ViewZoomIn => "view.zoom_in",
            Command::ViewZoomOut => "view.zoom_out",
            Command::ViewToggleFullscreen => "view.toggle_fullscreen",
            Command::ViewToggleEditPanel => "view.toggle_edit_panel",
            Command::ViewToggleSpread => "view.toggle_spread",
            Command::ViewToggleDirection => "view.toggle_direction",
            Command::AppSettings => "app.settings",
            Command::AppHelp => "app.help",
            Command::AppAbout => "app.about",
            Command::Escape => "app.escape",
            Command::AppQuit => "app.quit",
        }
    }

    pub fn from_id(id: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|c| c.id() == id)
    }

    /// Navigation commands: holding their key flips through images quickly.
    pub fn is_navigation(self) -> bool {
        matches!(self, Command::NavNext | Command::NavPrev | Command::NavFirst | Command::NavLast)
    }

    /// Whether a held key repeats the command. Only stepping commands do: navigation, zoom,
    /// undo and redo. Everything else runs once per press, so holding Delete cannot recycle file
    /// after file, Esc cannot go on to quit, and a toggle does not flicker.
    pub fn repeats(self) -> bool {
        self.is_navigation()
            || matches!(self, Command::ViewZoomIn | Command::ViewZoomOut | Command::EditUndo | Command::EditRedo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Command::ALL {
            assert!(seen.insert(c.id()), "duplicate id {}", c.id());
            assert_eq!(Command::from_id(c.id()), Some(c));
        }
        assert_eq!(Command::from_id("nope"), None);
    }

    #[test]
    fn only_stepping_commands_repeat_while_a_key_is_held() {
        for c in [Command::NavNext, Command::NavLast, Command::ViewZoomIn, Command::ViewZoomOut, Command::EditUndo] {
            assert!(c.repeats(), "{c:?}");
        }
        for c in [
            Command::FileDelete,
            Command::FileSave,
            Command::FileFixExtension,
            Command::Escape,
            Command::AppQuit,
            Command::ViewToggleFullscreen,
            Command::EditCrop,
        ] {
            assert!(!c.repeats(), "{c:?}");
        }
    }
}
