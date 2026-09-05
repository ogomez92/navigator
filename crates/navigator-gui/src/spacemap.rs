//! Pure model behind Ctrl+Shift+S (space breakdown).
//!
//! A `du`-style tree: every folder carries the total size of everything
//! beneath it, and its children are ordered largest first, so "where did
//! the space go?" is answered by walking down the top row at each level.
//!
//! Nothing in here touches an HWND or speaks. The Win32 half
//! (`space_window.rs`) owns the `SysTreeView32` and asks this model for
//! row text; the scan itself runs on a worker (see
//! `AppState::scan_space`) and hands the finished tree over by message.
//!
//! **Two producers, one aggregator.** A remote root is one
//! `rclone lsjson --recursive` — a flat list of root-relative paths and
//! sizes — fed through [`SpaceTree::from_items`]. A local root is walked
//! by [`SpaceTree::scan_local`] straight into the arena, because
//! materialising a path string per entry for a whole drive (the case
//! this screen exists for) is the memory cost `props::walk_tree` pays for
//! Alt+L and a million-entry volume cannot afford twice over. Both
//! producers go through the same [`Builder`], so the parent-completion,
//! roll-up and ordering rules are written exactly once.
//!
//! **Folder sizes are always rolled up from files, never read.** Some
//! backends report a directory `Size` (rclone hands most of them back as
//! `-1`, the local backend as `0`, a few as a real number); trusting it
//! would double-count against the children we sum ourselves, so a
//! directory's own size is discarded on entry. Sizes are *apparent*
//! (byte lengths), not allocated blocks — the same figure Alt+Enter
//! reports, and the only one a remote can answer at all.

use std::collections::HashMap;

use navigator_core::{EntryKind, NavPath};
use navigator_fs::read_dir;
use navigator_rclone::RemoteTreeItem;

/// One row of the breakdown. `size`, `files` and `dirs` are cumulative
/// for a directory (everything beneath it) and the file's own size / zero
/// for a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceNode {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Files anywhere below this directory. Always 0 for a file.
    pub files: u64,
    /// Directories anywhere below this directory. Always 0 for a file.
    pub dirs: u64,
    pub parent: Option<usize>,
    /// Ordered largest first, ties by name — the order the tree shows.
    pub children: Vec<usize>,
}

/// Arena-backed tree. Index [`SpaceTree::ROOT`] is the scanned folder
/// itself; a removed node stays in the arena but is unreachable from
/// the root, so indices handed to the tree control never dangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceTree {
    nodes: Vec<SpaceNode>,
    /// Sub-directories the walk could not read (permission denied, a
    /// reparse loop). Their contents are missing from every total above
    /// them, which is why the count is surfaced on the root row.
    pub unreadable: u64,
}

impl SpaceTree {
    pub const ROOT: usize = 0;

    /// Build from a flat listing of root-relative, forward-slashed paths
    /// — the shape `rclone lsjson --recursive` produces. Intermediate
    /// directories missing from the listing are synthesised, because a
    /// flat backend (S3 without directory markers) lists only objects.
    pub fn from_items<'a, I>(root_label: &str, items: I, unreadable: u64) -> Self
    where
        I: IntoIterator<Item = (&'a str, bool, u64)>,
    {
        let mut b = Builder::new(root_label);
        for (path, is_dir, size) in items {
            b.add_path(path, is_dir, size);
        }
        b.finish(unreadable)
    }

    /// Convenience over [`Self::from_items`] for the rclone walk.
    pub fn from_remote_items(root_label: &str, items: &[RemoteTreeItem]) -> Self {
        Self::from_items(
            root_label,
            items.iter().map(|i| (i.path.as_str(), i.is_dir, i.size)),
            0,
        )
    }

    /// Walk a **local** folder with `navigator_fs::read_dir` directly
    /// into the arena. Explicit stack (a deep tree must not blow the
    /// process stack); reparse points are counted with their own size
    /// and not followed, matching `props::compute_folder_stats` and
    /// `walk_tree`. An unreadable sub-directory is counted and skipped —
    /// one denied folder must not hide the rest of the drive.
    ///
    /// Remote paths must not come here: `read_dir` is `FindFirstFileExW`
    /// on the synthetic `\\?\NavigatorRemote\…` string, which fails at
    /// the root and would yield an empty tree rather than an error.
    pub fn scan_local(root: &NavPath) -> Self {
        Self::scan_local_with(root, &mut |_| true).expect("uncancellable scan")
    }

    /// [`Self::scan_local`] with a progress hook. `tick` is called with
    /// the running file count once per directory read; returning `false`
    /// abandons the walk, and the result is then `None`. A drive scan
    /// runs for minutes, and this is how the worker both speaks a count
    /// along the way and stops when the answer is no longer wanted.
    pub fn scan_local_with(root: &NavPath, tick: &mut dyn FnMut(u64) -> bool) -> Option<Self> {
        let label = root.to_string();
        let mut b = Builder::new(&label);
        let mut unreadable: u64 = 0;
        let mut files: u64 = 0;
        let mut stack: Vec<(NavPath, usize)> = vec![(root.clone(), Self::ROOT)];
        while let Some((dir, node)) = stack.pop() {
            if !tick(files) {
                return None;
            }
            let entries = match read_dir(&dir) {
                Ok(v) => v,
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            };
            for e in entries {
                match e.kind {
                    EntryKind::Directory => {
                        let child = b.add_child(node, &e.name, true, 0);
                        stack.push((dir.join(&e.name), child));
                    }
                    EntryKind::Symlink | EntryKind::File | EntryKind::Other => {
                        b.add_child(node, &e.name, false, e.size);
                        files += 1;
                    }
                }
            }
        }
        Some(b.finish(unreadable))
    }

    pub fn get(&self, idx: usize) -> &SpaceNode {
        &self.nodes[idx]
    }

    pub fn root(&self) -> &SpaceNode {
        &self.nodes[Self::ROOT]
    }

    pub fn children(&self, idx: usize) -> &[usize] {
        &self.nodes[idx].children
    }

    /// Number of nodes ever inserted, removed ones included — a bound on
    /// the indices this tree will hand out.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Path components from the root down to `idx`, root excluded. Empty
    /// for the root itself. This is what a caller joins onto the scanned
    /// `NavPath` to name the real item.
    pub fn components(&self, idx: usize) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let mut cur = idx;
        while let Some(p) = self.nodes[cur].parent {
            out.push(self.nodes[cur].name.as_str());
            cur = p;
        }
        out.reverse();
        out
    }

    /// Every ancestor of `idx`, nearest first, ending at the root.
    pub fn ancestors(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = idx;
        while let Some(p) = self.nodes[cur].parent {
            out.push(p);
            cur = p;
        }
        out
    }

    /// Detach `idx` after the item it names has been deleted, and take
    /// its size and counts out of every ancestor so the totals above it
    /// stay truthful without a rescan. The root cannot be removed.
    /// Returns the former parent so the caller can refresh that row.
    pub fn remove(&mut self, idx: usize) -> Option<usize> {
        let parent = self.nodes[idx].parent?;
        let (size, files, dirs, is_dir) = {
            let n = &self.nodes[idx];
            (n.size, n.files, n.dirs, n.is_dir)
        };
        // A directory counts itself in its parent's `dirs`; a file in
        // `files`. Both counted the subtree beneath already.
        let (files, dirs) = if is_dir {
            (files, dirs + 1)
        } else {
            (files + 1, dirs)
        };
        for a in self.ancestors(idx) {
            let n = &mut self.nodes[a];
            n.size = n.size.saturating_sub(size);
            n.files = n.files.saturating_sub(files);
            n.dirs = n.dirs.saturating_sub(dirs);
        }
        self.nodes[parent].children.retain(|&c| c != idx);
        self.nodes[idx].parent = None;
        Some(parent)
    }

    /// The text a tree row shows — and, since the tree control is what a
    /// screen reader reads, the whole sentence a user hears for the row.
    /// Name first (that is what they navigate by), then size, then the
    /// share of the *parent* (the du question: "of what's in here, how
    /// much is this?"), then the counts for a folder.
    pub fn row_label(&self, idx: usize) -> String {
        let n = &self.nodes[idx];
        if n.parent.is_none() {
            return self.root_label();
        }
        let mut s = format!("{} — {}", n.name, format_size(n.size));
        if let Some(p) = n.parent {
            let parent_size = self.nodes[p].size;
            if parent_size > 0 {
                s.push_str(&format!(", {}", format_share(n.size, parent_size)));
            }
        }
        if n.is_dir {
            if n.files == 0 && n.dirs == 0 {
                s.push_str(", empty");
            } else {
                s.push_str(&format!(", {}", count_phrase(n.files, n.dirs)));
            }
        }
        s
    }

    /// The root row doubles as the summary: no share (it is 100% of
    /// itself) and the unreadable count, because those folders' contents
    /// are missing from every number above them.
    fn root_label(&self) -> String {
        let r = self.root();
        let mut s = format!(
            "{} — {}, {}",
            r.name,
            format_size(r.size),
            count_phrase(r.files, r.dirs)
        );
        if self.unreadable > 0 {
            s.push_str(&format!(
                ", {} unreadable {}",
                group_thousands(self.unreadable),
                if self.unreadable == 1 {
                    "folder"
                } else {
                    "folders"
                }
            ));
        }
        s
    }
}

/// Shared construction path for both producers. Parents are always
/// created before their children, so a single reverse pass over the
/// arena rolls sizes and counts up to the root.
struct Builder {
    nodes: Vec<SpaceNode>,
    /// Directory path → node, for [`Builder::add_path`] only. The local
    /// walk never touches it (it carries the parent index down its
    /// stack), so a drive scan pays nothing for it.
    dirs_by_path: HashMap<String, usize>,
}

impl Builder {
    fn new(root_label: &str) -> Self {
        Self {
            nodes: vec![SpaceNode {
                name: root_label.to_string(),
                is_dir: true,
                size: 0,
                files: 0,
                dirs: 0,
                parent: None,
                children: Vec::new(),
            }],
            dirs_by_path: HashMap::new(),
        }
    }

    /// Append a node under `parent`. A directory's own reported size is
    /// dropped here — see the module docs.
    fn add_child(&mut self, parent: usize, name: &str, is_dir: bool, size: u64) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(SpaceNode {
            name: name.to_string(),
            is_dir,
            size: if is_dir { 0 } else { size },
            files: 0,
            dirs: 0,
            parent: Some(parent),
            children: Vec::new(),
        });
        self.nodes[parent].children.push(idx);
        idx
    }

    /// Insert a root-relative path, creating any directory on the way
    /// down that the listing has not named yet. A directory that arrives
    /// after a file inside it already implied it is simply matched.
    fn add_path(&mut self, path: &str, is_dir: bool, size: u64) {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return;
        }
        if is_dir {
            self.ensure_dir(path);
            return;
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((dir, name)) => (self.ensure_dir(dir), name),
            None => (SpaceTree::ROOT, path),
        };
        self.add_child(parent, name, false, size);
    }

    fn ensure_dir(&mut self, path: &str) -> usize {
        if path.is_empty() {
            return SpaceTree::ROOT;
        }
        if let Some(&idx) = self.dirs_by_path.get(path) {
            return idx;
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((dir, name)) => (self.ensure_dir(dir), name),
            None => (SpaceTree::ROOT, path),
        };
        let idx = self.add_child(parent, name, true, 0);
        self.dirs_by_path.insert(path.to_string(), idx);
        idx
    }

    fn finish(mut self, unreadable: u64) -> SpaceTree {
        // Roll up. Every node's parent has a smaller index, so walking the
        // arena backwards sees each child before its parent is read.
        for i in (1..self.nodes.len()).rev() {
            let (size, files, dirs, is_dir, parent) = {
                let n = &self.nodes[i];
                (n.size, n.files, n.dirs, n.is_dir, n.parent)
            };
            let Some(p) = parent else { continue };
            let pn = &mut self.nodes[p];
            pn.size = pn.size.saturating_add(size);
            if is_dir {
                pn.dirs = pn.dirs.saturating_add(dirs).saturating_add(1);
                pn.files = pn.files.saturating_add(files);
            } else {
                pn.files = pn.files.saturating_add(1);
            }
        }
        // Largest first is the whole point of the screen. Ties break on
        // name so two equal folders come out in a stable, readable order.
        let sizes: Vec<u64> = self.nodes.iter().map(|n| n.size).collect();
        let names: Vec<String> = self.nodes.iter().map(|n| n.name.to_lowercase()).collect();
        for n in &mut self.nodes {
            n.children.sort_by(|&a, &b| {
                sizes[b]
                    .cmp(&sizes[a])
                    .then_with(|| names[a].cmp(&names[b]))
            });
        }
        SpaceTree {
            nodes: self.nodes,
            unreadable,
        }
    }
}

/// `1,234 files, 12 folders` with each half dropped when zero, so an
/// all-file folder does not read "0 folders" every row.
pub fn count_phrase(files: u64, dirs: u64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if files > 0 || dirs == 0 {
        parts.push(format!(
            "{} {}",
            group_thousands(files),
            if files == 1 { "file" } else { "files" }
        ));
    }
    if dirs > 0 {
        parts.push(format!(
            "{} {}",
            group_thousands(dirs),
            if dirs == 1 { "folder" } else { "folders" }
        ));
    }
    parts.join(", ")
}

/// Whole-number percentage of `part` in `whole`, with the two ends
/// spelled out: a non-zero item never rounds to "0%" (it is "under 1%"),
/// and only an item that really is everything reads "100%".
pub fn format_share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".to_string();
    }
    if part >= whole {
        return "100%".to_string();
    }
    let pct = (part as u128 * 100 / whole as u128) as u64;
    if pct == 0 {
        if part == 0 {
            "0%".to_string()
        } else {
            "under 1%".to_string()
        }
    } else if pct >= 100 {
        // Rounding can't reach here (integer division floors), but keep
        // the guarantee local rather than relying on the branch above.
        "99%".to_string()
    } else {
        format!("{pct}%")
    }
}

/// `1234567` → `1,234,567`.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn format_size(n: u64) -> String {
    crate::listview::format_size(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, bytes: &[u8]) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, bytes).unwrap();
    }

    fn names(t: &SpaceTree, idx: usize) -> Vec<&str> {
        t.children(idx)
            .iter()
            .map(|&c| t.get(c).name.as_str())
            .collect()
    }

    fn child(t: &SpaceTree, parent: usize, name: &str) -> usize {
        *t.children(parent)
            .iter()
            .find(|&&c| t.get(c).name == name)
            .unwrap_or_else(|| panic!("no child {name:?} under {}", t.get(parent).name))
    }

    #[test]
    fn folder_sizes_are_rolled_up_from_files_and_children_sorted_largest_first() {
        let items = [
            ("small.txt", false, 10),
            ("big/a.bin", false, 500),
            ("big/b.bin", false, 400),
            ("big", true, 0),
            ("mid/deep/c.bin", false, 300),
            ("mid/deep", true, 0),
            ("mid", true, 0),
            ("empty", true, 0),
        ];
        let t = SpaceTree::from_items("root", items, 0);
        let r = t.root();
        assert_eq!(r.size, 1210);
        assert_eq!(r.files, 4);
        assert_eq!(r.dirs, 4); // big, mid, mid/deep, empty
        assert_eq!(
            names(&t, SpaceTree::ROOT),
            ["big", "mid", "small.txt", "empty"]
        );

        let big = child(&t, SpaceTree::ROOT, "big");
        assert_eq!(t.get(big).size, 900);
        assert_eq!(t.get(big).files, 2);
        assert_eq!(t.get(big).dirs, 0);
        let mid = child(&t, SpaceTree::ROOT, "mid");
        assert_eq!(t.get(mid).size, 300);
        assert_eq!(t.get(mid).files, 1);
        assert_eq!(t.get(mid).dirs, 1);
    }

    #[test]
    fn a_directorys_reported_size_is_discarded_not_double_counted() {
        // A backend that reports a real size on the directory entry must
        // not have it added on top of the files we sum ourselves.
        let items = [("d", true, 999_999), ("d/f", false, 5)];
        let t = SpaceTree::from_items("root", items, 0);
        assert_eq!(t.root().size, 5);
        assert_eq!(t.get(child(&t, SpaceTree::ROOT, "d")).size, 5);
    }

    #[test]
    fn missing_intermediate_directories_are_synthesised_once() {
        // Flat object stores list no directories at all. Both files share
        // the implied `a/b`, which must appear exactly once — and a later
        // explicit `a` entry must match the implied one, not duplicate it.
        let items = [
            ("a/b/one", false, 1),
            ("a/b/two", false, 2),
            ("a", true, 0),
            ("a/c", false, 4),
        ];
        let t = SpaceTree::from_items("root", items, 0);
        assert_eq!(names(&t, SpaceTree::ROOT), ["a"]);
        let a = child(&t, SpaceTree::ROOT, "a");
        assert_eq!(names(&t, a), ["c", "b"]); // 4 > 3
        let b = child(&t, a, "b");
        assert_eq!(t.get(b).size, 3);
        assert_eq!(t.get(b).files, 2);
        assert_eq!(t.root().dirs, 2);
    }

    #[test]
    fn remove_takes_the_subtree_out_of_every_ancestor() {
        let items = [
            ("keep.txt", false, 100),
            ("x/y/gone.bin", false, 700),
            ("x/y/also.bin", false, 200),
            ("x/other.bin", false, 50),
        ];
        let mut t = SpaceTree::from_items("root", items, 0);
        let x = child(&t, SpaceTree::ROOT, "x");
        let y = child(&t, x, "y");
        assert_eq!(t.root().size, 1050);
        assert_eq!(names(&t, x), ["y", "other.bin"]);

        let parent = t.remove(y);
        assert_eq!(parent, Some(x));
        assert_eq!(t.get(x).size, 50);
        assert_eq!(t.get(x).files, 1);
        assert_eq!(t.get(x).dirs, 0);
        assert_eq!(t.root().size, 150);
        assert_eq!(t.root().files, 2);
        assert_eq!(t.root().dirs, 1);
        assert_eq!(names(&t, x), ["other.bin"]);
        assert_eq!(t.get(y).parent, None);
        // Removing a file adjusts the file count, not the dir count.
        let keep = child(&t, SpaceTree::ROOT, "keep.txt");
        t.remove(keep);
        assert_eq!(t.root().size, 50);
        assert_eq!(t.root().files, 1);
        assert_eq!(t.root().dirs, 1);
    }

    #[test]
    fn the_root_cannot_be_removed() {
        let mut t = SpaceTree::from_items("root", [("f", false, 1)], 0);
        assert_eq!(t.remove(SpaceTree::ROOT), None);
        assert_eq!(t.root().size, 1);
    }

    #[test]
    fn components_name_the_path_from_the_root_down() {
        let t = SpaceTree::from_items("root", [("a/b/c.txt", false, 1)], 0);
        let a = child(&t, SpaceTree::ROOT, "a");
        let b = child(&t, a, "b");
        let c = child(&t, b, "c.txt");
        assert_eq!(t.components(c), ["a", "b", "c.txt"]);
        assert_eq!(t.components(SpaceTree::ROOT), Vec::<&str>::new());
        assert_eq!(t.ancestors(c), [b, a, SpaceTree::ROOT]);
    }

    #[test]
    fn row_labels_read_name_size_share_then_counts() {
        let items = [
            ("photos/a.jpg", false, 3 * 1024 * 1024),
            ("photos/b.jpg", false, 1024 * 1024),
            ("notes.txt", false, 1),
            ("void", true, 0),
        ];
        let t = SpaceTree::from_items(r"D:\stuff", items, 2);
        assert_eq!(
            t.row_label(SpaceTree::ROOT),
            r"D:\stuff — 4.0 MB, 3 files, 2 folders, 2 unreadable folders"
        );
        let photos = child(&t, SpaceTree::ROOT, "photos");
        assert_eq!(t.row_label(photos), "photos — 4.0 MB, 99%, 2 files");
        let a = child(&t, photos, "a.jpg");
        assert_eq!(t.row_label(a), "a.jpg — 3.0 MB, 75%");
        let notes = child(&t, SpaceTree::ROOT, "notes.txt");
        assert_eq!(t.row_label(notes), "notes.txt — 1 B, under 1%");
        let void = child(&t, SpaceTree::ROOT, "void");
        assert_eq!(t.row_label(void), "void — 0 B, 0%, empty");
    }

    #[test]
    fn share_never_rounds_a_real_item_to_zero_or_a_partial_one_to_everything() {
        assert_eq!(format_share(0, 100), "0%");
        assert_eq!(format_share(1, 1000), "under 1%");
        assert_eq!(format_share(50, 100), "50%");
        assert_eq!(format_share(999, 1000), "99%");
        assert_eq!(format_share(100, 100), "100%");
        assert_eq!(format_share(5, 0), "0%");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }

    #[test]
    fn count_phrase_drops_zero_halves_but_never_goes_blank() {
        assert_eq!(count_phrase(0, 0), "0 files");
        assert_eq!(count_phrase(1, 0), "1 file");
        assert_eq!(count_phrase(0, 1), "1 folder");
        assert_eq!(count_phrase(2, 3), "2 files, 3 folders");
    }

    #[test]
    fn local_scan_matches_the_flat_builder_for_the_same_tree() {
        // The two producers must agree: build the same folder both ways
        // and compare everything but the root label.
        let td = tempfile::tempdir().unwrap();
        write(&td.path().join("a.txt"), b"hello");
        write(&td.path().join("sub/b.bin"), &[0u8; 300]);
        write(&td.path().join("sub/nested/c.bin"), &[0u8; 40]);
        fs::create_dir_all(td.path().join("empty")).unwrap();

        let root = NavPath::new(td.path()).unwrap();
        let walked = SpaceTree::scan_local(&root);
        let flat = SpaceTree::from_items(
            &root.to_string(),
            [
                ("a.txt", false, 5),
                ("sub", true, 0),
                ("sub/b.bin", false, 300),
                ("sub/nested", true, 0),
                ("sub/nested/c.bin", false, 40),
                ("empty", true, 0),
            ],
            0,
        );
        assert_eq!(walked.root().size, 345);
        assert_eq!(walked.root().files, 3);
        assert_eq!(walked.root().dirs, 3);
        assert_eq!(walked.unreadable, 0);
        assert_eq!(
            names(&walked, SpaceTree::ROOT),
            names(&flat, SpaceTree::ROOT)
        );
        let ws = child(&walked, SpaceTree::ROOT, "sub");
        let fs_ = child(&flat, SpaceTree::ROOT, "sub");
        assert_eq!(walked.get(ws).size, flat.get(fs_).size);
        assert_eq!(names(&walked, ws), names(&flat, fs_));
        assert_eq!(walked.row_label(ws), flat.row_label(fs_));
    }
}
