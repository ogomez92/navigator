//! Event sounds: which `.wav` plays for which application event.
//!
//! Files live in `<exe_dir>/navigator_sounds`, alongside `config.toml` and
//! `plugins/` — same portable-install stance as everything else. Nothing is
//! bundled, so a fresh install is silent until the user drops WAVs in and
//! assigns them from Options → Sounds.
//!
//! The mapping is stored as `event key -> filename` rather than a fixed
//! struct with one field per event: a hand-edited config that names an
//! event we later rename (or one we haven't shipped yet) is ignored
//! instead of failing the whole parse, and adding an event doesn't
//! invalidate existing files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Folder holding the user's `.wav` files, relative to the exe directory.
pub const SOUNDS_DIR_NAME: &str = "navigator_sounds";

/// The application events a sound can be attached to.
///
/// Deliberately coarse: one entry per thing a user would describe as
/// "something happened", not one per internal state transition. Completion
/// events are split by verb (copy / move / delete) because that is the
/// distinction that matters when the sound is the only feedback you get,
/// and failure/cancel are separate from all of them so a botched operation
/// never sounds like a successful one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SoundEvent {
    Startup,
    Navigate,
    NavigateUp,
    Back,
    Forward,
    Copy,
    Cut,
    PasteStart,
    CopyDone,
    MoveDone,
    DeleteDone,
    RenameDone,
    NewItem,
    ExtractDone,
    ZipDone,
    UndoDone,
    SearchDone,
    Cancelled,
    Error,
}

impl SoundEvent {
    /// Every event, in the order the Options page lists them (roughly:
    /// lifecycle, navigation, clipboard, operations, outcomes).
    pub const ALL: [SoundEvent; 19] = [
        SoundEvent::Startup,
        SoundEvent::Navigate,
        SoundEvent::NavigateUp,
        SoundEvent::Back,
        SoundEvent::Forward,
        SoundEvent::Copy,
        SoundEvent::Cut,
        SoundEvent::PasteStart,
        SoundEvent::CopyDone,
        SoundEvent::MoveDone,
        SoundEvent::DeleteDone,
        SoundEvent::RenameDone,
        SoundEvent::NewItem,
        SoundEvent::ExtractDone,
        SoundEvent::ZipDone,
        SoundEvent::UndoDone,
        SoundEvent::SearchDone,
        SoundEvent::Cancelled,
        SoundEvent::Error,
    ];

    /// Stable key used in `config.toml`. Renaming one orphans the user's
    /// existing assignment (it is ignored, not an error) — don't.
    pub fn key(self) -> &'static str {
        match self {
            SoundEvent::Startup => "startup",
            SoundEvent::Navigate => "navigate",
            SoundEvent::NavigateUp => "navigate_up",
            SoundEvent::Back => "back",
            SoundEvent::Forward => "forward",
            SoundEvent::Copy => "copy",
            SoundEvent::Cut => "cut",
            SoundEvent::PasteStart => "paste_start",
            SoundEvent::CopyDone => "copy_done",
            SoundEvent::MoveDone => "move_done",
            SoundEvent::DeleteDone => "delete_done",
            SoundEvent::RenameDone => "rename_done",
            SoundEvent::NewItem => "new_item",
            SoundEvent::ExtractDone => "extract_done",
            SoundEvent::ZipDone => "zip_done",
            SoundEvent::UndoDone => "undo_done",
            SoundEvent::SearchDone => "search_done",
            SoundEvent::Cancelled => "cancelled",
            SoundEvent::Error => "error",
        }
    }

    /// Human-readable name shown in the Options → Sounds event list.
    pub fn label(self) -> &'static str {
        match self {
            SoundEvent::Startup => "Application started",
            SoundEvent::Navigate => "Opened a folder",
            SoundEvent::NavigateUp => "Went up one folder",
            SoundEvent::Back => "Went back in history",
            SoundEvent::Forward => "Went forward in history",
            SoundEvent::Copy => "Copied to clipboard",
            SoundEvent::Cut => "Cut to clipboard",
            SoundEvent::PasteStart => "Paste started",
            SoundEvent::CopyDone => "Copy finished",
            SoundEvent::MoveDone => "Move finished",
            SoundEvent::DeleteDone => "Delete finished",
            SoundEvent::RenameDone => "Rename finished",
            SoundEvent::NewItem => "New file or folder created",
            SoundEvent::ExtractDone => "Extraction finished",
            SoundEvent::ZipDone => "Zip finished",
            SoundEvent::UndoDone => "Undo finished",
            SoundEvent::SearchDone => "Search finished",
            SoundEvent::Cancelled => "Operation cancelled",
            SoundEvent::Error => "Operation failed",
        }
    }
}

/// `[sounds]` config section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Sounds {
    /// Master switch. `false` silences every event without discarding the
    /// per-event assignments, so it round-trips as a mute rather than a
    /// reset. Previews from Options → Sounds ignore it — you have to be
    /// able to audition a file before deciding to turn sounds on.
    pub enabled: bool,
    /// `SoundEvent::key()` → `.wav` filename inside `<exe_dir>/navigator_sounds`.
    /// A missing key or an empty value means "silent"; unknown keys are
    /// preserved on save but never played.
    pub events: BTreeMap<String, String>,
}

impl Default for Sounds {
    fn default() -> Self {
        Self {
            // On, but with no assignments — nothing plays until the user
            // maps a file, so the default is "ready", not "noisy".
            enabled: true,
            events: BTreeMap::new(),
        }
    }
}

impl Sounds {
    /// Filename assigned to `ev`, or `None` when the slot is unset. Empty
    /// strings are treated as unset — TOML has no null, and clearing a slot
    /// in the Options page writes `""` rather than deleting the key.
    pub fn file_for(&self, ev: SoundEvent) -> Option<&str> {
        self.events
            .get(ev.key())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    /// Assign (or with `None`, clear) the sound for `ev`.
    pub fn set(&mut self, ev: SoundEvent, file: Option<&str>) {
        match file.map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => {
                self.events.insert(ev.key().to_string(), f.to_string());
            }
            None => {
                self.events.remove(ev.key());
            }
        }
    }
}

/// Directory `.wav` files are read from: `<exe_dir>/navigator_sounds`.
pub fn sounds_dir() -> PathBuf {
    crate::exe_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(SOUNDS_DIR_NAME)
}

/// Absolute path of `file` inside the sounds folder. `file` is treated as a
/// bare filename: anything carrying a path separator or a `..` component is
/// rejected, so a hand-edited config cannot point the player at an
/// arbitrary location on disk.
pub fn sound_path(file: &str) -> Option<PathBuf> {
    let file = file.trim();
    if file.is_empty() || file.contains(['\\', '/']) || file == ".." {
        return None;
    }
    Some(sounds_dir().join(file))
}

/// Every `.wav` in the sounds folder, filenames only, sorted
/// case-insensitively. Empty when the folder is missing — it is created
/// on demand by Options → Sounds, not at startup.
pub fn list_sounds() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(sounds_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_wav(n))
        .collect();
    out.sort_by_key(|a| a.to_lowercase());
    out
}

/// Case-insensitive `.wav` check. Kept separate (and pure) so the
/// extension rule has one definition and a test can pin it.
pub fn is_wav(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
}
