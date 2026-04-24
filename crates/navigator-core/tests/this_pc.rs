//! Tests for the ThisPC sentinel path.

use navigator_core::NavPath;

#[test]
fn this_pc_is_constructible() {
    let p = NavPath::this_pc();
    assert!(p.is_this_pc());
}

#[test]
fn this_pc_has_no_parent() {
    // The navigate_up handler uses parent()==None as the signal to switch
    // into the ThisPC view, so ThisPC itself must never yield a parent.
    assert!(NavPath::this_pc().parent().is_none());
}

#[cfg(windows)]
#[test]
fn a_real_drive_root_has_no_parent_either() {
    // Confirms the contract the app relies on: Backspace from `C:\`
    // gets a `None` parent, which routes to ThisPC.
    let p = NavPath::new(r"C:\").unwrap();
    assert!(p.parent().is_none());
    assert!(!p.is_this_pc());
}

#[cfg(windows)]
#[test]
fn real_paths_are_not_this_pc() {
    let p = NavPath::new(r"C:\Users").unwrap();
    assert!(!p.is_this_pc());
}
