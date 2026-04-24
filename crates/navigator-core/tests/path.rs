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
