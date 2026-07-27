//! Conflict dialogs for paste, built on the modern TaskDialog API.
//!
//! Navigator does not ask Explorer's per-item question. rclone already
//! knows how to compare two trees, so the question is a *merge* question
//! asked once per batch:
//!
//! * **Ctrl+V** runs the configured [`ConflictMode`] and only stops to ask
//!   when a `--dry-run` pass proves something at the destination would
//!   actually be destroyed — see [`prompt_conflicts`]. A purely additive
//!   paste never shows a dialog.
//! * **Ctrl+Shift+V** (Paste special) always asks, offering every mode as a
//!   radio group — see [`prompt_mode`]. This is the only route to
//!   [`ConflictMode::Mirror`], which deletes destination files the user
//!   never selected.
//!
//! Requires ComCtl32 v6; the binary's manifest declares the dependency so
//! `TaskDialogIndirect` is always available at runtime.

use std::iter::once;

use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOGCONFIG_1,
    TDCBF_CANCEL_BUTTON, TDF_POSITION_RELATIVE_TO_WINDOW, TDF_USE_COMMAND_LINKS,
    TaskDialogIndirect,
};
use windows::core::PCWSTR;

use navigator_core::{ConflictMode, NavPath};

use crate::window::HwndSend;

/// How the user resolved a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteChoice {
    /// Run the paste in this mode.
    Mode(ConflictMode),
    /// Write every conflicting top-level item to a fresh numbered name so
    /// nothing is replaced. See [`unique_numbered_path`].
    ///
    /// Deliberately coarse: it renames the *selected* item, so pasting a
    /// `photos` folder onto an existing one produces `photos (1)` rather
    /// than scattering numbered files through a merged tree. Numbering
    /// inner files would be near-impossible to undo or reason about.
    KeepBoth,
    Cancel,
}

const ID_PROCEED: i32 = 1001;
const ID_ADD_NEW_ONLY: i32 = 1002;
const ID_KEEP_BOTH: i32 = 1003;

const ID_RADIO_BASE: i32 = 2001;
/// Radio id for the Keep-both row, which is not a [`ConflictMode`] and so
/// sits past the end of the mode block.
const ID_RADIO_KEEP_BOTH: i32 = ID_RADIO_BASE + ConflictMode::ALL.len() as i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

/// Max destination paths listed in a dialog body before we switch to a
/// bare count. Long enough to recognise what is at stake, short enough
/// that a screen reader is not stuck reading for a minute.
const MAX_LISTED: usize = 10;

/// Render a path list for a dialog body, truncating past [`MAX_LISTED`].
/// Pure so the truncation wording is testable without a window.
pub fn summarize_paths(paths: &[String]) -> String {
    let mut out = String::new();
    for p in paths.iter().take(MAX_LISTED) {
        out.push_str("    ");
        out.push_str(p);
        out.push('\n');
    }
    if paths.len() > MAX_LISTED {
        out.push_str(&format!("    … and {} more\n", paths.len() - MAX_LISTED));
    }
    out
}

/// The selected sources whose destination already exists — the items a
/// [`PasteChoice::KeepBoth`] would rename.
///
/// Conflicts reported by rclone are destination-relative paths that may
/// sit deep inside a merging directory, but Keep-both only ever acts on
/// the top-level selection, so it needs this list rather than the report.
///
/// Note a non-empty rclone conflict report always implies a non-empty
/// result here: nothing can exist inside a destination directory that does
/// not itself exist.
pub fn top_level_conflicts(sources: &[NavPath], dest_dir: &NavPath) -> Vec<NavPath> {
    sources
        .iter()
        .filter(|s| dest_dir.join(s.file_name()).as_path().exists())
        .cloned()
        .collect()
}

/// Sources worth handing to [`navigator_rclone::RcloneDriver::conflicts`].
///
/// For a local destination this is just [`top_level_conflicts`] — probing
/// `exists()` is free and skips the dry-run entirely for a clean paste.
///
/// For a **remote** destination it is every source. A remote `NavPath` is a
/// synthetic `\\?\NavigatorRemote\…` string, so `Path::exists()` is always
/// false for it; using it to pre-filter meant no candidates, hence no
/// dry-run, hence no conflict dialog *ever* when pasting onto a remote — the
/// two-pass diff is deliberately backend-agnostic but was unreachable.
/// Returning everything lets the dry-run do the real work, which is the only
/// way to answer the question for a backend we cannot stat cheaply.
pub fn conflict_candidates(sources: &[NavPath], dest_dir: &NavPath) -> Vec<NavPath> {
    if dest_dir.is_remote() {
        return sources.to_vec();
    }
    top_level_conflicts(sources, dest_dir)
}

/// Confirm a paste that would destroy something. Called only when the
/// dry-run diff found real victims, so there is always something to show.
///
/// `overwrites` and `deletes` are the destination paths at risk, already
/// stringified by the caller (which knows how to render remote paths).
pub fn prompt_conflicts(
    parent: Option<HwndSend>,
    mode: ConflictMode,
    overwrites: &[String],
    deletes: &[String],
    keep_both_offered: bool,
) -> PasteChoice {
    let title_w = wide("Paste");
    let heading = match (overwrites.len(), deletes.len()) {
        (n, 0) => format!("{} will replace {} existing item(s).", mode.label(), n),
        (0, d) => format!(
            "{} will delete {} item(s) at the destination.",
            mode.label(),
            d
        ),
        (n, d) => format!(
            "{} will replace {} item(s) and delete {} item(s) at the destination.",
            mode.label(),
            n,
            d
        ),
    };
    let heading_w = wide(&heading);

    let mut body = String::new();
    if !overwrites.is_empty() {
        body.push_str("These will be replaced:\n");
        body.push_str(&summarize_paths(overwrites));
    }
    if !deletes.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("These exist only at the destination and will be deleted:\n");
        body.push_str(&summarize_paths(deletes));
    }
    body.push_str("\nReplaced and deleted items are not recoverable.");
    let body_w = wide(&body);

    let proceed_w = wide(&format!(
        "{}\nReplace {} item(s)",
        mode.label(),
        overwrites.len() + deletes.len()
    ));
    let add_new_w = wide("Add new only\nLeave everything that already exists untouched");
    let keep_both_w = wide("Keep both\nGive each conflicting item a new numbered name");

    let mut buttons = vec![
        TASKDIALOG_BUTTON {
            nButtonID: ID_PROCEED,
            pszButtonText: PCWSTR(proceed_w.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ID_ADD_NEW_ONLY,
            pszButtonText: PCWSTR(add_new_w.as_ptr()),
        },
    ];
    if keep_both_offered {
        buttons.push(TASKDIALOG_BUTTON {
            nButtonID: ID_KEEP_BOTH,
            pszButtonText: PCWSTR(keep_both_w.as_ptr()),
        });
    }

    // Command links give each choice a title + explanation line, which is
    // what a screen reader reads out — far clearer than three bare words.
    let flags = TASKDIALOG_FLAGS(0) | TDF_POSITION_RELATIVE_TO_WINDOW | TDF_USE_COMMAND_LINKS;

    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent.map(|h| h.0).unwrap_or_default(),
        hInstance: Default::default(),
        dwFlags: flags,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title_w.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0::default(),
        pszMainInstruction: PCWSTR(heading_w.as_ptr()),
        pszContent: PCWSTR(body_w.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        // Default to the *safe* choice. The destructive path should cost a
        // deliberate keystroke, not an absent-minded Enter.
        nDefaultButton: ID_ADD_NEW_ONLY,
        cRadioButtons: 0,
        pRadioButtons: std::ptr::null(),
        nDefaultRadioButton: 0,
        pszVerificationText: PCWSTR::null(),
        pszExpandedInformation: PCWSTR::null(),
        pszExpandedControlText: PCWSTR::null(),
        pszCollapsedControlText: PCWSTR::null(),
        Anonymous2: TASKDIALOGCONFIG_1::default(),
        pszFooter: PCWSTR::null(),
        pfCallback: None,
        lpCallbackData: 0,
        cxWidth: 0,
    };

    let mut button = 0i32;
    let rc = unsafe { TaskDialogIndirect(&config, Some(&mut button), None, None) };
    if rc.is_err() {
        // API failure — treat as Cancel so a broken dialog can never
        // silently green-light a destructive paste.
        return PasteChoice::Cancel;
    }
    match button {
        ID_PROCEED => PasteChoice::Mode(mode),
        ID_ADD_NEW_ONLY => PasteChoice::Mode(ConflictMode::AddNewOnly),
        ID_KEEP_BOTH => PasteChoice::KeepBoth,
        _ => PasteChoice::Cancel,
    }
}

/// Paste special: pick the mode up front. `allow_mirror` is false for a cut
/// clipboard, where Mirror has no meaning (rclone has no verb that both
/// prunes the destination and empties the source).
/// `allow_keep_both` is false for a remote destination, where
/// [`unique_numbered_path`] cannot probe for a free name.
pub fn prompt_mode(
    parent: Option<HwndSend>,
    default: ConflictMode,
    allow_mirror: bool,
    allow_keep_both: bool,
) -> PasteChoice {
    let title_w = wide("Paste special");
    let heading_w = wide("How should existing items at the destination be handled?");
    let body_w = wide(
        "This choice applies to the whole paste. Items that do not exist at \
         the destination are always copied.",
    );

    let offered: Vec<ConflictMode> = ConflictMode::ALL
        .into_iter()
        .filter(|m| allow_mirror || !m.deletes_unselected())
        .collect();

    // `nDefaultRadioButton` must name a radio that actually exists. A
    // config hand-set to `on_conflict = "mirror"` plus a cut clipboard
    // filters Mirror out of `offered` while still pointing the default at
    // its id; TaskDialog then selects nothing, returns radio 0, and the
    // whole dialog reads as Cancel — Ctrl+Shift+V would silently refuse
    // every cut paste. Fall back to the first offered mode.
    let default = if offered.contains(&default) {
        default
    } else {
        offered.first().copied().unwrap_or(ConflictMode::Update)
    };

    // Keep the label strings alive for the duration of the call — the
    // TASKDIALOG_BUTTON array holds raw pointers into them.
    let labels: Vec<Vec<u16>> = offered
        .iter()
        .map(|m| wide(&format!("{} — {}", m.label(), m.description())))
        .collect();
    let keep_both_label =
        wide("Keep both — give each conflicting item a new numbered name, replacing nothing");

    let mut radios: Vec<TASKDIALOG_BUTTON> = offered
        .iter()
        .zip(labels.iter())
        .map(|(m, l)| TASKDIALOG_BUTTON {
            nButtonID: ID_RADIO_BASE + mode_index(*m),
            pszButtonText: PCWSTR(l.as_ptr()),
        })
        .collect();
    if allow_keep_both {
        radios.push(TASKDIALOG_BUTTON {
            nButtonID: ID_RADIO_KEEP_BOTH,
            pszButtonText: PCWSTR(keep_both_label.as_ptr()),
        });
    }

    let flags = TASKDIALOG_FLAGS(0) | TDF_POSITION_RELATIVE_TO_WINDOW;

    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent.map(|h| h.0).unwrap_or_default(),
        hInstance: Default::default(),
        dwFlags: flags,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title_w.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0::default(),
        pszMainInstruction: PCWSTR(heading_w.as_ptr()),
        pszContent: PCWSTR(body_w.as_ptr()),
        cButtons: 0,
        pButtons: std::ptr::null(),
        nDefaultButton: 0,
        cRadioButtons: radios.len() as u32,
        pRadioButtons: radios.as_ptr(),
        nDefaultRadioButton: ID_RADIO_BASE + mode_index(default),
        pszVerificationText: PCWSTR::null(),
        pszExpandedInformation: PCWSTR::null(),
        pszExpandedControlText: PCWSTR::null(),
        pszCollapsedControlText: PCWSTR::null(),
        Anonymous2: TASKDIALOGCONFIG_1::default(),
        pszFooter: PCWSTR::null(),
        pfCallback: None,
        lpCallbackData: 0,
        cxWidth: 0,
    };

    let mut button = 0i32;
    let mut radio = 0i32;
    let rc = unsafe { TaskDialogIndirect(&config, Some(&mut button), Some(&mut radio), None) };
    if rc.is_err() {
        return PasteChoice::Cancel;
    }
    // Anything but OK (Cancel, Esc, close) abandons the paste.
    if button != windows::Win32::UI::WindowsAndMessaging::IDOK.0 {
        return PasteChoice::Cancel;
    }
    choice_from_radio(radio)
}

/// Stable index of a mode within [`ConflictMode::ALL`], used to derive
/// radio ids. Kept as a function so the ids survive reordering the enum.
fn mode_index(m: ConflictMode) -> i32 {
    ConflictMode::ALL.iter().position(|x| *x == m).unwrap_or(0) as i32
}

/// Map a radio id back to a choice. Split out from the dialog so the id
/// arithmetic is testable without a window.
pub fn choice_from_radio(radio: i32) -> PasteChoice {
    if radio == ID_RADIO_KEEP_BOTH {
        return PasteChoice::KeepBoth;
    }
    let idx = radio - ID_RADIO_BASE;
    match ConflictMode::ALL.get(idx as usize) {
        Some(m) => PasteChoice::Mode(*m),
        None => PasteChoice::Cancel,
    }
}

/// The spoken summary for a finished paste.
///
/// `mode` is `None` for [`PasteChoice::KeepBoth`]. Naming the mode matters
/// for a screen-reader user: "8 items, add new only" explains why a paste
/// that looked like it should have changed something didn't.
pub fn paste_summary(
    mode: Option<ConflictMode>,
    total: usize,
    failed: u32,
    skipped: u32,
    renamed: u32,
) -> String {
    let how = match mode {
        Some(m) => m.label().to_lowercase(),
        None => "keep both".to_string(),
    };
    if failed > 0 {
        return format!(
            "finished with {} failures out of {} — {}",
            failed, total, how
        );
    }
    let mut msg = format!("done — {} items, {}", total - skipped as usize, how);
    if renamed > 0 {
        msg.push_str(&format!(", {} renamed", renamed));
    }
    if skipped > 0 {
        msg.push_str(&format!(", {} skipped", skipped));
    }
    msg
}

/// Given a destination path that may or may not already exist, produce a
/// non-existing sibling by appending a " (N)" suffix to the stem. If the
/// input path is already free, it is returned unchanged. Caps the search
/// at 9,999 attempts — beyond that it returns the input and lets the
/// operation fail with the usual collision behaviour, rather than looping
/// forever on a pathologically full directory.
///
/// Examples:
/// * `foo.txt` (taken) → `foo (1).txt`
/// * `foo.txt` (taken, plus `foo (1).txt` taken) → `foo (2).txt`
/// * `README` (no extension, taken) → `README (1)`
/// * `archive.tar.gz` (taken) → `archive.tar (1).gz` (Explorer parity —
///   only the last extension segment is preserved)
/// * `photos` (a directory, taken) → `photos (1)` — Keep-both renames whole
///   directories rather than numbering files inside them.
pub fn unique_numbered_path(dst: &std::path::Path) -> std::path::PathBuf {
    if !dst.exists() {
        return dst.to_path_buf();
    }
    let parent = dst.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = dst.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = dst.extension().and_then(|s| s.to_str());
    for n in 1..10_000 {
        let name = match ext {
            Some(e) if !e.is_empty() => format!("{} ({}).{}", stem, n, e),
            _ => format!("{} ({})", stem, n),
        };
        let candidate = parent.join(&name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dst.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Radio ids are derived from position in `ConflictMode::ALL`, so the
    /// round-trip must hold for every mode plus the Keep-both row.
    #[test]
    fn radio_ids_round_trip_to_choices() {
        for m in ConflictMode::ALL {
            let id = ID_RADIO_BASE + mode_index(m);
            assert_eq!(choice_from_radio(id), PasteChoice::Mode(m));
        }
        assert_eq!(choice_from_radio(ID_RADIO_KEEP_BOTH), PasteChoice::KeepBoth);
    }

    /// The Keep-both row must not collide with any mode id, or picking it
    /// would silently run a mode instead.
    #[test]
    fn keep_both_id_does_not_collide_with_modes() {
        for m in ConflictMode::ALL {
            assert_ne!(ID_RADIO_BASE + mode_index(m), ID_RADIO_KEEP_BOTH);
        }
    }

    /// An id from outside the table must abandon the paste rather than
    /// defaulting to a destructive mode.
    #[test]
    fn unknown_radio_id_cancels() {
        assert_eq!(choice_from_radio(0), PasteChoice::Cancel);
        assert_eq!(choice_from_radio(99_999), PasteChoice::Cancel);
    }

    /// Mirrors the clamp in `prompt_mode`. `nDefaultRadioButton` must name a
    /// radio that exists: a config hand-set to `on_conflict = "mirror"` plus
    /// a cut clipboard filters Mirror out of the offered list, and pointing
    /// the default at its absent id makes TaskDialog select nothing and
    /// return radio 0 — which `choice_from_radio` reads as Cancel, so
    /// Ctrl+Shift+V would silently refuse every cut paste.
    #[test]
    fn default_mode_is_clamped_to_the_offered_set() {
        let offered: Vec<ConflictMode> = ConflictMode::ALL
            .into_iter()
            .filter(|m| !m.deletes_unselected())
            .collect();
        assert!(
            !offered.contains(&ConflictMode::Mirror),
            "a cut clipboard must not offer Mirror"
        );

        let clamped = if offered.contains(&ConflictMode::Mirror) {
            ConflictMode::Mirror
        } else {
            offered.first().copied().unwrap_or(ConflictMode::Update)
        };
        assert_ne!(clamped, ConflictMode::Mirror);
        assert_ne!(
            choice_from_radio(ID_RADIO_BASE + mode_index(clamped)),
            PasteChoice::Cancel,
            "the clamped default must map to a real, selectable choice"
        );
    }

    #[test]
    fn short_path_lists_are_shown_in_full() {
        let paths: Vec<String> = (0..3).map(|i| format!("f{}.txt", i)).collect();
        let s = summarize_paths(&paths);
        assert!(s.contains("f0.txt") && s.contains("f2.txt"));
        assert!(!s.contains("more"));
    }

    /// Long lists truncate so a screen reader is not trapped reading
    /// hundreds of filenames, but the total is still conveyed.
    #[test]
    fn long_path_lists_are_truncated_with_a_count() {
        let paths: Vec<String> = (0..25).map(|i| format!("f{}.txt", i)).collect();
        let s = summarize_paths(&paths);
        assert!(s.contains("f0.txt"));
        assert!(!s.contains("f20.txt"));
        assert!(
            s.contains("and 15 more"),
            "expected remainder count, got: {}",
            s
        );
    }

    #[test]
    fn exactly_max_listed_does_not_say_more() {
        let paths: Vec<String> = (0..MAX_LISTED).map(|i| format!("f{}.txt", i)).collect();
        assert!(!summarize_paths(&paths).contains("more"));
    }
}
