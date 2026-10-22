//! "What is using this drive?" — as administrator.
//!
//! `navigator_fs::processes_using_drive` can only open processes running as
//! the same user at the same integrity level. An elevated terminal parked
//! in a folder on the drive, the Search indexer, antivirus — the usual
//! reasons an eject is vetoed — all refuse the peek, and the scan comes
//! back empty while one of them is exactly the culprit.
//!
//! Rather than run navigator itself elevated (every file operation would
//! then run as administrator), the scan alone is re-executed as a helper:
//! `navigator --drive-users <root> --out <file>` under UAC, which writes the
//! result as JSON and exits. Same shape as the `--shell-op` helper: `main`
//! calls [`crate::try_run_drive_users_helper`] before any config is read or
//! HWND created, and a malformed command line exits non-zero rather than
//! falling through to the GUI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use navigator_fs::DriveUser;

pub const HELPER_FLAG: &str = "--drive-users";
const OUT_FLAG: &str = "--out";

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_BAD_ARGS: i32 = 3;

/// How long the parent waits for the elevated scan, UAC prompt included.
/// The scan itself is well under a second; the rest is the user reading
/// the consent dialog.
const WAIT: Duration = Duration::from_secs(120);

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Recognise the helper form. `None` when this is not a helper invocation;
/// `Some(Err)` when it is one but malformed — the caller must exit, never
/// open a window.
pub fn parse_helper_args(argv: &[String]) -> Option<Result<(String, PathBuf), String>> {
    if argv.first().map(String::as_str) != Some(HELPER_FLAG) {
        return None;
    }
    let root = match argv.get(1) {
        Some(r) if !r.starts_with("--") => r.clone(),
        _ => return Some(Err(format!("{HELPER_FLAG} needs a drive root"))),
    };
    if argv.get(2).map(String::as_str) != Some(OUT_FLAG) {
        return Some(Err(format!("{HELPER_FLAG} needs {OUT_FLAG} <file>")));
    }
    let Some(out) = argv.get(3) else {
        return Some(Err(format!("{OUT_FLAG} needs a file path")));
    };
    if argv.len() > 4 {
        return Some(Err("unexpected extra arguments".into()));
    }
    Some(Ok((root, PathBuf::from(out))))
}

/// The helper body: scan, write JSON, exit.
pub fn run_helper(root: &str, out: &PathBuf) -> i32 {
    let users = navigator_fs::processes_using_drive(root);
    match serde_json::to_vec(&users).map(|bytes| std::fs::write(out, bytes)) {
        Ok(Ok(())) => EXIT_OK,
        _ => EXIT_FAILED,
    }
}

/// Run the scan as administrator and return what it found. Blocks for the
/// UAC prompt and the helper; worker thread only.
///
/// `Err` is the human-readable reason nothing could be learned — the UAC
/// prompt was declined, the helper did not start, it timed out, or its
/// output could not be read.
pub fn scan_elevated(root: &str) -> Result<Vec<DriveUser>, String> {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::PCWSTR;

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate navigator.exe: {e}"))?;
    let out = std::env::temp_dir().join(format!(
        "{}{}-{}.json",
        crate::tempsweep::DRIVE_USERS_PREFIX,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_file(&out);

    let argv = [
        HELPER_FLAG.to_string(),
        root.to_string(),
        OUT_FLAG.to_string(),
        out.to_string_lossy().into_owned(),
    ];
    let params = crate::elevated::quote_argv(&argv);

    let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain([0]).collect();
    let verb_w: Vec<u16> = "runas".encode_utf16().chain([0]).collect();
    let params_w: Vec<u16> = params.encode_utf16().chain([0]).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // A declined UAC prompt surfaces here as ERROR_CANCELLED.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|_| "the administrator prompt was declined".to_string())?;
    if info.hProcess.is_invalid() {
        return Err("no elevated process started".into());
    }

    let result = unsafe {
        let waited = WaitForSingleObject(info.hProcess, WAIT.as_millis() as u32);
        let mut code: u32 = 1;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if waited == WAIT_OBJECT_0 {
            Ok(code)
        } else {
            Err("the administrator scan did not finish in time".to_string())
        }
    };
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);

    match result? {
        0 => {}
        code => return Err(format!("the administrator scan failed (exit code {code})")),
    }
    let bytes = bytes.map_err(|e| format!("cannot read the scan result: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("scan result unreadable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn non_helper_invocations_are_not_recognised() {
        assert!(parse_helper_args(&s(&[])).is_none());
        assert!(parse_helper_args(&s(&["D:\\"])).is_none());
        assert!(parse_helper_args(&s(&["--shell-op", "copy"])).is_none());
    }

    #[test]
    fn well_formed_helper_args_parse() {
        let r = parse_helper_args(&s(&["--drive-users", "D:\\", "--out", "C:\\t\\x.json"]))
            .unwrap()
            .unwrap();
        assert_eq!(r.0, "D:\\");
        assert_eq!(r.1, PathBuf::from("C:\\t\\x.json"));
    }

    #[test]
    fn malformed_helper_args_are_errors_not_gui_launches() {
        assert!(parse_helper_args(&s(&["--drive-users"])).unwrap().is_err());
        assert!(
            parse_helper_args(&s(&["--drive-users", "--out", "x"]))
                .unwrap()
                .is_err()
        );
        assert!(
            parse_helper_args(&s(&["--drive-users", "D:\\"]))
                .unwrap()
                .is_err()
        );
        assert!(
            parse_helper_args(&s(&["--drive-users", "D:\\", "--out"]))
                .unwrap()
                .is_err()
        );
        assert!(
            parse_helper_args(&s(&["--drive-users", "D:\\", "--out", "x", "extra"]))
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn helper_writes_json_the_parent_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("users.json");
        assert_eq!(run_helper("C:\\", &out), EXIT_OK);
        let users: Vec<DriveUser> = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        // Unelevated, but running as the same user as the test: our own
        // tooling holds files on C:, so the list is never empty.
        assert!(!users.is_empty());
    }
}
