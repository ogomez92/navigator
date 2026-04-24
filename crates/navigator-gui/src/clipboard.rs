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

/// Multi-line detail view of a history entry. Shown in the ops history
/// window's bottom pane when the user highlights a row. Kind-specific:
/// paste shows from → to + counts; copy/cut echo the action + sources;
/// delete shows the doomed paths. Large selections get a head-only
/// listing with a count tail so the pane doesn't turn into a dump of
/// ten thousand lines for a big batch.
pub fn entry_details(e: &HistoryEntry) -> String {
    const HEAD: usize = 50;
    let n = e.sources.len();
    let verb = match e.kind.as_str() {
        "copy"        => "Copy",
        "cut"         => "Cut",
        "append-copy" => "Append to copy clipboard",
        "append-cut"  => "Append to cut clipboard",
        "paste"       => "Paste",
        "delete"      => "Delete",
        other         => other,
    };

    let mut s = String::new();
    s.push_str(&format!("Operation: {}\n", verb));
    s.push_str(&format!("When:      {}\n", format_ts(e.ts)));
    s.push_str(&format!("Items:     {}\n", n));
    if e.kind == "paste" {
        if let Some(dst) = e.dest.as_deref() {
            s.push_str(&format!("Dest:      {}\n", dst));
        }
    }
    // Source roots — one line summarising which directories the op
    // touched. Dedup so a batch of 50 files from one folder still shows
    // one entry, and heterogeneous drives both show up without
    // pretending they share a root.
    let roots = distinct_parents(&e.sources);
    if !roots.is_empty() {
        s.push_str(&format!("From:      {}\n", roots.join(", ")));
    }

    let header = match e.kind.as_str() {
        "paste"  => "\nSources:\n",
        "delete" => "\nDeleted:\n",
        _        => "\nPaths:\n",
    };
    s.push_str(header);
    for p in e.sources.iter().take(HEAD) {
        s.push_str("  ");
        s.push_str(p);
        s.push('\n');
    }
    if n > HEAD {
        s.push_str(&format!("  … {} more not shown\n", n - HEAD));
    }
    s
}

/// Distinct parent directories across `paths`, preserving first-seen
/// order. Empty parents (bare filenames) are dropped — the user can
/// read those straight off the per-path list below.
fn distinct_parents(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let parent = std::path::Path::new(p).parent()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        if parent.is_empty() { continue; }
        if !out.iter().any(|x| x == &parent) {
            out.push(parent);
        }
    }
    out
}

fn format_ts(ts: u64) -> String {
    // Convert Unix seconds → FILETIME (100-ns since 1601) so we reuse
    // the existing formatter without pulling in chrono.
    if ts == 0 { return "—".to_string(); }
    let ticks = ts.saturating_mul(10_000_000).saturating_add(116_444_736_000_000_000);
    let out = crate::listview::format_filetime(ticks);
    if out.is_empty() { ts.to_string() } else { out }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(n: usize) -> Vec<String> {
        (0..n).map(|i| format!(r"C:\proj\file{i}.txt")).collect()
    }

    #[test]
    fn copy_details_show_paths_and_count() {
        let e = HistoryEntry {
            kind: "copy".into(),
            sources: sources(3),
            dest: None,
            ts: 0,
        };
        let s = entry_details(&e);
        assert!(s.contains("Operation: Copy"));
        assert!(s.contains("Items:     3"));
        assert!(s.contains(r"C:\proj\file0.txt"));
        assert!(s.contains("From:      C:\\proj"), "common parent missing:\n{s}");
        assert!(!s.contains("Dest:"));
    }

    #[test]
    fn paste_details_show_dest() {
        let e = HistoryEntry {
            kind: "paste".into(),
            sources: sources(2),
            dest: Some(r"D:\backup".into()),
            ts: 0,
        };
        let s = entry_details(&e);
        assert!(s.contains("Operation: Paste"));
        assert!(s.contains(r"Dest:      D:\backup"));
        assert!(s.contains("Sources:"));
    }

    #[test]
    fn delete_details_use_deleted_header() {
        let e = HistoryEntry {
            kind: "delete".into(),
            sources: sources(1),
            dest: None,
            ts: 0,
        };
        let s = entry_details(&e);
        assert!(s.contains("Operation: Delete"));
        assert!(s.contains("Deleted:"));
    }

    #[test]
    fn large_batch_truncates_after_head() {
        let e = HistoryEntry {
            kind: "copy".into(),
            sources: sources(120),
            dest: None,
            ts: 0,
        };
        let s = entry_details(&e);
        assert!(s.contains("Items:     120"));
        assert!(s.contains("… 70 more not shown"), "truncation tail missing:\n{s}");
        // First few listed, last few elided.
        assert!(s.contains(r"C:\proj\file0.txt"));
        assert!(!s.contains(r"C:\proj\file119.txt"));
    }

    #[test]
    fn heterogeneous_parents_list_all_distinct_roots() {
        let e = HistoryEntry {
            kind: "copy".into(),
            sources: vec![r"C:\a\1.txt".into(), r"D:\b\2.txt".into()],
            dest: None,
            ts: 0,
        };
        let s = entry_details(&e);
        assert!(s.contains(r"From:      C:\a, D:\b"),
            "expected both roots comma-separated, got:\n{s}");
    }

    #[test]
    fn repeated_parents_deduped_in_from_line() {
        // Batch of 3 files from same folder → `From:` shows it once.
        let e = HistoryEntry {
            kind: "copy".into(),
            sources: vec![
                r"C:\proj\a.rs".into(),
                r"C:\proj\b.rs".into(),
                r"C:\proj\c.rs".into(),
            ],
            dest: None,
            ts: 0,
        };
        let s = entry_details(&e);
        // Count occurrences of the path — should be 1 on the From line.
        let from_lines: Vec<&str> = s.lines().filter(|l| l.starts_with("From:")).collect();
        assert_eq!(from_lines.len(), 1);
        assert_eq!(from_lines[0], r"From:      C:\proj");
    }
}
