//! Read-only text viewer window.
//!
//! A single modeless top-level window with a read-only multiline `EDIT` on
//! top and a `Close` button below. Used for screens that are "here is a
//! block of text, copy what you need" — currently:
//!
//!   * Alt+Enter → file / folder properties.
//!   * Alt+L     → recursive TOML dump of the focused folder.
//!
//! Shares one class + one singleton HWND (like `progress`); the text is
//! replaced on each open rather than accumulated, and the window is
//! brought to the front with keyboard focus on the edit so screen readers
//! start reading immediately.
//!
//! Separate from `progress` on purpose: the progress window's API (post
//! log lines, cancel, done) doesn't match a one-shot "set the text" flow,
//! and conflating them would reopen the progress window every time the
//! user hit Alt+Enter.

use std::ffi::c_void;

use once_cell::sync::OnceCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
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

const IDC_EDIT: u16 = 401;
const IDC_BTN_CLOSE: u16 = 402;

const CLASS: PCWSTR = w!("NavigatorTextViewer");

struct Data {
    edit: HWND,
    btn_close: HWND,
}

/// Show `text` in the viewer, replacing whatever was there. `parent` is
/// the main application window; the viewer is modeless but owned so it
/// closes with the app. Focuses the edit control so the user can
/// immediately Ctrl+A / Ctrl+C to copy.
pub fn show(parent: HWND, title: &str, text: &str) {
    let hwnd = match ensure_window(parent) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("viewer window create failed: {e:?}");
            return;
        }
    };
    set_title(hwnd, title);
    if let Some(d) = unsafe { data(hwnd) } {
        set_edit_text(d.edit, text);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        bring_to_foreground(hwnd, d.edit);
    }
}

static SINGLETON: OnceCell<std::sync::Mutex<Option<isize>>> = OnceCell::new();

fn ensure_window(parent: HWND) -> windows::core::Result<HWND> {
    ensure_class()?;
    let hinstance = unsafe { GetModuleHandleW(None)? };

    // Singleton: reuse the existing viewer if still alive.
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
            w!("Viewer"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_SIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            720,
            520,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    let data = Box::new(build_children(hwnd));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);
    }
    *cell.lock().unwrap() = Some(hwnd.0 as isize);
    // Initial layout pass so the edit fills the client area before first
    // paint (CreateWindowExW uses the initial size; WM_SIZE fires on any
    // later resize).
    layout(hwnd);
    Ok(hwnd)
}

fn bring_to_foreground(hwnd: HWND, focus_target: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOP, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos,
    };
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(focus_target));
        // EM_SETSEL (0x00B1) with (0, -1) selects everything so the user
        // can Ctrl+C immediately. Narrator reads "selected" on focus.
        SendMessageW(focus_target, 0x00B1, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
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

fn build_children(parent: HWND) -> Data {
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    let apply_font = |h: HWND| unsafe {
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    };

    let edit = mkmulti(parent, 10, 10, 700, 440, IDC_EDIT);
    apply_font(edit);
    // Release Tab to the parent so Tab cycles edit ↔ Close instead of
    // inserting a tab character.
    crate::window::install_tab_nav(edit);
    // Multi-line EDIT swallows VK_ESCAPE — subclass turns it into
    // WM_CLOSE on the parent so Esc actually closes the window.
    crate::window::install_esc_close(edit);
    let btn_close = mkbutton(parent, "&Close", 620, 460, 80, 28, IDC_BTN_CLOSE);
    apply_font(btn_close);
    crate::window::install_esc_close(btn_close);
    Data { edit, btn_close }
}

fn layout(hwnd: HWND) {
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    let mut rc = windows::Win32::Foundation::RECT::default();
    if unsafe { GetClientRect(hwnd, &raw mut rc) }.is_err() {
        return;
    }
    let w = (rc.right - rc.left).max(0);
    let h = (rc.bottom - rc.top).max(0);
    let pad = 10;
    let btn_w = 80;
    let btn_h = 28;
    let edit_h = (h - btn_h - pad * 3).max(40);
    unsafe {
        let _ = MoveWindow(d.edit, pad, pad, (w - pad * 2).max(40), edit_h, true);
        let _ = MoveWindow(
            d.btn_close,
            (w - btn_w - pad).max(pad),
            pad + edit_h + pad,
            btn_w,
            btn_h,
            true,
        );
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
            match cmd {
                IDC_BTN_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                }
                // IDOK (1) — Enter on the default button path. Close too.
                1 => {
                    let _ = DestroyWindow(hwnd);
                }
                // IDCANCEL (2) — Esc.
                2 => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
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

unsafe fn data<'a>(hwnd: HWND) -> Option<&'a mut Data> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut Data) })
    }
}

fn set_title(hwnd: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

fn set_edit_text(edit: HWND, s: &str) {
    // EDIT expects CR-LF line breaks; LF-only leaves everything on one
    // visible row. Normalise \n to \r\n without doubling up existing \r\n.
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
        let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));
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
