//! Navigation-history tests. Exercises push/back/forward semantics,
//! duplicate collapse, forward-stack truncation, and capacity bounds.

#![cfg(windows)]

use navigator_core::NavPath;
use navigator_gui::history::History;

fn p(s: &str) -> NavPath { NavPath::new(s).unwrap() }

#[test]
fn empty_history_cannot_move() {
    let h = History::default();
    assert!(!h.can_back());
    assert!(!h.can_forward());
    assert!(h.current().is_none());
}

#[test]
fn single_push_becomes_current() {
    let mut h = History::default();
    h.push(p(r"C:\one"));
    assert_eq!(h.current(), Some(&p(r"C:\one")));
    assert!(!h.can_back());
    assert!(!h.can_forward());
}

#[test]
fn back_and_forward_round_trip() {
    let mut h = History::default();
    h.push(p(r"C:\a"));
    h.push(p(r"C:\b"));
    h.push(p(r"C:\c"));
    assert_eq!(h.current(), Some(&p(r"C:\c")));

    assert_eq!(h.back(),    Some(&p(r"C:\b")));
    assert_eq!(h.back(),    Some(&p(r"C:\a")));
    assert_eq!(h.back(),    None); // clamped
    assert_eq!(h.current(), Some(&p(r"C:\a")));

    assert_eq!(h.forward(), Some(&p(r"C:\b")));
    assert_eq!(h.forward(), Some(&p(r"C:\c")));
    assert_eq!(h.forward(), None);
}

#[test]
fn push_after_back_truncates_forward() {
    let mut h = History::default();
    h.push(p(r"C:\a"));
    h.push(p(r"C:\b"));
    h.push(p(r"C:\c"));
    h.back();                  // at b
    h.push(p(r"C:\x"));        // new timeline from b
    // Forward stack discarded.
    assert_eq!(h.current(), Some(&p(r"C:\x")));
    assert!(!h.can_forward());
    // Back still goes to b, then a.
    assert_eq!(h.back(), Some(&p(r"C:\b")));
    assert_eq!(h.back(), Some(&p(r"C:\a")));
}

#[test]
fn consecutive_duplicate_push_collapses() {
    let mut h = History::default();
    h.push(p(r"C:\a"));
    h.push(p(r"C:\a"));
    h.push(p(r"C:\a"));
    assert_eq!(h.len(), 1);
}

#[test]
fn non_consecutive_duplicate_kept() {
    let mut h = History::default();
    h.push(p(r"C:\a"));
    h.push(p(r"C:\b"));
    h.push(p(r"C:\a"));
    assert_eq!(h.len(), 3);
}

#[test]
fn capacity_bounds_memory() {
    let mut h = History::with_capacity(3);
    h.push(p(r"C:\a"));
    h.push(p(r"C:\b"));
    h.push(p(r"C:\c"));
    h.push(p(r"C:\d"));
    assert_eq!(h.len(), 3);
    // Oldest (`a`) dropped; `b` is the back-most reachable entry.
    assert_eq!(h.current(), Some(&p(r"C:\d")));
    assert_eq!(h.back(), Some(&p(r"C:\c")));
    assert_eq!(h.back(), Some(&p(r"C:\b")));
    assert_eq!(h.back(), None);
}

#[test]
fn can_back_and_can_forward_track_cursor() {
    let mut h = History::default();
    h.push(p(r"C:\a"));
    h.push(p(r"C:\b"));
    assert!(h.can_back());
    assert!(!h.can_forward());
    h.back();
    assert!(!h.can_back());
    assert!(h.can_forward());
}
