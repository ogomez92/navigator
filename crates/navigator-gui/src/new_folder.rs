//! "New folder" (Ctrl+N) / "New file" (Ctrl+Shift+N) dialog. A single
//! modal input field; on OK the name is handed to
//! [`AppState::op_new_folder`] / [`AppState::op_new_file`], which create
//! the entry via rclone so remote and local endpoints work the same way.
//! The new-file path additionally opens the created file in the OS
//! default app for its extension.
//!
//! The entry is **not** created up front and then renamed (Explorer's
//! default) — if the user cancels, nothing happens at all.

use std::ffi::c_void;
use std::iter::once;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, HCURSOR, HMENU, IDC_ARROW, IsDialogMessageW, IsWindow,
    LoadCursorW, MSG, RegisterClassExW, SendMessageW, SetWindowLongPtrW, TranslateMessage,
    WINDOW_EX_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_SETFONT, WNDCLASSEXW, WS_BORDER,
    WS_CAPTION, WS_CHILD, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::app::AppState;

const CLASS: PCWSTR = w!("NavigatorNewFolder");
const ID_EDIT: u16 = 610;
const ID_OK: u16 = 1; // IDOK — Enter submits
const ID_CANCEL: u16 = 2;

/// What the dialog is asking the user to name. Drives the window title,
/// the field label, and which `AppState` op runs on commit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Folder,
    File,
}

impl Kind {
    fn title(self) -> PCWSTR {
        match self {
            Kind::Folder => w!("New folder — navigator"),
            Kind::File => w!("New file — navigator"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Folder => "Folder name:",
            Kind::File => "File name (with extension):",
        }
    }
}

struct Data {
    state: Arc<AppState>,
    kind: Kind,
    edit: HWND,
}

/// What `open_kind` hands to `WM_CREATE` through the shared cell.
type PendingOpen = (Arc<AppState>, Kind);

static PARAMS: OnceCell<Mutex<Option<PendingOpen>>> = OnceCell::new();

/// Open the "New folder" prompt (Ctrl+N).
pub fn open(parent: HWND, state: Arc<AppState>) {
    open_kind(parent, state, Kind::Folder);
}

/// Open the "New file" prompt (Ctrl+Shift+N). The created file is opened
/// in the OS default app for its extension once it exists.
pub fn open_file(parent: HWND, state: Arc<AppState>) {
    open_kind(parent, state, Kind::File);
}

fn open_kind(parent: HWND, state: Arc<AppState>, kind: Kind) {
    // Refuse to even open the dialog in contexts where nothing can be
    // created. Announces the reason so screen-reader users know why
    // nothing happened.
    if let Some(cwd) = state.model.cwd() {
        if cwd.is_this_pc() || cwd.is_remotes_root() {
            let what = match kind {
                Kind::Folder => "folder",
                Kind::File => "file",
            };
            state.say(&format!("cannot create {what} here"), true);
            return;
        }
    } else {
        return;
    }

    if let Err(e) = ensure_class() {
        tracing::error!("new folder class registration failed: {e:?}");
        return;
    }
    PARAMS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .replace((state, kind));
    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h,
        Err(_) => return,
    };
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            kind.title(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            160,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };
    modal_loop(parent, hwnd);
}

fn modal_loop(parent: HWND, hwnd: HWND) {
    unsafe {
        let _ = EnableWindow(parent, false);
        let mut msg = MSG::default();
        while IsWindow(Some(hwnd)).as_bool() {
            let got = GetMessageW(&mut msg, None, 0, 0).0;
            if got <= 0 {
                break;
            }
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = EnableWindow(parent, true);
        let _ = SetFocus(Some(parent));
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

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => unsafe {
            let Some((state, kind)) = PARAMS.get().and_then(|m| m.lock().take()) else {
                return LRESULT(-1);
            };
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let apply_font = |h: HWND| {
                SendMessageW(
                    h,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            };

            let label = mkstatic(hwnd, kind.label(), 10, 12, 460);
            apply_font(label);
            let edit = mkedit(hwnd, 10, 36, 450, ID_EDIT);
            apply_font(edit);
            let ok = mkbutton(hwnd, "&Create", 280, 80, 80, 28, ID_OK);
            apply_font(ok);
            let cancel = mkbutton(hwnd, "Cancel", 370, 80, 90, 28, ID_CANCEL);
            apply_font(cancel);

            let data = Box::new(Data { state, kind, edit });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);

            // Focus the edit so the user can type the name immediately.
            let _ = SetFocus(Some(edit));
            LRESULT(0)
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match cmd {
                ID_OK => {
                    let name = get_text(d.edit);
                    if !name.trim().is_empty() {
                        match d.kind {
                            Kind::Folder => d.state.op_new_folder(name),
                            Kind::File => d.state.op_new_file(name),
                        }
                    }
                    let _ = DestroyWindow(hwnd);
                }
                ID_CANCEL => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        },
        WM_DESTROY => unsafe {
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut Data);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
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
                WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0,
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
    // Create button is IDOK → Enter submits from the edit field.
    let default = if id == ID_OK {
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
