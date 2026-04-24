//! Per-item preflight dialog built on the modern TaskDialog API.
//!
//! For every destination that already exists we show a dialog with three
//! custom buttons — Overwrite, Skip, Cancel — plus a "Apply to all" check
//! box. When the user ticks it, the returned decision sticks for the rest
//! of the batch.
//!
//! Requires ComCtl32 v6; the binary's manifest declares the dependency so
//! `TaskDialogIndirect` is always available at runtime.

use std::iter::once;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOGCONFIG_1,
    TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TDCBF_CANCEL_BUTTON, TDF_POSITION_RELATIVE_TO_WINDOW,
};

use navigator_core::NavPath;

use crate::window::HwndSend;

/// What the user decided for a single conflicting destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemChoice {
    Overwrite,
    Skip,
    /// Write to a freshly-generated "<stem> (N).<ext>" name instead of
    /// replacing the existing destination. The caller picks the actual
    /// number via [`unique_numbered_path`].
    Rename,
    Cancel,
}

/// The decision state threaded through `run_batch`. Once a choice is
/// marked "sticky", future conflicts reuse it without re-prompting.
#[derive(Debug, Clone, Copy)]
pub struct BatchDecision {
    pub choice: ItemChoice,
    pub sticky: bool,
}

const ID_OVERWRITE: i32 = 1001;
const ID_SKIP: i32      = 1002;
const ID_RENAME: i32    = 1003;

/// Ask the user about a single conflict. Returns a [`BatchDecision`].
pub fn prompt_item(
    parent: Option<HwndSend>,
    src: &NavPath,
    dst: &NavPath,
    remaining: usize,
) -> BatchDecision {
    let title_w: Vec<u16> = "File already exists".encode_utf16().chain(once(0)).collect();
    let heading_w: Vec<u16> = format!(
        "\"{}\" already exists at the destination.",
        dst.file_name()
    ).encode_utf16().chain(once(0)).collect();
    let preview = unique_numbered_path(dst.as_path());
    let body = format!(
        "Source:\n{}\n\nDestination:\n{}\n\n\
         Overwrite replaces the destination with the source.\n\
         Skip leaves the destination untouched and continues with the rest.\n\
         Keep both writes the source to \"{}\" instead.\n\n\
         {} conflict(s) remaining.",
        src, dst, preview.file_name().and_then(|s| s.to_str()).unwrap_or(""), remaining,
    );
    let body_w: Vec<u16> = body.encode_utf16().chain(once(0)).collect();
    let verify_w: Vec<u16> = "Apply to all remaining conflicts"
        .encode_utf16().chain(once(0)).collect();
    let ovw_w: Vec<u16> = "&Overwrite".encode_utf16().chain(once(0)).collect();
    let skip_w: Vec<u16> = "&Skip".encode_utf16().chain(once(0)).collect();
    let keep_w: Vec<u16> = "&Keep both (append number)".encode_utf16().chain(once(0)).collect();

    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: ID_OVERWRITE,
            pszButtonText: PCWSTR(ovw_w.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ID_SKIP,
            pszButtonText: PCWSTR(skip_w.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ID_RENAME,
            pszButtonText: PCWSTR(keep_w.as_ptr()),
        },
    ];

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
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: ID_OVERWRITE,
        cRadioButtons: 0,
        pRadioButtons: std::ptr::null(),
        nDefaultRadioButton: 0,
        pszVerificationText: PCWSTR(verify_w.as_ptr()),
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
    let mut verify = BOOL(0);
    let rc = unsafe {
        TaskDialogIndirect(&config, Some(&mut button), None, Some(&mut verify))
    };
    if rc.is_err() {
        // API failure — treat as Cancel to be safe.
        return BatchDecision { choice: ItemChoice::Cancel, sticky: true };
    }
    let sticky = verify.as_bool();
    let choice = match button {
        ID_OVERWRITE => ItemChoice::Overwrite,
        ID_SKIP      => ItemChoice::Skip,
        ID_RENAME    => ItemChoice::Rename,
        _            => ItemChoice::Cancel,
    };
    BatchDecision { choice, sticky }
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
pub fn unique_numbered_path(dst: &std::path::Path) -> std::path::PathBuf {
    if !dst.exists() { return dst.to_path_buf(); }
    let parent = dst.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = dst
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let ext = dst.extension().and_then(|s| s.to_str());
    for n in 1..10_000 {
        let name = match ext {
            Some(e) if !e.is_empty() => format!("{} ({}).{}", stem, n, e),
            _ => format!("{} ({})", stem, n),
        };
        let candidate = parent.join(&name);
        if !candidate.exists() { return candidate; }
    }
    dst.to_path_buf()
}
