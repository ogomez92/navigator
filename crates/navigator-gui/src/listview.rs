//! Helpers for the virtual `SysListView32` we embed in the main window.
//!
//! The list view is created with `LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS`.
//! `LVS_OWNERDATA` ("virtual") means the control asks us for each cell via
//! `LVN_GETDISPINFO` instead of allocating `LVITEM`s up-front. That's how we
//! stay fast for million-entry folders.

use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, LVCFMT_LEFT, LVCFMT_RIGHT,
    LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVM_INSERTCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE,
    LVM_SETITEMCOUNT, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES,
    LVS_EX_HEADERDRAGDROP, LVS_EX_LABELTIP,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetNextDlgTabItem, GetParent, PostMessageW, SendMessageW, SetWindowPos,
    ShowWindow, HMENU, SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE, WM_COMMAND, WM_KEYDOWN, WS_BORDER,
    WS_CHILD, WS_VISIBLE,
};
use windows::Win32::UI::Shell::{SetWindowSubclass, DefSubclassProc};

// ListView-specific window style bits (not all exposed by the windows crate today).
const LVS_REPORT: u32 = 0x0001;
const LVS_OWNERDATA: u32 = 0x1000;
const LVS_SHOWSELALWAYS: u32 = 0x0008;
// Enables LVM_EDITLABELW. Without it, F2 / begin_rename is a no-op.
const LVS_EDITLABELS: u32 = 0x0200;

/// Logical column identity. The ListView's physical `iSubItem` indices
/// are a prefix of the visible subset of this enum, so callers must go
/// through `column_for_subitem` to get back to the logical column when
/// rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalColumn {
    Name,
    Size,
    Type,
    Modified,
}

/// Build the ordered visible-columns list from config. `Name` is always
/// first; the remaining three are appended only when their per-column
/// flag is on. Exposed for unit tests that assert on the mapping.
pub fn visible_columns(cols: &navigator_config::Columns) -> Vec<LogicalColumn> {
    let mut out = vec![LogicalColumn::Name];
    if cols.show_size     { out.push(LogicalColumn::Size); }
    if cols.show_type     { out.push(LogicalColumn::Type); }
    if cols.show_modified { out.push(LogicalColumn::Modified); }
    out
}

/// Translate the ListView's `iSubItem` into a `LogicalColumn` given the
/// current column config. Returns `None` if the index is out of range
/// (shouldn't happen in practice — defensive against races during a
/// column reconfigure).
pub fn column_for_subitem(
    cols: &navigator_config::Columns,
    sub_item: i32,
) -> Option<LogicalColumn> {
    if sub_item < 0 { return None; }
    visible_columns(cols).get(sub_item as usize).copied()
}

/// The ListView itself is a child HWND; we keep only the handle.
#[derive(Copy, Clone)]
pub struct ListView {
    pub hwnd: HWND,
}

impl ListView {
    /// Register common controls and create the child ListView inside `parent`.
    /// `state` is passed to the subclass as a non-owning pointer so the
    /// WM_CHAR handler can drive our own incremental type-ahead buffer.
    /// `AppState` is an Arc held by the main window — it outlives the
    /// listview, so the pointer stays valid for the window's lifetime.
    pub fn create(parent: HWND, id: u16, state: &std::sync::Arc<crate::app::AppState>) -> windows::core::Result<Self> {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        unsafe { let _ = InitCommonControlsEx(&icc); }

        // WS_TABSTOP lets IsDialogMessageW cycle focus onto the ListView
        // the same way it does onto the address bar. Without it, Tab leaves
        // the listview and never comes back.
        let style = WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0
            | windows::Win32::UI::WindowsAndMessaging::WS_TABSTOP.0
            | LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_EDITLABELS;

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("SysListView32"),
                PCWSTR::null(),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
                0, 0, 0, 0,
                Some(parent),
                Some(HMENU(id as isize as *mut c_void)),
                None,
                None,
            )?
        };

        // Quality-of-life: full row select, double buffering, tooltips for
        // truncated labels, header drag/drop, light gridlines. All of these
        // are screen-reader neutral.
        let ex_style = LVS_EX_FULLROWSELECT
            | LVS_EX_DOUBLEBUFFER
            | LVS_EX_LABELTIP
            | LVS_EX_GRIDLINES
            | LVS_EX_HEADERDRAGDROP;
        unsafe {
            SendMessageW(
                hwnd,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                Some(WPARAM(0)),
                Some(LPARAM(ex_style as isize)),
            );
        }

        let mut lv = Self { hwnd };
        let cols = state.config.read().general.columns;
        let mut next_idx = 0i32;
        for col in visible_columns(&cols) {
            let (title, width, right) = match col {
                LogicalColumn::Name     => (w!("Name"),     320, false),
                LogicalColumn::Size     => (w!("Size"),     100, true),
                LogicalColumn::Type     => (w!("Type"),     120, false),
                LogicalColumn::Modified => (w!("Modified"), 160, false),
            };
            lv.add_column(next_idx, title, width, right);
            next_idx += 1;
        }

        unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }

        // Subclass so we can intercept VK_BACK on the listview before it
        // gets consumed by the control's built-in incremental-search
        // buffer. Accelerator tables race with the listview on plain
        // Backspace; subclassing is the deterministic path.
        let state_ptr = std::sync::Arc::as_ptr(state) as usize;
        unsafe {
            let _ = SetWindowSubclass(hwnd, Some(listview_subclass_proc), SUBCLASS_ID, state_ptr);
        }
        Ok(lv)
    }

    fn add_column(&mut self, index: i32, text: PCWSTR, width: i32, right_align: bool) {
        let mut col: LVCOLUMNW = unsafe { std::mem::zeroed() };
        col.mask = LVCF_FMT | LVCF_WIDTH | LVCF_TEXT;
        col.fmt = if right_align { LVCFMT_RIGHT } else { LVCFMT_LEFT };
        col.cx = width;
        col.pszText = windows::core::PWSTR(text.as_ptr() as *mut _);
        unsafe {
            SendMessageW(
                self.hwnd,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index as usize)),
                Some(LPARAM(&raw const col as isize)),
            );
        }
    }

    /// Tell the control how many virtual rows to render.
    pub fn set_item_count(&self, count: usize) {
        unsafe {
            SendMessageW(
                self.hwnd,
                LVM_SETITEMCOUNT,
                Some(WPARAM(count)),
                Some(LPARAM(0)),
            );
        }
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x, y, w, h,
                SWP_NOZORDER,
            );
        }
    }

    pub fn focus(&self) {
        unsafe { let _ = SetFocus(Some(self.hwnd)); }
    }

    /// Tear down every column and re-insert the ones enabled by `cols`,
    /// in canonical order. Called after the user toggles columns in
    /// Options so the change takes effect without a restart.
    pub fn reconfigure_columns(&self, cols: &navigator_config::Columns) {
        const LVM_DELETECOLUMN: u32 = 0x1000 + 28;
        const LVM_GETCOLUMNWIDTH: u32 = 0x1000 + 29;
        // We don't know how many columns currently exist from the API
        // side without LVM_GETHEADER+Header_GetItemCount gymnastics, so
        // keep deleting at index 0 until the call fails (returns 0).
        unsafe {
            loop {
                // LVM_GETCOLUMNWIDTH returns 0 when the column doesn't
                // exist — safer probe than "delete until it fails".
                let r = SendMessageW(
                    self.hwnd,
                    LVM_GETCOLUMNWIDTH,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                );
                if r.0 == 0 { break; }
                let _ = SendMessageW(
                    self.hwnd,
                    LVM_DELETECOLUMN,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                );
            }
        }

        let mut me = Self { hwnd: self.hwnd };
        let mut next_idx = 0i32;
        for col in visible_columns(cols) {
            let (title, width, right) = match col {
                LogicalColumn::Name     => (w!("Name"),     320, false),
                LogicalColumn::Size     => (w!("Size"),     100, true),
                LogicalColumn::Type     => (w!("Type"),     120, false),
                LogicalColumn::Modified => (w!("Modified"), 160, false),
            };
            me.add_column(next_idx, title, width, right);
            next_idx += 1;
        }
    }
}

const SUBCLASS_ID: usize = 0xA55A;

/// Subclass proc: intercepts keys we need to scope to listview focus so
/// they don't fire accelerator-style from within the address bar. Routes
/// them to the parent as WM_COMMAND.
///
/// - VK_BACK  → `Commands::Back`         (parent directory)
/// - VK_DELETE → `Commands::Delete`       (rclone purge selection)
/// - VK_RETURN → `Commands::OpenFocused`  (open folder / launch file)
///
/// Command IDs are hard-coded so this file doesn't need to import the
/// parent window's types. They MUST match `window::Commands`.
unsafe extern "system" fn listview_subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
    _id: usize,
    data: usize,
) -> windows::Win32::Foundation::LRESULT {
    const CMD_BACK: u16 = 106;
    const CMD_DELETE: u16 = 105;
    const CMD_OPEN_FOCUSED: u16 = 112;
    const WM_CHAR: u32 = 0x0102;

    // Drive incremental type-ahead ourselves. Consuming WM_CHAR keeps the
    // listview's internal prefix buffer empty, so a letter pressed after
    // navigating into a new folder starts a fresh search instead of
    // continuing the previous folder's buffer.
    if msg == WM_CHAR && data != 0 {
        let ch = wp.0 as u32;
        // Filter: printable Unicode only, no control chars. Ctrl+letter
        // WM_CHAR codes land in 0x01..=0x1A — skip those too.
        if ch >= 0x20 && ch != 0x7F {
            if let Some(c) = char::from_u32(ch) {
                let state = unsafe { &*(data as *const crate::app::AppState) };
                if let Some(idx) = state.type_ahead_step(c) {
                    focus_row(hwnd, idx);
                }
            }
        }
        return windows::Win32::Foundation::LRESULT(0);
    }

    if msg == WM_KEYDOWN {
        let vk = wp.0 as u32;
        let routed = match vk {
            0x08 => Some(CMD_BACK),         // VK_BACK
            0x2E => Some(CMD_DELETE),       // VK_DELETE
            0x0D => Some(CMD_OPEN_FOCUSED), // VK_RETURN
            _ => None,
        };
        if let Some(cmd) = routed {
            unsafe {
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(
                        Some(parent),
                        WM_COMMAND,
                        windows::Win32::Foundation::WPARAM(cmd as usize),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
            }
            return windows::Win32::Foundation::LRESULT(0);
        }
    }
    // Tab / Shift+Tab: explicitly traverse tabstops. IsDialogMessageW in
    // the main message loop *should* handle this, but SysListView32 with
    // LVS_OWNERDATA swallows Tab in some states — driving it ourselves
    // guarantees the user can leave the listview.
    if msg == WM_KEYDOWN && wp.0 as u32 == 0x09 {
        unsafe {
            let shift = (GetKeyState(0x10 /* VK_SHIFT */) as i32) < 0;
            if let Ok(parent) = GetParent(hwnd) {
                if let Ok(next) = GetNextDlgTabItem(parent, Some(hwnd), shift) {
                    let _ = SetFocus(Some(next));
                }
            }
        }
        return windows::Win32::Foundation::LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

/// Single-select + focus the row at `idx` on a listview, scroll it into
/// view. Same semantics as `window::select_row` — duplicated here so the
/// subclass doesn't need to cross-import.
fn focus_row(lv: windows::Win32::Foundation::HWND, idx: usize) {
    use windows::Win32::UI::Controls::{
        LIST_VIEW_ITEM_STATE_FLAGS, LVITEMW, LVM_ENSUREVISIBLE, LVM_SETITEMSTATE,
    };
    const SEL_FOCUS: LIST_VIEW_ITEM_STATE_FLAGS = LIST_VIEW_ITEM_STATE_FLAGS(0x0003);
    unsafe {
        let mut clear: LVITEMW = std::mem::zeroed();
        clear.stateMask = SEL_FOCUS;
        SendMessageW(
            lv,
            LVM_SETITEMSTATE,
            Some(windows::Win32::Foundation::WPARAM(usize::MAX)),
            Some(windows::Win32::Foundation::LPARAM(&raw const clear as isize)),
        );
        let mut item: LVITEMW = std::mem::zeroed();
        item.state = SEL_FOCUS;
        item.stateMask = SEL_FOCUS;
        SendMessageW(
            lv,
            LVM_SETITEMSTATE,
            Some(windows::Win32::Foundation::WPARAM(idx)),
            Some(windows::Win32::Foundation::LPARAM(&raw const item as isize)),
        );
        SendMessageW(
            lv,
            LVM_ENSUREVISIBLE,
            Some(windows::Win32::Foundation::WPARAM(idx)),
            Some(windows::Win32::Foundation::LPARAM(0)),
        );
    }
}

/// Format a byte count with binary suffixes. Matches the screen-reader-
/// friendly form used by Explorer's Narrator ("1.2 MB", not "1.2 MiB").
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    let mut n = bytes as f64;
    let mut i = 0;
    while n >= 1024.0 && i + 1 < UNITS.len() {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", n, UNITS[i])
    }
}

/// Format a Windows FILETIME (100-ns ticks since 1601) as "YYYY-MM-DD HH:MM".
/// Uses `SYSTEMTIME` locally rather than pulling `chrono` in.
pub fn format_filetime(ticks: u64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::FileTimeToSystemTime;
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut st = SYSTEMTIME::default();
    let ok = unsafe { FileTimeToSystemTime(&ft, &raw mut st) }.is_ok();
    if !ok { return String::new(); }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
    )
}

/// Format as a relative expression: "5 minutes ago", "3 hours ago",
/// "yesterday", "4 days ago", "last month", "2 years ago". Returns an
/// absolute timestamp for anything more than a year old, since relative
/// phrasing stops being useful at that scale.
pub fn format_filetime_relative(ticks: u64) -> String {
    use windows::Win32::Foundation::FILETIME;

    if ticks == 0 { return String::new(); }

    // Compare against "now" in FILETIME ticks. GetSystemTimeAsFileTime
    // gives UTC-100ns, same basis as ftLastWriteTime.
    let now_ft: FILETIME = unsafe {
        windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime()
    };
    let now = ((now_ft.dwHighDateTime as u64) << 32) | now_ft.dwLowDateTime as u64;
    if ticks > now {
        // Clock skew or a future mtime — fall back to absolute.
        return format_filetime(ticks);
    }
    let diff = now - ticks;
    // 100-ns ticks → seconds.
    let secs = diff / 10_000_000;
    let mins = secs / 60;
    let hours = secs / 3_600;
    let days = secs / 86_400;

    if secs < 45          { "just now".to_string() }
    else if secs < 90     { "about a minute ago".to_string() }
    else if mins < 45     { format!("{} minutes ago", mins) }
    else if mins < 90     { "about an hour ago".to_string() }
    else if hours < 24    { format!("{} hours ago", hours) }
    else if days == 1     { "yesterday".to_string() }
    else if days < 7      { format!("{} days ago", days) }
    else if days < 14     { "last week".to_string() }
    else if days < 30     { format!("{} weeks ago", days / 7) }
    else if days < 60     { "last month".to_string() }
    else if days < 365    { format!("{} months ago", days / 30) }
    else if days < 730    { "last year".to_string() }
    else                  { format!("{} years ago", days / 365) }
}
