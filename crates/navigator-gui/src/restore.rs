//! Restoring out of rustic backups — the pure half.
//!
//! A paste whose clipboard holds items from a rustic snapshot is a
//! restore (`WorkerCtx::run_restore_batch` in `app.rs` drives it). It keeps
//! the paste's conflict vocabulary, but rustic has none of rclone's
//! conflict flags: `rustic restore` skips identical files, **overwrites
//! every file that differs**, leaves extra local files alone, and — with
//! `--delete` — prunes them. Every other mode is built here out of
//! excludes, from what `rustic diff` says about the existing destination:
//!
//! | mode | excluded from the restore |
//! |---|---|
//! | Add new only | every file that differs (`M`) |
//! | Update | differing files whose local copy is *newer* than the backup's |
//! | Replace | nothing a restore can write safely |
//! | Mirror | as Replace, plus `--delete` — folders only |
//!
//! and in **every** mode the type changes (`T`: a local file where the
//! backup has a folder, or the reverse), because restoring over one
//! crashes rustic half-way. Those are reported, not silently dropped.

use std::path::{Path, PathBuf};

use navigator_core::{ConflictMode, Entry, EntryKind, FileTime, NavPath};
use navigator_rclone::rustic::{DiffEntry, Node, Snapshot};

/// What one item's restore will exclude, and what it would destroy.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ItemPlan {
    /// Paths relative to the item, for `Operation::RusticRestore`.
    pub excludes: Vec<String>,
    /// Files the restore would overwrite, relative to the item.
    pub overwrites: Vec<String>,
    /// Local files `--delete` would remove (Mirror only).
    pub deletes: Vec<String>,
    /// Entries left alone because local and backup disagree on the type.
    pub retyped: Vec<String>,
}

/// Plan one item's restore under `mode` from its `diff` against the
/// existing destination. `local_newer(rel)` answers "is the local copy of
/// this differing file newer than the backup's?" — only Update asks.
pub fn plan_item(
    mode: ConflictMode,
    diff: &[DiffEntry],
    local_newer: impl Fn(&str) -> bool,
) -> ItemPlan {
    let mut plan = ItemPlan::default();
    for e in diff {
        match e.status {
            'T' => {
                plan.excludes.push(e.path.clone());
                plan.retyped.push(e.path.clone());
            }
            'M' => {
                let protect = match mode {
                    ConflictMode::AddNewOnly => true,
                    ConflictMode::Update => local_newer(&e.path),
                    ConflictMode::Replace | ConflictMode::Mirror => false,
                };
                if protect {
                    plan.excludes.push(e.path.clone());
                } else {
                    plan.overwrites.push(e.path.clone());
                }
            }
            '+' if mode == ConflictMode::Mirror => plan.deletes.push(e.path.clone()),
            _ => {}
        }
    }
    plan
}

/// How one item lands at the destination, given what is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// Nothing there: restore straight in, nothing to compare.
    Fresh,
    /// Same kind already there: merge, under the plan's excludes.
    Merge,
    /// A file where the item is a folder, or the reverse. Only Keep both
    /// can restore it; every other mode leaves it and says so.
    KindClash,
}

pub fn landing(src_is_dir: bool, local: &Path) -> Landing {
    match std::fs::symlink_metadata(local) {
        Err(_) => Landing::Fresh,
        Ok(m) if m.is_dir() == src_is_dir => Landing::Merge,
        Ok(_) => Landing::KindClash,
    }
}

/// Display form of a path inside an item, for the conflict dialog: the
/// item's own name for a file, `item\inner\path` for a folder.
pub fn shown(item: &str, rel: &str, item_is_dir: bool) -> String {
    if item_is_dir && !rel.is_empty() {
        format!("{}\\{}", item, rel.replace('/', "\\"))
    } else {
        item.to_string()
    }
}

/// A local file's mtime as a [`FileTime`], to compare with a backup node.
pub fn local_mtime(p: &Path) -> Option<FileTime> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(FileTime(
        FileTime::UNIX_EPOCH_TICKS + d.as_secs() * 10_000_000 + (d.subsec_nanos() / 100) as u64,
    ))
}

/// Is the local copy newer than the backed-up one? Whole seconds only:
/// filesystems disagree on sub-second precision, and a file restored from
/// this very backup must count as "the same age", not "newer".
pub fn is_newer(local: FileTime, backup: FileTime) -> bool {
    local.0 / 10_000_000 > backup.0 / 10_000_000
}

/// The trailing note on a restore's spoken summary for the items it had to
/// leave alone, or `None`.
pub fn retyped_note(retyped: &[String]) -> Option<String> {
    match retyped.len() {
        0 => None,
        1 => Some(format!(
            "{} was not restored: it is a file in one place and a folder in the other",
            retyped[0]
        )),
        n => Some(format!(
            "{n} items were not restored: each is a file in one place and a folder in the other"
        )),
    }
}

/// Where an opened backup file is staged: under the remote cache, by
/// repository, snapshot and path, so two snapshots' copies of one file
/// never overwrite each other.
pub fn stage_path(cache_root: &Path, repo: &str, snapshot_id: &str, sub: &str) -> PathBuf {
    let mut p = cache_dir(cache_root).join(format!("{:016x}", fnv1a(repo)));
    p.push(snapshot_id);
    for part in sub.split(['/', '\\']).filter(|s| !s.is_empty()) {
        p.push(
            part.chars()
                .map(|c| match c {
                    '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\0' => '_',
                    c => c,
                })
                .collect::<String>(),
        );
    }
    p
}

/// Every opened backup file lives under here. Empty trash deletes it —
/// see `AppState::op_empty_trash`.
pub fn cache_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("rustic")
}

/// Stable across runs (unlike `DefaultHasher`), so a reopened file lands
/// on its earlier staged copy and rustic can skip restoring it again.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Properties text for a snapshot row.
pub fn snapshot_properties(repo: &str, s: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("Snapshot:    {}\n", s.id));
    out.push_str(&format!("Repository:  {}\n", repo));
    out.push_str(&format!("Time:        {}\n", s.time));
    if !s.hostname.is_empty() {
        out.push_str(&format!("Host:        {}\n", s.hostname));
    }
    if !s.label.is_empty() {
        out.push_str(&format!("Label:       {}\n", s.label));
    }
    if !s.tags.is_empty() {
        out.push_str(&format!("Tags:        {}\n", s.tags.join(", ")));
    }
    if let Some(d) = s.description.as_deref().filter(|d| !d.is_empty()) {
        out.push_str(&format!("Description: {}\n", d));
    }
    out.push_str("Paths:\n");
    for p in &s.paths {
        out.push_str(&format!("  {}\n", p));
    }
    if let Some(m) = &s.summary {
        out.push_str(&format!(
            "Contents:    {} files, {} folders, {}\n",
            m.total_files_processed,
            m.total_dirs_processed,
            crate::props::format_size_with_bytes(m.total_bytes_processed)
        ));
        out.push_str(&format!(
            "Added:       {} new to the repository\n",
            crate::props::format_size_with_bytes(m.data_added)
        ));
    }
    if !s.program_version.is_empty() {
        out.push_str(&format!("Made by:     {}\n", s.program_version));
    }
    out
}

/// Properties text for an entry inside a snapshot. Folder sizes are not
/// totalled: that would mean listing every directory below, one rustic
/// process each.
pub fn node_properties(entry: &Entry, path: &NavPath, node: Option<&Node>) -> String {
    let mut out = String::new();
    let kind = match entry.kind {
        EntryKind::File => "File (in a backup)",
        EntryKind::Directory => "Folder (in a backup)",
        EntryKind::Symlink => "Symbolic link (in a backup)",
        EntryKind::Other => "Special file (in a backup)",
    };
    out.push_str(&format!("Name:      {}\n", entry.name));
    out.push_str(&format!(
        "Location:  {}\n",
        path.rustic_display().unwrap_or_else(|| path.to_string())
    ));
    out.push_str(&format!("Type:      {}\n", kind));
    if entry.kind == EntryKind::File {
        out.push_str(&format!(
            "Size:      {}\n",
            crate::props::format_size_with_bytes(entry.size)
        ));
    }
    if let Some(n) = node {
        if let Some(t) = n.mtime.as_deref() {
            out.push_str(&format!("Modified:  {}\n", t));
        }
        if let Some(t) = n.ctime.as_deref() {
            out.push_str(&format!("Created:   {}\n", t));
        }
        if let Some(l) = n.linktarget.as_deref() {
            out.push_str(&format!("Link to:   {}\n", l));
        }
    }
    out.push_str("\nRead-only: copy it and paste somewhere to restore it.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(status: char, path: &str) -> DiffEntry {
        DiffEntry {
            status,
            path: path.into(),
        }
    }

    fn diff() -> Vec<DiffEntry> {
        vec![
            d('M', "old.txt"),
            d('M', "edited-later.txt"),
            d('T', "was-a-folder"),
            d('+', "mine.txt"),
            d('-', "missing.txt"),
        ]
    }

    #[test]
    fn add_new_only_protects_every_differing_file() {
        let p = plan_item(ConflictMode::AddNewOnly, &diff(), |_| false);
        assert_eq!(p.excludes, ["old.txt", "edited-later.txt", "was-a-folder"]);
        assert!(p.overwrites.is_empty());
        assert!(p.deletes.is_empty());
    }

    #[test]
    fn update_protects_only_newer_local_copies() {
        let p = plan_item(ConflictMode::Update, &diff(), |rel| {
            rel == "edited-later.txt"
        });
        assert_eq!(p.overwrites, ["old.txt"]);
        assert_eq!(p.excludes, ["edited-later.txt", "was-a-folder"]);
    }

    #[test]
    fn replace_overwrites_and_mirror_also_deletes() {
        let p = plan_item(ConflictMode::Replace, &diff(), |_| true);
        assert_eq!(p.overwrites, ["old.txt", "edited-later.txt"]);
        assert!(p.deletes.is_empty(), "a plain restore never deletes");
        let p = plan_item(ConflictMode::Mirror, &diff(), |_| true);
        assert_eq!(p.deletes, ["mine.txt"]);
    }

    #[test]
    fn type_changes_are_always_excluded_and_reported() {
        // Restoring over one crashes rustic, in every mode.
        for mode in [
            ConflictMode::AddNewOnly,
            ConflictMode::Update,
            ConflictMode::Replace,
            ConflictMode::Mirror,
        ] {
            let p = plan_item(mode, &diff(), |_| false);
            assert!(p.excludes.contains(&"was-a-folder".to_string()), "{mode:?}");
            assert_eq!(p.retyped, ["was-a-folder"]);
        }
        assert!(retyped_note(&[]).is_none());
        assert!(
            retyped_note(&["x".into()])
                .unwrap()
                .starts_with("x was not restored")
        );
    }

    #[test]
    fn newer_compares_whole_seconds() {
        let s =
            |secs: u64, sub: u64| FileTime(FileTime::UNIX_EPOCH_TICKS + secs * 10_000_000 + sub);
        assert!(
            !is_newer(s(100, 9_999_999), s(100, 0)),
            "same second is not newer"
        );
        assert!(is_newer(s(101, 0), s(100, 5)));
        assert!(!is_newer(s(99, 0), s(100, 0)));
    }

    #[test]
    fn landing_tells_a_kind_clash_from_a_merge() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(landing(false, &f), Landing::Merge);
        assert_eq!(landing(true, &f), Landing::KindClash);
        assert_eq!(landing(true, t.path()), Landing::Merge);
        assert_eq!(landing(false, &t.path().join("nope")), Landing::Fresh);
    }

    #[test]
    fn dialog_paths_name_the_item() {
        assert_eq!(
            shown("docs", "nested/big.bin", true),
            r"docs\nested\big.bin"
        );
        assert_eq!(shown("a.txt", "a.txt", false), "a.txt");
    }

    #[test]
    fn read_only_staged_copies_can_still_be_emptied() {
        // Opened copies are marked read-only; Empty trash removes them with
        // `remove_dir_all`, which must not trip over the attribute.
        let t = tempfile::tempdir().unwrap();
        let dir = cache_dir(t.path());
        let f = stage_path(t.path(), r"D:\repo", "4f13f557", "C/a.txt");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, b"x").unwrap();
        let mut perm = std::fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&f, perm).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn staged_copies_are_kept_apart_per_snapshot() {
        let root = Path::new(r"C:\x\.remote-cache");
        let a = stage_path(root, r"D:\repo", "4f13f557", "C/Users/a.txt");
        let b = stage_path(root, r"D:\repo", "193c9e8a", "C/Users/a.txt");
        assert_ne!(a, b);
        assert!(a.ends_with(r"4f13f557\C\Users\a.txt"));
        assert_eq!(a, stage_path(root, r"D:\repo", "4f13f557", "C/Users/a.txt"));
        assert_ne!(a, stage_path(root, r"E:\repo", "4f13f557", "C/Users/a.txt"));
    }
}
