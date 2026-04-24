//! Tests for the preflight rename helper.
//!
//! The "Keep both" option in the conflict dialog picks a fresh sibling
//! name by walking `name (1)`, `name (2)`, … until it finds a free slot.
//! These tests drive that logic against real temp directories so the
//! existence probe is exercised.

#![cfg(windows)]

use std::fs;

use navigator_gui::preflight::unique_numbered_path;

fn touch(p: &std::path::Path) {
    fs::write(p, b"").expect("create fixture file");
}

#[test]
fn free_path_is_returned_unchanged() {
    let tmp = tempdir();
    let target = tmp.join("new.txt");
    let got = unique_numbered_path(&target);
    assert_eq!(got, target);
}

#[test]
fn first_collision_gets_one_suffix() {
    let tmp = tempdir();
    let target = tmp.join("foo.txt");
    touch(&target);
    let got = unique_numbered_path(&target);
    assert_eq!(got, tmp.join("foo (1).txt"));
}

#[test]
fn numbering_advances_past_existing_suffixed_siblings() {
    let tmp = tempdir();
    let target = tmp.join("bar.txt");
    touch(&target);
    touch(&tmp.join("bar (1).txt"));
    touch(&tmp.join("bar (2).txt"));
    let got = unique_numbered_path(&target);
    assert_eq!(got, tmp.join("bar (3).txt"));
}

#[test]
fn extensionless_file_appends_suffix_without_dot() {
    let tmp = tempdir();
    let target = tmp.join("README");
    touch(&target);
    let got = unique_numbered_path(&target);
    assert_eq!(got, tmp.join("README (1)"));
}

#[test]
fn multi_extension_preserves_last_segment() {
    // Explorer parity: "foo.tar.gz" → "foo.tar (1).gz".
    let tmp = tempdir();
    let target = tmp.join("foo.tar.gz");
    touch(&target);
    let got = unique_numbered_path(&target);
    assert_eq!(got, tmp.join("foo.tar (1).gz"));
}

/// Make a fresh temp directory for a test. Uses the OS temp dir and a
/// nonce combining thread id + timestamp so parallel tests don't collide.
fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("navigator-preflight-{}-{}", t, n));
    fs::create_dir_all(&p).expect("mkdir tempdir");
    p
}
