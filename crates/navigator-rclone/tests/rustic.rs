//! End-to-end tests against the real `rustic` on `PATH`. Each test builds
//! its own throwaway repository (`rustic init` + `rustic backup` of a small
//! tree), then drives [`RusticDriver`] the way the GUI does and checks the
//! result on disk. Skipped, loudly, when rustic is not installed.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use navigator_core::NavPath;
use navigator_rclone::op::OpEvent;
use navigator_rclone::rustic::{
    RusticError, RusticSettings, snapshot_id_of, snapshot_row_name, snapshot_start,
};
use navigator_rclone::{Operation, RusticDriver};

const PASSWORD: &str = "correct horse";

fn rustic_available() -> bool {
    let ok = Command::new("rustic")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("rustic not on PATH — skipping");
    }
    ok
}

/// Whether the developer's own environment supplies a password, which
/// would make "no password configured" untestable from here.
fn ambient_password() -> bool {
    [
        "RUSTIC_PASSWORD",
        "RUSTIC_PASSWORD_FILE",
        "RUSTIC_PASSWORD_COMMAND",
    ]
    .iter()
    .any(|v| std::env::var_os(v).is_some())
}

fn write(p: &Path, body: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: String,
    /// rclone path to purge on drop, for a repository made on a real remote.
    remote_dir: Option<String>,
    /// Row name of the one snapshot, as the GUI lists it.
    row: String,
    /// Where the backed-up `data` folder sits inside the snapshot tree.
    data_sub: String,
}

impl Fixture {
    fn new() -> Self {
        Self::at(|root| (root.join("repo").to_string_lossy().into_owned(), None))
    }

    /// A fixture whose repository lives wherever `repo_of(tmp root)` says:
    /// the repo spec, plus an rclone path to purge afterwards if any.
    fn at(repo_of: impl FnOnce(&Path) -> (String, Option<String>)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let data = root.join("data");
        write(&data.join("a.txt"), b"hello\n");
        write(&data.join("docs").join("b.md"), b"world\n");
        write(&data.join("docs").join("nested").join("big.bin"), &vec![7u8; 300_000]);
        write(&data.join("docs").join("＂quoted＂.txt"), b"q\n");
        write(&data.join("docs").join("report [final].txt"), b"r\n");
        write(&data.join("with space").join("c d.txt"), b"x\n");
        let (repo, remote_dir) = repo_of(&root);
        for args in [
            vec!["init".to_string()],
            vec!["backup".to_string(), data.to_string_lossy().into_owned()],
        ] {
            let out = Command::new("rustic")
                .arg("-r")
                .arg(&repo)
                .args(&args)
                .env("RUSTIC_PASSWORD", PASSWORD)
                .env_remove("RUSTIC_PASSWORD_FILE")
                .env_remove("RUSTIC_PASSWORD_COMMAND")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "rustic {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let d = unlocked();
        let snaps = d.snapshots(&repo).unwrap();
        assert_eq!(snaps.len(), 1);
        let row = snapshot_row_name(&snaps[0], false);
        let data_sub = snapshot_start(&snaps[0]);
        Self {
            _tmp: tmp,
            root,
            repo,
            remote_dir,
            row,
            data_sub,
        }
    }

    fn snap(&self, rel: &str) -> NavPath {
        let sub = if rel.is_empty() {
            self.data_sub.clone()
        } else {
            format!("{}/{}", self.data_sub, rel)
        };
        NavPath::rustic(&self.repo, &self.row, &sub)
    }

    fn out(&self, rel: &str) -> PathBuf {
        self.root.join("out").join(rel)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(dir) = &self.remote_dir {
            let _ = Command::new("rclone").arg("purge").arg(dir).output();
        }
    }
}

/// A driver that already knows the password.
fn unlocked() -> RusticDriver {
    let d = RusticDriver::new(RusticSettings::default());
    d.set_prompter(Arc::new(|_, _| Some(PASSWORD.to_string())));
    d
}

fn restore(d: &RusticDriver, src: NavPath, dst: &Path, excludes: Vec<String>) -> (bool, Option<String>) {
    let h = d
        .spawn(Operation::RusticRestore {
            src,
            dst: NavPath::new(dst.to_path_buf()).unwrap(),
            excludes,
            delete: false,
        })
        .expect("spawn");
    for ev in h.events.iter() {
        if let OpEvent::Done { success, error, .. } = ev {
            return (success, error.map(|e| e.reason()));
        }
    }
    panic!("no Done event")
}

#[test]
fn prompts_once_then_remembers_the_password() {
    if !rustic_available() || ambient_password() {
        return;
    }
    let fx = Fixture::new();
    let d = RusticDriver::new(RusticSettings::default());
    let asked = Arc::new(AtomicUsize::new(0));
    let a = Arc::clone(&asked);
    let repo = fx.repo.clone();
    d.set_prompter(Arc::new(move |r, retry| {
        assert_eq!(r, repo);
        let n = a.fetch_add(1, Ordering::SeqCst);
        // First answer is wrong; the second ask must say so.
        assert_eq!(retry, n > 0);
        Some(if n == 0 { "nope".into() } else { PASSWORD.into() })
    }));
    assert_eq!(d.snapshots(&fx.repo).unwrap().len(), 1);
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    d.list(&fx.snap("")).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 2, "a remembered password is not asked for again");
}

#[test]
fn cancelling_the_prompt_is_its_own_error() {
    if !rustic_available() || ambient_password() {
        return;
    }
    let fx = Fixture::new();
    let d = RusticDriver::new(RusticSettings::default());
    d.set_prompter(Arc::new(|_, _| None));
    assert_eq!(d.snapshots(&fx.repo).unwrap_err(), RusticError::PasswordCancelled);
    // And with nobody to ask at all.
    let d = RusticDriver::new(RusticSettings::default());
    assert_eq!(d.snapshots(&fx.repo).unwrap_err(), RusticError::PasswordCancelled);
}

#[test]
fn a_configured_password_file_means_no_prompt() {
    if !rustic_available() {
        return;
    }
    let fx = Fixture::new();
    let file = fx.root.join("pw.txt");
    fs::write(&file, PASSWORD).unwrap();
    let d = RusticDriver::new(RusticSettings {
        password_file: file.to_string_lossy().into_owned(),
        ..Default::default()
    });
    d.set_prompter(Arc::new(|_, _| panic!("must not prompt")));
    assert_eq!(d.snapshots(&fx.repo).unwrap().len(), 1);
}

#[test]
fn lists_snapshots_and_directories() {
    if !rustic_available() {
        return;
    }
    let fx = Fixture::new();
    let d = unlocked();
    let root = d.list(&NavPath::rustic_repo(&fx.repo)).unwrap();
    assert_eq!(root.len(), 1);
    assert!(root[0].is_dir());
    assert_eq!(snapshot_id_of(&root[0].name), snapshot_id_of(&fx.row));
    assert!(root[0].modified.0 > 0);

    let mut names: Vec<(String, bool, u64)> = d
        .list(&fx.snap(""))
        .unwrap()
        .into_iter()
        .map(|e| (e.name.clone(), e.is_dir(), e.size))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("a.txt".into(), false, 6),
            ("docs".into(), true, 0),
            ("with space".into(), true, 0),
        ]
    );
    let docs = d.list(&fx.snap("docs")).unwrap();
    assert!(docs.iter().any(|e| e.name == "＂quoted＂.txt"));
    assert!(d.is_dir(&fx.snap("docs")).unwrap());
    assert!(!d.is_dir(&fx.snap("a.txt")).unwrap());
    assert!(matches!(d.list(&fx.snap("nope")), Err(RusticError::NotFound(_))));
}

#[test]
fn restores_a_file_and_a_folder_to_exact_destinations() {
    if !rustic_available() {
        return;
    }
    let fx = Fixture::new();
    let d = unlocked();
    let (ok, err) = restore(&d, fx.snap("a.txt"), &fx.out("a.txt"), vec![]);
    assert!(ok, "{err:?}");
    assert_eq!(fs::read(fx.out("a.txt")).unwrap(), b"hello\n");
    // A folder's *contents* land in the destination, so it is named for it.
    let (ok, err) = restore(&d, fx.snap("docs"), &fx.out("docs"), vec![]);
    assert!(ok, "{err:?}");
    assert_eq!(fs::read(fx.out("docs").join("b.md")).unwrap(), b"world\n");
    assert_eq!(fs::metadata(fx.out("docs").join("nested").join("big.bin")).unwrap().len(), 300_000);
    assert!(fx.out("docs").join("＂quoted＂.txt").exists());
    // Nothing comes back read-only (restic does that to rustic snapshots).
    assert!(!fs::metadata(fx.out("docs").join("b.md")).unwrap().permissions().readonly());
}

#[test]
fn diff_names_what_a_restore_would_overwrite_and_excludes_protect_it() {
    if !rustic_available() {
        return;
    }
    let fx = Fixture::new();
    let d = unlocked();
    let dst = fx.out("docs");
    assert!(restore(&d, fx.snap("docs"), &dst, vec![]).0);
    write(&dst.join("b.md"), b"edited locally\n");
    write(&dst.join("report [final].txt"), b"also edited\n");
    write(&dst.join("mine.txt"), b"only here\n");
    // A file where the snapshot has a folder: restoring over that crashes
    // rustic, so it must show up as `T` for the caller to exclude.
    fs::remove_dir_all(dst.join("nested")).unwrap();
    write(&dst.join("nested"), b"now a file\n");

    let mut diff: Vec<(char, String)> = d
        .diff(&fx.snap("docs"), &dst)
        .unwrap()
        .into_iter()
        .map(|e| (e.status, e.path))
        .collect();
    diff.sort();
    assert!(diff.contains(&('M', "b.md".into())), "{diff:?}");
    assert!(diff.contains(&('M', "report [final].txt".into())), "{diff:?}");
    assert!(diff.contains(&('+', "mine.txt".into())), "{diff:?}");
    assert!(diff.contains(&('T', "nested".into())), "{diff:?}");

    let excludes: Vec<String> = diff
        .iter()
        .filter(|(s, _)| matches!(s, 'M' | 'T'))
        .map(|(_, p)| p.clone())
        .collect();
    let (ok, err) = restore(&d, fx.snap("docs"), &dst, excludes);
    assert!(ok, "{err:?}");
    assert_eq!(fs::read(dst.join("b.md")).unwrap(), b"edited locally\n");
    assert_eq!(fs::read(dst.join("report [final].txt")).unwrap(), b"also edited\n");
    assert_eq!(fs::read(dst.join("nested")).unwrap(), b"now a file\n");
    assert!(dst.join("mine.txt").exists());

    // Without excludes the modified files come back from the snapshot.
    fs::remove_file(dst.join("nested")).unwrap();
    let (ok, err) = restore(&d, fx.snap("docs"), &dst, vec![]);
    assert!(ok, "{err:?}");
    assert_eq!(fs::read(dst.join("b.md")).unwrap(), b"world\n");
    assert_eq!(fs::read(dst.join("report [final].txt")).unwrap(), b"r\n");
    assert!(dst.join("mine.txt").exists(), "a plain restore never deletes");
}

#[test]
fn a_restore_from_a_missing_path_fails_with_a_sentence() {
    if !rustic_available() {
        return;
    }
    let fx = Fixture::new();
    let d = unlocked();
    let (ok, err) = restore(&d, fx.snap("gone.txt"), &fx.out("gone.txt"), vec![]);
    assert!(!ok);
    let err = err.unwrap();
    assert!(err.contains("not found") || err.contains("gone.txt"), "{err}");
    assert!(!err.contains("Backtrace"), "{err}");
}

#[test]
fn cancelling_leaves_no_partial_new_destination() {
    if !rustic_available() {
        return;
    }
    let fx = Fixture::new();
    let d = unlocked();
    let dst = fx.out("docs");
    let h = d
        .spawn(Operation::RusticRestore {
            src: fx.snap("docs"),
            dst: NavPath::new(dst.clone()).unwrap(),
            excludes: vec![],
            delete: false,
        })
        .unwrap();
    h.cancel();
    for ev in h.events.iter() {
        if let OpEvent::Done { success, .. } = ev {
            assert!(!success, "a cancelled restore is not a success");
            break;
        }
    }
    assert!(!dst.exists(), "the half-restored folder was ours to remove");
}

fn rclone_available() -> bool {
    let ok = Command::new("rclone")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("rclone not on PATH — skipping");
    }
    ok
}

/// A repository reached through rustic's `rclone:` backend, which is what
/// Tools → Open as rustic backup builds for a folder on a remote. Offline
/// by default: an on-the-fly `:local:` remote over the temp dir, so the
/// whole `rclone serve restic` path runs with no network. Set
/// `NAVIGATOR_RUSTIC_TEST_REMOTE` to a real remote folder
/// (`sb:/home/scratch`) to run it there instead; it works in a fresh
/// sub-folder and purges it afterwards.
#[test]
fn lists_and_restores_through_an_rclone_remote() {
    if !rustic_available() || !rclone_available() {
        return;
    }
    let fx = Fixture::at(|root| match std::env::var("NAVIGATOR_RUSTIC_TEST_REMOTE") {
        Ok(base) => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = format!(
                "{}/navigator-test-{}-{nanos}",
                base.trim_end_matches('/'),
                std::process::id()
            );
            (format!("rclone:{dir}"), Some(dir))
        }
        // rustic reads `rclone:C:\…` as a local path; forward slashes and
        // an explicit backend keep it on rclone.
        Err(_) => {
            let local = root.join("repo").to_string_lossy().replace('\\', "/");
            (format!("rclone::local:{local}"), None)
        }
    });
    let d = unlocked();
    let root = d.list(&NavPath::rustic_repo(&fx.repo)).unwrap();
    assert_eq!(root.len(), 1);
    let names: Vec<String> = d.list(&fx.snap("docs")).unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.iter().any(|n| n == "b.md"), "{names:?}");

    let (ok, err) = restore(&d, fx.snap("docs"), &fx.out("docs"), vec![]);
    assert!(ok, "{err:?}");
    assert_eq!(fs::read(fx.out("docs").join("b.md")).unwrap(), b"world\n");

    write(&fx.out("docs").join("b.md"), b"changed\n");
    let diff = d.diff(&fx.snap("docs"), &fx.out("docs")).unwrap();
    assert!(diff.iter().any(|e| e.status == 'M' && e.path == "b.md"), "{diff:?}");
}
