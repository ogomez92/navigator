//! "Compare trees" prompt — one big paste box and a Compare button.
//!
//! Built via [`crate::dialog::run_modal`] so it is a real `#32770` dialog:
//! screen readers announce it as a dialog, Tab traversal and Esc/Enter
//! come from `DefDlgProc`, and the paste box gets focus on open because
//! pasting is the only thing anyone comes here to do.
//!
//! **The edit's text limit is raised explicitly.** A multiline `EDIT`
//! defaults to a 32 KB cap on what the *user* can put in it, and an Alt+L
//! dump of a few thousand files clears that easily. Windows enforces the
//! cap by silently truncating the paste, so the failure would be a
//! comparison that quietly reports the tail of the other tree as missing.

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

/// EM_SETLIMITTEXT with `wParam = 0` sets a multiline edit to its maximum
/// (0x7FFFFFFE characters) rather than to zero.
const EM_SETLIMITTEXT: u32 = 0x00C5;

const ID_EDIT: u16 = 720;
const ID_BTN_OK: u16 = 1;
const ID_BTN_CANCEL: u16 = 2;

struct Data {
    edit: HWND,
    result: Arc<Mutex<Option<String>>>,
}

struct Init {
    here: String,
    result: Arc<Mutex<Option<String>>>,
}

/// Ask for the other tree. Returns the pasted text, or `None` if the user
/// cancelled or left the box empty.
pub fn open(parent: HWND, here: &str) -> Option<String> {
    let result = Arc::new(Mutex::new(None::<String>));
    let init = Box::into_raw(Box::new(Init {
        here: here.to_string(),
        result: result.clone(),
    })) as isize;
    crate::dialog::run_modal(
        parent,
        "Compare trees — navigator",
        345,
        270,
        Some(dialog_proc),
        init,
    );
    let text = result.lock().take()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
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

            apply_font(mkstatic(
                hwnd,
                &format!("This tree: {}", init.here),
                10,
                10,
                500,
            ));
            apply_font(mkstatic(
                hwnd,
                "Paste the other tree below — an Alt+L dump, or one path per line.",
                10,
                32,
                500,
            ));
            apply_font(mkstatic(
                hwnd,
                "A folder missing in whole is reported once, without its contents.",
                10,
                52,
                500,
            ));

            let edit = mkmultiedit(hwnd, 10, 76, 500, 250, ID_EDIT);
            apply_font(edit);

            let ok = mkbutton(hwnd, "&Compare", 330, 338, 90, 28, ID_BTN_OK);
            apply_font(ok);
            let cancel = mkbutton(hwnd, "Cancel", 430, 338, 80, 28, ID_BTN_CANCEL);
            apply_font(cancel);

            let data = Box::new(Data {
                edit,
                result: init.result,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);

            // Returning 1 lets the dialog manager set focus to the first
            // tabstop, which is the paste box.
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

fn mkmultiedit(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    // No ES_WANTRETURN: Enter belongs to the default button, so paste +
    // Enter runs the comparison. A pasted newline is unaffected — the
    // style only governs a typed VK_RETURN.
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0
        | 0x00200000 /* WS_VSCROLL */
        | 0x0004     /* ES_MULTILINE */
        | 0x0040; /* ES_AUTOVSCROLL */
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap();
        SendMessageW(h, EM_SETLIMITTEXT, Some(WPARAM(0)), Some(LPARAM(0)));
        // Multiline edits eat Tab for indentation; hand it back to the
        // dialog manager so Tab reaches the buttons.
        crate::window::install_tab_nav(h);
        h
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
