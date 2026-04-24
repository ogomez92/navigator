//! Options — a real Win32 property sheet built with `PropertySheetW` from
//! Comctl32.
//!
//! Each page is its own child dialog with its own `DLGTEMPLATE`, so the
//! property sheet gives us canonical tab-list accessibility
//! (ROLE_SYSTEM_PAGETABLIST + one ROLE_SYSTEM_PAGETAB per page), proper
//! Ctrl+Tab / Ctrl+Shift+Tab cycling between pages, and correct
//! Tab / Shift+Tab traversal *within* the active page. Previous attempts
//! used a single dialog + sibling-panel show/hide; that pattern breaks
//! keyboard traversal (the tab control falls out of the tabstop loop once
//! focus enters a panel) and doesn't model the tab-to-page relationship
//! for screen readers.
//!
//! Page contents are still built programmatically in `WM_INITDIALOG` — no
//! `.rc` file or build-time resource step is needed.

use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    NMHDR, PROPSHEETHEADERW_V2, PROPSHEETHEADERW_V2_0, PROPSHEETHEADERW_V2_1,
    PROPSHEETHEADERW_V2_2, PROPSHEETHEADERW_V2_3, PROPSHEETHEADERW_V2_4, PROPSHEETPAGEW,
    PROPSHEETPAGEW_0, PROPSHEETPAGEW_1, PROPSHEETPAGEW_2, PSH_NOAPPLYNOW, PSH_NOCONTEXTHELP,
    PSH_PROPSHEETPAGE, PSN_APPLY, PSP_DLGINDIRECT, PSP_USETITLE, PropertySheetW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CreateWindowExW, GetWindowLongPtrW, HMENU, SendMessageW,
    SetWindowLongPtrW, SetWindowTextW, WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX, WM_COMMAND,
    WM_INITDIALOG, WM_NOTIFY, WM_SETFONT, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
    GetWindowTextLengthW, GetWindowTextW,
};

use crate::app::AppState;

// Offsets reserved by `DefDlgProc`. DWLP_MSGRESULT is used to return
// PSNRET_* from PSN_APPLY handlers; DWLP_USER stores our page Data.
const DWLP_MSGRESULT: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(0);
const DWLP_USER:      WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

// Control IDs — scoped per page. Property-sheet child dialogs are
// isolated so IDs can collide across pages without confusion.
const ID_CHECK_RELATIVE: u16   = 100;
const ID_CHECK_NEW_BOTTOM: u16 = 101;
const ID_CHECK_HIDDEN: u16     = 200;
const ID_CHECK_SYSTEM: u16     = 201;
const ID_EDIT_INTERVAL: u16    = 300;
const ID_CHECK_PROG: u16       = 700;
const ID_EDIT_TRANSFERS: u16   = 701;
const ID_LIST_PLUGINS: u16     = 400;
const ID_BTN_RELOAD: u16       = 401;
const ID_LIST_HOTSPOTS: u16    = 500;
const ID_BTN_HOTSPOT_CLEAR: u16     = 501;
const ID_BTN_HOTSPOT_CLEAR_ALL: u16 = 502;
const ID_CHECK_COL_SIZE: u16      = 600;
const ID_CHECK_COL_TYPE: u16      = 601;
const ID_CHECK_COL_MODIFIED: u16  = 602;

/// Open the Options property sheet as a modal. Blocks until user closes.
pub fn open(parent: HWND, state: Arc<AppState>) -> windows::core::Result<()> {
    // Page templates are shared bytes owned for the duration of the call;
    // the `windows` crate's `PropertySheetW` takes raw pointers into them.
    let page_template = crate::dialog::build_propsheet_page_template(320, 270);

    // Titles as UTF-16, null-terminated. Owned for the life of the call
    // so the PCWSTR pointers we stash stay valid.
    let title_general:  Vec<u16> = "General\0".encode_utf16().collect();
    let title_view:     Vec<u16> = "View\0".encode_utf16().collect();
    let title_columns:  Vec<u16> = "Columns\0".encode_utf16().collect();
    let title_speech:   Vec<u16> = "Speech\0".encode_utf16().collect();
    let title_rclone:   Vec<u16> = "Rclone\0".encode_utf16().collect();
    let title_plugins:  Vec<u16> = "Plugins\0".encode_utf16().collect();
    let title_hotspots: Vec<u16> = "Hotspots\0".encode_utf16().collect();

    let caption: Vec<u16> = "Options — navigator\0".encode_utf16().collect();

    let hinstance = unsafe { GetModuleHandleW(None)?.into() };

    // One owned Arc<AppState> box per page — each page's DialogProc takes
    // ownership in WM_INITDIALOG.
    let make_lparam = || LPARAM(Box::into_raw(Box::new(state.clone())) as isize);

    let mut pages: Vec<PROPSHEETPAGEW> = vec![
        make_page(&page_template, &title_general,  hinstance, Some(page_general_proc),  make_lparam()),
        make_page(&page_template, &title_view,     hinstance, Some(page_view_proc),     make_lparam()),
        make_page(&page_template, &title_columns,  hinstance, Some(page_columns_proc),  make_lparam()),
        make_page(&page_template, &title_speech,   hinstance, Some(page_speech_proc),   make_lparam()),
        make_page(&page_template, &title_rclone,   hinstance, Some(page_rclone_proc),   make_lparam()),
        make_page(&page_template, &title_plugins,  hinstance, Some(page_plugins_proc),  make_lparam()),
        make_page(&page_template, &title_hotspots, hinstance, Some(page_hotspots_proc), make_lparam()),
    ];

    let mut header = PROPSHEETHEADERW_V2 {
        dwSize: std::mem::size_of::<PROPSHEETHEADERW_V2>() as u32,
        dwFlags: PSH_PROPSHEETPAGE | PSH_NOAPPLYNOW | PSH_NOCONTEXTHELP,
        hwndParent: parent,
        hInstance: hinstance,
        Anonymous1: PROPSHEETHEADERW_V2_0 { pszIcon: PCWSTR::null() },
        pszCaption: PCWSTR(caption.as_ptr()),
        nPages: pages.len() as u32,
        Anonymous2: PROPSHEETHEADERW_V2_1 { nStartPage: 0 },
        Anonymous3: PROPSHEETHEADERW_V2_2 { ppsp: pages.as_mut_ptr() },
        pfnCallback: None,
        Anonymous4: PROPSHEETHEADERW_V2_3 { pszbmWatermark: PCWSTR::null() },
        hplWatermark: windows::Win32::Graphics::Gdi::HPALETTE::default(),
        Anonymous5: PROPSHEETHEADERW_V2_4 { pszbmHeader: PCWSTR::null() },
    };

    let _hook = crate::dialog::AnimDisableHook::install();
    unsafe { PropertySheetW(&mut header); }
    Ok(())
}

fn make_page(
    template: &[u8],
    title: &[u16],
    hinstance: windows::Win32::Foundation::HINSTANCE,
    proc: windows::Win32::UI::WindowsAndMessaging::DLGPROC,
    lparam: LPARAM,
) -> PROPSHEETPAGEW {
    PROPSHEETPAGEW {
        dwSize: std::mem::size_of::<PROPSHEETPAGEW>() as u32,
        dwFlags: PSP_DLGINDIRECT | PSP_USETITLE,
        hInstance: hinstance,
        Anonymous1: PROPSHEETPAGEW_0 {
            pResource: template.as_ptr() as *mut _,
        },
        Anonymous2: PROPSHEETPAGEW_1 { pszIcon: PCWSTR::null() },
        pszTitle: PCWSTR(title.as_ptr()),
        pfnDlgProc: proc,
        lParam: lparam,
        pfnCallback: None,
        pcRefParent: std::ptr::null_mut(),
        pszHeaderTitle: PCWSTR::null(),
        pszHeaderSubTitle: PCWSTR::null(),
        hActCtx: windows::Win32::Foundation::HANDLE::default(),
        Anonymous3: PROPSHEETPAGEW_2 { pszbmHeader: PCWSTR::null() },
    }
}

// --- shared page helpers --------------------------------------------------

/// Unpack the `Box<Arc<AppState>>` pointer from a page's lParam on
/// WM_INITDIALOG. Returns the owned Arc (caller drops via page Data).
unsafe fn take_state_from_init(lp: LPARAM) -> Arc<AppState> {
    unsafe {
        let ppsp = lp.0 as *mut PROPSHEETPAGEW;
        let state_ptr = (*ppsp).lParam.0 as *mut Arc<AppState>;
        *Box::from_raw(state_ptr)
    }
}

fn apply_font_to(h: HWND) {
    unsafe {
        let font = GetStockObject(DEFAULT_GUI_FONT);
        SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
}

/// PSN_APPLY response: PSNRET_NOERROR (0) = accept, proceed.
/// Caller must return 1 (TRUE) from the DialogProc after calling this.
unsafe fn set_apply_ok(hwnd: HWND) {
    unsafe { SetWindowLongPtrW(hwnd, DWLP_MSGRESULT, 0); }
}

// --- General page ---------------------------------------------------------

struct GeneralData {
    state: Arc<AppState>,
    check_relative: HWND,
    check_new_bottom: HWND,
}

unsafe extern "system" fn page_general_proc(hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let check_relative = create_checkbox(hwnd, "Show &relative dates (e.g. \"5 minutes ago\")",
                                                  12, 16, ID_CHECK_RELATIVE);
            let check_new_bottom = create_checkbox(hwnd, "&New items appear at the bottom of the list",
                                                    12, 44, ID_CHECK_NEW_BOTTOM);
            apply_font_to(check_relative);
            apply_font_to(check_new_bottom);

            let g = state.config.read();
            set_check(check_relative, g.general.show_relative_dates);
            set_check(check_new_bottom, g.general.new_items_at_bottom);
            drop(g);

            let data = Box::new(GeneralData { state, check_relative, check_new_bottom });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut GeneralData);
                    let relative = get_check(d.check_relative);
                    let new_bottom = get_check(d.check_new_bottom);
                    d.state.config.with_mut(|c| {
                        c.general.show_relative_dates = relative;
                        c.general.new_items_at_bottom = new_bottom;
                    });
                    let _ = d.state.config.save();
                }
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 /* WM_DESTROY */ => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut GeneralData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

// --- View page ------------------------------------------------------------

struct ViewData {
    state: Arc<AppState>,
    check_hidden: HWND,
    check_system: HWND,
}

unsafe extern "system" fn page_view_proc(hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let check_hidden = create_checkbox(hwnd, "Show &hidden files", 12, 16, ID_CHECK_HIDDEN);
            let check_system = create_checkbox(hwnd, "Show &system files", 12, 44, ID_CHECK_SYSTEM);
            apply_font_to(check_hidden);
            apply_font_to(check_system);

            let g = state.config.read();
            set_check(check_hidden, g.general.show_hidden);
            set_check(check_system, g.general.show_system);
            drop(g);

            let data = Box::new(ViewData { state, check_hidden, check_system });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut ViewData);
                    let hidden = get_check(d.check_hidden);
                    let system = get_check(d.check_system);
                    d.state.config.with_mut(|c| {
                        c.general.show_hidden = hidden;
                        c.general.show_system = system;
                    });
                    let _ = d.state.config.save();
                    let filter = crate::model::Filter { show_hidden: hidden, show_system: system };
                    let _ = d.state.model.set_filter(filter);
                    if let Some(cwd) = d.state.model.cwd() {
                        d.state.navigate(cwd);
                    }
                }
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut ViewData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

// --- Columns page ---------------------------------------------------------

struct ColumnsData {
    state: Arc<AppState>,
    check_size: HWND,
    check_type: HWND,
    check_modified: HWND,
}

unsafe extern "system" fn page_columns_proc(hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl = create_label(hwnd,
                "Name column is always shown. Toggle the others:",
                12, 12, 420);
            let check_size = create_checkbox(hwnd, "Show &Size column",
                                             12, 40, ID_CHECK_COL_SIZE);
            let check_type = create_checkbox(hwnd, "Show &Type column",
                                             12, 68, ID_CHECK_COL_TYPE);
            let check_modified = create_checkbox(hwnd, "Show &Modified column",
                                                 12, 96, ID_CHECK_COL_MODIFIED);
            apply_font_to(lbl);
            apply_font_to(check_size);
            apply_font_to(check_type);
            apply_font_to(check_modified);

            let cols = state.config.read().general.columns;
            set_check(check_size, cols.show_size);
            set_check(check_type, cols.show_type);
            set_check(check_modified, cols.show_modified);

            let data = Box::new(ColumnsData { state, check_size, check_type, check_modified });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut ColumnsData);
                    let new_cols = navigator_config::Columns {
                        show_size: get_check(d.check_size),
                        show_type: get_check(d.check_type),
                        show_modified: get_check(d.check_modified),
                    };
                    let prev = d.state.config.read().general.columns;
                    if new_cols != prev {
                        d.state.config.with_mut(|c| c.general.columns = new_cols);
                        let _ = d.state.config.save();
                        // Rebuild the listview columns in place and refresh
                        // so the new layout takes effect without a restart.
                        d.state.reconfigure_listview_columns();
                    }
                }
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut ColumnsData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

// --- Speech page ----------------------------------------------------------

struct SpeechData {
    state: Arc<AppState>,
    edit_interval: HWND,
}

unsafe extern "system" fn page_speech_proc(hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let label = create_label(hwnd, "Announce progress every (seconds, 0 = off):",
                                     12, 18, 320);
            let edit_interval = create_edit(hwnd, 12, 42, 80, ID_EDIT_INTERVAL);
            apply_font_to(label);
            apply_font_to(edit_interval);

            let g = state.config.read();
            set_text(edit_interval, &g.general.announce_interval_secs.to_string());
            drop(g);

            let data = Box::new(SpeechData { state, edit_interval });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut SpeechData);
                    let interval: u32 = get_text(d.edit_interval).parse().unwrap_or(0);
                    d.state.config.with_mut(|c| c.general.announce_interval_secs = interval);
                    let _ = d.state.config.save();
                }
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut SpeechData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

// --- Rclone page ----------------------------------------------------------

struct RcloneData {
    state: Arc<AppState>,
    check_prog: HWND,
    edit_transfers: HWND,
}

unsafe extern "system" fn page_rclone_proc(hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let check_prog = create_checkbox(hwnd, "Show &progress window during operations",
                                             12, 16, ID_CHECK_PROG);
            let lbl = create_label(hwnd, "&Simultaneous transfers (--transfers, 1–64):",
                                   12, 54, 320);
            let edit_transfers = create_edit(hwnd, 12, 78, 80, ID_EDIT_TRANSFERS);
            apply_font_to(check_prog);
            apply_font_to(lbl);
            apply_font_to(edit_transfers);

            let r = state.config.read().rclone.clone();
            set_check(check_prog, r.progress_window);
            set_text(edit_transfers, &r.transfers_clamped().to_string());

            let data = Box::new(RcloneData { state, check_prog, edit_transfers });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut RcloneData);
                    let prog = get_check(d.check_prog);
                    // Fall back to the current configured value if the
                    // edit is empty or unparseable so a stray keystroke
                    // can't silently reset the setting to 1.
                    let current = d.state.config.read().rclone.transfers_clamped();
                    let entered: u32 = get_text(d.edit_transfers)
                        .parse()
                        .ok()
                        .filter(|n: &u32| *n >= 1 && *n <= 64)
                        .unwrap_or(current);
                    d.state.config.with_mut(|c| {
                        c.rclone.progress_window = prog;
                        c.rclone.transfers = entered;
                    });
                    let _ = d.state.config.save();
                }
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut RcloneData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

// --- Plugins page ---------------------------------------------------------

struct PluginsData {
    state: Arc<AppState>,
    list_plugins: HWND,
}

unsafe extern "system" fn page_plugins_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl1 = create_label(hwnd, "Drop plugin DLLs into:", 12, 12, 200);
            let dir_text = navigator_config::plugin_dir().display().to_string();
            let lbl2 = create_label(hwnd, &dir_text, 12, 32, 440);
            let list_plugins = create_listbox(hwnd, 12, 58, 440, 200, ID_LIST_PLUGINS);
            let btn_reload = create_button(hwnd, "&Reload plugins", 12, 264, 140, 26, ID_BTN_RELOAD);
            apply_font_to(lbl1);
            apply_font_to(lbl2);
            apply_font_to(list_plugins);
            apply_font_to(btn_reload);

            let data = Box::new(PluginsData { state, list_plugins });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize);
            refresh_plugin_list(&*raw);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw == 0 { return 0; }
            let d = &mut *(raw as *mut PluginsData);
            if cmd == ID_BTN_RELOAD {
                if let Some(reg) = d.state.plugin_registry() {
                    reg.load_from_dir(&navigator_config::plugin_dir());
                    refresh_plugin_list(d);
                }
                return 1;
            }
            0
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                // Plugins page has no config to commit — reload is immediate.
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut PluginsData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

fn refresh_plugin_list(d: &PluginsData) {
    // LB_RESETCONTENT = 0x0184, LB_ADDSTRING = 0x0180
    unsafe { SendMessageW(d.list_plugins, 0x0184, Some(WPARAM(0)), Some(LPARAM(0))); }
    if let Some(reg) = d.state.plugin_registry() {
        for name in reg.names() {
            let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
            unsafe {
                SendMessageW(d.list_plugins, 0x0180,
                             Some(WPARAM(0)), Some(LPARAM(w.as_ptr() as isize)));
            }
        }
    }
}

// --- Hotspots page --------------------------------------------------------

struct HotspotsData {
    state: Arc<AppState>,
    list_hotspots: HWND,
}

unsafe extern "system" fn page_hotspots_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl1 = create_label(hwnd,
                "Ctrl+Shift+1..0 saves the selected entry to the matching slot",
                12, 12, 440);
            let lbl2 = create_label(hwnd,
                "(overwrites). Ctrl+1..0 jumps to that slot.",
                12, 30, 440);
            let list_hotspots = create_listbox(hwnd, 12, 54, 440, 200, ID_LIST_HOTSPOTS);
            let btn_clear = create_button(hwnd, "&Clear selected",
                                          12, 262, 140, 26, ID_BTN_HOTSPOT_CLEAR);
            let btn_clear_all = create_button(hwnd, "Clear &all",
                                              160, 262, 110, 26, ID_BTN_HOTSPOT_CLEAR_ALL);
            apply_font_to(lbl1); apply_font_to(lbl2);
            apply_font_to(list_hotspots);
            apply_font_to(btn_clear); apply_font_to(btn_clear_all);

            let data = Box::new(HotspotsData { state, list_hotspots });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize);
            refresh_hotspot_list(&*raw);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw == 0 { return 0; }
            let d = &mut *(raw as *mut HotspotsData);
            match cmd {
                ID_BTN_HOTSPOT_CLEAR => {
                    clear_selected_hotspot(d);
                    1
                }
                ID_BTN_HOTSPOT_CLEAR_ALL => {
                    clear_all_hotspots(d);
                    1
                }
                _ => 0,
            }
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                // Hotspot changes already persisted on Clear button click.
                set_apply_ok(hwnd);
                return 1;
            }
            0
        },
        0x0002 => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw != 0 {
                let _ = Box::from_raw(raw as *mut HotspotsData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

/// Repopulate the hotspots listbox from current config state, preserving
/// the selected index so rapid successive clears keep focus on the same
/// slot.
fn refresh_hotspot_list(d: &HotspotsData) {
    // LB_GETCURSEL = 0x0188, LB_SETCURSEL = 0x0186,
    // LB_RESETCONTENT = 0x0184, LB_ADDSTRING = 0x0180
    let prev = unsafe {
        SendMessageW(d.list_hotspots, 0x0188, Some(WPARAM(0)), Some(LPARAM(0))).0
    };
    unsafe { SendMessageW(d.list_hotspots, 0x0184, Some(WPARAM(0)), Some(LPARAM(0))); }
    let slots = d.state.config.read().hotspots.clone();
    for (i, slot) in slots.iter().enumerate() {
        let label = if slot.is_empty() {
            format!("{}: (empty)", i + 1)
        } else {
            format!("{}: {}", i + 1, slot)
        };
        let w: Vec<u16> = label.encode_utf16().chain([0]).collect();
        unsafe {
            SendMessageW(d.list_hotspots, 0x0180,
                         Some(WPARAM(0)), Some(LPARAM(w.as_ptr() as isize)));
        }
    }
    if prev >= 0 && (prev as usize) < slots.len() {
        unsafe {
            SendMessageW(d.list_hotspots, 0x0186,
                         Some(WPARAM(prev as usize)), Some(LPARAM(0)));
        }
    }
}

fn clear_selected_hotspot(d: &HotspotsData) {
    let idx = unsafe {
        SendMessageW(d.list_hotspots, 0x0188, Some(WPARAM(0)), Some(LPARAM(0))).0
    };
    if idx < 0 { return; }
    let idx = idx as usize;
    d.state.config.with_mut(|c| {
        if idx < c.hotspots.len() { c.hotspots[idx].clear(); }
    });
    let _ = d.state.config.save();
    refresh_hotspot_list(d);
}

fn clear_all_hotspots(d: &HotspotsData) {
    d.state.config.with_mut(|c| {
        for slot in c.hotspots.iter_mut() { slot.clear(); }
    });
    let _ = d.state.config.save();
    refresh_hotspot_list(d);
}

// --- low-level control helpers -------------------------------------------

fn create_checkbox(parent: HWND, text: &str, x: i32, y: i32, id: u16) -> HWND {
    let tw: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(tw.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
            ),
            x, y, 420, 22,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        ).unwrap()
    }
}

fn create_label(parent: HWND, text: &str, x: i32, y: i32, w: i32) -> HWND {
    let tw: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(tw.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            x, y, w, 20,
            Some(parent), None,
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        ).unwrap()
    }
}

fn create_edit(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0,
            ),
            x, y, w, 22,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        ).unwrap()
    }
}

fn create_listbox(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    // LBS_HASSTRINGS = 0x0040, LBS_NOTIFY = 0x0001, WS_VSCROLL = 0x00200000
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0
        | 0x00200000 | 0x0041;
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            x, y, w, h,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        ).unwrap()
    }
}

fn create_button(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let tw: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(tw.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
            ),
            x, y, w, h,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        ).unwrap()
    }
}

// --- value accessors ------------------------------------------------------

fn set_check(hwnd: HWND, on: bool) {
    // BM_SETCHECK = 0x00F1
    unsafe {
        SendMessageW(hwnd, 0x00F1, Some(WPARAM(if on { 1 } else { 0 })), Some(LPARAM(0)));
    }
}

fn get_check(hwnd: HWND) -> bool {
    // BM_GETCHECK = 0x00F0, BST_CHECKED = 1
    unsafe {
        SendMessageW(hwnd, 0x00F0, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1
    }
}

fn set_text(hwnd: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe { let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr())); }
}

fn get_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 { return String::new(); }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(hwnd, &mut buf);
        if got <= 0 { return String::new(); }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}
