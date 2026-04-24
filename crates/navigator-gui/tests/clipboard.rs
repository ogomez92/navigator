//! Dry-run tests for the clipboard + history layer. No filesystem IO —
//! we exercise the serde roundtrip and the pure helpers only.

#![cfg(windows)]

use navigator_gui::clipboard::{ClipFile, HistoryEntry, entry_label};

#[test]
fn clipfile_serde_roundtrip_copy() {
    let c = ClipFile {
        sources: vec![r"C:\foo\bar.txt".into(), r"C:\foo\baz".into()],
        cut: false,
        ts: 1_700_000_000,
    };
    let s = serde_json::to_string(&c).unwrap();
    let back: ClipFile = serde_json::from_str(&s).unwrap();
    assert_eq!(back.sources, c.sources);
    assert!(!back.cut);
    assert_eq!(back.ts, 1_700_000_000);
}

#[test]
fn clipfile_serde_roundtrip_cut_preserves_flag() {
    let c = ClipFile { sources: vec!["a".into()], cut: true, ts: 0 };
    let s = serde_json::to_string(&c).unwrap();
    let back: ClipFile = serde_json::from_str(&s).unwrap();
    assert!(back.cut);
}

#[test]
fn clipfile_default_is_empty() {
    let c = ClipFile::default();
    assert!(c.sources.is_empty());
    assert!(!c.cut);
    assert_eq!(c.ts, 0);
}

#[test]
fn clipfile_deserialize_missing_fields_uses_defaults() {
    // Forward-compat: a file written by an older build with only `sources`
    // should still load cleanly.
    let back: ClipFile = serde_json::from_str(r#"{"sources":["x"]}"#).unwrap();
    assert_eq!(back.sources, vec!["x".to_string()]);
    assert!(!back.cut);
    assert_eq!(back.ts, 0);
}

#[test]
fn history_entry_serde_roundtrip() {
    let e = HistoryEntry {
        kind: "paste".into(),
        sources: vec![r"D:\x\a.bin".into(), r"D:\x\b.bin".into()],
        dest: Some(r"E:\archive".into()),
        ts: 42,
    };
    let s = serde_json::to_string(&e).unwrap();
    let back: HistoryEntry = serde_json::from_str(&s).unwrap();
    assert_eq!(back.kind, "paste");
    assert_eq!(back.sources, e.sources);
    assert_eq!(back.dest.as_deref(), Some(r"E:\archive"));
    assert_eq!(back.ts, 42);
}

#[test]
fn entry_label_copy_single_item() {
    let e = HistoryEntry {
        kind: "copy".into(),
        sources: vec![r"C:\a\report.txt".into()],
        dest: None,
        ts: 0,
    };
    assert_eq!(entry_label(&e), "Copy report.txt");
}

#[test]
fn entry_label_suffix_shows_extra_count() {
    let e = HistoryEntry {
        kind: "cut".into(),
        sources: vec!["one".into(), "two".into(), "three".into()],
        dest: None,
        ts: 0,
    };
    // basename of "one" is itself; +2 more.
    assert_eq!(entry_label(&e), "Cut one (+2 more)");
}

#[test]
fn entry_label_paste_includes_dest_basename() {
    let e = HistoryEntry {
        kind: "paste".into(),
        sources: vec![r"C:\src\file.dat".into()],
        dest: Some(r"D:\target".into()),
        ts: 0,
    };
    assert_eq!(entry_label(&e), "Paste file.dat → target");
}

#[test]
fn entry_label_append_variants_have_friendly_verb() {
    for (kind, expected) in &[
        ("append-copy", "Append copy x"),
        ("append-cut",  "Append cut x"),
        ("delete",      "Delete x"),
    ] {
        let e = HistoryEntry {
            kind: (*kind).into(),
            sources: vec!["x".into()],
            dest: None,
            ts: 0,
        };
        assert_eq!(entry_label(&e).as_str(), *expected);
    }
}

#[test]
fn entry_label_unknown_kind_passes_through() {
    let e = HistoryEntry {
        kind: "future-op".into(),
        sources: vec!["q".into()],
        dest: None,
        ts: 0,
    };
    // Unknown kinds are rendered verbatim rather than "Unknown".
    assert_eq!(entry_label(&e), "future-op q");
}

#[test]
fn volume_root_of_drive_letter_path() {
    use std::path::Path;
    let got = navigator_gui::app::volume_root_of(Path::new(r"C:\foo\bar\baz.txt"));
    assert_eq!(got.as_deref(), Some(Path::new(r"C:\")));
}

#[test]
fn volume_root_of_drive_root_itself() {
    use std::path::Path;
    let got = navigator_gui::app::volume_root_of(Path::new(r"D:\"));
    assert_eq!(got.as_deref(), Some(Path::new(r"D:\")));
}

#[test]
fn volume_root_of_unc_share() {
    use std::path::Path;
    let got = navigator_gui::app::volume_root_of(Path::new(r"\\host\share\dir\file"));
    assert_eq!(got.as_deref(), Some(Path::new(r"\\host\share\")));
}
