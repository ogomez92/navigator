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
    #[serde(default)]
    pub stats: Option<Stats>,
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
}
