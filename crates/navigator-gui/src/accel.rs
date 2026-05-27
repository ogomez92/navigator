//! Translate human-readable shortcut chords into the data
//! `CreateAcceleratorTableW` expects.
//!
//! Keeping the translation pure (no Win32 imports beyond the flag types)
//! lets us unit-test every branch. The shortcut editor dialog calls
//! [`chord_to_accel`] on save; the main window uses the same function when
//! rebuilding the accelerator table.

#![cfg(windows)]

use navigator_config::ShortcutChord;
use windows::Win32::UI::WindowsAndMessaging::{ACCEL_VIRT_FLAGS, FALT, FCONTROL, FSHIFT, FVIRTKEY};

/// Parse a key name into a Virtual-Key code. Accepts:
///
///   * a single ASCII letter A-Z (case insensitive)
///   * a single digit 0-9
///   * `F1` … `F24`
///   * named keys (case-insensitive): `Up`, `Down`, `Left`, `Right`,
///     `Home`, `End`, `PageUp`, `PageDown`, `Insert`, `Delete`, `Tab`,
///     `Space`, `Escape`/`Esc`, `Enter`/`Return`, `Backspace`
///
/// Returns `None` for anything else — callers treat that as "no accelerator".
pub fn parse_vk(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes.len() == 1 {
        let b = bytes[0].to_ascii_uppercase();
        return match b {
            b'A'..=b'Z' => Some(b as u16),
            b'0'..=b'9' => Some(b as u16),
            _ => None,
        };
    }
    if bytes[0].to_ascii_uppercase() == b'F' && bytes.len() <= 3 {
        if let Ok(n) = s[1..].parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(0x6F + n); // VK_F1 = 0x70, so n=1 → 0x70.
            }
        }
    }
    named_vk(s)
}

/// Look up a named virtual key (case-insensitive). Kept as a small table
/// rather than a match so the editor UI can enumerate the valid names.
fn named_vk(s: &str) -> Option<u16> {
    const NAMED: &[(&str, u16)] = &[
        ("Up", 0x26),
        ("Down", 0x28),
        ("Left", 0x25),
        ("Right", 0x27),
        ("Home", 0x24),
        ("End", 0x23),
        ("PageUp", 0x21),
        ("PageDown", 0x22),
        ("Insert", 0x2D),
        ("Delete", 0x2E),
        ("Tab", 0x09),
        ("Space", 0x20),
        ("Escape", 0x1B),
        ("Esc", 0x1B),
        ("Enter", 0x0D),
        ("Return", 0x0D),
        ("Backspace", 0x08),
    ];
    NAMED
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(s))
        .map(|(_, vk)| *vk)
}

/// Resolve a chord into the `(vk, fVirt)` pair stored in an `ACCEL`
/// record. Returns `None` for an unparsable key name.
pub fn chord_to_accel(chord: &ShortcutChord) -> Option<(u16, ACCEL_VIRT_FLAGS)> {
    let vk = parse_vk(&chord.key)?;
    let mut mods = FVIRTKEY;
    if chord.ctrl {
        mods = mods | FCONTROL;
    }
    if chord.shift {
        mods = mods | FSHIFT;
    }
    if chord.alt {
        mods = mods | FALT;
    }
    Some((vk, mods))
}
