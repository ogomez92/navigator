//! How a paste resolves destinations that already exist.
//!
//! Navigator does not ask the Explorer question ("this file exists —
//! replace it?") per item. rclone already knows how to compare a source
//! and a destination tree, so the question we ask instead is a *merge*
//! question, once per batch, and the answer maps onto rclone flags.
//!
//! Lives in `navigator-core` because both `navigator-config` (persisting
//! the user's default) and `navigator-rclone` (turning it into argv) need
//! it, and neither should depend on the other.

use serde::{Deserialize, Serialize};

/// What to do about destinations that already exist.
///
/// The three non-[`Mirror`](ConflictMode::Mirror) modes are pure flag
/// changes on the normal copy/move verbs. `Mirror` changes the verb to
/// `sync`, which is why it is gated behind Paste special in the UI: it
/// deletes files at the destination that the user never selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictMode {
    /// Never touch anything already present at the destination. Existing
    /// files are left exactly as they are, even if the source differs.
    AddNewOnly,
    /// Transfer only what differs, and never replace a destination that is
    /// newer than the source. The default: it is the mode that does the
    /// obviously-right thing for a merge without being able to lose an
    /// edit made at the destination.
    #[default]
    Update,
    /// Re-transfer every source unconditionally, even files that are
    /// byte-identical at the destination.
    Replace,
    /// Make the destination identical to the source, deleting whatever the
    /// source does not have.
    Mirror,
}

impl ConflictMode {
    /// Every mode, in the order they should appear in a UI.
    pub const ALL: [ConflictMode; 4] = [
        ConflictMode::AddNewOnly,
        ConflictMode::Update,
        ConflictMode::Replace,
        ConflictMode::Mirror,
    ];

    /// rclone flags this mode contributes. `Mirror` adds no flags — it is
    /// expressed by switching the verb to `sync`, and wants rclone's
    /// default size+mtime comparison for the files it does copy.
    pub fn flags(self) -> &'static [&'static str] {
        match self {
            ConflictMode::AddNewOnly => &["--ignore-existing"],
            ConflictMode::Update => &["--update"],
            ConflictMode::Replace => &["--ignore-times"],
            ConflictMode::Mirror => &[],
        }
    }

    /// `true` when this mode can destroy data already at the destination.
    /// Drives whether the UI confirms before running.
    pub fn is_destructive(self) -> bool {
        match self {
            ConflictMode::AddNewOnly => false,
            ConflictMode::Update | ConflictMode::Replace | ConflictMode::Mirror => true,
        }
    }

    /// `true` when this mode deletes destination files that were never
    /// part of the user's selection. Only `Mirror` does.
    pub fn deletes_unselected(self) -> bool {
        matches!(self, ConflictMode::Mirror)
    }

    /// Short label for menus and radio buttons.
    pub fn label(self) -> &'static str {
        match self {
            ConflictMode::AddNewOnly => "Add new only",
            ConflictMode::Update => "Update",
            ConflictMode::Replace => "Replace",
            ConflictMode::Mirror => "Mirror",
        }
    }

    /// One-line explanation, phrased for a screen reader reading a radio
    /// button description out loud.
    pub fn description(self) -> &'static str {
        match self {
            ConflictMode::AddNewOnly => {
                "Copy only items that do not exist at the destination. Nothing existing is touched."
            }
            ConflictMode::Update => {
                "Copy items that differ, but never replace a destination file that is newer."
            }
            ConflictMode::Replace => {
                "Copy everything, replacing destination files even when they are identical."
            }
            ConflictMode::Mirror => {
                "Make the destination match the source exactly, deleting items the source does not have."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Update` is the default because it is the only mode that both makes
    /// progress on a merge and cannot clobber a newer destination edit.
    #[test]
    fn default_is_update() {
        assert_eq!(ConflictMode::default(), ConflictMode::Update);
    }

    /// Flag mapping is the whole contract with rclone; assert it literally
    /// so a rename or typo cannot slip through.
    #[test]
    fn flags_match_rclone_spelling() {
        assert_eq!(ConflictMode::AddNewOnly.flags(), &["--ignore-existing"]);
        assert_eq!(ConflictMode::Update.flags(), &["--update"]);
        assert_eq!(ConflictMode::Replace.flags(), &["--ignore-times"]);
        assert!(ConflictMode::Mirror.flags().is_empty());
    }

    /// Only `AddNewOnly` is safe to run with no confirmation, and only
    /// `Mirror` touches files outside the user's selection. Both facts
    /// drive UI gating, so pin them.
    #[test]
    fn destructiveness_is_classified() {
        assert!(!ConflictMode::AddNewOnly.is_destructive());
        assert!(ConflictMode::Update.is_destructive());
        assert!(ConflictMode::Replace.is_destructive());
        assert!(ConflictMode::Mirror.is_destructive());

        assert!(ConflictMode::Mirror.deletes_unselected());
        for m in [
            ConflictMode::AddNewOnly,
            ConflictMode::Update,
            ConflictMode::Replace,
        ] {
            assert!(!m.deletes_unselected());
        }
    }

    /// TOML persists the mode as a snake_case string; round-trip it so a
    /// serde rename cannot silently break existing config files.
    #[test]
    fn serde_uses_snake_case() {
        let json = serde_json::to_string(&ConflictMode::AddNewOnly).unwrap();
        assert_eq!(json, "\"add_new_only\"");
        let back: ConflictMode = serde_json::from_str("\"mirror\"").unwrap();
        assert_eq!(back, ConflictMode::Mirror);
    }
}
