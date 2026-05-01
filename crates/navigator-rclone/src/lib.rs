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
//! A pre-flight `--dry-run` pass collects the set of destinations that
//! already exist, so the UI can prompt before overwriting. We never pass
//! `--ignore-existing` implicitly — the user's choice decides.

pub mod log;
pub mod op;

pub use log::{LogEvent, LogLevel, Stats};
pub use op::{
    OpHandle, Operation, OverwritePolicy, PreflightReport, RcloneDriver, RemoteSize, RemoteStat,
};
