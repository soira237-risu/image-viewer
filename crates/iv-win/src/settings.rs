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
        std::fs::write(path, format!("{}{body}", Self::HEADER)).map_err(|e| format!("{}: {e}", path.display()))
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
                Err(e) => (Settings::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
            Err(e) => (Settings::default(), Some(format!("{}: {e}", path.display()))),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
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
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), (s.clone(), None));

        std::fs::write(&path, "wrap = false\n").unwrap();
        let (partial, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert!(!partial.wrap);
        assert_eq!(partial.cache_mib, Settings::default().cache_mib, "missing keys take defaults");

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
