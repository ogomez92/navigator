//! Staging directory for remote files the user opens.
//!
//! Opening a file that lives on an rclone remote isn't something Windows
//! can do natively — ShellExecute needs a local path. We pull the file
//! down to `<exe_dir>/.remote-cache/<remote>/<sub/path>/<file>`, hand
//! that off to ShellExecute, then keep the staged copy under a `notify`
//! watcher so the next save triggers an "upload back?" prompt.
//!
//! State is per-process: records vanish when the app exits, but the
//! staged files themselves stay (same "no auto-purge" stance as `.trash`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

use navigator_core::NavPath;

use crate::window::{HwndSend, WMAPP_REMOTE_EDIT};

/// One staged file's bookkeeping.
pub struct StageRecord {
    /// Original remote path (`NavPath::is_remote()`).
    pub remote: NavPath,
    /// Last mtime we know is in sync with the remote. `None` before the
    /// first download finishes.
    pub last_known_mtime: Option<SystemTime>,
    /// `true` while an upload-prompt dialog is on screen or an upload is
    /// in flight. Stops the watcher from re-firing a new prompt while the
    /// current one is still being handled — editors often emit several
    /// Modify events per save.
    pub prompting: bool,
}

/// Process-wide cache. Held as `Arc<RemoteCache>` on `AppState`.
pub struct RemoteCache {
    pub root: PathBuf,
    pub records: Arc<Mutex<HashMap<PathBuf, StageRecord>>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
}

impl RemoteCache {
    pub fn new() -> Self {
        let root = navigator_config::exe_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".remote-cache");
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            records: Arc::new(Mutex::new(HashMap::new())),
            watcher: Mutex::new(None),
        }
    }

    /// Staging path for `remote:sub`. Preserves the sub-path layout so
    /// two different files with the same basename don't collide, and
    /// keeps the extension intact so ShellExecute picks the right
    /// association. Creates parent dirs.
    pub fn stage_path_for(&self, remote_name: &str, sub: &str) -> PathBuf {
        let mut p = self.root.clone();
        p.push(sanitize_component(remote_name));
        for part in sub.split(['/', '\\']) {
            if part.is_empty() { continue; }
            p.push(sanitize_component(part));
        }
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        p
    }

    /// Record a freshly-downloaded file and make sure the cache-wide
    /// watcher is armed. Idempotent — called from every download worker.
    pub fn register(self: &Arc<Self>, staged: PathBuf, remote: NavPath, hwnd: HwndSend) {
        self.insert_record(staged, remote);
        self.ensure_watcher(hwnd);
    }

    /// Just the bookkeeping half of `register` — split out so tests can
    /// exercise the record machinery without arming a real Win32
    /// `PostMessageW` target.
    pub fn insert_record(&self, staged: PathBuf, remote: NavPath) {
        let mtime = staged.metadata().ok().and_then(|m| m.modified().ok());
        let rec = StageRecord { remote, last_known_mtime: mtime, prompting: false };
        self.records.lock().insert(staged, rec);
    }

    /// Expose `prompting` / mtime decision for tests — real usage routes
    /// through the watcher closure which holds the same logic inline.
    #[cfg(test)]
    fn should_prompt(&self, staged: &Path, new_mtime: Option<SystemTime>) -> bool {
        let mut g = self.records.lock();
        let Some(rec) = g.get_mut(staged) else { return false };
        if rec.prompting { return false; }
        let changed = match (new_mtime, rec.last_known_mtime) {
            (Some(a), Some(b)) => a > b,
            (Some(_), None) => true,
            _ => false,
        };
        if changed { rec.prompting = true; }
        changed
    }

    /// Called by the main thread after the upload prompt closes (either
    /// the user declined or the upload finished). Clears `prompting` and,
    /// on success, re-baselines the mtime so the next save re-triggers.
    pub fn finish_prompt(&self, staged: &Path, new_mtime: Option<SystemTime>) {
        let mut g = self.records.lock();
        if let Some(rec) = g.get_mut(staged) {
            rec.prompting = false;
            if new_mtime.is_some() {
                rec.last_known_mtime = new_mtime;
            }
        }
    }

    /// Look up the remote for a staged path.
    pub fn remote_for(&self, staged: &Path) -> Option<NavPath> {
        self.records.lock().get(staged).map(|r| r.remote.clone())
    }

    fn ensure_watcher(self: &Arc<Self>, hwnd: HwndSend) {
        let mut guard = self.watcher.lock();
        if guard.is_some() { return; }

        let records = Arc::clone(&self.records);
        let hwnd_raw: isize = hwnd.0.0 as isize;

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return; };
            if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                return;
            }
            for path in event.paths {
                let mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                let mut g = records.lock();
                let Some(rec) = g.get_mut(&path) else { continue; };
                if rec.prompting { continue; }
                // Prompt only if the file is newer than what we last
                // baselined (or mtime is unreadable but record exists —
                // still likely a save).
                let changed = match (mtime, rec.last_known_mtime) {
                    (Some(a), Some(b)) => a > b,
                    (Some(_), None) => true,
                    _ => false,
                };
                if !changed { continue; }
                rec.prompting = true;
                drop(g);
                let payload = Box::into_raw(Box::new(path.clone())) as isize;
                unsafe {
                    let h = windows::Win32::Foundation::HWND(hwnd_raw as *mut _);
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(h),
                        WMAPP_REMOTE_EDIT,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(payload),
                    );
                }
            }
        });

        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("remote-cache watcher failed: {}", e);
                return;
            }
        };
        if let Err(e) = watcher.watch(&self.root, RecursiveMode::Recursive) {
            tracing::warn!("remote-cache watch({:?}) failed: {}", self.root, e);
            return;
        }
        *guard = Some(watcher);
    }
}

impl Default for RemoteCache {
    fn default() -> Self { Self::new() }
}

/// Strip characters that would break a Windows path component.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\0' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fake_cache() -> RemoteCache { RemoteCache::new() }

    #[test]
    fn stage_path_preserves_sub_layout() {
        let cache = fake_cache();
        let p = cache.stage_path_for("gdrive", "photos/2024/trip.jpg");
        assert!(p.ends_with("gdrive/photos/2024/trip.jpg") || p.ends_with(r"gdrive\photos\2024\trip.jpg"));
    }

    #[test]
    fn stage_path_accepts_backslash_sub() {
        let cache = fake_cache();
        let p = cache.stage_path_for("mac", r"Downloads\incoming\a.txt");
        assert!(p.ends_with("mac/Downloads/incoming/a.txt") || p.ends_with(r"mac\Downloads\incoming\a.txt"));
    }

    #[test]
    fn stage_path_skips_empty_components() {
        let cache = fake_cache();
        let p1 = cache.stage_path_for("gdrive", "/photos//2024/");
        let p2 = cache.stage_path_for("gdrive", "photos/2024");
        assert_eq!(p1, p2);
    }

    #[test]
    fn sanitize_strips_reserved_chars() {
        assert_eq!(sanitize_component("a:b*c?"), "a_b_c_");
        assert_eq!(sanitize_component("ok-name.123"), "ok-name.123");
    }

    #[test]
    fn sanitize_applied_to_remote_name() {
        // A remote with a hostile character (shouldn't exist in rclone
        // configs, but defend anyway) still produces a usable path.
        let cache = fake_cache();
        let p = cache.stage_path_for("bad:name", "x");
        let s = p.to_string_lossy();
        assert!(!s.contains("bad:name"));
        assert!(s.contains("bad_name"));
    }

    #[test]
    fn insert_then_remote_for_roundtrip() {
        let cache = fake_cache();
        let staged = cache.stage_path_for("gdrive", "a.txt");
        cache.insert_record(staged.clone(), NavPath::remote("gdrive", "a.txt"));
        let got = cache.remote_for(&staged).unwrap();
        assert_eq!(got.rclone_arg().as_deref(), Some("gdrive:a.txt"));
    }

    #[test]
    fn remote_for_unknown_path_is_none() {
        let cache = fake_cache();
        let p = cache.stage_path_for("gdrive", "ghost.txt");
        assert!(cache.remote_for(&p).is_none());
    }

    #[test]
    fn should_prompt_fires_once_then_suppressed() {
        let cache = fake_cache();
        let staged = cache.stage_path_for("gdrive", "note.txt");
        cache.insert_record(staged.clone(), NavPath::remote("gdrive", "note.txt"));
        // Baseline is None (file doesn't exist). First change fires.
        let t1 = SystemTime::now();
        assert!(cache.should_prompt(&staged, Some(t1)));
        // Re-entry while prompting is suppressed.
        let t2 = t1 + Duration::from_secs(5);
        assert!(!cache.should_prompt(&staged, Some(t2)));
    }

    #[test]
    fn finish_prompt_rebaselines_and_unblocks() {
        let cache = fake_cache();
        let staged = cache.stage_path_for("gdrive", "re.txt");
        cache.insert_record(staged.clone(), NavPath::remote("gdrive", "re.txt"));
        let t1 = SystemTime::now();
        assert!(cache.should_prompt(&staged, Some(t1)));
        // User declined / upload finished: record new mtime and clear.
        cache.finish_prompt(&staged, Some(t1));
        // Same mtime — no prompt.
        assert!(!cache.should_prompt(&staged, Some(t1)));
        // Newer mtime — prompts again.
        let t2 = t1 + Duration::from_secs(1);
        assert!(cache.should_prompt(&staged, Some(t2)));
    }

    #[test]
    fn finish_prompt_without_mtime_just_clears_flag() {
        let cache = fake_cache();
        let staged = cache.stage_path_for("gdrive", "np.txt");
        cache.insert_record(staged.clone(), NavPath::remote("gdrive", "np.txt"));
        let t1 = SystemTime::now();
        assert!(cache.should_prompt(&staged, Some(t1)));
        cache.finish_prompt(&staged, None);
        // Same mtime should still fire because baseline is unchanged.
        assert!(cache.should_prompt(&staged, Some(t1)));
    }

    #[test]
    fn insert_is_idempotent_on_path() {
        let cache = fake_cache();
        let staged = cache.stage_path_for("gdrive", "x.txt");
        cache.insert_record(staged.clone(), NavPath::remote("gdrive", "x.txt"));
        cache.insert_record(staged.clone(), NavPath::remote("gdrive", "x.txt"));
        assert_eq!(cache.records.lock().len(), 1);
    }
}
