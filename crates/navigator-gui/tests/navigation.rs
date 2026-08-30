//! Which folder counts as "where the user is" — the rule a finishing
//! background operation must consult before touching the listing.

#![cfg(windows)]

use navigator_core::NavPath;
use navigator_gui::app::{effective_folder, focus_target_belongs};

fn p(s: &str) -> NavPath {
    NavPath::new(s).unwrap()
}

/// With nothing in flight, the folder on screen is the answer.
#[test]
fn settled_view_answers_with_the_current_folder() {
    let cwd = p(r"C:\work");
    assert_eq!(effective_folder(None, Some(&cwd)), Some(&cwd));
    assert_eq!(effective_folder(None, None), None);
}

/// A navigation the user has already asked for wins over the listing still
/// on screen. This is the whole point: a scan is queued, so for the gap
/// between the keypress and the listing arriving, `cwd` still names the
/// folder being *left* — and a worker that trusted it would re-list that
/// folder, landing its listing after the user's and dragging them back.
#[test]
fn an_in_flight_navigation_wins_over_what_is_still_on_screen() {
    let leaving = p(r"C:\work");
    let heading_to = p(r"C:\photos");
    assert_eq!(
        effective_folder(Some(&heading_to), Some(&leaving)),
        Some(&heading_to),
        "the user is already gone as far as they are concerned"
    );
}

/// First navigation of the session: no listing yet, but the intent still
/// counts, so an operation finishing there is not treated as off-screen.
#[test]
fn an_in_flight_navigation_counts_before_the_first_listing() {
    let first = p(r"C:\work");
    assert_eq!(effective_folder(Some(&first), None), Some(&first));
}

/// Re-navigating to the folder you are already in (F5, a filter toggle, a
/// post-operation refresh) leaves the answer unchanged — the refresh must
/// not read as "the user left".
#[test]
fn refreshing_in_place_does_not_change_the_answer() {
    let here = p(r"C:\work");
    let same = p(r"C:\work");
    assert_eq!(effective_folder(Some(&same), Some(&here)), Some(&same));
    assert_eq!(effective_folder(Some(&same), Some(&here)), Some(&here));
}

/// A caret target belongs to the listing of its own parent and no other.
/// An operation arms it before it runs, so the folder it was meant for may
/// well be gone by the time a listing turns up to consume it.
#[test]
fn a_focus_target_belongs_only_to_its_own_folder() {
    let work = p(r"C:\work");
    assert!(focus_target_belongs(&work, &p(r"C:\work\notes.txt")));
    // Somewhere else entirely — a slow delete must not move the caret here.
    assert!(!focus_target_belongs(
        &p(r"C:\photos"),
        &p(r"C:\work\notes.txt")
    ));
    // A grandchild belongs to the sub-folder's listing, not this one.
    assert!(!focus_target_belongs(&work, &p(r"C:\work\sub\deep.txt")));
    // The folder itself is not one of its own rows.
    assert!(!focus_target_belongs(&work, &work));
}

/// Root forms are where the parent arithmetic is easiest to get wrong: a
/// drive root and a UNC share root both carry a trailing separator that a
/// child's `parent()` has to reproduce exactly, or every navigate-up on a
/// share would silently lose its refocus.
#[test]
fn focus_targets_resolve_at_drive_and_share_roots() {
    assert!(focus_target_belongs(&p(r"C:\"), &p(r"C:\pagefile.sys")));
    let share = p(r"\\host\share");
    assert!(
        focus_target_belongs(&share, &share.join("movies")),
        "share root {share} must own its children"
    );
    assert!(focus_target_belongs(
        &p(r"\\host\share\movies"),
        &p(r"\\host\share\movies\a.mkv")
    ));
}

/// This PC lists drives, which have no parent — the one case where the
/// parent rule needs its own branch.
#[test]
fn drive_roots_belong_to_the_this_pc_listing() {
    let this_pc = NavPath::this_pc();
    assert!(focus_target_belongs(&this_pc, &p(r"D:\")));
    // A file is never a row of This PC.
    assert!(!focus_target_belongs(&this_pc, &p(r"D:\notes.txt")));
    // And a drive root is not a row of some other folder's listing.
    assert!(!focus_target_belongs(&p(r"C:\work"), &p(r"D:\")));
}

/// Remote paths take a different `parent()` route (string surgery on the
/// sentinel prefix), so the same rule has to hold across it.
#[test]
fn remote_focus_targets_belong_to_their_remote_folder() {
    let downloads = NavPath::remote("mac", "Downloads");
    assert!(focus_target_belongs(
        &downloads,
        &NavPath::remote("mac", "Downloads/report.pdf")
    ));
    assert!(!focus_target_belongs(
        &NavPath::remote("mac", "Music"),
        &NavPath::remote("mac", "Downloads/report.pdf")
    ));
    // A remote root is a row of the remotes listing.
    assert!(focus_target_belongs(
        &NavPath::remotes_root(),
        &NavPath::remote("mac", "")
    ));
}
