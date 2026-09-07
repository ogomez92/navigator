//! Space breakdown window (Ctrl+Shift+S).
//!
//! A single modeless top-level window — same shape as `viewer` — holding
//! a `SysTreeView32` over a [`SpaceTree`], plus Open / Delete / Rescan /
//! Close buttons. A native tree view is the point: screen readers
//! already know how to read one (level, expanded / collapsed, "3 of 12"),
//! and Left / Right / Backspace / Home / End / type-ahead all come from
//! the control rather than from us.
//!
//! Keys in the tree:
//!
//!   * Right / Left — expand into a folder / collapse or go to the parent
//!     (native).
//!   * Enter — go there: a folder is navigated into, a file is focused in
//!     its parent (`AppState::jump_to`). The window closes; the scan is
//!     stale the moment the user starts changing things.
//!   * Delete — run the normal delete path on the row
//!     (`AppState::delete_targets`): `.trash` + undo locally, the shell's
//!     own prompt on a share, confirm + purge on a remote. The row and its
//!     size come out of the tree at once so the totals above it stay
//!     truthful.
//!   * F5 — rescan the same root. Esc — close.
//!
//! **Children are inserted lazily, on expand.** A drive holds a million
//! entries and the tree control is not virtual; inserting them all up
//! front would freeze the pump for the whole insert. Each folder's rows
//! go in the first time it is expanded, and a folder with more than
//! [`MAX_ROWS_PER_FOLDER`] children shows the largest of them and one
//! trailing row that says how much the rest add up to — the tail of a
//! largest-first list is by definition what is *not* taking the space.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;

use navigator_core::NavPath;
use once_cell::sync::OnceCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    HTREEITEM, ICC_TREEVIEW_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, NM_DBLCLK,
    NM_RETURN, NMHDR, NMTREEVIEWW, NMTVKEYDOWN, TVE_EXPAND, TVGN_CARET, TVGN_CHILD, TVGN_NEXT,
    TVGN_PARENT, TVI_LAST, TVI_ROOT, TVIF_CHILDREN, TVIF_PARAM, TVIF_TEXT, TVINSERTSTRUCTW,
    TVINSERTSTRUCTW_0, TVITEMEXW_CHILDREN, TVITEMW, TVM_DELETEITEM, TVM_ENSUREVISIBLE, TVM_EXPAND,
    TVM_GETITEMW, TVM_GETNEXTITEM, TVM_INSERTITEMW, TVM_SELECTITEM, TVM_SETEXTENDEDSTYLE,
    TVM_SETITEMW, TVN_ITEMEXPANDINGW, TVN_KEYDOWN, TVS_DISABLEDRAGDROP, TVS_EX_DOUBLEBUFFER,
    TVS_HASBUTTONS, TVS_HASLINES, TVS_LINESATROOT, TVS_SHOWSELALWAYS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, HCURSOR, HMENU, IDC_ARROW,
    LoadCursorW, MoveWindow, RegisterClassExW, SW_SHOW, SendMessageW, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_KEYDOWN,
    WM_NOTIFY, WM_SETFONT, WM_SETREDRAW, WM_SIZE, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_OVERLAPPED, WS_SIZEBOX, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::app::AppState;
use crate::spacemap::SpaceTree;

const IDC_TREE: u16 = 501;
const IDC_BTN_OPEN: u16 = 502;
const IDC_BTN_DELETE: u16 = 503;
const IDC_BTN_RESCAN: u16 = 504;
const IDC_BTN_CLOSE: u16 = 505;
const IDC_HEADING: u16 = 506;

const CLASS: PCWSTR = w!("NavigatorSpaceBreakdown");

/// Largest rows shown per folder before the rest are folded into one
/// "… and N more" row. High enough that a real folder is never cut, low
/// enough that a flat dump of a million files cannot stall the pump.
pub const MAX_ROWS_PER_FOLDER: usize = 5000;

/// `lParam` on the fold-away row. Not a node index, and never dereferenced
/// as one — every lookup goes through [`node_of`], which rejects it.
const OVERFLOW_ROW: isize = -1;

const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_TAB: u16 = 0x09;
const VK_DELETE: u16 = 0x2E;
const VK_F5: u16 = 0x74;

/// Private to this window class: "run the delete on the caret row", posted
/// from `TVN_KEYDOWN` so it runs after the tree has finished with the key.
const WM_DEFERRED_DELETE: u32 = 0x8000 /* WM_APP */ + 40;

struct Data {
    state: Arc<AppState>,
    tree_ctl: HWND,
    heading: HWND,
    btn_open: HWND,
    btn_delete: HWND,
    btn_rescan: HWND,
    btn_close: HWND,
    root: NavPath,
    tree: SpaceTree,
    /// Node indices whose children have been inserted into the control.
    populated: HashSet<usize>,
}

/// Show `tree` for `root`, replacing whatever the window held. `parent`
/// is the main window; the breakdown is modeless but owned, so it closes
/// with the app. Focus lands on the root row so a screen reader reads
/// the summary immediately.
pub fn show(parent: HWND, state: Arc<AppState>, root: NavPath, tree: SpaceTree) {
    let hwnd = match ensure_window(parent, state) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("space window create failed: {e:?}");
            return;
        }
    };
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    let label = root.rclone_arg().unwrap_or_else(|| root.to_string());
    set_text(hwnd, &format!("Space breakdown — {label}"));
    d.root = root;
    d.tree = tree;
    d.populated.clear();
    rebuild_tree(d);
    set_text(d.heading, &heading_text(&d.tree));
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    bring_to_foreground(hwnd, d.tree_ctl);
}

/// The line above the tree. The root row already carries the totals; this
/// says what the numbers *are*, because "size" on a folder screen is read
/// as disk usage and these are byte lengths — the only figure a remote
/// can answer, and what Alt+Enter reports too.
fn heading_text(tree: &SpaceTree) -> String {
    let r = tree.root();
    format!(
        "{} in {}. Largest first; sizes are file lengths, not disk allocation. \
         Enter goes there, Delete removes, F5 rescans.",
        crate::listview::format_size(r.size),
        crate::spacemap::count_phrase(r.files, r.dirs),
    )
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
            w!("Space breakdown"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_SIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
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
    // The listview registers its own class family; the tree view is a
    // separate ICC flag and nothing else in the app has asked for it.
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_TREEVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
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

fn build_children(hwnd: HWND, state: Arc<AppState>) -> Data {
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    let apply_font = |h: HWND| unsafe {
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    };

    let heading = mkstatic(hwnd, 10, 10, 720, IDC_HEADING);
    apply_font(heading);
    let tree_ctl = mktree(hwnd, 10, 50, 720, 420, IDC_TREE);
    apply_font(tree_ctl);
    install_tree_keys(tree_ctl);
    // Tab order: tree → Open → Delete → Rescan → Close.
    let btn_open = mkbutton(hwnd, "&Open", 330, 480, 90, 28, IDC_BTN_OPEN);
    let btn_delete = mkbutton(hwnd, "&Delete", 430, 480, 90, 28, IDC_BTN_DELETE);
    let btn_rescan = mkbutton(hwnd, "&Rescan", 530, 480, 90, 28, IDC_BTN_RESCAN);
    let btn_close = mkbutton(hwnd, "&Close", 630, 480, 90, 28, IDC_BTN_CLOSE);
    for b in [btn_open, btn_delete, btn_rescan, btn_close] {
        apply_font(b);
        // No dialog manager on a plain window: Tab, Esc and Enter each
        // need a subclass, same as the viewer's controls.
        crate::window::install_tab_nav(b);
        crate::window::install_esc_close(b);
        install_enter_click(b);
    }
    Data {
        state,
        tree_ctl,
        heading,
        btn_open,
        btn_delete,
        btn_rescan,
        btn_close,
        root: NavPath::this_pc(),
        tree: SpaceTree::from_items("", std::iter::empty(), 0),
        populated: HashSet::new(),
    }
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
    let heading_h = 36;
    let btn_w = 90;
    let btn_h = 28;
    let tree_y = pad + heading_h + pad;
    let tree_h = (h - tree_y - btn_h - pad * 2).max(40);
    let btn_y = tree_y + tree_h + pad;
    let inner_w = (w - pad * 2).max(40);
    unsafe {
        let _ = MoveWindow(d.heading, pad, pad, inner_w, heading_h, true);
        let _ = MoveWindow(d.tree_ctl, pad, tree_y, inner_w, tree_h, true);
        let mut x = (w - pad - btn_w * 4 - pad * 3).max(pad);
        for b in [d.btn_open, d.btn_delete, d.btn_rescan, d.btn_close] {
            let _ = MoveWindow(b, x, btn_y, btn_w, btn_h, true);
            x += btn_w + pad;
        }
    }
}

fn bring_to_foreground(hwnd: HWND, focus_target: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOP, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos,
    };
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(focus_target));
    }
}

// ---------------------------------------------------------------------
// Tree population
// ---------------------------------------------------------------------

/// Throw away every row and start again from the root, expanded one
/// level so the largest top-level items are on screen and reachable
/// with a single Down.
fn rebuild_tree(d: &mut Data) {
    unsafe {
        SendMessageW(d.tree_ctl, WM_SETREDRAW, Some(WPARAM(0)), Some(LPARAM(0)));
        SendMessageW(
            d.tree_ctl,
            TVM_DELETEITEM,
            Some(WPARAM(0)),
            Some(LPARAM(TVI_ROOT.0)),
        );
    }
    let root_has_children = !d.tree.children(SpaceTree::ROOT).is_empty();
    let hroot = insert_row(
        d.tree_ctl,
        TVI_ROOT,
        &d.tree.row_label(SpaceTree::ROOT),
        SpaceTree::ROOT as isize,
        root_has_children,
    );
    ensure_populated(d, SpaceTree::ROOT, hroot);
    unsafe {
        // Populate first, then expand: `TVM_EXPAND` does not reliably
        // raise `TVN_ITEMEXPANDING`, so the lazy path can't be relied on
        // for the one level that must be open when the window appears.
        SendMessageW(
            d.tree_ctl,
            TVM_EXPAND,
            Some(WPARAM(TVE_EXPAND.0 as usize)),
            Some(LPARAM(hroot.0)),
        );
        SendMessageW(
            d.tree_ctl,
            TVM_SELECTITEM,
            Some(WPARAM(TVGN_CARET as usize)),
            Some(LPARAM(hroot.0)),
        );
        SendMessageW(
            d.tree_ctl,
            TVM_ENSUREVISIBLE,
            Some(WPARAM(0)),
            Some(LPARAM(hroot.0)),
        );
        SendMessageW(d.tree_ctl, WM_SETREDRAW, Some(WPARAM(1)), Some(LPARAM(0)));
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(d.tree_ctl), None, true);
    }
}

/// Insert `node`'s children under `hitem` if that has not happened yet.
fn ensure_populated(d: &mut Data, node: usize, hitem: HTREEITEM) {
    if !d.populated.insert(node) {
        return;
    }
    let children = d.tree.children(node);
    let shown = children.len().min(MAX_ROWS_PER_FOLDER);
    unsafe {
        SendMessageW(d.tree_ctl, WM_SETREDRAW, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    for &c in &children[..shown] {
        let n = d.tree.get(c);
        insert_row(
            d.tree_ctl,
            hitem,
            &d.tree.row_label(c),
            c as isize,
            n.is_dir && !n.children.is_empty(),
        );
    }
    if children.len() > shown {
        let rest = &children[shown..];
        let hidden_size: u64 = rest.iter().map(|&c| d.tree.get(c).size).sum();
        insert_row(
            d.tree_ctl,
            hitem,
            &overflow_label(rest.len(), hidden_size),
            OVERFLOW_ROW,
            false,
        );
    }
    unsafe {
        SendMessageW(d.tree_ctl, WM_SETREDRAW, Some(WPARAM(1)), Some(LPARAM(0)));
    }
}

/// The fold-away row's text. Its share is left out on purpose: it is not
/// one item, and the number that matters is how much the tail adds up to.
pub fn overflow_label(hidden: usize, hidden_size: u64) -> String {
    format!(
        "… and {} smaller items, {} together (rescan a subfolder to see them)",
        crate::spacemap::group_thousands(hidden as u64),
        crate::listview::format_size(hidden_size),
    )
}

fn insert_row(
    tree_ctl: HWND,
    parent: HTREEITEM,
    text: &str,
    lparam: isize,
    has_children: bool,
) -> HTREEITEM {
    let mut wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let item = TVITEMW {
        mask: TVIF_TEXT | TVIF_PARAM | TVIF_CHILDREN,
        pszText: PWSTR(wide.as_mut_ptr()),
        cChildren: TVITEMEXW_CHILDREN(if has_children { 1 } else { 0 }),
        lParam: LPARAM(lparam),
        ..Default::default()
    };
    let ins = TVINSERTSTRUCTW {
        hParent: parent,
        hInsertAfter: TVI_LAST,
        Anonymous: TVINSERTSTRUCTW_0 { item },
    };
    let r = unsafe {
        SendMessageW(
            tree_ctl,
            TVM_INSERTITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&raw const ins as isize)),
        )
    };
    HTREEITEM(r.0)
}

fn set_row_text(tree_ctl: HWND, hitem: HTREEITEM, text: &str) {
    let mut wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let item = TVITEMW {
        mask: TVIF_TEXT,
        hItem: hitem,
        pszText: PWSTR(wide.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            tree_ctl,
            TVM_SETITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&raw const item as isize)),
        );
    }
}

/// The model node behind a row, or `None` for the fold-away row.
fn node_of(tree_ctl: HWND, hitem: HTREEITEM) -> Option<usize> {
    if hitem.0 == 0 {
        return None;
    }
    let mut item = TVITEMW {
        mask: TVIF_PARAM,
        hItem: hitem,
        ..Default::default()
    };
    let ok = unsafe {
        SendMessageW(
            tree_ctl,
            TVM_GETITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&raw mut item as isize)),
        )
    };
    if ok.0 == 0 || item.lParam.0 < 0 {
        return None;
    }
    Some(item.lParam.0 as usize)
}

fn next_item(tree_ctl: HWND, relation: u32, from: HTREEITEM) -> HTREEITEM {
    let r = unsafe {
        SendMessageW(
            tree_ctl,
            TVM_GETNEXTITEM,
            Some(WPARAM(relation as usize)),
            Some(LPARAM(from.0)),
        )
    };
    HTREEITEM(r.0)
}

fn caret(tree_ctl: HWND) -> HTREEITEM {
    next_item(tree_ctl, TVGN_CARET, HTREEITEM(0))
}

/// The real path a row names: the scanned root joined with the node's
/// components. Works for local, UNC and remote roots alike because
/// `NavPath::join` does.
fn path_of(d: &Data, node: usize) -> NavPath {
    let mut p = d.root.clone();
    for c in d.tree.components(node) {
        p = p.join(c);
    }
    p
}

// ---------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------

/// Enter / Open: go to the row and close. A folder is entered; a file is
/// focused in its parent, which is where the user can act on it.
fn open_selected(hwnd: HWND) {
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    let Some(node) = node_of(d.tree_ctl, caret(d.tree_ctl)) else {
        d.state.say("that row is a summary, not an item", false);
        return;
    };
    let path = path_of(d, node);
    let is_dir = d.tree.get(node).is_dir;
    // The close is *posted*, not done here: this runs inside the tree's
    // own `NM_RETURN` (or a button's `BN_CLICKED`), and destroying a
    // control from within its notification pulls the floor out from
    // under the code that sent it. The navigation is queued too (a scan
    // command), so the window is gone before the listing lands.
    if is_dir {
        d.state.navigate(path);
    } else {
        d.state.jump_to(path);
    }
    post_close(hwnd);
}

fn post_close(hwnd: HWND) {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Delete: hand the row to the app's delete path and, if that started,
/// take the row out of the tree so every total above it drops at once.
///
/// The row is removed as soon as the operation is *started*, not when it
/// finishes: the local path is an instant rename into `.trash` and the
/// remote path has already been confirmed, so the only way the tree can
/// overstate what is gone is a shell prompt on a share answered No — and
/// the shell says "delete cancelled" out loud, after which F5 is the
/// truth again.
fn delete_selected(hwnd: HWND) {
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    let hitem = caret(d.tree_ctl);
    let Some(node) = node_of(d.tree_ctl, hitem) else {
        d.state.say("that row is a summary, not an item", false);
        return;
    };
    if node == SpaceTree::ROOT {
        d.state.say(
            "this is the folder being sized; go down a level to delete something in it",
            true,
        );
        return;
    }
    let path = path_of(d, node);
    let is_dir = d.tree.get(node).is_dir;

    // Collect the chain of ancestor rows before the delete rearranges
    // anything; each one's label changes.
    let mut chain: Vec<HTREEITEM> = Vec::new();
    let mut up = next_item(d.tree_ctl, TVGN_PARENT, hitem);
    while up.0 != 0 {
        chain.push(up);
        up = next_item(d.tree_ctl, TVGN_PARENT, up);
    }

    if !d.state.delete_targets(vec![(path, is_dir)]) {
        return;
    }
    let Some(parent_node) = d.tree.remove(node) else {
        return;
    };
    unsafe {
        SendMessageW(
            d.tree_ctl,
            TVM_DELETEITEM,
            Some(WPARAM(0)),
            Some(LPARAM(hitem.0)),
        );
    }
    // Ancestors lost size; the siblings' share of the parent changed.
    for h in &chain {
        if let Some(n) = node_of(d.tree_ctl, *h) {
            set_row_text(d.tree_ctl, *h, &d.tree.row_label(n));
        }
    }
    if let Some(&hparent) = chain.first() {
        let mut sib = next_item(d.tree_ctl, TVGN_CHILD, hparent);
        while sib.0 != 0 {
            if let Some(n) = node_of(d.tree_ctl, sib) {
                set_row_text(d.tree_ctl, sib, &d.tree.row_label(n));
            }
            sib = next_item(d.tree_ctl, TVGN_NEXT, sib);
        }
        // A folder whose last row just went has no expander any more.
        if d.tree.children(parent_node).is_empty() {
            let item = TVITEMW {
                mask: TVIF_CHILDREN,
                hItem: hparent,
                cChildren: TVITEMEXW_CHILDREN(0),
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    d.tree_ctl,
                    TVM_SETITEMW,
                    Some(WPARAM(0)),
                    Some(LPARAM(&raw const item as isize)),
                );
            }
        }
    }
    set_text(d.heading, &heading_text(&d.tree));
}

fn rescan(hwnd: HWND) {
    let Some(d) = (unsafe { data(hwnd) }) else {
        return;
    };
    d.state.scan_space(d.root.clone());
}

// ---------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout(hwnd);
            LRESULT(0)
        }
        WM_DEFERRED_DELETE => {
            delete_selected(hwnd);
            LRESULT(0)
        }
        WM_NOTIFY => unsafe {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.idFrom as u16 != IDC_TREE {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            match hdr.code {
                TVN_ITEMEXPANDINGW => {
                    let nm = &*(lp.0 as *const NMTREEVIEWW);
                    if nm.action == TVE_EXPAND
                        && let Some(d) = data(hwnd)
                        && let Some(node) = node_of(d.tree_ctl, nm.itemNew.hItem)
                    {
                        ensure_populated(d, node, nm.itemNew.hItem);
                    }
                    LRESULT(0)
                }
                NM_RETURN | NM_DBLCLK => {
                    open_selected(hwnd);
                    LRESULT(1)
                }
                TVN_KEYDOWN => {
                    let kd = &*(lp.0 as *const NMTVKEYDOWN);
                    match kd.wVKey {
                        VK_DELETE => {
                            // Deferred: the delete removes the row the
                            // tree is mid-way through handling a key for,
                            // and may put up a modal confirm. Neither
                            // belongs inside the control's notification.
                            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                                Some(hwnd),
                                WM_DEFERRED_DELETE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                            LRESULT(1)
                        }
                        VK_F5 => {
                            rescan(hwnd);
                            LRESULT(1)
                        }
                        _ => LRESULT(0),
                    }
                }
                _ => LRESULT(0),
            }
        },
        WM_COMMAND => {
            let cmd = (wp.0 & 0xFFFF) as u16;
            match cmd {
                IDC_BTN_OPEN => open_selected(hwnd),
                IDC_BTN_DELETE => delete_selected(hwnd),
                IDC_BTN_RESCAN => rescan(hwnd),
                // Close, IDOK and IDCANCEL all close.
                IDC_BTN_CLOSE | 1 | 2 => post_close(hwnd),
                _ => {}
            }
            LRESULT(0)
        },
        // Focus given to the frame (alt-tab back, caption click) belongs
        // on the tree, exactly as the main window hands its own to the
        // listview.
        0x0007 /* WM_SETFOCUS */ => unsafe {
            if let Some(d) = data(hwnd) {
                let _ = SetFocus(Some(d.tree_ctl));
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
                let d = Box::from_raw(raw as *mut Data);
                // A rescan still running would otherwise re-open the
                // window the user just closed.
                d.state.cancel_space_scan();
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

fn set_text(hwnd: HWND, s: &str) {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

// ---------------------------------------------------------------------
// Subclasses
// ---------------------------------------------------------------------

/// The tree's own keys: Esc closes, Tab moves on, and the Enter
/// keystroke's `WM_CHAR` is swallowed so the control doesn't beep after
/// `NM_RETURN` has already opened the row.
fn install_tree_keys(tree_ctl: HWND) {
    unsafe {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            tree_ctl,
            Some(tree_subclass_proc),
            0xB350,
            0,
        );
    }
}

unsafe extern "system" fn tree_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    const WM_CHAR: u32 = 0x0102;
    unsafe {
        if msg == WM_KEYDOWN {
            match wp.0 as u16 {
                VK_ESCAPE => {
                    if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
                        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                            Some(parent),
                            WM_CLOSE,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                    return LRESULT(0);
                }
                VK_TAB => {
                    let shift =
                        (windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) as i32) < 0;
                    if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd)
                        && let Ok(next) = windows::Win32::UI::WindowsAndMessaging::GetNextDlgTabItem(
                            parent,
                            Some(hwnd),
                            shift,
                        )
                    {
                        let _ = SetFocus(Some(next));
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        if msg == WM_CHAR
            && (wp.0 as u16 == VK_RETURN || wp.0 as u16 == VK_ESCAPE || wp.0 as u16 == VK_TAB)
        {
            return LRESULT(0);
        }
        windows::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// Enter on a focused push button clicks it. The dialog manager would do
/// this for a real dialog; a plain window gets nothing, and a button
/// that only answers to Space is a button a keyboard user will miss.
fn install_enter_click(button: HWND) {
    unsafe {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            button,
            Some(enter_click_subclass_proc),
            0xB351,
            0,
        );
    }
}

unsafe extern "system" fn enter_click_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    const BN_CLICKED: usize = 0;
    unsafe {
        if msg == WM_KEYDOWN && wp.0 as u16 == VK_RETURN {
            if let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) {
                let id = windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(hwnd) as usize;
                SendMessageW(
                    parent,
                    WM_COMMAND,
                    Some(WPARAM((BN_CLICKED << 16) | (id & 0xFFFF))),
                    Some(LPARAM(hwnd.0 as isize)),
                );
            }
            return LRESULT(0);
        }
        windows::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wp, lp)
    }
}

// ---------------------------------------------------------------------
// Control factories
// ---------------------------------------------------------------------

fn mkstatic(parent: HWND, x: i32, y: i32, w: i32, id: u16) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            x,
            y,
            w,
            36,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(GetModuleHandleW(None).unwrap().into()),
            None,
        )
        .unwrap()
    }
}

fn mktree(parent: HWND, x: i32, y: i32, w: i32, h: i32, id: u16) -> HWND {
    let style = WS_CHILD.0
        | WS_VISIBLE.0
        | WS_BORDER.0
        | WS_TABSTOP.0
        | TVS_HASBUTTONS
        | TVS_HASLINES
        | TVS_LINESATROOT
        | TVS_SHOWSELALWAYS
        | TVS_DISABLEDRAGDROP;
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("SysTreeView32"),
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
    };
    unsafe {
        SendMessageW(
            hwnd,
            TVM_SETEXTENDEDSTYLE,
            Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)),
            Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize)),
        );
    }
    hwnd
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_row_names_the_count_and_the_total() {
        assert_eq!(
            overflow_label(12345, 2048),
            "… and 12,345 smaller items, 2.0 KB together (rescan a subfolder to see them)"
        );
    }

    #[test]
    fn heading_states_the_unit_and_the_keys() {
        let t = SpaceTree::from_items("r", [("a", false, 1024)], 0);
        let h = heading_text(&t);
        assert!(h.starts_with("1.0 KB in 1 file."), "{h}");
        assert!(h.contains("not disk allocation"), "{h}");
        assert!(
            h.contains("Enter goes there, Delete removes, F5 rescans"),
            "{h}"
        );
    }
}
