//! Detached shell copy/move/delete — `SHFileOperationW` in a child process.
//!
//! `op_paste_from_clipboard` deliberately hands large pastes to the
//! Windows shell copy engine rather than rclone: the shell is what
//! antivirus recognises as a legitimate file operation, and it brings its
//! own accessible progress and overwrite UI. But `SHFileOperationW` is
//! synchronous, and it used to be called straight from the window
//! procedure. Two consequences, both bad:
//!
//!   * The message pump was blocked for the entire transfer. Navigator
//!     could not repaint, answer a screen reader, or accept a keystroke
//!     until the last byte landed.
//!   * The transfer lived inside navigator's process, so closing the
//!     window killed a copy that might be hours from finishing.
//!
//! Running it on a worker thread would fix the first and not the second.
//! So it runs in a **separate process** instead: navigator re-executes
//! its own binary with `--shell-op`, which does nothing but the transfer.
//! The parent is free the moment `CreateProcess` returns, and the copy
//! outlives it — quitting navigator mid-paste now leaves the shell's
//! progress dialog running, exactly like starting the copy from Explorer
//! and then closing the Explorer window.
//!
//! Sources travel through a temp file, not argv. A paste of a few
//! thousand paths blows past the 32 KB command-line limit, and the
//! failure mode of a truncated list is a silent partial copy.
//!
//! [`ShellVerb::Delete`] rides the same machinery for an unrelated
//! reason: deleting on a UNC share must not stage into a `.trash`
//! directory at the share root, so those targets go to the shell and get
//! Explorer's behaviour and Explorer's confirmation. See [`shell_delete`].

#![cfg(windows)]

use std::io;
use std::path::{Path, PathBuf};

/// Exit codes the helper process reports back to the parent. The parent
/// only reads them to pick a spoken summary, so an unknown code is
/// treated as a plain failure.
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
/// The user pressed Cancel in the shell's own progress dialog.
pub const EXIT_ABORTED: i32 = 2;
pub const EXIT_BAD_ARGS: i32 = 3;

/// What the helper process should do with the source list.
///
/// `Delete` is the odd one out: it takes no destination, and it is not
/// here for the antivirus reason the transfer verbs are. It exists
/// because a delete on a UNC share has nowhere sane to stage — see
/// [`NavPath::is_unc`](navigator_core::NavPath::is_unc) — so the shell's
/// own "permanently delete?" prompt becomes the confirmation and the
/// shell's engine does the work, exactly as Explorer would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellVerb {
    Copy,
    Move,
    Delete,
}

impl ShellVerb {
    /// The `--shell-op` token for this verb. On-disk contract in the same
    /// sense the sound keys are: the parent and the child are the same
    /// binary, but a helper left over from a half-finished upgrade must
    /// not silently mean something else.
    pub fn token(self) -> &'static str {
        match self {
            ShellVerb::Copy => "copy",
            ShellVerb::Move => "move",
            ShellVerb::Delete => "delete",
        }
    }

    /// `--dest` is required for the transfer verbs and forbidden for
    /// `Delete`.
    pub fn needs_dest(self) -> bool {
        !matches!(self, ShellVerb::Delete)
    }
}

/// A parsed `--shell-op` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgs {
    pub verb: ShellVerb,
    /// `Some` for copy/move, always `None` for delete.
    pub dest: Option<PathBuf>,
    pub list_file: PathBuf,
}

/// Recognise the helper invocation:
/// `--shell-op copy|move --dest <dir> --list <file>` or
/// `--shell-op delete --list <file>`.
///
/// Returns `None` for anything that is not a `--shell-op` command line so
/// `main` can fall through to the normal GUI path. Returns
/// `Some(Err(..))` when it *is* one but is malformed — that must not
/// silently start a file explorer instead.
pub fn parse_helper_args(argv: &[String]) -> Option<Result<HelperArgs, String>> {
    if argv.first().map(String::as_str) != Some("--shell-op") {
        return None;
    }
    let verb = match argv.get(1).map(String::as_str) {
        Some("copy") => ShellVerb::Copy,
        Some("move") => ShellVerb::Move,
        Some("delete") => ShellVerb::Delete,
        Some(other) => return Some(Err(format!("unknown shell-op verb {other:?}"))),
        None => return Some(Err("--shell-op needs copy, move or delete".into())),
    };

    let mut dest: Option<PathBuf> = None;
    let mut list_file: Option<PathBuf> = None;
    let mut i = 2;
    while i < argv.len() {
        match argv[i].as_str() {
            "--dest" => match argv.get(i + 1) {
                Some(v) => {
                    dest = Some(PathBuf::from(v));
                    i += 2;
                }
                None => return Some(Err("--dest needs a value".into())),
            },
            "--list" => match argv.get(i + 1) {
                Some(v) => {
                    list_file = Some(PathBuf::from(v));
                    i += 2;
                }
                None => return Some(Err("--list needs a value".into())),
            },
            other => return Some(Err(format!("unexpected argument {other:?}"))),
        }
    }

    let Some(list_file) = list_file else {
        return Some(Err("--shell-op needs --list".into()));
    };
    // A `--dest` on a delete is a caller bug, and the interesting failure
    // is the one where the two got crossed: silently ignoring it would
    // let a mis-built command line delete the sources of what was meant
    // to be a copy.
    match (verb.needs_dest(), &dest) {
        (true, None) => return Some(Err("--shell-op needs --dest".into())),
        (false, Some(_)) => return Some(Err("--shell-op delete takes no --dest".into())),
        _ => {}
    }
    Some(Ok(HelperArgs {
        verb,
        dest,
        list_file,
    }))
}

/// Serialise `sources` into the newline-delimited body of a list file.
///
/// Windows filenames cannot contain `\n` or `\r`, so newline delimiting
/// is unambiguous for the local paths the shell engine accepts. A path
/// that somehow carries one is dropped rather than written: a corrupted
/// list is read back as two bogus paths, and the shell would either fail
/// confusingly or — worse — act on the wrong file.
pub fn encode_list(sources: &[PathBuf]) -> String {
    let mut out = String::new();
    for s in sources {
        let text = s.to_string_lossy();
        if text.contains(['\n', '\r']) {
            tracing::warn!("skipping shell-op source with a newline in its name: {text:?}");
            continue;
        }
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// Inverse of [`encode_list`]. Blank lines are skipped so a trailing
/// newline (or a hand-edited file) can't produce an empty path that the
/// shell would interpret as the current directory.
pub fn decode_list(body: &str) -> Vec<PathBuf> {
    body.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Launch the detached helper. Returns the child so the caller can wait
/// on it for a completion announcement — dropping the handle is fine and
/// leaves the copy running.
///
/// The temp list file is owned by the *child*, which deletes it after
/// reading. The parent cannot: it is gone long before the child starts.
pub fn spawn_detached(
    sources: &[PathBuf],
    verb: ShellVerb,
    dest: Option<&Path>,
) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;

    if verb.needs_dest() && dest.is_none() {
        return Err(io::Error::other("copy/move needs a destination"));
    }

    // CREATE_NO_WINDOW: the debug build is a console subsystem binary, and
    // without this every paste would flash a console window and steal
    // focus from the listview.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // CREATE_BREAKAWAY_FROM_JOB: if navigator itself was started inside a
    // job object that kills its children on close (some terminals and task
    // runners do this), the helper would inherit the job and die with us —
    // defeating the entire point. Jobs that forbid breakaway make
    // CreateProcess fail, so this is attempted and then retried without.
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let body = encode_list(sources);
    if body.is_empty() {
        return Err(io::Error::other("no usable source paths"));
    }
    let list_file = list_file_path();
    std::fs::write(&list_file, body)?;

    let exe = std::env::current_exe()?;
    let build = |flags: u32| {
        let mut c = std::process::Command::new(&exe);
        c.arg("--shell-op").arg(verb.token());
        if let Some(d) = dest {
            c.arg("--dest").arg(d);
        }
        c.arg("--list").arg(&list_file).creation_flags(flags);
        c
    };

    let child = match build(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("shell-op breakaway spawn failed ({e}); retrying inside the job");
            match build(CREATE_NO_WINDOW).spawn() {
                Ok(c) => c,
                Err(e) => {
                    // Nothing will consume the list file now.
                    let _ = std::fs::remove_file(&list_file);
                    return Err(e);
                }
            }
        }
    };

    // The shell's progress and conflict dialogs belong to the child. Hand
    // over our foreground right so they come up in front of the user
    // instead of behind navigator's window — an overwrite prompt the user
    // never sees is an operation that appears to hang.
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id());
    }

    Ok(child)
}

/// Per-invocation temp path for the source list. PID + a process-local
/// counter keep concurrent pastes — including ones from a peer navigator
/// instance — off each other's file.
fn list_file_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "navigator-shellop-{}-{}.txt",
        std::process::id(),
        n
    ))
}

/// Child-process entry point. Reads the list, runs the shell transfer,
/// removes the list file, and returns the process exit code.
pub fn run_helper(args: &HelperArgs) -> i32 {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };

    let body = match std::fs::read_to_string(&args.list_file) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("shell-op: cannot read {:?}: {}", args.list_file, e);
            return EXIT_FAILED;
        }
    };
    let _ = std::fs::remove_file(&args.list_file);

    let sources = decode_list(&body);
    if sources.is_empty() {
        tracing::error!("shell-op: source list is empty");
        return EXIT_FAILED;
    }

    // The shell engine wants COM on the calling thread. This process
    // exists solely for the transfer, so the main thread is the right (and
    // only) apartment.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }

    let outcome = match args.verb {
        ShellVerb::Delete => shell_delete(&sources),
        verb => {
            // `parse_helper_args` guarantees this, but the child must not
            // fall back to *some* directory if that ever stops being true.
            let Some(dest) = args.dest.as_deref() else {
                tracing::error!("shell-op: {} without a destination", verb.token());
                return EXIT_BAD_ARGS;
            };
            shell_copy_move(&sources, dest, matches!(verb, ShellVerb::Move))
        }
    };

    match outcome {
        Ok(true) => EXIT_ABORTED,
        Ok(false) => EXIT_OK,
        Err(e) => {
            tracing::error!("shell-op: {}", e);
            EXIT_FAILED
        }
    }
}

/// Delete `sources` through the Windows shell engine.
///
/// This is the UNC-share delete path. Navigator's own delete stages to
/// `<volume_root>\.trash` and offers Ctrl+Z, which is wrong for a network
/// share in two ways: the volume root belongs to somebody else's server,
/// and a `.trash` directory created there comes back hidden from a macOS
/// or Samba SMB server (both flag dot-prefixed names), so the file
/// appears to have simply vanished. Handing the delete to the shell gives
/// the user Explorer's behaviour instead — Recycle Bin where one exists,
/// and on a share the shell's own "are you sure you want to permanently
/// delete" prompt, which is the confirmation.
///
/// `FOF_ALLOWUNDO` is what asks for the Recycle Bin. Keep it even though
/// only UNC paths reach here: it is what makes the shell prompt rather
/// than delete silently, and it costs nothing when the share cannot
/// honour it.
///
/// Returns `Ok(true)` if the user declined the prompt or cancelled
/// mid-operation, `Ok(false)` on a clean run, `Err` on a shell error.
fn shell_delete(sources: &[PathBuf]) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, SHFILEOPSTRUCTW, SHFileOperationW};
    use windows::core::PCWSTR;

    let mut from: Vec<u16> = Vec::new();
    for s in sources {
        from.extend(s.as_os_str().encode_wide());
        from.push(0);
    }
    from.push(0);

    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND(std::ptr::null_mut()),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        // FO_DELETE has no destination; pTo must stay null.
        pTo: PCWSTR::null(),
        fFlags: FOF_ALLOWUNDO.0 as u16,
        ..Default::default()
    };

    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "SHFileOperation(delete) returned 0x{:x}",
            rc
        )));
    }
    // Answering "No" to the confirmation is a clean return with this flag
    // set, not an error code.
    Ok(op.fAnyOperationsAborted.as_bool())
}

/// Copy or move `sources` into the `dest` directory via the Windows
/// shell copy engine (`SHFileOperationW`). This is the deliberate
/// exception to the "all mutations go through rclone" invariant: the
/// shell engine is what antivirus recognises as a legitimate file
/// operation, so pasting thousands of files this way avoids the
/// heuristics that flag rclone. The shell owns the (accessible) progress
/// and overwrite-conflict UI.
///
/// Returns `Ok(true)` if the user aborted mid-operation, `Ok(false)` on
/// a clean run, `Err` on a shell error code.
fn shell_copy_move(sources: &[PathBuf], dest: &Path, move_op: bool) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{
        FO_COPY, FO_MOVE, FOF_NOCONFIRMMKDIR, SHFILEOPSTRUCTW, SHFileOperationW,
    };
    use windows::core::PCWSTR;

    // pFrom: each source NUL-terminated, list ends in a double-NUL.
    let mut from: Vec<u16> = Vec::new();
    for s in sources {
        from.extend(s.as_os_str().encode_wide());
        from.push(0);
    }
    from.push(0);

    // pTo: the single destination directory, also double-NUL-terminated.
    let mut to: Vec<u16> = dest.as_os_str().encode_wide().collect();
    to.push(0);
    to.push(0);

    let mut op = SHFILEOPSTRUCTW {
        // No owner window: this process has none. The shell parents its
        // progress and conflict dialogs to the desktop, and the parent
        // handed us its foreground right so they still come up in front.
        hwnd: HWND(std::ptr::null_mut()),
        wFunc: if move_op { FO_MOVE } else { FO_COPY },
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR(to.as_ptr()),
        // Don't prompt to create the destination — it already exists.
        // Overwrite/skip conflicts still surface the shell's own dialog.
        fFlags: FOF_NOCONFIRMMKDIR.0 as u16,
        ..Default::default()
    };

    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "SHFileOperation returned 0x{:x}",
            rc
        )));
    }
    Ok(op.fAnyOperationsAborted.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn non_shell_op_argv_falls_through_to_the_gui() {
        assert!(parse_helper_args(&args(&[])).is_none());
        assert!(parse_helper_args(&args(&[r"C:\Users"])).is_none());
        assert!(parse_helper_args(&args(&["-r", "mac:Downloads"])).is_none());
    }

    #[test]
    fn parses_copy_and_move() {
        let got = parse_helper_args(&args(&[
            "--shell-op",
            "copy",
            "--dest",
            r"D:\dst",
            "--list",
            r"C:\tmp\l.txt",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            got,
            HelperArgs {
                verb: ShellVerb::Copy,
                dest: Some(PathBuf::from(r"D:\dst")),
                list_file: PathBuf::from(r"C:\tmp\l.txt"),
            }
        );

        let moved = parse_helper_args(&args(&[
            "--shell-op",
            "move",
            "--dest",
            r"D:\dst",
            "--list",
            r"C:\tmp\l.txt",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(moved.verb, ShellVerb::Move);
    }

    /// Delete carries no destination — the whole point is that there is
    /// nowhere to stage to.
    #[test]
    fn parses_delete_without_a_dest() {
        let got = parse_helper_args(&args(&["--shell-op", "delete", "--list", r"C:\tmp\l.txt"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            got,
            HelperArgs {
                verb: ShellVerb::Delete,
                dest: None,
                list_file: PathBuf::from(r"C:\tmp\l.txt"),
            }
        );
    }

    /// A `--dest` on a delete means the caller crossed two commands. The
    /// dangerous reading is "copy these somewhere" arriving as "delete
    /// these", so it is rejected rather than ignored.
    #[test]
    fn delete_with_a_dest_is_rejected_not_ignored() {
        let r = parse_helper_args(&args(&[
            "--shell-op",
            "delete",
            "--dest",
            r"D:\dst",
            "--list",
            r"C:\tmp\l.txt",
        ]));
        assert!(matches!(r, Some(Err(_))), "got {r:?}");
    }

    /// The verb tokens are what a parent process writes and a child
    /// parses. They must round-trip.
    #[test]
    fn verb_tokens_round_trip() {
        for v in [ShellVerb::Copy, ShellVerb::Move, ShellVerb::Delete] {
            let mut argv = vec!["--shell-op".to_string(), v.token().to_string()];
            if v.needs_dest() {
                argv.push("--dest".into());
                argv.push(r"D:\dst".into());
            }
            argv.push("--list".into());
            argv.push(r"C:\l.txt".into());
            assert_eq!(parse_helper_args(&argv).unwrap().unwrap().verb, v);
        }
    }

    /// Flag order must not matter, and paths with spaces have to survive
    /// the round trip — `C:\Program Files` is the obvious real case.
    #[test]
    fn accepts_flags_in_either_order_and_paths_with_spaces() {
        let got = parse_helper_args(&args(&[
            "--shell-op",
            "copy",
            "--list",
            r"C:\tmp\my list.txt",
            "--dest",
            r"C:\Program Files\dst",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(got.dest, Some(PathBuf::from(r"C:\Program Files\dst")));
        assert_eq!(got.list_file, PathBuf::from(r"C:\tmp\my list.txt"));
    }

    /// A malformed `--shell-op` must be an error, never a fall-through:
    /// launching a GUI window because a flag was missing would leave the
    /// user staring at a file explorer they did not ask for while their
    /// paste silently never happened.
    #[test]
    fn malformed_shell_op_is_an_error_not_a_fallthrough() {
        for bad in [
            args(&["--shell-op"]),
            args(&["--shell-op", "shred", "--dest", "d", "--list", "l"]),
            args(&["--shell-op", "copy", "--list", "l"]),
            args(&["--shell-op", "copy", "--dest", "d"]),
            args(&["--shell-op", "copy", "--dest"]),
            args(&["--shell-op", "copy", "--dest", "d", "--list", "l", "extra"]),
            args(&["--shell-op", "delete"]),
            args(&["--shell-op", "delete", "--list"]),
        ] {
            let r = parse_helper_args(&bad);
            assert!(
                matches!(r, Some(Err(_))),
                "{bad:?} should be a shell-op error, got {r:?}"
            );
        }
    }

    #[test]
    fn list_round_trips() {
        let sources = vec![
            PathBuf::from(r"C:\a\one.txt"),
            PathBuf::from(r"C:\Program Files\two three.txt"),
            PathBuf::from(r"\\server\share\four.txt"),
        ];
        assert_eq!(decode_list(&encode_list(&sources)), sources);
    }

    /// The list is newline-delimited, so a name containing a newline
    /// would decode as two paths. Windows forbids those characters, but
    /// the encoder drops such an entry rather than emitting a list that
    /// makes the shell act on a path nobody selected.
    #[test]
    fn newline_bearing_source_is_dropped_not_split() {
        let sources = vec![
            PathBuf::from("C:\\a\\good.txt"),
            PathBuf::from("C:\\a\\ev\nil.txt"),
            PathBuf::from("C:\\a\\also_good.txt"),
        ];
        let decoded = decode_list(&encode_list(&sources));
        assert_eq!(
            decoded,
            vec![
                PathBuf::from("C:\\a\\good.txt"),
                PathBuf::from("C:\\a\\also_good.txt"),
            ]
        );
    }

    /// A trailing newline is normal output from the encoder; it must not
    /// decode into an empty path, which the shell would resolve to the
    /// current directory.
    #[test]
    fn blank_lines_never_become_paths() {
        assert!(decode_list("\n\n   \n").is_empty());
        assert_eq!(
            decode_list("C:\\a.txt\n\nC:\\b.txt\n"),
            vec![PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\b.txt")]
        );
    }

    /// Two pastes in flight at once must not share a list file — the
    /// first child to finish deletes it, and the second would find
    /// nothing to copy.
    #[test]
    fn concurrent_pastes_get_distinct_list_files() {
        let a = list_file_path();
        let b = list_file_path();
        assert_ne!(a, b);
    }
}
