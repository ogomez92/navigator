//! Top-level window, menu, address bar, and ListView wiring.
//!
//! Layout (top to bottom):
//!   * address bar       — edit control with WS_TABSTOP
//!   * virtual ListView  — the main file list
//!
//! Tab order is address ↔ listview. We route messages through
//! `IsDialogMessageW` so the standard tab-stop traversal does the work —
//! no custom focus management needed.

use std::cell::RefCell;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tracing::error;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    LIST_VIEW_ITEM_STATE_FLAGS, LVFI_PARTIAL, LVFI_STRING, LVIF_TEXT, NMHDR, NMITEMACTIVATE,
    NMLISTVIEW, NMLVDISPINFOW, NMLVFINDITEMW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ACCEL, AppendMenuW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    CreateAcceleratorTableW, CreateMenu, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetClientRect, GetMessageW, GetWindowLongPtrW, HCURSOR, HICON, HMENU, IDC_ARROW,
    IDI_APPLICATION, IsDialogMessageW, LoadCursorW, LoadIconW, MF_CHECKED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, MSG, PostQuitMessage, RegisterClassExW, SendMessageW, SetMenu,
    SetWindowLongPtrW, SetWindowTextW, TranslateAcceleratorW, TranslateMessage, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_NOTIFY, WM_SETFONT,
    WM_SIZE, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use navigator_core::{Entry, EntryKind, NavPath};

use crate::app::AppState;
use crate::listview::{
    column_for_subitem, format_filetime, format_filetime_relative, format_size,
    ListView, LogicalColumn,
};

/// Directory-listed payload posted back from the scan worker.
pub const WMAPP_DIR_LISTED: u32 = WM_APP + 1;
/// Search-results payload. Carries `(root, query, entries)`.
pub const WMAPP_SEARCH_RESULTS: u32 = WM_APP + 3;
/// File-watcher notification. Carries `(root, event_kind, name)`.
pub const WMAPP_WATCH_EVENT: u32 = WM_APP + 4;
/// Redraw a single listview row. `wParam` = visible index.
pub const WMAPP_REDRAW_ROW: u32 = WM_APP + 5;
/// Directory listing failed. Carries `(path, error_message)`.
pub const WMAPP_DIR_ERROR: u32 = WM_APP + 6;
/// Rebuild the ListView's columns from current config. No payload.
pub const WMAPP_RECONFIGURE_COLUMNS: u32 = WM_APP + 7;
/// Show the text viewer with a given title + body. Payload is
/// `Box<(String, String)>` — `(title, body)`. Posted from the worker
/// threads that compute properties / tree dumps so the viewer window
/// is always created on the UI thread.
pub const WMAPP_VIEWER_SHOW: u32 = WM_APP + 8;
/// A staged remote file was modified on disk. Payload is `Box<PathBuf>`
/// (the staged local path). Handler prompts "upload back?" and, on yes,
/// spawns an rclone worker. Posted from the remote-cache watcher thread.
pub const WMAPP_REMOTE_EDIT: u32 = WM_APP + 9;

const IDC_LISTVIEW: u16 = 1001;
const IDC_ADDRESS: u16 = 1002;

const ADDRESS_BAR_HEIGHT: i32 = 26;
const CLASS_NAME: PCWSTR = w!("NavigatorMainWindow");

// Listview notification codes (declared as u32 in windows-rs; see controlids.h).
const LVN_GETDISPINFOW: u32 = 4294967119;
const LVN_ITEMCHANGED: u32 = 4294967195;
const LVN_ITEMACTIVATE_CODE: u32 = 4294967182;
const LVN_ODFINDITEMW: u32 = 4294967117;
const LVN_BEGINLABELEDITW: u32 = 4294967121;
const LVN_ENDLABELEDITW: u32 = 4294967120;
// LVN_FIRST (-100) - 15. Virtual (LVS_OWNERDATA) listviews fire this
// for range selection changes (shift-click / shift-arrow) instead of
// per-item LVN_ITEMCHANGED.
const LVN_ODSTATECHANGED: u32 = 4294967181;

#[repr(C)]
#[allow(non_snake_case)]
struct NMLVODSTATECHANGE {
    hdr: NMHDR,
    iFrom: i32,
    iTo: i32,
    uNewState: u32,
    uOldState: u32,
}

thread_local! {
    static DISP_SCRATCH: RefCell<Vec<u16>> = RefCell::new(Vec::with_capacity(512));
}

#[derive(Copy, Clone)]
pub struct HwndSend(pub HWND);
unsafe impl Send for HwndSend {}
unsafe impl Sync for HwndSend {}

pub struct Window {
    pub hwnd: HWND,
    pub listview: ListView,
    pub address: HWND,
    pub status: HWND,
}

pub struct WindowData {
    pub state: Arc<AppState>,
    pub listview: ListView,
    pub address: HWND,
    pub status: HWND,
    /// Current accelerator table. Rebuilt live when the user edits
    /// shortcut actions (see `rebuild_accels`).
    pub accel: parking_lot::Mutex<windows::Win32::UI::WindowsAndMessaging::HACCEL>,
}

static WM_CREATE_PARAMS: OnceCell<Mutex<Option<Arc<AppState>>>> = OnceCell::new();

fn ensure_class() -> windows::core::Result<()> {
    static REGISTERED: OnceCell<()> = OnceCell::new();
    if REGISTERED.get().is_some() { return Ok(()); }
    let hinstance = unsafe { GetModuleHandleW(None)? };
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default()),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or(HICON::default()),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
    }
    let _ = REGISTERED.set(());
    Ok(())
}

pub fn create(state: Arc<AppState>) -> windows::core::Result<Window> {
    ensure_class()?;
    let hinstance = unsafe { GetModuleHandleW(None)? };

    WM_CREATE_PARAMS.get_or_init(|| Mutex::new(None)).lock().replace(state.clone());

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!("navigator"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT, CW_USEDEFAULT, 1100, 700,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };

    let data_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if data_ptr == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let data = unsafe { &*(data_ptr as *const WindowData) };
    crate::perf::disable_animations(hwnd);
    crate::perf::disable_animations(data.listview.hwnd);
    crate::perf::disable_animations(data.address);
    crate::perf::disable_animations(data.status);
    Ok(Window {
        hwnd,
        listview: data.listview,
        address: data.address,
        status: data.status,
    })
}

/// Create the bottom status bar. `msctls_statusbar32` handles its own
/// resizing when forwarded WM_SIZE — we just kick it on parent resize.
fn create_status_bar(parent: HWND) -> windows::core::Result<HWND> {
    const SBARS_SIZEGRIP: u32 = 0x0100;
    let style = WS_CHILD.0 | WS_VISIBLE.0 | SBARS_SIZEGRIP;
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("msctls_statusbar32"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            0, 0, 0, 0,
            Some(parent),
            Some(HMENU(1003_isize as *mut std::ffi::c_void)),
            Some(hinstance.into()),
            None,
        )
    }
}

pub fn run_message_loop(hwnd: HWND) -> i32 {
    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0).0 };
        if got <= 0 { break; }

        unsafe {
            // Pick up the latest accelerator table each iteration so that
            // live reloads (after the shortcut editor saves) take effect
            // immediately.
            let accel = window_data(hwnd)
                .map(|d| *d.accel.lock())
                .unwrap_or_default();

            // Accelerators FIRST, dialog-manager second. The reverse
            // order lets `IsDialogMessageW` eat Ctrl+letter chords
            // (treating them as mnemonic lookups) before the accel
            // table ever sees them — that's what broke user-defined
            // Ctrl+T. Petzold's canonical pump is accel, then dialog.
            if !accel.is_invalid()
                && TranslateAcceleratorW(hwnd, accel, &msg) != 0
            {
                continue;
            }
            // Dialog-manager tab traversal + default-button handling. This
            // is why plain top-level windows get Tab between children for
            // free when their controls have WS_TABSTOP.
            if IsDialogMessageW(hwnd, &mut msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    msg.wParam.0 as i32
}

/// Rebuild the window's accelerator table from the static bindings plus
/// whatever `AppState::actions()` currently reports. Safe to call on the
/// UI thread at any time — the old table is destroyed after the new one
/// is installed so TranslateAccelerator never sees an invalid handle.
pub fn rebuild_accels(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{DestroyAcceleratorTable, HACCEL};

    let Some(data) = (unsafe { window_data(hwnd) }) else { return; };

    // NB: plain VK_BACK / VK_DELETE / VK_RETURN are NOT in this table. An
    // accelerator fires window-wide regardless of focus — if the address
    // bar has focus and the user hits Backspace to edit, a VK_BACK accel
    // steals the key and navigates up instead. Same for Delete in the
    // edit. Those keys are handled by the listview subclass so they only
    // act when the listview actually has focus. See `listview.rs`.
    //
    // All common bindings (Ctrl+C / X / V / A, F2, F5, Ctrl+H, etc.) now
    // come from `state.actions()` — `default_actions()` seeds them so a
    // fresh install still behaves like Explorer, and users can rebind any
    // of them via the shortcut editor.
    let mut accels: Vec<ACCEL> = Vec::new();
    for (i, action) in data.state.actions().iter().enumerate() {
        match chord_to_accel(&action.chord) {
            Some((vk, mods)) => {
                let cmd_id = Commands::ActionBase as u16 + i as u16;
                tracing::info!(
                    "accel: action[{}] {:?} chord={:?} -> vk=0x{:02X} fVirt=0x{:X} cmd={}",
                    i, action.name, action.chord, vk, mods.0, cmd_id,
                );
                accels.push(ACCEL { fVirt: mods, key: vk, cmd: cmd_id });
            }
            None => {
                tracing::warn!(
                    "accel: action[{}] {:?} has unparsable chord {:?} — not bound",
                    i, action.name, action.chord,
                );
            }
        }
    }

    let new_accel: HACCEL = unsafe { CreateAcceleratorTableW(&accels).unwrap_or_default() };
    tracing::info!(
        "accel table rebuilt: {} entries, handle valid={}",
        accels.len(),
        !new_accel.is_invalid(),
    );
    let mut guard = data.accel.lock();
    let old = std::mem::replace(&mut *guard, new_accel);
    drop(guard);
    if !old.is_invalid() {
        unsafe { let _ = DestroyAcceleratorTable(old); }
    }
}

#[repr(u16)]
#[derive(Clone, Copy)]
pub enum Commands {
    // Edit
    Copy = 100,
    Cut = 101,
    Paste = 102,
    SelectAll = 103,
    Refresh = 104,
    Delete = 105,
    Back = 106,
    AltUp = 107,
    Rename = 108,
    CopyPaths = 109,
    HistBack = 110,
    HistForward = 111,
    /// Open (activate) the currently focused listview item. Posted by the
    /// listview subclass when the user presses Enter — we can't rely on
    /// LVN_ITEMACTIVATE because `IsDialogMessageW` in the main loop
    /// intercepts VK_RETURN before the listview gets it.
    OpenFocused = 112,
    Undo = 113,
    CopyToClipboard = 114,
    AppendCopy = 115,
    AppendCut = 116,
    // File menu
    ToggleHidden = 120,
    ToggleSystem = 121,
    Exit = 122,
    NavigateUp = 123,
    ShowProperties = 124,
    // View menu
    SortName = 130,
    SortSize = 131,
    SortModified = 132,
    SortCreated = 133,
    SortDescending = 134,
    Search = 135,
    SortType = 136,
    FocusAddress = 137,
    // Tools menu
    Options = 140,
    Shortcuts = 141,
    RecentOpsWindow = 142,
    ConnectRemote = 143,
    NewFolder = 144,
    DumpTree = 145,
    Extract = 146,
    // Help menu
    About = 160,
    // Shortcut/Action dynamic range
    ActionBase = 0x4000,
}

// Chord → accelerator translation lives in `crate::accel` so it can be
// unit-tested without the window proc.
use crate::accel::chord_to_accel;

/// Build the main menu. Returns `menu` only — the old recent-ops
/// submenu has been replaced by a single menu entry that opens the
/// `ops_window` modeless viewer, so no HMENU needs to be stashed.
fn build_menu() -> HMENU {
    unsafe {
        let file = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(file, MF_STRING, Commands::NewFolder as usize,       w!("&New folder…\tCtrl+N"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, Commands::HistBack as usize,        w!("&Back\tAlt+Left"));
        let _ = AppendMenuW(file, MF_STRING, Commands::HistForward as usize,     w!("&Forward\tAlt+Right"));
        let _ = AppendMenuW(file, MF_STRING, Commands::NavigateUp as usize,      w!("Navigate &up\tAlt+Up"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, Commands::Refresh as usize,         w!("&Refresh\tF5"));
        let _ = AppendMenuW(file, MF_STRING, Commands::ShowProperties as usize,  w!("P&roperties\tAlt+Enter"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, Commands::RecentOpsWindow as usize, w!("Recent &operations…"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, Commands::ToggleHidden as usize,    w!("Show &hidden files\tCtrl+H"));
        let _ = AppendMenuW(file, MF_STRING, Commands::ToggleSystem as usize,    w!("Show &system files\tCtrl+Shift+H"));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, Commands::Exit as usize,            w!("E&xit\tAlt+F4"));

        let edit = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(edit, MF_STRING, Commands::Undo as usize,      w!("&Undo\tCtrl+Z"));
        let _ = AppendMenuW(edit, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(edit, MF_STRING, Commands::Cut as usize,            w!("Cu&t\tCtrl+X"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::Copy as usize,           w!("&Copy\tCtrl+C"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::CopyToClipboard as usize, w!("Copy to &OS clipboard\tAlt+C"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::CopyPaths as usize,      w!("Copy &paths\tCtrl+Shift+C"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::AppendCopy as usize,     w!("Append to copy\tCtrl+Alt+C"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::AppendCut as usize,      w!("Append to cut\tCtrl+Alt+X"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::Paste as usize,          w!("&Paste\tCtrl+V"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::Delete as usize,         w!("&Delete\tDel"));
        let _ = AppendMenuW(edit, MF_STRING, Commands::Rename as usize,         w!("Rena&me\tF2"));
        let _ = AppendMenuW(edit, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(edit, MF_STRING, Commands::SelectAll as usize,      w!("Select &all\tCtrl+A"));

        // Sort key lives under its own submenu so each key is a distinct
        // menu item (easier to hit than a radio-group scattered inline).
        // Sorting works regardless of column visibility — e.g. Sort by
        // Type is available even if the Type column is hidden from
        // Options → Columns.
        let sort = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(sort, MF_STRING, Commands::SortName as usize,     w!("&Name"));
        let _ = AppendMenuW(sort, MF_STRING, Commands::SortSize as usize,     w!("&Size"));
        let _ = AppendMenuW(sort, MF_STRING, Commands::SortType as usize,     w!("&Type"));
        let _ = AppendMenuW(sort, MF_STRING, Commands::SortModified as usize, w!("Date &modified"));
        let _ = AppendMenuW(sort, MF_STRING, Commands::SortCreated as usize,  w!("Date &created"));
        let _ = AppendMenuW(sort, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(sort, MF_STRING, Commands::SortDescending as usize, w!("&Descending order"));

        let view = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(view, MF_POPUP, sort.0 as usize, w!("&Sort by"));
        let _ = AppendMenuW(view, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(view, MF_STRING, Commands::Search as usize,        w!("&Find in folder…\tCtrl+F"));
        let _ = AppendMenuW(view, MF_STRING, Commands::FocusAddress as usize,  w!("Focus &address bar\tAlt+D"));

        let tools = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(tools, MF_STRING, Commands::ConnectRemote as usize, w!("&Connect to remote…"));
        let _ = AppendMenuW(tools, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(tools, MF_STRING, Commands::DumpTree as usize,  w!("&Dump folder tree\tAlt+L"));
        let _ = AppendMenuW(tools, MF_STRING, Commands::Extract as usize,   w!("&Extract archive(s)\tCtrl+E"));
        let _ = AppendMenuW(tools, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(tools, MF_STRING, Commands::Options as usize,   w!("&Options…"));
        let _ = AppendMenuW(tools, MF_STRING, Commands::Shortcuts as usize, w!("&Shortcuts and Actions…"));

        let help = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(help, MF_STRING, Commands::About as usize,     w!("&About…"));

        let bar = CreateMenu().unwrap();
        let _ = AppendMenuW(bar, MF_POPUP, file.0 as usize,  w!("&File"));
        let _ = AppendMenuW(bar, MF_POPUP, edit.0 as usize,  w!("&Edit"));
        // Alt+V / Alt+T are the natural mnemonics. A user who binds an
        // accelerator on those chords (via the shortcut editor) will
        // override the menu because TranslateAcceleratorW runs before the
        // menu loop gets a chance.
        let _ = AppendMenuW(bar, MF_POPUP, view.0 as usize,  w!("&View"));
        let _ = AppendMenuW(bar, MF_POPUP, tools.0 as usize, w!("&Tools"));
        let _ = AppendMenuW(bar, MF_POPUP, help.0 as usize,  w!("&Help"));
        bar
    }
}

fn create_address_bar(parent: HWND) -> windows::core::Result<HWND> {
    // ES_LEFT is 0x0000 — left alignment is the default. The previous
    // 0x0004 here was actually ES_MULTILINE (mis-commented), which both
    // wrapped the address bar visually and caused IsDialogMessageW to
    // stop routing Enter → IDOK so the user couldn't commit a typed
    // path. Single-line edit only: ES_AUTOHSCROLL keeps the caret
    // visible while typing past the right edge.
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0
        | 0x0080;  // ES_AUTOHSCROLL
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            0, 0, 0, 0,
            Some(parent),
            Some(HMENU(IDC_ADDRESS as isize as *mut std::ffi::c_void)),
            Some(hinstance.into()),
            None,
        )?;
        // Stock GUI font, so the edit uses the normal window font rather
        // than the chunky system font.
        let font = GetStockObject(DEFAULT_GUI_FONT);
        SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            hwnd, Some(tab_nav_subclass_proc), 0xB33F, 0,
        );
        Ok(hwnd)
    }
}

/// Install the tab-nav subclass on `hwnd`. Use this on any EDIT control
/// (single- or multi-line) that should release Tab to the parent's tab
/// order instead of capturing it.
pub fn install_tab_nav(hwnd: HWND) {
    unsafe {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            hwnd, Some(tab_nav_subclass_proc), 0xB33F, 0,
        );
    }
}

/// Subclass that posts `WM_CLOSE` to `hwnd`'s parent when the user hits
/// Escape. Use it on any child control that should trigger window-close
/// on Esc — regular (non-dialog) windows don't route Esc through
/// IsDialogMessageW, so without this a multiline EDIT eats the key and
/// the user is stuck with Alt+F4.
pub fn install_esc_close(hwnd: HWND) {
    unsafe {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            hwnd, Some(esc_close_subclass_proc), 0xB340, 0,
        );
    }
}

unsafe extern "system" fn esc_close_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN && wp.0 as u32 == 0x1B /* VK_ESCAPE */ {
        unsafe {
            if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(parent),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
                return LRESULT(0);
            }
        }
    }
    unsafe { windows::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wp, lp) }
}

/// Explicit Tab/Shift+Tab focus traversal for controls where the dialog
/// manager doesn't reliably do it (single-line edits that inherit input
/// focus from a listview Tab, for example). Generic — can subclass any
/// child control that should participate in parent tab-order cycling.
unsafe extern "system" fn tab_nav_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN && wp.0 as u32 == 0x09 /* VK_TAB */ {
        unsafe {
            let shift = (windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) as i32) < 0;
            if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
                if let Ok(next) = windows::Win32::UI::WindowsAndMessaging::GetNextDlgTabItem(
                    parent, Some(hwnd), shift,
                ) {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(next));
                }
            }
        }
        return LRESULT(0);
    }
    unsafe { windows::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => unsafe {
            let state = WM_CREATE_PARAMS
                .get()
                .and_then(|m| m.lock().take())
                .expect("WM_CREATE_PARAMS not populated");

            let menu = build_menu();
            SetMenu(hwnd, Some(menu)).ok();

            let address = match create_address_bar(hwnd) {
                Ok(h) => h,
                Err(e) => { error!("create_address_bar: {e:?}"); return LRESULT(-1); }
            };
            let listview = match ListView::create(hwnd, IDC_LISTVIEW, &state) {
                Ok(lv) => lv,
                Err(e) => { error!("ListView::create: {e:?}"); return LRESULT(-1); }
            };
            let status = match create_status_bar(hwnd) {
                Ok(h) => h,
                Err(e) => { error!("create_status_bar: {e:?}"); return LRESULT(-1); }
            };

            let data = Box::new(WindowData {
                state: state.clone(),
                listview,
                address,
                status,
                accel: parking_lot::Mutex::new(
                    windows::Win32::UI::WindowsAndMessaging::HACCEL::default(),
                ),
            });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);

            // Build the initial accelerator table now that `state.actions()`
            // is reachable via window_data.
            rebuild_accels(hwnd);

            sync_menu_checks(hwnd, &state);
            state.set_hwnd(hwnd);
            state.navigate(state.initial_path.clone());
            listview.focus();
            LRESULT(0)
        },

        WM_SIZE => unsafe {
            let Some(data) = window_data(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
            // Forward to the status bar so it auto-docks at the bottom.
            SendMessageW(data.status, WM_SIZE, Some(wp), Some(lp));

            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &raw mut rc);
            let w = rc.right;
            let h = rc.bottom;

            // Measure the status bar's actual height — it resizes itself
            // based on font and DPI, so we can't hard-code.
            let mut sb_rc = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(data.status, &raw mut sb_rc);
            let sb_h = (sb_rc.bottom - sb_rc.top).max(0);

            // Address bar at the top, listview in the middle, status at bottom.
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                data.address,
                None, 0, 0, w, ADDRESS_BAR_HEIGHT,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            );
            let lv_h = (h - ADDRESS_BAR_HEIGHT - sb_h).max(0);
            data.listview.resize(0, ADDRESS_BAR_HEIGHT, w, lv_h);
            LRESULT(0)
        },

        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            let Some(data) = window_data(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
            if hdr.idFrom != IDC_LISTVIEW as usize {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            handle_listview_notify(hwnd, data, hdr, lp)
        },

        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let ctrl_hwnd = HWND(lp.0 as *mut _);

            if let Some(data) = window_data(hwnd) {
                handle_command(hwnd, data, cmd, ctrl_hwnd);
            }
            LRESULT(0)
        },

        WMAPP_DIR_LISTED => unsafe {
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            let payload: Box<(NavPath, Vec<Entry>)> = Box::from_raw(lp.0 as *mut _);
            let (path, entries) = *payload;
            let count = data.state.model.set_listing(path.clone(), entries);
            data.listview.set_item_count(count);
            set_address_text(data.address, &address_display(&path));
            set_status_text(data.status, &format!("{} items", count));
            set_title_from_path(hwnd, &path);
            // Intentionally no prism announcement here — native screen
            // readers (NVDA / Narrator / JAWS) already announce the
            // listview's focused item on focus change, and the count is
            // visible in the status bar + title. Prism adding "X — N
            // items" on top was duplicate noise.
            refocus_after_up(data, &path);
            data.state.watch_cwd(&path);
            if let Some(reg) = data.state.plugin_registry() {
                reg.dispatch_navigated(&path.to_string());
            }
            LRESULT(0)
        },

        WMAPP_RECONFIGURE_COLUMNS => unsafe {
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            let cols = data.state.config.read().general.columns;
            data.listview.reconfigure_columns(&cols);
            // The set of visible columns changed; kick a refresh so the
            // virtual control re-queries the text for every visible row.
            data.state.refresh();
            LRESULT(0)
        },

        WMAPP_DIR_ERROR => unsafe {
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            let payload: Box<(NavPath, String)> = Box::from_raw(lp.0 as *mut _);
            let (path, err) = *payload;
            data.state.say(&format!("cannot open {}: {}", path.file_name(), err), true);
            crate::dialogs::show_error(
                Some(HwndSend(hwnd)),
                "Cannot open folder",
                &format!("{}\n\n{}", path, err),
            );
            // Leave the previous listing + title intact; the user can
            // correct the address bar and try again.
            LRESULT(0)
        },

        WMAPP_SEARCH_RESULTS => unsafe {
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            let payload: Box<(NavPath, String, Vec<Entry>)> = Box::from_raw(lp.0 as *mut _);
            let (root, query, entries) = *payload;
            let n = entries.len();
            let count = data.state.model.set_search_results(root.clone(), entries);
            data.listview.set_item_count(count);
            set_address_text(data.address, &format!("Search: {}", query));
            set_status_text(data.status,
                &format!("{} match{} for {:?}", n, if n == 1 { "" } else { "es" }, query));
            data.state.say(
                &format!("{} results for {}", n, query),
                true,
            );
            LRESULT(0)
        },

        WMAPP_WATCH_EVENT => unsafe {
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            let payload: Box<(NavPath, crate::watcher::WatchEvent)> = Box::from_raw(lp.0 as *mut _);
            let (root, ev) = *payload;
            data.state.on_watch_event(root, ev);
            // Refresh the visible count after the event was folded in.
            let count = data.state.model.len();
            data.listview.set_item_count(count);
            refresh_status_selection(data);
            LRESULT(0)
        },

        WMAPP_REDRAW_ROW => unsafe {
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            let idx = wp.0;
            // LVM_REDRAWITEMS = 0x1015. Bracketing the same index re-queries
            // the row via LVN_GETDISPINFO and repaints it in place —
            // cheaper than invalidating the whole listview client area.
            SendMessageW(
                data.listview.hwnd,
                0x1015,
                Some(WPARAM(idx)),
                Some(LPARAM(idx as isize)),
            );
            // The control queues the redraw; ask it to paint now.
            const LVM_UPDATE: u32 = 0x102A;
            SendMessageW(
                data.listview.hwnd,
                LVM_UPDATE,
                Some(WPARAM(idx)),
                Some(LPARAM(0)),
            );
            LRESULT(0)
        },

        WMAPP_VIEWER_SHOW => unsafe {
            let payload: Box<(String, String)> = Box::from_raw(lp.0 as *mut _);
            let (title, body) = *payload;
            crate::viewer::show(hwnd, &title, &body);
            LRESULT(0)
        },

        WMAPP_REMOTE_EDIT => unsafe {
            let payload: Box<std::path::PathBuf> = Box::from_raw(lp.0 as *mut _);
            let staged = *payload;
            let Some(data) = window_data(hwnd) else { return LRESULT(0) };
            prompt_remote_upload(hwnd, &data.state, staged);
            LRESULT(0)
        },

        WM_KEYDOWN => unsafe {
            // Enter on the listview opens the focused entry (handled globally
            // because we don't own the ListView's own key handling).
            if wp.0 as u32 == 0x0D {
                if let Some(data) = window_data(hwnd) {
                    open_focused(data);
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        },

        // When the top-level window itself receives focus — first show,
        // alt-tab back, click on the caption — punt focus to the listview
        // rather than letting Windows park it on the first tabstop
        // (address bar). The listview is what the user wants 99% of the
        // time; they can still Tab to the address bar explicitly.
        0x0007 /* WM_SETFOCUS */ => unsafe {
            if let Some(data) = window_data(hwnd) {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(
                    Some(data.listview.hwnd),
                );
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        },

        // Shift+F10 / Applications key / right-click → shell context menu.
        0x007B /* WM_CONTEXTMENU */ => unsafe {
            let Some(data) = window_data(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
            // WM_CONTEXTMENU lparam is (-1,-1) for keyboard (Shift+F10 / VK_APPS).
            let lo = sign_extend_16((lp.0 as i32) & 0xFFFF);
            let hi = sign_extend_16(((lp.0 as i32) >> 16) & 0xFFFF);
            let from_keyboard = lo == -1 && hi == -1;
            let pt = compute_context_point(data, lp);
            let paths = data.state.model.selected_paths();
            if !paths.is_empty() {
                crate::context_menu::show(hwnd, pt, &paths, from_keyboard);
            } else {
                let sel = data.state.model.selection_snapshot();
                if let (Some(idx), Some(cwd)) = (sel.focus(), data.state.model.cwd()) {
                    if let Some(e) = data.state.model.get(idx) {
                        crate::context_menu::show(hwnd, pt, &[cwd.join(&e.name)], from_keyboard);
                    }
                }
            }
            LRESULT(0)
        },

        // Submenu messages forwarded to IContextMenu2/3 so Send To, New,
        // and other owner-drawn shell submenus render. (Recent
        // operations is now its own window; no menu rebuild to do here.)
        0x0117 /* WM_INITMENUPOPUP */
        | 0x002B /* WM_DRAWITEM */
        | 0x002C /* WM_MEASUREITEM */
        | 0x0120 /* WM_MENUCHAR */ => unsafe {
            if let Some(r) = crate::context_menu::forward_menu_msg(msg, wp, lp) {
                return r;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        },

        WM_CLOSE => unsafe {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        },

        WM_DESTROY => unsafe {
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut WindowData);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        },

        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

pub unsafe fn window_data<'a>(hwnd: HWND) -> Option<&'a WindowData> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if raw == 0 { None } else { Some(unsafe { &*(raw as *const WindowData) }) }
}

unsafe fn handle_listview_notify(
    hwnd: HWND,
    data: &WindowData,
    hdr: &NMHDR,
    lp: LPARAM,
) -> LRESULT {
    match hdr.code {
        LVN_GETDISPINFOW => {
            let disp = unsafe { &mut *(lp.0 as *mut NMLVDISPINFOW) };
            fill_dispinfo(&data.state, disp);
            LRESULT(0)
        }
        LVN_ODFINDITEMW => {
            // Incremental type-ahead in a virtual list. Return the matching
            // visible index or -1. This is what Explorer does.
            let find = unsafe { &*(lp.0 as *const NMLVFINDITEMW) };
            let idx = resolve_finditem(&data.state, find);
            LRESULT(idx)
        }
        LVN_ITEMCHANGED => {
            // Mirror focus + selection state into the model. Without this
            // `op_copy`, `op_delete`, `open_focused` (Enter key) all see
            // an empty Selection and do nothing — the model is the source
            // of truth for the Operation-layer APIs.
            //
            // Native screen readers announce focus changes themselves;
            // no prism say() here.
            let nm = unsafe { &*(lp.0 as *const NMLISTVIEW) };
            mirror_item_change(&data.state, nm);
            refresh_status_selection(data);
            LRESULT(0)
        }
        LVN_ODSTATECHANGED => {
            // Virtual list range select — shift-click / shift-arrow pick
            // multiple rows in one shot. Control sends one range notification
            // instead of per-row LVN_ITEMCHANGED, so without mirroring here
            // op_copy / op_cut / op_delete see an empty Selection.
            let nm = unsafe { &*(lp.0 as *const NMLVODSTATECHANGE) };
            mirror_range_change(&data.state, nm);
            refresh_status_selection(data);
            LRESULT(0)
        }
        LVN_ITEMACTIVATE_CODE => {
            let nm = unsafe { &*(lp.0 as *const NMITEMACTIVATE) };
            activate_index(&data.state, nm.iItem as usize);
            LRESULT(0)
        }
        LVN_BEGINLABELEDITW => {
            // Default ListView label-edit selects the entire filename, so
            // typing immediately wipes the extension. For files, narrow the
            // initial selection to the stem (everything before the final
            // dot) — Explorer parity, and what users expect when F2-renaming
            // `report.txt` to keep `.txt`. Folders keep the full selection.
            let disp = unsafe { &*(lp.0 as *const NMLVDISPINFOW) };
            let idx = disp.item.iItem as usize;
            if let Some(entry) = data.state.model.get(idx) {
                if let Some(end) = rename_stem_select_end(&entry.name, entry.is_dir()) {
                    const LVM_GETEDITCONTROL: u32 = 0x1000 + 24;
                    const EM_SETSEL: u32 = 0x00B1;
                    unsafe {
                        let edit = SendMessageW(
                            data.listview.hwnd,
                            LVM_GETEDITCONTROL,
                            Some(WPARAM(0)),
                            Some(LPARAM(0)),
                        );
                        let edit_hwnd = HWND(edit.0 as *mut _);
                        if !edit_hwnd.0.is_null() {
                            SendMessageW(
                                edit_hwnd,
                                EM_SETSEL,
                                Some(WPARAM(0)),
                                Some(LPARAM(end as isize)),
                            );
                        }
                    }
                }
            }
            // Return 0 to allow the edit; anything non-zero cancels.
            LRESULT(0)
        }
        LVN_ENDLABELEDITW => {
            let disp = unsafe { &*(lp.0 as *const NMLVDISPINFOW) };
            on_end_label_edit(&data.state, disp);
            // Return TRUE to accept the inline text into the ListView's label
            // cache. We also fire the rclone rename async, so the visible
            // label will snap back on refresh if the op fails.
            LRESULT(1)
        }
        _ => unsafe { DefWindowProcW(hwnd, WM_NOTIFY, WPARAM(0), lp) },
    }
}

/// When F2 begins inline-rename, decide where the selection should end so
/// the file extension is left out. Returns the UTF-16 code-unit count of
/// the stem (the prefix to keep selected) or `None` to fall back to
/// "select all" — used for directories, names with no extension, and
/// dotfiles like `.gitignore` where the whole name is the meaningful part.
fn rename_stem_select_end(name: &str, is_dir: bool) -> Option<i32> {
    if is_dir { return None; }
    let pos = name.rfind('.')?;
    if pos == 0 { return None; }
    Some(name[..pos].encode_utf16().count() as i32)
}

fn on_end_label_edit(state: &Arc<AppState>, disp: &NMLVDISPINFOW) {
    let idx = disp.item.iItem as usize;
    let ptr = disp.item.pszText.as_ptr();
    if ptr.is_null() { return; } // user cancelled (Esc)
    let new_name = read_wstr_cstr(ptr);
    if new_name.is_empty() { return; }
    let Some(entry) = state.model.get(idx) else { return; };
    if new_name == entry.name { return; }
    state.op_rename(&entry.name, &new_name);
}


/// Resolve an `LVN_ODFINDITEMW` message into an index. The control passes
/// us the accumulated prefix the user has typed and a starting index; we
/// search visible entries for the first case-insensitive prefix match.
fn resolve_finditem(state: &Arc<AppState>, find: &NMLVFINDITEMW) -> isize {
    // Only string-based searches (flags may also include LVFI_PARTIAL,
    // LVFI_WRAP). Anything else we punt to the default.
    let flags = find.lvfi.flags;
    if !(flags.contains(LVFI_STRING) || flags.contains(LVFI_PARTIAL)) {
        return -1;
    }
    let ptr = find.lvfi.psz.as_ptr();
    if ptr.is_null() { return -1; }
    let prefix = read_wstr_cstr(ptr);
    if prefix.is_empty() { return -1; }

    let from = if find.iStart >= 0 { Some(find.iStart as usize) } else { None };
    match state.model.find_prefix(&prefix, from) {
        Some(i) => i as isize,
        None => -1,
    }
}

fn read_wstr_cstr(p: *const u16) -> String {
    if p.is_null() { return String::new(); }
    unsafe {
        let mut len = 0usize;
        while *p.add(len) != 0 { len += 1; if len > 4096 { break; } }
        let slice = std::slice::from_raw_parts(p, len);
        String::from_utf16_lossy(slice)
    }
}

fn fill_dispinfo(state: &Arc<AppState>, disp: &mut NMLVDISPINFOW) {
    if (disp.item.mask & LVIF_TEXT).0 == 0 { return; }
    let idx = disp.item.iItem as usize;
    let sub = disp.item.iSubItem;
    let Some(entry) = state.model.get(idx) else { return; };

    let (relative, cols) = {
        let g = state.config.read();
        (g.general.show_relative_dates, g.general.columns)
    };
    let text: String = match column_for_subitem(&cols, sub) {
        Some(LogicalColumn::Name) => entry.name.clone(),
        Some(LogicalColumn::Size) => if entry.is_dir() { String::new() } else { format_size(entry.size) },
        Some(LogicalColumn::Type) => kind_label(&entry),
        Some(LogicalColumn::Modified) => if relative { format_filetime_relative(entry.modified.0) }
                                         else         { format_filetime(entry.modified.0) },
        None => String::new(),
    };

    DISP_SCRATCH.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.extend(text.encode_utf16());
        buf.push(0);
        let max = disp.item.cchTextMax as usize;
        let copy = buf.len().min(max);
        if copy > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(buf.as_ptr(), disp.item.pszText.as_ptr(), copy);
            }
        }
    });
}

fn kind_label(e: &Entry) -> String {
    match e.kind {
        EntryKind::Directory => "Folder".into(),
        EntryKind::Symlink => "Link".into(),
        EntryKind::Other => "Other".into(),
        EntryKind::File => {
            let ext = std::path::Path::new(&e.name).extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext.is_empty() { "File".into() } else { format!("{} file", ext.to_uppercase()) }
        }
    }
}

fn activate_index(state: &Arc<AppState>, idx: usize) {
    let Some(entry) = state.model.get(idx) else { return; };
    let Some(cwd) = state.model.cwd() else { return; };
    if cwd.is_this_pc() {
        // ThisPC view: entries carry a display string like
        // "Local Disk (C:)". Parse out the drive letter and navigate to
        // its root. If parsing fails (e.g. an unrecognised entry), we
        // quietly bail rather than silently opening the wrong path.
        if let Some(drive_root) = navigator_fs::drive_path_from_display(&entry.name) {
            if let Ok(p) = navigator_core::NavPath::new(drive_root) {
                state.navigate(p);
            }
        }
        return;
    }
    if entry.is_dir() {
        state.navigate(cwd.join(&entry.name));
    } else {
        state.open_file(cwd.join(&entry.name));
    }
}

/// Derive the screen-point to anchor the context menu at. Keyboard
/// invocation sends `lp = (-1, -1)` — in that case we use the focused
/// listview item's screen rect so the menu pops exactly next to the caret.
fn compute_context_point(data: &WindowData, lp: LPARAM) -> POINT {
    let lo = (lp.0 as i32) & 0xFFFF;
    let hi = ((lp.0 as i32) >> 16) & 0xFFFF;
    let x = sign_extend_16(lo);
    let y = sign_extend_16(hi);
    if x == -1 && y == -1 {
        // LVM_GETITEMRECT = 0x100E, with WPARAM = item, LPARAM = RECT*.
        // LVIR_SELECTBOUNDS = 3 — bounds of the selection rectangle.
        if let Some(idx) = data.state.model.selection_snapshot().focus() {
            let mut rc = windows::Win32::Foundation::RECT {
                left: 3, top: 0, right: 0, bottom: 0,
            };
            unsafe {
                SendMessageW(
                    data.listview.hwnd,
                    0x100E,
                    Some(WPARAM(idx)),
                    Some(LPARAM(&raw mut rc as isize)),
                );
                let mut p = POINT { x: rc.left, y: rc.bottom };
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(data.listview.hwnd, &mut p);
                return p;
            }
        }
    }
    POINT { x, y }
}

fn sign_extend_16(v: i32) -> i32 {
    let v = v & 0xFFFF;
    if v & 0x8000 != 0 { v | !0xFFFF } else { v }
}

/// Ask the user whether to upload `staged` back to its origin remote.
/// Called on the UI thread from the WMAPP_REMOTE_EDIT arm so the
/// MessageBox gets a real parent hwnd and can be announced properly.
fn prompt_remote_upload(hwnd: HWND, state: &Arc<crate::app::AppState>, staged: std::path::PathBuf) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, MessageBoxW, MB_DEFBUTTON1, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO, IDYES,
    };
    use windows::core::PCWSTR;

    let Some(remote) = state.remote_cache.remote_for(&staged) else {
        // Record gone — nothing to do.
        return;
    };
    let remote_display = remote.rclone_arg().unwrap_or_else(|| remote.to_string());
    let body = format!(
        "The file you opened from {} was modified.\n\n\
         Upload the changes back to the remote?",
        remote_display,
    );
    let title_w: Vec<u16> = "Upload to remote?".encode_utf16().chain([0]).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain([0]).collect();
    // If navigator already holds foreground, bring the dialog up front
    // so the user can act on it immediately. If another app is active
    // (e.g. the editor that triggered the save), stay passive — the
    // prompt will surface once the user alt-tabs back. Never steal.
    let is_foreground = unsafe { GetForegroundWindow() } == hwnd;
    let mut flags = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON1;
    if is_foreground { flags |= MB_SETFOREGROUND; }
    let rc = unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            flags,
        ).0
    };
    if rc == IDYES.0 {
        state.op_remote_upload(staged, remote);
    } else {
        // Re-baseline so we don't re-prompt immediately on the same save;
        // next save bumps mtime and fires again as expected.
        let mtime = staged.metadata().ok().and_then(|m| m.modified().ok());
        state.remote_cache.finish_prompt(&staged, mtime);
    }
}

fn open_focused(data: &WindowData) {
    let sel = data.state.model.selection_snapshot();
    let Some(idx) = sel.focus() else { return; };
    activate_index(&data.state, idx);
}

fn handle_command(hwnd: HWND, data: &WindowData, cmd: u16, ctrl: HWND) {
    match cmd {
        x if x == Commands::Copy as u16 => data.state.op_copy(),
        x if x == Commands::CopyPaths as u16 => data.state.op_copy_paths(),
        x if x == Commands::Cut as u16 => data.state.op_cut(),
        x if x == Commands::Paste as u16 => data.state.op_paste(),
        x if x == Commands::SelectAll as u16 => select_all(data),
        x if x == Commands::Refresh as u16 => data.state.refresh(),
        x if x == Commands::Delete as u16 => data.state.op_delete(),
        x if x == Commands::Back as u16 || x == Commands::AltUp as u16 => data.state.navigate_up(),
        x if x == Commands::HistBack as u16 => data.state.go_back(),
        x if x == Commands::HistForward as u16 => data.state.go_forward(),
        x if x == Commands::Undo as u16 => data.state.op_undo(),
        x if x == Commands::OpenFocused as u16 => open_focused(data),
        x if x == Commands::Rename as u16 => begin_rename(data),
        x if x == Commands::CopyToClipboard as u16 => data.state.op_copy_to_clipboard(),
        x if x == Commands::AppendCopy as u16 => data.state.op_append_clipboard(false),
        x if x == Commands::AppendCut as u16 => data.state.op_append_clipboard(true),
        x if x == Commands::NavigateUp as u16 => data.state.navigate_up(),
        x if x == Commands::ShowProperties as u16 => data.state.op_show_properties(),
        x if x == Commands::DumpTree as u16 => data.state.op_dump_tree(),
        x if x == Commands::Extract as u16 => data.state.op_extract(),
        x if x == Commands::FocusAddress as u16 => focus_address(data),
        x if x == Commands::ToggleHidden as u16 => {
            data.state.toggle_hidden();
            sync_menu_checks(hwnd, &data.state);
        }
        x if x == Commands::ToggleSystem as u16 => {
            data.state.toggle_system();
            sync_menu_checks(hwnd, &data.state);
        }
        x if x == Commands::SortName as u16 => { data.state.set_sort_mode(navigator_config::SortMode::Name); sync_menu_checks(hwnd, &data.state); }
        x if x == Commands::SortSize as u16 => { data.state.set_sort_mode(navigator_config::SortMode::Size); sync_menu_checks(hwnd, &data.state); }
        x if x == Commands::SortType as u16 => { data.state.set_sort_mode(navigator_config::SortMode::Type); sync_menu_checks(hwnd, &data.state); }
        x if x == Commands::SortModified as u16 => { data.state.set_sort_mode(navigator_config::SortMode::Modified); sync_menu_checks(hwnd, &data.state); }
        x if x == Commands::SortCreated as u16 => { data.state.set_sort_mode(navigator_config::SortMode::Created); sync_menu_checks(hwnd, &data.state); }
        x if x == Commands::SortDescending as u16 => { data.state.toggle_sort_descending(); sync_menu_checks(hwnd, &data.state); }
        x if x == Commands::Search as u16 => { crate::search::open(hwnd, data.state.clone()); }
        x if x == Commands::Exit as u16 => unsafe { let _ = DestroyWindow(hwnd); },
        x if x == Commands::About as u16 => {
            data.state.say("navigator 0.1 — accessible file explorer", false);
        }
        x if x == Commands::Options as u16 => {
            if let Err(e) = crate::options::open(hwnd, data.state.clone()) {
                crate::dialogs::show_error(
                    Some(HwndSend(hwnd)),
                    "Options failed to open",
                    &e.to_string(),
                );
            }
        }
        x if x == Commands::Shortcuts as u16 => {
            if let Err(e) = crate::shortcut_editor::open(hwnd, data.state.clone()) {
                crate::dialogs::show_error(
                    Some(HwndSend(hwnd)),
                    "Shortcut editor failed to open",
                    &e.to_string(),
                );
            }
        }
        x if x == Commands::RecentOpsWindow as u16 => {
            crate::ops_window::open(hwnd, data.state.clone());
        }
        x if x == Commands::ConnectRemote as u16 => {
            // Drop the user into the Remotes virtual root. The scan
            // worker calls `rclone listremotes` from there, and each
            // entry opens as a remote root on activation — no extra
            // dialog needed.
            data.state.navigate(navigator_core::NavPath::remotes_root());
        }
        x if x == Commands::NewFolder as u16 => {
            crate::new_folder::open(hwnd, data.state.clone());
        }
        x if (x >= Commands::ActionBase as u16) => {
            // Shortcut action. ID = ActionBase + index into `state.actions()`.
            let idx = (x - Commands::ActionBase as u16) as usize;
            let actions = data.state.actions();
            tracing::info!("action dispatch: cmd={} idx={} total_actions={}", x, idx, actions.len());
            if let Some(action) = actions.get(idx) {
                if let Some(ic) = action.internal {
                    dispatch_internal(hwnd, data, ic);
                } else {
                    data.state.run_action(action);
                }
            } else {
                tracing::warn!("action index {} out of range", idx);
            }
        }
        _ => {
            // VK_RETURN with no default button routes here via IsDialogMessageW
            // as WM_COMMAND(IDOK). Route by focus: listview → open item,
            // address → navigate. Without this the address bar handler
            // fired regardless of focus, so Enter on a listview row
            // re-navigated to whatever text was in the address bar.
            //
            // Edit-control change notifications (EN_CHANGE / EN_UPDATE)
            // also arrive here as WM_COMMAND from `data.address`. We
            // intentionally ignore them — navigation only happens on
            // Enter so the user can type a path in peace without the
            // listing thrashing on every keystroke.
            if cmd == 1 /* IDOK */ {
                let focus = unsafe {
                    windows::Win32::UI::Input::KeyboardAndMouse::GetFocus()
                };
                if focus == data.listview.hwnd {
                    open_focused(data);
                } else if focus == data.address {
                    navigate_from_address(data);
                }
            }
            let _ = ctrl;
        }
    }
}

fn navigate_from_address(data: &WindowData) {
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    let text = get_window_text(data.address);
    if text.is_empty() { return; }
    let pb = std::path::PathBuf::from(&text);
    match navigator_core::NavPath::new(pb) {
        Ok(p) => {
            data.state.navigate(p);
            // Punt focus back to the listview so the user can keyboard-
            // navigate the new listing immediately. The actual scan is
            // async; if it errors, WMAPP_DIR_ERROR shows a dialog and
            // focus is no worse than wherever the user landed before.
            unsafe { let _ = SetFocus(Some(data.listview.hwnd)); }
        }
        Err(_) => data.state.say("path is not absolute", true),
    }
}

fn get_window_text(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 { return String::new(); }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(hwnd, &mut buf);
        if got <= 0 { return String::new(); }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

/// User-facing string for the address bar. Real paths display as-is;
/// remote paths display in rclone CLI form (`remote:sub/path`) rather
/// than the internal `\\?\NavigatorRemote\...` encoding.
fn address_display(path: &NavPath) -> String {
    if path.is_remotes_root() {
        "Remotes".to_string()
    } else if path.is_remote() {
        path.rclone_arg().unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    }
}

fn set_address_text(hwnd: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe { let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr())); }
}

/// Update the main window title to `"<folder> — navigator"`. For drive
/// roots (`C:\`) and the This PC sentinel, `file_name()` is empty; fall
/// back to the full path (or "This PC") so the title is never blank.
fn set_title_from_path(hwnd: HWND, path: &NavPath) {
    let label = if path.is_this_pc() {
        "This PC".to_string()
    } else if path.is_remotes_root() {
        "Remotes".to_string()
    } else if path.is_remote() {
        // Show `remote:sub/path` in the title so the user always sees
        // where they are even when the file name is empty (remote root).
        path.rclone_arg().unwrap_or_else(|| path.to_string())
    } else {
        let name = path.file_name();
        if name.is_empty() { path.to_string() } else { name.to_string() }
    };
    let title = format!("{} — navigator", label);
    let w: Vec<u16> = title.encode_utf16().chain([0]).collect();
    unsafe { let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr())); }
}

/// Set the status-bar's first part to `s`. The status bar uses its own
/// message protocol (`SB_SETTEXTW`) rather than SetWindowTextW because it
/// supports multiple "parts" with independent text.
fn set_status_text(status: HWND, s: &str) {
    const SB_SETTEXTW: u32 = 0x040B;
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe {
        // wParam low word = part index; 0 is the single default part.
        SendMessageW(status, SB_SETTEXTW, Some(WPARAM(0)), Some(LPARAM(w.as_ptr() as isize)));
    }
}

/// Re-count the selection and reflect it in the status bar. Cheap —
/// `Selection::len` is O(1); `selected_paths` is O(n) but bounded by the
/// user's actual selection count.
/// Fold one `LVN_ITEMCHANGED` notification into the model's `Selection`.
/// The listview sends `iItem = -1` when the change affects every row at
/// once (e.g. `LVM_SETITEMSTATE` with wParam = -1 for Select All).
fn mirror_item_change(state: &Arc<AppState>, nm: &NMLISTVIEW) {
    const LVIS_FOCUSED: u32 = 0x0001;
    const LVIS_SELECTED: u32 = 0x0002;
    let old = nm.uOldState;
    let new = nm.uNewState;
    // Pre-read length outside the write lock to avoid a parking_lot
    // re-entrance (model.len() also takes a lock).
    let total = state.model.len();
    let idx = nm.iItem;
    state.model.with_selection(|sel| {
        if idx < 0 {
            if new & LVIS_SELECTED != 0 {
                if total > 0 { sel.set_single(0); sel.extend_to(total - 1); }
            } else if old & LVIS_SELECTED != 0 {
                sel.clear();
            }
            return;
        }
        let i = idx as usize;
        if new & LVIS_FOCUSED != 0 && old & LVIS_FOCUSED == 0 {
            sel.set_focus(Some(i));
        }
        let was = old & LVIS_SELECTED != 0;
        let is  = new & LVIS_SELECTED != 0;
        if is && !was { sel.insert(i); }
        else if was && !is { sel.remove(i); }
    });
}

/// Fold one `LVN_ODSTATECHANGED` (virtual range select) into the model's
/// `Selection`. Range is inclusive; `iFrom`/`iTo` may arrive in any order.
fn mirror_range_change(state: &Arc<AppState>, nm: &NMLVODSTATECHANGE) {
    const LVIS_SELECTED: u32 = 0x0002;
    let was = nm.uOldState & LVIS_SELECTED != 0;
    let is  = nm.uNewState & LVIS_SELECTED != 0;
    if was == is { return; }
    if nm.iFrom < 0 || nm.iTo < 0 { return; }
    let (lo, hi) = if nm.iFrom <= nm.iTo { (nm.iFrom, nm.iTo) } else { (nm.iTo, nm.iFrom) };
    state.model.with_selection(|sel| {
        for i in (lo as usize)..=(hi as usize) {
            if is { sel.insert(i); } else { sel.remove(i); }
        }
    });
}

fn refresh_status_selection(data: &WindowData) {
    let total = data.state.model.len();
    let sel = data.state.model.selection_snapshot();
    let n = sel.len();
    let text = if n == 0 {
        format!("{} items", total)
    } else {
        format!("{} of {} selected", n, total)
    };
    set_status_text(data.status, &text);
}

fn sync_menu_checks(hwnd: HWND, state: &Arc<AppState>) {
    use windows::Win32::UI::WindowsAndMessaging::{CheckMenuItem, GetMenu, MF_BYCOMMAND};
    let filter = state.model.filter();
    let sort = state.model.sort();
    unsafe {
        let menu = GetMenu(hwnd);
        if menu.is_invalid() { return; }
        let hidden = if filter.show_hidden { MF_CHECKED } else { MF_UNCHECKED };
        let system = if filter.show_system { MF_CHECKED } else { MF_UNCHECKED };
        CheckMenuItem(menu, Commands::ToggleHidden as u32, (MF_BYCOMMAND | hidden).0);
        CheckMenuItem(menu, Commands::ToggleSystem as u32, (MF_BYCOMMAND | system).0);

        // Sort keys — set exactly one to MF_CHECKED. We don't use
        // CheckMenuRadioItem because the command IDs are non-contiguous
        // after adding SortType, and the radio-range API requires a
        // contiguous block of IDs.
        let all: [(Commands, navigator_config::SortMode); 5] = [
            (Commands::SortName,     navigator_config::SortMode::Name),
            (Commands::SortSize,     navigator_config::SortMode::Size),
            (Commands::SortType,     navigator_config::SortMode::Type),
            (Commands::SortModified, navigator_config::SortMode::Modified),
            (Commands::SortCreated,  navigator_config::SortMode::Created),
        ];
        for (cmd, mode) in all {
            let flag = if sort.mode == mode { MF_CHECKED } else { MF_UNCHECKED };
            CheckMenuItem(menu, cmd as u32, (MF_BYCOMMAND | flag).0);
        }
        let desc = if sort.descending { MF_CHECKED } else { MF_UNCHECKED };
        CheckMenuItem(menu, Commands::SortDescending as u32, (MF_BYCOMMAND | desc).0);
    }
}

/// Run a built-in UI command bound to a user shortcut. Matches the
/// `Commands::*` arms in `handle_command` but is reachable from the
/// action dispatcher so the same operations are available through both
/// paths. Menu checks stay in sync for the toggles.
fn dispatch_internal(hwnd: HWND, data: &WindowData, ic: navigator_config::InternalCommand) {
    use navigator_config::InternalCommand as IC;
    let state = &data.state;
    match ic {
        IC::Copy         => state.op_copy(),
        IC::Cut          => state.op_cut(),
        IC::AppendCopy   => state.op_append_clipboard(false),
        IC::AppendCut    => state.op_append_clipboard(true),
        IC::Paste        => state.op_paste(),
        IC::CopyPaths    => state.op_copy_paths(),
        IC::CopyToClipboard => state.op_copy_to_clipboard(),
        IC::Delete       => state.op_delete(),
        IC::Rename       => begin_rename(data),
        IC::SelectAll    => select_all(data),
        IC::Refresh      => state.refresh(),
        IC::ToggleHidden => { state.toggle_hidden(); sync_menu_checks(hwnd, state); }
        IC::ToggleSystem => { state.toggle_system(); sync_menu_checks(hwnd, state); }
        IC::Search       => { crate::search::open(hwnd, state.clone()); }
        IC::NavigateUp   => state.navigate_up(),
        IC::HistBack     => state.go_back(),
        IC::HistForward  => state.go_forward(),
        IC::Undo         => state.op_undo(),
        IC::ShowProperties => state.op_show_properties(),
        IC::DumpTree     => state.op_dump_tree(),
        IC::Extract      => state.op_extract(),
        IC::NewFolder    => { crate::new_folder::open(hwnd, state.clone()); }
        IC::FocusAddress => focus_address(data),
        other => {
            if let Some(slot) = other.hotspot_goto_slot() {
                state.hotspot_goto(slot);
            } else if let Some(slot) = other.hotspot_set_slot() {
                state.hotspot_set(slot);
            } else {
                tracing::warn!("dispatch_internal: unhandled InternalCommand {:?}", other);
            }
        }
    }
}

/// Move focus to the address bar and select all of its current text so
/// the user can immediately overtype. Mirrors Alt+D in Explorer / browsers.
fn focus_address(data: &WindowData) {
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    // EM_SETSEL = 0x00B1, wParam = start, lParam = end. (-1, -1) selects
    // all in some contexts; (0, -1) is the documented "select everything"
    // form for edit controls.
    const EM_SETSEL: u32 = 0x00B1;
    unsafe {
        let _ = SetFocus(Some(data.address));
        SendMessageW(
            data.address,
            EM_SETSEL,
            Some(WPARAM(0)),
            Some(LPARAM(-1)),
        );
    }
}

fn begin_rename(data: &WindowData) {
    use windows::Win32::UI::Controls::LVM_EDITLABELW;
    let sel = data.state.model.selection_snapshot();
    let Some(idx) = sel.focus() else { data.state.say("no item focused", false); return; };
    unsafe {
        SendMessageW(
            data.listview.hwnd,
            LVM_EDITLABELW,
            Some(WPARAM(idx)),
            Some(LPARAM(0)),
        );
    }
}

/// After a directory listing arrives, land focus somewhere sensible. If
/// the navigation carried a `pending_focus` target (set by `navigate_up`
/// or by `jump_to` for hotspots), focus that row by name. Otherwise —
/// this is a forward navigation, e.g. Enter on a folder — default-select
/// the first row so keyboard users aren't stranded with focus on an
/// invisible nothing. For real directories the match is by filename; for
/// drive roots landing on the This PC virtual view, we invert
/// `drive_path_from_display` to map a listing entry back to its root
/// path and compare.
fn refocus_after_up(data: &WindowData, cwd: &NavPath) {
    let pending = data.state.take_pending_focus();

    if let Some(child) = pending {
        let target_idx = if cwd.is_this_pc() {
            let child_str = child.to_string();
            data.state.model.index_of(|e| {
                navigator_fs::drive_path_from_display(&e.name)
                    .map(|s| s == child_str)
                    .unwrap_or(false)
            })
        } else {
            let child_name = child.file_name().to_string();
            if child_name.is_empty() {
                None
            } else {
                data.state.model.index_of(|e| e.name == child_name)
            }
        };

        if let Some(idx) = target_idx {
            select_row(data.listview.hwnd, idx);
            return;
        }
        // Named target not found (deleted, filtered out, etc.) — fall
        // through to row-0 default rather than leaving focus nowhere.
    }

    if data.state.model.len() > 0 {
        select_row(data.listview.hwnd, 0);
    }
}

/// Focus + single-select the row at `idx` in the listview, scrolling it
/// into view. Clears any previous selection first.
fn select_row(lv: HWND, idx: usize) {
    use windows::Win32::UI::Controls::{LVITEMW, LVM_ENSUREVISIBLE, LVM_SETITEMSTATE};
    // The `windows` crate exposes LVIS_* as a newtype without BitOr, so
    // combine the raw bits ourselves. LVIS_FOCUSED = 0x1, LVIS_SELECTED = 0x2.
    const SEL_FOCUS: LIST_VIEW_ITEM_STATE_FLAGS = LIST_VIEW_ITEM_STATE_FLAGS(0x0003);
    unsafe {
        // Clear all selection + focus first; otherwise multi-select sticky
        // state can leave the previous selection alive on top of ours.
        let mut clear: LVITEMW = std::mem::zeroed();
        clear.state = LIST_VIEW_ITEM_STATE_FLAGS(0);
        clear.stateMask = SEL_FOCUS;
        SendMessageW(
            lv,
            LVM_SETITEMSTATE,
            Some(WPARAM(usize::MAX)),
            Some(LPARAM(&raw const clear as isize)),
        );
        let mut item: LVITEMW = std::mem::zeroed();
        item.state = SEL_FOCUS;
        item.stateMask = SEL_FOCUS;
        SendMessageW(
            lv,
            LVM_SETITEMSTATE,
            Some(WPARAM(idx)),
            Some(LPARAM(&raw const item as isize)),
        );
        SendMessageW(
            lv,
            LVM_ENSUREVISIBLE,
            Some(WPARAM(idx)),
            Some(LPARAM(0)),
        );
    }
}

fn select_all(data: &WindowData) {
    use windows::Win32::UI::Controls::{LVITEMW, LVIS_SELECTED, LVM_SETITEMSTATE};
    const LVIS_MASK: LIST_VIEW_ITEM_STATE_FLAGS = LIST_VIEW_ITEM_STATE_FLAGS(0x000F);
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.state = LVIS_SELECTED;
    item.stateMask = LVIS_MASK;
    unsafe {
        SendMessageW(
            data.listview.hwnd,
            LVM_SETITEMSTATE,
            Some(WPARAM(usize::MAX)),
            Some(LPARAM(&raw const item as isize)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::rename_stem_select_end;

    #[test]
    fn file_with_extension_selects_stem() {
        assert_eq!(rename_stem_select_end("report.txt", false), Some(6));
    }

    #[test]
    fn file_with_multi_extension_selects_through_last_dot() {
        // Explorer parity: `archive.tar.gz` keeps `.gz`, selects `archive.tar`.
        assert_eq!(rename_stem_select_end("archive.tar.gz", false), Some(11));
    }

    #[test]
    fn file_without_extension_selects_all() {
        assert_eq!(rename_stem_select_end("Makefile", false), None);
    }

    #[test]
    fn dotfile_selects_all() {
        assert_eq!(rename_stem_select_end(".gitignore", false), None);
    }

    #[test]
    fn directory_selects_all_even_with_dot() {
        assert_eq!(rename_stem_select_end("repo.git", true), None);
    }

    #[test]
    fn non_ascii_uses_utf16_units() {
        // "α.txt" — α is one UTF-16 unit; stem length should be 1.
        assert_eq!(rename_stem_select_end("α.txt", false), Some(1));
    }

    #[test]
    fn non_bmp_char_counts_as_two_utf16_units() {
        // 🦀 (U+1F980) is a surrogate pair — 2 UTF-16 units.
        assert_eq!(rename_stem_select_end("🦀.png", false), Some(2));
    }
}
