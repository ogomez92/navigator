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

// --- Keep-both scope ------------------------------------------------------

/// Keep-both acts on the *selected* items, not on files buried in a merging
/// tree. Only sources whose destination name is already taken get renamed.
#[test]
fn top_level_conflicts_lists_only_colliding_selections() {
    use navigator_core::NavPath;
    use navigator_gui::preflight::top_level_conflicts;

    let tmp = tempdir();
    let src_dir = tmp.join("src");
    let dst_dir = tmp.join("dst");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();

    touch(&src_dir.join("collides.txt"));
    touch(&dst_dir.join("collides.txt"));
    touch(&src_dir.join("fresh.txt"));

    let sources = vec![
        NavPath::new(src_dir.join("collides.txt")).unwrap(),
        NavPath::new(src_dir.join("fresh.txt")).unwrap(),
    ];
    let got = top_level_conflicts(&sources, &NavPath::new(dst_dir).unwrap());
    let names: Vec<String> = got.iter().map(|p| p.file_name().to_string()).collect();
    assert_eq!(names, vec!["collides.txt"]);
}

/// A directory that already exists at the destination is a top-level
/// conflict in its own right — Keep-both renames the whole incoming folder
/// rather than numbering files inside it.
#[test]
fn top_level_conflicts_counts_directories_as_whole_units() {
    use navigator_core::NavPath;
    use navigator_gui::preflight::top_level_conflicts;

    let tmp = tempdir();
    let src_dir = tmp.join("src");
    let dst_dir = tmp.join("dst");
    fs::create_dir_all(src_dir.join("photos")).unwrap();
    fs::create_dir_all(dst_dir.join("photos")).unwrap();
    // An inner file that would conflict on a merge — irrelevant here.
    touch(&src_dir.join("photos").join("a.jpg"));
    touch(&dst_dir.join("photos").join("a.jpg"));

    let sources = vec![NavPath::new(src_dir.join("photos")).unwrap()];
    let got = top_level_conflicts(&sources, &NavPath::new(dst_dir).unwrap());
    assert_eq!(got.len(), 1, "the folder itself is the conflict");
    assert_eq!(got[0].file_name(), "photos");
}

#[test]
fn top_level_conflicts_is_empty_for_a_clean_destination() {
    use navigator_core::NavPath;
    use navigator_gui::preflight::top_level_conflicts;

    let tmp = tempdir();
    let src_dir = tmp.join("src");
    let dst_dir = tmp.join("dst");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();
    touch(&src_dir.join("a.txt"));

    let sources = vec![NavPath::new(src_dir.join("a.txt")).unwrap()];
    assert!(
        top_level_conflicts(&sources, &NavPath::new(dst_dir).unwrap()).is_empty(),
        "a clean destination must not prompt, and must not spawn a dry-run"
    );
}

/// Keep-both on a directory produces a numbered sibling directory, not a
/// numbered file — the suffix goes on the whole name when there is no
/// extension to preserve.
#[test]
fn unique_numbered_path_renames_directories_whole() {
    let tmp = tempdir();
    let dir = tmp.join("photos");
    fs::create_dir(&dir).unwrap();
    assert_eq!(unique_numbered_path(&dir), tmp.join("photos (1)"));
}

// --- Spoken summary ------------------------------------------------------

/// The summary names the mode so a screen-reader user can tell a no-op
/// "add new only" paste from a paste that genuinely did nothing.
#[test]
fn paste_summary_names_the_mode() {
    use navigator_core::ConflictMode;
    use navigator_gui::preflight::paste_summary;

    assert_eq!(
        paste_summary(Some(ConflictMode::AddNewOnly), 8, 0, 0, 0),
        "done — 8 items, add new only"
    );
    assert_eq!(
        paste_summary(Some(ConflictMode::Update), 3, 0, 0, 0),
        "done — 3 items, update"
    );
    assert_eq!(
        paste_summary(Some(ConflictMode::Mirror), 2, 0, 0, 0),
        "done — 2 items, mirror"
    );
}

#[test]
fn paste_summary_reports_keep_both_and_renames() {
    use navigator_gui::preflight::paste_summary;
    assert_eq!(
        paste_summary(None, 4, 0, 0, 2),
        "done — 4 items, keep both, 2 renamed"
    );
}

#[test]
fn paste_summary_leads_with_failures() {
    use navigator_core::ConflictMode;
    use navigator_gui::preflight::paste_summary;
    let s = paste_summary(Some(ConflictMode::Update), 5, 2, 0, 0);
    assert!(s.starts_with("finished with 2 failures"), "got: {}", s);
    assert!(s.contains("out of 5"));
}

#[test]
fn paste_summary_discounts_skipped_from_the_total() {
    use navigator_core::ConflictMode;
    use navigator_gui::preflight::paste_summary;
    assert_eq!(
        paste_summary(Some(ConflictMode::Update), 5, 0, 2, 0),
        "done — 3 items, update, 2 skipped"
    );
}

// --- Undo safety invariant ------------------------------------------------

/// The rule that makes undo safe: a paste's undo record may only name
/// destinations that did not exist beforehand.
///
/// Every conflict mode can decline to write an existing destination —
/// `AddNewOnly` skips it, `Update` spares it when newer, `Replace`
/// overwrites it with no backup, `KeepBoth` writes a numbered sibling — so
/// a precomputed `dest.join(name)` list made Ctrl+Z delete exactly the
/// files the chosen mode had protected. This mirrors the filter in
/// `op_paste`; if that filter is ever dropped, this fails.
#[test]
fn undo_targets_exclude_preexisting_destinations() {
    use navigator_core::NavPath;

    let tmp = tempdir();
    let src_dir = tmp.join("src");
    let dst_dir = tmp.join("dst");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();

    touch(&src_dir.join("fresh.txt"));
    touch(&src_dir.join("collides.txt"));
    touch(&dst_dir.join("collides.txt"));

    let sources = [
        NavPath::new(src_dir.join("fresh.txt")).unwrap(),
        NavPath::new(src_dir.join("collides.txt")).unwrap(),
    ];
    let dest = NavPath::new(dst_dir).unwrap();

    let (created, originals): (Vec<NavPath>, Vec<NavPath>) = sources
        .iter()
        .map(|s| (dest.join(s.file_name()), s.clone()))
        .filter(|(d, _)| !d.as_path().exists())
        .unzip();

    assert_eq!(
        created.len(),
        1,
        "only the brand-new destination is undoable"
    );
    assert_eq!(created[0].file_name(), "fresh.txt");
    assert_eq!(
        originals.len(),
        created.len(),
        "pairs must stay aligned — run_revert_paste indexes them in lockstep"
    );
    assert_eq!(originals[0].file_name(), "fresh.txt");
    assert!(
        !created.iter().any(|c| c.file_name() == "collides.txt"),
        "undo must never delete a destination that predates the paste"
    );
}

// --- Remote destinations -------------------------------------------------

/// A remote `NavPath` is a synthetic `\\?\NavigatorRemote\…` string, so
/// `Path::exists()` is always false for it. Using it to pre-filter meant a
/// remote paste produced no candidates, ran no dry-run, and therefore never
/// warned before overwriting. `conflict_candidates` must hand everything to
/// the (backend-agnostic) dry-run instead.
#[test]
fn remote_destinations_yield_all_candidates() {
    use navigator_core::NavPath;
    use navigator_gui::preflight::{conflict_candidates, top_level_conflicts};

    let sources = vec![
        NavPath::new(r"C:\src\a.txt").unwrap(),
        NavPath::new(r"C:\src\b.txt").unwrap(),
    ];
    let remote = NavPath::new("mac:Downloads").unwrap();
    assert!(remote.is_remote(), "fixture must actually be a remote path");

    assert!(
        top_level_conflicts(&sources, &remote).is_empty(),
        "exists() cannot see a remote — this is why it must not gate detection"
    );
    assert_eq!(
        conflict_candidates(&sources, &remote).len(),
        2,
        "a remote destination must send every source to the dry-run"
    );
}

/// For a local destination the cheap `exists()` pre-filter is kept, so a
/// clean paste still skips the dry-run entirely.
#[test]
fn local_destinations_keep_the_cheap_prefilter() {
    use navigator_core::NavPath;
    use navigator_gui::preflight::conflict_candidates;

    let tmp = tempdir();
    let src_dir = tmp.join("src");
    let dst_dir = tmp.join("dst");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();
    touch(&src_dir.join("a.txt"));

    let sources = vec![NavPath::new(src_dir.join("a.txt")).unwrap()];
    let dest = NavPath::new(dst_dir).unwrap();
    assert!(
        conflict_candidates(&sources, &dest).is_empty(),
        "no collision means no dry-run and no dialog"
    );
}
