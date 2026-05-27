//! Accessible modal dialogs built on top of Win32 primitives.
//!
//! Everything in here must be screen-reader friendly. For simple
//! confirmations we use `MessageBoxW` — standard OS dialog, announced by
//! Narrator/NVDA/JAWS exactly the same way File Explorer's prompts are.

use std::iter::once;

use windows::Win32::UI::WindowsAndMessaging::{
    IDCANCEL, IDNO, IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNOCANCEL, MessageBoxW,
};
use windows::core::PCWSTR;

use navigator_core::NavPath;

use crate::window::HwndSend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    Cancel,
}

/// Ask the user how to handle existing destinations.
///
/// Returns `Some(choice)` on a real decision, `None` if the user closes the
/// dialog some other way (e.g. Escape). The message body lists the conflicts
/// (truncated) so the screen reader can read them back.
pub fn ask_overwrite(parent: Option<HwndSend>, conflicts: &[NavPath]) -> Option<ConflictChoice> {
    let list = preview_list(conflicts, 10);
    let msg = format!(
        "{} destination file(s) already exist:\n\n{}\n\n\
         Yes = overwrite all\n\
         No = skip all\n\
         Cancel = cancel the operation",
        conflicts.len(),
        list,
    );
    let rc = msgbox(
        parent,
        "Files already exist",
        &msg,
        MB_YESNOCANCEL | MB_ICONWARNING,
    );
    match rc {
        x if x == IDYES.0 => Some(ConflictChoice::Overwrite),
        x if x == IDNO.0 => Some(ConflictChoice::Skip),
        x if x == IDCANCEL.0 => Some(ConflictChoice::Cancel),
        _ => None,
    }
}

/// Surface an error with OK-only confirmation. Always shown regardless of
/// the "progress window" preference — failures should never go silent.
pub fn show_error(parent: Option<HwndSend>, title: &str, body: &str) {
    msgbox(parent, title, body, MB_OK | MB_ICONWARNING);
}

/// Plain informational OK dialog.
pub fn show_info(parent: Option<HwndSend>, title: &str, body: &str) {
    msgbox(parent, title, body, MB_OK | MB_ICONINFORMATION);
}

fn preview_list(paths: &[NavPath], max: usize) -> String {
    let mut out = String::new();
    for (i, p) in paths.iter().take(max).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&p.to_string());
    }
    if paths.len() > max {
        out.push_str(&format!("\n… and {} more", paths.len() - max));
    }
    out
}

fn msgbox(
    parent: Option<HwndSend>,
    title: &str,
    body: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> i32 {
    let title_w: Vec<u16> = title.encode_utf16().chain(once(0)).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain(once(0)).collect();
    let parent_hwnd = parent.map(|h| h.0);
    unsafe {
        MessageBoxW(
            parent_hwnd,
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            style,
        )
        .0
    }
}
