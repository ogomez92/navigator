//! Directory-scan tests against real temp directories.
//!
//! Uses `tempfile::TempDir` so the fixtures clean themselves up. We stick
//! to Windows since `navigator-fs` is `#[cfg(windows)]`-only.

#![cfg(windows)]

use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use navigator_core::{EntryKind, NavPath};
use navigator_fs::read_dir;

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
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW,
    };
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("secret.txt");
    fs::write(&p, b"").unwrap();

    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    unsafe { SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_HIDDEN); }

    let entries = read_dir(&make_absolute(dir.path())).unwrap();
    let secret = entries.iter().find(|e| e.name == "secret.txt").unwrap();
    assert!(secret.hidden, "hidden flag should be set");
}

#[test]
fn detects_system_attribute() {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_SYSTEM, SetFileAttributesW,
    };
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("driver.sys");
    fs::write(&p, b"").unwrap();

    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    unsafe { SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_SYSTEM); }

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
