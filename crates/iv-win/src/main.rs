//! image-viewer: `image-viewer [--trace] [--no-activate] [--keymap <file.toml>] [--settings <file.toml>] [file]`

// no console window for release builds launched from Explorer
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Instant;

use iv_win::keymap::Keymap;
use iv_win::settings::Settings;
use iv_win::trace::Trace;
use iv_win::win::window::{Options, run};

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("image-viewer"))
}

fn main() {
    let start = Instant::now();
    let mut trace_enabled = false;
    let mut no_activate = false;
    let mut keymap_path: Option<PathBuf> = None;
    let mut settings_path: Option<PathBuf> = None;
    let mut file = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--keymap" {
            keymap_path = args.next().map(PathBuf::from);
        } else if arg == "--settings" {
            settings_path = args.next().map(PathBuf::from);
        } else if arg == "--trace" {
            trace_enabled = true;
        } else if arg == "--no-activate" {
            no_activate = true;
        } else {
            file = Some(PathBuf::from(arg));
        }
    }
    let trace = Trace::new(start, trace_enabled);

    let keymap_path = keymap_path.or_else(|| config_dir().map(|d| d.join("keymap.toml")));
    let (keymap, warnings) = match keymap_path {
        Some(path) => Keymap::load(&path),
        None => (Keymap::defaults(), Vec::new()),
    };
    for w in warnings {
        eprintln!("keymap: {w}");
    }
    let settings_path = settings_path.or_else(|| config_dir().map(|d| d.join("settings.toml")));
    let (settings, warning) = match &settings_path {
        Some(p) => Settings::load(p),
        None => (Settings::default(), None),
    };
    if let Some(w) = warning {
        eprintln!("settings: {w}");
    }
    std::process::exit(run(Options { file, keymap, settings, settings_path, trace, no_activate }));
}
