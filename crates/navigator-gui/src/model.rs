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

#[derive(Copy, Clone, Debug)]
pub struct Sort {
    pub mode: SortMode,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Self { Self { mode: SortMode::Name, descending: false } }
}

#[derive(Clone, Default)]
pub struct Model(Arc<RwLock<ModelInner>>);

#[derive(Default)]
struct ModelInner {
    cwd: Option<NavPath>,
    all: Vec<Entry>,
    visible: Vec<u32>,          // indices into `all`
    selection: Selection,
    filter: Filter,
    sort: Sort,
    /// Search-result mode: entry names are relative paths below `cwd` and
    /// the sort sticks to the order set by the searcher (scan order).
    search_mode: bool,
}

impl Model {
    pub fn new() -> Self { Self::default() }

    pub fn cwd(&self) -> Option<NavPath> { self.0.read().cwd.clone() }

    pub fn len(&self) -> usize { self.0.read().visible.len() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// Fetch the `i`-th visible entry.
    pub fn get(&self, i: usize) -> Option<Entry> {
        let g = self.0.read();
        g.visible.get(i).and_then(|&idx| g.all.get(idx as usize)).cloned()
    }

    pub fn filter(&self) -> Filter { self.0.read().filter }

    pub fn set_filter(&self, filter: Filter) -> usize {
        let mut g = self.0.write();
        g.filter = filter;
        rebuild_visible(&mut g);
        g.visible.len()
    }

    pub fn sort(&self) -> Sort { self.0.read().sort }

    pub fn set_sort(&self, sort: Sort) -> usize {
        let mut g = self.0.write();
        g.sort = sort;
        if !g.search_mode {
            sort_entries(&mut g.all, sort);
            rebuild_visible(&mut g);
        }
        g.visible.len()
    }

    pub fn is_search_mode(&self) -> bool { self.0.read().search_mode }

    /// Replace the raw listing. Returns the new visible length so the caller
    /// can update the ListView's virtual item count in one call.
    pub fn set_listing(&self, cwd: NavPath, mut entries: Vec<Entry>) -> usize {
        let sort = self.0.read().sort;
        sort_entries(&mut entries, sort);
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

    pub fn selection_snapshot(&self) -> Selection { self.0.read().selection.clone() }

    pub fn selected_paths(&self) -> Vec<NavPath> {
        let g = self.0.read();
        let Some(cwd) = g.cwd.as_ref() else { return Vec::new(); };
        g.selection
            .iter()
            .filter_map(|i| g.visible.get(i).and_then(|&idx| g.all.get(idx as usize)))
            .map(|e| cwd.join(&e.name))
            .collect()
    }

    /// Visible index of the first entry matching `pred`. Used after
    /// navigate-up to re-focus the child directory we just left.
    pub fn index_of(&self, mut pred: impl FnMut(&Entry) -> bool) -> Option<usize> {
        let g = self.0.read();
        g.visible.iter().enumerate().find_map(|(vi, &ai)| {
            g.all.get(ai as usize).filter(|e| pred(e)).map(|_| vi)
        })
    }

    /// Find the first visible index whose name starts with `prefix` (ASCII
    /// case-insensitive), optionally resuming past `from`. Returns `None`
    /// when nothing matches.
    pub fn find_prefix(&self, prefix: &str, from: Option<usize>) -> Option<usize> {
        if prefix.is_empty() { return None; }
        let g = self.0.read();
        let len = g.visible.len();
        if len == 0 { return None; }
        let start = from.map(|i| (i + 1) % len).unwrap_or(0);
        let prefix_lc: String = prefix.to_ascii_lowercase();
        for step in 0..len {
            let i = (start + step) % len;
            let idx = g.visible[i] as usize;
            if let Some(e) = g.all.get(idx) {
                if e.name.to_ascii_lowercase().starts_with(&prefix_lc) {
                    return Some(i);
                }
            }
        }
        None
    }
}

fn rebuild_visible(inner: &mut ModelInner) {
    let f = inner.filter;
    inner.visible.clear();
    for (idx, e) in inner.all.iter().enumerate() {
        if e.hidden && !f.show_hidden { continue; }
        if e.system && !f.show_system { continue; }
        inner.visible.push(idx as u32);
    }
}

/// Directories first, then the chosen key. Explorer-style: folders always
/// cluster at the top regardless of key, flipping `descending` only
/// reverses the within-kind order.
pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    use std::cmp::Ordering;
    entries.sort_by(|a, b| {
        match (a.is_dir(), b.is_dir()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => {
                let key = match sort.mode {
                    SortMode::Name     => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortMode::Size     => a.size.cmp(&b.size),
                    SortMode::Type     => type_key(a).cmp(&type_key(b))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                    SortMode::Modified => a.modified.cmp(&b.modified),
                    SortMode::Created  => a.created.cmp(&b.created),
                };
                if sort.descending { key.reverse() } else { key }
            }
        }
    });
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
