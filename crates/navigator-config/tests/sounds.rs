//! Event-sound config: key stability, assignment semantics, and the
//! filename guard.

use std::collections::HashSet;

use navigator_config::{Config, SoundEvent, Sounds};

#[test]
fn all_lists_every_event_exactly_once() {
    let keys: HashSet<&str> = SoundEvent::ALL.iter().map(|e| e.key()).collect();
    assert_eq!(
        keys.len(),
        SoundEvent::ALL.len(),
        "duplicate key in SoundEvent::ALL — two events would share one \
         config slot and overwrite each other"
    );
}

#[test]
fn every_event_has_a_distinct_label() {
    let labels: HashSet<&str> = SoundEvent::ALL.iter().map(|e| e.label()).collect();
    assert_eq!(
        labels.len(),
        SoundEvent::ALL.len(),
        "duplicate label — the Options list would show two identical rows"
    );
}

/// Keys are the on-disk contract. Renaming one silently orphans the
/// user's assignment (it parses fine and simply stops playing), so the
/// handful that ship today are pinned here.
#[test]
fn keys_are_stable() {
    assert_eq!(SoundEvent::CopyDone.key(), "copy_done");
    assert_eq!(SoundEvent::MoveDone.key(), "move_done");
    assert_eq!(SoundEvent::DeleteDone.key(), "delete_done");
    assert_eq!(SoundEvent::Navigate.key(), "navigate");
    assert_eq!(SoundEvent::NavigateUp.key(), "navigate_up");
    assert_eq!(SoundEvent::Back.key(), "back");
    assert_eq!(SoundEvent::Error.key(), "error");
}

#[test]
fn default_is_enabled_but_silent() {
    let s = Sounds::default();
    assert!(s.enabled, "master switch should ship on");
    for ev in SoundEvent::ALL {
        assert_eq!(
            s.file_for(ev),
            None,
            "{:?} has a default sound but nothing is bundled",
            ev
        );
    }
}

#[test]
fn set_and_clear_round_trip() {
    let mut s = Sounds::default();
    s.set(SoundEvent::CopyDone, Some("done.wav"));
    assert_eq!(s.file_for(SoundEvent::CopyDone), Some("done.wav"));
    // Other slots stay untouched.
    assert_eq!(s.file_for(SoundEvent::MoveDone), None);

    s.set(SoundEvent::CopyDone, None);
    assert_eq!(s.file_for(SoundEvent::CopyDone), None);
}

/// The Options page writes `""` to clear a slot rather than deleting the
/// key, and TOML has no null — so an empty (or whitespace) value has to
/// read back as "no sound", not as a file named "".
#[test]
fn empty_value_reads_as_unset() {
    let mut s = Sounds::default();
    s.set(SoundEvent::Copy, Some("   "));
    assert_eq!(s.file_for(SoundEvent::Copy), None);

    let parsed: Sounds = toml::from_str(
        r#"
        enabled = true
        [events]
        copy = ""
        cut = "snip.wav"
        "#,
    )
    .expect("parse");
    assert_eq!(parsed.file_for(SoundEvent::Copy), None);
    assert_eq!(parsed.file_for(SoundEvent::Cut), Some("snip.wav"));
}

/// An unknown key must not fail the parse: a config written by a newer
/// build (or hand-edited with a typo) still has to load, or the user loses
/// every other setting in the file too.
#[test]
fn unknown_event_key_is_ignored_not_fatal() {
    let parsed: Sounds = toml::from_str(
        r#"
        enabled = false
        [events]
        not_a_real_event = "x.wav"
        copy_done = "ok.wav"
        "#,
    )
    .expect("unknown keys must not fail the parse");
    assert!(!parsed.enabled);
    assert_eq!(parsed.file_for(SoundEvent::CopyDone), Some("ok.wav"));
}

#[test]
fn config_round_trips_the_sounds_section() {
    let mut c = Config::default();
    c.sounds.enabled = false;
    c.sounds.set(SoundEvent::MoveDone, Some("moved.wav"));

    let text = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&text).expect("deserialize");
    assert!(!back.sounds.enabled);
    assert_eq!(
        back.sounds.file_for(SoundEvent::MoveDone),
        Some("moved.wav")
    );
}

/// `sound_path` is the only thing standing between a hand-edited
/// `config.toml` and an arbitrary path on disk, so it takes bare
/// filenames and nothing else.
#[test]
fn sound_path_rejects_anything_but_a_bare_filename() {
    assert!(navigator_config::sound_path("beep.wav").is_some());
    assert!(navigator_config::sound_path("").is_none());
    assert!(navigator_config::sound_path("   ").is_none());
    assert!(navigator_config::sound_path("..").is_none());
    assert!(navigator_config::sound_path("..\\..\\evil.wav").is_none());
    assert!(navigator_config::sound_path("sub/dir/beep.wav").is_none());
    assert!(navigator_config::sound_path("C:\\windows\\beep.wav").is_none());
}

#[test]
fn wav_extension_check_is_case_insensitive() {
    assert!(navigator_config::sounds::is_wav("a.wav"));
    assert!(navigator_config::sounds::is_wav("A.WAV"));
    assert!(navigator_config::sounds::is_wav("mixed.Wav"));
    assert!(!navigator_config::sounds::is_wav("a.mp3"));
    assert!(!navigator_config::sounds::is_wav("wav"));
    assert!(!navigator_config::sounds::is_wav("a.wav.mp3"));
}

/// The folder name is part of the user-facing contract (it is what the
/// README tells people to create), so it is pinned rather than left to
/// drift with a refactor.
#[test]
fn sounds_dir_is_named_navigator_sounds() {
    assert_eq!(navigator_config::SOUNDS_DIR_NAME, "navigator_sounds");
    assert!(
        navigator_config::sounds_dir().ends_with("navigator_sounds"),
        "sounds dir must sit directly under the exe dir"
    );
}
