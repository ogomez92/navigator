//! Model unit tests. Exercises filtering, prefix search, and selection
//! projection — all the logic that sits between raw scan results and the
//! virtual ListView.

#![cfg(windows)]

use navigator_core::{Entry, EntryKind, FileTime, NavPath};
use navigator_gui::model::{Filter, Model};

fn entry(name: &str, kind: EntryKind, hidden: bool, system: bool) -> Entry {
    Entry {
        name: name.into(),
        kind,
        size: 0,
        modified: FileTime::default(),
        created: FileTime::default(),
        attrs: 0,
        hidden,
        system,
    }
}

fn entry_full(
    name: &str, kind: EntryKind,
    size: u64, modified: u64, created: u64,
) -> Entry {
    Entry {
        name: name.into(), kind, size,
        modified: FileTime(modified),
        created: FileTime(created),
        attrs: 0, hidden: false, system: false,
    }
}

fn file(name: &str) -> Entry { entry(name, EntryKind::File, false, false) }
fn dir(name: &str) -> Entry { entry(name, EntryKind::Directory, false, false) }

fn cwd() -> NavPath { NavPath::new(r"C:\ignored").unwrap() }

#[test]
fn default_sort_puts_directories_first() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        file("zeta.txt"),
        dir("alpha"),
        file("alpha.txt"),
        dir("zeta_dir"),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    // Dirs come first, alphabetically case-insensitive.
    assert_eq!(names, vec!["alpha", "zeta_dir", "alpha.txt", "zeta.txt"]);
}

#[test]
fn hidden_filter_excludes_by_default() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        file("visible.txt"),
        entry(".config", EntryKind::File, true, false),
    ]);
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(0).unwrap().name, "visible.txt");
}

#[test]
fn hidden_filter_shows_when_enabled() {
    let m = Model::new();
    m.set_filter(Filter { show_hidden: true, show_system: false });
    m.set_listing(cwd(), vec![
        file("visible.txt"),
        entry(".config", EntryKind::File, true, false),
    ]);
    assert_eq!(m.len(), 2);
}

#[test]
fn system_filter_independent_of_hidden() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        entry("pagefile.sys", EntryKind::File, false, true),
        entry("normal.txt", EntryKind::File, false, false),
    ]);
    assert_eq!(m.len(), 1);
    m.set_filter(Filter { show_hidden: false, show_system: true });
    assert_eq!(m.len(), 2);
}

#[test]
fn toggle_filter_rebuilds_view_without_reloading() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        file("a"),
        entry("b", EntryKind::File, true, false),
        entry("c", EntryKind::File, false, true),
    ]);
    assert_eq!(m.len(), 1);
    m.set_filter(Filter { show_hidden: true, show_system: true });
    assert_eq!(m.len(), 3);
    m.set_filter(Filter { show_hidden: false, show_system: false });
    assert_eq!(m.len(), 1);
}

#[test]
fn find_prefix_matches_first_occurrence() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        dir("Options"),
        dir("OneDrive"),
        file("readme.md"),
    ]);
    // Sorted: "OneDrive", "Options", "readme.md".
    let i = m.find_prefix("o", None).expect("has match");
    assert_eq!(m.get(i).unwrap().name, "OneDrive");

    // Typing "op" must jump to "Options" even when starting from "OneDrive".
    let j = m.find_prefix("op", None).expect("has op match");
    assert_eq!(m.get(j).unwrap().name, "Options");
}

#[test]
fn find_prefix_resumes_after_current() {
    let m = Model::new();
    m.set_listing(cwd(), vec![dir("apple"), dir("apricot"), dir("banana")]);
    let first = m.find_prefix("a", None).unwrap();
    assert_eq!(m.get(first).unwrap().name, "apple");
    let next = m.find_prefix("a", Some(first)).unwrap();
    assert_eq!(m.get(next).unwrap().name, "apricot");
    // Wraps back to first after all matches exhausted.
    let wrap = m.find_prefix("a", Some(next)).unwrap();
    assert_eq!(m.get(wrap).unwrap().name, "apple");
}

#[test]
fn find_prefix_case_insensitive() {
    let m = Model::new();
    m.set_listing(cwd(), vec![dir("Options"), dir("Projects")]);
    assert!(m.find_prefix("OPT", None).is_some());
    assert!(m.find_prefix("opt", None).is_some());
    assert!(m.find_prefix("Pro", None).is_some());
}

#[test]
fn find_prefix_returns_none_on_empty_model() {
    let m = Model::new();
    m.set_listing(cwd(), vec![]);
    assert!(m.find_prefix("x", None).is_none());
}

#[test]
fn find_prefix_empty_input_returns_none() {
    let m = Model::new();
    m.set_listing(cwd(), vec![dir("a")]);
    assert!(m.find_prefix("", None).is_none());
}

#[test]
fn selected_paths_joins_cwd_and_name() {
    let m = Model::new();
    m.set_listing(cwd(), vec![file("alpha"), file("beta"), file("gamma")]);
    m.with_selection(|s| { s.toggle(0); s.toggle(2); });
    let paths: Vec<_> = m.selected_paths().iter().map(|p| p.file_name().to_string()).collect();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&"alpha".to_string()));
    assert!(paths.contains(&"gamma".to_string()));
}

#[test]
fn selected_paths_tracks_visible_not_raw_indices() {
    // With a hidden file between two visible files, selecting visible
    // index 1 must map to the second visible file, not the hidden one.
    let m = Model::new();
    m.set_listing(cwd(), vec![
        file("a"),
        entry("hidden", EntryKind::File, true, false),
        file("b"),
    ]);
    assert_eq!(m.len(), 2); // hidden dropped
    m.with_selection(|s| { s.set_single(1); });
    let paths = m.selected_paths();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].file_name(), "b");
}

#[test]
fn sort_by_size_ascending() {
    use navigator_config::SortMode;
    use navigator_gui::model::Sort;

    let m = Model::new();
    m.set_sort(Sort { mode: SortMode::Size, descending: false });
    m.set_listing(cwd(), vec![
        entry_full("big",    EntryKind::File, 1_000_000, 0, 0),
        entry_full("small",  EntryKind::File, 100,       0, 0),
        entry_full("medium", EntryKind::File, 10_000,    0, 0),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec!["small", "medium", "big"]);
}

#[test]
fn sort_by_modified_descending_places_newest_first() {
    use navigator_config::SortMode;
    use navigator_gui::model::Sort;

    let m = Model::new();
    m.set_sort(Sort { mode: SortMode::Modified, descending: true });
    m.set_listing(cwd(), vec![
        entry_full("old",    EntryKind::File, 0, 100, 0),
        entry_full("newer",  EntryKind::File, 0, 200, 0),
        entry_full("newest", EntryKind::File, 0, 300, 0),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec!["newest", "newer", "old"]);
}

#[test]
fn sort_by_created_distinct_from_modified() {
    use navigator_config::SortMode;
    use navigator_gui::model::Sort;

    let m = Model::new();
    m.set_sort(Sort { mode: SortMode::Created, descending: false });
    m.set_listing(cwd(), vec![
        entry_full("b", EntryKind::File, 0, /*mod*/ 999, /*created*/ 100),
        entry_full("a", EntryKind::File, 0, /*mod*/ 100, /*created*/ 200),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    // Modified times would sort a-before-b; created times reverse it.
    assert_eq!(names, vec!["b", "a"]);
}

#[test]
fn directories_always_cluster_first_regardless_of_sort_key() {
    use navigator_config::SortMode;
    use navigator_gui::model::Sort;

    let m = Model::new();
    m.set_sort(Sort { mode: SortMode::Size, descending: false });
    m.set_listing(cwd(), vec![
        entry_full("huge_file", EntryKind::File,      1_000_000, 0, 0),
        entry_full("small_dir", EntryKind::Directory, 0,         0, 0),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec!["small_dir", "huge_file"]);
}

#[test]
fn append_entries_does_not_re_sort() {
    let m = Model::new();
    m.set_listing(cwd(), vec![file("a"), file("c")]);
    // Normal sort would put 'b' between 'a' and 'c'. Append keeps it at end.
    m.append_entries(vec![file("b")]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec!["a", "c", "b"]);
}

#[test]
fn remove_by_name_drops_entry() {
    let m = Model::new();
    m.set_listing(cwd(), vec![file("keep"), file("drop")]);
    assert_eq!(m.len(), 2);
    m.remove_by_name("drop");
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(0).unwrap().name, "keep");
}

#[test]
fn search_mode_keeps_provided_order() {
    let m = Model::new();
    // Purposefully unsorted — `set_search_results` must not re-sort.
    m.set_search_results(cwd(), vec![
        file(r"sub\zeta.txt"),
        file(r"alpha.txt"),
        file(r"sub\beta.txt"),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec![r"sub\zeta.txt", r"alpha.txt", r"sub\beta.txt"]);
    assert!(m.is_search_mode());
}

#[test]
fn set_listing_exits_search_mode() {
    let m = Model::new();
    m.set_search_results(cwd(), vec![file("match")]);
    assert!(m.is_search_mode());
    m.set_listing(cwd(), vec![file("plain")]);
    assert!(!m.is_search_mode());
}

#[test]
fn update_entry_replaces_attributes_in_place() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        entry_full("a", EntryKind::File, 10, 100, 100),
        entry_full("b", EntryKind::File, 20, 100, 100),
    ]);
    let before = m.get(0).unwrap();
    assert_eq!(before.name, "a");
    assert_eq!(before.size, 10);

    // Modify event → watcher re-stats and calls update_entry. Size grew.
    let updated = entry_full("a", EntryKind::File, 9999, 500, 100);
    let vis = m.update_entry("a", updated).expect("entry exists");
    assert_eq!(vis, 0);
    assert_eq!(m.get(0).unwrap().size, 9999);
    // Siblings untouched.
    assert_eq!(m.get(1).unwrap().name, "b");
}

#[test]
fn update_entry_returns_none_when_missing() {
    let m = Model::new();
    m.set_listing(cwd(), vec![file("only")]);
    let r = m.update_entry("missing", file("missing"));
    assert!(r.is_none());
}

#[test]
fn update_entry_returns_none_when_hidden_by_filter() {
    let m = Model::new();
    m.set_listing(cwd(), vec![
        entry("a", EntryKind::File, false, false),
        entry("secret", EntryKind::File, true, false),
    ]);
    // `secret` is filtered out. Updating it should succeed at the raw
    // level but not expose a visible index the caller could redraw.
    let r = m.update_entry("secret", entry("secret", EntryKind::File, true, false));
    assert!(r.is_none());
}

#[test]
fn set_listing_clears_selection() {
    let m = Model::new();
    m.set_listing(cwd(), vec![file("a"), file("b")]);
    m.with_selection(|s| { s.set_single(0); });
    m.set_listing(cwd(), vec![file("c")]);
    assert_eq!(m.selection_snapshot().len(), 0);
}

#[test]
fn sort_by_type_clusters_same_extension() {
    // Directories still cluster at the top. Within files, same-extension
    // entries sort together and extension-key is case-insensitive.
    let m = Model::new();
    m.set_sort(navigator_gui::model::Sort {
        mode: navigator_config::SortMode::Type,
        descending: false,
    });
    m.set_listing(cwd(), vec![
        file("a.txt"),
        file("b.md"),
        file("c.TXT"),
        file("d.md"),
        dir("folder"),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec!["folder", "b.md", "d.md", "a.txt", "c.TXT"]);
}

#[test]
fn sort_by_type_puts_extensionless_first() {
    let m = Model::new();
    m.set_sort(navigator_gui::model::Sort {
        mode: navigator_config::SortMode::Type,
        descending: false,
    });
    m.set_listing(cwd(), vec![
        file("README"),
        file("notes.txt"),
        file("Makefile"),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    // Empty extension sorts before "txt", and within no-extension the
    // name-tiebreaker puts Makefile before README (case-insensitive).
    assert_eq!(names, vec!["Makefile", "README", "notes.txt"]);
}

#[test]
fn sort_by_type_descending_reverses_within_kind() {
    let m = Model::new();
    m.set_sort(navigator_gui::model::Sort {
        mode: navigator_config::SortMode::Type,
        descending: true,
    });
    m.set_listing(cwd(), vec![
        file("a.txt"),
        file("b.md"),
        file("c.txt"),
    ]);
    let names: Vec<_> = (0..m.len()).map(|i| m.get(i).unwrap().name).collect();
    assert_eq!(names, vec!["c.txt", "a.txt", "b.md"]);
}
