//! The image viewer on plain Win32. Everything here except `win` is platform independent and
//! tested without a window: the CPU renderer, view math, commands, key bindings, settings,
//! editing helpers, help contents and wheel handling.

pub mod commands;
pub mod edit;
pub mod help;
pub mod i18n;
pub mod keymap;
pub mod panel;
pub mod raster;
pub mod settings;
pub mod trace;
pub mod view;
pub mod wheel;
#[cfg(windows)]
pub mod win;
