//! Config serialization and loader tests.

use navigator_config::{Config, ShortcutAction, ShortcutChord};

#[test]
fn defaults_bind_common_editor_ops() {
    use navigator_config::InternalCommand;
    let c = Config::default();
    // A fresh install ships with the well-known editor chords wired up.
    let has_copy = c.shortcuts.iter().any(|a| {
        a.internal == Some(InternalCommand::Copy)
            && a.chord.ctrl && !a.chord.shift && !a.chord.alt
            && a.chord.key.eq_ignore_ascii_case("c")
    });
    let has_paste = c.shortcuts.iter().any(|a| {
        a.internal == Some(InternalCommand::Paste) && a.chord.ctrl
    });
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
    assert_eq!(c.general.announce_interval_secs, 0);
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
            ctrl: true, shift: true, alt: false,
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
    assert!(reparsed.shortcuts.iter().any(|a|
        a.name == "Copy" && a.internal == Some(navigator_config::InternalCommand::Copy)
    ));

    let found = reparsed.shortcuts.iter().find(|a| a.name == "Custom").unwrap();
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
fn sort_mode_type_roundtrips_through_toml() {
    // The Type variant was added after Name/Size/Modified/Created — guard
    // against accidentally dropping it from the enum.
    let mut c = Config::default();
    c.general.sort_mode = navigator_config::SortMode::Type;
    let text = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&text).expect("reparse");
    assert_eq!(back.general.sort_mode, navigator_config::SortMode::Type);
}
