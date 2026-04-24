//! File-backed cross-instance clipboard + operation history.
//!
//! The Windows clipboard is intentionally not used — copy/cut/paste in
//! navigator is an rclone batch operation, not OLE `IDataObject` shell
//! integration, and polluting the OS clipboard with navigator's own idea
//! of "selected files" would fight whatever the user is actually doing
//! in other apps.
//!
//! Instead, state lives in a small JSON file next to the executable:
//!
//! - `clipboard.json`         — the current clip (sources + cut flag).
//!   Read on every paste, written on every copy/cut/append. Two
//!   navigator instances share it automatically.
//! - `clipboard_history.json` — rolling log of recent operations (copy,
//!   cut, paste, delete). Feeds the File menu's "Recent operations"
//!   submenu so users can re-seed the clipboard from a past action.
//!
//! Portability: `<exe_dir>` matches where `config.toml` and `plugins/`
//! already live (see `navigator_config::exe_dir`), so the whole app is
//! still a single drop-in folder.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Max history entries kept on disk / shown in the menu.
pub const MAX_HISTORY: usize = 20;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClipFile {
    /// Absolute source paths, in the order they were added.
    #[serde(default)]
    pub sources: Vec<String>,
    /// `true` = cut (move on paste). `false` = copy.
    #[serde(default)]
    pub cut: bool,
    /// Unix timestamp in seconds of the last mutation.
    #[serde(default)]
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// "copy" | "cut" | "append-copy" | "append-cut" | "paste" | "delete".
    pub kind: String,
    pub sources: Vec<String>,
    /// Destination folder for `paste`; `None` otherwise.
    #[serde(default)]
    pub dest: Option<String>,
    pub ts: u64,
}

pub fn clip_path() -> PathBuf {
    exe_dir().join("clipboard.json")
}

pub fn history_path() -> PathBuf {
    exe_dir().join("clipboard_history.json")
}

fn exe_dir() -> PathBuf {
    navigator_config::exe_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn load_clip() -> ClipFile {
    match std::fs::read_to_string(clip_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => ClipFile::default(),
    }
}

pub fn save_clip(c: &ClipFile) {
    if let Ok(s) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(clip_path(), s);
    }
}

pub fn load_history() -> Vec<HistoryEntry> {
    match std::fs::read_to_string(history_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Push a new entry at the front, cap at `MAX_HISTORY`.
pub fn push_history(entry: HistoryEntry) {
    let mut entries = load_history();
    entries.insert(0, entry);
    if entries.len() > MAX_HISTORY { entries.truncate(MAX_HISTORY); }
    if let Ok(s) = serde_json::to_string_pretty(&entries) {
        let _ = std::fs::write(history_path(), s);
    }
}

/// Short human-readable label for an entry. Used to populate menu items.
pub fn entry_label(e: &HistoryEntry) -> String {
    let n = e.sources.len();
    let first = e.sources.first()
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    let suffix = if n > 1 { format!(" (+{} more)", n - 1) } else { String::new() };
    match e.kind.as_str() {
        "paste" => {
            let dest = e.dest.as_deref()
                .and_then(|p| std::path::Path::new(p).file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("?");
            format!("Paste {}{} → {}", first, suffix, dest)
        }
        other => {
            let verb = match other {
                "copy" => "Copy",
                "cut"  => "Cut",
                "append-copy" => "Append copy",
                "append-cut"  => "Append cut",
                "delete" => "Delete",
                _ => other,
            };
            format!("{} {}{}", verb, first, suffix)
        }
    }
}
