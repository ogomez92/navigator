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

pub mod log;
pub mod op;

pub use log::{LogEvent, LogLevel, Stats, Transferring};
pub use op::{
    Canceller, ConflictReport, OpHandle, Operation, PreflightReport, Progress, RcloneDriver,
    RemoteSize, RemoteStat, victims,
};
