//! The job editor — one dialog for "New job" and "Edit job".
//!
//! Built via [`crate::dialog::run_modal`] so it is a real `#32770` dialog
//! (announced as a dialog, Tab order and Esc/Enter from `DefDlgProc`).
//! Every field is a plain control a screen reader already knows: the
//! sources and excludes are multiline edits with one entry per line, so
//! paths can be pasted in or trimmed out without a list-management UI.
//!
//! Save validates through `Job::validate` plus a parse of every path, and
//! keeps the dialog open on a problem — the message names the first bad
//! line — so a typo costs a correction, not the whole form.

use std::ffi::c_void;
use std::iter::once;
use std::sync::Arc;

use navigator_config::Job;
use navigator_core::ConflictMode;
use parking_lot::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST,
    CreateWindowExW, EndDialog, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, HMENU,
    SendMessageW, SetWindowLongPtrW, SetWindowTextW, WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX,
    WM_COMMAND, WM_INITDIALOG, WM_SETFONT, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, w};

use crate::window::HwndSend;

// DWLP_USER offset on a dialog window (x64: 16). See `options.rs`.
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);
const EM_SETLIMITTEXT: u32 = 0x00C5;
const BM_SETCHECK: u32 = 0x00F1;
const BM_GETCHECK: u32 = 0x00F0;

const ID_NAME: u16 = 740;
const ID_SOURCES: u16 = 741;
const ID_DEST: u16 = 742;
const ID_MODE: u16 = 743;
const ID_CONTENTS: u16 = 744;
const ID_LINKS: u16 = 745;
const ID_EXCLUDES: u16 = 746;
const ID_TAGS: u16 = 747;
const ID_BTN_OK: u16 = 1;
const ID_BTN_CANCEL: u16 = 2;

/// Combo rows, in order. Index ↔ [`ConflictMode::ALL`].
const MODE_LABELS: [&str; 4] = [
    "Add new only — never touch what is already there",
    "Update — copy what differs, keep newer destination files",
    "Replace — copy everything, overwrite all",
    "Mirror — make the destination identical, deleting extras",
];

struct Data {
    name: HWND,
    sources: HWND,
    dest: HWND,
    mode: HWND,
    contents: HWND,
    links: HWND,
    excludes: HWND,
    tags: HWND,
    original: Job,
    taken: Vec<String>,
    result: Arc<Mutex<Option<Job>>>,
}

struct Init {
    job: Job,
    taken: Vec<String>,
    result: Arc<Mutex<Option<Job>>>,
}

/// Edit `job` (empty fields for a new one). `taken` is every *other*
/// job's name. Returns the job to save, or `None` on cancel.
pub fn edit(parent: HWND, job: Job, taken: Vec<String>, title: &str) -> Option<Job> {
    let result = Arc::new(Mutex::new(None::<Job>));
    let init = Box::into_raw(Box::new(Init {
        job,
        taken,
        result: result.clone(),
    })) as isize;
    crate::dialog::run_modal(parent, title, 345, 305, Some(dialog_proc), init);
    result.lock().take()
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let init: Init = *Box::from_raw(lp.0 as *mut Init);
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let apply_font = |h: HWND| {
                SendMessageW(
                    h,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            };
            let j = &init.job;

            apply_font(mkstatic(hwnd, "&Name:", 10, 13, 100));
            let name = mkedit(hwnd, 120, 10, 390, ID_NAME);
            apply_font(name);
            set_text(name, &j.name);

            apply_font(mkstatic(
                hwnd,
                "&Sources, one per line (a drive root like D:\\ is fine):",
                10,
                42,
                500,
            ));
            let sources = mkmultiedit(hwnd, 10, 62, 500, 92, ID_SOURCES);
            apply_font(sources);
            set_text(sources, &j.sources.join("\r\n"));

            apply_font(mkstatic(
                hwnd,
                "&Destination — a folder, remote:path, or rustic:<repository>:",
                10,
                162,
                500,
            ));
            let dest = mkedit(hwnd, 10, 182, 500, ID_DEST);
            apply_font(dest);
            set_text(dest, &j.dest);

            apply_font(mkstatic(
                hwnd,
                "&If the destination already has a file (copy jobs):",
                10,
                216,
                270,
            ));
            let selected = ConflictMode::ALL
                .iter()
                .position(|m| *m == j.mode)
                .unwrap_or(1);
            let mode = mkcombo(hwnd, 285, 212, 225, ID_MODE, &MODE_LABELS, selected);
            apply_font(mode);

            let contents = mkcheck(
                hwnd,
                "Copy each source's &contents straight into the destination (no folder per source)",
                10,
                244,
                ID_CONTENTS,
            );
            apply_font(contents);
            set_check(contents, j.contents_only);
            let links = mkcheck(
                hwnd,
                "&Keep symlinks and junctions as links",
                10,
                268,
                ID_LINKS,
            );
            apply_font(links);
            set_check(links, j.links);

            apply_font(mkstatic(
                hwnd,
                "E&xclude — names or globs, one per line, matched at any depth:",
                10,
                296,
                500,
            ));
            let excludes = mkmultiedit(hwnd, 10, 316, 500, 70, ID_EXCLUDES);
            apply_font(excludes);
            set_text(excludes, &j.excludes.join("\r\n"));

            apply_font(mkstatic(
                hwnd,
                "Rustic &tags, comma separated (rustic destinations only):",
                10,
                396,
                330,
            ));
            let tags = mkedit(hwnd, 345, 393, 165, ID_TAGS);
            apply_font(tags);
            set_text(tags, &j.tags.join(", "));

            let ok = mkbutton(hwnd, "&Save", 330, 432, 90, 28, ID_BTN_OK);
            apply_font(ok);
            let cancel = mkbutton(hwnd, "Cancel", 430, 432, 80, 28, ID_BTN_CANCEL);
            apply_font(cancel);

            let data = Box::new(Data {
                name,
                sources,
                dest,
                mode,
                contents,
                links,
                excludes,
                tags,
                original: init.job,
                taken: init.taken,
                result: init.result,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            // Returning 1 lets the dialog manager focus the first tabstop:
            // the name box.
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return 0;
            };
            match cmd {
                ID_BTN_OK => {
                    let job = read_job(d);
                    match check(&job, &d.taken) {
                        Ok(()) => {
                            *d.result.lock() = Some(job);
                            finish(hwnd, cmd);
                        }
                        Err(e) => {
                            crate::dialogs::show_error(Some(HwndSend(hwnd)), "Can't save job", &e);
                        }
                    }
                    1
                }
                ID_BTN_CANCEL => {
                    finish(hwnd, cmd);
                    1
                }
                _ => 0,
            }
        },
        _ => 0,
    }
}

/// The dialog's fields as a [`Job`]. `last_run` is carried over from the
/// job being edited: the editor never rewrites run history.
fn read_job(d: &Data) -> Job {
    let mode_idx =
        unsafe { SendMessageW(d.mode, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))) }.0;
    let mode = usize::try_from(mode_idx)
        .ok()
        .and_then(|i| ConflictMode::ALL.get(i).copied())
        .unwrap_or(d.original.mode);
    Job {
        name: get_text(d.name).trim().to_string(),
        sources: crate::jobs::split_lines(&get_text(d.sources)),
        dest: get_text(d.dest).trim().to_string(),
        mode,
        contents_only: get_check(d.contents),
        links: get_check(d.links),
        excludes: crate::jobs::split_lines(&get_text(d.excludes)),
        tags: crate::jobs::split_tags(&get_text(d.tags)),
        last_run: d.original.last_run.clone(),
    }
}

/// Everything `Job::validate` checks, plus: every path parses, a rustic
/// job's sources are local, and nothing is a path inside a backup.
pub fn check(job: &Job, taken: &[String]) -> Result<(), String> {
    job.validate(taken)?;
    for s in &job.sources {
        let p = crate::jobs::parse_path(s).map_err(|e| format!("source {e}"))?;
        if p.is_rustic() {
            return Err(format!(
                "{s} is inside a backup; copy it out with paste instead of a job"
            ));
        }
        if p.is_this_pc() || p.is_remotes_root() {
            return Err(format!("{s} is not a folder"));
        }
        if job.kind() == navigator_config::JobKind::RusticBackup && p.is_remote() {
            return Err(format!("rustic backs up local paths only; {s} is a remote"));
        }
    }
    if job.kind() == navigator_config::JobKind::RusticBackup {
        let d = crate::jobs::parse_path(&job.dest).map_err(|e| format!("destination {e}"))?;
        if !d.is_rustic_repo_root() {
            return Err(
                "a rustic destination is the repository itself (rustic:<repository>), not a snapshot inside it"
                    .into(),
            );
        }
    } else {
        let d = crate::jobs::parse_path(&job.dest).map_err(|e| format!("destination {e}"))?;
        if d.is_rustic() {
            return Err(
                "a destination inside a backup is not writable; for a rustic job use rustic:<repository>"
                    .into(),
            );
        }
        if d.is_this_pc() || d.is_remotes_root() {
            return Err("the destination is not a folder".into());
        }
    }
    Ok(())
}

fn finish(hwnd: HWND, code: u16) {
    unsafe {
        let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
        if raw != 0 {
            let _ = Box::from_raw(raw as *mut Data);
            SetWindowLongPtrW(hwnd, DWLP_USER, 0);
        }
        let _ = EndDialog(hwnd, code as isize);
    }
}

unsafe fn data<'a>(hwnd: HWND) -> Option<&'a mut Data> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, DWLP_USER) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut Data) })
    }
}

fn hinst() -> windows::Win32::Foundation::HINSTANCE {
    unsafe { GetModuleHandleW(None).unwrap().into() }
}

fn style(bits: u32) -> windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE {
    windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(bits)
}

fn mkstatic(parent: HWND, text: &str, x: i32, y: i32, w: i32) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(t.as_ptr()),
            style(WS_CHILD.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            20,
            Some(parent),
            None,
            Some(hinst()),
            None,
        )
        .unwrap()
    }
}

fn mkedit(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            style(WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0 | 0x0080 /* ES_AUTOHSCROLL */),
            x,
            y,
            w,
            24,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(hinst()),
            None,
        )
        .unwrap();
        crate::window::install_tab_nav(h);
        h
    }
}

fn mkmultiedit(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    // ES_WANTRETURN: Enter inserts a line here — one path per line is the
    // whole input model — so Save is reached with Tab, Alt+S or a click.
    let bits = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0 | WS_VSCROLL.0
        | 0x0004  /* ES_MULTILINE */
        | 0x0040  /* ES_AUTOVSCROLL */
        | 0x1000; /* ES_WANTRETURN */
    unsafe {
        let e = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            style(bits),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(hinst()),
            None,
        )
        .unwrap();
        SendMessageW(e, EM_SETLIMITTEXT, Some(WPARAM(0)), Some(LPARAM(0)));
        crate::window::install_tab_nav(e);
        e
    }
}

fn mkcombo(parent: HWND, x: i32, y: i32, w: i32, id: u16, items: &[&str], selected: usize) -> HWND {
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            w!(""),
            style(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            x,
            y,
            w,
            200,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(hinst()),
            None,
        )
        .unwrap();
        for it in items {
            let t: Vec<u16> = it.encode_utf16().chain(once(0)).collect();
            SendMessageW(
                h,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(t.as_ptr() as isize)),
            );
        }
        SendMessageW(h, CB_SETCURSEL, Some(WPARAM(selected)), Some(LPARAM(0)));
        h
    }
}

fn mkcheck(parent: HWND, text: &str, x: i32, y: i32, id: u16) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(t.as_ptr()),
            style(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
            x,
            y,
            500,
            22,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(hinst()),
            None,
        )
        .unwrap()
    }
}

fn mkbutton(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    let default = if id == ID_BTN_OK {
        0x0001 /* BS_DEFPUSHBUTTON */
    } else {
        BS_PUSHBUTTON as u32
    };
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(t.as_ptr()),
            style(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | default),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(hinst()),
            None,
        )
        .unwrap()
    }
}

fn set_text(h: HWND, s: &str) {
    let t: Vec<u16> = s.encode_utf16().chain(once(0)).collect();
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(t.as_ptr()));
    }
}

fn get_text(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(h, &mut buf);
        if got <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

fn set_check(h: HWND, on: bool) {
    unsafe {
        SendMessageW(
            h,
            BM_SETCHECK,
            Some(WPARAM(if on { 1 } else { 0 })),
            Some(LPARAM(0)),
        );
    }
}

fn get_check(h: HWND) -> bool {
    unsafe { SendMessageW(h, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(sources: &[&str], dest: &str) -> Job {
        Job {
            name: "n".into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            dest: dest.into(),
            ..Job::default()
        }
    }

    #[test]
    fn check_refuses_backup_paths_and_remote_rustic_sources() {
        assert!(check(&job(&[r"D:\a"], r"T:\b"), &[]).is_ok());
        assert!(check(&job(&[r"rustic:D:\repo › snap 1234 › C"], r"T:\b"), &[]).is_err());
        assert!(check(&job(&[r"D:\a"], r"rustic:D:\repo › snap 1234"), &[]).is_err());
        assert!(check(&job(&["mac:x"], r"rustic:D:\repo"), &[]).is_err());
        assert!(check(&job(&[r"D:\a"], r"rustic:D:\repo"), &[]).is_ok());
        assert!(check(&job(&["not absolute"], r"T:\b"), &[]).is_err());
        assert!(check(&job(&[r"D:\a"], "relative"), &[]).is_err());
        assert!(check(&job(&["mac:Downloads"], "sb:/home/vol"), &[]).is_ok());
    }
}
