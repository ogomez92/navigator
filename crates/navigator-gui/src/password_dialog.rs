//! Password prompt for a rustic repository.
//!
//! Raised by [`crate::window::WMAPP_RUSTIC_PASSWORD`], which the rustic
//! driver's prompter *sends* (not posts) from whichever worker hit the
//! locked repository, so that worker blocks until the user answers. Built
//! via [`crate::dialog::run_modal`]: a real `#32770` dialog, announced as
//! one, with Enter on OK and Esc on Cancel from `DefDlgProc`.
//!
//! The password stays in memory for the rest of the session (see
//! `navigator_rclone::rustic`), so the prompt appears once per repository
//! per run — and says so, because "will it ask me again?" is the first
//! thing anyone wonders about a password box.

use std::ffi::c_void;
use std::iter::once;
use std::sync::Arc;

use parking_lot::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CreateWindowExW, EndDialog, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, HMENU, SendMessageW, SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX,
    WM_COMMAND, WM_INITDIALOG, WM_SETFONT, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

// DWLP_USER offset on a dialog window (x64: 16). See `options.rs`.
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

const ES_PASSWORD: u32 = 0x0020;
const ES_AUTOHSCROLL: u32 = 0x0080;

const ID_EDIT: u16 = 730;
const ID_BTN_OK: u16 = 1;
const ID_BTN_CANCEL: u16 = 2;

/// What `WMAPP_RUSTIC_PASSWORD` carries: the question in, the answer out.
/// Lives on the asking worker's stack for the duration of the `SendMessage`.
pub struct Request {
    pub repo: String,
    /// The previous attempt was refused.
    pub retry: bool,
    pub answer: Option<String>,
}

struct Data {
    edit: HWND,
    result: Arc<Mutex<Option<String>>>,
}

struct Init {
    repo: String,
    retry: bool,
    result: Arc<Mutex<Option<String>>>,
}

/// Ask for `repo`'s password. `None` if the user cancelled. An empty
/// password is a legitimate answer for a repository created with one.
pub fn ask(parent: HWND, repo: &str, retry: bool) -> Option<String> {
    let result = Arc::new(Mutex::new(None::<String>));
    let init = Box::into_raw(Box::new(Init {
        repo: repo.to_string(),
        retry,
        result: result.clone(),
    })) as isize;
    crate::dialog::run_modal(
        parent,
        "Backup password — navigator",
        260,
        100,
        Some(dialog_proc),
        init,
    );
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

            // The refusal goes first: it is the news, and a screen reader
            // reads the dialog's text top to bottom when it opens.
            let mut y = 10;
            if init.retry {
                apply_font(mkstatic(
                    hwnd,
                    "That password was not accepted. Try again.",
                    10,
                    y,
                    370,
                ));
                y += 22;
            }
            apply_font(mkstatic(
                hwnd,
                &format!("rustic repository {}", init.repo),
                10,
                y,
                370,
            ));
            y += 22;
            apply_font(mkstatic(
                hwnd,
                "Kept in memory until navigator closes; never saved.",
                10,
                y,
                370,
            ));
            y += 26;
            // The label directly before the edit is what MSAA names it by.
            apply_font(mkstatic(hwnd, "&Password:", 10, y + 3, 70));
            let edit = mkedit(hwnd, 85, y, 295, ID_EDIT);
            apply_font(edit);
            y += 36;
            apply_font(mkbutton(hwnd, "OK", 210, y, 80, 26, ID_BTN_OK));
            apply_font(mkbutton(hwnd, "Cancel", 300, y, 80, 26, ID_BTN_CANCEL));

            let data = Box::new(Data {
                edit,
                result: init.result,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            // Focus lands on the first tabstop, the password box.
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return 0;
            };
            match cmd {
                ID_BTN_OK => {
                    *d.result.lock() = Some(get_text(d.edit));
                    finish(hwnd, cmd);
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

/// Reclaim the per-dialog box and close. `EndDialog`, never
/// `DestroyWindow` — see the dialog conventions in CLAUDE.md.
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

fn mkstatic(parent: HWND, text: &str, x: i32, y: i32, w: i32) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            20,
            Some(parent),
            None,
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mkedit(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0
                    | WS_VISIBLE.0
                    | WS_BORDER.0
                    | WS_TABSTOP.0
                    | ES_PASSWORD
                    | ES_AUTOHSCROLL,
            ),
            x,
            y,
            w,
            24,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
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
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | default,
            ),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
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
