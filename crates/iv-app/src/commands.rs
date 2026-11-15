//! Every user-triggerable action, addressable by a stable string id. Key bindings, menus and
//! (later) scripts all go through [`Command`], so adding an action means adding a variant here
//! and handling it in one place.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    FileOpen,
    NavNext,
    NavPrev,
    NavFirst,
    NavLast,
    ViewFit,
    ViewActualSize,
    ViewZoomIn,
    ViewZoomOut,
    ViewToggleFullscreen,
    /// Leave full screen; when windowed, quit.
    Escape,
    AppQuit,
}

impl Command {
    pub const ALL: [Command; 12] = [
        Command::FileOpen,
        Command::NavNext,
        Command::NavPrev,
        Command::NavFirst,
        Command::NavLast,
        Command::ViewFit,
        Command::ViewActualSize,
        Command::ViewZoomIn,
        Command::ViewZoomOut,
        Command::ViewToggleFullscreen,
        Command::Escape,
        Command::AppQuit,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Command::FileOpen => "file.open",
            Command::NavNext => "nav.next",
            Command::NavPrev => "nav.prev",
            Command::NavFirst => "nav.first",
            Command::NavLast => "nav.last",
            Command::ViewFit => "view.fit",
            Command::ViewActualSize => "view.actual_size",
            Command::ViewZoomIn => "view.zoom_in",
            Command::ViewZoomOut => "view.zoom_out",
            Command::ViewToggleFullscreen => "view.toggle_fullscreen",
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
}
