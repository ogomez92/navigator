//! End-to-end cover for the recursive Extract sweep.
//!
//! The pure halves (extension sets, volume parsing, the confirmation
//! wording) are unit-tested inside `extract.rs`. What can only be pinned
//! against a real `7z.exe` is the chain they feed: a folder handed to
//! Ctrl+E is walked, every archive found is extracted *next to itself*
//! rather than into the folder the user was standing in, the whole volume
//! set is purged afterwards, and nothing the sweep declined to take is
//! touched.
//!
//! Skipped (not failed) when 7-Zip isn't installed — the binary is a
//! runtime dependency, not a build one.

#![cfg(windows)]

use std::path::{Path, PathBuf};

use navigator_config::Extraction;
use navigator_core::NavPath;
use navigator_gui::extract;

/// Scratch directory for one test, deleted when the returned guard drops.
///
/// A guard rather than a bare path: these tests drive a real `7z.exe`, so
/// an assertion that fires part-way through would otherwise strand a tree
/// of archives and extracted output in `%TEMP%` permanently. The name is
/// unique per call, so a previous run cannot answer the assertions either.
fn scratch(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("nav-extract-e2e-{tag}-"))
        .tempdir()
        .expect("mkdir scratch")
}

/// Build `dest` from the files in `src_dir` using the real 7z. Returns
/// false if 7z refused, so the caller can fail with a useful message.
fn make_zip(seven_zip: &Path, src_dir: &Path, dest: &Path) -> bool {
    std::process::Command::new(seven_zip)
        .current_dir(src_dir)
        .args(["a", "-tzip", "-y", "--"])
        .arg(dest)
        .arg("*")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run(targets: Vec<NavPath>, opts: Extraction, seven_zip: PathBuf) {
    let (tx, _rx) = crossbeam_channel::unbounded();
    // `load_or_default` resolves next to the *test* binary
    // (`target/debug/deps`), so this never reads or writes the user's
    // real config. No sound files are mapped there, so `play` is a no-op.
    let sound =
        navigator_gui::sound::SoundPlayer::start(navigator_config::ConfigHandle::load_or_default());
    extract::run_extract(targets, opts, seven_zip, tx, sound);
}

#[test]
fn a_swept_folder_extracts_each_archive_in_place_and_purges_it() {
    let Some(seven_zip) = extract::find_7z() else {
        eprintln!("7z not installed; skipping");
        return;
    };
    let tmp = scratch("sweep");
    let root = tmp.path();

    // Staging area for the archive's contents. Two entries, so the
    // wrap-folder rule applies and the output is a named subfolder.
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("one.txt"), b"one").unwrap();
    std::fs::write(staging.join("two.txt"), b"two").unwrap();

    // The archive lives two levels down: the whole point is that the
    // sweep reaches it and that it unpacks *there*, not at the root.
    let deep = root.join("a").join("b");
    std::fs::create_dir_all(&deep).unwrap();
    let archive = deep.join("inner.zip");
    assert!(make_zip(&seven_zip, &staging, &archive), "7z a failed");
    std::fs::remove_dir_all(&staging).unwrap();

    // Bait: neither may be taken by a recursive sweep. `.exe` is
    // extractable when pointed at directly and must not be swept;
    // `.txt` is not an archive at all.
    std::fs::write(root.join("keep.exe"), b"MZ").unwrap();
    std::fs::write(deep.join("notes.txt"), b"notes").unwrap();

    let found = extract::sweep_archives(root);
    assert_eq!(found, vec![archive.clone()], "sweep took the wrong set");

    run(
        vec![NavPath::new(&archive).unwrap()],
        Extraction {
            delete_when_extracted: true,
            create_folder: true,
        },
        seven_zip,
    );

    // Extracted beside the archive, wrapped in the archive's own name.
    let out = deep.join("inner");
    assert!(out.join("one.txt").is_file(), "missing {:?}", out);
    assert!(out.join("two.txt").is_file(), "missing {:?}", out);
    // Nothing landed in the folder the user was standing in.
    assert!(!root.join("one.txt").exists(), "extracted into the root");
    // Purged, and only it.
    assert!(!archive.exists(), "archive was not deleted");
    assert!(root.join("keep.exe").is_file(), "swept an executable");
    assert!(deep.join("notes.txt").is_file(), "deleted a non-archive");
}

#[test]
fn a_failed_extraction_keeps_its_archive() {
    let Some(seven_zip) = extract::find_7z() else {
        eprintln!("7z not installed; skipping");
        return;
    };
    let tmp = scratch("corrupt");
    let root = tmp.path();
    // Named like an archive, isn't one — 7z exits non-zero, and a delete
    // here would destroy the only copy of whatever it really is.
    let bogus = root.join("broken.zip");
    std::fs::write(&bogus, b"not actually a zip file").unwrap();

    run(
        vec![NavPath::new(&bogus).unwrap()],
        Extraction {
            delete_when_extracted: true,
            create_folder: true,
        },
        seven_zip,
    );

    assert!(bogus.is_file(), "a failed extraction deleted its archive");
}

#[test]
fn a_split_7z_extracts_from_its_first_part_and_purges_the_whole_set() {
    // The whole chain for `name.7z.001`: the extension is `001`, which
    // is in no extension table, so recognising it is a filename rule
    // rather than a list entry — and the purge has to take every part,
    // since a set missing `.001` is a set nothing can open.
    let Some(seven_zip) = extract::find_7z() else {
        eprintln!("7z not installed; skipping");
        return;
    };
    let tmp = scratch("split");
    let root = tmp.path();

    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    // Two entries so the wrapper folder applies, and big enough that
    // `-v64k` really produces several volumes. Incompressible content
    // (a counter, not a run of one byte) keeps 7z from folding it back
    // into a single part.
    for (seed, name) in [(0x1234_5678u32, "one.bin"), (0x9e37_79b9, "two.bin")] {
        let mut x = seed;
        let blob: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        std::fs::write(staging.join(name), &blob).unwrap();
    }

    let archive = root.join("Split Set.7z");
    let ok = std::process::Command::new(&seven_zip)
        .current_dir(&staging)
        .args(["a", "-t7z", "-v64k", "-y", "--"])
        .arg(&archive)
        .arg("*")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "7z a -v64k failed");
    std::fs::remove_dir_all(&staging).unwrap();

    let first = root.join("Split Set.7z.001");
    let second = root.join("Split Set.7z.002");
    assert!(first.is_file(), "7z did not produce .001");
    assert!(second.is_file(), "7z did not split into several volumes");
    assert!(!archive.exists(), "a split write left an unsplit archive");

    // Selecting every part is the normal Ctrl+A gesture; only the first
    // may be queued, and the rest must not come back as failures.
    let parts: Vec<(NavPath, bool)> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| (NavPath::new(e.path()).unwrap(), false))
        .collect();
    let (direct, _) = extract::split_extract_selection(&parts);
    assert_eq!(
        direct,
        vec![NavPath::new(&first).unwrap()],
        "queued the whole set"
    );

    run(
        direct,
        Extraction {
            delete_when_extracted: true,
            create_folder: true,
        },
        seven_zip,
    );

    // Wrapped in the set's name, with the volume number gone from it.
    let out = root.join("Split Set");
    assert!(out.join("one.bin").is_file(), "missing {:?}", out);
    assert!(out.join("two.bin").is_file(), "missing {:?}", out);
    // Every part purged, not just the one 7z was pointed at.
    let leftovers: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".7z."))
        .collect();
    assert!(leftovers.is_empty(), "orphaned volumes: {leftovers:?}");
}
