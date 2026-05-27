//! "Recent operations" window.
//!
//! Replaces the old File → Recent operations submenu with a proper
//! modeless window: list on top, multiline read-only detail pane
//! underneath, Close button. Tab cycles list ↔ detail ↔ close ↔ list.
//! Escape closes the window.
//!
//! Enter / double-click on a list row restores that operation via
//! `AppState::op_restore_from_history` — same semantics the submenu had,
//! just with room to see what you're about to re-seed first.
//!
//! Follows the singleton pattern from `viewer` / `progress`: one HWND
//! per class, recreated on reopen after destroy.

use std::ffi::c_void;

use once_cell::sync::OnceCell;
use std::sync::Arc;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, HCURSOR, HMENU, IDC_ARROW,
    LoadCursorW, MoveWindow, RegisterClassExW, SW_SHOW, SendMessageW, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_SETFONT,
    WM_SIZE, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_OVERLAPPED, WS_SIZEBOX, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::app::AppState;
use crate::clipboard::{HistoryEntry, entry_details, entry_label, load_history};

const IDC_LIST: u16 = 501;
const IDC_DETAIL: u16 = 502;
const IDC_BTN_RESTORE: u16 = 503;
const IDC_BTN_CLOSE: u16 = 504;

const CLASS: PCWSTR = w!("NavigatorOpsHistory");

// Listbox notifications / messages we care about.
const LBN_SELCHANGE: u16 = 1;
const LBN_DBLCLK: u16 = 2;
const LB_GETCURSEL: u32 = 0x0188;
const LB_SETCURSEL: u32 = 0x0186;
const LB_RESETCONTENT: u32 = 0x0184;
const LB_ADDSTRING: u32 = 0x0180;

struct Data {
    state: Arc<AppState>,
    list: HWND,
    detail: HWND,
    btn_restore: HWND,
    btn_close: HWND,
    entries: Vec<HistoryEntry>,
}

/// Open (or raise) the ops-history window. Always refreshes the list
/// from disk so entries written by another navigator instance show up.
pub fn open(parent: HWND, state: Arc<AppState>) {
    let hwnd = match ensure_window(parent, state) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("ops-history window failed: {e:?}");
            return;
        }
    };
    refresh_entries(hwnd);
    if let Some(d) = unsafe { data(hwnd) } {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(d.list));
        }
    }
}

static SINGLETON: OnceCell<std::sync::Mutex<Option<isize>>> = OnceCell::new();

fn ensure_window(parent: HWND, state: Arc<AppState>) -> windows::core::Result<HWND> {
    ensure_class()?;
    let hinstance = unsafe { GetModuleHandleW(None)? };

    let cell = SINGLETON.get_or_init(|| std::sync::Mutex::new(None));
    if let Some(raw) = *cell.lock().unwrap() {
        let h = HWND(raw as *mut c_void);
        if unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(h)) }.as_bool() {
            return Ok(h);
        }
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Recent operations — navigator"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_SIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            720,
            560,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    let data = Box::new(build_children(hwnd, state));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);
    }
    *cell.lock().unwrap() = Some(hwnd.0 as isize);
    layout(hwnd);
    Ok(hwnd)
}

fn ensure_class() -> windows::core::Result<()> {
    static REG: OnceCell<()> = OnceCell::new();
    if REG.get().is_some() {
        return Ok(());
    }
    let hinstance = unsafe { GetModuleHandleW(None)? };
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default()),
            lpszClassName: CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
    }
    let _ = REG.set(());
    Ok(())
}

fn build_children(parent: HWND, state: Arc<AppState>) -> Data {
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    let apply_font = |h: HWND| unsafe {
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    };

    let list = mklist(parent, 10, 10, 700, 200, IDC_LIST);
    let detail = mkmulti(parent, 10, 220, 700, 260, IDC_DETAIL);
    let btn_restore = mkbutton(parent, "&Restore", 530, 490, 85, 28, IDC_BTN_RESTORE);
    let btn_close = mkbutton(parent, "&Close", 625, 490, 85, 28, IDC_BTN_CLOSE);
    apply_font(list);
    apply_font(detail);
    apply_font(btn_restore);
    apply_font(btn_close);

    // Listbox + EDIT need tab-cycling into the parent dialog order and
    // Esc-close. Without the subclasses Tab gets trapped and Esc is
    // swallowed silently.
    crate::window::install_tab_nav(list);
    crate::window::install_tab_nav(detail);
    crate::window::install_esc_close(list);
    crate::window::install_esc_close(detail);
    crate::window::install_esc_close(btn_restore);
    crate::window::install_esc_close(btn_close);

    Data {
        state,
        list,
        detail,
        btn_restore,
        btn_close,
        entries: Vec::new(),
    }
}

fn layout(hwnd: HWND) {
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    let mut rc = RECT::default();
    if unsafe { GetClientRect(hwnd, &raw mut rc) }.is_err() {
        return;
    }
    let w = (rc.right - rc.left).max(0);
    let h = (rc.bottom - rc.top).max(0);
    let pad = 10;
    let btn_w = 85;
    let btn_h = 28;

    // List gets the top third; detail the rest minus button row. Keeps
    // the relationship readable at any window size.
    let list_h = ((h - pad * 4 - btn_h) / 3).max(80);
    let detail_y = pad + list_h + pad;
    let detail_h = (h - detail_y - pad * 2 - btn_h).max(60);
    let btn_y = detail_y + detail_h + pad;
    let btn_close_x = (w - btn_w - pad).max(pad);
    let btn_restore_x = btn_close_x - btn_w - pad;

    unsafe {
        let _ = MoveWindow(d.list, pad, pad, (w - pad * 2).max(40), list_h, true);
        let _ = MoveWindow(
            d.detail,
            pad,
            detail_y,
            (w - pad * 2).max(40),
            detail_h,
            true,
        );
        let _ = MoveWindow(d.btn_restore, btn_restore_x, btn_y, btn_w, btn_h, true);
        let _ = MoveWindow(d.btn_close, btn_close_x, btn_y, btn_w, btn_h, true);
    }
}

/// Reload the history file + repopulate the list. Keeps the caret on
/// whatever row was selected before (by index) so the user's context
/// survives the refresh.
fn refresh_entries(hwnd: HWND) {
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    let prev =
        unsafe { SendMessageW(d.list, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32 };

    unsafe {
        SendMessageW(d.list, LB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    d.entries = load_history();
    if d.entries.is_empty() {
        let w: Vec<u16> = "(no recent operations)\0".encode_utf16().collect();
        unsafe {
            SendMessageW(
                d.list,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(w.as_ptr() as isize)),
            );
        }
        set_text(d.detail, "No recent operations recorded yet.");
        return;
    }
    for e in &d.entries {
        let label = entry_label(e);
        let w: Vec<u16> = label.encode_utf16().chain([0]).collect();
        unsafe {
            SendMessageW(
                d.list,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(w.as_ptr() as isize)),
            );
        }
    }
    let sel = if prev >= 0 && (prev as usize) < d.entries.len() {
        prev as usize
    } else {
        0
    };
    unsafe {
        SendMessageW(d.list, LB_SETCURSEL, Some(WPARAM(sel)), Some(LPARAM(0)));
    }
    update_detail_pane(d, sel);
}

fn update_detail_pane(d: &Data, idx: usize) {
    let text = d
        .entries
        .get(idx)
        .map(entry_details)
        .unwrap_or_else(|| "No selection.".to_string());
    set_text(d.detail, &text);
}

fn set_text(h: HWND, s: &str) {
    // EDIT wants CR-LF; same normaliser pattern as `viewer::set_edit_text`.
    let mut buf = String::with_capacity(s.len());
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            buf.push('\r');
        }
        buf.push(c);
        prev = c;
    }
    let w: Vec<u16> = buf.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let code = ((wp.0 >> 16) & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match cmd {
                IDC_LIST => {
                    match code {
                        LBN_SELCHANGE => {
                            let idx = SendMessageW(d.list, LB_GETCURSEL,
                                Some(WPARAM(0)), Some(LPARAM(0))).0 as i32;
                            if idx >= 0 { update_detail_pane(d, idx as usize); }
                        }
                        LBN_DBLCLK => {
                            restore_selected(d);
                            let _ = DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                    LRESULT(0)
                }
                IDC_BTN_RESTORE => {
                    restore_selected(d);
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                IDC_BTN_CLOSE => { let _ = DestroyWindow(hwnd); LRESULT(0) }
                1 /* IDOK */ => {
                    // Enter inside the list restores the highlighted row.
                    restore_selected(d);
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                2 /* IDCANCEL */ => { let _ = DestroyWindow(hwnd); LRESULT(0) }
                _ => LRESULT(0),
            }
        },
        WM_CLOSE => unsafe {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        },
        WM_DESTROY => unsafe {
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut Data);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            if let Some(cell) = SINGLETON.get() {
                *cell.lock().unwrap() = None;
            }
            LRESULT(0)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn restore_selected(d: &Data) {
    if d.entries.is_empty() {
        return;
    }
    let idx =
        unsafe { SendMessageW(d.list, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32 };
    if idx < 0 {
        return;
    }
    d.state.op_restore_from_history(idx as usize);
}

unsafe fn data<'a>(hwnd: HWND) -> Option<&'a mut Data> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut Data) })
    }
}

// --- control builders -----------------------------------------------------

fn mklist(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    // LBS_HASSTRINGS = 0x40, LBS_NOTIFY = 0x01, WS_VSCROLL = 0x200000
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0 | 0x0020_0000 | 0x0041;
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

fn mkmulti(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let style = WS_CHILD.0
        | WS_VISIBLE.0
        | WS_BORDER.0
        | WS_VSCROLL
        | WS_HSCROLL
        | ES_MULTILINE
        | ES_READONLY
        | ES_AUTOVSCROLL
        | ES_AUTOHSCROLL
        | WS_TABSTOP.0;
    unsafe {
        CreateWindowExW(
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
        .unwrap()
    }
}

fn mkbutton(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
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

const WS_VSCROLL: u32 = 0x00200000;
const WS_HSCROLL: u32 = 0x00100000;
const ES_MULTILINE: u32 = 0x0004;
const ES_READONLY: u32 = 0x0800;
const ES_AUTOVSCROLL: u32 = 0x0040;
const ES_AUTOHSCROLL: u32 = 0x0080;
