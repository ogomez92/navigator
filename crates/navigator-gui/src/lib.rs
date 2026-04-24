//! Win32 GUI shell for navigator.
//!
//! Why native Win32 controls? MSAA + UIA just work. `SysListView32` in report
//! mode is what Explorer uses — screen readers already know how to read it,
//! including announce-on-focus, selection counts, column headers, and
//! incremental type-ahead. Reimplementing any of that in a custom-drawn list
//! is a trap.

#![cfg(windows)]

pub mod accel;
pub mod actions;
pub mod app;
pub mod clipboard;
pub mod context_menu;
pub mod dialog;
pub mod dialogs;
pub mod history;
pub mod listview;
pub mod model;
pub mod options;
pub mod plugins;
pub mod preflight;
pub mod progress;
pub mod search;
pub mod shortcut_editor;
pub mod watcher;
pub mod speech;
pub mod window;

pub use app::{run, AppConfig};
pub use model::Model;
