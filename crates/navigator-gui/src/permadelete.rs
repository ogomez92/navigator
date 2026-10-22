//! Permanent delete (Ctrl+Shift+Delete): the pure half.
//!
//! The dialog that guards a permanent delete has to name what is about to
//! go and how big it is, and that body is built here, away from the HWND
//! and the worker that walked the folders, so its wording and truncation
//! are unit-tested. The impure driver is `AppState::op_permanent_delete`
//! (survey on a worker) → `confirm_permanent_delete_survey` (dialog on the
//! UI thread) → `WorkerCtx::run_permanent_delete`.

use crate::listview::format_size;

/// One surveyed item, ready to be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurveyedItem {
    pub name: String,
    pub is_dir: bool,
    /// Recursive byte total for a folder, the file's own size otherwise.
    /// `None` when the size could not be determined (a remote folder
    /// rclone could not size, an unreadable local one).
    pub bytes: Option<u64>,
    /// Files found under a folder. Meaningless (and `None`) for a file.
    pub files_inside: Option<u64>,
}

/// Items listed by name before the list is cut off with "… and N more".
/// A `MessageBoxW` has no scrollbar, so a Ctrl+A over a big folder must
/// not produce a dialog taller than the screen.
pub const MAX_LISTED: usize = 15;

/// The confirmation body: a warning, one line per item with its size, and
/// the total. Every item counts toward the total even when the list is
/// truncated, so the headline number is always the whole selection.
pub fn confirm_body(items: &[SurveyedItem]) -> String {
    let n = items.len();
    let mut body = format!(
        "Permanently delete {} {}?\n\
         This skips .trash — there is no undo.\n\n",
        n,
        if n == 1 { "item" } else { "items" },
    );
    let mut total: u64 = 0;
    let mut unknown = 0usize;
    for (i, it) in items.iter().enumerate() {
        match it.bytes {
            Some(b) => total = total.saturating_add(b),
            None => unknown += 1,
        }
        if i >= MAX_LISTED {
            continue;
        }
        body.push_str("• ");
        body.push_str(&it.name);
        body.push_str(" — ");
        match it.bytes {
            Some(b) => body.push_str(&format_size(b)),
            None => body.push_str("size unknown"),
        }
        if it.is_dir {
            match it.files_inside {
                Some(f) => body.push_str(&format!(
                    " (folder, {} {})",
                    f,
                    if f == 1 { "file" } else { "files" }
                )),
                None => body.push_str(" (folder)"),
            }
        }
        body.push('\n');
    }
    if n > MAX_LISTED {
        body.push_str(&format!("… and {} more\n", n - MAX_LISTED));
    }
    body.push_str(&format!("\nTotal: {}", format_size(total)));
    if unknown > 0 {
        body.push_str(&format!(
            " (plus {} {} of unknown size)",
            unknown,
            if unknown == 1 { "item" } else { "items" }
        ));
    }
    body
}

/// The sentence spoken once the delete has run.
pub fn summary(ok: usize, failed: usize) -> String {
    match failed {
        0 => format!(
            "permanently deleted {} {}",
            ok,
            if ok == 1 { "item" } else { "items" }
        ),
        f => format!("permanently deleted {}, {} failed", ok, f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, bytes: u64) -> SurveyedItem {
        SurveyedItem {
            name: name.into(),
            is_dir: false,
            bytes: Some(bytes),
            files_inside: None,
        }
    }

    #[test]
    fn body_names_every_item_with_its_size_and_the_total() {
        let items = [
            file("a.txt", 1024),
            SurveyedItem {
                name: "photos".into(),
                is_dir: true,
                bytes: Some(3 * 1024 * 1024),
                files_inside: Some(12),
            },
        ];
        let body = confirm_body(&items);
        assert!(body.starts_with("Permanently delete 2 items?"), "{body}");
        assert!(body.contains("• a.txt — 1.0 KB\n"), "{body}");
        assert!(
            body.contains("• photos — 3.0 MB (folder, 12 files)\n"),
            "{body}"
        );
        assert!(body.contains("Total: 3.0 MB"), "{body}");
        assert!(!body.contains("unknown"), "{body}");
    }

    #[test]
    fn a_single_item_is_singular() {
        let body = confirm_body(&[file("x", 5)]);
        assert!(body.starts_with("Permanently delete 1 item?"), "{body}");
        assert!(body.contains("Total: 5 B"), "{body}");
    }

    #[test]
    fn the_list_is_truncated_but_the_total_is_not() {
        let items: Vec<SurveyedItem> = (0..40).map(|i| file(&format!("f{i}"), 100)).collect();
        let body = confirm_body(&items);
        assert!(body.contains("• f14 — "), "{body}");
        assert!(!body.contains("• f15 — "), "{body}");
        assert!(body.contains("… and 25 more\n"), "{body}");
        assert!(body.contains("Total: 3.9 KB"), "{body}");
    }

    #[test]
    fn unknown_sizes_are_called_out_instead_of_counted_as_zero() {
        let items = [
            file("a", 10),
            SurveyedItem {
                name: "remote_dir".into(),
                is_dir: true,
                bytes: None,
                files_inside: None,
            },
        ];
        let body = confirm_body(&items);
        assert!(
            body.contains("• remote_dir — size unknown (folder)\n"),
            "{body}"
        );
        assert!(
            body.contains("Total: 10 B (plus 1 item of unknown size)"),
            "{body}"
        );
    }

    #[test]
    fn summary_reports_failures() {
        assert_eq!(summary(1, 0), "permanently deleted 1 item");
        assert_eq!(summary(3, 0), "permanently deleted 3 items");
        assert_eq!(summary(2, 1), "permanently deleted 2, 1 failed");
    }
}
