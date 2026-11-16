//! User settings, kept in `settings.toml` next to `keymap.toml`. Every field has a default, so a
//! missing or partial file works, and out-of-range values are clamped rather than rejected.

use std::path::Path;

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
    /// Where the main window was when it last closed (written on close, not by the settings
    /// window). Kept last: TOML needs plain values before tables.
    pub window: Option<WindowPlacement>,
}

/// The main window's restored (not maximised, not full-screen) rectangle in workspace
/// coordinates, as `GetWindowPlacement` reports it, and whether it was maximised.
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

    fn plausible(&self) -> bool {
        let size_ok = |v: i32| (Self::MIN_SIZE..=100_000).contains(&v);
        let pos_ok = |v: i32| (-100_000..=100_000).contains(&v);
        size_ok(self.width) && size_ok(self.height) && pos_ok(self.x) && pos_ok(self.y)
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
            window: None,
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
        self.window = self.window.filter(WindowPlacement::plausible);
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

    /// Records the window placement in the file at `path`, leaving every other value as the
    /// file has it: changes made in the settings window but not saved stay unsaved. A file that
    /// does not parse is left alone rather than replaced by defaults.
    pub fn save_window(path: &Path, placement: WindowPlacement) -> Result<(), String> {
        let (mut on_disk, warning) = Settings::load(path);
        if let Some(w) = warning {
            return Err(w);
        }
        if on_disk.window == Some(placement) {
            return Ok(());
        }
        on_disk.window = Some(placement);
        on_disk.save(path)
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
    fn window_placement_is_saved_alone_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let place = WindowPlacement { x: -1800, y: 40, width: 1600, height: 1000, maximized: true };
        // no file yet: defaults plus the placement
        Settings::save_window(&path, place).unwrap();
        let (s, warning) = Settings::load(&path);
        assert!(warning.is_none());
        assert_eq!(s, Settings { window: Some(place), ..Settings::default() });

        // other values on disk are kept, and unsaved in-memory values are not written
        std::fs::write(&path, "wrap = false\nwheel_navigates = true\n").unwrap();
        Settings::save_window(&path, place).unwrap();
        let s = Settings::load(&path).0;
        assert!(!s.wrap && s.wheel_navigates);
        assert_eq!(s.window, Some(place));

        // a file without the table (older versions) still loads
        std::fs::write(&path, "save_quality = 70\n").unwrap();
        assert_eq!(Settings::load(&path).0.window, None);

        // nonsense sizes are dropped instead of restoring a 1-pixel window
        std::fs::write(&path, "[window]\nx = 0\ny = 0\nwidth = 3\nheight = 900\nmaximized = false\n").unwrap();
        assert_eq!(Settings::load(&path).0.window, None);

        // a broken file is not overwritten
        std::fs::write(&path, "wrap = \"maybe\"").unwrap();
        assert!(Settings::save_window(&path, place).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "wrap = \"maybe\"");
    }
}
