//! Thin helper for building in-memory `DLGTEMPLATE`s and running real
//! modal dialogs via `DialogBoxIndirectParamW`.
//!
//! Why a real dialog and not a regular window playing dress-up? The OS
//! registers the `#32770` dialog class itself — screen readers see it
//! as `ROLE_SYSTEM_DIALOG`, keyboard traversal runs through `DefDlgProc`
//! (Tab / Shift+Tab, accelerators, default button handling, IDOK/IDCANCEL
//! on Enter/Esc), and the window gets the dialog-box look-and-feel
//! (thin caption, non-resizable frame) for free. A custom class with a
//! hand-rolled message pump only gets part of the way.
//!
//! Controls are still built programmatically in `WM_INITDIALOG`, so
//! nothing here requires a .rc file or a build-time resource step.

#![cfg(windows)]

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DLGPROC, DialogBoxIndirectParamW, SetWindowsHookExW, UnhookWindowsHookEx,
    HCBT_CREATEWND, HHOOK, WH_CBT,
};

// DLGTEMPLATE style bits. The `windows` crate exposes most of these but
// the DS_* constants live in scattered modules; defining them here keeps
// the template builder self-contained.
const WS_POPUP: u32      = 0x80000000;
const WS_CAPTION: u32    = 0x00C00000;
const WS_SYSMENU: u32    = 0x00080000;
const DS_MODALFRAME: u32 = 0x00000080;
const DS_SETFONT: u32    = 0x00000040;
const DS_CENTER: u32     = 0x00000800;
const DS_3DLOOK: u32     = 0x00000004;
const WS_EX_CONTROLPARENT: u32 = 0x00010000;

/// Run a modal dialog described by `title` + `(cx_dlu, cy_dlu)`. Controls
/// are created inside `dlg_proc` on `WM_INITDIALOG`. `init_param` is the
/// value that arrives as the `lParam` of `WM_INITDIALOG`; call sites use
/// it to hand in a `Box::into_raw(Box::new(...))` pointer for per-dialog
/// state. Returns whatever `EndDialog` was called with (typically IDOK /
/// IDCANCEL).
pub fn run_modal(
    parent: HWND,
    title: &str,
    cx_dlu: u16,
    cy_dlu: u16,
    dlg_proc: DLGPROC,
    init_param: isize,
) -> isize {
    let template = build_template(title, cx_dlu, cy_dlu);
    let hinstance = unsafe { GetModuleHandleW(None).unwrap() };
    let _hook = AnimDisableHook::install();
    unsafe {
        DialogBoxIndirectParamW(
            Some(hinstance.into()),
            template.as_ptr() as *const _,
            Some(parent),
            dlg_proc,
            LPARAM(init_param),
        )
    }
}

/// Thread-scoped WH_CBT hook that calls `perf::disable_animations` on
/// every window created on the current thread. Used to cover dialog +
/// property-sheet child hwnds the OS creates on our behalf — we never
/// see their `CreateWindowExW` to call the helper directly.
pub(crate) struct AnimDisableHook(HHOOK);

impl AnimDisableHook {
    pub(crate) fn install() -> Option<Self> {
        unsafe {
            let tid = GetCurrentThreadId();
            SetWindowsHookExW(WH_CBT, Some(cbt_proc), None, tid)
                .ok()
                .map(Self)
        }
    }
}

impl Drop for AnimDisableHook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
    }
}

unsafe extern "system" fn cbt_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HCBT_CREATEWND as i32 {
        // wParam is the HWND of the new window. It exists at this point;
        // animations are disabled before the first paint.
        let hwnd = HWND(wp.0 as *mut _);
        crate::perf::disable_animations(hwnd);
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// Build an in-memory `DLGTEMPLATE` with DS_SETFONT pointing at
/// "MS Shell Dlg" (the virtual font Windows maps to the user's shell font).
/// Build a DLGTEMPLATE suitable for a `PROPSHEETPAGEW`. Property-sheet
/// pages are child dialogs of the outer sheet; they carry no caption
/// and live inside the tab control's client area. Property sheet
/// auto-sizes the host to fit the largest page, so exact dims here only
/// matter when there's a single page.
pub fn build_propsheet_page_template(cx_dlu: u16, cy_dlu: u16) -> Vec<u8> {
    const WS_CHILD: u32    = 0x40000000;
    const DS_CONTROL: u32  = 0x00000400;

    let style = WS_CHILD | DS_3DLOOK | DS_CONTROL | DS_SETFONT;

    let mut buf: Vec<u8> = Vec::with_capacity(64);
    buf.extend_from_slice(&style.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());              // ex_style
    buf.extend_from_slice(&0u16.to_le_bytes());              // cdit = 0
    buf.extend_from_slice(&0i16.to_le_bytes());              // x
    buf.extend_from_slice(&0i16.to_le_bytes());              // y
    buf.extend_from_slice(&(cx_dlu as i16).to_le_bytes());
    buf.extend_from_slice(&(cy_dlu as i16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());              // no menu
    buf.extend_from_slice(&0u16.to_le_bytes());              // default dialog class
    buf.extend_from_slice(&0u16.to_le_bytes());              // empty title terminator

    buf.extend_from_slice(&9u16.to_le_bytes());
    for u in "MS Shell Dlg".encode_utf16() { buf.extend_from_slice(&u.to_le_bytes()); }
    buf.extend_from_slice(&0u16.to_le_bytes());

    while buf.len() % 4 != 0 { buf.push(0); }
    buf
}

fn build_template(title: &str, cx: u16, cy: u16) -> Vec<u8> {
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU
        | DS_MODALFRAME | DS_SETFONT | DS_CENTER | DS_3DLOOK;

    let mut buf: Vec<u8> = Vec::with_capacity(128);
    buf.extend_from_slice(&style.to_le_bytes());
    buf.extend_from_slice(&WS_EX_CONTROLPARENT.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());           // cdit = 0
    buf.extend_from_slice(&0i16.to_le_bytes());           // x
    buf.extend_from_slice(&0i16.to_le_bytes());           // y
    buf.extend_from_slice(&(cx as i16).to_le_bytes());    // cx
    buf.extend_from_slice(&(cy as i16).to_le_bytes());    // cy
    buf.extend_from_slice(&0u16.to_le_bytes());           // no menu
    buf.extend_from_slice(&0u16.to_le_bytes());           // default dialog class
    for u in title.encode_utf16() { buf.extend_from_slice(&u.to_le_bytes()); }
    buf.extend_from_slice(&0u16.to_le_bytes());           // title terminator

    // DS_SETFONT: point size + typeface (null-terminated UTF-16).
    buf.extend_from_slice(&9u16.to_le_bytes());
    for u in "MS Shell Dlg".encode_utf16() { buf.extend_from_slice(&u.to_le_bytes()); }
    buf.extend_from_slice(&0u16.to_le_bytes());

    while buf.len() % 4 != 0 { buf.push(0); }
    buf
}
