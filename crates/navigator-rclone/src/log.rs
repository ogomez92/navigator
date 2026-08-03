//! Minimal deserializer for rclone's `--use-json-log` output.
//!
//! rclone emits one JSON object per line on stdout/stderr. The schema is not
//! formally versioned, so we deserialize loosely and surface the raw record
//! when fields are missing.

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    #[serde(other)]
    Unknown,
}

/// One line of rclone JSON log output.
#[derive(Debug, Clone, Deserialize)]
pub struct LogEvent {
    #[serde(default)]
    pub level: Option<LogLevel>,
    #[serde(default)]
    pub msg: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default, rename = "objectType")]
    pub object_type: Option<String>,
    /// Set on `--dry-run` records to the verb that *would* have run, e.g.
    /// `"copy"` or `"delete"`. This is the reliable way to read a dry-run:
    /// the human-readable `msg` has changed spelling across rclone
    /// releases (it used to be "Would copy", it is now "Skipped copy as
    /// --dry-run is set"), and matching on that text silently stopped
    /// working. The structured field has been stable.
    #[serde(default)]
    pub skipped: Option<String>,
    #[serde(default)]
    pub stats: Option<Stats>,
}

impl LogEvent {
    /// `true` if this is a dry-run record for `verb` (`"copy"`, `"move"`,
    /// `"delete"`) carrying an object path.
    pub fn is_dry_run(&self, verb: &str) -> bool {
        self.skipped.as_deref() == Some(verb) && self.object.is_some()
    }
}

/// The `stats` object attached to NOTICE-level stats log records.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(non_snake_case)]
pub struct Stats {
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub totalBytes: u64,
    #[serde(default)]
    pub transfers: u64,
    #[serde(default)]
    pub totalTransfers: u64,
    #[serde(default)]
    pub speed: f64,
    #[serde(default)]
    pub eta: Option<u64>,
    #[serde(default)]
    pub elapsedTime: f64,
    #[serde(default)]
    pub errors: u64,
    /// rclone's own summary of what went wrong, attached to every stats
    /// tick once `errors > 0`. It is the fallback error message when a
    /// run leaves no `Failed to …` verdict behind — see
    /// [`crate::error::ErrorCollector`].
    #[serde(default)]
    pub lastError: Option<String>,
    /// Files moving right now. A stats record carries no top-level
    /// `object` — that only appears on the per-file INFO records, which
    /// fire when a transfer *ends* — so this is the only source for "what
    /// is it working on at this instant".
    #[serde(default)]
    pub transferring: Vec<Transferring>,
}

/// One in-flight transfer inside a [`Stats`] record. rclone reports far
/// more per entry (size, speed, percentage, both filesystems); the name is
/// all the progress window needs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Transferring {
    #[serde(default)]
    pub name: String,
}
