//! User-defined shortcut actions.
//!
//! A shortcut binds a modifier+key chord to a command that launches a program
//! on the currently selected path. Placeholders in `args` are expanded at
//! invocation time:
//!
//!   `{path}`    — full path of the selected entry
//!   `{folder}`  — directory of the selection: self if directory, parent if file
//!   `{parent}`  — parent directory (always)
//!   `{name}`    — file/directory name only
//!
//! A shortcut binds a key chord to one of two things:
//!
//!   * an **internal** built-in UI command (copy, cut, rename, refresh …) —
//!     selected via `InternalCommand`, `command` / `args` are ignored.
//!   * an **external** program launch — `command` + `args` with placeholder
//!     expansion on the selected entry.
//!
//! `default_actions()` seeds the well-known editor bindings so a fresh
//! install already has Ctrl+C / Ctrl+X / Ctrl+V / F2 / F5 / etc. wired up;
//! users can rebind any of them via the shortcut editor.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShortcutChord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Virtual-key name. Accepted forms:
    ///   * single ASCII letter `A`-`Z` or digit `0`-`9` (case-insensitive)
    ///   * `F1` … `F24`
    ///   * arrow + navigation keys: `Up`, `Down`, `Left`, `Right`, `Home`,
    ///     `End`, `PageUp`, `PageDown`, `Insert`, `Delete`, `Tab`,
    ///     `Space`, `Escape`/`Esc`, `Enter`/`Return`, `Backspace`
    /// Kept as a string so the config file stays readable.
    pub key: String,
}

/// Built-in UI commands bindable via the shortcut system. Adding a new
/// variant requires wiring it in `navigator-gui`'s action dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InternalCommand {
    Copy,
    Cut,
    /// Add current selection to the existing copy/cut clipboard set —
    /// useful for building a multi-folder gather before one paste.
    AppendCopy,
    AppendCut,
    Paste,
    CopyPaths,
    Delete,
    Rename,
    SelectAll,
    Refresh,
    ToggleHidden,
    ToggleSystem,
    Search,
    NavigateUp,
    HistBack,
    HistForward,
    /// Reverse the most recent undoable action (clipboard change, paste).
    Undo,
    /// Hotspot slots 1..=10 — jump to the saved entry. `Hotspot{N}` is the
    /// GOTO action (default: Ctrl+{N}, slot 10 = Ctrl+0). If the slot is
    /// empty the chord just announces that fact.
    Hotspot1,
    Hotspot2,
    Hotspot3,
    Hotspot4,
    Hotspot5,
    Hotspot6,
    Hotspot7,
    Hotspot8,
    Hotspot9,
    Hotspot10,
    /// Hotspot slots 1..=10 — SET the slot to the currently selected entry
    /// (overwrites). Default chord: Ctrl+Shift+{N}, slot 10 = Ctrl+Shift+0.
    /// Requires exactly one selected row; otherwise announces an error
    /// through prism and leaves the slot untouched.
    HotspotSet1,
    HotspotSet2,
    HotspotSet3,
    HotspotSet4,
    HotspotSet5,
    HotspotSet6,
    HotspotSet7,
    HotspotSet8,
    HotspotSet9,
    HotspotSet10,
    /// Show a read-only properties window for the focused entry. Files
    /// show metadata (size, dates, attrs, extension info); folders also
    /// recursively compute total size, counts and an extension histogram.
    ShowProperties,
    /// Recursively enumerate the focused folder (or current folder if a
    /// file is focused) and dump the tree as TOML in a copy-friendly
    /// viewer window.
    DumpTree,
    /// Prompt for a name and create a new empty folder in the current
    /// directory. The folder is only created after the user commits the
    /// name — unlike Explorer we don't create `New folder` up front.
    NewFolder,
    /// Move focus to the address bar and select its text. Default
    /// chord is Alt+D — same as Explorer / browsers.
    FocusAddress,
    /// Copy the selected file/folder *handles* to the real Windows
    /// clipboard as `CF_HDROP` (plus a `Preferred DropEffect = COPY`
    /// hint), so pasting in Explorer / other apps reproduces the
    /// files. Distinct from the app's own file-backed clipboard, which
    /// only navigator instances see. Default chord: Alt+C.
    CopyToClipboard,
}

impl InternalCommand {
    /// Returns the 1..=10 slot number if this command is a hotspot GOTO.
    pub fn hotspot_goto_slot(self) -> Option<u8> {
        Some(match self {
            InternalCommand::Hotspot1 => 1,
            InternalCommand::Hotspot2 => 2,
            InternalCommand::Hotspot3 => 3,
            InternalCommand::Hotspot4 => 4,
            InternalCommand::Hotspot5 => 5,
            InternalCommand::Hotspot6 => 6,
            InternalCommand::Hotspot7 => 7,
            InternalCommand::Hotspot8 => 8,
            InternalCommand::Hotspot9 => 9,
            InternalCommand::Hotspot10 => 10,
            _ => return None,
        })
    }

    /// Returns the 1..=10 slot number if this command is a hotspot SET.
    pub fn hotspot_set_slot(self) -> Option<u8> {
        Some(match self {
            InternalCommand::HotspotSet1 => 1,
            InternalCommand::HotspotSet2 => 2,
            InternalCommand::HotspotSet3 => 3,
            InternalCommand::HotspotSet4 => 4,
            InternalCommand::HotspotSet5 => 5,
            InternalCommand::HotspotSet6 => 6,
            InternalCommand::HotspotSet7 => 7,
            InternalCommand::HotspotSet8 => 8,
            InternalCommand::HotspotSet9 => 9,
            InternalCommand::HotspotSet10 => 10,
            _ => return None,
        })
    }
}

/// Number of hotspot slots exposed in the UI. Bound to Ctrl+Shift+1..0 by
/// default (slot 10 = Ctrl+Shift+0).
pub const HOTSPOT_COUNT: u8 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutAction {
    pub name: String,
    #[serde(default)]
    pub chord: ShortcutChord,
    /// When `Some`, invoking this action runs a built-in UI command and
    /// `command`/`args` are ignored. When `None`, the action launches an
    /// external program.
    #[serde(default)]
    pub internal: Option<InternalCommand>,
    /// Program to launch. Resolved against `PATH` if not absolute.
    /// Ignored when `internal` is `Some`.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// If true, the action targets the *first* selected entry only. Default
    /// false means the program is invoked once per selected entry.
    /// Ignored for internal commands.
    #[serde(default)]
    pub single: bool,
}

pub fn default_actions() -> Vec<ShortcutAction> {
    use InternalCommand::*;
    let chord = |ctrl: bool, shift: bool, alt: bool, key: &str| ShortcutChord {
        ctrl, shift, alt, key: key.into(),
    };
    let internal = |name: &str, ic: InternalCommand, c: ShortcutChord| ShortcutAction {
        name: name.into(),
        chord: c,
        internal: Some(ic),
        command: String::new(),
        args: Vec::new(),
        single: false,
    };
    vec![
        internal("Copy",           Copy,         chord(true,  false, false, "C")),
        internal("Cut",            Cut,          chord(true,  false, false, "X")),
        // Append-to-clipboard lives on Ctrl+Alt+C/X so Ctrl+Shift+C can
        // be the Windows-11-style "copy full path(s)" chord users expect.
        internal("Append to copy", AppendCopy,   chord(true,  false, true,  "C")),
        internal("Append to cut",  AppendCut,    chord(true,  false, true,  "X")),
        internal("Paste",          Paste,        chord(true,  false, false, "V")),
        internal("Copy paths",     CopyPaths,    chord(true,  true,  false, "C")),
        internal("Select all",     SelectAll,    chord(true,  false, false, "A")),
        internal("Rename",         Rename,       chord(false, false, false, "F2")),
        internal("Refresh",        Refresh,      chord(false, false, false, "F5")),
        internal("Toggle hidden",  ToggleHidden, chord(true,  false, false, "H")),
        internal("Toggle system",  ToggleSystem, chord(true,  true,  false, "H")),
        internal("Find in folder", Search,       chord(true,  false, false, "F")),
        internal("Navigate up",    NavigateUp,   chord(false, false, true,  "Up")),
        internal("History back",   HistBack,     chord(false, false, true,  "Left")),
        internal("History forward",HistForward,  chord(false, false, true,  "Right")),
        internal("Undo",           Undo,         chord(true,  false, false, "Z")),
        // Hotspot GOTO (Ctrl+N → jump to slot N).
        internal("Hotspot 1",  Hotspot1,  chord(true, false, false, "1")),
        internal("Hotspot 2",  Hotspot2,  chord(true, false, false, "2")),
        internal("Hotspot 3",  Hotspot3,  chord(true, false, false, "3")),
        internal("Hotspot 4",  Hotspot4,  chord(true, false, false, "4")),
        internal("Hotspot 5",  Hotspot5,  chord(true, false, false, "5")),
        internal("Hotspot 6",  Hotspot6,  chord(true, false, false, "6")),
        internal("Hotspot 7",  Hotspot7,  chord(true, false, false, "7")),
        internal("Hotspot 8",  Hotspot8,  chord(true, false, false, "8")),
        internal("Hotspot 9",  Hotspot9,  chord(true, false, false, "9")),
        internal("Hotspot 10", Hotspot10, chord(true, false, false, "0")),
        // Hotspot SET (Ctrl+Shift+N → save current selection to slot N).
        internal("Set hotspot 1",  HotspotSet1,  chord(true, true, false, "1")),
        internal("Set hotspot 2",  HotspotSet2,  chord(true, true, false, "2")),
        internal("Set hotspot 3",  HotspotSet3,  chord(true, true, false, "3")),
        internal("Set hotspot 4",  HotspotSet4,  chord(true, true, false, "4")),
        internal("Set hotspot 5",  HotspotSet5,  chord(true, true, false, "5")),
        internal("Set hotspot 6",  HotspotSet6,  chord(true, true, false, "6")),
        internal("Set hotspot 7",  HotspotSet7,  chord(true, true, false, "7")),
        internal("Set hotspot 8",  HotspotSet8,  chord(true, true, false, "8")),
        internal("Set hotspot 9",  HotspotSet9,  chord(true, true, false, "9")),
        internal("Set hotspot 10", HotspotSet10, chord(true, true, false, "0")),
        // Read-only info viewers. Alt+Enter matches Explorer's Properties
        // chord; Alt+L dumps the tree as TOML in the same viewer shell.
        internal("Show properties", ShowProperties, chord(false, false, true, "Return")),
        internal("Dump tree",       DumpTree,       chord(false, false, true, "L")),
        internal("New folder",      NewFolder,      chord(true,  false, false, "N")),
        internal("Focus address bar", FocusAddress, chord(false, false, true,  "D")),
        // Real Windows clipboard (CF_HDROP) — paste into Explorer etc.
        internal("Copy to OS clipboard", CopyToClipboard, chord(false, false, true, "C")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find the seeded default for a given internal command, if any.
    fn default_chord(ic: InternalCommand) -> Option<ShortcutChord> {
        default_actions()
            .into_iter()
            .find(|a| a.internal == Some(ic))
            .map(|a| a.chord)
    }

    #[test]
    fn show_properties_default_is_alt_enter() {
        let c = default_chord(InternalCommand::ShowProperties).expect("seeded");
        assert!(c.alt && !c.ctrl && !c.shift);
        assert!(
            c.key.eq_ignore_ascii_case("return") || c.key.eq_ignore_ascii_case("enter"),
            "unexpected key: {:?}", c.key,
        );
    }

    #[test]
    fn dump_tree_default_is_alt_l() {
        let c = default_chord(InternalCommand::DumpTree).expect("seeded");
        assert!(c.alt && !c.ctrl && !c.shift);
        assert!(c.key.eq_ignore_ascii_case("l"), "unexpected key: {:?}", c.key);
    }

    #[test]
    fn no_two_defaults_share_a_chord() {
        // Defence against a new action silently shadowing an existing one.
        let actions = default_actions();
        for (i, a) in actions.iter().enumerate() {
            for b in &actions[i + 1..] {
                if a.chord.ctrl  == b.chord.ctrl
                    && a.chord.shift == b.chord.shift
                    && a.chord.alt   == b.chord.alt
                    && a.chord.key.eq_ignore_ascii_case(&b.chord.key)
                {
                    panic!("duplicate default chord {:?} on {:?} and {:?}",
                           a.chord, a.name, b.name);
                }
            }
        }
    }
}
