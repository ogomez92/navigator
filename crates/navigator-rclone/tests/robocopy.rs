//! End-to-end tests against the real `robocopy.exe` (System32, so always
//! present on Windows). Every paste mode is checked by its effect on disk,
//! not by the flags it produced — the flags are pinned by the unit tests in
//! `robocopy.rs`; these pin what robocopy then *does* with them.

#![cfg(windows)]

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use navigator_core::{ConflictMode, NavPath};
use navigator_rclone::op::OpEvent;
use navigator_rclone::{ErrorKind, Operation, RcloneError, RobocopyDriver};

fn nav(p: &Path) -> NavPath {
    NavPath::new(p.to_path_buf()).unwrap()
}

fn write(p: &Path, body: &str) {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, body).unwrap();
}

fn set_mtime(p: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn old() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap()
}

struct Outcome {
    success: bool,
    error: Option<RcloneError>,
    progress: Vec<navigator_rclone::Progress>,
}

fn run(op: Operation) -> Outcome {
    run_with(RobocopyDriver::from_path(), op)
}

fn run_with(driver: RobocopyDriver, op: Operation) -> Outcome {
    let h = driver.spawn(op).expect("spawn");
    let mut progress = Vec::new();
    for ev in h.events.iter() {
        match ev {
            OpEvent::Progress(p) => progress.push(p),
            OpEvent::Done { success, error, .. } => {
                return Outcome {
                    success,
                    error,
                    progress,
                };
            }
            OpEvent::Log(_) => {}
        }
    }
    panic!("no Done event");
}

/// A source folder and a destination that already holds a copy of it in
/// every state a mode has to decide about.
///
/// | file | destination | Update | Add new only | Replace |
/// |---|---|---|---|---|
/// | `new.txt` | absent | copy | copy | copy |
/// | `stale.txt` | older | copy | keep | copy |
/// | `edited.txt` | newer | keep | keep | copy |
/// | `same.txt` | identical | keep | keep | copy |
struct Fixture {
    _tmp: tempfile::TempDir,
    src: std::path::PathBuf,
    dst_parent: std::path::PathBuf,
    dst: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src").join("photos");
    let dst_parent = tmp.path().join("dst");
    let dst = dst_parent.join("photos");
    write(&src.join("new.txt"), "new");
    write(&src.join("stale.txt"), "src-newer");
    write(&src.join("edited.txt"), "src-older");
    set_mtime(&src.join("edited.txt"), old());
    write(&src.join("same.txt"), "same");
    set_mtime(&src.join("same.txt"), old());
    write(&src.join("sub").join("deep.txt"), "deep");
    write(&src.join("ñandú ｜x.txt"), "wide");

    write(&dst.join("stale.txt"), "dst-older");
    set_mtime(&dst.join("stale.txt"), old());
    write(&dst.join("edited.txt"), "dst-newer");
    write(&dst.join("same.txt"), "same");
    set_mtime(&dst.join("same.txt"), old());
    write(&dst.join("extra.txt"), "only at destination");
    write(&dst.join("extra_dir").join("e.txt"), "only at destination");
    Fixture {
        _tmp: tmp,
        src,
        dst_parent,
        dst,
    }
}

fn copy_dir(f: &Fixture, mode: ConflictMode) -> Operation {
    Operation::Copy {
        sources: vec![nav(&f.src)],
        dest_dir: nav(&f.dst_parent),
        mode,
    }
}

/// A saved job's folder step: the source's *contents* mirrored into the
/// destination folder itself, with the job's excludes kept out of both
/// the copy and the prune.
#[test]
fn sync_dir_mirrors_contents_into_the_destination_itself_and_honours_excludes() {
    let f = fixture();
    write(&f.src.join("node_modules").join("dep.js"), "dep");
    write(&f.dst.join("node_modules").join("stale.js"), "stale");
    let driver = RobocopyDriver::from_path().with_extra_flags(vec![
        "/XD".into(),
        "node_modules".into(),
        "/XF".into(),
        "node_modules".into(),
    ]);
    let out = run_with(
        driver,
        Operation::SyncDir {
            src: nav(&f.src),
            dst: nav(&f.dst),
            mode: ConflictMode::Mirror,
        },
    );
    assert!(out.success, "{:?}", out.error);
    // Contents landed in `dst`, not in `dst\photos`.
    assert_eq!(read(&f.dst.join("new.txt")), "new");
    assert_eq!(read(&f.dst.join("sub").join("deep.txt")), "deep");
    assert!(!f.dst.join("photos").exists());
    // Mirror pruned the extras…
    assert!(!f.dst.join("extra.txt").exists());
    assert!(!f.dst.join("extra_dir").exists());
    // …but an excluded folder is neither copied nor pruned.
    assert!(!f.dst.join("node_modules").join("dep.js").exists());
    assert!(f.dst.join("node_modules").join("stale.js").exists());
}

#[test]
fn update_copies_what_differs_and_spares_newer_destinations() {
    let f = fixture();
    let o = run(copy_dir(&f, ConflictMode::Update));
    assert!(o.success, "{:?}", o.error);
    assert_eq!(read(&f.dst.join("new.txt")), "new");
    assert_eq!(read(&f.dst.join("stale.txt")), "src-newer");
    assert_eq!(
        read(&f.dst.join("edited.txt")),
        "dst-newer",
        "newer destination kept"
    );
    assert_eq!(read(&f.dst.join("sub").join("deep.txt")), "deep");
    assert_eq!(read(&f.dst.join("ñandú ｜x.txt")), "wide");
    assert!(f.dst.join("extra.txt").exists(), "only Mirror deletes");
}

#[test]
fn add_new_only_never_touches_an_existing_file() {
    let f = fixture();
    let o = run(copy_dir(&f, ConflictMode::AddNewOnly));
    assert!(o.success, "{:?}", o.error);
    assert_eq!(read(&f.dst.join("new.txt")), "new");
    assert_eq!(read(&f.dst.join("stale.txt")), "dst-older");
    assert_eq!(read(&f.dst.join("edited.txt")), "dst-newer");
}

#[test]
fn replace_overwrites_even_a_newer_destination() {
    let f = fixture();
    let o = run(copy_dir(&f, ConflictMode::Replace));
    assert!(o.success, "{:?}", o.error);
    assert_eq!(read(&f.dst.join("stale.txt")), "src-newer");
    assert_eq!(read(&f.dst.join("edited.txt")), "src-older");
    assert!(f.dst.join("extra.txt").exists());
}

#[test]
fn mirror_makes_the_destination_match_and_prunes_the_rest() {
    let f = fixture();
    let o = run(copy_dir(&f, ConflictMode::Mirror));
    assert!(o.success, "{:?}", o.error);
    assert!(!f.dst.join("extra.txt").exists());
    assert!(!f.dst.join("extra_dir").exists());
    // rclone sync's default comparison: anything that differs is copied,
    // in either direction.
    assert_eq!(read(&f.dst.join("edited.txt")), "src-older");
    assert_eq!(read(&f.dst.join("stale.txt")), "src-newer");
    assert_eq!(read(&f.dst.join("ñandú ｜x.txt")), "wide");
}

/// Robocopy copies the mtime, so a later Update compares like with like.
#[test]
fn copied_files_keep_their_modification_time() {
    let f = fixture();
    assert!(run(copy_dir(&f, ConflictMode::Update)).success);
    let s = fs::metadata(f.src.join("sub").join("deep.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let d = fs::metadata(f.dst.join("sub").join("deep.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(s, d);
}

/// A move under Update: everything not protected leaves the source,
/// *including* a file already identical at the destination (`/MOV` alone
/// would leave it behind). The newer destination is protected, so its
/// source stays — never deleted unseen as rclone's `move --update` does.
#[test]
fn update_move_empties_the_source_except_what_it_protects() {
    let f = fixture();
    let o = run(Operation::Move {
        sources: vec![nav(&f.src)],
        dest_dir: nav(&f.dst_parent),
        mode: ConflictMode::Update,
    });
    assert!(o.success, "{:?}", o.error);
    assert!(
        !f.src.join("same.txt").exists(),
        "identical file must still move"
    );
    assert!(!f.src.join("new.txt").exists());
    assert!(!f.src.join("stale.txt").exists());
    assert!(!f.src.join("sub").exists());
    assert_eq!(
        read(&f.src.join("edited.txt")),
        "src-older",
        "protected, so kept"
    );
    assert_eq!(read(&f.dst.join("edited.txt")), "dst-newer");
    assert_eq!(read(&f.dst.join("stale.txt")), "src-newer");
}

#[test]
fn add_new_only_move_leaves_existing_names_in_the_source() {
    let f = fixture();
    let o = run(Operation::Move {
        sources: vec![nav(&f.src)],
        dest_dir: nav(&f.dst_parent),
        mode: ConflictMode::AddNewOnly,
    });
    assert!(o.success, "{:?}", o.error);
    assert!(!f.src.join("new.txt").exists());
    for kept in ["stale.txt", "edited.txt", "same.txt"] {
        assert!(f.src.join(kept).exists(), "{kept}");
    }
    assert_eq!(read(&f.dst.join("stale.txt")), "dst-older");
}

#[test]
fn moving_a_single_file_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a").join("one.txt");
    write(&src, "x");
    write(&tmp.path().join("a").join("sibling.txt"), "stay");
    let dst = tmp.path().join("b");
    fs::create_dir_all(&dst).unwrap();
    let o = run(Operation::Move {
        sources: vec![nav(&src)],
        dest_dir: nav(&dst),
        mode: ConflictMode::Update,
    });
    assert!(o.success, "{:?}", o.error);
    assert!(!src.exists());
    assert_eq!(read(&dst.join("one.txt")), "x");
    assert!(tmp.path().join("a").join("sibling.txt").exists());
    assert!(!dst.join("sibling.txt").exists(), "only the selected file");
}

/// A batch copies exactly its listed names — the unlisted sibling must
/// not come along — and handles names rclone has to translate.
#[test]
fn batch_copies_exactly_the_listed_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let names = [
        "a.txt",
        "with space.txt",
        "＂quoted＂ ｜pipe.mp3",
        "‛＂Title‛＂ [id].mp3",
    ];
    for n in names {
        write(&src.join(n), n);
    }
    write(&src.join("unlisted.txt"), "no");
    fs::create_dir_all(&dst).unwrap();
    let o = run(Operation::CopyBatch {
        src_root: nav(&src),
        list_file: tmp.path().join("unused.txt"),
        names: names.iter().map(|s| s.to_string()).collect(),
        dest_dir: nav(&dst),
        mode: ConflictMode::Update,
    });
    assert!(o.success, "{:?}", o.error);
    for n in names {
        assert_eq!(read(&dst.join(n)), n);
    }
    assert!(!dst.join("unlisted.txt").exists());
}

/// Mirror on a batch would purge everything unlisted. It must not.
#[test]
fn a_mirror_batch_never_deletes_unlisted_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a.txt"), "a");
    write(&src.join("b.txt"), "b");
    write(&dst.join("keep_me.txt"), "precious");
    let o = run(Operation::CopyBatch {
        src_root: nav(&src),
        list_file: tmp.path().join("unused.txt"),
        names: vec!["a.txt".into(), "b.txt".into()],
        dest_dir: nav(&dst),
        mode: ConflictMode::Mirror,
    });
    assert!(o.success, "{:?}", o.error);
    assert_eq!(read(&dst.join("keep_me.txt")), "precious");
    assert_eq!(read(&dst.join("a.txt")), "a");
}

#[test]
fn a_batch_longer_than_one_command_line_copies_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let names: Vec<String> = (0..1500)
        .map(|i| format!("a fairly long file name number {i:05}.txt"))
        .collect();
    for n in &names {
        write(&src.join(n), "x");
    }
    fs::create_dir_all(&dst).unwrap();
    let o = run(Operation::CopyBatch {
        src_root: nav(&src),
        list_file: tmp.path().join("unused.txt"),
        names: names.clone(),
        dest_dir: nav(&dst),
        mode: ConflictMode::Update,
    });
    assert!(o.success, "{:?}", o.error);
    assert_eq!(fs::read_dir(&dst).unwrap().count(), names.len());
    let last = o.progress.last().expect("progress");
    assert_eq!(last.files_total, names.len() as u64);
}

/// The conflict dialog's two lists, per mode.
#[test]
fn conflicts_name_what_each_mode_would_destroy() {
    let f = fixture();
    let rc = RobocopyDriver::from_path();
    let names = |v: &[std::path::PathBuf]| {
        let mut v: Vec<String> = v.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        v.sort();
        v
    };

    let r = rc.conflicts(&copy_dir(&f, ConflictMode::Update)).unwrap();
    assert_eq!(names(&r.overwrites), ["stale.txt"]);
    assert!(r.deletes.is_empty());

    let r = rc.conflicts(&copy_dir(&f, ConflictMode::Replace)).unwrap();
    assert_eq!(
        names(&r.overwrites),
        ["edited.txt", "same.txt", "stale.txt"]
    );

    let r = rc.conflicts(&copy_dir(&f, ConflictMode::Mirror)).unwrap();
    assert_eq!(names(&r.overwrites), ["edited.txt", "stale.txt"]);
    assert_eq!(names(&r.deletes), ["extra.txt", "extra_dir"]);

    assert!(
        rc.conflicts(&copy_dir(&f, ConflictMode::AddNewOnly))
            .unwrap()
            .is_empty()
    );
    assert!(
        !f.dst.join("new.txt").exists(),
        "detection must not copy anything"
    );
    assert!(f.dst.join("extra.txt").exists(), "nor delete anything");
}

/// A move that only has identical files to "overwrite" puts nothing at
/// risk and must not raise a dialog for them.
#[test]
fn identical_files_are_not_conflicts_for_a_move() {
    let f = fixture();
    let r = RobocopyDriver::from_path()
        .conflicts(&Operation::Move {
            sources: vec![nav(&f.src)],
            dest_dir: nav(&f.dst_parent),
            mode: ConflictMode::Update,
        })
        .unwrap();
    let mut got: Vec<String> = r
        .overwrites
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    got.sort();
    assert_eq!(got, ["stale.txt"]);
}

#[test]
fn progress_reports_totals_up_front() {
    let f = fixture();
    let o = run(copy_dir(&f, ConflictMode::Update));
    assert!(o.success);
    let p = o.progress.last().expect("at least one progress tick");
    // new, stale, sub\deep, ñandú — edited and same are skipped by Update.
    assert_eq!(p.files_total, 4);
    assert_eq!(p.bytes_total, (3 + 9 + 4 + 4) as u64);
    assert_eq!(p.bytes_done, p.bytes_total);
}

#[test]
fn a_missing_source_fails_as_not_found() {
    // The GUI filters vanished sources before a paste; what still reaches
    // robocopy is a source whose folder disappeared in between.
    let tmp = tempfile::tempdir().unwrap();
    let missing_parent = tmp.path().join("gone").join("x.txt");
    let o = run(Operation::Copy {
        sources: vec![nav(&missing_parent)],
        dest_dir: nav(tmp.path()),
        mode: ConflictMode::Update,
    });
    assert!(!o.success);
    let e = o.error.unwrap();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");
}

/// A locked destination fails that file — reported once, named, with
/// robocopy's own sentence — and the rest of the copy still lands.
#[test]
fn a_locked_file_is_reported_and_the_rest_still_copies() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("locked.txt"), "new");
    write(&src.join("ok.txt"), "ok");
    write(&dst.join("locked.txt"), "old");
    let _hold = fs::File::options()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(dst.join("locked.txt"))
        .unwrap();
    let o = run(Operation::CopyBatch {
        src_root: nav(&src),
        list_file: tmp.path().join("unused.txt"),
        names: vec!["locked.txt".into(), "ok.txt".into()],
        dest_dir: nav(&dst),
        mode: ConflictMode::Replace,
    });
    assert!(!o.success);
    let e = o.error.unwrap();
    assert_eq!(e.object.as_deref(), Some("locked.txt"), "{e:?}");
    assert!(!e.message.is_empty());
    assert_eq!(e.detail.len(), 1, "a retry is not a second failure: {e:?}");
    assert_eq!(read(&dst.join("ok.txt")), "ok");
}

/// Cancelling must never leave a destination that looks finished but
/// isn't. With `/MT` robocopy preallocates every file at full size and
/// only stamps the mtime at the end, so a killed copy left full-size
/// files with fresh mtimes that Update would then skip forever. Whatever
/// the timing, every destination file afterwards is either absent or a
/// complete copy carrying the source's mtime.
#[test]
fn cancelling_leaves_no_half_written_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big");
    let dst_parent = tmp.path().join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst_parent).unwrap();
    let chunk = vec![7u8; 128 * 1024 * 1024];
    for i in 0..6 {
        fs::write(src.join(format!("f{i}.bin")), &chunk).unwrap();
        set_mtime(&src.join(format!("f{i}.bin")), old());
    }
    let h = RobocopyDriver::from_path()
        .spawn(Operation::Copy {
            sources: vec![nav(&src)],
            dest_dir: nav(&dst_parent),
            mode: ConflictMode::Update,
        })
        .unwrap();
    let canceller = h.canceller();
    let mut cancelled = false;
    let mut success = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        match h.events.recv_timeout(Duration::from_millis(50)) {
            Ok(OpEvent::Progress(p)) if !cancelled && p.bytes_done > 0 => {
                canceller.cancel();
                cancelled = true;
            }
            Ok(OpEvent::Done { success: s, .. }) => {
                success = Some(s);
                break;
            }
            _ => {}
        }
    }
    assert!(cancelled || success == Some(true), "never saw progress");
    let dst = dst_parent.join("big");
    for i in 0..6 {
        let d = dst.join(format!("f{i}.bin"));
        let Ok(m) = fs::metadata(&d) else { continue };
        assert_eq!(m.len(), chunk.len() as u64, "{d:?}");
        assert_eq!(m.modified().unwrap(), old(), "{d:?} is a partial copy");
    }
}

/// A move with nothing to protect takes the folder itself, not just its
/// contents — otherwise every completed move would report a leftover.
#[test]
fn a_complete_folder_move_removes_the_source_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a").join("photos");
    write(&src.join("x.txt"), "x");
    write(&src.join("sub").join("y.txt"), "y");
    let dst = tmp.path().join("b");
    fs::create_dir_all(&dst).unwrap();
    let o = run(Operation::Move {
        sources: vec![nav(&src)],
        dest_dir: nav(&dst),
        mode: ConflictMode::Update,
    });
    assert!(o.success, "{:?}", o.error);
    assert!(!src.exists(), "source folder must be gone");
    assert_eq!(read(&dst.join("photos").join("sub").join("y.txt")), "y");
}
