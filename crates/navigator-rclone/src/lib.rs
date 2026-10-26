//! rclone driver.
//!
//! All copy / move / delete go through `rclone`. We run it with
//! `--use-json-log --log-level INFO --stats=1s --stats-log-level NOTICE`
//! and parse each stdout line as a JSON log record. That gives us:
//!
//!   * structured errors with paths attached
//!   * periodic progress (bytes / eta / current file)
//!   * a natural place to hook cancellation (kill the child)
//!
//! Conflict handling is mode-based rather than per-item: a paste carries a
//! [`navigator_core::ConflictMode`] which becomes rclone flags (or, for
//! `Mirror`, the `sync` verb). [`RcloneDriver::conflicts`] runs two
//! `--dry-run` passes and diffs them to find which existing destinations
//! the chosen mode would actually destroy, so the UI can confirm only when
//! data is genuinely at risk.
//!
//! Transfers between two filesystem paths (local drives, UNC shares) run
//! on robocopy instead — see [`robocopy`]. It speaks the same
//! [`op::OpEvent`] stream, so callers only choose which driver to spawn.

pub mod encoding;
pub mod error;
pub mod log;
pub mod op;
pub mod robocopy;
pub mod rustic;

pub use encoding::{to_standard_name, to_standard_path};
pub use error::{ErrorCollector, ErrorKind, RcloneError};
pub use log::{LogEvent, LogLevel, Stats, Transferring};
pub use op::{
    Canceller, ConflictReport, OpHandle, Operation, PreflightReport, Progress, RcloneDriver,
    RemoteSize, RemoteStat, RemoteTreeItem, kill_child_with_process, victims,
};
pub use robocopy::RobocopyDriver;
pub use rustic::RusticDriver;
