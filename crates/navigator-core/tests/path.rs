//! NavPath invariants.

use navigator_core::NavPath;

#[test]
fn rejects_relative_paths() {
    assert!(NavPath::new("foo/bar").is_err());
    assert!(NavPath::new("./relative").is_err());
}

#[cfg(windows)]
#[test]
fn accepts_absolute_windows_paths() {
    let p = NavPath::new(r"C:\Users\Public").expect("absolute accepted");
    assert_eq!(p.file_name(), "Public");
}

#[cfg(windows)]
#[test]
fn join_appends_child() {
    let p = NavPath::new(r"C:\Users").unwrap();
    let c = p.join("Public");
    assert_eq!(c.file_name(), "Public");
    assert_eq!(c.as_path().to_string_lossy(), r"C:\Users\Public");
}

#[cfg(windows)]
#[test]
fn parent_goes_up_one() {
    let p = NavPath::new(r"C:\Users\Public").unwrap();
    let parent = p.parent().expect("has parent");
    assert_eq!(parent.file_name(), "Users");
}

#[cfg(windows)]
#[test]
fn parent_of_drive_root_is_none() {
    let p = NavPath::new(r"C:\").unwrap();
    // C:\ has no parent dir on Windows.
    assert!(p.parent().is_none());
}

#[cfg(windows)]
#[test]
fn accepts_unc_share_root_without_trailing_sep() {
    // Rust's Path::is_absolute returns false for `\\host\share` because
    // there's no root component — only a prefix. NavPath::new retries
    // with a trailing separator so users can type IP-based shares.
    let p = NavPath::new(r"\\100.86.173.34\media").expect("UNC accepted");
    assert!(p.as_path().is_absolute());
}

#[cfg(windows)]
#[test]
fn accepts_unc_share_with_trailing_sep() {
    let p = NavPath::new(r"\\server\share\").expect("UNC accepted");
    assert!(p.as_path().is_absolute());
}

#[cfg(windows)]
#[test]
fn accepts_unc_path_into_share() {
    let p = NavPath::new(r"\\server\share\folder").expect("UNC accepted");
    assert_eq!(p.file_name(), "folder");
}

/// Delete branches on `is_unc` to keep `.trash` off network shares, so
/// every shape of UNC path has to be recognised — and none of the
/// sentinels, which also start with two backslashes, may be mistaken for
/// one.
#[cfg(windows)]
#[test]
fn is_unc_covers_every_share_shape_and_no_sentinel() {
    for s in [
        r"\\100.86.173.34\uri\Downloads\sync",
        r"\\100.86.173.34\uri",
        r"\\server\share\",
        r"\\server",
        "//server/share/file",
    ] {
        let p = NavPath::new(s).expect("UNC accepted");
        assert!(p.is_unc(), "{s} should be UNC");
    }

    for s in [r"C:\Users", r"D:\"] {
        let p = NavPath::new(s).expect("local accepted");
        assert!(!p.is_unc(), "{s} should not be UNC");
    }

    assert!(!NavPath::this_pc().is_unc());
    assert!(!NavPath::remotes_root().is_unc());
    assert!(!NavPath::remote("mac", "Downloads").is_unc());
    assert!(!NavPath::remote("mac", "").is_unc());
}

/// `\\?\UNC\host\share` is the extended-length spelling of a share and is
/// still a network path, while every other `\\?\` prefix is not.
#[cfg(windows)]
#[test]
fn extended_length_unc_is_unc_but_extended_length_local_is_not() {
    assert!(NavPath::new(r"\\?\UNC\server\share\file").unwrap().is_unc());
    assert!(!NavPath::new(r"\\?\C:\Users\file").unwrap().is_unc());
}
