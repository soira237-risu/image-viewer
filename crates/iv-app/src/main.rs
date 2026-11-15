//! image-viewer: `image-viewer [--trace] [--no-activate] [--keymap <file.toml>] [file]`

// no console window for release builds launched from Explorer
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod commands;
mod edit;
mod keymap;
mod panel;
mod trace;

use std::path::PathBuf;
use std::time::Instant;

use winit::event_loop::{ControlFlow, EventLoop};

use crate::app::{App, AppEvent};
use crate::keymap::Keymap;
use crate::trace::Trace;

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("image-viewer"))
}

fn main() {
    let start = Instant::now();
    // the slowest startup step; overlaps argument parsing, keymap loading and window creation
    let gpu_init = app::start_gpu_init();
    let mut trace_enabled = false;
    let mut no_activate = false;
    let mut keymap_path: Option<PathBuf> = None;
    let mut file = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--keymap" {
            keymap_path = args.next().map(PathBuf::from);
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

    let event_loop = match EventLoop::<AppEvent>::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot create the event loop: {e}");
            std::process::exit(1);
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new(event_loop.create_proxy(), keymap, trace, Some(gpu_init));
    app.no_activate = no_activate;
    // decoding starts now and overlaps window and GPU creation
    if let Some(path) = file {
        app.open(path);
    }
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("event loop error: {e}");
        std::process::exit(1);
    }
}
