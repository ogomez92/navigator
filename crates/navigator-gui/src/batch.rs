//! Grouping a paste into as few rclone invocations as possible.
//!
//! `Operation::Copy` is one rclone process per item, so an N-file paste was
//! N sequential spawns and `--transfers` had a single file to chew on.
//! Measured through the driver on 200 small files (~240 KB total): **28.98 s
//! per-item versus 0.26 s batched** — essentially all of it process startup,
//! not I/O.
//!
//! [`partition`] splits a selection into groups that can share one
//! `--files-from` invocation and singles that cannot. The rules exist for
//! specific reasons, each pinned by a test:
//!
//! * **Directories are never batchable.** `rclone copy --files-from` reads
//!   a directory entry, transfers nothing, warns about nothing, and exits
//!   0. A folder routed through a list is silently lost while the paste
//!   reports success. This is the whole reason the module exists rather
//!   than a one-line change at the call site.
//! * **Names with newlines are never batchable.** The list format is
//!   one-name-per-line, so an embedded newline would split into two bogus
//!   entries — which, per the rule above, then silently do nothing. Windows
//!   forbids these characters in filenames but rclone remotes need not.
//! * **Groups of one stay single.** A temp file plus a list round-trip buys
//!   nothing for a single item.
//!
//! Everything here is pure — `is_dir` is injected — so the partition logic
//! is testable without touching a filesystem or spawning rclone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use navigator_core::NavPath;

/// Files sharing one source directory, destined for one rclone call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    /// Common parent, passed to rclone as the source root.
    pub src_root: NavPath,
    /// Names relative to `src_root`, one per line in the list file.
    pub names: Vec<String>,
}

/// Result of splitting a selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partition {
    /// Batchable groups, each becoming one `--files-from` invocation.
    pub groups: Vec<FileGroup>,
    /// Items that must run one-per-invocation: directories, groups of one,
    /// and anything whose name cannot survive the list format.
    pub singles: Vec<NavPath>,
}

impl Partition {
    /// Total rclone invocations this partition implies.
    pub fn invocations(&self) -> usize {
        self.groups.len() + self.singles.len()
    }
}

/// True when `name` can appear in a `--files-from` list without corrupting
/// it. The list is newline-delimited, so a name containing one would be
/// read as two entries.
pub fn is_listable(name: &str) -> bool {
    !name.is_empty() && !name.contains('\n') && !name.contains('\r')
}

/// Split `sources` into batchable groups and per-item singles.
///
/// `is_dir` is injected so this is pure and testable; production passes a
/// closure over the real filesystem. Ordering is deterministic: groups come
/// out sorted by source root, and `singles` preserves the original
/// selection order, so the spoken summary and the focus target don't jump
/// around between runs.
pub fn partition(sources: &[NavPath], is_dir: impl Fn(&NavPath) -> bool) -> Partition {
    let mut by_parent: BTreeMap<PathBuf, Vec<(usize, NavPath)>> = BTreeMap::new();
    let mut singles: Vec<(usize, NavPath)> = Vec::new();

    for (i, s) in sources.iter().enumerate() {
        let name = s.file_name().to_string();
        let parent = s.as_path().parent().map(Path::to_path_buf);
        match parent {
            // A directory, an unlistable name, or a path with no parent
            // (a drive root) can never join a list.
            Some(p) if !is_dir(s) && is_listable(&name) => {
                by_parent.entry(p).or_default().push((i, s.clone()));
            }
            _ => singles.push((i, s.clone())),
        }
    }

    let mut groups = Vec::new();
    for (parent, items) in by_parent {
        if items.len() < 2 {
            // Not worth a temp file; hand it back as a single.
            singles.extend(items);
            continue;
        }
        let Ok(src_root) = NavPath::new(parent) else {
            // Parent didn't survive NavPath validation — fall back rather
            // than guess at a root.
            singles.extend(items);
            continue;
        };
        groups.push(FileGroup {
            src_root,
            names: items
                .iter()
                .map(|(_, s)| s.file_name().to_string())
                .collect(),
        });
    }

    // Restore selection order among singles so "1 of N" narration and the
    // pending-focus target stay predictable.
    singles.sort_by_key(|(i, _)| *i);
    Partition {
        groups,
        singles: singles.into_iter().map(|(_, s)| s).collect(),
    }
}

/// A `--files-from` list on disk, deleted when dropped.
///
/// rclone needs a real file path, and the operation outlives the call that
/// builds the argv, so the caller holds this guard for the duration.
#[derive(Debug)]
pub struct TempList {
    path: PathBuf,
}

impl TempList {
    /// Write `names` one per line into a uniquely-named temp file.
    ///
    /// `seq` disambiguates concurrent pastes within one process; the pid
    /// keeps two navigator instances apart. Deliberately avoids the system
    /// temp dir being shared with a stale file of the same name by
    /// including both.
    /// Names arrive in **OS** form, straight off `FindFirstFileExW`, and
    /// are translated into rclone's namespace on the way in — see
    /// [`navigator_rclone::encoding`]. That translation is the whole
    /// reason this cannot be a plain `join("\n")`: rclone matches each
    /// entry against its *own* listing of the source root, which is in
    /// standard form, so a raw on-disk name carrying a full-width `｜` or
    /// an escaped `‛＂` matches nothing. rclone then transfers nothing,
    /// logs nothing, and exits 0 — the silent half of the bug, and the
    /// one `--local-encoding None` never touched.
    pub fn write(names: &[String], seq: u64) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "{}{}-{}.txt",
            crate::tempsweep::FILES_FROM_PREFIX,
            std::process::id(),
            seq
        ));
        // Written as UTF-8 with plain \n. rclone reads the list as UTF-8
        // and treats \n as the separator; a trailing newline is fine.
        let mut body = String::with_capacity(names.iter().map(|n| n.len() + 1).sum());
        for n in names {
            body.push_str(&navigator_rclone::to_standard_name(n));
            body.push('\n');
        }
        std::fs::write(&path, body)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempList {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(s: &str) -> NavPath {
        NavPath::new(s).unwrap()
    }

    /// The rule the whole module exists to enforce. `--files-from` ignores
    /// directory entries silently and exits 0, so a folder that reached a
    /// list would vanish while the paste claimed success.
    #[test]
    fn directories_never_join_a_group() {
        let sources = vec![
            np("C:\\src\\a.txt"),
            np("C:\\src\\folder"),
            np("C:\\src\\b.txt"),
        ];
        let p = partition(&sources, |s| s.file_name() == "folder");
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].names, vec!["a.txt", "b.txt"]);
        assert_eq!(p.singles, vec![np("C:\\src\\folder")]);
    }

    /// A selection of only directories must produce no list at all.
    #[test]
    fn all_directories_produce_no_groups() {
        let sources = vec![np("C:\\src\\one"), np("C:\\src\\two")];
        let p = partition(&sources, |_| true);
        assert!(p.groups.is_empty());
        assert_eq!(p.singles.len(), 2);
        assert_eq!(p.invocations(), 2);
    }

    /// The payoff case: many files from one folder collapse to one call.
    #[test]
    fn many_files_from_one_folder_collapse_to_one_invocation() {
        let sources: Vec<NavPath> = (0..200)
            .map(|i| np(&format!("C:\\src\\f{}.txt", i)))
            .collect();
        let p = partition(&sources, |_| false);
        assert_eq!(p.invocations(), 1, "200 files must become one rclone call");
        assert_eq!(p.groups[0].names.len(), 200);
        assert_eq!(p.groups[0].src_root, np("C:\\src"));
    }

    /// `--files-from` takes one source root, so a clipboard gathered from
    /// several folders (via Append to copy) splits per folder.
    #[test]
    fn files_from_different_parents_group_separately() {
        let sources = vec![
            np("C:\\one\\a.txt"),
            np("C:\\two\\b.txt"),
            np("C:\\one\\c.txt"),
            np("C:\\two\\d.txt"),
        ];
        let p = partition(&sources, |_| false);
        assert_eq!(p.groups.len(), 2);
        assert!(p.singles.is_empty());
        let roots: Vec<String> = p.groups.iter().map(|g| g.src_root.to_string()).collect();
        assert!(roots.contains(&"C:\\one".to_string()));
        assert!(roots.contains(&"C:\\two".to_string()));
    }

    /// One file from a folder gains nothing from a temp file + list.
    #[test]
    fn a_lone_file_stays_single() {
        let sources = vec![np("C:\\src\\only.txt")];
        let p = partition(&sources, |_| false);
        assert!(p.groups.is_empty());
        assert_eq!(p.singles, sources);
    }

    /// Mixed: two from one folder batch, the lone one from another doesn't.
    #[test]
    fn lone_file_in_its_own_folder_is_not_batched() {
        let sources = vec![
            np("C:\\one\\a.txt"),
            np("C:\\one\\b.txt"),
            np("C:\\two\\solo.txt"),
        ];
        let p = partition(&sources, |_| false);
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].names, vec!["a.txt", "b.txt"]);
        assert_eq!(p.singles, vec![np("C:\\two\\solo.txt")]);
        assert_eq!(p.invocations(), 2);
    }

    /// A newline in a name would split into two list entries which, being
    /// nonexistent, rclone then ignores silently. Keep them off the list.
    #[test]
    fn names_with_newlines_are_not_listable() {
        assert!(is_listable("normal.txt"));
        assert!(is_listable("with space and ünïcode.txt"));
        assert!(!is_listable("evil\nname.txt"));
        assert!(!is_listable("evil\rname.txt"));
        assert!(!is_listable(""));
    }

    /// A name that cannot survive the list format must drop to the
    /// per-item path rather than corrupt the list for its whole group.
    /// Windows forbids newlines in filenames, but an rclone remote need
    /// not, and a corrupted entry fails *silently*.
    #[test]
    fn unlistable_names_fall_back_to_singles() {
        let evil = NavPath::new("C:\\src\\evil\nname.txt")
            .expect("NavPath should accept it; the list format is what cannot");
        let sources = vec![np("C:\\src\\a.txt"), evil.clone(), np("C:\\src\\b.txt")];
        let p = partition(&sources, |_| false);
        assert_eq!(
            p.groups[0].names,
            vec!["a.txt", "b.txt"],
            "the well-behaved names still batch together"
        );
        assert_eq!(p.singles, vec![evil], "the newline name runs on its own");
    }

    /// Selection order must survive so "N of M" narration and the
    /// pending-focus target stay stable across runs.
    #[test]
    fn singles_preserve_selection_order() {
        let sources = vec![
            np("C:\\src\\zdir"),
            np("C:\\src\\a.txt"),
            np("C:\\src\\adir"),
        ];
        let p = partition(&sources, |s| s.file_name().ends_with("dir"));
        assert_eq!(
            p.singles,
            vec![
                np("C:\\src\\zdir"),
                np("C:\\src\\a.txt"),
                np("C:\\src\\adir")
            ],
            "zdir was selected first and must stay first"
        );
    }

    #[test]
    fn empty_selection_is_empty_partition() {
        let p = partition(&[], |_| false);
        assert_eq!(p.invocations(), 0);
    }

    #[test]
    fn temp_list_writes_one_name_per_line_and_cleans_up() {
        let names = vec!["a.txt".to_string(), "with space.txt".to_string()];
        let path = {
            let list = TempList::write(&names, 4242).unwrap();
            let body = std::fs::read_to_string(list.path()).unwrap();
            assert_eq!(body, "a.txt\nwith space.txt\n");
            list.path().to_path_buf()
        };
        assert!(!path.exists(), "temp list must be removed on drop");
    }

    /// Non-ASCII names must round-trip as UTF-8 — rclone reads the list as
    /// UTF-8, and an accent is not something its encoder looks at, so
    /// these must reach the list byte-identical.
    #[test]
    fn temp_list_round_trips_unicode() {
        let names = vec!["acentuado-ñé.txt".to_string(), "日本語.txt".to_string()];
        let list = TempList::write(&names, 4243).unwrap();
        let body = std::fs::read_to_string(list.path()).unwrap();
        assert_eq!(body, "acentuado-ñé.txt\n日本語.txt\n");
    }

    /// The silent-loss case. `--files-from` entries are matched against
    /// rclone's own listing of the source, which is in rclone's namespace,
    /// so a name carrying a full-width twin or an escaped one has to be
    /// translated on the way into the list. Getting this wrong costs no
    /// error and no exit code — just files that never arrive.
    #[test]
    fn temp_list_translates_names_into_rclones_namespace() {
        let names = vec![
            // yt-dlp's full-width pipe, escaped by an earlier copy.
            "10 Famous \u{201B}\u{FF5C} Andre Antunes [x].mp3".to_string(),
            // A bare full-width quote, never escaped.
            "\u{FF02}quoted\u{FF02}.mp3".to_string(),
            "ordinary.mp3".to_string(),
        ];
        let list = TempList::write(&names, 4244).unwrap();
        let body = std::fs::read_to_string(list.path()).unwrap();
        assert_eq!(
            body,
            "10 Famous \u{FF5C} Andre Antunes [x].mp3\n\"quoted\".mp3\nordinary.mp3\n"
        );
    }
}
