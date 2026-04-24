//! Per-directory file watcher. Built on `notify` (which uses
//! `ReadDirectoryChangesW` on Windows under the hood). We watch the
//! current-working-directory non-recursively and fold the events into the
//! model so the visible listing self-updates without a full rescan.

use std::path::PathBuf;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use navigator_core::NavPath;

use crate::window::{HwndSend, WMAPP_WATCH_EVENT};

/// Simplified kind of filesystem change. We map notify's richer `EventKind`
/// onto this enum because the GUI thread only cares about three cases.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    Added(String),
    Removed(String),
    /// Mtime/size changed — easiest correct behaviour is a full refresh.
    Modified(String),
    /// Rename/move. We treat it as Remove(from) + Add(to) at the UI.
    Renamed { from: Option<String>, to: Option<String> },
}

/// Start watching `path`. Every relevant change posts a
/// `WMAPP_WATCH_EVENT` to `hwnd` with a boxed `(root, WatchEvent)`.
///
/// Returns the `RecommendedWatcher` so the caller can keep it alive — the
/// watcher stops when dropped.
pub fn watch(path: NavPath, hwnd: HwndSend) -> notify::Result<RecommendedWatcher> {
    let root = path.clone();
    let target_dir: PathBuf = path.as_path().to_path_buf();
    // Store the HWND as a plain `isize` so the closure captures a `Send`
    // field (Rust 2021 partial-capture would otherwise see
    // `hwnd.0: HWND`, which is not `Send`). We rebuild the HWND inside
    // the callback before calling `PostMessageW`.
    let hwnd_raw: isize = hwnd.0.0 as isize;

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return; };
        for ev in simplify(&event) {
            let payload = Box::new((root.clone(), ev));
            unsafe {
                let h = windows::Win32::Foundation::HWND(hwnd_raw as *mut _);
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(h),
                    WMAPP_WATCH_EVENT,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(Box::into_raw(payload) as isize),
                );
            }
        }
    })?;
    watcher.watch(&target_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Convert a `notify::Event` into zero or more `WatchEvent`s. One notify
/// event can carry multiple paths (renames especially); we emit one
/// `WatchEvent` per path so the UI handles them one at a time.
fn simplify(event: &Event) -> Vec<WatchEvent> {
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

    let names: Vec<String> = event.paths.iter()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
        .collect();

    match event.kind {
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder | CreateKind::Other) => {
            names.into_iter().map(WatchEvent::Added).collect()
        }
        EventKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | RemoveKind::Other) => {
            names.into_iter().map(WatchEvent::Removed).collect()
        }
        EventKind::Modify(ModifyKind::Name(mode)) => {
            // Rename can arrive as split From + To events (one path each) or
            // a combined Both event (two paths). Handle both shapes.
            match mode {
                RenameMode::From => names.into_iter().map(|n| {
                    WatchEvent::Renamed { from: Some(n), to: None }
                }).collect(),
                RenameMode::To => names.into_iter().map(|n| {
                    WatchEvent::Renamed { from: None, to: Some(n) }
                }).collect(),
                RenameMode::Both => {
                    let mut it = names.into_iter();
                    let from = it.next();
                    let to = it.next();
                    vec![WatchEvent::Renamed { from, to }]
                }
                _ => names.into_iter().map(WatchEvent::Modified).collect(),
            }
        }
        EventKind::Modify(_) => {
            names.into_iter().map(WatchEvent::Modified).collect()
        }
        _ => Vec::new(),
    }
}
