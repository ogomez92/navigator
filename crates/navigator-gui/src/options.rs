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

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    NMHDR, PROPSHEETHEADERW_V2, PROPSHEETHEADERW_V2_0, PROPSHEETHEADERW_V2_1,
    PROPSHEETHEADERW_V2_2, PROPSHEETHEADERW_V2_3, PROPSHEETHEADERW_V2_4, PROPSHEETPAGEW,
    PROPSHEETPAGEW_0, PROPSHEETPAGEW_1, PROPSHEETPAGEW_2, PSH_NOAPPLYNOW, PSH_NOCONTEXTHELP,
    PSH_PROPSHEETPAGE, PSN_APPLY, PSP_DLGINDIRECT, PSP_USETITLE, PropertySheetW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CreateWindowExW, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, HMENU, SendMessageW, SetWindowLongPtrW, SetWindowTextW, WINDOW_EX_STYLE,
    WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG, WM_NOTIFY, WM_SETFONT, WS_BORDER, WS_CHILD,
    WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use navigator_core::ConflictMode;

use crate::app::AppState;

// Offsets reserved by `DefDlgProc`. DWLP_MSGRESULT is used to return
// PSNRET_* from PSN_APPLY handlers; DWLP_USER stores our page Data.
const DWLP_MSGRESULT: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(0);
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

// Control IDs — scoped per page. Property-sheet child dialogs are
// isolated so IDs can collide across pages without confusion.
const ID_CHECK_RELATIVE: u16 = 100;
const ID_CHECK_NEW_BOTTOM: u16 = 101;
const ID_CHECK_HIDDEN: u16 = 200;
const ID_CHECK_SYSTEM: u16 = 201;
const ID_EDIT_INTERVAL: u16 = 300;
const ID_CHECK_PROG: u16 = 700;
const ID_EDIT_TRANSFERS: u16 = 701;
const ID_COMBO_CONFLICT: u16 = 702;
const ID_CHECK_EXTRACT_DELETE: u16 = 800;
const ID_CHECK_EXTRACT_FOLDER: u16 = 801;
const ID_LIST_PLUGINS: u16 = 400;
const ID_BTN_RELOAD: u16 = 401;
const ID_LIST_HOTSPOTS: u16 = 500;
const ID_BTN_HOTSPOT_CLEAR: u16 = 501;
const ID_BTN_HOTSPOT_CLEAR_ALL: u16 = 502;
const ID_CHECK_COL_SIZE: u16 = 600;
const ID_CHECK_COL_TYPE: u16 = 601;
const ID_CHECK_COL_MODIFIED: u16 = 602;
const ID_CHECK_SOUNDS: u16 = 900;
const ID_LIST_SOUND_EVENTS: u16 = 901;
const ID_COMBO_SOUND_FILE: u16 = 902;
const ID_BTN_SOUND_FOLDER: u16 = 903;
const ID_BTN_SOUND_RESCAN: u16 = 904;

/// Open the Options property sheet as a modal. Blocks until user closes.
pub fn open(parent: HWND, state: Arc<AppState>) -> windows::core::Result<()> {
    // Page templates are shared bytes owned for the duration of the call;
    // the `windows` crate's `PropertySheetW` takes raw pointers into them.
    let page_template = crate::dialog::build_propsheet_page_template(320, 270);

    // Titles as UTF-16, null-terminated. Owned for the life of the call
    // so the PCWSTR pointers we stash stay valid.
    let title_general: Vec<u16> = "General\0".encode_utf16().collect();
    let title_view: Vec<u16> = "View\0".encode_utf16().collect();
    let title_columns: Vec<u16> = "Columns\0".encode_utf16().collect();
    let title_speech: Vec<u16> = "Speech\0".encode_utf16().collect();
    let title_rclone: Vec<u16> = "Rclone\0".encode_utf16().collect();
    let title_extract: Vec<u16> = "Extraction\0".encode_utf16().collect();
    let title_sounds: Vec<u16> = "Sounds\0".encode_utf16().collect();
    let title_plugins: Vec<u16> = "Plugins\0".encode_utf16().collect();
    let title_hotspots: Vec<u16> = "Hotspots\0".encode_utf16().collect();

    let caption: Vec<u16> = "Options — navigator\0".encode_utf16().collect();

    let hinstance = unsafe { GetModuleHandleW(None)?.into() };

    // One owned Arc<AppState> box per page — each page's DialogProc takes
    // ownership in WM_INITDIALOG.
    let make_lparam = || LPARAM(Box::into_raw(Box::new(state.clone())) as isize);

    let mut pages: Vec<PROPSHEETPAGEW> = vec![
        make_page(
            &page_template,
            &title_general,
            hinstance,
            Some(page_general_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_view,
            hinstance,
            Some(page_view_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_columns,
            hinstance,
            Some(page_columns_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_speech,
            hinstance,
            Some(page_speech_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_rclone,
            hinstance,
            Some(page_rclone_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_extract,
            hinstance,
            Some(page_extract_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_sounds,
            hinstance,
            Some(page_sounds_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_plugins,
            hinstance,
            Some(page_plugins_proc),
            make_lparam(),
        ),
        make_page(
            &page_template,
            &title_hotspots,
            hinstance,
            Some(page_hotspots_proc),
            make_lparam(),
        ),
    ];

    let mut header = PROPSHEETHEADERW_V2 {
        dwSize: std::mem::size_of::<PROPSHEETHEADERW_V2>() as u32,
        dwFlags: PSH_PROPSHEETPAGE | PSH_NOAPPLYNOW | PSH_NOCONTEXTHELP,
        hwndParent: parent,
        hInstance: hinstance,
        Anonymous1: PROPSHEETHEADERW_V2_0 {
            pszIcon: PCWSTR::null(),
        },
        pszCaption: PCWSTR(caption.as_ptr()),
        nPages: pages.len() as u32,
        Anonymous2: PROPSHEETHEADERW_V2_1 { nStartPage: 0 },
        Anonymous3: PROPSHEETHEADERW_V2_2 {
            ppsp: pages.as_mut_ptr(),
        },
        pfnCallback: None,
        Anonymous4: PROPSHEETHEADERW_V2_3 {
            pszbmWatermark: PCWSTR::null(),
        },
        hplWatermark: windows::Win32::Graphics::Gdi::HPALETTE::default(),
        Anonymous5: PROPSHEETHEADERW_V2_4 {
            pszbmHeader: PCWSTR::null(),
        },
    };

    let _hook = crate::dialog::AnimDisableHook::install();
    unsafe {
        PropertySheetW(&mut header);
    }
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
        Anonymous2: PROPSHEETPAGEW_1 {
            pszIcon: PCWSTR::null(),
        },
        pszTitle: PCWSTR(title.as_ptr()),
        pfnDlgProc: proc,
        lParam: lparam,
        pfnCallback: None,
        pcRefParent: std::ptr::null_mut(),
        pszHeaderTitle: PCWSTR::null(),
        pszHeaderSubTitle: PCWSTR::null(),
        hActCtx: windows::Win32::Foundation::HANDLE::default(),
        Anonymous3: PROPSHEETPAGEW_2 {
            pszbmHeader: PCWSTR::null(),
        },
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
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

/// PSN_APPLY response: PSNRET_NOERROR (0) = accept, proceed.
/// Caller must return 1 (TRUE) from the DialogProc after calling this.
unsafe fn set_apply_ok(hwnd: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, DWLP_MSGRESULT, 0);
    }
}

// --- General page ---------------------------------------------------------

struct GeneralData {
    state: Arc<AppState>,
    check_relative: HWND,
    check_new_bottom: HWND,
}

unsafe extern "system" fn page_general_proc(
    hwnd: HWND,
    msg: u32,
    _wp: WPARAM,
    lp: LPARAM,
) -> isize {
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

            let data = Box::new(ViewData {
                state,
                check_hidden,
                check_system,
            });
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
                    let filter = crate::model::Filter {
                        show_hidden: hidden,
                        show_system: system,
                    };
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

unsafe extern "system" fn page_columns_proc(
    hwnd: HWND,
    msg: u32,
    _wp: WPARAM,
    lp: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl = create_label(
                hwnd,
                "Name column is always shown. Toggle the others:",
                12,
                12,
                420,
            );
            let check_size = create_checkbox(hwnd, "Show &Size column", 12, 40, ID_CHECK_COL_SIZE);
            let check_type = create_checkbox(hwnd, "Show &Type column", 12, 68, ID_CHECK_COL_TYPE);
            let check_modified =
                create_checkbox(hwnd, "Show &Modified column", 12, 96, ID_CHECK_COL_MODIFIED);
            apply_font_to(lbl);
            apply_font_to(check_size);
            apply_font_to(check_type);
            apply_font_to(check_modified);

            let cols = state.config.read().general.columns;
            set_check(check_size, cols.show_size);
            set_check(check_type, cols.show_type);
            set_check(check_modified, cols.show_modified);

            let data = Box::new(ColumnsData {
                state,
                check_size,
                check_type,
                check_modified,
            });
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

            let label = create_label(
                hwnd,
                "Announce progress every (seconds, 0 = off):",
                12,
                18,
                320,
            );
            let edit_interval = create_edit(hwnd, 12, 42, 80, ID_EDIT_INTERVAL);
            apply_font_to(label);
            apply_font_to(edit_interval);

            let g = state.config.read();
            set_text(edit_interval, &g.general.announce_interval_secs.to_string());
            drop(g);

            let data = Box::new(SpeechData {
                state,
                edit_interval,
            });
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
                    d.state
                        .config
                        .with_mut(|c| c.general.announce_interval_secs = interval);
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
    combo_conflict: HWND,
}

/// Modes offered as the *default* for a plain paste. Mirror is deliberately
/// absent: it deletes destination files the user never selected, so it is
/// reachable only from Paste special where the choice is explicit. A
/// hand-edited `config.toml` that sets it is still honoured.
const DEFAULT_CONFLICT_MODES: [ConflictMode; 3] = [
    ConflictMode::AddNewOnly,
    ConflictMode::Update,
    ConflictMode::Replace,
];

unsafe extern "system" fn page_rclone_proc(hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let check_prog = create_checkbox(
                hwnd,
                "Show &progress window during operations",
                12,
                16,
                ID_CHECK_PROG,
            );
            let lbl = create_label(
                hwnd,
                "&Simultaneous transfers (--transfers, 1–64):",
                12,
                54,
                320,
            );
            let edit_transfers = create_edit(hwnd, 12, 78, 80, ID_EDIT_TRANSFERS);

            let r = state.config.read().rclone.clone();

            let lbl_conflict = create_label(
                hwnd,
                "When items already e&xist at the destination:",
                12,
                116,
                320,
            );
            let labels: Vec<String> = DEFAULT_CONFLICT_MODES
                .iter()
                .map(|m| format!("{} — {}", m.label(), m.description()))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
            let selected = DEFAULT_CONFLICT_MODES
                .iter()
                .position(|m| *m == r.on_conflict)
                .or_else(|| {
                    // A config set to Mirror (or anything not offered here)
                    // shows as Update rather than presenting a wrong
                    // selection. Looked up by value, not a hard-coded index,
                    // so reordering the const cannot silently change it.
                    DEFAULT_CONFLICT_MODES
                        .iter()
                        .position(|m| *m == ConflictMode::Update)
                })
                .unwrap_or(0);
            let combo_conflict =
                create_combo(hwnd, 12, 140, 420, ID_COMBO_CONFLICT, &label_refs, selected);
            // Two single-line statics rather than one long one: `create_label`
            // makes a fixed 20px-tall STATIC with no wrapping, so a 95-char
            // string would simply be clipped.
            let lbl_hint = create_label(
                hwnd,
                "Paste only asks when this would replace or delete something.",
                12,
                172,
                430,
            );
            let lbl_hint2 = create_label(
                hwnd,
                "Ctrl+Shift+V chooses the mode for a single paste.",
                12,
                192,
                430,
            );

            apply_font_to(check_prog);
            apply_font_to(lbl);
            apply_font_to(edit_transfers);
            apply_font_to(lbl_conflict);
            apply_font_to(combo_conflict);
            apply_font_to(lbl_hint);
            apply_font_to(lbl_hint2);

            set_check(check_prog, r.progress_window);
            set_text(edit_transfers, &r.transfers_clamped().to_string());

            let data = Box::new(RcloneData {
                state,
                check_prog,
                edit_transfers,
                combo_conflict,
            });
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
                    // Leave the stored mode alone if the combo somehow has
                    // no selection, so a config hand-set to Mirror is not
                    // silently rewritten just by opening Options.
                    let mode = combo_selection(d.combo_conflict)
                        .and_then(|i| DEFAULT_CONFLICT_MODES.get(i).copied());
                    d.state.config.with_mut(|c| {
                        c.rclone.progress_window = prog;
                        c.rclone.transfers = entered;
                        if let Some(m) = mode {
                            c.rclone.on_conflict = m;
                        }
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

// --- Extraction page ------------------------------------------------------

struct ExtractData {
    state: Arc<AppState>,
    check_delete: HWND,
    check_folder: HWND,
}

unsafe extern "system" fn page_extract_proc(
    hwnd: HWND,
    msg: u32,
    _wp: WPARAM,
    lp: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl = create_label(
                hwnd,
                "Ctrl+E extracts the selected archives via 7z on PATH.",
                12,
                12,
                440,
            );
            let check_delete = create_checkbox(
                hwnd,
                "&Delete archive after successful extraction",
                12,
                40,
                ID_CHECK_EXTRACT_DELETE,
            );
            let check_folder = create_checkbox(
                hwnd,
                "Wrap extracted files in a &folder when the archive has\r\nmore than one top-level entry",
                12,
                70,
                ID_CHECK_EXTRACT_FOLDER,
            );
            apply_font_to(lbl);
            apply_font_to(check_delete);
            apply_font_to(check_folder);

            let e = state.config.read().extraction;
            set_check(check_delete, e.delete_when_extracted);
            set_check(check_folder, e.create_folder);

            let data = Box::new(ExtractData {
                state,
                check_delete,
                check_folder,
            });
            SetWindowLongPtrW(hwnd, DWLP_USER, Box::into_raw(data) as isize);
            1
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut ExtractData);
                    let delete = get_check(d.check_delete);
                    let folder = get_check(d.check_folder);
                    d.state.config.with_mut(|c| {
                        c.extraction.delete_when_extracted = delete;
                        c.extraction.create_folder = folder;
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
                let _ = Box::from_raw(raw as *mut ExtractData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

// --- Sounds page ----------------------------------------------------------

struct SoundsData {
    state: Arc<AppState>,
    check_enabled: HWND,
    list_events: HWND,
    combo_files: HWND,
    /// `.wav` filenames currently in the sounds folder, in combo order.
    /// Combo index 0 is "(none)", so a file at `files[i]` sits at combo
    /// index `i + 1`.
    files: Vec<String>,
    /// Pending assignment per [`SoundEvent::ALL`] slot; empty = silent.
    /// Edits live here until PSN_APPLY so Cancel really cancels — the only
    /// thing that happens immediately is the preview playback.
    assignments: Vec<String>,
}

/// Combo index 0 is the "no sound" entry; real files start at 1.
const SOUND_NONE_LABEL: &str = "(none)";

unsafe extern "system" fn page_sounds_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    use windows::Win32::UI::WindowsAndMessaging::{CBN_SELCHANGE, LBN_SELCHANGE};

    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl_dir = create_label(hwnd, "WAV files are read from:", 12, 12, 440);
            let dir_text = navigator_config::sounds_dir().display().to_string();
            let lbl_path = create_label(hwnd, &dir_text, 12, 32, 440);
            let check_enabled =
                create_checkbox(hwnd, "&Enable event sounds", 12, 58, ID_CHECK_SOUNDS);
            let lbl_events = create_label(hwnd, "&Events:", 12, 88, 440);
            let list_events = create_listbox(hwnd, 12, 108, 440, 170, ID_LIST_SOUND_EVENTS);
            let lbl_combo = create_label(
                hwnd,
                "&Sound for the selected event (plays when chosen):",
                12,
                288,
                440,
            );
            let combo_files = create_combo(hwnd, 12, 308, 440, ID_COMBO_SOUND_FILE, &[], 0);
            let btn_folder = create_button(
                hwnd,
                "&Open sounds folder",
                12,
                340,
                170,
                26,
                ID_BTN_SOUND_FOLDER,
            );
            let btn_rescan = create_button(
                hwnd,
                "&Rescan folder",
                192,
                340,
                130,
                26,
                ID_BTN_SOUND_RESCAN,
            );

            for h in [
                lbl_dir,
                lbl_path,
                check_enabled,
                lbl_events,
                list_events,
                lbl_combo,
                combo_files,
                btn_folder,
                btn_rescan,
            ] {
                apply_font_to(h);
            }

            let (enabled, assignments) = {
                let cfg = state.config.read();
                (
                    cfg.sounds.enabled,
                    navigator_config::SoundEvent::ALL
                        .iter()
                        .map(|ev| cfg.sounds.file_for(*ev).unwrap_or("").to_string())
                        .collect::<Vec<String>>(),
                )
            };
            set_check(check_enabled, enabled);

            let data = Box::new(SoundsData {
                state,
                check_enabled,
                list_events,
                combo_files,
                files: Vec::new(),
                assignments,
            });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize);

            let d = &mut *raw;
            reload_sound_files(d);
            refresh_sound_event_list(d);
            // Land on the first event so the combo below is never showing a
            // selection that belongs to nothing.
            listbox_set_selection(d.list_events, Some(0));
            sync_sound_combo(d);
            1
        },
        WM_COMMAND => unsafe {
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw == 0 {
                return 0;
            }
            let d = &mut *(raw as *mut SoundsData);
            let id = (wp.0 & 0xFFFF) as u16;
            let code = ((wp.0 >> 16) & 0xFFFF) as u32;
            match (id, code) {
                // A different event is selected — show what it is mapped to.
                (ID_LIST_SOUND_EVENTS, LBN_SELCHANGE) => {
                    sync_sound_combo(d);
                    1
                }
                // A file was chosen — assign it and play it straight away.
                (ID_COMBO_SOUND_FILE, CBN_SELCHANGE) => {
                    assign_selected_sound(d);
                    1
                }
                (ID_BTN_SOUND_FOLDER, _) => {
                    open_sounds_folder();
                    1
                }
                (ID_BTN_SOUND_RESCAN, _) => {
                    reload_sound_files(d);
                    refresh_sound_event_list(d);
                    sync_sound_combo(d);
                    1
                }
                _ => 0,
            }
        },
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.code == PSN_APPLY {
                let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
                if raw != 0 {
                    let d = &mut *(raw as *mut SoundsData);
                    let enabled = get_check(d.check_enabled);
                    let assignments = d.assignments.clone();
                    d.state.config.with_mut(|c| {
                        c.sounds.enabled = enabled;
                        for (ev, file) in navigator_config::SoundEvent::ALL.iter().zip(&assignments)
                        {
                            c.sounds.set(*ev, Some(file.as_str()));
                        }
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
                let _ = Box::from_raw(raw as *mut SoundsData);
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            0
        },
        _ => 0,
    }
}

/// Re-read the sounds folder and rebuild the combo's item list. Called on
/// open and from Rescan, so a user can drop WAVs in with Options still up.
fn reload_sound_files(d: &mut SoundsData) {
    d.files = navigator_config::list_sounds();
    combo_reset(d.combo_files);
    combo_add(d.combo_files, SOUND_NONE_LABEL);
    for f in &d.files {
        combo_add(d.combo_files, f);
    }
}

/// Rebuild the event listbox, preserving the selected row so a rescan or
/// an assignment doesn't move the user's place.
fn refresh_sound_event_list(d: &SoundsData) {
    let prev = listbox_selection(d.list_events);
    listbox_reset(d.list_events);
    for (i, ev) in navigator_config::SoundEvent::ALL.iter().enumerate() {
        listbox_add(d.list_events, &sound_event_row(d, i, *ev));
    }
    if let Some(p) = prev.filter(|p| *p < navigator_config::SoundEvent::ALL.len()) {
        listbox_set_selection(d.list_events, Some(p));
    }
}

/// One listbox row: the event and what it currently plays. An assignment
/// naming a file that isn't in the folder is flagged rather than silently
/// shown as configured — it will not play, and that should be visible here
/// rather than discovered when the event fires and nothing happens.
fn sound_event_row(d: &SoundsData, idx: usize, ev: navigator_config::SoundEvent) -> String {
    match d.assignments.get(idx).map(String::as_str) {
        Some("") | None => format!("{} — {}", ev.label(), SOUND_NONE_LABEL),
        Some(f) if d.files.iter().any(|x| x == f) => format!("{} — {}", ev.label(), f),
        Some(f) => format!("{} — {} (missing)", ev.label(), f),
    }
}

/// Point the combo at whatever the selected event currently plays.
fn sync_sound_combo(d: &SoundsData) {
    let Some(idx) = listbox_selection(d.list_events) else {
        combo_set_selection(d.combo_files, 0);
        return;
    };
    let assigned = d.assignments.get(idx).map(String::as_str).unwrap_or("");
    let pos = d
        .files
        .iter()
        .position(|f| f == assigned)
        // +1 to skip the "(none)" row; an assignment we can't find on disk
        // falls back to "(none)" so the combo never claims a file it can't
        // play — the listbox row is where the missing name is reported.
        .map(|i| i + 1)
        .unwrap_or(0);
    combo_set_selection(d.combo_files, pos);
}

/// Apply the combo's current file to the selected event and preview it.
/// Preview ignores the enabled checkbox on purpose: auditioning a file is
/// how you decide whether to switch sounds on at all.
fn assign_selected_sound(d: &mut SoundsData) {
    let Some(ev_idx) = listbox_selection(d.list_events) else {
        return;
    };
    let Some(combo_idx) = combo_selection(d.combo_files) else {
        return;
    };
    let file = if combo_idx == 0 {
        String::new()
    } else {
        match d.files.get(combo_idx - 1) {
            Some(f) => f.clone(),
            None => return,
        }
    };
    if let Some(slot) = d.assignments.get_mut(ev_idx) {
        *slot = file.clone();
    }
    refresh_sound_event_list(d);
    if !file.is_empty()
        && let Some(path) = navigator_config::sound_path(&file)
    {
        d.state.sound.play_file(path);
    }
}

/// Create the sounds folder if needed and open it in the shell, so the
/// user can drop WAVs in without hunting for the install directory.
fn open_sounds_folder() {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let dir = navigator_config::sounds_dir();
    let _ = std::fs::create_dir_all(&dir);
    let w: Vec<u16> = dir
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
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
            let btn_reload =
                create_button(hwnd, "&Reload plugins", 12, 264, 140, 26, ID_BTN_RELOAD);
            apply_font_to(lbl1);
            apply_font_to(lbl2);
            apply_font_to(list_plugins);
            apply_font_to(btn_reload);

            let data = Box::new(PluginsData {
                state,
                list_plugins,
            });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize);
            refresh_plugin_list(&*raw);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw == 0 {
                return 0;
            }
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
    unsafe {
        SendMessageW(d.list_plugins, 0x0184, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    if let Some(reg) = d.state.plugin_registry() {
        for name in reg.names() {
            let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
            unsafe {
                SendMessageW(
                    d.list_plugins,
                    0x0180,
                    Some(WPARAM(0)),
                    Some(LPARAM(w.as_ptr() as isize)),
                );
            }
        }
    }
}

// --- Hotspots page --------------------------------------------------------

struct HotspotsData {
    state: Arc<AppState>,
    list_hotspots: HWND,
}

unsafe extern "system" fn page_hotspots_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => unsafe {
            let state = take_state_from_init(lp);

            let lbl1 = create_label(
                hwnd,
                "Ctrl+Shift+1..0 saves the selected entry to the matching slot",
                12,
                12,
                440,
            );
            let lbl2 = create_label(
                hwnd,
                "(overwrites). Ctrl+1..0 jumps to that slot.",
                12,
                30,
                440,
            );
            let list_hotspots = create_listbox(hwnd, 12, 54, 440, 200, ID_LIST_HOTSPOTS);
            let btn_clear = create_button(
                hwnd,
                "&Clear selected",
                12,
                262,
                140,
                26,
                ID_BTN_HOTSPOT_CLEAR,
            );
            let btn_clear_all = create_button(
                hwnd,
                "Clear &all",
                160,
                262,
                110,
                26,
                ID_BTN_HOTSPOT_CLEAR_ALL,
            );
            apply_font_to(lbl1);
            apply_font_to(lbl2);
            apply_font_to(list_hotspots);
            apply_font_to(btn_clear);
            apply_font_to(btn_clear_all);

            let data = Box::new(HotspotsData {
                state,
                list_hotspots,
            });
            let raw = Box::into_raw(data);
            SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize);
            refresh_hotspot_list(&*raw);
            1
        },
        WM_COMMAND => unsafe {
            let cmd = (wp.0 & 0xFFFF) as u16;
            let raw = GetWindowLongPtrW(hwnd, DWLP_USER);
            if raw == 0 {
                return 0;
            }
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
    let prev = unsafe { SendMessageW(d.list_hotspots, 0x0188, Some(WPARAM(0)), Some(LPARAM(0))).0 };
    unsafe {
        SendMessageW(d.list_hotspots, 0x0184, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    let slots = d.state.config.read().hotspots.clone();
    for (i, slot) in slots.iter().enumerate() {
        let label = if slot.is_empty() {
            format!("{}: (empty)", i + 1)
        } else {
            format!("{}: {}", i + 1, slot)
        };
        let w: Vec<u16> = label.encode_utf16().chain([0]).collect();
        unsafe {
            SendMessageW(
                d.list_hotspots,
                0x0180,
                Some(WPARAM(0)),
                Some(LPARAM(w.as_ptr() as isize)),
            );
        }
    }
    if prev >= 0 && (prev as usize) < slots.len() {
        unsafe {
            SendMessageW(
                d.list_hotspots,
                0x0186,
                Some(WPARAM(prev as usize)),
                Some(LPARAM(0)),
            );
        }
    }
}

fn clear_selected_hotspot(d: &HotspotsData) {
    let idx = unsafe { SendMessageW(d.list_hotspots, 0x0188, Some(WPARAM(0)), Some(LPARAM(0))).0 };
    if idx < 0 {
        return;
    }
    let idx = idx as usize;
    d.state.config.with_mut(|c| {
        if idx < c.hotspots.len() {
            c.hotspots[idx].clear();
        }
    });
    let _ = d.state.config.save();
    refresh_hotspot_list(d);
}

fn clear_all_hotspots(d: &HotspotsData) {
    d.state.config.with_mut(|c| {
        for slot in c.hotspots.iter_mut() {
            slot.clear();
        }
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
            x,
            y,
            420,
            22,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

/// Drop-down list (no editable field) pre-filled with `items`, with
/// `selected` chosen. `CBS_DROPDOWNLIST` keeps it keyboard-navigable and
/// announced as a combo box by screen readers; `WS_VSCROLL` matters because
/// the list is taller than the collapsed control.
fn create_combo(
    parent: HWND,
    x: i32,
    y: i32,
    w: i32,
    id: u16,
    items: &[&str],
    selected: usize,
) -> HWND {
    use windows::Win32::UI::WindowsAndMessaging::{
        CB_ADDSTRING, CB_SETCURSEL, CBS_DROPDOWNLIST, WS_VSCROLL,
    };
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            x,
            y,
            w,
            // Height covers the collapsed field plus the dropped list.
            200,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap();
        for it in items {
            let tw: Vec<u16> = it.encode_utf16().chain([0]).collect();
            SendMessageW(
                h,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(tw.as_ptr() as isize)),
            );
        }
        SendMessageW(h, CB_SETCURSEL, Some(WPARAM(selected)), Some(LPARAM(0)));
        h
    }
}

/// Current selection index of a combo box, or `None` if nothing is selected.
fn combo_selection(h: HWND) -> Option<usize> {
    use windows::Win32::UI::WindowsAndMessaging::CB_GETCURSEL;
    let r = unsafe { SendMessageW(h, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))) };
    // CB_ERR (-1) means no selection.
    if r.0 < 0 { None } else { Some(r.0 as usize) }
}

fn combo_reset(h: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::CB_RESETCONTENT;
    unsafe {
        SendMessageW(h, CB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
    }
}

fn combo_add(h: HWND, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::CB_ADDSTRING;
    let w: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        SendMessageW(
            h,
            CB_ADDSTRING,
            Some(WPARAM(0)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
}

/// Set a combo's selection *without* generating `CBN_SELCHANGE` — that
/// notification is reserved for user action. Sending it programmatically
/// (as `CB_SETCURSEL` deliberately does not) would make selecting an event
/// in the listbox re-assign and replay its own sound.
fn combo_set_selection(h: HWND, idx: usize) {
    use windows::Win32::UI::WindowsAndMessaging::CB_SETCURSEL;
    unsafe {
        SendMessageW(h, CB_SETCURSEL, Some(WPARAM(idx)), Some(LPARAM(0)));
    }
}

fn listbox_reset(h: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::LB_RESETCONTENT;
    unsafe {
        SendMessageW(h, LB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
    }
}

fn listbox_add(h: HWND, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::LB_ADDSTRING;
    let w: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        SendMessageW(
            h,
            LB_ADDSTRING,
            Some(WPARAM(0)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
}

/// Selected index of a single-selection listbox, or `None` (LB_ERR).
fn listbox_selection(h: HWND) -> Option<usize> {
    use windows::Win32::UI::WindowsAndMessaging::LB_GETCURSEL;
    let r = unsafe { SendMessageW(h, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))) };
    if r.0 < 0 { None } else { Some(r.0 as usize) }
}

/// Set (or with `None`, clear) a listbox selection. Like `CB_SETCURSEL`
/// this does not raise `LBN_SELCHANGE`, so callers that need the combo
/// resynced must call [`sync_sound_combo`] themselves.
fn listbox_set_selection(h: HWND, idx: Option<usize>) {
    use windows::Win32::UI::WindowsAndMessaging::LB_SETCURSEL;
    let w = match idx {
        Some(i) => i,
        None => usize::MAX, // (WPARAM)-1 clears the selection
    };
    unsafe {
        SendMessageW(h, LB_SETCURSEL, Some(WPARAM(w)), Some(LPARAM(0)));
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

fn create_edit(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
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
            22,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn create_listbox(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    // LBS_HASSTRINGS = 0x0040, LBS_NOTIFY = 0x0001, WS_VSCROLL = 0x00200000
    let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0 | 0x00200000 | 0x0041;
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
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
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
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut std::ffi::c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

// --- value accessors ------------------------------------------------------

fn set_check(hwnd: HWND, on: bool) {
    // BM_SETCHECK = 0x00F1
    unsafe {
        SendMessageW(
            hwnd,
            0x00F1,
            Some(WPARAM(if on { 1 } else { 0 })),
            Some(LPARAM(0)),
        );
    }
}

fn get_check(hwnd: HWND) -> bool {
    // BM_GETCHECK = 0x00F0, BST_CHECKED = 1
    unsafe { SendMessageW(hwnd, 0x00F0, Some(WPARAM(0)), Some(LPARAM(0))).0 == 1 }
}

fn set_text(hwnd: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

fn get_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let got = GetWindowTextW(hwnd, &mut buf);
        if got <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..got as usize])
    }
}
