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
pub mod backup;
pub mod backup_dialog;
pub mod batch;
pub mod clipboard;
pub mod compare;
pub mod compare_dialog;
pub mod context_menu;
pub mod dialog;
pub mod dialogs;
pub mod elevated;
pub mod extract;
pub mod history;
pub mod listview;
pub mod model;
pub mod narrate;
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
pub mod shell_op;
pub mod shortcut_editor;
pub mod sound;
pub mod space_window;
pub mod spacemap;
pub mod speech;
pub mod tempsweep;
pub mod viewer;
pub mod watcher;
pub mod window;

pub use app::{AppConfig, run};
pub use model::Model;

/// Handle a `--shell-op` command line, if that is what this process was
/// launched for. `Some(exit_code)` means the invocation was a detached
/// shell copy/move and has now run to completion — `main` must exit with
/// that code and never build a window. `None` means an ordinary launch.
///
/// Lives here rather than in `main` so the binary crate stays free of
/// Win32; see [`shell_op`] for why the transfer runs out of process.
pub fn try_run_shell_op(argv: &[String]) -> Option<i32> {
    match shell_op::parse_helper_args(argv)? {
        Ok(args) => Some(shell_op::run_helper(&args)),
        Err(msg) => {
            tracing::error!("shell-op: {}", msg);
            Some(shell_op::EXIT_BAD_ARGS)
        }
    }
}
