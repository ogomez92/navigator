//! Drive enumeration tests. Any Windows machine has at least C:, so we
//! can assert the basic shape without a fixture.

#![cfg(windows)]

use navigator_fs::{drive_info, drive_path_from_display, list_drives};

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

/// The drive spec leads the row, so a screen reader says "D:" before the
/// label and the alphabetical sort orders by letter rather than by name.
#[test]
fn drive_entries_lead_with_the_drive_letter() {
    for d in list_drives() {
        let spec = d.name.split_whitespace().next().unwrap_or_default();
        assert_eq!(
            spec.len(),
            2,
            "entry must start with a bare drive spec: {:?}",
            d.name
        );
        assert!(
            spec.ends_with(':'),
            "expected `X:` prefix, got {:?}",
            d.name
        );
        // Never `D: ()` — an unlabelled volume falls back to its kind.
        assert!(
            !d.name.contains("()"),
            "empty parenthetical in {:?}",
            d.name
        );
    }
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
    assert_eq!(
        drive_path_from_display("C: (Windows)"),
        Some(r"C:\".to_string())
    );
    assert_eq!(
        drive_path_from_display("D: (Data)"),
        Some(r"D:\".to_string())
    );
    // A label with spaces is still just a trailing token.
    assert_eq!(
        drive_path_from_display("E: (My External Drive)"),
        Some(r"E:\".to_string())
    );
    // Every entry list_drives produces must parse back.
    for d in list_drives() {
        assert!(
            drive_path_from_display(&d.name).is_some(),
            "list_drives entry must round-trip: {:?}",
            d.name
        );
    }
}

#[test]
fn non_drive_names_return_none() {
    assert!(drive_path_from_display("Not a drive").is_none());
    assert!(drive_path_from_display("").is_none());
    assert!(drive_path_from_display("CD: (too long)").is_none());
    assert!(drive_path_from_display("1: (not a letter)").is_none());
}

/// Only the *leading* token counts, so a real folder that happens to be
/// named like a drive annotation can never be opened as a volume.
#[test]
fn a_folder_named_like_a_drive_annotation_is_not_a_drive() {
    assert!(drive_path_from_display("Backup (C:)").is_none());
    assert!(drive_path_from_display("Foo (bar)").is_none());
}

/// Properties on a drive must answer from constant-time volume queries —
/// C: always has a filesystem and a non-zero capacity.
#[test]
fn drive_info_reports_capacity_for_the_system_drive() {
    let info = drive_info(r"C:\");
    assert!(!info.is_unavailable(), "C: must be readable: {info:?}");
    assert!(info.volume_info_ok, "volume info for C: {info:?}");
    assert!(
        !info.file_system.is_empty(),
        "C: must report a filesystem: {info:?}"
    );
    let space = info.space.expect("C: must report capacity");
    assert!(space.total > 0, "non-zero total: {space:?}");
    assert!(space.free <= space.total, "free within total: {space:?}");
    assert!(
        space.available <= space.free,
        "quota'd availability never exceeds free space: {space:?}"
    );
    assert!(space.used() <= space.total);
    assert!((0.0..=100.0).contains(&space.used_percent()));
    let cluster = info.cluster.expect("C: must report cluster geometry");
    assert!(cluster.allocation_unit() >= 512, "{cluster:?}");
}

/// A trailing separator is optional — callers hand us both forms.
#[test]
fn drive_info_normalises_the_root_form() {
    assert_eq!(drive_info("C:").root, r"C:\");
    assert_eq!(drive_info(r"C:\").root, r"C:\");
    assert_eq!(drive_info("C:/").root, r"C:\");
}

/// A letter with nothing mounted must come back flagged, not as a screen
/// of plausible zeroes. `Z:` is unmounted on essentially every machine;
/// skip the assertion if this one is the exception.
#[test]
fn an_unmounted_letter_is_reported_unavailable() {
    let info = drive_info(r"Z:\");
    if info.drive_type <= 1 {
        assert!(
            info.is_unavailable(),
            "unmounted Z: must not look like a real volume: {info:?}"
        );
    }
}
