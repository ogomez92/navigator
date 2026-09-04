//! "Restore from backup" picker — a list of recorded backups and a
//! Restore button.
//!
//! Built via [`crate::dialog::run_modal`] so it is a real `#32770` dialog:
//! screen readers announce it as a dialog, Esc/Enter route through
//! `DefDlgProc`, and the listbox is the first tabstop so arrow keys work
//! the moment it opens. The dialog itself is the confirmation — the user
//! picks a row and presses Restore — so there is no second "are you
//! sure?" prompt (asking twice trains the user to Enter through dialogs).
//! What makes that safe is the restore flow, not the dialog: the current
//! version is staged to `.trash` before the backup is copied back.
//!
//! The list is always scoped to one folder (see
//! `backup::backups_in_dir`), and the caller passes that folder in so the
//! dialog can name it — in the caption, which a screen reader announces
//! on open, and again in the heading. A restore is a destructive write,
//! so "which folder am I about to write into?" must be answerable without
//! reading a row.

use std::ffi::c_void;
use std::iter::once;
use std::sync::Arc;

use parking_lot::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CreateWindowExW, EndDialog, GetWindowLongPtrW, HMENU, SendMessageW,
    SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG,
    WM_SETFONT, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

// DWLP_USER offset on a dialog window (x64: 16). See `options.rs`.
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

const LB_ADDSTRING: u32 = 0x0180;
const LB_SETCURSEL: u32 = 0x0186;
const LB_GETCURSEL: u32 = 0x0188;
/// Notification code (HIWORD of `wParam`) for a listbox double-click.
const LBN_DBLCLK: u16 = 2;

const ID_LIST: u16 = 730;
const ID_BTN_OK: u16 = 1; // doubles as IDOK so Enter restores
const ID_BTN_CANCEL: u16 = 2; // doubles as IDCANCEL so Esc cancels

struct Data {
    list: HWND,
    result: Arc<Mutex<Option<usize>>>,
}

struct Init {
    labels: Vec<String>,
    preselect: usize,
    folder: String,
    result: Arc<Mutex<Option<usize>>>,
}

/// Show the picker. `folder` is the directory the list was scoped to —
/// every row restores into it, and it is named in the caption and the
/// heading so that is never in doubt. Returns the index (into `labels`,
/// i.e. into the caller's entry list) of the backup to restore, or `None`
/// on cancel.
pub fn pick(parent: HWND, labels: &[String], preselect: usize, folder: &str) -> Option<usize> {
    if labels.is_empty() {
        return None;
    }
    let result = Arc::new(Mutex::new(None::<usize>));
    let init = Box::into_raw(Box::new(Init {
        labels: labels.to_vec(),
        preselect: preselect.min(labels.len() - 1),
        folder: folder.to_string(),
        result: result.clone(),
    })) as isize;
    crate::dialog::run_modal(
        parent,
        &format!("Restore from backup — {}", folder),
        345,
        270,
        Some(dialog_proc),
        init,
    );
    let taken = result.lock().take();
    taken.filter(|&i| i < labels.len())
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
                &format!("Backups taken from {} — newest first.", init.folder),
                10,
                10,
                500,
            ));
            apply_font(mkstatic(
                hwnd,
                "Restoring replaces the current version — it is moved to .trash first.",
                10,
                32,
                500,
            ));

            let list = mklistbox(hwnd, 10, 60, 500, 266, ID_LIST);
            apply_font(list);
            for label in &init.labels {
                let t: Vec<u16> = label.encode_utf16().chain(once(0)).collect();
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    Some(WPARAM(0)),
                    Some(LPARAM(t.as_ptr() as isize)),
                );
            }
            SendMessageW(
                list,
                LB_SETCURSEL,
                Some(WPARAM(init.preselect)),
                Some(LPARAM(0)),
            );

            let ok = mkbutton(hwnd, "&Restore", 330, 338, 90, 28, ID_BTN_OK);
            apply_font(ok);
            let cancel = mkbutton(hwnd, "Cancel", 430, 338, 80, 28, ID_BTN_CANCEL);
            apply_font(cancel);

            let data = Box::new(Data {
                list,
                result: init.result,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);

            // Returning 1 lets the dialog manager focus the first tabstop —
            // the listbox, so Up/Down work immediately.
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let notify = ((wp.0 >> 16) & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return 0;
            };
            // Double-clicking a row is the same gesture as Restore.
            if cmd == ID_LIST && notify == LBN_DBLCLK {
                commit(hwnd, d);
                return 1;
            }
            match cmd {
                ID_BTN_OK => {
                    commit(hwnd, d);
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

/// Record the current selection (if any) and close.
fn commit(hwnd: HWND, d: &mut Data) {
    let sel = unsafe { SendMessageW(d.list, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))) };
    if sel.0 >= 0 {
        *d.result.lock() = Some(sel.0 as usize);
    }
    finish(hwnd, ID_BTN_OK);
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

fn mklistbox(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0
        | 0x00200000 /* WS_VSCROLL */
        | 0x0001     /* LBS_NOTIFY — send LBN_* to the parent */
        | 0x0100; /* LBS_NOINTEGRALHEIGHT — keep the exact height we asked for */
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
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
