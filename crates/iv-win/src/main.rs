//! image-viewer: `image-viewer [--trace] [--no-activate] [--keymap <file.toml>] [--settings <file.toml>] [file]`
//!
//! Also `image-viewer --version` (prints the version), `image-viewer --check-update` (asks
//! GitHub for the latest release and prints whether it is newer; exit code 1 when the check
//! failed) and `image-viewer --register` / `--unregister` (lists the viewer in Windows' "Open
//! with" and Default apps for the current user, or removes it; the settings window has the same
//! buttons). None of these opens a window.

// no console window for release builds launched from Explorer
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Instant;

use iv_win::diag;
use iv_win::keymap::Keymap;
use iv_win::settings::Settings;
use iv_win::trace::Trace;
use iv_win::win::shell;
use iv_win::win::window::{Options, run};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

/// A flag that does its job and exits instead of opening the window.
#[derive(Clone, Copy)]
enum Action {
    Version,
    CheckUpdate,
    /// `true`: register for "Open with"; `false`: remove that registration.
    Register(bool),
}

/// `--check-update`: what the check found, in `t`'s language, and whether it worked.
fn check_update(t: &iv_win::i18n::Strings) -> (String, bool) {
    use iv_win::update::{self, Outcome, State};
    let result = iv_win::win::update_check::check();
    let ok = result.is_ok();
    let page = match &result {
        Ok(Outcome::Newer { page, .. }) => Some(page.clone()),
        _ => None,
    };
    let mut text = format!("image-viewer: {}", update::status_text(&State::Done(result), t));
    if let Some(page) = page {
        text.push('\n');
        text.push_str(&page);
    }
    (text, ok)
}

/// Prints a line for a command-line flag. A release build has no console of its own, so it
/// borrows the one it was started from, if any; output redirected to a file or pipe needs none.
fn console_line(text: &str) {
    use std::io::Write;
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("image-viewer"))
}

fn main() {
    let start = Instant::now();
    // log times count from here; the file is only created by the first message
    diag::init(diag::default_dir());
    let mut trace_enabled = false;
    let mut no_activate = false;
    let mut keymap_path: Option<PathBuf> = None;
    let mut settings_path: Option<PathBuf> = None;
    let mut file = None;
    let mut action: Option<Action> = None;
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
        } else if arg == "--version" || arg == "-V" {
            action = Some(Action::Version);
        } else if arg == "--check-update" {
            action = Some(Action::CheckUpdate);
        } else if arg == "--register" {
            action = Some(Action::Register(true));
        } else if arg == "--unregister" {
            action = Some(Action::Register(false));
        } else {
            file = Some(PathBuf::from(arg));
        }
    }
    if let Some(Action::Version) = action {
        console_line(&format!("image-viewer {}", env!("CARGO_PKG_VERSION")));
        return;
    }
    let trace = Trace::new(start, trace_enabled);

    let keymap_path = keymap_path.or_else(|| config_dir().map(|d| d.join("keymap.toml")));
    let (keymap, keymap_warnings) = match keymap_path {
        Some(path) => Keymap::load(&path),
        None => (Keymap::defaults(), Vec::new()),
    };
    for w in &keymap_warnings {
        diag::log(&format!("keymap: {w}"));
    }
    let settings_path = settings_path.or_else(|| config_dir().map(|d| d.join("settings.toml")));
    let (settings, warning) = match &settings_path {
        Some(p) => Settings::load(p),
        None => (Settings::default(), None),
    };
    if let Some(Action::CheckUpdate) = action {
        let (text, ok) = check_update(settings.language.strings());
        console_line(&text);
        std::process::exit(i32::from(!ok));
    }
    if let Some(Action::Register(on)) = action {
        let result = if on { shell::register(settings.language.strings()) } else { shell::unregister() };
        if let Err(e) = &result {
            console_line(&format!("image-viewer: {e}"));
        }
        std::process::exit(i32::from(result.is_err()));
    }
    if let Some(w) = &warning {
        diag::log(&format!("settings: {w}"));
    }
    // from here on a crash of the window's thread (this one) leaves crash.log and says so
    diag::install_panic_hook(settings.language, iv_win::win::dialogs::show_fatal);
    std::process::exit(run(Options {
        file,
        keymap,
        keymap_warnings,
        settings,
        settings_warning: warning,
        settings_path,
        trace,
        no_activate,
    }));
}
