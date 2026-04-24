//! Selection-model unit tests. Covers the three interaction modes the GUI
//! exposes: single-click (set_single), Ctrl+Space (toggle), and Shift+arrow
//! (extend_to).

use navigator_core::Selection;

#[test]
fn new_selection_is_empty() {
    let s = Selection::default();
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
    assert_eq!(s.focus(), None);
    assert_eq!(s.anchor(), None);
}

#[test]
fn set_single_replaces_existing() {
    let mut s = Selection::default();
    s.set_single(2);
    s.set_single(5);
    assert_eq!(s.len(), 1);
    assert!(s.contains(5));
    assert!(!s.contains(2));
    assert_eq!(s.focus(), Some(5));
    assert_eq!(s.anchor(), Some(5));
}

#[test]
fn toggle_adds_and_removes() {
    let mut s = Selection::default();
    s.toggle(3);
    s.toggle(5);
    assert!(s.contains(3) && s.contains(5));
    s.toggle(3);
    assert!(!s.contains(3));
    assert!(s.contains(5));
    // Focus follows the last toggle.
    assert_eq!(s.focus(), Some(3));
}

#[test]
fn toggle_sets_anchor_once() {
    let mut s = Selection::default();
    s.toggle(10);
    s.toggle(20);
    // Anchor frozen at first toggle.
    assert_eq!(s.anchor(), Some(10));
}

#[test]
fn extend_to_forward() {
    let mut s = Selection::default();
    s.set_single(2);
    s.extend_to(5);
    let items: Vec<_> = s.iter().collect();
    assert_eq!(items, vec![2, 3, 4, 5]);
    assert_eq!(s.focus(), Some(5));
}

#[test]
fn extend_to_backward() {
    let mut s = Selection::default();
    s.set_single(5);
    s.extend_to(2);
    let items: Vec<_> = s.iter().collect();
    assert_eq!(items, vec![2, 3, 4, 5]);
    assert_eq!(s.focus(), Some(2));
}

#[test]
fn extend_to_uses_anchor_not_focus() {
    // After set_single(3), anchor=3. Toggle 7 (focus=7, anchor still 3).
    // Extend_to(1) must create range 1..=3, not 3..=7 or 1..=7.
    let mut s = Selection::default();
    s.set_single(3);
    s.toggle(7);
    s.extend_to(1);
    let items: Vec<_> = s.iter().collect();
    assert_eq!(items, vec![1, 2, 3]);
}

#[test]
fn clear_resets_everything() {
    let mut s = Selection::default();
    s.set_single(4);
    s.extend_to(9);
    s.clear();
    assert!(s.is_empty());
    assert_eq!(s.focus(), None);
    assert_eq!(s.anchor(), None);
}
