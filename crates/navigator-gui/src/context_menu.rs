//! Windows Shell context menu — the one Explorer shows on right-click.
//!
//! We route Shift+F10 and VK_APPS through `WM_CONTEXTMENU` into [`show`].
//! The flow:
//!
//!   1. Convert each selected path into an absolute PIDL.
//!   2. Bind to the parent IShellFolder (all selection must share a parent —
//!      which holds naturally because they come from one directory view).
//!   3. Ask the parent for an [`IContextMenu`] on the child PIDLs.
//!   4. Let it populate a `HMENU` we own, then `TrackPopupMenuEx` to display.
//!   5. If the user picks a verb, hand the command back via
//!      [`IContextMenu::InvokeCommand`].
//!
//! Submenus (Send to, New, etc.) require forwarding a few messages to
//! `IContextMenu2::HandleMenuMsg` / `IContextMenu3::HandleMenuMsg2`. We
//! stash the active interface in a thread-local for the duration of the
//! TrackPopupMenuEx call; the main window proc checks it on the relevant
//! messages and forwards.
//!
//! Most of the wait is the shell's, inside `QueryContextMenu`: the first
//! menu in a process measured ~1 s (loading every handler DLL), later ones
//! ~0.2 s. [`spawn_warmup`] pays the first-time part on a background thread
//! at startup.

use std::cell::RefCell;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMINVOKECOMMANDINFO, IContextMenu, IContextMenu2, IContextMenu3, ILFree, IShellFolder,
    SHBindToParent, SHGetDesktopFolder, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, HMENU, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenuEx,
};
use windows::core::{Interface, PCWSTR};

use navigator_core::NavPath;

// IContextMenu::QueryContextMenu wants a range of command IDs. Use a high
// range so it never collides with our menu/accelerator IDs (which top out
// near 0x4FFF in `window::Commands::ActionBase`).
const CMD_BASE: u32 = 0x8000;
const CMD_MAX: u32 = 0xFFFF;

/// CMF_* flags from shobjidl_core.h.
const CMF_NORMAL: u32 = 0x0000_0000;
const CMF_EXPLORE: u32 = 0x0000_0020;
const CMF_EXTENDEDVERBS: u32 = 0x0000_0100;

thread_local! {
    /// Active IContextMenu2/3 during `TrackPopupMenuEx`. The window proc
    /// peeks at this on WM_INITMENUPOPUP / WM_DRAWITEM / WM_MEASUREITEM
    /// and forwards those to the shell so submenus draw correctly.
    static ACTIVE: RefCell<Option<ActiveMenu>> = const { RefCell::new(None) };

    /// Whether we've called CoInitializeEx on this thread. Cheap check
    /// that avoids re-init.
    static COM_READY: RefCell<bool> = const { RefCell::new(false) };
}

/// Either IContextMenu2 or IContextMenu3 (preferred) for message forwarding.
struct ActiveMenu {
    m2: Option<IContextMenu2>,
    m3: Option<IContextMenu3>,
}

fn ensure_com() {
    COM_READY.with(|f| {
        if !*f.borrow() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            }
            *f.borrow_mut() = true;
        }
    });
}

/// Show the shell context menu at screen coordinates `pt` for the given
/// selection. `paths` must be non-empty and all share the same parent
/// directory (the GUI only ever selects inside one folder, so this holds).
///
/// `from_keyboard` should be true when the menu was triggered by Shift+F10
/// or VK_APPS — in that case we pre-select the first item so screen readers
/// announce something. Right-click should leave selection alone (Explorer
/// parity).
pub fn show(hwnd: HWND, pt: POINT, paths: &[NavPath], from_keyboard: bool) {
    if paths.is_empty() {
        return;
    }
    ensure_com();

    match unsafe { show_inner(hwnd, pt, paths, from_keyboard) } {
        Ok(()) => {}
        Err(e) => {
            tracing::warn!("context menu failed: {e:?}");
        }
    }
}

/// Build and discard one menu for navigator's own exe and one for its
/// folder, on a background thread, so the first Shift+F10 doesn't pay for
/// loading every shell extension DLL (measured: first file menu ~1000 ms
/// cold, ~250 ms after this).
///
/// Nothing is cached for reuse: the DLLs stay loaded in the process, but
/// every [`show`] still builds a fresh menu for the actual selection, so a
/// different file type still gets its own verbs (checked item for item
/// against a cold build). Only `QueryContextMenu` runs here — no verb is
/// ever invoked. The apartment is torn down afterwards; the gain survives
/// that.
pub fn spawn_warmup() {
    if let Err(e) = std::thread::Builder::new()
        .name("navigator-shell-menu-warmup".into())
        .spawn(|| {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let started = std::time::Instant::now();
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
                let targets = once(exe.as_path()).chain(exe.parent());
                for target in targets {
                    if let Err(e) = build(HWND::default(), &[target]) {
                        tracing::debug!("context menu warm-up for {target:?} failed: {e:?}");
                    }
                }
                CoUninitialize();
            }
            tracing::debug!("context menu warm-up took {:?}", started.elapsed());
        })
    {
        tracing::debug!("context menu warm-up: not started: {e}");
    }
}

/// A populated shell menu. Dropping it destroys the `HMENU`, releases the
/// `IContextMenu` and frees the PIDLs, in that order.
struct Built {
    hmenu: HMENU,
    ctx_menu: IContextMenu,
    _pidls: PidlGuard,
}

impl Drop for Built {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.hmenu);
        }
    }
}

/// Absolute PIDLs, returned owned by `SHParseDisplayName` — freed via
/// ILFree on drop.
struct PidlGuard(Vec<*mut ITEMIDLIST>);

impl Drop for PidlGuard {
    fn drop(&mut self) {
        for p in self.0.drain(..) {
            if !p.is_null() {
                unsafe {
                    ILFree(Some(p));
                }
            }
        }
    }
}

/// Ask the shell for the context menu of `paths` and populate a fresh
/// `HMENU` with it. `Ok(None)` when no path could be parsed.
unsafe fn build(hwnd: HWND, paths: &[&Path]) -> windows::core::Result<Option<Built>> {
    let mut abs_pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    for p in paths {
        let wide = to_wide(p);
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let r = unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) };
        if r.is_err() || pidl.is_null() {
            continue;
        }
        abs_pidls.push(pidl);
    }
    let pidls = PidlGuard(abs_pidls.clone());
    if abs_pidls.is_empty() {
        return Ok(None);
    }

    // Decide the parent IShellFolder:
    //   * All selections share one parent → bind to that folder and use the
    //     child PIDLs; Explorer-identical behavior.
    //   * Selections span multiple parents → use the desktop folder with
    //     absolute PIDLs (desktop treats absolute PIDLs as its children).
    let same_parent = paths.windows(2).all(|w| w[0].parent() == w[1].parent());

    let (parent, children_owned): (IShellFolder, Vec<*const ITEMIDLIST>) = if same_parent {
        let mut child_first: *mut ITEMIDLIST = std::ptr::null_mut();
        let parent: IShellFolder = unsafe { SHBindToParent(abs_pidls[0], Some(&mut child_first))? };
        let mut children: Vec<*const ITEMIDLIST> = Vec::with_capacity(abs_pidls.len());
        children.push(child_first as *const _);
        for &abs in &abs_pidls[1..] {
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            let _: IShellFolder = unsafe { SHBindToParent(abs, Some(&mut child))? };
            children.push(child as *const _);
        }
        (parent, children)
    } else {
        // Multi-parent fallback. Desktop accepts any absolute PIDL as a
        // "child"; the resulting IContextMenu contains the verbs common to
        // the full selection, which is what Explorer does when you
        // Ctrl-click across folders.
        let desktop = unsafe { SHGetDesktopFolder()? };
        let children: Vec<*const ITEMIDLIST> = abs_pidls.iter().map(|&p| p as *const _).collect();
        (desktop, children)
    };

    let ctx_menu: IContextMenu = unsafe { parent.GetUIObjectOf(hwnd, &children_owned, None)? };

    // Populate an HMENU from the context-menu object.
    let hmenu: HMENU = unsafe { CreatePopupMenu()? };
    let built = Built {
        hmenu,
        ctx_menu,
        _pidls: pidls,
    };

    // CMF_EXTENDEDVERBS reveals the Shift+F10 "extended" entries. Adding
    // it unconditionally matches what Explorer does when the user invokes
    // the menu via keyboard, which is our case.
    let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_EXTENDEDVERBS;
    // When the keyboard was used, GetKeyState on VK_SHIFT returns high
    // bit set. Explorer shows extended verbs with Shift held; we already
    // set the flag unconditionally so no-op here, but we keep the
    // primitive so future tuning is one-line.
    let _ = &mut flags;

    unsafe {
        // QueryContextMenu returns an HRESULT where the low word is the
        // largest-used command offset. We only care about success/failure.
        let hr = built
            .ctx_menu
            .QueryContextMenu(hmenu, 0, CMD_BASE, CMD_MAX, flags);
        if hr.0 < 0 {
            return Err(windows::core::Error::from_hresult(hr));
        }
    }
    Ok(Some(built))
}

unsafe fn show_inner(
    hwnd: HWND,
    pt: POINT,
    paths: &[NavPath],
    from_keyboard: bool,
) -> windows::core::Result<()> {
    let paths: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let Some(built) = (unsafe { build(hwnd, &paths)? }) else {
        return Ok(());
    };
    let (hmenu, ctx_menu) = (built.hmenu, &built.ctx_menu);

    // Stash IContextMenu2/3 for submenu message forwarding.
    let m2 = ctx_menu.cast::<IContextMenu2>().ok();
    let m3 = ctx_menu.cast::<IContextMenu3>().ok();
    ACTIVE.with(|a| *a.borrow_mut() = Some(ActiveMenu { m2, m3 }));

    // Without `SetForegroundWindow` the popup menu is created but doesn't
    // receive keyboard input — arrow keys don't navigate and Esc doesn't
    // dismiss. Shift+F10 hits this because the main window may not be
    // the foreground window even when it owns keyboard focus (e.g.
    // another app briefly had focus, or the listview subclass routed
    // the chord here from a focus-not-foreground state). The documented
    // workaround is `SetForegroundWindow(hwnd)` before the call plus a
    // dummy `PostMessage(hwnd, WM_NULL, 0, 0)` after so the menu's idle
    // loop gets one more pump to exit cleanly.
    use windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, SetForegroundWindow, WM_KEYDOWN, WM_NULL,
    };
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
    // Keyboard invocation: post a synthetic VK_DOWN so the menu's modal
    // pump highlights the first item on entry. Without this the popup
    // appears with no selection, so MSAA/UIA emits no focus change and
    // screen readers stay silent until the user arrows manually. Mouse
    // right-click skips this for Explorer parity.
    if from_keyboard {
        const VK_DOWN: usize = 0x28;
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(VK_DOWN), LPARAM(0));
        }
    }
    let cmd = unsafe {
        TrackPopupMenuEx(
            hmenu,
            (TPM_RETURNCMD | TPM_LEFTALIGN | TPM_RIGHTBUTTON).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        )
        .0 as u32
    };
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_NULL,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }

    ACTIVE.with(|a| *a.borrow_mut() = None);

    if (CMD_BASE..=CMD_MAX).contains(&cmd) {
        let verb_id = cmd - CMD_BASE;
        unsafe {
            invoke(ctx_menu, hwnd, verb_id)?;
        }
    }
    Ok(())
}

unsafe fn invoke(ctx: &IContextMenu, hwnd: HWND, verb_id: u32) -> windows::core::Result<()> {
    // Shell convention: pass the verb offset as a MAKEINTRESOURCE in
    // `lpVerb`. That's what Explorer does when the user picks a menu item;
    // it matches the offset handed back from TrackPopupMenuEx.
    let info = CMINVOKECOMMANDINFO {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
        fMask: 0,
        hwnd,
        lpVerb: windows::core::PCSTR(verb_id as usize as *const u8),
        lpParameters: windows::core::PCSTR::null(),
        lpDirectory: windows::core::PCSTR::null(),
        nShow: windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL.0,
        dwHotKey: 0,
        hIcon: Default::default(),
    };
    unsafe {
        ctx.InvokeCommand(&info)?;
    }
    Ok(())
}

/// Forward a shell-relevant message to the active IContextMenu2/3, if any.
///
/// The main window proc calls this on `WM_INITMENUPOPUP`, `WM_DRAWITEM`,
/// `WM_MEASUREITEM`, and `WM_MENUCHAR`. Returns `Some(lresult)` if the
/// shell handled the message — in that case the caller should return the
/// value verbatim instead of falling back to `DefWindowProc`.
pub fn forward_menu_msg(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    ACTIVE.with(|a| {
        let borrow = a.borrow();
        let active = borrow.as_ref()?;
        // IContextMenu3 HandleMenuMsg2 returns an explicit LRESULT; prefer it.
        if let Some(m3) = active.m3.as_ref() {
            let mut out = LRESULT(0);
            let r = unsafe { m3.HandleMenuMsg2(msg, wp, lp, Some(&mut out)) };
            if r.is_ok() {
                return Some(out);
            }
        }
        if let Some(m2) = active.m2.as_ref() {
            let r = unsafe { m2.HandleMenuMsg(msg, wp, lp) };
            if r.is_ok() {
                return Some(LRESULT(0));
            }
        }
        None
    })
}

fn to_wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(once(0)).collect()
}
