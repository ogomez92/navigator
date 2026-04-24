//! Drive enumeration tests. Any Windows machine has at least C:, so we
//! can assert the basic shape without a fixture.

#![cfg(windows)]

use navigator_fs::{drive_path_from_display, list_drives};

#[test]
fn returns_at_least_the_system_drive() {
    let drives = list_drives();
    assert!(!drives.is_empty(), "at least one drive must be enumerated");
    assert!(
        drives.iter().any(|d| d.name.contains("C:")),
        "system drive C: should appear; got {:?}",
        drives.iter().map(|d| d.name.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn every_drive_entry_is_directory_kind() {
    // This PC view opens drives via the same activate-index path as
    // normal folders; they must look like directories to the model.
    let drives = list_drives();
    for d in drives {
        assert!(d.is_dir(), "drive {:?} must be Directory kind", d.name);
    }
}

#[test]
fn drive_display_roundtrips_to_path() {
    assert_eq!(drive_path_from_display("Local Disk (C:)"), Some(r"C:\".to_string()));
    assert_eq!(drive_path_from_display("System (D:)"),     Some(r"D:\".to_string()));
}

#[test]
fn drive_display_without_parens_returns_none() {
    assert!(drive_path_from_display("Not a drive").is_none());
    assert!(drive_path_from_display("Incomplete (no close").is_none());
}

#[test]
fn drive_display_with_non_drive_paren_content_returns_none() {
    // Parenthetical content must end in `:` to be a drive spec.
    assert!(drive_path_from_display("Foo (bar)").is_none());
}
