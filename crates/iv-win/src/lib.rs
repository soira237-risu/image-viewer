//! The image viewer on plain Win32. Everything here except `win` is platform independent and
//! tested without a window: the CPU renderer, view math, commands, key bindings, settings and
//! editing helpers.

pub mod commands;
pub mod edit;
pub mod i18n;
pub mod keymap;
pub mod panel;
pub mod raster;
pub mod settings;
pub mod trace;
pub mod view;
#[cfg(windows)]
pub mod win;
