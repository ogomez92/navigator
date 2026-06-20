//! Optional progress window used while rclone operations run.
//!
//! The UI thread owns the HWND. Worker threads push updates by
//! `PostMessageW` with our `WMAPP_PROGRESS_*` codes — the window proc
//! receives them serially and updates the controls.
//!
//! The window is a singleton: one active queue at a time. Closing it while
//! an operation is in flight just hides the UI; it does not cancel. The
//! Cancel button triggers a caller-supplied closure (usually
//! `OpHandle::cancel`) and disables itself.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use once_cell::sync::OnceCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    GWLP_USERDATA, GetWindowLongPtrW, HCURSOR, HMENU, IDC_ARROW, LoadCursorW, RegisterClassExW,
    SW_HIDE, SW_SHOW, SendMessageW, SetWindowLongPtrW, SetWindowTextW, ShowWindow, WINDOW_EX_STYLE,
    WM_APP, WM_COMMAND, WM_DESTROY, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

/// Messages the worker thread posts to the progress window.
pub const WMAPP_PROGRESS_STATUS: u32 = WM_APP + 100;
pub const WMAPP_PROGRESS_LOG: u32 = WM_APP + 101;
pub const WMAPP_PROGRESS_DONE: u32 = WM_APP + 102;

const IDC_LOG: u16 = 303;
const IDC_BTN_CANCEL: u16 = 304;
const IDC_BTN_CLOSE: u16 = 305;

const CLASS: PCWSTR = w!("NavigatorProgress");

/// Shared cancel callback: the worker installs one via `set_cancel`; the
/// UI thread invokes it when the user clicks Cancel. `Option` because it's
/// unset until a worker arms it; `Arc<Mutex<…>>` so handle clones share it.
type CancelSlot = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

/// Shared handle: workers call methods on this, the UI thread owns the HWND.
#[derive(Clone)]
pub struct ProgressHandle {
    hwnd: HWND,
    cancel_flag: CancelSlot,
}

// HWND is not Send but we never touch it outside of PostMessage which is
// thread-safe. Wrap the pointer.
unsafe impl Send for ProgressHandle {}
unsafe impl Sync for ProgressHandle {}

impl ProgressHandle {
    pub fn post_status(&self, current: &str, done: u64, total: u64) {
        // Payload is a heap-leaked Box<(String, u64, u64)> reclaimed by the
        // window proc.
        let payload = Box::into_raw(Box::new((current.to_string(), done, total)));
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(self.hwnd),
                WMAPP_PROGRESS_STATUS,
                WPARAM(0),
                LPARAM(payload as isize),
            );
        }
    }

    pub fn post_log(&self, line: &str) {
        let payload = Box::into_raw(Box::new(line.to_string()));
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(self.hwnd),
                WMAPP_PROGRESS_LOG,
                WPARAM(0),
                LPARAM(payload as isize),
            );
        }
    }

    pub fn post_done(&self, success: bool) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(self.hwnd),
                WMAPP_PROGRESS_DONE,
                WPARAM(if success { 1 } else { 0 }),
                LPARAM(0),
            );
        }
    }

    pub fn set_cancel<F: FnMut() + Send + 'static>(&self, f: F) {
        *self.cancel_flag.lock().unwrap() = Some(Box::new(f));
    }
}

struct Data {
    label_current: HWND,
    label_stats: HWND,
    progbar: HWND,
    log: HWND,
    btn_cancel: HWND,
    cancel_flag: CancelSlot,
    finished: bool,
}

/// Open (or reveal) the progress window. Returns a handle that worker
/// threads can post updates to.
pub fn open(parent: HWND) -> windows::core::Result<ProgressHandle> {
    ensure_class()?;
    let hinstance = unsafe { GetModuleHandleW(None)? };

    // Singleton: reuse the previous window if it's still alive.
    if let Some(h) = SINGLETON.get().and_then(|m| m.lock().unwrap().clone())
        && unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(h.hwnd)) }.as_bool()
    {
        unsafe {
            let _ = ShowWindow(h.hwnd, SW_SHOW);
        }
        bring_to_foreground(h.hwnd);
        return Ok(h);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Progress — navigator"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            560,
            460,
            Some(parent),
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    let cancel_flag = Arc::new(Mutex::new(None));
    let data = Box::new(build_children(hwnd, cancel_flag.clone()));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);
    }

    let handle = ProgressHandle { hwnd, cancel_flag };
    let _ = SINGLETON.get_or_init(|| std::sync::Mutex::new(None));
    *SINGLETON.get().unwrap().lock().unwrap() = Some(handle.clone());
    bring_to_foreground(hwnd);
    Ok(handle)
}

/// Raise the progress window to the top and land keyboard focus on the
/// log edit so screen readers start reading the stream immediately.
/// Without this the window opens *behind* the main window on reuse
/// (`SW_SHOW` alone doesn't reorder Z-order) and focus stays on whatever
/// control was last active.
fn bring_to_foreground(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOP, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos,
    };
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        // Prefer focusing the log edit (it carries live-region semantics
        // for screen readers). Fall back to the window itself if Data
        // isn't installed yet (only possible in the reuse path before
        // the initial create finishes).
        let target = if let Some(d) = data(hwnd) {
            d.log
        } else {
            hwnd
        };
        let _ = SetFocus(Some(target));
    }
}

static SINGLETON: OnceCell<std::sync::Mutex<Option<ProgressHandle>>> = OnceCell::new();

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

fn build_children(parent: HWND, cancel_flag: CancelSlot) -> Data {
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    let apply_font = |h: HWND| unsafe {
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    };

    let label_current = mkstatic(parent, "Preparing…", 10, 10, 540);
    apply_font(label_current);
    let label_stats = mkstatic(parent, "", 10, 30, 540);
    apply_font(label_stats);
    let progbar = mkprogbar(parent, 10, 55, 540, 20);
    let log = mkmulti(parent, 10, 85, 540, 300);
    apply_font(log);
    let btn_cancel = mkbutton(parent, "&Cancel", 380, 395, 80, 28, IDC_BTN_CANCEL);
    apply_font(btn_cancel);
    let btn_close = mkbutton(parent, "&Close", 470, 395, 80, 28, IDC_BTN_CLOSE);
    apply_font(btn_close);

    Data {
        label_current,
        label_stats,
        progbar,
        log,
        btn_cancel,
        cancel_flag,
        finished: false,
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WMAPP_PROGRESS_STATUS => unsafe {
            let Some(d) = data(hwnd) else {
                return LRESULT(0);
            };
            let payload: Box<(String, u64, u64)> = Box::from_raw(lp.0 as *mut _);
            let (current, done, total) = *payload;
            set_text(d.label_current, &format!("Current: {current}"));
            let stats = if total > 0 {
                format!(
                    "{} / {} ({}%)",
                    fmt_bytes(done),
                    fmt_bytes(total),
                    (done as f64 / total as f64 * 100.0) as u32
                )
            } else {
                fmt_bytes(done).to_string()
            };
            set_text(d.label_stats, &stats);
            let percent = if total > 0 {
                ((done as u128 * 100) / total as u128).min(100) as usize
            } else {
                0
            };
            // PBM_SETPOS = 0x0402
            SendMessageW(d.progbar, 0x0402, Some(WPARAM(percent)), Some(LPARAM(0)));
            LRESULT(0)
        },
        WMAPP_PROGRESS_LOG => unsafe {
            let Some(d) = data(hwnd) else {
                return LRESULT(0);
            };
            let payload: Box<String> = Box::from_raw(lp.0 as *mut _);
            append_log(d.log, &payload);
            LRESULT(0)
        },
        WMAPP_PROGRESS_DONE => unsafe {
            let Some(d) = data(hwnd) else {
                return LRESULT(0);
            };
            d.finished = true;
            set_text(
                d.label_current,
                if wp.0 == 1 {
                    "Done."
                } else {
                    "Finished with errors."
                },
            );
            // Disable Cancel since there's nothing to cancel anymore.
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(d.btn_cancel, false);
            LRESULT(0)
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let Some(d) = data(hwnd) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match cmd {
                IDC_BTN_CANCEL => {
                    if let Some(f) = d.cancel_flag.lock().unwrap().as_mut() {
                        f();
                    }
                    set_text(d.label_current, "Cancelling…");
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(
                        d.btn_cancel,
                        false,
                    );
                }
                IDC_BTN_CLOSE => {
                    let _ = ShowWindow(hwnd, SW_HIDE);
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

fn set_text(hwnd: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

fn append_log(hwnd: HWND, line: &str) {
    // EM_SETSEL = 0x00B1, EM_REPLACESEL = 0x00C2, EM_SCROLLCARET = 0x00B7
    let mut payload = String::from(line);
    if !payload.ends_with('\n') {
        payload.push_str("\r\n");
    }
    let w: Vec<u16> = payload.encode_utf16().chain([0]).collect();
    unsafe {
        // Move caret to end.
        SendMessageW(hwnd, 0x00B1, Some(WPARAM(-1i32 as usize)), Some(LPARAM(-1)));
        SendMessageW(
            hwnd,
            0x00C2,
            Some(WPARAM(0)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
        SendMessageW(hwnd, 0x00B7, Some(WPARAM(0)), Some(LPARAM(0)));
    }
}

fn fmt_bytes(n: u64) -> String {
    crate::listview::format_size(n)
}

// --- control builders -----------------------------------------------------

fn mkstatic(parent: HWND, text: &str, x: i32, y: i32, w: i32) -> HWND {
    let t: Vec<u16> = text.encode_utf16().chain([0]).collect();
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

fn mkprogbar(parent: HWND, x: i32, y: i32, w: i32, h: i32) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("msctls_progress32"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            h,
            Some(parent),
            None,
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mkmulti(parent: HWND, x: i32, y: i32, w: i32, h: i32) -> HWND {
    let style = WS_CHILD.0
        | WS_VISIBLE.0
        | WS_BORDER.0
        | WS_VSCROLL
        | ES_MULTILINE
        | ES_READONLY
        | ES_AUTOVSCROLL
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
            Some(HMENU(IDC_LOG as isize as *mut c_void)),
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
const ES_MULTILINE: u32 = 0x0004;
const ES_READONLY: u32 = 0x0800;
const ES_AUTOVSCROLL: u32 = 0x0040;
