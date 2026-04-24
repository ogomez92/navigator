//! Ctrl+F search dialog. Modal input with a single edit field; Enter kicks
//! off [`AppState::start_search`] from the current working directory.

use std::ffi::c_void;
use std::iter::once;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CreateWindowExW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, HCURSOR, HMENU, IDC_ARROW, IsDialogMessageW, IsWindow, LoadCursorW, MSG,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE,
    WM_COMMAND, WM_CREATE, WM_DESTROY, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, GWLP_USERDATA,
};

use crate::app::AppState;

const CLASS: PCWSTR = w!("NavigatorSearch");
const ID_EDIT: u16   = 600;
const ID_FIND: u16   = 1; // IDOK — makes Enter submit
const ID_CANCEL: u16 = 2;

struct Data {
    state: Arc<AppState>,
    edit: HWND,
}

static PARAMS: OnceCell<Mutex<Option<Arc<AppState>>>> = OnceCell::new();

pub fn open(parent: HWND, state: Arc<AppState>) {
    if let Err(e) = ensure_class() {
        tracing::error!("search class registration failed: {e:?}");
        return;
    }
    PARAMS.get_or_init(|| Mutex::new(None)).lock().replace(state);
    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h,
        Err(_) => return,
    };
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Find in folder — navigator"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT, CW_USEDEFAULT, 480, 160,
            Some(parent), None,
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
            if got <= 0 { break; }
            if IsDialogMessageW(hwnd, &mut msg).as_bool() { continue; }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = EnableWindow(parent, true);
        let _ = SetFocus(Some(parent));
    }
}

fn ensure_class() -> windows::core::Result<()> {
    static REG: OnceCell<()> = OnceCell::new();
    if REG.get().is_some() { return Ok(()); }
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
            let Some(state) = PARAMS.get().and_then(|m| m.lock().take()) else {
                return LRESULT(-1);
            };
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let apply_font = |h: HWND| {
                SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            };

            let label = mkstatic(hwnd, "Find name containing:", 10, 12, 460);
            apply_font(label);
            let edit = mkedit(hwnd, 10, 36, 450, ID_EDIT);
            apply_font(edit);
            let find = mkbutton(hwnd, "&Find", 280, 80, 80, 28, ID_FIND);
            apply_font(find);
            let cancel = mkbutton(hwnd, "Cancel", 370, 80, 90, 28, ID_CANCEL);
            apply_font(cancel);

            let data = Box::new(Data { state, edit });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);

            // Focus the edit so typing works immediately.
            let _ = SetFocus(Some(edit));
            LRESULT(0)
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
            match cmd {
                ID_FIND => {
                    let query = get_text(d.edit);
                    if !query.is_empty() {
                        if let Some(cwd) = d.state.model.cwd() {
                            d.state.start_search(cwd, query);
                        }
                    }
                    let _ = DestroyWindow(hwnd);
                }
                ID_CANCEL => { let _ = DestroyWindow(hwnd); }
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
    if raw == 0 { None } else { Some(unsafe { &mut *(raw as *mut Data) }) }
}

fn mkstatic(parent: HWND, text: &str, x: i32, y: i32, w: i32) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("STATIC"), PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            x, y, w, 20, Some(parent), None, Some(GetModuleHandleW(None).unwrap().into()), None,
        ).unwrap()
    }
}

fn mkedit(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("EDIT"), w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0,
            ),
            x, y, w, 24, Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()), None,
        ).unwrap()
    }
}

fn mkbutton(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    // The Find button is IDOK → Enter submits from the edit field.
    let default = if id == ID_FIND { 0x0001 /* BS_DEFPUSHBUTTON */ } else { BS_PUSHBUTTON as u32 };
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("BUTTON"), PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | default,
            ),
            x, y, w, h, Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()), None,
        ).unwrap()
    }
}

fn get_text(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h);
        if len <= 0 { return String::new(); }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(h, &mut buf);
        if got <= 0 { return String::new(); }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}
