//! Recursive-search tests. Builds a known fixture tree and asserts on the
//! results `search_recursive` returns.

#![cfg(windows)]

use std::fs;

use navigator_core::NavPath;
use navigator_fs::search_recursive;

fn abs(p: &std::path::Path) -> NavPath { NavPath::new(p.to_path_buf()).unwrap() }

#[test]
fn substring_match_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Report.pdf"), b"").unwrap();
    fs::write(dir.path().join("notes.txt"),  b"").unwrap();

    let results = search_recursive(&abs(dir.path()), "REPORT", 100);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Report.pdf");
}

#[test]
fn recurses_into_subdirectories() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("sub1").join("sub2")).unwrap();
    fs::write(dir.path().join("root.txt"), b"").unwrap();
    fs::write(dir.path().join("sub1").join("mid.txt"), b"").unwrap();
    fs::write(dir.path().join("sub1").join("sub2").join("deep.txt"), b"").unwrap();

    let results = search_recursive(&abs(dir.path()), "txt", 100);
    let names: Vec<_> = results.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 3);
    // Names are relative paths from the search root. Separator is
    // whatever PathBuf uses on Windows (`\`).
    assert!(names.iter().any(|n| n == "root.txt"));
    assert!(names.iter().any(|n| n.ends_with("mid.txt")   && n.contains("sub1")));
    assert!(names.iter().any(|n| n.ends_with("deep.txt") && n.contains("sub2")));
}

#[test]
fn empty_query_returns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("anything.txt"), b"").unwrap();
    let r = search_recursive(&abs(dir.path()), "", 100);
    assert!(r.is_empty());
}

#[test]
fn max_results_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..50 {
        fs::write(dir.path().join(format!("file_{}.txt", i)), b"").unwrap();
    }
    let r = search_recursive(&abs(dir.path()), "file", 10);
    assert_eq!(r.len(), 10);
}

#[test]
fn matches_directory_names_too() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("projects_hot")).unwrap();
    fs::write(dir.path().join("projects_hot").join("stuff.txt"), b"").unwrap();

    let r = search_recursive(&abs(dir.path()), "projects", 100);
    assert!(r.iter().any(|e| e.name == "projects_hot"));
}

#[test]
fn no_match_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"").unwrap();
    let r = search_recursive(&abs(dir.path()), "nonexistent_xyz_9_7", 100);
    assert!(r.is_empty());
}
