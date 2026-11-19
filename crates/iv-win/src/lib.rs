//! The image viewer on plain Win32. Everything here except `win` is platform independent and
//! tested without a window: the CPU renderer, view math, commands, key bindings, settings,
//! editing helpers, help contents, wheel handling, clipboard data, the "Open with" and thumbnail
//! registration plans and reading the answer to a check for updates.

#[cfg(test)]
mod app_icon;
pub mod assoc;
pub mod clipboard;
pub mod commands;
pub mod diag;
pub mod edit;
pub mod help;
pub mod i18n;
pub mod keymap;
pub mod panel;
pub mod raster;
pub mod settings;
pub mod thumbs;
pub mod trace;
pub mod update;
pub mod view;
pub mod wheel;
#[cfg(windows)]
pub mod win;
