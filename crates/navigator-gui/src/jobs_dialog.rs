//! The saved-jobs list (Tools → Jobs, Ctrl+J): pick a job and Run it, or
//! New / Edit / Delete. Same shape as the backup picker — a real `#32770`
//! dialog from [`crate::dialog::run_modal`], listbox first in the tab
//! order so arrows work the moment it opens, Enter runs.
//!
//! The dialog returns one action and closes; `AppState::op_jobs` loops,
//! re-opening it after an edit or a delete with the list rebuilt, so the
//! dialog never has to mutate the config itself.

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

const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

const LB_ADDSTRING: u32 = 0x0180;
const LB_SETCURSEL: u32 = 0x0186;
const LB_GETCURSEL: u32 = 0x0188;
const LBN_DBLCLK: u16 = 2;

const ID_LIST: u16 = 750;
const ID_BTN_RUN: u16 = 1; // IDOK: Enter runs
const ID_BTN_CLOSE: u16 = 2; // IDCANCEL: Esc closes
const ID_BTN_NEW: u16 = 751;
const ID_BTN_EDIT: u16 = 752;
const ID_BTN_DELETE: u16 = 753;

/// What the user chose. Indices are into the caller's job list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobsAction {
    Run(usize),
    New,
    Edit(usize),
    Delete(usize),
}

struct Data {
    list: HWND,
    result: Arc<Mutex<Option<JobsAction>>>,
}

struct Init {
    labels: Vec<String>,
    preselect: usize,
    result: Arc<Mutex<Option<JobsAction>>>,
}

/// Show the list. `None` means closed without choosing.
pub fn pick(parent: HWND, labels: &[String], preselect: usize) -> Option<JobsAction> {
    let result = Arc::new(Mutex::new(None::<JobsAction>));
    let init = Box::into_raw(Box::new(Init {
        labels: labels.to_vec(),
        preselect: preselect.min(labels.len().saturating_sub(1)),
        result: result.clone(),
    })) as isize;
    crate::dialog::run_modal(
        parent,
        "Jobs — navigator",
        345,
        270,
        Some(dialog_proc),
        init,
    );
    let taken = result.lock().take();
    // Row-bound actions must point at a row that exists.
    match taken {
        Some(JobsAction::Run(i) | JobsAction::Edit(i) | JobsAction::Delete(i))
            if i >= labels.len() =>
        {
            None
        }
        other => other,
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

            let heading = if init.labels.is_empty() {
                "No jobs yet. New… saves the clipboard's items as a job into the current folder."
                    .to_string()
            } else {
                format!(
                    "{} saved job(s). Enter runs the selected job and shows a report when it finishes.",
                    init.labels.len()
                )
            };
            apply_font(mkstatic(hwnd, &heading, 10, 10, 500));

            let list = mklistbox(hwnd, 10, 36, 500, 290, ID_LIST);
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
            if !init.labels.is_empty() {
                SendMessageW(
                    list,
                    LB_SETCURSEL,
                    Some(WPARAM(init.preselect)),
                    Some(LPARAM(0)),
                );
            }

            apply_font(mkbutton(hwnd, "&Run", 10, 338, 80, 28, ID_BTN_RUN));
            apply_font(mkbutton(hwnd, "&New…", 100, 338, 80, 28, ID_BTN_NEW));
            apply_font(mkbutton(hwnd, "&Edit…", 190, 338, 80, 28, ID_BTN_EDIT));
            apply_font(mkbutton(hwnd, "&Delete", 280, 338, 80, 28, ID_BTN_DELETE));
            apply_font(mkbutton(hwnd, "Close", 430, 338, 80, 28, ID_BTN_CLOSE));

            let data = Box::new(Data {
                list,
                result: init.result,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let notify = ((wp.0 >> 16) & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return 0;
            };
            if cmd == ID_LIST && notify == LBN_DBLCLK {
                choose(hwnd, d, JobsAction::Run);
                return 1;
            }
            match cmd {
                ID_BTN_RUN => {
                    choose(hwnd, d, JobsAction::Run);
                    1
                }
                ID_BTN_EDIT => {
                    choose(hwnd, d, JobsAction::Edit);
                    1
                }
                ID_BTN_DELETE => {
                    choose(hwnd, d, JobsAction::Delete);
                    1
                }
                ID_BTN_NEW => {
                    *d.result.lock() = Some(JobsAction::New);
                    finish(hwnd, cmd);
                    1
                }
                ID_BTN_CLOSE => {
                    finish(hwnd, cmd);
                    1
                }
                _ => 0,
            }
        },
        _ => 0,
    }
}

/// Record `make(selected row)` and close — or do nothing when no row is
/// selected (an empty list), so Enter on an empty dialog is inert rather
/// than a panic or a phantom action.
fn choose(hwnd: HWND, d: &mut Data, make: fn(usize) -> JobsAction) {
    let sel = unsafe { SendMessageW(d.list, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))) };
    if sel.0 < 0 {
        return;
    }
    *d.result.lock() = Some(make(sel.0 as usize));
    finish(hwnd, ID_BTN_RUN);
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
        | 0x0001     /* LBS_NOTIFY */
        | 0x0100; /* LBS_NOINTEGRALHEIGHT */
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
    let default = if id == ID_BTN_RUN {
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
