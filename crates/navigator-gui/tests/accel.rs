//! Chord-to-accelerator translation tests.

#![cfg(windows)]

use navigator_config::ShortcutChord;
use navigator_gui::accel::{chord_to_accel, parse_vk};

#[test]
fn letters_upper_and_lower_produce_same_vk() {
    assert_eq!(parse_vk("A"), Some(0x41));
    assert_eq!(parse_vk("a"), Some(0x41));
    assert_eq!(parse_vk("Z"), Some(0x5A));
}

#[test]
fn digits_are_vk_same_as_ascii() {
    assert_eq!(parse_vk("0"), Some(0x30));
    assert_eq!(parse_vk("9"), Some(0x39));
}

#[test]
fn f_keys_map_to_vk_f1_through_f24() {
    assert_eq!(parse_vk("F1"), Some(0x70));
    assert_eq!(parse_vk("F12"), Some(0x7B));
    assert_eq!(parse_vk("F24"), Some(0x87));
}

#[test]
fn f_keys_out_of_range_rejected() {
    assert_eq!(parse_vk("F0"), None);
    assert_eq!(parse_vk("F25"), None);
    assert_eq!(parse_vk("F99"), None);
}

#[test]
fn empty_and_garbage_rejected() {
    assert_eq!(parse_vk(""), None);
    assert_eq!(parse_vk("   "), None);
    assert_eq!(parse_vk("??"), None);
    assert_eq!(parse_vk("AB"), None);
    // `Foo` starts with F but isn't an F-key.
    assert_eq!(parse_vk("Foo"), None);
}

#[test]
fn chord_with_no_key_yields_none() {
    let c = ShortcutChord {
        ctrl: true,
        ..Default::default()
    };
    assert!(chord_to_accel(&c).is_none());
}

#[test]
fn chord_always_sets_fvirtkey() {
    // Even with zero modifiers, FVIRTKEY must be present so TranslateAccelerator
    // treats the key field as a VK rather than a character.
    let c = ShortcutChord {
        key: "T".into(),
        ..Default::default()
    };
    let (_, mods) = chord_to_accel(&c).expect("parseable");
    use windows::Win32::UI::WindowsAndMessaging::FVIRTKEY;
    assert!(mods.0 & FVIRTKEY.0 != 0);
}

#[test]
fn chord_modifiers_additive() {
    use windows::Win32::UI::WindowsAndMessaging::{FALT, FCONTROL, FSHIFT, FVIRTKEY};
    let c = ShortcutChord {
        ctrl: true,
        shift: true,
        alt: true,
        key: "X".into(),
    };
    let (vk, mods) = chord_to_accel(&c).unwrap();
    assert_eq!(vk, 0x58); // 'X'
    let expected = (FVIRTKEY | FCONTROL | FSHIFT | FALT).0;
    assert_eq!(mods.0, expected);
}

#[test]
fn default_terminal_chord_parses() {
    let c = ShortcutChord {
        alt: true,
        key: "T".into(),
        ..Default::default()
    };
    let r = chord_to_accel(&c);
    assert!(
        r.is_some(),
        "the default Open-in-Terminal chord must be bindable"
    );
}
