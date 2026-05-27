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
pub mod elevated;
pub mod extract;
pub mod history;
pub mod listview;
pub mod model;
pub mod new_folder;
pub mod ops_window;
pub mod options;
pub mod perf;
pub mod plugins;
pub mod preflight;
pub mod progress;
pub mod props;
pub mod remote_cache;
pub mod search;
pub mod shortcut_editor;
pub mod speech;
pub mod viewer;
pub mod watcher;
pub mod window;

pub use app::{AppConfig, run};
pub use model::Model;
