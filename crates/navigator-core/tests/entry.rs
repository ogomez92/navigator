use navigator_core::{Entry, EntryKind, FileTime};

#[test]
fn filetime_unix_epoch_converts_to_zero() {
    let ft = FileTime(FileTime::UNIX_EPOCH_TICKS);
    assert_eq!(ft.to_unix_secs(), 0);
}

#[test]
fn filetime_seconds_after_unix_epoch() {
    // 1 second after the unix epoch = UNIX_EPOCH_TICKS + 10_000_000 ticks
    // (100ns tick resolution).
    let ft = FileTime(FileTime::UNIX_EPOCH_TICKS + 10_000_000);
    assert_eq!(ft.to_unix_secs(), 1);
}

#[test]
fn is_dir_on_directory_kind() {
    let e = Entry {
        name: "foo".into(),
        kind: EntryKind::Directory,
        size: 0,
        modified: FileTime::default(),
        created: FileTime::default(),
        attrs: 0,
        hidden: false,
        system: false,
    };
    assert!(e.is_dir());
}

#[test]
fn is_dir_false_for_files_and_links() {
    let e = Entry {
        name: "foo".into(),
        kind: EntryKind::File,
        size: 0,
        modified: FileTime::default(),
        created: FileTime::default(),
        attrs: 0,
        hidden: false,
        system: false,
    };
    assert!(!e.is_dir());

    let link = Entry {
        kind: EntryKind::Symlink,
        ..e.clone()
    };
    assert!(!link.is_dir());
}
