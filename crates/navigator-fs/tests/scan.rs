//! Directory-scan tests against real temp directories.
//!
//! Uses `tempfile::TempDir` so the fixtures clean themselves up. We stick
//! to Windows since `navigator-fs` is `#[cfg(windows)]`-only.

#![cfg(windows)]

use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use navigator_core::{EntryKind, NavPath};
use navigator_fs::{read_dir, stat_entry};

fn make_absolute(p: &Path) -> NavPath {
    NavPath::new(p.to_path_buf()).expect("tempdir is absolute")
}

#[test]
fn reads_plain_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"hello").unwrap();
    fs::write(dir.path().join("b.log"), b"log").unwrap();

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    assert_eq!(entries.len(), 2);
    let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.log".to_string()));
    for e in &entries {
        assert_eq!(e.kind, EntryKind::File);
        assert!(!e.hidden);
        assert!(!e.system);
    }
}

#[test]
fn reads_nested_dirs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("subdir")).unwrap();
    fs::write(dir.path().join("file.txt"), b"").unwrap();

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    assert_eq!(entries.len(), 2);
    let sub = entries.iter().find(|e| e.name == "subdir").unwrap();
    assert_eq!(sub.kind, EntryKind::Directory);
    assert!(sub.is_dir());
}

#[test]
fn detects_hidden_attribute() {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("secret.txt");
    fs::write(&p, b"").unwrap();

    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_HIDDEN);
    }

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    let secret = entries.iter().find(|e| e.name == "secret.txt").unwrap();
    assert!(secret.hidden, "hidden flag should be set");
}

#[test]
fn detects_system_attribute() {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_SYSTEM, SetFileAttributesW};
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("driver.sys");
    fs::write(&p, b"").unwrap();

    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_SYSTEM);
    }

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    let sys = entries.iter().find(|e| e.name == "driver.sys").unwrap();
    assert!(sys.system, "system flag should be set");
}

#[test]
fn empty_directory_returns_empty_vec() {
    let dir = tempfile::tempdir().unwrap();
    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn errors_for_nonexistent_path() {
    let nav = NavPath::new(r"C:\definitely_does_not_exist_9f7c3b").unwrap();
    let r = read_dir(&nav);
    assert!(r.is_err(), "expected error for missing directory");
}

#[test]
fn reads_sizes_accurately() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"hello, world";
    fs::write(dir.path().join("s.txt"), body).unwrap();

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    let f = &entries[0];
    assert_eq!(f.size, body.len() as u64);
}

#[test]
fn dot_entries_excluded() {
    // FindFirstFile reports `.` and `..` — the scanner must drop them.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("one"), b"").unwrap();

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "one");
}

/// The watcher path depends on `stat_entry` reporting exactly what a
/// `read_dir` of the parent would have reported for that one child —
/// same name, kind, size and attributes. If they ever diverge, a file
/// folded in by the watcher would render differently from the same file
/// after a refresh.
#[test]
fn stat_entry_matches_the_read_dir_entry_for_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"twelve bytes";
    fs::write(dir.path().join("s.txt"), body).unwrap();
    fs::create_dir(dir.path().join("kid")).unwrap();

    let listed = read_dir(&make_absolute(dir.path())).unwrap();

    let file = stat_entry(&dir.path().join("s.txt")).expect("file must stat");
    let from_listing = listed.iter().find(|e| e.name == "s.txt").unwrap();
    assert_eq!(file.name, from_listing.name);
    assert_eq!(file.kind, from_listing.kind);
    assert_eq!(file.size, from_listing.size);
    assert_eq!(file.attrs, from_listing.attrs);
    assert_eq!(file.modified, from_listing.modified);
    assert_eq!(file.size, body.len() as u64);

    let sub = stat_entry(&dir.path().join("kid")).expect("dir must stat");
    assert_eq!(sub.name, "kid");
    assert_eq!(sub.kind, EntryKind::Directory);
}

/// A trailing separator makes `FindFirstFileExW` fail with
/// ERROR_INVALID_NAME. Watcher paths and `NavPath::join` can both hand
/// one over, so `stat_entry` normalises it away rather than reporting the
/// file as gone.
#[test]
fn stat_entry_tolerates_a_trailing_separator() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("kid")).unwrap();
    let with_sep = dir.path().join("kid").to_string_lossy().into_owned() + "\\";
    let e = stat_entry(Path::new(&with_sep)).expect("trailing slash must still stat");
    assert_eq!(e.name, "kid");
}

/// The overwhelmingly common watcher race: a file is created and removed
/// before the notification is drained. `None`, not a panic or a stale
/// entry.
#[test]
fn stat_entry_returns_none_for_a_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    assert!(stat_entry(&dir.path().join("never_existed_4a91")).is_none());
}
