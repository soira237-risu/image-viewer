//! Key chords → commands, with user overrides from `keymap.toml`.
//!
//! ```toml
//! [bindings]
//! "Ctrl+O" = "file.open"
//! "F"      = ""            # empty string removes a default binding
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::commands::Command;

/// A key plus modifiers, normalised so equal chords compare equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Canonical key name: `"Left"`, `"F11"`, `"Esc"`, an uppercase letter, or a single symbol.
    pub key: String,
}

const NAMED: &[&str] = &[
    "Left",
    "Right",
    "Up",
    "Down",
    "Esc",
    "Enter",
    "Space",
    "Tab",
    "Backspace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "PageUp",
    "PageDown",
];

/// Canonical spelling of a key name, or `None` if it is not a key this keymap knows.
pub fn canonical_key(name: &str) -> Option<String> {
    let alias = match name.to_ascii_lowercase().as_str() {
        "escape" => "Esc",
        "return" => "Enter",
        "del" => "Delete",
        "plus" => "+",
        "minus" => "-",
        "arrowleft" => "Left",
        "arrowright" => "Right",
        "arrowup" => "Up",
        "arrowdown" => "Down",
        _ => "",
    };
    if !alias.is_empty() {
        return Some(alias.to_string());
    }
    if let Some(n) = NAMED.iter().find(|n| n.eq_ignore_ascii_case(name)) {
        return Some(n.to_string());
    }
    if let Some(num) = name.strip_prefix(['F', 'f']).and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&num).then(|| format!("F{num}"));
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_whitespace() && !c.is_control() => Some(c.to_uppercase().collect()),
        _ => None,
    }
}

impl Chord {
    /// Builds a chord from an already-canonical key. Shift is dropped for symbol keys, whose
    /// character already reflects it (Shift+= arrives as "+").
    pub fn new(key: impl Into<String>, ctrl: bool, alt: bool, shift: bool) -> Self {
        let key = key.into();
        let symbol = key.chars().count() == 1 && !key.chars().all(char::is_alphabetic);
        Self { ctrl, alt, shift: shift && !symbol, key }
    }

    /// Parses `"Ctrl+Shift+S"`, `"F11"`, `"Ctrl++"`. Modifier names are case-insensitive.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        // the key is whatever follows the last modifier; "Ctrl++" ends in a "+" key
        let mut rest = s;
        // a trailing "+" (empty tail) is the key itself, not a separator
        while let Some((head, tail)) = rest.split_once('+').filter(|(_, tail)| !tail.is_empty()) {
            match head.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                _ => return Err(format!("unknown modifier '{head}' in '{s}'")),
            }
            rest = tail;
        }
        let key = canonical_key(rest.trim()).ok_or_else(|| format!("unknown key '{rest}' in '{s}'"))?;
        Ok(Chord::new(key, ctrl, alt, shift))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        f.write_str(&self.key)
    }
}

pub struct Keymap {
    bindings: HashMap<Chord, Command>,
}

const DEFAULTS: &[(&str, Command)] = &[
    ("Ctrl+O", Command::FileOpen),
    ("Ctrl+S", Command::FileSave),
    ("Ctrl+Shift+S", Command::FileSaveAs),
    ("R", Command::EditRotateCw),
    ("Ctrl+R", Command::EditRotateCw),
    ("L", Command::EditRotateCcw),
    ("Ctrl+L", Command::EditRotateCcw),
    ("H", Command::EditFlipH),
    ("V", Command::EditFlipV),
    ("C", Command::EditCrop),
    ("Enter", Command::EditCrop),
    ("Ctrl+Z", Command::EditUndo),
    ("Ctrl+Y", Command::EditRedo),
    ("Ctrl+Shift+Z", Command::EditRedo),
    ("Right", Command::NavNext),
    ("Space", Command::NavNext),
    ("PageDown", Command::NavNext),
    ("Left", Command::NavPrev),
    ("Backspace", Command::NavPrev),
    ("PageUp", Command::NavPrev),
    ("Home", Command::NavFirst),
    ("End", Command::NavLast),
    ("F", Command::ViewFit),
    ("Ctrl+0", Command::ViewFit),
    ("1", Command::ViewActualSize),
    ("Ctrl+1", Command::ViewActualSize),
    ("+", Command::ViewZoomIn),
    ("=", Command::ViewZoomIn),
    ("Ctrl++", Command::ViewZoomIn),
    ("Ctrl+=", Command::ViewZoomIn),
    ("-", Command::ViewZoomOut),
    ("Ctrl+-", Command::ViewZoomOut),
    ("F11", Command::ViewToggleFullscreen),
    ("Alt+Enter", Command::ViewToggleFullscreen),
    ("Esc", Command::Escape),
    ("Ctrl+W", Command::AppQuit),
    ("Ctrl+Q", Command::AppQuit),
];

#[derive(serde::Deserialize)]
struct KeymapFile {
    #[serde(default)]
    bindings: HashMap<String, String>,
}

impl Keymap {
    pub fn defaults() -> Self {
        let bindings = DEFAULTS
            .iter()
            .map(|(chord, cmd)| (Chord::parse(chord).expect("default chords are valid"), *cmd))
            .collect();
        Self { bindings }
    }

    pub fn lookup(&self, chord: &Chord) -> Option<Command> {
        self.bindings.get(chord).copied()
    }

    /// Applies overrides from TOML text. Returns one warning per entry that could not be used;
    /// valid entries still apply.
    pub fn apply_toml(&mut self, text: &str) -> Result<Vec<String>, String> {
        let file: KeymapFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut warnings = Vec::new();
        let mut entries: Vec<_> = file.bindings.into_iter().collect();
        entries.sort(); // deterministic warning order
        for (chord, id) in entries {
            let chord = match Chord::parse(&chord) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(e);
                    continue;
                }
            };
            if id.is_empty() {
                self.bindings.remove(&chord);
            } else if let Some(cmd) = Command::from_id(&id) {
                self.bindings.insert(chord, cmd);
            } else {
                warnings.push(format!("unknown command '{id}' for {chord}"));
            }
        }
        Ok(warnings)
    }

    /// Defaults plus the user's file, if it exists. Problems are reported, never fatal.
    pub fn load(path: &Path) -> (Self, Vec<String>) {
        let mut map = Self::defaults();
        let warnings = match std::fs::read_to_string(path) {
            Ok(text) => match map.apply_toml(&text) {
                Ok(w) => w.into_iter().map(|w| format!("{}: {w}", path.display())).collect(),
                Err(e) => vec![format!("{}: {e}", path.display())],
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => vec![format!("{}: {e}", path.display())],
        };
        (map, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalises_chords() {
        assert_eq!(Chord::parse("ctrl+shift+s").unwrap(), Chord::new("S", true, false, true));
        assert_eq!(Chord::parse("Ctrl++").unwrap(), Chord::new("+", true, false, false));
        assert_eq!(Chord::parse("F11").unwrap().key, "F11");
        assert_eq!(Chord::parse("escape").unwrap().key, "Esc");
        assert_eq!(Chord::parse("ArrowRight").unwrap().key, "Right");
        assert_eq!(Chord::parse("Alt+Enter").unwrap().to_string(), "Alt+Enter");
        assert!(Chord::parse("Hyper+X").is_err());
        assert!(Chord::parse("F99").is_err());
        assert!(Chord::parse("NotAKey").is_err());
    }

    #[test]
    fn shift_is_implied_for_symbols_but_not_letters() {
        // Shift+= arrives as "+" with shift held; it must hit the "+" binding
        assert_eq!(Chord::new("+", false, false, true), Chord::parse("+").unwrap());
        assert_ne!(Chord::new("S", false, false, true), Chord::parse("S").unwrap());
    }

    #[test]
    fn defaults_cover_every_command() {
        let map = Keymap::defaults();
        for cmd in Command::ALL {
            assert!(map.bindings.values().any(|c| *c == cmd), "{} has no default key", cmd.id());
        }
        assert_eq!(map.lookup(&Chord::parse("Ctrl+O").unwrap()), Some(Command::FileOpen));
    }

    #[test]
    fn toml_overrides_rebind_unbind_and_warn() {
        let mut map = Keymap::defaults();
        let warnings = map
            .apply_toml(
                r#"
                [bindings]
                "Q" = "app.quit"
                "F" = ""
                "Ctrl+O" = "no.such.command"
                "Super+X" = "app.quit"
                "#,
            )
            .unwrap();
        assert_eq!(map.lookup(&Chord::parse("Q").unwrap()), Some(Command::AppQuit));
        assert_eq!(map.lookup(&Chord::parse("F").unwrap()), None);
        // a bad entry leaves the default in place
        assert_eq!(map.lookup(&Chord::parse("Ctrl+O").unwrap()), Some(Command::FileOpen));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(map.apply_toml("not toml [").is_err());
    }

    #[test]
    fn missing_file_is_not_a_warning() {
        let (map, warnings) = Keymap::load(Path::new("Z:/definitely/not/here/keymap.toml"));
        assert!(warnings.is_empty());
        assert!(map.lookup(&Chord::parse("F11").unwrap()).is_some());
    }
}
