//! End-to-end check that a real rclone transfer produces sensible spoken
//! progress.
//!
//! The unit tests in `narrate.rs` pin the meter's arithmetic against
//! hand-fed fractions. This one closes the loop: it runs an actual copy,
//! feeds rclone's own stats records through
//! `Progress::fraction` → `Meter` → `phrase`, and asserts the result is
//! something a user would want to hear. That path is where the previous
//! implementation broke — the arithmetic was fine, it just never received
//! (or never spoke) the numbers.
//!
//! Skipped at runtime when `rclone` is not on PATH, matching the driver's
//! own integration tests.

#![cfg(windows)]

use std::fs;
use std::time::{Duration, Instant};

use navigator_core::{ConflictMode, NavPath};
use navigator_gui::narrate::{Cadence, Failure, Meter, failure_report, phrase};
use navigator_rclone::op::OpEvent;
use navigator_rclone::{Operation, RcloneDriver};

fn rclone_available() -> bool {
    std::process::Command::new("rclone")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Number of files in the fixture. Chosen so the copy outlives rclone's
/// one-second `--stats` interval: a handful of large files finishes inside
/// a single tick, which would make "progress climbed" vacuously true.
const FILES: usize = 2000;

/// Copy a batch through one `--files-from` invocation — the shape a plain
/// multi-file paste takes — and collect what the narrator would have said.
#[test]
fn a_real_batch_copy_narrates_monotonic_progress() {
    if !rclone_available() {
        eprintln!("rclone not available; skipping");
        return;
    }
    let src_dir = tempfile::tempdir().unwrap();
    let dst_dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..FILES).map(|i| format!("f{i}.bin")).collect();
    let blob = [7u8; 1024];
    for n in &names {
        fs::write(src_dir.path().join(n), blob).unwrap();
    }
    let list = src_dir.path().join("list.txt");
    fs::write(&list, format!("{}\n", names.join("\n"))).unwrap();

    let driver = RcloneDriver::from_path();
    let handle = driver
        .spawn(Operation::CopyBatch {
            src_root: NavPath::new(src_dir.path().to_path_buf()).unwrap(),
            list_file: list.clone(),
            dest_dir: NavPath::new(dst_dir.path().to_path_buf()).unwrap(),
            mode: ConflictMode::Update,
        })
        .expect("spawn");

    // One meter for the whole job, exactly as `run_batch` builds it: the
    // group is worth one unit per listed file.
    let mut meter = Meter::new(names.len() as u64);
    meter.begin(names.len() as u64);
    // Zero interval so every distinct phrase is captured; the interval
    // itself is covered by the unit tests.
    let mut cadence = Cadence::new(Duration::ZERO, Instant::now());
    let mut spoken: Vec<String> = Vec::new();
    let mut percents: Vec<u32> = Vec::new();
    let mut saw_current_file = false;

    for ev in handle.events.iter() {
        match ev {
            OpEvent::Progress(p) => {
                if p.current.is_some() {
                    saw_current_file = true;
                }
                if let Some(f) = p.fraction() {
                    meter.set_fraction(f);
                }
                percents.push(meter.percent().unwrap());
                if let Some(text) = phrase(&meter)
                    && cadence.due(Instant::now(), &text)
                {
                    spoken.push(text);
                }
            }
            OpEvent::Done { success, .. } => {
                assert!(success, "the copy itself must succeed");
                break;
            }
            OpEvent::Log(_) => {}
        }
    }
    meter.finish();

    for n in &names {
        assert!(dst_dir.path().join(n).exists(), "{n} did not arrive");
    }
    assert!(
        !spoken.is_empty(),
        "a real multi-file copy must produce something to say — this is \
         precisely what batching silenced"
    );
    assert!(
        percents.windows(2).all(|w| w[1] >= w[0]),
        "percentage went backwards: {percents:?}"
    );
    assert!(
        saw_current_file,
        "rclone names the in-flight file in its stats `transferring` list; \
         the progress window's Current line depends on it"
    );
    assert!(
        percents.len() > 1,
        "fixture finished inside one --stats tick, so nothing was actually \
         measured; enlarge it"
    );
    assert_eq!(meter.percent(), Some(100), "a finished job reads as 100%");
    assert_eq!(
        meter.units_done(),
        FILES as u64,
        "the meter must account for every item"
    );
    assert_eq!(
        phrase(&meter),
        None,
        "the completion summary speaks for a finished job"
    );
    // Every utterance is short enough to speak inside a normal cadence.
    for s in &spoken {
        assert!(s.len() < 40, "utterance too long to speak: {s:?}");
    }
}

/// The failure counterpart, closing the same loop: a real rclone error,
/// through the distiller, into the exact sentence the user is told.
///
/// This is the case that motivated the rewrite. rclone reports a delete
/// of a missing file as five JSON records — timestamps, Go source
/// locations, three identical retries — and the dialog used to show the
/// last ten of them verbatim. Asserting on the finished string is the
/// only way to catch a regression back to that: every layer in between
/// can look correct while the text stays unreadable.
#[test]
fn a_real_rclone_failure_becomes_one_readable_sentence() {
    if !rclone_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("never-existed.txt");
    let driver = RcloneDriver::from_path();
    let handle = driver
        .spawn(Operation::Delete {
            targets: vec![NavPath::new(missing.clone()).unwrap()],
            is_dir: false,
        })
        .expect("spawn");

    let mut failures = Vec::new();
    for ev in handle.events.iter() {
        if let OpEvent::Done { success, error, .. } = ev {
            assert!(!success);
            let err = error.expect("a failed op must carry an error");
            failures.push(Failure::from_rclone(&err, Some("never-existed.txt".into())));
            break;
        }
    }

    let report = failure_report("Deleting", 1, &failures).expect("a failure must report");
    assert_eq!(report.title, "Delete failed");
    assert_eq!(
        report.headline,
        "Delete failed: not found, never-existed.txt"
    );

    // The body may quote rclone, but nothing in it may be a log record.
    for marker in ["{\"", "slog/logger.go", "\"level\"", "Attempt 1/3", "\\?\\"] {
        assert!(
            !report.body.contains(marker),
            "log-record scaffolding leaked into the dialog ({marker}): {}",
            report.body
        );
    }
    // And it stays short enough to read, rather than being a log dump.
    assert!(
        report.body.lines().count() <= 6,
        "failure body is a wall of text: {}",
        report.body
    );
    eprintln!("--- dialog ---\n{}\n--------------", report.body);
}
