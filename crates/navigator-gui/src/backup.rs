//! Backup / restore-from-backup for the focused entry.
//!
//! Ctrl+Shift+B copies the focused file or folder to
//! `<volume_root>\.navigator_backup\<id>\<basename>` — same volume as the
//! source, same shape as `.trash` — and records the pair in
//! `<exe_dir>/backups.json`. Ctrl+Shift+R lists the recorded backups
//! (newest first), and restoring one **replaces** the current version:
//! whatever sits at the original path is first moved to `.trash` (the
//! ordinary delete staging, so the pre-restore state is recoverable), then
//! the backup is *copied* back. Copied, not moved — the backup survives the
//! restore, so a game save can be restored over and over.
//!
//! **A backup is only taken where a restore is guaranteed.**
//! [`backup_dir_on_volume_of`] refuses UNC shares (same litter problem as
//! `.trash` — a `.navigator_backup` at the root of somebody else's file
//! server, hidden by SMB dot-name mapping), and refuses remote / sentinel
//! paths outright: a remote backend may not support the rename-to-trash
//! half of the restore, and `volume_root_of` is meaningless there. The
//! guard lives here, not just in `op_backup`, so a future caller can't
//! reintroduce it by forgetting to branch — the same defence
//! `trash_dir_on_volume_of` carries.
//!
//! Like `.trash`, `.navigator_backup` is never auto-purged: it holds user
//! data. The record file is likewise uncapped — capping it would silently
//! orphan backup directories that still hold data. Each record is two
//! paths, not an operation's whole source list, so it stays small (unlike
//! `clipboard_history.json`, whose growth is why `push_history` needs a
//! writer thread; these reads/writes happen on op workers anyway).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use navigator_core::NavPath;

/// Directory created at each volume root to hold backups. A constant so
/// the naming half and anything that ever needs to recognise one of these
/// directories cannot drift apart.
pub const BACKUP_DIR_NAME: &str = ".navigator_backup";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    /// Name of the per-backup directory under `.navigator_backup`.
    pub id: String,
    /// Absolute path the item lived at when it was backed up — where a
    /// restore writes back to.
    pub original: String,
    /// Absolute path of the backed-up copy inside `.navigator_backup`.
    pub backup: String,
    #[serde(default)]
    pub is_dir: bool,
    /// Unix timestamp (seconds) of the backup.
    #[serde(default)]
    pub ts: u64,
}

pub fn backups_path() -> PathBuf {
    exe_dir().join("backups.json")
}

fn exe_dir() -> PathBuf {
    navigator_config::exe_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn load_backups() -> Vec<BackupEntry> {
    load_backups_from(&backups_path())
}

/// Record a completed backup, newest first. Called by the backup worker
/// *after* the copy succeeds, so the file never names a backup that holds
/// no data.
pub fn record_backup(entry: BackupEntry) {
    record_backup_at(&backups_path(), entry);
}

/// Drop the record for `id` — used when a restore finds the backup
/// directory gone (the user deleted `.navigator_backup` by hand), so the
/// list doesn't keep offering a restore that can never run.
pub fn remove_backup_record(id: &str) {
    remove_backup_record_at(&backups_path(), id);
}

// Path-taking halves of the persistence, separated so tests can run
// against a `tempfile::TempDir` instead of the real `<exe_dir>` file.

pub fn load_backups_from(path: &Path) -> Vec<BackupEntry> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_backups_to(path: &Path, entries: &[BackupEntry]) {
    if let Ok(s) = serde_json::to_string_pretty(entries) {
        let _ = std::fs::write(path, s);
    }
}

pub fn record_backup_at(path: &Path, entry: BackupEntry) {
    let mut entries = load_backups_from(path);
    entries.insert(0, entry);
    save_backups_to(path, &entries);
}

pub fn remove_backup_record_at(path: &Path, id: &str) {
    let mut entries = load_backups_from(path);
    let before = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() != before {
        save_backups_to(path, &entries);
    }
}

/// Fresh identifier for one backup: `<unix_ts>_<pid>_<counter>`. The
/// counter keeps rapid successive backups within one process apart (same
/// scheme as `.trash`); the pid keeps two running navigator instances
/// apart, which trash tolerates but a *recorded* id must not — the id is
/// the directory name a later restore resolves.
pub fn new_backup_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}_{}_{}",
        crate::clipboard::now_ts(),
        std::process::id(),
        n
    )
}

/// Name the per-backup directory on the same drive/volume as `path`,
/// e.g. `C:\.navigator_backup\<id>\`. Naming only — no IO; the worker
/// creates it immediately before the copy that lands in it, same as the
/// trash flow.
///
/// Returns `None` for UNC shares and for remote / sentinel paths — see
/// the module docs for why those must never be backed up.
pub fn backup_dir_on_volume_of(path: &NavPath, id: &str) -> Option<NavPath> {
    if path.is_unc() || path.is_remote() || path.is_this_pc() || path.is_remotes_root() {
        return None;
    }
    let root = crate::app::volume_root_of(path.as_path())?;
    NavPath::new(root.join(BACKUP_DIR_NAME).join(id)).ok()
}

/// Comparison key for a directory path: separators normalised, any
/// trailing separator dropped, ASCII-folded. Windows paths are
/// case-insensitive, and `D:\code\tools`, `D:/code/tools` and
/// `D:\code\tools\` all name the same folder.
fn dir_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let trimmed = s.trim_end_matches('\\');
    let base = if trimmed.is_empty() {
        s.as_str()
    } else {
        trimmed
    };
    base.to_ascii_lowercase()
}

/// The backups Ctrl+Shift+R may offer while the user stands in `cwd`:
/// those whose original lived *in this folder*, newest first.
///
/// **The restore picker is anchored to a folder, not a global list.** An
/// unscoped list let a Ctrl+Shift+R pressed in `D:\code\tools` restore
/// `C:\meow` — a destructive write into a folder the user was not even
/// looking at, one mis-keyed Enter away, and with no dialog at all
/// whenever exactly one backup happened to be recorded.
///
/// Scoping by *folder* rather than by the focused row is deliberate: the
/// case this feature exists for is a game that just ate its save, so the
/// item itself is frequently gone and there is no row left to stand on.
/// Its parent folder is still where the user is, so a restore stays
/// reachable while the blast radius stays inside the current directory.
pub fn backups_in_dir(entries: &[BackupEntry], cwd: &Path) -> Vec<BackupEntry> {
    let want = dir_key(cwd);
    entries
        .iter()
        .filter(|e| {
            Path::new(&e.original)
                .parent()
                .is_some_and(|parent| dir_key(parent) == want)
        })
        .cloned()
        .collect()
}

/// One-line label for the restore picker: name, kind, date. The list is
/// scoped to a single folder and the dialog names that folder in its
/// heading, so the row does **not** repeat the directory — a screen
/// reader would otherwise read the same path aloud on every row before
/// reaching the part that differs. The name leads because it is what the
/// user is looking for — same reasoning as the drive-letter-first rule
/// for This PC rows.
pub fn entry_label(e: &BackupEntry) -> String {
    let name = Path::new(&e.original)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(e.original.as_str());
    let kind = if e.is_dir { " (folder)" } else { "" };
    format!("{}{} — {}", name, kind, crate::clipboard::format_ts(e.ts))
}

/// Which row the restore picker should open on, given the already
/// folder-scoped list from [`backups_in_dir`]. That list is newest first,
/// so the first record whose original matches the focused path is the most
/// recent backup of the thing the user is standing on — the overwhelmingly
/// common restore. No match (the item was deleted, or focus is on a
/// sibling) falls back to row 0, the newest backup in this folder.
/// Case-insensitive because Windows paths are.
pub fn preselect_index(entries: &[BackupEntry], focused: Option<&str>) -> usize {
    let Some(f) = focused else {
        return 0;
    };
    entries
        .iter()
        .position(|e| e.original.eq_ignore_ascii_case(f))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, original: &str, ts: u64) -> BackupEntry {
        BackupEntry {
            id: id.into(),
            original: original.into(),
            backup: format!(r"D:\{}\{}\x", BACKUP_DIR_NAME, id),
            is_dir: false,
            ts,
        }
    }

    #[test]
    fn backup_dir_lands_in_navigator_backup_on_the_volume_root() {
        let p = NavPath::new(r"D:\Games\Elden Ring\saves").unwrap();
        let dir = backup_dir_on_volume_of(&p, "123_9_0").expect("local path names a dir");
        assert_eq!(dir.to_string(), format!(r"D:\{}\123_9_0", BACKUP_DIR_NAME));
    }

    /// Same guard as `trash_dir_is_never_named_on_a_unc_share`, widened:
    /// a backup we could take but not restore (or that litters somebody
    /// else's server root) must never be *named*, so no caller can take it.
    #[test]
    fn backup_dir_is_never_named_on_unc_remote_or_sentinel_paths() {
        let unc = NavPath::new(r"\\host\share\dir\file.txt").unwrap();
        assert!(backup_dir_on_volume_of(&unc, "1_2_3").is_none());

        let remote = NavPath::new("mac:Downloads/incoming").unwrap();
        assert!(remote.is_remote(), "test premise: this parses as a remote");
        assert!(backup_dir_on_volume_of(&remote, "1_2_3").is_none());

        assert!(backup_dir_on_volume_of(&NavPath::this_pc(), "1_2_3").is_none());
        assert!(backup_dir_on_volume_of(&NavPath::remotes_root(), "1_2_3").is_none());
    }

    #[test]
    fn records_round_trip_and_insert_newest_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("backups.json");

        record_backup_at(&file, entry("a", r"C:\one", 10));
        record_backup_at(&file, entry("b", r"C:\two", 20));

        let got = load_backups_from(&file);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "b", "newest record must lead the list");
        assert_eq!(got[1].id, "a");
        assert_eq!(got[1].original, r"C:\one");
        assert_eq!(got[1].ts, 10);
    }

    #[test]
    fn removing_a_record_leaves_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("backups.json");
        record_backup_at(&file, entry("a", r"C:\one", 10));
        record_backup_at(&file, entry("b", r"C:\two", 20));

        remove_backup_record_at(&file, "b");
        let got = load_backups_from(&file);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a");

        // Removing an unknown id is a no-op, not an error.
        remove_backup_record_at(&file, "zzz");
        assert_eq!(load_backups_from(&file).len(), 1);
    }

    #[test]
    fn a_corrupt_or_missing_record_file_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("backups.json");
        assert!(load_backups_from(&file).is_empty());
        std::fs::write(&file, "not json {").unwrap();
        assert!(load_backups_from(&file).is_empty());
    }

    #[test]
    fn backup_ids_are_unique_within_a_process() {
        let a = new_backup_id();
        let b = new_backup_id();
        assert_ne!(a, b);
        let pid = format!("_{}_", std::process::id());
        assert!(a.contains(&pid), "id should embed the pid: {a}");
    }

    #[test]
    fn preselect_picks_the_newest_backup_of_the_focused_path() {
        let entries = vec![
            entry("c", r"C:\other", 30),
            entry("b", r"C:\Games\saves", 20),
            entry("a", r"C:\games\SAVES", 10), // older backup of the same path
        ];
        // Case-insensitive match, and the first (newest) hit wins.
        assert_eq!(preselect_index(&entries, Some(r"c:\GAMES\saves")), 1);
        // No match or no focus → newest overall.
        assert_eq!(preselect_index(&entries, Some(r"C:\elsewhere")), 0);
        assert_eq!(preselect_index(&entries, None), 0);
    }

    #[test]
    fn label_leads_with_the_name_and_marks_folders() {
        let mut e = entry("a", r"C:\Games\Elden Ring\saves", 0);
        e.is_dir = true;
        let label = entry_label(&e);
        assert!(
            label.starts_with("saves (folder) — "),
            "label should lead with name+kind: {label}"
        );
        // The picker is scoped to one folder and names it in the heading,
        // so the row must not repeat that folder on every line.
        assert!(
            !label.contains(r"C:\Games\Elden Ring"),
            "row should not repeat the folder: {label}"
        );
    }

    /// The bug this scoping exists for: standing in one folder must never
    /// offer to restore over a path somewhere else entirely.
    #[test]
    fn the_picker_only_sees_backups_taken_from_the_current_folder() {
        let entries = vec![
            entry("c", r"C:\meow", 30),
            entry("b", r"D:\code\tools\navigator", 20),
            entry("a", r"D:\code\tools\other\deep", 10),
        ];
        let here = backups_in_dir(&entries, Path::new(r"D:\code\tools"));
        assert_eq!(here.len(), 1, "only a direct child belongs to this folder");
        assert_eq!(here[0].original, r"D:\code\tools\navigator");

        // Trailing separator, forward slashes and case must not change the
        // answer — all three name the same folder on Windows.
        for cwd in [r"D:\code\tools\", "D:/code/tools", r"d:\CODE\Tools"] {
            assert_eq!(
                backups_in_dir(&entries, Path::new(cwd)).len(),
                1,
                "{cwd} should scope like the plain form"
            );
        }

        // A drive root is a folder like any other.
        assert_eq!(backups_in_dir(&entries, Path::new(r"C:\")).len(), 1);
        assert!(backups_in_dir(&entries, Path::new(r"E:\nothing")).is_empty());
    }

    /// Order is preserved, so `preselect_index` still means "newest backup
    /// of the focused item" once the list has been scoped.
    #[test]
    fn scoping_keeps_newest_first_order() {
        let entries = vec![
            entry("c", r"D:\saves\game", 30),
            entry("b", r"C:\elsewhere", 25),
            entry("a", r"D:\saves\game", 10),
        ];
        let here = backups_in_dir(&entries, Path::new(r"D:\saves"));
        assert_eq!(here.len(), 2);
        assert_eq!(here[0].ts, 30, "newest first must survive the filter");
        assert_eq!(preselect_index(&here, Some(r"d:\saves\GAME")), 0);
    }
}
