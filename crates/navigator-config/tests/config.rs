//! Config serialization and loader tests.

use navigator_config::{Config, PasteConflictMode, ShortcutAction, ShortcutChord};

#[test]
fn defaults_bind_common_editor_ops() {
    use navigator_config::InternalCommand;
    let c = Config::default();
    // A fresh install ships with the well-known editor chords wired up.
    let has_copy = c.shortcuts.iter().any(|a| {
        a.internal == Some(InternalCommand::Copy)
            && a.chord.ctrl
            && !a.chord.shift
            && !a.chord.alt
            && a.chord.key.eq_ignore_ascii_case("c")
    });
    let has_paste = c
        .shortcuts
        .iter()
        .any(|a| a.internal == Some(InternalCommand::Paste) && a.chord.ctrl);
    let has_rename = c.shortcuts.iter().any(|a| {
        a.internal == Some(InternalCommand::Rename) && a.chord.key.eq_ignore_ascii_case("f2")
    });
    assert!(has_copy, "default Ctrl+C → Copy missing");
    assert!(has_paste, "default Ctrl+V → Paste missing");
    assert!(has_rename, "default F2 → Rename missing");
}

#[test]
fn defaults_have_flags_off() {
    let c = Config::default();
    assert!(!c.general.show_hidden);
    assert!(!c.general.show_system);
    assert!(!c.rclone.progress_window);
}

/// Progress narration is on out of the box. It used to default to `0`
/// (off) and that was survivable only because a paste narrated its own
/// item loop; batching removed the loop, so a fresh install with `0` here
/// would run a ten-minute copy in total silence.
#[test]
fn progress_announcements_are_on_by_default() {
    assert_eq!(Config::default().general.announce_interval_secs, 5);
}

#[test]
fn default_rclone_transfers_is_eight() {
    // Matches the navigator-rclone DEFAULT_TRANSFERS constant so a fresh
    // install spawns ops with --transfers 8 out of the box.
    let c = Config::default();
    assert_eq!(c.rclone.transfers, 8);
    assert_eq!(c.rclone.transfers_clamped(), 8);
}

#[test]
fn rclone_transfers_clamps_absurd_values() {
    // A hand-edited config with 0 or a huge value must not be handed
    // straight to rclone — clamp to the 1..=64 band before use.
    let mut c = Config::default();
    c.rclone.transfers = 0;
    assert_eq!(c.rclone.transfers_clamped(), 1);
    c.rclone.transfers = 1000;
    assert_eq!(c.rclone.transfers_clamped(), 64);
}

#[test]
fn rclone_section_roundtrips_through_toml() {
    let mut c = Config::default();
    c.rclone.progress_window = true;
    c.rclone.transfers = 12;
    let text = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&text).expect("parse");
    assert!(back.rclone.progress_window);
    assert_eq!(back.rclone.transfers, 12);
}

#[test]
fn rclone_section_is_optional_in_toml() {
    // Configs written before the rclone section existed must still load
    // with sensible defaults rather than erroring out.
    let text = r#"
        [general]
        show_hidden = true
    "#;
    let c: Config = toml::from_str(text).expect("parse");
    assert_eq!(c.rclone.transfers, 8);
    assert!(!c.rclone.progress_window);
}

#[test]
fn toml_roundtrip_preserves_shortcuts() {
    let mut original = Config::default();
    original.shortcuts.push(ShortcutAction {
        name: "Custom".into(),
        chord: ShortcutChord {
            ctrl: true,
            shift: true,
            alt: false,
            key: "F9".into(),
        },
        internal: None,
        command: "my.exe".into(),
        args: vec!["{path}".into(), "--flag".into()],
        single: true,
    });

    let text = toml::to_string_pretty(&original).expect("serialize");
    let reparsed: Config = toml::from_str(&text).expect("parse");

    // Also verify the new internal field round-trips. This was a silent
    // data-loss risk when InternalCommand was added.
    assert!(
        reparsed.shortcuts.iter().any(
            |a| a.name == "Copy" && a.internal == Some(navigator_config::InternalCommand::Copy)
        )
    );

    let found = reparsed
        .shortcuts
        .iter()
        .find(|a| a.name == "Custom")
        .unwrap();
    assert!(found.chord.ctrl && found.chord.shift && !found.chord.alt);
    assert_eq!(found.chord.key, "F9");
    assert_eq!(found.command, "my.exe");
    assert_eq!(found.args, vec!["{path}", "--flag"]);
    assert!(found.single);
}

#[test]
fn partial_config_loads_missing_sections() {
    // Only [general] + [rclone] set — plugins, shortcuts, recent_paths must default.
    let text = r#"
        [general]
        show_hidden = true
        show_system = false
        announce_interval_secs = 30

        [rclone]
        progress_window = true
    "#;
    let c: Config = toml::from_str(text).expect("partial parse");
    assert!(c.general.show_hidden);
    assert!(c.rclone.progress_window);
    assert_eq!(c.general.announce_interval_secs, 30);
    // Shortcuts default is populated by default_actions(), not empty —
    // serde_default uses Config::default() for the field.
    // But with explicit [general] and no [shortcuts], serde gets a missing
    // field and falls back to Vec::default() which is empty.
    assert!(c.plugins.entries.is_empty());
    assert!(c.recent_paths.is_empty());
}

#[test]
fn unknown_fields_ignored_for_forward_compat() {
    // Simulates a config written by a newer version with extra keys.
    let text = r#"
        [general]
        show_hidden = true
        future_option = "xyz"

        [[shortcuts]]
        name = "x"
        command = "x.exe"
        unknown_future_field = 42
        [shortcuts.chord]
        ctrl = true
        key = "X"
        deprecated_win = false
    "#;
    let c: Config = toml::from_str(text).expect("should ignore unknown fields");
    assert!(c.general.show_hidden);
    assert_eq!(c.shortcuts.len(), 1);
    assert_eq!(c.shortcuts[0].name, "x");
    assert!(c.shortcuts[0].chord.ctrl);
}

#[test]
fn chord_defaults_to_all_false() {
    let chord = ShortcutChord::default();
    assert!(!chord.ctrl && !chord.shift && !chord.alt);
    assert!(chord.key.is_empty());
}

#[test]
fn default_columns_are_all_visible() {
    // Fresh installs keep the historical four-column layout. Flipping any
    // default here is a visible UX change — the test exists so we don't
    // do it by accident.
    let c = Config::default();
    assert!(c.general.columns.show_size);
    assert!(c.general.columns.show_type);
    assert!(c.general.columns.show_modified);
}

#[test]
fn columns_roundtrip_through_toml() {
    let mut c = Config::default();
    c.general.columns.show_type = false;
    c.general.columns.show_modified = false;
    let text = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&text).expect("reparse");
    assert!(back.general.columns.show_size);
    assert!(!back.general.columns.show_type);
    assert!(!back.general.columns.show_modified);
}

#[test]
fn columns_section_is_optional_in_toml() {
    // Pre-existing configs never saw `[general.columns]`. Loading one
    // should fall back to the fully-visible default rather than erroring.
    let text = r#"
        [general]
        show_hidden = false
    "#;
    let c: Config = toml::from_str(text).expect("parse");
    assert!(c.general.columns.show_size);
    assert!(c.general.columns.show_type);
    assert!(c.general.columns.show_modified);
}

#[test]
fn extraction_defaults_match_spec() {
    // Default behaviour: delete the archive after a clean extraction,
    // and wrap loose-content archives in a folder. Flip these here only
    // alongside the corresponding behavioural change in `extract`.
    let c = Config::default();
    assert!(c.extraction.delete_when_extracted);
    assert!(c.extraction.create_folder);
}

#[test]
fn extraction_section_is_optional_in_toml() {
    // Pre-existing configs never saw `[extraction]` — they must load
    // with the spec defaults rather than crashing.
    let text = r#"
        [general]
        show_hidden = false
    "#;
    let c: Config = toml::from_str(text).expect("parse");
    assert!(c.extraction.delete_when_extracted);
    assert!(c.extraction.create_folder);
}

#[test]
fn extraction_section_roundtrips_through_toml() {
    let mut c = Config::default();
    c.extraction.delete_when_extracted = false;
    c.extraction.create_folder = false;
    let text = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&text).expect("reparse");
    assert!(!back.extraction.delete_when_extracted);
    assert!(!back.extraction.create_folder);
}

#[test]
fn new_items_at_bottom_default_is_on() {
    // Extract-without-refresh relies on the watcher folding new files
    // into the listing via `Model::append_entries`, which only happens
    // when `general.new_items_at_bottom` is true. Flipping the default
    // would silently break that flow — guard it here.
    let c = Config::default();
    assert!(c.general.new_items_at_bottom);
}

#[test]
fn sort_mode_type_roundtrips_through_toml() {
    // The Type variant was added after Name/Size/Modified/Created — guard
    // against accidentally dropping it from the enum.
    let mut c = Config::default();
    c.general.sort_mode = navigator_config::SortMode::Type;
    let text = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&text).expect("reparse");
    assert_eq!(back.general.sort_mode, navigator_config::SortMode::Type);
}

#[test]
fn on_conflict_defaults_to_update() {
    // Update is the only mode that both makes progress on a merge and
    // cannot roll back a newer edit at the destination, which is what
    // lets a plain Ctrl+V run without prompting. Guard the default.
    let c = Config::default();
    assert_eq!(c.rclone.on_conflict, PasteConflictMode::Update);
}

#[test]
fn on_conflict_section_is_optional_in_toml() {
    // Pre-existing configs have an [rclone] section with no on_conflict
    // key — they must load as Update rather than failing.
    let text = r#"
        [rclone]
        progress_window = true
        transfers = 4
    "#;
    let c: Config = toml::from_str(text).expect("parse");
    assert_eq!(c.rclone.on_conflict, PasteConflictMode::Update);
    assert!(c.rclone.progress_window);
    assert_eq!(c.rclone.transfers, 4);
}

#[test]
fn on_conflict_roundtrips_every_mode_through_toml() {
    // The mode is persisted as a snake_case string. Round-trip all four
    // so a serde rename cannot silently reset a user's choice.
    for mode in PasteConflictMode::ALL {
        let mut c = Config::default();
        c.rclone.on_conflict = mode;
        let text = toml::to_string_pretty(&c).expect("serialize");
        let back: Config = toml::from_str(&text).expect("reparse");
        assert_eq!(
            back.rclone.on_conflict, mode,
            "mode {:?} did not survive",
            mode
        );
    }
}

#[test]
fn junk_on_conflict_value_falls_back_to_defaults() {
    // A hand-edited config with a bogus mode must not wedge the app.
    // `ConfigHandle::load_or_default` is infallible by design, so the
    // parse failure here is what that path swallows — assert the parse
    // does fail loudly at this level so the fallback is the only route.
    let text = r#"
        [rclone]
        on_conflict = "obliterate"
    "#;
    assert!(
        toml::from_str::<Config>(text).is_err(),
        "an unknown mode must not silently deserialize to something"
    );
}

#[test]
fn default_chords_are_unique() {
    // The accel table matches modifiers strictly, so two defaults on the
    // same chord means one silently never fires. Adding a binding (this
    // guard was written when Paste special claimed Ctrl+Shift+V) must not
    // shadow an existing one.
    use std::collections::HashMap;
    let c = Config::default();
    let mut seen: HashMap<(bool, bool, bool, String), String> = HashMap::new();
    for a in &c.shortcuts {
        let key = (
            a.chord.ctrl,
            a.chord.shift,
            a.chord.alt,
            a.chord.key.to_ascii_uppercase(),
        );
        if let Some(prev) = seen.insert(key.clone(), a.name.clone()) {
            panic!(
                "default chord collision on {:?}: {:?} and {:?}",
                key, prev, a.name
            );
        }
    }
}

#[test]
fn paste_special_is_bound_by_default() {
    use navigator_config::InternalCommand;
    let c = Config::default();
    let found = c.shortcuts.iter().any(|a| {
        a.internal == Some(InternalCommand::PasteSpecial)
            && a.chord.ctrl
            && a.chord.shift
            && !a.chord.alt
            && a.chord.key.eq_ignore_ascii_case("v")
    });
    assert!(found, "default Ctrl+Shift+V → Paste special missing");
}
