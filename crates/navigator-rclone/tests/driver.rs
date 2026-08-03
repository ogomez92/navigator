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

use navigator_core::{ConflictMode, NavPath};
use navigator_rclone::op::OpEvent;
use navigator_rclone::{Operation, RcloneDriver};

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

fn nav(p: &Path) -> NavPath {
    NavPath::new(p.to_path_buf()).unwrap()
}

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
    if !rclone_available() {
        eprintln!("rclone not available; skipping");
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("a.txt");
    fs::write(&src, b"hello").unwrap();

    // Dry-run via the preflight helper — copy uses `copyto` internally,
    // so the preflight report gives us a signal without mutating dst.
    let driver = RcloneDriver::from_path();
    let report = driver
        .preflight(&Operation::Copy {
            sources: vec![nav(&src)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        })
        .expect("preflight");

    assert!(
        !dst_dir.path().join("a.txt").exists(),
        "dry-run must not create dest"
    );
    assert!(!report.raw_log.is_empty(), "expected log records");
}

#[test]
fn copy_single_file_actually_copies() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("alpha.bin");
    fs::write(&src, b"payload").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Copy {
            sources: vec![nav(&src)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok, "copy should succeed");
    assert!(
        dst_dir.path().join("alpha.bin").exists(),
        "dest file must exist"
    );
    assert_eq!(
        fs::read(dst_dir.path().join("alpha.bin")).unwrap(),
        b"payload"
    );
}

#[test]
fn rename_moves_and_renames_in_place() {
    if !rclone_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("old.txt");
    let dst = dir.path().join("new.txt");
    fs::write(&src, b"x").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Rename {
            src: nav(&src),
            dst: nav(&dst),
        },
    );
    assert!(ok, "rename should succeed");
    assert!(!src.exists(), "old name should be gone");
    assert!(dst.exists(), "new name should exist");
    assert_eq!(fs::read(&dst).unwrap(), b"x");
}

#[test]
fn add_new_only_preserves_existing_destination() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();

    let src = src_dir.path().join("f.bin");
    fs::write(&src, b"NEW").unwrap();
    let dst = dst_dir.path().join("f.bin");
    fs::write(&dst, b"OLD").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Copy {
            sources: vec![nav(&src)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::AddNewOnly,
        },
    );
    // With --ignore-existing, rclone skips and returns success.
    assert!(ok);
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"OLD",
        "destination must not be overwritten"
    );
}

#[test]
fn update_mode_replaces_differing_destination() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();

    // Use contents of different sizes so rclone's default "skip if same
    // size + mtime" optimisation cannot short-circuit the copy. The
    // purpose of this test is to verify the conflict mode itself, not
    // rclone's change-detection.
    //
    // Write order and the sleep both matter: `Update` passes `--update`,
    // which refuses to replace a destination that is *newer* than the
    // source. Windows file timestamps are coarse (a whole clock tick), so
    // writing these back-to-back in either order lands them in the same
    // tick and the outcome depends on which side of the tick boundary the
    // writes fall — a real flake. Stage the older file first, then wait out
    // a tick so the source is unambiguously newer.
    let dst = dst_dir.path().join("f.bin");
    fs::write(&dst, b"OLD").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let src = src_dir.path().join("f.bin");
    fs::write(&src, b"NEW_CONTENT_LONGER").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Copy {
            sources: vec![nav(&src)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok);
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"NEW_CONTENT_LONGER",
        "destination must be overwritten when the source differs"
    );
}

/// The whole point of the conflict rewrite: a real two-pass dry-run against
/// a real rclone must isolate the file that already exists and differs
/// from the file that is brand new. `same.txt` is byte-identical with a
/// matching mtime, so it should not appear anywhere — not as a conflict and
/// not as a transfer.
#[test]
fn conflicts_isolates_existing_differing_destinations() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let driver = RcloneDriver::from_path();

    // `Copy` appends the source basename to dest_dir, so the pre-existing
    // destination tree has to live at <dst_parent>/<src basename> for the
    // conflicts to line up.
    let dst_parent = tempfile::tempdir().unwrap();
    let dst_dir = dst_parent.path().join(src_dir.path().file_name().unwrap());
    fs::create_dir(&dst_dir).unwrap();

    // Identical size is not enough for rclone to skip a file — it compares
    // mtime too. Rather than reach for a timestamp-setting crate, seed the
    // destination copy with rclone itself, which preserves mtime. That
    // makes "identical" mean exactly what rclone means by it.
    let same_src = src_dir.path().join("same.txt");
    fs::write(&same_src, b"same").unwrap();
    let (seeded, _) = run_to_completion(
        &driver,
        Operation::CopyTo {
            src: nav(&same_src),
            dst: nav(&dst_dir.join("same.txt")),
        },
    );
    assert!(seeded, "seeding the identical fixture file must succeed");

    // Destination first, source second, with a tick between: under `Update`
    // rclone refuses to replace a destination that is newer than the source,
    // so the source has to win on mtime for this to be a conflict at all.
    // (The inverse is pinned by `update_mode_spares_newer_destination`.)
    fs::write(dst_dir.join("differs.txt"), b"old").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    fs::write(src_dir.path().join("differs.txt"), b"NEW_AND_LONGER").unwrap();
    fs::write(src_dir.path().join("brandnew.txt"), b"x").unwrap();

    let report = driver
        .conflicts(&Operation::Copy {
            sources: vec![nav(src_dir.path())],
            dest_dir: nav(dst_parent.path()),
            mode: ConflictMode::Update,
        })
        .expect("conflicts pass should run");

    let names: Vec<String> = report
        .overwrites
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["differs.txt"],
        "only the existing-and-differing file is a conflict; got {:?}",
        report.overwrites
    );
    assert!(
        report.deletes.is_empty(),
        "a copy never deletes; got {:?}",
        report.deletes
    );
}

/// The safety property that makes `Update` a defensible default: a
/// destination file that is *newer* than the source is left alone, so a
/// stale paste cannot silently roll back an edit made at the destination.
/// It follows that such a file is not reported as a conflict either —
/// there is nothing to warn about, because nothing will be written.
#[test]
fn update_mode_spares_newer_destination() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_parent = tempfile::tempdir().unwrap();
    let dst_dir = dst_parent.path().join(src_dir.path().file_name().unwrap());
    fs::create_dir(&dst_dir).unwrap();

    // Source first, destination second — the destination is the newer edit.
    // The sleep is load-bearing: Windows file timestamps advance in coarse
    // ticks, so without it both writes can share a timestamp, the
    // destination is then not *strictly* newer, and `--update` transfers
    // after all. See `update_mode_replaces_differing_destination`.
    fs::write(src_dir.path().join("notes.txt").as_path(), b"stale source").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    fs::write(dst_dir.join("notes.txt"), b"fresher destination edit").unwrap();

    let driver = RcloneDriver::from_path();
    let report = driver
        .conflicts(&Operation::Copy {
            sources: vec![nav(src_dir.path())],
            dest_dir: nav(dst_parent.path()),
            mode: ConflictMode::Update,
        })
        .expect("conflicts pass should run");
    assert!(
        report.is_empty(),
        "a newer destination is not at risk under Update; got {:?}",
        report.overwrites
    );

    // Replace ignores mtime entirely, so the same pair *is* a conflict.
    let report = driver
        .conflicts(&Operation::Copy {
            sources: vec![nav(src_dir.path())],
            dest_dir: nav(dst_parent.path()),
            mode: ConflictMode::Replace,
        })
        .expect("conflicts pass should run");
    let names: Vec<String> = report
        .overwrites
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["notes.txt"],
        "Replace overrides the newer-destination guard; got {:?}",
        report.overwrites
    );
}

/// `AddNewOnly` cannot overwrite by construction, so `conflicts` must
/// short-circuit to an empty report without spawning rclone at all.
#[test]
fn conflicts_short_circuits_for_add_new_only() {
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    fs::write(src_dir.path().join("f.txt"), b"NEW").unwrap();
    fs::write(dst_dir.path().join("f.txt"), b"OLD").unwrap();

    // Deliberately points at a nonexistent binary: if this spawned rclone
    // the call would error instead of returning an empty report.
    let driver = RcloneDriver::with_exe("rclone-does-not-exist-xyz");
    let report = driver
        .conflicts(&Operation::Copy {
            sources: vec![nav(src_dir.path())],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::AddNewOnly,
        })
        .expect("must not spawn rclone");
    assert!(report.is_empty());
}

/// Mirror reports the destination entries it would prune, which is what
/// the Paste special confirm lists. These are files the user never
/// selected, so getting this list right is the safety gate.
#[test]
fn mirror_reports_destination_deletions() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_parent = tempfile::tempdir().unwrap();
    let dst_dir = dst_parent.path().join(
        src_dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    fs::create_dir(&dst_dir).unwrap();

    fs::write(src_dir.path().join("keep.txt"), b"keep").unwrap();
    fs::write(dst_dir.join("only-at-dest.txt"), b"doomed").unwrap();

    let driver = RcloneDriver::from_path();
    let report = driver
        .conflicts(&Operation::Copy {
            sources: vec![nav(src_dir.path())],
            dest_dir: nav(dst_parent.path()),
            mode: ConflictMode::Mirror,
        })
        .expect("conflicts pass should run");

    let deleted: Vec<String> = report
        .deletes
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        deleted,
        vec!["only-at-dest.txt"],
        "mirror must report the destination-only file as a deletion; got {:?}",
        report.deletes
    );
}

#[test]
fn move_single_file_removes_source() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("moveme.txt");
    fs::write(&src, b"contents").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Move {
            sources: vec![nav(&src)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok);
    assert!(!src.exists(), "source must be gone after move");
    assert!(dst_dir.path().join("moveme.txt").exists());
}

#[test]
fn move_directory_relocates_files() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let tree = src_dir.path().join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("root.txt"), b"r").unwrap();
    fs::write(tree.join("sub").join("leaf.txt"), b"l").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Move {
            sources: vec![nav(&tree)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok, "move should succeed");
    assert!(dst_dir.path().join("tree").join("root.txt").exists());
    assert!(
        dst_dir
            .path()
            .join("tree")
            .join("sub")
            .join("leaf.txt")
            .exists()
    );
    assert!(!tree.join("root.txt").exists(), "source files must be gone");
    assert!(
        !tree.join("sub").join("leaf.txt").exists(),
        "source files must be gone"
    );
}

#[test]
fn delete_purges_path() {
    if !rclone_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let target_dir = dir.path().join("doomed");
    fs::create_dir(&target_dir).unwrap();
    fs::write(target_dir.join("a"), b"").unwrap();
    fs::write(target_dir.join("b"), b"").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Delete {
            targets: vec![nav(&target_dir)],
            is_dir: true,
        },
    );
    assert!(ok);
    assert!(!target_dir.exists(), "purged directory must be gone");
}

#[test]
fn touch_creates_empty_file() {
    if !rclone_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("brand.new");
    assert!(!file.exists(), "precondition: file must not exist yet");

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(&driver, Operation::Touch { file: nav(&file) });
    assert!(ok, "touch should succeed");
    assert!(file.exists(), "touch must create the file");
    assert_eq!(fs::read(&file).unwrap(), b"", "touched file must be empty");
}

#[test]
fn preflight_on_missing_source_reports_error() {
    if !rclone_available() {
        return;
    }
    let dst_dir = tempfile::tempdir().unwrap();
    let bogus = nav(Path::new(r"C:\does_not_exist_8b3a7c\nope.bin"));

    let driver = RcloneDriver::from_path();
    let report = driver
        .preflight(&Operation::Copy {
            sources: vec![bogus],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        })
        .expect("preflight returns even on failure");

    // rclone's error vocabulary for "missing source" varies slightly
    // between versions — it may be logged as Error, Critical, Warning,
    // or a plain message. Any record whose text mentions the bogus path
    // is evidence the failure surfaced to us.
    let mentioned = report.raw_log.iter().any(|e| {
        e.msg.to_lowercase().contains("not found")
            || e.msg.to_lowercase().contains("failed")
            || e.msg.to_lowercase().contains("no such")
            || e.object
                .as_deref()
                .is_some_and(|o| o.contains("does_not_exist_8b3a7c"))
    });
    assert!(
        mentioned,
        "expected log to mention the missing source; got {:#?}",
        report
            .raw_log
            .iter()
            .map(|e| (&e.level, &e.msg))
            .collect::<Vec<_>>()
    );
}

// --- Failure reporting ----------------------------------------------------

/// Drive an op that is expected to fail and return what `Done` carried.
fn run_for_error(
    driver: &RcloneDriver,
    op: Operation,
) -> (bool, Option<i32>, Option<navigator_rclone::RcloneError>) {
    let h = driver.spawn(op).expect("spawn");
    for ev in h.events.iter() {
        if let OpEvent::Done {
            success,
            exit_code,
            error,
        } = ev
        {
            return (success, exit_code, error);
        }
    }
    panic!("stream ended without Done");
}

/// The failure the user hits most: acting on something that isn't there
/// any more (stale listing, a peer instance already moved it). rclone
/// reports it across five JSON records; what reaches the app has to be
/// the sentence, not the records.
#[test]
fn deleting_a_missing_file_reports_a_typed_error() {
    if !rclone_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let driver = RcloneDriver::from_path();
    let (ok, code, err) = run_for_error(
        &driver,
        Operation::Delete {
            targets: vec![nav(&dir.path().join("never-existed.txt"))],
            is_dir: false,
        },
    );
    assert!(!ok, "deleting a missing file must fail");
    assert_eq!(code, Some(4), "rclone documents 4 as file-not-found");

    let err = err.expect("a failed op must carry an error");
    assert_eq!(err.kind, navigator_rclone::ErrorKind::NotFound);
    assert_eq!(err.summary(), "not found");
    // The whole point: no JSON, no timestamp, no Go source location.
    assert!(
        !err.message.contains("{\"") && !err.message.contains("slog/logger.go"),
        "message still carries log-record scaffolding: {}",
        err.message
    );
    assert!(
        !err.message.starts_with("Attempt "),
        "retry wrapper not stripped: {}",
        err.message
    );
    // Three identical retries, one line of detail.
    assert_eq!(err.detail.len(), 1, "retries must dedupe: {:?}", err.detail);
}

/// Same for a copy whose source vanished, which fails in a different
/// place (rclone can't even build the source filesystem) and exits 3
/// rather than 4.
#[test]
fn copying_a_missing_source_reports_a_typed_error() {
    if !rclone_available() {
        return;
    }
    let dst = tempfile::tempdir().unwrap();
    let driver = RcloneDriver::from_path();
    let (ok, code, err) = run_for_error(
        &driver,
        Operation::CopyTo {
            src: nav(Path::new(r"C:\does_not_exist_8b3a7c\nope.bin")),
            dst: nav(&dst.path().join("nope.bin")),
        },
    );
    assert!(!ok);
    assert_eq!(code, Some(3), "rclone documents 3 as directory-not-found");
    let err = err.expect("a failed op must carry an error");
    assert_eq!(err.kind, navigator_rclone::ErrorKind::NotFound);
    assert_eq!(err.summary(), "not found");
}

/// A successful op must not manufacture an error — `Done.error` is the
/// signal callers branch on.
#[test]
fn a_successful_op_carries_no_error() {
    if !rclone_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let driver = RcloneDriver::from_path();
    let (ok, code, err) = run_for_error(
        &driver,
        Operation::Touch {
            file: nav(&dir.path().join("fine.txt")),
        },
    );
    assert!(ok);
    assert_eq!(code, Some(0));
    assert!(err.is_none(), "success must not report an error");
}

#[test]
fn default_transfers_value_matches_constant() {
    // Fresh drivers carry the library default. Tied to the constant so
    // a bump here + the config default stay in sync.
    let driver = RcloneDriver::from_path();
    assert_eq!(driver.transfers(), navigator_rclone::op::DEFAULT_TRANSFERS);
    assert_eq!(driver.transfers(), 8);
}

#[test]
fn base_args_include_transfers_flag() {
    // Every spawned rclone process must carry the configured
    // --transfers value. Asserting through base_args() bypasses the
    // need for a real rclone binary on PATH.
    let driver = RcloneDriver::from_path().with_transfers(12);
    let args = driver.base_args();
    let pos = args
        .iter()
        .position(|a| a == "--transfers")
        .expect("--transfers missing from base args");
    assert_eq!(args.get(pos + 1).map(String::as_str), Some("12"));
}

#[test]
fn with_transfers_clamps_zero_up_to_one() {
    // rclone rejects --transfers 0; driver silently promotes so a bad
    // config can't bomb every op.
    let driver = RcloneDriver::from_path().with_transfers(0);
    assert_eq!(driver.transfers(), 1);
    let args = driver.base_args();
    let pos = args.iter().position(|a| a == "--transfers").unwrap();
    assert_eq!(args.get(pos + 1).map(String::as_str), Some("1"));
}

#[test]
fn transfers_overrides_propagate_to_running_ops() {
    // End-to-end: set --transfers, spawn a real copy, assert it still
    // succeeds. Proves the flag doesn't break rclone parsing. Skipped
    // when no rclone binary is on PATH.
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    fs::write(src_dir.path().join("x.bin"), b"hello").unwrap();

    let driver = RcloneDriver::from_path().with_transfers(2);
    assert_eq!(driver.transfers(), 2);
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Copy {
            sources: vec![nav(&src_dir.path().join("x.bin"))],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok, "copy with custom --transfers should succeed");
    assert!(dst_dir.path().join("x.bin").exists());
}

#[test]
fn nested_directory_copy_preserves_tree() {
    if !rclone_available() {
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let tree = src_dir.path().join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("root.txt"), b"r").unwrap();
    fs::write(tree.join("sub").join("leaf.txt"), b"l").unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::Copy {
            sources: vec![nav(&tree)],
            dest_dir: nav(dst_dir.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok);
    assert!(dst_dir.path().join("tree").join("root.txt").exists());
    assert!(
        dst_dir
            .path()
            .join("tree")
            .join("sub")
            .join("leaf.txt")
            .exists()
    );
}

// --- Batched --files-from transfers --------------------------------------

/// Write a `--files-from` list and return its path, keeping the tempdir
/// alive via the returned guard.
fn list_file(dir: &std::path::Path, names: &[&str]) -> std::path::PathBuf {
    let p = dir.join("files-from.txt");
    let mut body = String::new();
    for n in names {
        body.push_str(n);
        body.push('\n');
    }
    fs::write(&p, body).unwrap();
    p
}

/// The payoff: many files land in one invocation, straight into dest_dir
/// with no source-basename directory in between.
#[test]
fn copy_batch_transfers_every_listed_file() {
    if !rclone_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..25).map(|i| format!("f{}.txt", i)).collect();
    for n in &names {
        fs::write(src.path().join(n), n.as_bytes()).unwrap();
    }
    // Names that historically broke the rclone path: spaces and non-ASCII.
    fs::write(src.path().join("with space.txt"), b"sp").unwrap();
    fs::write(src.path().join("acentuado-ñé.txt"), b"ac").unwrap();

    let mut all: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    all.push("with space.txt");
    all.push("acentuado-ñé.txt");
    let lst = tempfile::tempdir().unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::CopyBatch {
            src_root: nav(src.path()),
            list_file: list_file(lst.path(), &all),
            dest_dir: nav(dst.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok, "batch copy should succeed");
    for n in &all {
        assert!(
            dst.path().join(n).exists(),
            "{} did not arrive at the destination",
            n
        );
    }
    assert!(
        src.path().join("f0.txt").exists(),
        "a copy must leave the source in place"
    );
}

/// The trap that shapes the whole design: rclone reads a directory entry in
/// a `--files-from` list, transfers nothing, warns about nothing, and exits
/// **0**. If this ever starts working (or starts erroring), the partition
/// rule in `navigator_gui::batch` can be revisited — until then, a folder
/// routed through a list is silently lost while the paste reports success.
#[test]
fn files_from_silently_ignores_directories() {
    if !rclone_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::create_dir(src.path().join("adir")).unwrap();
    fs::write(src.path().join("adir").join("inner.txt"), b"x").unwrap();
    fs::write(src.path().join("keep.txt"), b"k").unwrap();
    let lst = tempfile::tempdir().unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::CopyBatch {
            src_root: nav(src.path()),
            list_file: list_file(lst.path(), &["keep.txt", "adir"]),
            dest_dir: nav(dst.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(
        ok,
        "rclone reports success even though the directory vanished"
    );
    assert!(dst.path().join("keep.txt").exists(), "the file arrives");
    assert!(
        !dst.path().join("adir").exists(),
        "REGRESSION GUARD: rclone now handles directories in --files-from. \
         navigator_gui::batch::partition excludes them on the assumption it \
         does not — revisit that rule."
    );
}

/// The move counterpart must empty the source, which is what makes a cut
/// paste correct.
#[test]
fn move_batch_removes_sources() {
    if !rclone_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    for n in ["a.txt", "b.txt"] {
        fs::write(src.path().join(n), n.as_bytes()).unwrap();
    }
    let lst = tempfile::tempdir().unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::MoveBatch {
            src_root: nav(src.path()),
            list_file: list_file(lst.path(), &["a.txt", "b.txt"]),
            dest_dir: nav(dst.path()),
            mode: ConflictMode::Update,
        },
    );
    assert!(ok);
    for n in ["a.txt", "b.txt"] {
        assert!(dst.path().join(n).exists(), "{} must arrive", n);
        assert!(!src.path().join(n).exists(), "{} must leave the source", n);
    }
}

/// `AddNewOnly` must still protect existing destinations when batched —
/// the mode flags have to survive the `--files-from` argv rearrangement.
#[test]
fn batch_honours_add_new_only() {
    if !rclone_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    fs::write(src.path().join("keep.txt"), b"NEW_LONGER").unwrap();
    fs::write(dst.path().join("keep.txt"), b"OLD").unwrap();
    fs::write(src.path().join("fresh.txt"), b"f").unwrap();
    let lst = tempfile::tempdir().unwrap();

    let driver = RcloneDriver::from_path();
    let (ok, _) = run_to_completion(
        &driver,
        Operation::CopyBatch {
            src_root: nav(src.path()),
            list_file: list_file(lst.path(), &["keep.txt", "fresh.txt"]),
            dest_dir: nav(dst.path()),
            mode: ConflictMode::AddNewOnly,
        },
    );
    assert!(ok);
    assert_eq!(
        fs::read(dst.path().join("keep.txt")).unwrap(),
        b"OLD",
        "AddNewOnly must not overwrite even inside a batch"
    );
    assert!(
        dst.path().join("fresh.txt").exists(),
        "new files still land"
    );
}

/// Conflict detection has to work through the batched op too, or a
/// many-file overwrite would run with no confirmation at all.
#[test]
fn batch_conflict_detection_finds_overwrites() {
    if !rclone_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // Destination first, then a tick, so the source is unambiguously newer
    // and `--update` does not spare it. See update_mode_spares_newer_destination.
    fs::write(dst.path().join("differs.txt"), b"old").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    fs::write(src.path().join("differs.txt"), b"NEW_AND_LONGER").unwrap();
    fs::write(src.path().join("brandnew.txt"), b"n").unwrap();
    let lst = tempfile::tempdir().unwrap();

    let driver = RcloneDriver::from_path();
    let report = driver
        .conflicts(&Operation::CopyBatch {
            src_root: nav(src.path()),
            list_file: list_file(lst.path(), &["differs.txt", "brandnew.txt"]),
            dest_dir: nav(dst.path()),
            mode: ConflictMode::Update,
        })
        .expect("batched conflicts pass should run");
    let names: Vec<String> = report
        .overwrites
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["differs.txt"],
        "only the existing-and-differing file is a conflict; got {:?}",
        report.overwrites
    );
}
