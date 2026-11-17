//! User settings, kept in `settings.toml` next to `keymap.toml`. Every field has a default, so a
//! missing or partial file works, and out-of-range values are clamped rather than rejected.
//!
//! `settings.toml` belongs to the user: only the settings window's Save writes it. Where the main
//! window was when it closed is program state and lives in its own `window.toml` beside it, so
//! closing the window never creates or rewrites the settings file.

use std::path::{Path, PathBuf};

use crate::i18n::Language;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Neighbours decoded ahead (doubled in two-page view).
    pub prefetch_count: usize,
    /// Memory for decoded images, in MiB.
    pub cache_mib: u32,
    /// Stepping past the last image continues at the first.
    pub wrap: bool,
    /// Background around the image, sRGB.
    pub background: [u8; 3],
    /// Two-page view reads right to left (manga) when turned on.
    pub spread_right_to_left: bool,
    /// Above this zoom, pixels are drawn as sharp squares.
    pub nearest_above_zoom: f32,
    /// JPEG / WebP / AVIF quality for Save and Save As.
    pub save_quality: u8,
    /// Language of menus, windows and messages (`"ko"` or `"en"`).
    pub language: Language,
    /// The plain mouse wheel moves to the previous / next image (Ctrl+wheel zooms); off: the
    /// wheel zooms at the cursor.
    pub wheel_navigates: bool,
    /// Ask before moving a file to the Recycle Bin (Delete). Unsaved edits are always asked about.
    pub confirm_delete: bool,
}

/// The main window's restored (not maximised, not full-screen) rectangle in workspace
/// coordinates, as `GetWindowPlacement` reports it, and whether it was maximised. Saved in
/// `window.toml` next to the settings file when the window closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl WindowPlacement {
    /// Smallest restored size worth bringing back; anything smaller is treated as broken.
    pub const MIN_SIZE: i32 = 120;

    /// First line of the state file, so whoever opens it knows where the settings are.
    const HEADER: &str = "# Written by image-viewer when its window closes. Settings are in settings.toml.\n";

    fn plausible(&self) -> bool {
        let size_ok = |v: i32| (Self::MIN_SIZE..=100_000).contains(&v);
        let pos_ok = |v: i32| (-100_000..=100_000).contains(&v);
        size_ok(self.width) && size_ok(self.height) && pos_ok(self.x) && pos_ok(self.y)
    }

    /// The state file that goes with the settings file at `settings_path`: `window.toml` in the
    /// same folder.
    pub fn path_for(settings_path: &Path) -> PathBuf {
        settings_path.with_file_name("window.toml")
    }

    /// The placement saved at `path`. A missing, unreadable or broken file, or one with an
    /// implausible size or position, gives `None`: the window then opens at its default place.
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str::<Self>(&text).ok().filter(Self::plausible)
    }

    /// Writes the placement to `path` (creating its folder) unless the file already holds it.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if Self::load(path) == Some(*self) {
            return Ok(());
        }
        let body = toml::to_string(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        write_text(path, &format!("{}{body}", Self::HEADER))
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            prefetch_count: 6,
            cache_mib: 1024,
            wrap: true,
            background: [26, 26, 28],
            spread_right_to_left: true,
            nearest_above_zoom: 3.0,
            save_quality: 90,
            language: Language::Korean,
            wheel_navigates: false,
            confirm_delete: true,
        }
    }
}

impl Settings {
    /// Brings every value into its supported range.
    pub fn clamped(mut self) -> Self {
        self.prefetch_count = self.prefetch_count.clamp(0, 32);
        self.cache_mib = self.cache_mib.clamp(64, 65536);
        self.nearest_above_zoom =
            if self.nearest_above_zoom.is_finite() { self.nearest_above_zoom.clamp(1.0, 64.0) } else { 3.0 };
        self.save_quality = self.save_quality.clamp(1, 100);
        self
    }

    /// Reads `path`; a missing file gives defaults, a broken one gives defaults and a warning.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Settings>(&text) {
                Ok(s) => (s.clamped(), None),
                Err(e) => (Settings::default(), Some(format!("{}: {}", path.display(), toml_error(&text, &e)))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
            Err(e) => (Settings::default(), Some(format!("{}: {e}", path.display()))),
        }
    }

    /// Writes the settings to `path` atomically (a crash or a full disk leaves the old file
    /// whole). A file there that cannot be read as settings (the one [`Settings::load`] warned
    /// about, which the user may have edited by hand) is first kept beside it as
    /// `settings.toml.bak` (or `.bak2`, … when taken); its new name is returned.
    pub fn save(&self, path: &Path) -> Result<Option<PathBuf>, String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let kept = keep_unreadable(path)?;
        write_text(path, &text)?;
        Ok(kept)
    }
}

/// A TOML parse error in one line (`line 3: missing comma …`), fit for the title bar; the
/// error's own text spans several lines with a drawing of the spot.
pub fn toml_error(text: &str, e: &toml::de::Error) -> String {
    let message = e.message().trim().replace('\n', " ");
    match e.span() {
        Some(span) => {
            let line = text.get(..span.start).unwrap_or(text).matches('\n').count() + 1;
            format!("line {line}: {message}")
        }
        None => message,
    }
}

/// Replaces `path` with `text` atomically.
fn write_text(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    iv_pipeline::export::write_atomic(path, |f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Copies `path` to a free `<name>.bak`, `<name>.bak2`, … when it exists but does not parse as
/// settings, so replacing it never destroys what the user wrote.
fn keep_unreadable(path: &Path) -> Result<Option<PathBuf>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let readable = std::str::from_utf8(&bytes).is_ok_and(|text| toml::from_str::<Settings>(text).is_ok());
    if readable {
        return Ok(None);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 1..1000 {
        let suffix = if n == 1 { String::new() } else { n.to_string() };
        let bak = path.with_file_name(format!("{name}.bak{suffix}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&bak) {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(&bytes).and_then(|()| f.sync_all()).map_err(|e| format!("{}: {e}", bak.display()))?;
                return Ok(Some(bak));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", bak.display())),
        }
    }
    Err(format!("{}: no free backup name", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.toml");
        let mut s = Settings {
            wrap: false,
            save_quality: 70,
            background: [255, 0, 10],
            language: Language::English,
            ..Default::default()
        };
        assert_eq!(s.save(&path).unwrap(), None, "nothing to keep when there is no file");
        assert_eq!(Settings::load(&path), (s.clone(), None));

        std::fs::write(&path, "wrap = false\n").unwrap();
        let (partial, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert!(!partial.wrap);
        assert_eq!(partial.cache_mib, Settings::default().cache_mib, "missing keys take defaults");
        assert!(partial.confirm_delete, "deleting asks first unless turned off");
        std::fs::write(&path, "confirm_delete = false\n").unwrap();
        assert!(!Settings::load(&path).0.confirm_delete);

        std::fs::write(&path, "cache_mib = 1\nsave_quality = 0\nnearest_above_zoom = 1000.0\n").unwrap();
        let (clamped, _) = Settings::load(&path);
        assert_eq!((clamped.cache_mib, clamped.save_quality, clamped.nearest_above_zoom), (64, 1, 64.0));

        std::fs::write(&path, "wrap = \"maybe\"").unwrap();
        let (fallback, warning) = Settings::load(&path);
        assert_eq!(fallback, Settings::default());
        assert!(warning.is_some());
        s = Settings::load(&dir.path().join("none.toml")).0;
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn saving_over_an_unreadable_file_keeps_it_and_writes_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let broken = "# my colours\nbackground = [10, 20\nwrap = false\n";
        std::fs::write(&path, broken).unwrap();
        let (loaded, warning) = Settings::load(&path);
        let warning = warning.expect("a broken file is reported");
        // one line, with where the problem is: it is shown in the title bar
        assert!(!warning.contains('\n') && warning.contains("settings.toml: line 3: "), "{warning}");
        assert_eq!(loaded, Settings::default());

        // the first save keeps the hand-edited file instead of silently replacing it
        let kept = loaded.save(&path).unwrap().expect("the unreadable file is kept");
        assert_eq!(kept, dir.path().join("settings.toml.bak"));
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), broken);
        assert_eq!(Settings::load(&path), (Settings::default(), None));
        // now the file is readable: later saves just replace it
        assert_eq!(loaded.save(&path).unwrap(), None);

        // broken again: an existing backup is never overwritten
        std::fs::write(&path, "wrap = \"maybe\"").unwrap();
        assert_eq!(loaded.save(&path).unwrap(), Some(dir.path().join("settings.toml.bak2")));
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), broken);

        // only the settings, the two backups and no temporary file
        let mut names: Vec<_> =
            std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["settings.toml", "settings.toml.bak", "settings.toml.bak2"]);
    }

    #[test]
    fn window_placement_has_its_own_file_and_never_touches_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("cfg").join("settings.toml");
        let path = WindowPlacement::path_for(&settings);
        assert_eq!(path, dir.path().join("cfg").join("window.toml"));
        let place = WindowPlacement { x: -1800, y: 40, width: 1600, height: 1000, maximized: true };

        // no files yet: only the state file appears, no settings.toml full of frozen defaults
        assert_eq!(WindowPlacement::load(&path), None);
        place.save(&path).unwrap();
        assert_eq!(WindowPlacement::load(&path), Some(place));
        assert!(!settings.exists(), "closing the window must not create settings.toml");

        // a hand-edited settings file (comments, partial keys, unknown language) stays byte for byte
        let hand = "# mine\nwrap = false # keep\nlanguage = \"fr\"\n";
        std::fs::write(&settings, hand).unwrap();
        let moved = WindowPlacement { x: 10, ..place };
        moved.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), hand);
        assert_eq!(WindowPlacement::load(&path), Some(moved));

        // the same placement again does not rewrite the file
        std::fs::write(&path, format!("# note\n{}", toml::to_string(&moved).unwrap())).unwrap();
        moved.save(&path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("# note\n"));

        // nonsense sizes and broken files are ignored instead of restoring a 1-pixel window,
        // and the next save replaces them
        std::fs::write(&path, "x = 0\ny = 0\nwidth = 3\nheight = 900\nmaximized = false\n").unwrap();
        assert_eq!(WindowPlacement::load(&path), None);
        std::fs::write(&path, "x = \"left\"").unwrap();
        assert_eq!(WindowPlacement::load(&path), None);
        place.save(&path).unwrap();
        assert_eq!(WindowPlacement::load(&path), Some(place));

        // a settings file that still holds a [window] table from a development build loads fine
        std::fs::write(
            &settings,
            "wrap = false\n[window]\nx = 0\ny = 0\nwidth = 800\nheight = 600\nmaximized = false\n",
        )
        .unwrap();
        let (s, warning) = Settings::load(&settings);
        assert!(warning.is_none());
        assert!(!s.wrap);
    }
}
