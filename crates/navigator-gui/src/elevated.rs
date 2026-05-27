//! UAC-elevated rclone retry path.
//!
//! `RcloneDriver::spawn` pipes stdout/stderr so the GUI can stream
//! progress and capture stderr. UAC elevation through `ShellExecuteEx`
//! does not — child handles aren't inherited across the elevation
//! boundary, so there's no way to attach pipes after the fact. We work
//! around this by:
//!
//!   1. Building the same argv `spawn` would have used, plus
//!      `--log-file <temp>` so rclone writes its output somewhere we
//!      can read after the child exits.
//!   2. Calling `ShellExecuteExW` with verb `runas` — Windows shows the
//!      UAC prompt; the user approves; the elevated child starts.
//!   3. Blocking on the child's process handle, then reading the temp
//!      log so we can surface a tail back to the user.
//!
//! Triggered from `WorkerCtx::run_one` when an unelevated op fails with
//! a tail that matches `looks_like_access_denied`. There is no extra
//! confirmation prompt — the UAC dialog itself is the prompt.

use std::path::PathBuf;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PCWSTR;

use navigator_rclone::op::{Operation, RcloneDriver, op_args};

/// Outcome of a UAC-elevated rclone retry.
pub struct Outcome {
    pub success: bool,
    /// Tail of the elevated child's combined log output (whatever
    /// `--log-file` captured). Empty when the file was missing or
    /// unreadable.
    pub log_tail: String,
}

/// Re-issue `op` under UAC elevation. Blocks until the elevated child
/// exits. Returns `Err` only when `ShellExecuteEx` itself failed (UAC
/// declined, executable missing, etc.); a child that ran but failed
/// comes back as `Ok` with `success = false` so the caller can surface
/// the same kind of error dialog as the unelevated path.
pub fn run(driver: &RcloneDriver, op: &Operation) -> std::io::Result<Outcome> {
    // Per-op temp log path. Includes the parent PID so concurrent
    // retries from peer instances can't stomp each other; rclone
    // appends if the file exists, but we want a clean read.
    let log_path: PathBuf =
        std::env::temp_dir().join(format!("navigator-elevated-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    // Compose argv that mirrors `RcloneDriver::spawn`, plus the
    // log-file redirect since we can't pipe.
    let mut argv: Vec<String> = driver.base_args();
    argv.push("--log-file".into());
    argv.push(log_path.to_string_lossy().into_owned());
    argv.extend(op_args(op, /*dry_run=*/ false));

    let params = quote_argv(&argv);

    let exe_w: Vec<u16> = driver
        .exe()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    let verb_w: Vec<u16> = "runas".encode_utf16().chain([0]).collect();
    let params_w: Vec<u16> = params.encode_utf16().chain([0]).collect();

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: SW_HIDE.0 as i32,
        ..Default::default()
    };

    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| std::io::Error::other(format!("ShellExecuteEx: {e}")))?;
    if info.hProcess.is_invalid() {
        return Err(std::io::Error::other(
            "UAC declined or no elevated process started",
        ));
    }

    let exit_code = unsafe {
        let _ = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 1;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code
    };

    let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    Ok(Outcome {
        success: exit_code == 0,
        log_tail: tail,
    })
}

/// Format an argv into a single command-line string per the Microsoft
/// `CommandLineToArgvW` quoting rules. Wraps any arg that contains a
/// space or quote, and escapes embedded backslashes preceding the
/// closing quote. Without this, paths with spaces (`C:\Program Files`)
/// would be split into two args by the elevated child's parser.
fn quote_argv(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&quote_one(a));
    }
    out
}

fn quote_one(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"', '\n', '\u{B}']) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in s.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Escape every preceding backslash, then the quote.
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push('\\');
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    // Trailing backslashes need doubling so the closing quote isn't
    // interpreted as escaped.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote_argv;

    #[test]
    fn no_quoting_for_simple_args() {
        assert_eq!(
            quote_argv(&[
                "copyto".into(),
                "lin:/home/x.md".into(),
                "D:\\dst.md".into()
            ]),
            "copyto lin:/home/x.md D:\\dst.md"
        );
    }

    #[test]
    fn quotes_args_with_spaces() {
        let s = quote_argv(&["copyto".into(), r"C:\Program Files\foo.md".into()]);
        assert_eq!(s, r#"copyto "C:\Program Files\foo.md""#);
    }

    #[test]
    fn escapes_trailing_backslashes_inside_quoted() {
        // `C:\path with space\` — trailing slash before closing quote
        // would consume the quote without escaping; doubled.
        let s = quote_argv(&[r"C:\path with space\".into()]);
        assert_eq!(s, r#""C:\path with space\\""#);
    }

    #[test]
    fn escapes_embedded_quote() {
        let s = quote_argv(&[r#"a"b"#.into()]);
        assert_eq!(s, r#""a\"b""#);
    }
}
