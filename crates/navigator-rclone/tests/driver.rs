//! End-to-end tests against a real rclone binary, using `--dry-run` so no
//! bytes actually move. Each test creates a tempdir fixture, runs the
//! driver, and asserts on the emitted events + filesystem state.
//!
//! If `rclone` is not on PATH, these tests are skipped at runtime (the spawn
//! returns an I/O error, which we treat as "environment not available").
//! The unit tests in `log_parser.rs` cover the parse path without external
//! dependencies.

#![cfg(windows)]

use std::fs;
use std::path::Path;

use navigator_core::NavPath;
use navigator_rclone::op::OpEvent;
use navigator_rclone::{Operation, OverwritePolicy, RcloneDriver};

fn rclone_available() -> bool {
    // Shelling out once confirms rclone is actually callable; we cache
    // nothing because the overhead is trivial next to the test fixture
    // setup.
    std::process::Command::new("rclone")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn nav(p: &Path) -> NavPath { NavPath::new(p.to_path_buf()).unwrap() }

/// Drive one rclone op to completion. Returns `(success, event_count)`.
fn run_to_completion(driver: &RcloneDriver, op: Operation) -> (bool, usize) {
    let h = driver.spawn(op).expect("spawn");
    let mut n = 0usize;
    let mut success = false;
    for ev in h.events.iter() {
        n += 1;
        if let OpEvent::Done { success: ok, .. } = ev {
            success = ok;
            break;
        }
    }
    (success, n)
}

#[test]
fn copy_single_file_dry_run_does_not_touch_dest() {
    if !rclone_available() { eprintln!("rclone not available; skipping"); return; }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("a.txt");
    fs::write(&src, b"hello").unwrap();

    // Dry-run via the preflight helper — copy uses `copyto` internally,
    // so the preflight report gives us a signal without mutating dst.
    let driver = RcloneDriver::from_path();
    let report = driver.preflight(&Operation::Copy {
        sources: vec![nav(&src)],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Always,
    }).expect("preflight");

    assert!(!dst_dir.path().join("a.txt").exists(),
        "dry-run must not create dest");
    assert!(!report.raw_log.is_empty(), "expected log records");
}

#[test]
fn copy_single_file_actually_copies() {
    if !rclone_available() { return; }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("alpha.bin");
    fs::write(&src, b"payload").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Copy {
        sources: vec![nav(&src)],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Always,
    });
    assert!(ok, "copy should succeed");
    assert!(dst_dir.path().join("alpha.bin").exists(), "dest file must exist");
    assert_eq!(fs::read(dst_dir.path().join("alpha.bin")).unwrap(), b"payload");
}

#[test]
fn rename_moves_and_renames_in_place() {
    if !rclone_available() { return; }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("old.txt");
    let dst = dir.path().join("new.txt");
    fs::write(&src, b"x").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Rename {
        src: nav(&src), dst: nav(&dst),
    });
    assert!(ok, "rename should succeed");
    assert!(!src.exists(), "old name should be gone");
    assert!(dst.exists(), "new name should exist");
    assert_eq!(fs::read(&dst).unwrap(), b"x");
}

#[test]
fn overwrite_never_policy_preserves_existing_destination() {
    if !rclone_available() { return; }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();

    let src = src_dir.path().join("f.bin");
    fs::write(&src, b"NEW").unwrap();
    let dst = dst_dir.path().join("f.bin");
    fs::write(&dst, b"OLD").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Copy {
        sources: vec![nav(&src)],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Never,
    });
    // With --ignore-existing, rclone skips and returns success.
    assert!(ok);
    assert_eq!(fs::read(&dst).unwrap(), b"OLD", "destination must not be overwritten");
}

#[test]
fn overwrite_always_policy_replaces_destination() {
    if !rclone_available() { return; }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();

    // Use contents of different sizes so rclone's default "skip if same
    // size + mtime" optimisation cannot short-circuit the copy. The
    // purpose of this test is to verify the overwrite policy itself, not
    // rclone's change-detection.
    let src = src_dir.path().join("f.bin");
    fs::write(&src, b"NEW_CONTENT_LONGER").unwrap();
    let dst = dst_dir.path().join("f.bin");
    fs::write(&dst, b"OLD").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Copy {
        sources: vec![nav(&src)],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Always,
    });
    assert!(ok);
    assert_eq!(fs::read(&dst).unwrap(), b"NEW_CONTENT_LONGER",
        "destination must be overwritten when policy is Always");
}

#[test]
fn move_single_file_removes_source() {
    if !rclone_available() { return; }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("moveme.txt");
    fs::write(&src, b"contents").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Move {
        sources: vec![nav(&src)],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Always,
    });
    assert!(ok);
    assert!(!src.exists(), "source must be gone after move");
    assert!(dst_dir.path().join("moveme.txt").exists());
}

#[test]
fn delete_purges_path() {
    if !rclone_available() { return; }
    let dir = tempfile::tempdir().unwrap();
    let target_dir = dir.path().join("doomed");
    fs::create_dir(&target_dir).unwrap();
    fs::write(target_dir.join("a"), b"").unwrap();
    fs::write(target_dir.join("b"), b"").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Delete {
        targets: vec![nav(&target_dir)],
    });
    assert!(ok);
    assert!(!target_dir.exists(), "purged directory must be gone");
}

#[test]
fn preflight_on_missing_source_reports_error() {
    if !rclone_available() { return; }
    let dst_dir = tempfile::tempdir().unwrap();
    let bogus = nav(Path::new(r"C:\does_not_exist_8b3a7c\nope.bin"));

    let driver = RcloneDriver::from_path();
    let report = driver.preflight(&Operation::Copy {
        sources: vec![bogus],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Always,
    }).expect("preflight returns even on failure");

    // rclone's error vocabulary for "missing source" varies slightly
    // between versions — it may be logged as Error, Critical, Warning,
    // or a plain message. Any record whose text mentions the bogus path
    // is evidence the failure surfaced to us.
    let mentioned = report.raw_log.iter().any(|e|
        e.msg.to_lowercase().contains("not found")
        || e.msg.to_lowercase().contains("failed")
        || e.msg.to_lowercase().contains("no such")
        || e.object.as_deref().is_some_and(|o| o.contains("does_not_exist_8b3a7c"))
    );
    assert!(
        mentioned,
        "expected log to mention the missing source; got {:#?}",
        report.raw_log.iter().map(|e| (&e.level, &e.msg)).collect::<Vec<_>>()
    );
}

#[test]
fn nested_directory_copy_preserves_tree() {
    if !rclone_available() { return; }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let tree = src_dir.path().join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("root.txt"), b"r").unwrap();
    fs::write(tree.join("sub").join("leaf.txt"), b"l").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Copy {
        sources: vec![nav(&tree)],
        dest_dir: nav(dst_dir.path()),
        policy: OverwritePolicy::Always,
    });
    assert!(ok);
    assert!(dst_dir.path().join("tree").join("root.txt").exists());
    assert!(dst_dir.path().join("tree").join("sub").join("leaf.txt").exists());
}
