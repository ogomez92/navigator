//! Backing model for the list view. Lives on the UI thread; directory
//! scans happen on a worker thread and post results back via `WM_APP + N`.
//!
//! We keep the *raw* scan in `all` and publish a filtered view in `visible`.
//! Toggling "show hidden" / "show system" rebuilds `visible` without needing
//! a new scan.

use std::sync::Arc;

use parking_lot::RwLock;

use navigator_config::SortMode;
use navigator_core::{Entry, NavPath, Selection};

#[derive(Copy, Clone, Debug, Default)]
pub struct Filter {
    pub show_hidden: bool,
    pub show_system: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Sort {
    pub mode: SortMode,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            mode: SortMode::Name,
            descending: false,
        }
    }
}

#[derive(Clone, Default)]
pub struct Model(Arc<RwLock<ModelInner>>);

#[derive(Default)]
struct ModelInner {
    cwd: Option<NavPath>,
    all: Vec<Entry>,
    visible: Vec<u32>, // indices into `all`
    selection: Selection,
    filter: Filter,
    sort: Sort,
    /// Search-result mode: entry names are relative paths below `cwd` and
    /// the sort sticks to the order set by the searcher (scan order).
    search_mode: bool,
}

impl Model {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cwd(&self) -> Option<NavPath> {
        self.0.read().cwd.clone()
    }

    pub fn len(&self) -> usize {
        self.0.read().visible.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fetch the `i`-th visible entry.
    pub fn get(&self, i: usize) -> Option<Entry> {
        let g = self.0.read();
        g.visible
            .get(i)
            .and_then(|&idx| g.all.get(idx as usize))
            .cloned()
    }

    pub fn filter(&self) -> Filter {
        self.0.read().filter
    }

    pub fn set_filter(&self, filter: Filter) -> usize {
        let mut g = self.0.write();
        g.filter = filter;
        rebuild_visible(&mut g);
        g.visible.len()
    }

    pub fn sort(&self) -> Sort {
        self.0.read().sort
    }

    /// Record the sort preference. Deliberately does **not** re-sort the
    /// loaded entries: every caller follows this with a `refresh()`, and
    /// the rescan carries the new preference to the scan worker, which
    /// sorts off the UI thread. Sorting here as well would do the same
    /// n log n twice, the first time in the message pump.
    pub fn set_sort(&self, sort: Sort) -> usize {
        let mut g = self.0.write();
        g.sort = sort;
        g.visible.len()
    }

    pub fn is_search_mode(&self) -> bool {
        self.0.read().search_mode
    }

    /// Replace the raw listing with entries in arbitrary order, sorting
    /// them here. Returns the new visible length so the caller can update
    /// the ListView's virtual item count in one call.
    pub fn set_listing(&self, cwd: NavPath, entries: Vec<Entry>) -> usize {
        self.set_listing_presorted(cwd, entries, None)
    }

    /// Replace the raw listing with a vector the producer has already
    /// ordered. `sorted_as` is the order it used.
    ///
    /// The scan worker sorts before posting so a large folder doesn't
    /// spend its n log n inside the message pump, where the window is
    /// unresponsive. When the tag matches the current preference the
    /// vector is installed as-is. It can fail to match — the user is free
    /// to change the sort mode while a scan is in flight — and then this
    /// is the cheap correctness net that re-sorts.
    pub fn set_listing_presorted(
        &self,
        cwd: NavPath,
        mut entries: Vec<Entry>,
        sorted_as: Option<Sort>,
    ) -> usize {
        let sort = self.0.read().sort;
        if sorted_as != Some(sort) {
            sort_entries(&mut entries, sort);
        }
        let mut g = self.0.write();
        g.cwd = Some(cwd);
        g.all = entries;
        g.selection.clear();
        g.search_mode = false;
        rebuild_visible(&mut g);
        g.visible.len()
    }

    /// Replace the listing with search results. `entries` must carry
    /// relative paths in their `name` field (joined to `root` on open).
    pub fn set_search_results(&self, root: NavPath, entries: Vec<Entry>) -> usize {
        let mut g = self.0.write();
        g.cwd = Some(root);
        g.all = entries;
        g.selection.clear();
        g.search_mode = true;
        rebuild_visible(&mut g);
        g.visible.len()
    }

    /// Append entries to an existing listing without re-sorting — used by
    /// the file watcher when `new_items_at_bottom` is on.
    pub fn append_entries(&self, mut entries: Vec<Entry>) -> usize {
        let mut g = self.0.write();
        g.all.append(&mut entries);
        rebuild_visible(&mut g);
        g.visible.len()
    }

    /// Remove every entry whose name matches `name`. Used by the file
    /// watcher on delete notifications. Returns the new visible count.
    pub fn remove_by_name(&self, name: &str) -> usize {
        let mut g = self.0.write();
        g.all.retain(|e| e.name != name);
        rebuild_visible(&mut g);
        g.visible.len()
    }

    /// Replace the entry with matching `name`. Used by the watcher on
    /// Modify events so the displayed size/mtime reflect reality. Returns
    /// the *visible* index of the updated row if found, so the window can
    /// invalidate just that row rather than repainting the whole list.
    pub fn update_entry(&self, name: &str, new_entry: Entry) -> Option<usize> {
        let mut g = self.0.write();
        let raw_idx = g.all.iter().position(|e| e.name == name)?;
        g.all[raw_idx] = new_entry;
        // Find where that raw index lives in the filtered view, if visible.
        g.visible.iter().position(|&v| v as usize == raw_idx)
    }

    pub fn with_selection<R>(&self, f: impl FnOnce(&mut Selection) -> R) -> R {
        let mut g = self.0.write();
        f(&mut g.selection)
    }

    pub fn selection_snapshot(&self) -> Selection {
        self.0.read().selection.clone()
    }

    /// Name of the currently focused entry, if any.
    ///
    /// Read *before* a new listing is installed so the caret can be put
    /// back on the same row afterwards: indices don't survive a re-listing
    /// (the folder may have gained or lost entries, or been re-sorted), but
    /// the name does.
    pub fn focused_name(&self) -> Option<String> {
        let g = self.0.read();
        let vi = g.selection.focus()?;
        g.visible
            .get(vi)
            .and_then(|&ai| g.all.get(ai as usize))
            .map(|e| e.name.clone())
    }

    pub fn selected_paths(&self) -> Vec<NavPath> {
        let g = self.0.read();
        let Some(cwd) = g.cwd.as_ref() else {
            return Vec::new();
        };
        g.selection
            .iter()
            .filter_map(|i| g.visible.get(i).and_then(|&idx| g.all.get(idx as usize)))
            .map(|e| cwd.join(&e.name))
            .collect()
    }

    /// Like [`selected_paths`](Self::selected_paths) but also reports
    /// whether each entry is a directory. Delete uses this to pick the
    /// right rclone verb for remote targets — `purge` for directories,
    /// `deletefile` for files (the verbs are not interchangeable).
    pub fn selected_paths_with_kind(&self) -> Vec<(NavPath, bool)> {
        let g = self.0.read();
        let Some(cwd) = g.cwd.as_ref() else {
            return Vec::new();
        };
        g.selection
            .iter()
            .filter_map(|i| g.visible.get(i).and_then(|&idx| g.all.get(idx as usize)))
            .map(|e| (cwd.join(&e.name), e.is_dir()))
            .collect()
    }

    /// Visible index of the first entry matching `pred`. Used after
    /// navigate-up to re-focus the child directory we just left.
    pub fn index_of(&self, mut pred: impl FnMut(&Entry) -> bool) -> Option<usize> {
        let g = self.0.read();
        g.visible
            .iter()
            .enumerate()
            .find_map(|(vi, &ai)| g.all.get(ai as usize).filter(|e| pred(e)).map(|_| vi))
    }

    /// Find the first visible index whose name starts with `prefix` (ASCII
    /// case-insensitive), optionally resuming past `from`. Returns `None`
    /// when nothing matches.
    pub fn find_prefix(&self, prefix: &str, from: Option<usize>) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let g = self.0.read();
        let len = g.visible.len();
        if len == 0 {
            return None;
        }
        let start = from.map(|i| (i + 1) % len).unwrap_or(0);
        let prefix_lc: String = prefix.to_ascii_lowercase();
        for step in 0..len {
            let i = (start + step) % len;
            let idx = g.visible[i] as usize;
            if let Some(e) = g.all.get(idx)
                && e.name.to_ascii_lowercase().starts_with(&prefix_lc)
            {
                return Some(i);
            }
        }
        None
    }
}

fn rebuild_visible(inner: &mut ModelInner) {
    let f = inner.filter;
    inner.visible.clear();
    for (idx, e) in inner.all.iter().enumerate() {
        if e.hidden && !f.show_hidden {
            continue;
        }
        if e.system && !f.show_system {
            continue;
        }
        inner.visible.push(idx as u32);
    }
}

/// Directories first, then the chosen key. Explorer-style: folders always
/// cluster at the top regardless of key, flipping `descending` only
/// reverses the within-kind order.
///
/// **Keys are computed once per entry, not once per comparison.** The
/// Name and Type comparators need a case-folded name, and building it
/// inside the closure meant two `String` allocations on every one of the
/// n log n comparisons — for a 200k-entry folder, several million
/// allocations for a sort that only needs 200k. Decorating up front makes
/// it linear in allocations and lets the comparison itself be a plain
/// memcmp on borrowed slices.
pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    use std::cmp::Ordering;

    // Only the key-bearing modes need a decoration pass; Size / Modified
    // / Created compare integers already in the Entry.
    let folded: Vec<(String, String)> = match sort.mode {
        SortMode::Name => entries
            .iter()
            .map(|e| (e.name.to_lowercase(), String::new()))
            .collect(),
        SortMode::Type => entries
            .iter()
            .map(|e| (e.name.to_lowercase(), type_key(e)))
            .collect(),
        _ => Vec::new(),
    };

    // Sort an index permutation so the (possibly large) Entry values move
    // exactly once, at the end, instead of on every swap.
    let mut order: Vec<u32> = (0..entries.len() as u32).collect();
    order.sort_by(|&ia, &ib| {
        let (a, b) = (&entries[ia as usize], &entries[ib as usize]);
        match (a.is_dir(), b.is_dir()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => {
                let key = match sort.mode {
                    SortMode::Name => folded[ia as usize].0.cmp(&folded[ib as usize].0),
                    SortMode::Size => a.size.cmp(&b.size),
                    SortMode::Type => folded[ia as usize]
                        .1
                        .cmp(&folded[ib as usize].1)
                        .then_with(|| folded[ia as usize].0.cmp(&folded[ib as usize].0)),
                    SortMode::Modified => a.modified.cmp(&b.modified),
                    SortMode::Created => a.created.cmp(&b.created),
                };
                if sort.descending { key.reverse() } else { key }
            }
        }
    });

    apply_permutation(entries, &order);
}

/// Reorder `entries` in place so that afterwards `entries[i]` is whatever
/// used to live at `order[i]`. Walks each permutation cycle once, so it is
/// O(n) moves and needs no second `Vec<Entry>`.
fn apply_permutation(entries: &mut [Entry], order: &[u32]) {
    debug_assert_eq!(entries.len(), order.len());
    // `pos[j]` tracks where the element originally at j currently sits, so
    // a cycle can be rotated with plain swaps.
    let mut pos: Vec<u32> = (0..entries.len() as u32).collect();
    let mut at: Vec<u32> = (0..entries.len() as u32).collect();
    for i in 0..order.len() {
        let want = order[i];
        let cur = pos[want as usize] as usize;
        if cur == i {
            continue;
        }
        entries.swap(i, cur);
        let displaced = at[i];
        at.swap(i, cur);
        pos[want as usize] = i as u32;
        pos[displaced as usize] = cur as u32;
    }
}

/// Type-sort key for a single entry: the file extension (lowercased).
/// Extensionless files sort before extensioned ones so a folder's README
/// and Makefile cluster together. Independent of whether the Type column
/// is currently visible — sort by type still works after hiding it.
pub fn type_key(e: &Entry) -> String {
    std::path::Path::new(&e.name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use navigator_core::{EntryKind, FileTime};

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: 0,
            modified: FileTime::default(),
            created: FileTime::default(),
            attrs: 0,
            hidden: false,
            system: false,
        }
    }

    fn sized(name: &str, size: u64, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            kind: if is_dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            size,
            modified: FileTime(size),
            created: FileTime(size),
            attrs: 0,
            hidden: false,
            system: false,
        }
    }

    /// The straightforward comparator the decorate/permute version
    /// replaced. Kept in the tests as the oracle: whatever the fast path
    /// does, it must land on exactly the order this produces.
    fn reference_sort(entries: &mut [Entry], sort: Sort) {
        use std::cmp::Ordering;
        entries.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => {
                let key = match sort.mode {
                    SortMode::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortMode::Size => a.size.cmp(&b.size),
                    SortMode::Type => type_key(a)
                        .cmp(&type_key(b))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                    SortMode::Modified => a.modified.cmp(&b.modified),
                    SortMode::Created => a.created.cmp(&b.created),
                };
                if sort.descending { key.reverse() } else { key }
            }
        });
    }

    /// `sort_entries` now decorates keys up front and permutes an index
    /// vector rather than comparing (and re-lowercasing) entries directly.
    /// That is a pure speed change — the resulting order must be
    /// byte-identical to the old comparator for every mode and direction,
    /// including the folders-first tiebreak and duplicate keys.
    #[test]
    fn fast_sort_matches_the_reference_comparator_for_every_mode() {
        let input = vec![
            sized("Zebra.txt", 30, false),
            sized("alpha.rs", 10, false),
            sized("docs", 0, true),
            sized("BETA.rs", 10, false),
            sized("Archive", 0, true),
            sized("readme", 5, false),
            sized("gamma.txt", 30, false),
            sized("alpha.rs", 99, false), // duplicate name, different size
            sized("zeta", 1, false),
        ];

        for mode in [
            SortMode::Name,
            SortMode::Size,
            SortMode::Type,
            SortMode::Modified,
            SortMode::Created,
        ] {
            for descending in [false, true] {
                let sort = Sort { mode, descending };
                let mut fast = input.clone();
                sort_entries(&mut fast, sort);
                let mut slow = input.clone();
                reference_sort(&mut slow, sort);

                let fast_names: Vec<(&str, u64)> =
                    fast.iter().map(|e| (e.name.as_str(), e.size)).collect();
                let slow_names: Vec<(&str, u64)> =
                    slow.iter().map(|e| (e.name.as_str(), e.size)).collect();
                assert_eq!(
                    fast_names, slow_names,
                    "mode={mode:?} descending={descending} diverged from the reference order"
                );
            }
        }
    }

    /// `apply_permutation` rotates cycles in place with swaps. A long
    /// single cycle and a fully reversed input are the two shapes most
    /// likely to expose an index-bookkeeping slip, so pin both — a wrong
    /// permutation silently scrambles the listing rather than erroring.
    #[test]
    fn permutation_survives_long_cycles_and_full_reversal() {
        let names: Vec<String> = (0..64).map(|i| format!("f{i:03}")).collect();

        // Full reversal: every element moves, one long cycle pair-wise.
        let mut entries: Vec<Entry> = names.iter().map(|n| file(n)).collect();
        let order: Vec<u32> = (0..64u32).rev().collect();
        apply_permutation(&mut entries, &order);
        let got: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        let want: Vec<&str> = names.iter().rev().map(|s| s.as_str()).collect();
        assert_eq!(got, want, "reversal permutation");

        // Rotate-by-one: a single 64-long cycle.
        let mut entries: Vec<Entry> = names.iter().map(|n| file(n)).collect();
        let order: Vec<u32> = (0..64u32).map(|i| (i + 1) % 64).collect();
        apply_permutation(&mut entries, &order);
        let got: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        let want: Vec<&str> = (0..64).map(|i| names[(i + 1) % 64].as_str()).collect();
        assert_eq!(got, want, "rotation permutation");
    }

    /// The scan worker pre-sorts, so a listing tagged with the order it
    /// already carries must be installed untouched — that is the whole
    /// point of moving the work off the UI thread. But a preference the
    /// user changed mid-scan has to be honoured, so a mismatched tag
    /// re-sorts.
    #[test]
    fn presorted_listing_is_trusted_only_when_the_tag_matches() {
        let m = Model::new();
        let cwd = NavPath::new(std::path::PathBuf::from(if cfg!(windows) {
            r"C:\tmp\nav"
        } else {
            "/tmp/nav"
        }))
        .unwrap();
        let by_name = Sort {
            mode: SortMode::Name,
            descending: false,
        };
        m.set_sort(by_name);

        // Tagged with the active sort but deliberately out of order: the
        // model must take our word for it and not re-sort.
        m.set_listing_presorted(cwd.clone(), vec![file("zzz"), file("aaa")], Some(by_name));
        let got: Vec<String> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
        assert_eq!(got, vec!["zzz", "aaa"], "matching tag must be trusted");

        // Same entries tagged with a different sort → re-sorted here.
        m.set_listing_presorted(
            cwd,
            vec![file("zzz"), file("aaa")],
            Some(Sort {
                mode: SortMode::Size,
                descending: false,
            }),
        );
        let got: Vec<String> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
        assert_eq!(got, vec!["aaa", "zzz"], "stale tag must trigger a re-sort");
    }

    /// `append_entries` is the path the Extract worker relies on (via the
    /// directory watcher) to surface freshly-extracted files without a
    /// re-sort. The new entries must land at the bottom; the previous
    /// listing's order must not be perturbed.
    #[test]
    fn append_entries_preserves_existing_order_and_appends_at_end() {
        let m = Model::new();
        let cwd = NavPath::new(std::path::PathBuf::from(if cfg!(windows) {
            r"C:\tmp\nav"
        } else {
            "/tmp/nav"
        }))
        .unwrap();
        m.set_listing(cwd, vec![file("alpha"), file("bravo"), file("charlie")]);
        // Existing order is the sorted view.
        let before: Vec<String> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
        assert_eq!(before, vec!["alpha", "bravo", "charlie"]);

        // Simulate two new files showing up after the user extracted an
        // archive: `aaaa` (would sort first) and `zeta`. With append-mode
        // the watcher hands these straight to `append_entries`, and they
        // should both land at the bottom regardless of their sort order.
        m.append_entries(vec![file("aaaa"), file("zeta")]);
        let after: Vec<String> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
        assert_eq!(after, vec!["alpha", "bravo", "charlie", "aaaa", "zeta"]);
    }

    /// Type-ahead cycling depends on `find_prefix` resuming *past* the
    /// current focus and wrapping. Focused on `he`, a single `h` must
    /// advance to the next `h` entry (`ho`) rather than snapping back to
    /// the first one (`ha`) — the bug behind the type-ahead fix.
    #[test]
    fn find_prefix_resumes_past_focus_and_wraps() {
        let m = Model::new();
        let cwd = NavPath::new(std::path::PathBuf::from(if cfg!(windows) {
            r"C:\tmp\nav"
        } else {
            "/tmp/nav"
        }))
        .unwrap();
        // Sorted view: ha(0), he(1), ho(2).
        m.set_listing(cwd, vec![file("ha"), file("he"), file("ho")]);

        // Focused on `he` (index 1): single `h` advances to `ho`.
        assert_eq!(m.find_prefix("h", Some(1)), Some(2));
        // From `ho` it wraps around to `ha`.
        assert_eq!(m.find_prefix("h", Some(2)), Some(0));
        // From the top (no focus) it still lands on the first match.
        assert_eq!(m.find_prefix("h", None), Some(0));
        // A `from` that is itself the sole match returns itself (wrap).
        assert_eq!(m.find_prefix("he", Some(1)), Some(1));
    }

    /// Search-mode rows store relative paths (`subA\\file.ext`). Copy-paths,
    /// activate, and the context menu all funnel through `selected_paths`,
    /// which must join those relative names onto the search root to yield
    /// real absolute paths — otherwise Ctrl+Shift+C would copy the bare
    /// relative form and the shell context menu would build PIDLs against
    /// nothing.
    #[cfg(windows)]
    #[test]
    fn selected_paths_in_search_mode_reconstruct_full_paths() {
        let m = Model::new();
        let root = NavPath::new(std::path::PathBuf::from(r"C:\tmp\nav")).unwrap();
        m.set_search_results(
            root.clone(),
            vec![file(r"subA\file.ext"), file(r"subB\nested\other.txt")],
        );
        // Select both rows.
        m.with_selection(|sel| {
            sel.insert(0);
            sel.insert(1);
        });
        let paths: Vec<String> = m.selected_paths().iter().map(|p| p.to_string()).collect();
        assert_eq!(
            paths,
            vec![
                String::from(r"C:\tmp\nav\subA\file.ext"),
                String::from(r"C:\tmp\nav\subB\nested\other.txt"),
            ],
        );
    }
}
