//! Action placeholder-expansion tests. No filesystem required — we pass
//! `is_dir` explicitly so tests are fully deterministic.

#![cfg(windows)]

use std::path::Path;

use navigator_config::{ShortcutAction, ShortcutChord};
use navigator_gui::actions::{expand, expand_args};

#[test]
fn path_substitution_uses_full_string() {
    let r = expand("{path}", Path::new(r"C:\Users\name\file.txt"), false);
    assert_eq!(r, r"C:\Users\name\file.txt");
}

#[test]
fn folder_is_self_for_directories() {
    let r = expand("{folder}", Path::new(r"C:\Projects"), true);
    assert_eq!(r, r"C:\Projects");
}

#[test]
fn folder_is_parent_for_files() {
    let r = expand("{folder}", Path::new(r"C:\Projects\src.rs"), false);
    assert_eq!(r, r"C:\Projects");
}

#[test]
fn parent_is_always_the_directory_containing_target() {
    let file = expand("{parent}", Path::new(r"C:\a\b.txt"), false);
    let dir  = expand("{parent}", Path::new(r"C:\a\b"),     true);
    assert_eq!(file, r"C:\a");
    // Note: parent of a directory is its own parent, not itself.
    assert_eq!(dir,  r"C:\a");
}

#[test]
fn name_returns_basename() {
    assert_eq!(expand("{name}", Path::new(r"C:\foo\bar.txt"), false), "bar.txt");
    assert_eq!(expand("{name}", Path::new(r"C:\foo\mydir"),   true),  "mydir");
}

#[test]
fn all_placeholders_together() {
    let template = r"cd {parent} && echo {name} in {folder} from {path}";
    let r = expand(template, Path::new(r"C:\src\hello.rs"), false);
    assert_eq!(
        r,
        r"cd C:\src && echo hello.rs in C:\src from C:\src\hello.rs"
    );
}

#[test]
fn path_with_spaces_preserved_unquoted() {
    // Expansion itself does not quote. Callers (e.g. `op_copy_paths`) are
    // responsible for quoting when they paste the result into a shell.
    let r = expand("{path}", Path::new(r"C:\Program Files\tool.exe"), false);
    assert_eq!(r, r"C:\Program Files\tool.exe");
}

#[test]
fn unicode_path_roundtrips() {
    let r = expand("{name}", Path::new(r"C:\proj\файл.txt"), false);
    assert_eq!(r, "файл.txt");
}

#[test]
fn expand_args_substitutes_every_arg() {
    let action = ShortcutAction {
        name: "x".into(),
        chord: ShortcutChord::default(),
        internal: None,
        command: "wt.exe".into(),
        args: vec!["-d".into(), "{folder}".into(), "--file".into(), "{name}".into()],
        single: true,
    };
    let out = expand_args(&action, Path::new(r"C:\work\build.rs"), false);
    assert_eq!(out, vec![
        "-d".to_string(),
        r"C:\work".to_string(),
        "--file".to_string(),
        "build.rs".to_string(),
    ]);
}

#[test]
fn placeholders_idempotent_when_absent() {
    // Template without any placeholder comes through unchanged.
    let r = expand("hello world", Path::new(r"C:\x"), true);
    assert_eq!(r, "hello world");
}

#[test]
fn unknown_braced_tokens_left_alone() {
    // Forward-compat: a template with an unknown `{foo}` must not explode.
    let r = expand("{foo}/{name}", Path::new(r"C:\x\y.txt"), false);
    assert_eq!(r, "{foo}/y.txt");
}
