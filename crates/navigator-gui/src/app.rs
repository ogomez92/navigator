//! Top-level application glue: owns the model, the speech sink, the
//! background scan worker, and the clipboard for cut/copy.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::thread;

use crossbeam_channel::{unbounded, Sender};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tracing::{error, warn};

use navigator_config::ConfigHandle;
use navigator_core::NavPath;
use navigator_fs::read_dir;
use navigator_plugin_api::host::HostCallbacks;
use navigator_rclone::{Operation, OverwritePolicy, RcloneDriver};

use crate::plugins::{Host as PluginHost, PluginRegistry};

use crate::history::History;
use crate::model::{Filter, Model};
use crate::speech::SpeechSink;
use crate::window::{
    create as create_window, run_message_loop, HwndSend,
    WMAPP_DIR_ERROR, WMAPP_DIR_LISTED, WMAPP_SEARCH_RESULTS,
};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub struct AppConfig {
    pub initial_path: NavPath,
    pub plugin_dir: Option<PathBuf>,
    pub config: ConfigHandle,
}

impl AppConfig {
    pub fn with_defaults() -> Self {
        Self {
            initial_path: NavPath::default_root(),
            plugin_dir: None,
            config: ConfigHandle::load_or_default(),
        }
    }
}

enum ScanCmd {
    List(NavPath, HwndSend),
    Search { root: NavPath, query: String, hwnd: HwndSend },
    #[allow(dead_code)]
    Shutdown,
}

pub struct AppState {
    pub initial_path: NavPath,
    pub model: Model,
    pub speech: SpeechSink,
    pub rclone: RcloneDriver,
    pub config: ConfigHandle,
    plugin_reg: OnceCell<Arc<PluginRegistry>>,
    hwnd: Mutex<Option<HwndSend>>,
    scan_tx: Sender<ScanCmd>,
    history: Mutex<History>,
    /// Suppress the next `navigate` pushing onto the history stack. Used so
    /// `back`/`forward` can navigate without rewriting the trail.
    suppress_history: Mutex<bool>,
    /// Active file watcher. Dropped automatically when replaced, so each
    /// navigation cleanly unsubscribes from the previous directory.
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Child directory to re-focus after the next successful listing.
    /// Set by `navigate_up` so Backspace / Alt+Up returns the caret to
    /// the folder the user just left, the way Explorer does.
    pending_focus: Mutex<Option<NavPath>>,
    /// Incremental type-ahead prefix for the listview. We drive type-ahead
    /// ourselves instead of letting `SysListView32` accumulate chars in
    /// its private buffer — that buffer can't be cleared externally, so
    /// after navigating into a new folder a stale prefix would still
    /// match. Tuple: (prefix, last key tick).
    type_ahead: Mutex<(String, std::time::Instant)>,
    /// LIFO stack of reversible actions. In-memory only (does not persist
    /// across runs); bounded to `UNDO_STACK_MAX`. Push on every mutating
    /// op, pop on `op_undo`.
    undo_stack: Mutex<Vec<UndoAction>>,
    /// Self-referential weak pointer populated in `new` so worker
    /// threads can reach back into the `AppState` (e.g. to set
    /// `pending_focus` after a revert-delete) without us having to
    /// change every method signature to take `self: &Arc<Self>`.
    self_weak: OnceCell<Weak<AppState>>,
}

const UNDO_STACK_MAX: usize = 50;

/// Record enough to reverse a prior operation. Paste reversal shells out
/// to a worker thread like the forward op does, so the UI stays
/// responsive and progress announcements flow through the usual channel.
#[derive(Debug, Clone)]
enum UndoAction {
    /// Reverse a copy / cut / append-clipboard — just restore the
    /// previous clip file.
    ClipChange { prev: crate::clipboard::ClipFile },
    /// Reverse a paste. `created[i]` is the new path at dest for
    /// `originals[i]`. Copy-mode undo deletes `created`; cut-mode undo
    /// moves each `created[i]` back to `originals[i]`.
    Paste {
        created: Vec<NavPath>,
        originals: Vec<NavPath>,
        cut_mode: bool,
    },
    /// Reverse a delete. Each `(trash_path, original)` pair was the
    /// target of a trash-rename during `op_delete`; undo moves the
    /// trash entry back to its original path.
    Delete { pairs: Vec<(NavPath, NavPath)> },
}

/// Find the filesystem-root of `path` — `C:\` for a drive-letter path,
/// `\\host\share\` for UNC, `/` on non-Windows. Pure (no IO); surfaced
/// as its own function for testability.
pub fn volume_root_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors().last().map(|p| p.to_path_buf())
}

/// Create a fresh trash subdirectory on the same drive/volume as `path`,
/// e.g. `C:\.trash\<ts>_<n>\`. Keeping trash on the same volume means
/// `Operation::Rename` is a true O(1) move instead of a cross-drive
/// copy+delete, and keeps each drive self-contained (unplugging the
/// drive doesn't strand trash on another volume). Dir name is
/// `<unix_ts>_<counter>` — counter is monotonic within the process so
/// rapid successive deletes don't collide.
fn make_trash_dir_on_volume_of(path: &NavPath) -> Option<NavPath> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ts = crate::clipboard::now_ts();
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let root = volume_root_of(path.as_path())?;
    let dir = root.join(".trash").join(format!("{}_{}", ts, n));
    std::fs::create_dir_all(&dir).ok()?;
    NavPath::new(dir).ok()
}

impl AppState {
    pub fn new(cfg: &AppConfig) -> Arc<Self> {
        let (tx, rx) = unbounded::<ScanCmd>();
        thread::Builder::new()
            .name("navigator-scan".into())
            .spawn(move || scan_worker(rx))
            .expect("spawn scan worker");

        let model = Model::new();
        // Seed filter + sort from persisted config so the first scan shows
        // whatever the user saw last time.
        {
            let g = cfg.config.read();
            model.set_filter(Filter {
                show_hidden: g.general.show_hidden,
                show_system: g.general.show_system,
            });
            model.set_sort(crate::model::Sort {
                mode: g.general.sort_mode,
                descending: g.general.sort_descending,
            });
        }

        let me = Arc::new(Self {
            initial_path: cfg.initial_path.clone(),
            model,
            speech: SpeechSink::start(),
            rclone: RcloneDriver::from_path(),
            config: cfg.config.clone(),
            plugin_reg: OnceCell::new(),
            hwnd: Mutex::new(None),
            scan_tx: tx,
            history: Mutex::new(History::default()),
            suppress_history: Mutex::new(false),
            watcher: Mutex::new(None),
            pending_focus: Mutex::new(None),
            type_ahead: Mutex::new((String::new(), std::time::Instant::now())),
            undo_stack: Mutex::new(Vec::new()),
            self_weak: OnceCell::new(),
        });
        let _ = me.self_weak.set(Arc::downgrade(&me));
        me
    }

    /// Build the plugin host, load any plugins on disk, and wire the nav
    /// bridge thread. Call once after `new`.
    pub fn bootstrap_plugins(self: &Arc<Self>) {
        // Nav bridge: plugins push path strings → a worker forwards them
        // into `AppState::navigate`. We use a weak ref so the thread
        // terminates when the app is dropped.
        let (nav_tx, nav_rx) = unbounded::<NavPath>();
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("navigator-plugin-nav".into())
            .spawn(move || {
                while let Ok(p) = nav_rx.recv() {
                    let Some(s) = weak.upgrade() else { break; };
                    s.navigate(p);
                }
            })
            .expect("spawn plugin nav bridge");

        let host: Arc<dyn HostCallbacks> = Arc::new(PluginHost::new(self.speech.handle(), nav_tx));
        let reg = Arc::new(PluginRegistry::new(host));

        let dir = navigator_config::plugin_dir();
        reg.load_from_dir(&dir);
        let _ = self.plugin_reg.set(reg);
    }

    pub fn plugin_registry(&self) -> Option<&Arc<PluginRegistry>> {
        self.plugin_reg.get()
    }

    /// Ask the ListView to repaint a single visible row. Cheapest way to
    /// reflect a Modify watcher event: send `LVM_REDRAWITEMS` bracketing
    /// the single row, which forces the control to re-query that row via
    /// `LVN_GETDISPINFO`.
    pub fn invalidate_row(&self, vis_idx: usize) {
        let Some(h) = self.hwnd() else { return; };
        // LVM_REDRAWITEMS = 0x1015. wParam = first, lParam = last.
        // ListView handle is a child of the main window; we post to the
        // main hwnd which routes to the listview via its registered id.
        // Easier: send via the actual listview handle. We don't have it
        // directly here, so ask the main window to resolve it.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(h.0),
                crate::window::WMAPP_REDRAW_ROW,
                windows::Win32::Foundation::WPARAM(vis_idx),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }

    /// Start watching `path`, replacing any previous watcher. Called by the
    /// window when a directory listing finishes. ThisPC is a virtual view
    /// — no real directory to watch, so we just drop any old watcher.
    pub fn watch_cwd(&self, path: &NavPath) {
        if path.is_this_pc() {
            *self.watcher.lock() = None;
            return;
        }
        let Some(hwnd) = self.hwnd() else { return; };
        match crate::watcher::watch(path.clone(), hwnd) {
            Ok(w) => { *self.watcher.lock() = Some(w); }
            Err(e) => { warn!("file watcher failed: {e}"); *self.watcher.lock() = None; }
        }
    }

    /// Fold a filesystem change event into the model.
    pub fn on_watch_event(&self, root: NavPath, ev: crate::watcher::WatchEvent) {
        // Only consume events for the currently-displayed directory; a
        // stale event from a previous cwd should not mutate the new view.
        if self.model.cwd().as_ref() != Some(&root) { return; }
        if self.model.is_search_mode() { return; }

        let append_mode = self.config.read().general.new_items_at_bottom;

        match ev {
            crate::watcher::WatchEvent::Added(name) => {
                if append_mode {
                    if let Some(e) = single_entry(&root, &name) {
                        self.model.append_entries(vec![e]);
                    }
                } else {
                    self.refresh();
                }
            }
            crate::watcher::WatchEvent::Removed(name) => {
                self.model.remove_by_name(&name);
            }
            crate::watcher::WatchEvent::Modified(name) => {
                // Re-stat the file and replace the cached entry so size +
                // mtime columns reflect reality. Return the visible index
                // via AppState → the window handler invalidates that row.
                if let Some(new) = single_entry(&root, &name) {
                    if let Some(vis_idx) = self.model.update_entry(&name, new) {
                        self.invalidate_row(vis_idx);
                    }
                }
            }
            crate::watcher::WatchEvent::Renamed { from, to } => {
                if let Some(f) = from { self.model.remove_by_name(&f); }
                if let Some(t) = to {
                    if let Some(e) = single_entry(&root, &t) {
                        if append_mode {
                            self.model.append_entries(vec![e]);
                        } else {
                            self.refresh();
                        }
                    }
                }
            }
        }
    }

    /// Snapshot of configured shortcut actions.
    pub fn actions(&self) -> Vec<navigator_config::ShortcutAction> {
        self.config.read().shortcuts.clone()
    }

    /// Run a shortcut action against the currently focused / selected entry.
    pub fn run_action(&self, action: &navigator_config::ShortcutAction) {
        tracing::info!("run_action: {:?} cmd={:?} args={:?}", action.name, action.command, action.args);
        let mut paths = self.model.selected_paths();
        tracing::info!("run_action: {} selected path(s)", paths.len());
        if paths.is_empty() {
            // Fall back to the focused row if there is no selection.
            let sel = self.model.selection_snapshot();
            tracing::info!("run_action: selection snapshot focus={:?} len={}", sel.focus(), sel.len());
            if let (Some(idx), Some(cwd)) = (sel.focus(), self.model.cwd()) {
                if let Some(e) = self.model.get(idx) {
                    let p = cwd.join(&e.name);
                    tracing::info!("run_action: fallback to focused entry {:?}", p.to_string());
                    paths.push(p);
                }
            }
        }
        if paths.is_empty() {
            tracing::warn!("run_action: no target — aborting");
            self.say("nothing selected", false);
            return;
        }
        let targets: &[NavPath] = if action.single { &paths[..1] } else { &paths[..] };
        for p in targets {
            tracing::info!("run_action: spawning {:?} for target {:?}", action.command, p.to_string());
            match crate::actions::spawn_action(action, p) {
                Ok(()) => tracing::info!("run_action: spawn OK"),
                Err(e) => {
                    error!("action {:?} failed: {}", action.name, e);
                    crate::dialogs::show_error(self.hwnd(), "Action failed", &format!("{}: {}", action.name, e));
                }
            }
        }
    }

    pub fn set_hwnd(&self, hwnd: HWND) { *self.hwnd.lock() = Some(HwndSend(hwnd)); }
    fn hwnd(&self) -> Option<HwndSend> { *self.hwnd.lock() }

    /// Public accessor for the main-window HWND. Modules that schedule
    /// UI-thread work (accelerator rebuild, error dialogs) need it.
    pub fn main_hwnd(&self) -> Option<HWND> { self.hwnd().map(|h| h.0) }

    pub fn say(&self, text: &str, interrupt: bool) { self.speech.say(text, interrupt); }

    pub fn navigate(&self, path: NavPath) {
        let Some(hwnd) = self.hwnd() else {
            warn!("navigate before hwnd set; dropping");
            return;
        };
        // Clear type-ahead prefix so a letter pressed in the new folder
        // doesn't resume the previous folder's search buffer.
        self.reset_type_ahead();
        // Record in history unless the call came from back/forward, which
        // sets `suppress_history` so the cursor stays where the user put it.
        let mut suppress = self.suppress_history.lock();
        if *suppress {
            *suppress = false;
        } else {
            self.history.lock().push(path.clone());
        }
        drop(suppress);
        let _ = self.scan_tx.send(ScanCmd::List(path, hwnd));
    }

    /// Wipe the incremental type-ahead prefix. Called on navigation so a
    /// stale buffer from the previous folder doesn't carry over.
    pub fn reset_type_ahead(&self) {
        let mut g = self.type_ahead.lock();
        g.0.clear();
        g.1 = std::time::Instant::now();
    }

    /// Append `ch` to the current type-ahead prefix and find the next
    /// matching visible row. Resets the prefix if more than `timeout_ms`
    /// has elapsed since the previous keystroke, matching the
    /// Explorer-style cadence (so typing "a" … pause … "b" starts fresh
    /// instead of searching "ab"). Returns the row index of the match,
    /// or `None` if nothing matches.
    pub fn type_ahead_step(&self, ch: char) -> Option<usize> {
        const TIMEOUT_MS: u128 = 1000;
        let now = std::time::Instant::now();
        let mut g = self.type_ahead.lock();
        if now.duration_since(g.1).as_millis() > TIMEOUT_MS {
            g.0.clear();
        }
        g.0.push(ch);
        g.1 = now;
        let prefix = g.0.clone();
        drop(g);
        self.model.find_prefix(&prefix, None)
    }

    /// Navigate to the previous history entry. Silently no-ops at the start
    /// of history (announced via prism so keyboard users know).
    pub fn go_back(&self) {
        let target = self.history.lock().back().cloned();
        match target {
            Some(p) => {
                *self.suppress_history.lock() = true;
                self.navigate(p);
            }
            None => self.say("no previous folder", false),
        }
    }

    pub fn go_forward(&self) {
        let target = self.history.lock().forward().cloned();
        match target {
            Some(p) => {
                *self.suppress_history.lock() = true;
                self.navigate(p);
            }
            None => self.say("no forward folder", false),
        }
    }

    /// Navigate to `target`'s parent folder and arrange for the listing
    /// hook to re-focus `target` by name. For a drive root the parent is
    /// the virtual "This PC" view. Reuses the same `pending_focus` slot
    /// as `navigate_up`, which the dir-listed handler consumes.
    pub fn jump_to(&self, target: NavPath) {
        if target.is_this_pc() { self.navigate(target); return; }
        let parent = target.parent().unwrap_or_else(NavPath::this_pc);
        *self.pending_focus.lock() = Some(target);
        self.navigate(parent);
    }

    /// Jump to the entry saved at hotspot `slot` (1..=HOTSPOT_COUNT).
    /// Empty slot announces the fact via prism and does nothing else.
    pub fn hotspot_goto(&self, slot: u8) {
        use navigator_config::HOTSPOT_COUNT;
        if slot == 0 || slot > HOTSPOT_COUNT {
            self.say("invalid hotspot slot", false);
            return;
        }
        let idx = (slot - 1) as usize;
        let existing: String = {
            let cfg = self.config.read();
            cfg.hotspots.get(idx).cloned().unwrap_or_default()
        };
        if existing.is_empty() {
            self.say(&format!("hotspot {} empty", slot), false);
            return;
        }
        match NavPath::new(PathBuf::from(&existing)) {
            Ok(p) => {
                self.say(&format!("hotspot {}", slot), false);
                self.jump_to(p);
            }
            Err(_) => self.say(&format!("hotspot {} has invalid path", slot), false),
        }
    }

    /// Record the currently selected entry into hotspot `slot`. Overwrites
    /// any existing value. Strict single-selection gate — zero, or more
    /// than one, selected row announces an error via prism and leaves the
    /// slot untouched.
    pub fn hotspot_set(&self, slot: u8) {
        use navigator_config::HOTSPOT_COUNT;
        if slot == 0 || slot > HOTSPOT_COUNT {
            self.say("invalid hotspot slot", false);
            return;
        }
        let idx = (slot - 1) as usize;

        let paths = self.model.selected_paths();
        let target = match paths.len() {
            1 => paths.into_iter().next().unwrap(),
            0 => { self.say("nothing selected, cannot set hotspot", false); return; }
            n => { self.say(&format!("{} items selected, hotspot needs exactly one", n), false); return; }
        };

        let display = target.to_string();
        self.config.with_mut(|c| {
            if idx < c.hotspots.len() { c.hotspots[idx] = display.clone(); }
        });
        let _ = self.config.save();
        self.say(&format!("hotspot {} set to {}", slot, target.file_name()), false);
    }

    pub fn navigate_up(&self) {
        if let Some(cwd) = self.model.cwd() {
            if cwd.is_this_pc() {
                // Already above drives — nothing to pop to.
                self.say("at this pc", false);
                return;
            }
            // Remember the child so the post-listing hook can re-focus
            // on it. Matching for folders is by name; for drive roots →
            // This PC we match via `drive_path_from_display` inverse.
            *self.pending_focus.lock() = Some(cwd.clone());
            if let Some(parent) = cwd.parent() {
                self.navigate(parent);
            } else {
                // At a drive root (e.g. `C:\`). Step one level up into
                // the virtual "This PC" view so the user sees drives.
                self.navigate(NavPath::this_pc());
            }
        }
    }

    /// Take-and-clear the pending child to refocus. Called from the
    /// WMAPP_DIR_LISTED handler after the new listing is installed.
    pub fn take_pending_focus(&self) -> Option<NavPath> {
        self.pending_focus.lock().take()
    }

    /// Arm the next listing to refocus `target` (matched by filename in
    /// the post-listing hook). Shared by `navigate_up`, `jump_to`, and the
    /// revert-delete worker which wants to land focus on the restored row.
    pub fn set_pending_focus(&self, target: NavPath) {
        *self.pending_focus.lock() = Some(target);
    }

    pub fn refresh(&self) {
        if let Some(cwd) = self.model.cwd() {
            self.navigate(cwd);
        }
    }

    /// Ask the main window to tear down and rebuild the ListView's
    /// columns from current config. Used after the Options → Columns
    /// page commits a change.
    pub fn reconfigure_listview_columns(&self) {
        let Some(h) = self.hwnd() else { return; };
        unsafe {
            let _ = PostMessageW(
                Some(h.0),
                crate::window::WMAPP_RECONFIGURE_COLUMNS,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    /// Flip "show hidden". Announces the new state and refreshes the
    /// virtual ListView (caller is responsible for repainting).
    pub fn toggle_hidden(&self) {
        let mut filter = self.model.filter();
        filter.show_hidden = !filter.show_hidden;
        let count = self.model.set_filter(filter);
        self.config.with_mut(|c| c.general.show_hidden = filter.show_hidden);
        let _ = self.config.save();
        self.refresh_count_on_control(count);
        self.say(
            if filter.show_hidden { "showing hidden files" } else { "hiding hidden files" },
            false,
        );
    }

    pub fn set_sort_mode(&self, mode: navigator_config::SortMode) {
        let mut s = self.model.sort();
        s.mode = mode;
        self.model.set_sort(s);
        self.config.with_mut(|c| c.general.sort_mode = mode);
        let _ = self.config.save();
        self.say(&format!("sort by {}", match mode {
            navigator_config::SortMode::Name => "name",
            navigator_config::SortMode::Size => "size",
            navigator_config::SortMode::Type => "type",
            navigator_config::SortMode::Modified => "date modified",
            navigator_config::SortMode::Created => "date created",
        }), false);
        self.refresh();
    }

    pub fn toggle_sort_descending(&self) {
        let mut s = self.model.sort();
        s.descending = !s.descending;
        self.model.set_sort(s);
        self.config.with_mut(|c| c.general.sort_descending = s.descending);
        let _ = self.config.save();
        self.say(if s.descending { "descending" } else { "ascending" }, false);
        self.refresh();
    }

    /// Kick off a recursive search from `root` for `query` (case-insensitive
    /// substring match on file/directory names). Runs on the scan worker
    /// thread; results land back via WMAPP_SEARCH_RESULTS.
    pub fn start_search(&self, root: NavPath, query: String) {
        let Some(hwnd) = self.hwnd() else { return; };
        let _ = self.scan_tx.send(ScanCmd::Search { root, query, hwnd });
        self.say("searching", false);
    }

    pub fn toggle_system(&self) {
        let mut filter = self.model.filter();
        filter.show_system = !filter.show_system;
        let count = self.model.set_filter(filter);
        self.config.with_mut(|c| c.general.show_system = filter.show_system);
        let _ = self.config.save();
        self.refresh_count_on_control(count);
        self.say(
            if filter.show_system { "showing system files" } else { "hiding system files" },
            false,
        );
    }

    /// Post a synthetic `WMAPP_DIR_LISTED` with the current listing so the
    /// window updates its virtual count without touching the filesystem.
    fn refresh_count_on_control(&self, _count: usize) {
        // Simplest path: just re-emit the current scan by navigating to it
        // again. Cheap because `read_dir` at the cwd is already hot cache.
        if let Some(cwd) = self.model.cwd() { self.navigate(cwd); }
    }

    pub fn open_file(&self, path: NavPath) {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use std::os::windows::ffi::OsStrExt;

        let path_w: Vec<u16> = path.as_path().as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let _ = ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path_w.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    fn push_undo(&self, a: UndoAction) {
        let mut g = self.undo_stack.lock();
        g.push(a);
        if g.len() > UNDO_STACK_MAX {
            let excess = g.len() - UNDO_STACK_MAX;
            g.drain(0..excess);
        }
    }

    /// Reverse the most recent undoable action. Clipboard restores happen
    /// inline; paste reversals spawn a worker because they call rclone.
    pub fn op_undo(&self) {
        let action = { self.undo_stack.lock().pop() };
        let Some(action) = action else {
            self.say("nothing to undo", false);
            return;
        };
        match action {
            UndoAction::ClipChange { prev } => {
                crate::clipboard::save_clip(&prev);
                self.say(
                    &format!("undo: clipboard reverted ({} items)", prev.sources.len()),
                    false,
                );
            }
            UndoAction::Paste { created, originals, cut_mode } => {
                self.say(
                    &format!(
                        "undo: reverting paste of {} items",
                        created.len(),
                    ),
                    false,
                );
                let state = self.clone_for_worker();
                std::thread::Builder::new()
                    .name("navigator-undo-paste".into())
                    .spawn(move || state.run_revert_paste(created, originals, cut_mode))
                    .expect("spawn undo-paste worker");
            }
            UndoAction::Delete { pairs } => {
                self.say(
                    &format!("undo: restoring {} deleted items", pairs.len()),
                    false,
                );
                let state = self.clone_for_worker();
                std::thread::Builder::new()
                    .name("navigator-undo-delete".into())
                    .spawn(move || state.run_revert_delete(pairs))
                    .expect("spawn undo-delete worker");
            }
        }
    }

    pub fn op_copy(&self) {
        let paths = self.model.selected_paths();
        if paths.is_empty() { self.say("nothing selected", false); return; }
        let n = paths.len();
        let sources: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let prev = crate::clipboard::load_clip();
        crate::clipboard::save_clip(&crate::clipboard::ClipFile {
            sources: sources.clone(),
            cut: false,
            ts: crate::clipboard::now_ts(),
        });
        crate::clipboard::push_history(crate::clipboard::HistoryEntry {
            kind: "copy".into(),
            sources,
            dest: None,
            ts: crate::clipboard::now_ts(),
        });
        self.push_undo(UndoAction::ClipChange { prev });
        self.say(&format!("{} items copied to clipboard", n), false);
    }

    pub fn op_cut(&self) {
        let paths = self.model.selected_paths();
        if paths.is_empty() { self.say("nothing selected", false); return; }
        let n = paths.len();
        let sources: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let prev = crate::clipboard::load_clip();
        crate::clipboard::save_clip(&crate::clipboard::ClipFile {
            sources: sources.clone(),
            cut: true,
            ts: crate::clipboard::now_ts(),
        });
        crate::clipboard::push_history(crate::clipboard::HistoryEntry {
            kind: "cut".into(),
            sources,
            dest: None,
            ts: crate::clipboard::now_ts(),
        });
        self.push_undo(UndoAction::ClipChange { prev });
        self.say(&format!("{} items cut to clipboard", n), false);
    }

    /// Add current selection to the existing clipboard set. `cut_mode`
    /// switches between append-to-copy and append-to-cut. If the clipboard
    /// currently holds the opposite mode (or is empty), it's replaced
    /// rather than mixed — mixing cut and copy sources in one paste has
    /// no sensible semantics.
    pub fn op_append_clipboard(&self, cut_mode: bool) {
        let incoming = self.model.selected_paths();
        if incoming.is_empty() { self.say("nothing selected", false); return; }

        let mut clip = crate::clipboard::load_clip();
        let prev = clip.clone();
        let incoming_s: Vec<String> = incoming.iter().map(|p| p.to_string()).collect();

        if clip.sources.is_empty() || clip.cut != cut_mode {
            let n = incoming_s.len();
            clip = crate::clipboard::ClipFile {
                sources: incoming_s.clone(),
                cut: cut_mode,
                ts: crate::clipboard::now_ts(),
            };
            crate::clipboard::save_clip(&clip);
            crate::clipboard::push_history(crate::clipboard::HistoryEntry {
                kind: if cut_mode { "cut".into() } else { "copy".into() },
                sources: incoming_s,
                dest: None,
                ts: crate::clipboard::now_ts(),
            });
            self.say(
                &format!(
                    "{} items {} to clipboard",
                    n,
                    if cut_mode { "cut" } else { "copied" },
                ),
                false,
            );
            return;
        }

        // Same mode — append, skipping duplicates so a second press on the
        // same row doesn't double-book it.
        let mut added_paths: Vec<String> = Vec::new();
        for p in incoming_s {
            if !clip.sources.iter().any(|x| x == &p) {
                clip.sources.push(p.clone());
                added_paths.push(p);
            }
        }
        clip.ts = crate::clipboard::now_ts();
        let total = clip.sources.len();
        let added = added_paths.len();
        crate::clipboard::save_clip(&clip);
        if added > 0 {
            crate::clipboard::push_history(crate::clipboard::HistoryEntry {
                kind: if cut_mode { "append-cut".into() } else { "append-copy".into() },
                sources: added_paths,
                dest: None,
                ts: crate::clipboard::now_ts(),
            });
            self.push_undo(UndoAction::ClipChange { prev });
        }
        self.say(
            &format!(
                "{} added, {} total in {} clipboard",
                added,
                total,
                if cut_mode { "cut" } else { "copy" },
            ),
            false,
        );
    }

    pub fn op_paste(&self) {
        let Some(dest) = self.model.cwd() else { return; };
        let clip = crate::clipboard::load_clip();
        if clip.sources.is_empty() { self.say("clipboard empty", false); return; }

        // Rehydrate string paths to NavPaths; skip any that are no longer
        // absolute (manually-edited file, mount unplugged, etc.).
        let sources: Vec<NavPath> = clip.sources.iter()
            .filter_map(|s| NavPath::new(PathBuf::from(s)).ok())
            .collect();
        if sources.is_empty() { self.say("clipboard paths invalid", false); return; }

        crate::clipboard::push_history(crate::clipboard::HistoryEntry {
            kind: "paste".into(),
            sources: clip.sources.clone(),
            dest: Some(dest.to_string()),
            ts: crate::clipboard::now_ts(),
        });

        // Record undo BEFORE spawning the worker so Ctrl+Z can target the
        // paste even if it's still in flight. `run_batch` may skip
        // individual items on user "Skip" — the undo attempt will just
        // no-op on those missing paths.
        let created: Vec<NavPath> = sources.iter()
            .map(|s| dest.join(s.file_name()))
            .collect();
        self.push_undo(UndoAction::Paste {
            created,
            originals: sources.clone(),
            cut_mode: clip.cut,
        });

        // Fan out to one Operation per source — a single OpHandle per file
        // keeps stats attribution clean and lets the progress window show
        // "file N of M" without having to rebuild rclone's stat stream.
        let cut = clip.cut;
        let state = self.clone_for_worker();
        std::thread::Builder::new()
            .name("navigator-batch-op".into())
            .spawn(move || state.run_batch(sources, dest, cut))
            .expect("spawn batch worker");
    }

    pub fn op_delete(&self) {
        let paths = self.model.selected_paths();
        if paths.is_empty() { self.say("nothing selected", false); return; }
        crate::clipboard::push_history(crate::clipboard::HistoryEntry {
            kind: "delete".into(),
            sources: paths.iter().map(|p| p.to_string()).collect(),
            dest: None,
            ts: crate::clipboard::now_ts(),
        });

        // Pick the row to land focus on after the delete completes so the
        // caret doesn't jump to row 0. Prefer the first unselected row at
        // or after the lowest selected index (Explorer behaviour). Fall
        // back to the last unselected row before the selection when the
        // tail of the listing was deleted. `None` = everything selected,
        // leave pending_focus empty.
        let next_focus = self.pick_post_delete_focus();
        if let Some(target) = next_focus {
            *self.pending_focus.lock() = Some(target);
        }

        // Move each target to `<volume_root>/.trash/<ts>_<n>/<basename>`
        // on the same drive as the source. Same-volume keeps the rename
        // atomic and avoids stranding trash on an unrelated volume.
        // Multiple targets on the same drive each get their own dir so
        // identical basenames across different selections don't clobber.
        let mut pairs: Vec<(NavPath, NavPath)> = Vec::with_capacity(paths.len());
        for p in &paths {
            let Some(trash_dir) = make_trash_dir_on_volume_of(p) else {
                self.say(&format!("failed to create trash dir for {}", p.file_name()), true);
                continue;
            };
            let trash_path = trash_dir.join(p.file_name());
            pairs.push((trash_path, p.clone()));
        }
        if pairs.is_empty() {
            self.say("delete targets resolved to nothing", true);
            return;
        }
        self.push_undo(UndoAction::Delete { pairs: pairs.clone() });

        let state = self.clone_for_worker();
        std::thread::Builder::new()
            .name("navigator-batch-delete".into())
            .spawn(move || state.run_trash_batch(pairs))
            .expect("spawn delete batch");
    }

    /// Compute the `pending_focus` target the listing hook should land on
    /// after a delete's refresh. Walks visible rows and returns the path
    /// of the first non-selected row at or after the lowest selected
    /// index, falling back to the last non-selected row before the
    /// selection for tail deletes. Returns `None` when every visible row
    /// is selected — nothing survives, so there's nothing to focus.
    fn pick_post_delete_focus(&self) -> Option<NavPath> {
        let cwd = self.model.cwd()?;
        let sel = self.model.selection_snapshot();
        let total = self.model.len();
        if total == 0 || sel.is_empty() { return None; }

        let selected: std::collections::HashSet<usize> = sel.iter().collect();
        let min_sel = selected.iter().min().copied()?;

        // First unselected row at or after min_sel.
        for i in min_sel..total {
            if !selected.contains(&i) {
                let e = self.model.get(i)?;
                return Some(cwd.join(&e.name));
            }
        }
        // None found — selection runs to the tail. Walk back from min_sel
        // to pick the nearest surviving predecessor.
        for i in (0..min_sel).rev() {
            if !selected.contains(&i) {
                let e = self.model.get(i)?;
                return Some(cwd.join(&e.name));
            }
        }
        None
    }

    /// Reinstate a clipboard from a history entry (user clicked a "Recent
    /// operations" menu item). Filters out paths that no longer exist,
    /// since the user may have moved / deleted them since the entry was
    /// recorded. Announces counts for both outcomes so keyboard users
    /// know what actually went in.
    pub fn op_restore_from_history(&self, idx: usize) {
        let entries = crate::clipboard::load_history();
        let Some(entry) = entries.get(idx) else {
            self.say("history entry no longer exists", false);
            return;
        };
        let (present, missing): (Vec<String>, Vec<String>) = entry.sources
            .iter()
            .cloned()
            .partition(|p| std::path::Path::new(p).exists());
        if present.is_empty() {
            self.say(
                &format!(
                    "all {} paths missing; clipboard unchanged",
                    missing.len(),
                ),
                false,
            );
            return;
        }
        let cut = matches!(entry.kind.as_str(), "cut" | "append-cut");
        let prev = crate::clipboard::load_clip();
        crate::clipboard::save_clip(&crate::clipboard::ClipFile {
            sources: present.clone(),
            cut,
            ts: crate::clipboard::now_ts(),
        });
        self.push_undo(UndoAction::ClipChange { prev });
        if missing.is_empty() {
            self.say(&format!("{} items restored to clipboard", present.len()), false);
        } else {
            self.say(
                &format!(
                    "{} items restored, {} missing skipped",
                    present.len(), missing.len(),
                ),
                false,
            );
        }
    }

    /// Cheap snapshot of the bits worker threads need. Keeps `AppState` out
    /// of the closure so we don't leak `Arc<Self>` into threads that only
    /// need to speak + spawn rclone.
    fn clone_for_worker(&self) -> WorkerCtx {
        let (progress_on, announce_interval_secs) = {
            let g = self.config.read();
            (g.general.progress_window, g.general.announce_interval_secs)
        };
        let progress = if progress_on {
            self.hwnd().and_then(|h| crate::progress::open(h.0).ok())
        } else { None };
        WorkerCtx {
            rclone: self.rclone.clone(),
            speech: self.speech.handle(),
            scan_tx: self.scan_tx.clone(),
            refresh_target: self.model.cwd(),
            hwnd: self.hwnd(),
            progress,
            announce_interval_secs,
            state: self.self_weak.get().cloned().unwrap_or_else(Weak::new),
        }
    }

    /// Copy the full path(s) of the current selection to the Windows
    /// clipboard as CF_UNICODETEXT. Paths containing whitespace get wrapped
    /// in double quotes so the result is paste-safe into a shell or args
    /// field. Joined with CR-LF for multi-select.
    pub fn op_copy_paths(&self) {
        let paths = self.model.selected_paths();
        if paths.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        let text = paths.iter()
            .map(|p| {
                let s = p.to_string();
                if s.chars().any(|c| c.is_whitespace()) {
                    format!("\"{}\"", s)
                } else {
                    s
                }
            })
            .collect::<Vec<_>>()
            .join("\r\n");

        let n = paths.len();
        match set_clipboard_text(self.main_hwnd(), &text) {
            Ok(()) => {
                let msg = if n == 1 {
                    "path copied".to_string()
                } else {
                    format!("{} paths copied", n)
                };
                self.say(&msg, false);
            }
            Err(e) => {
                self.say(&format!("clipboard failed: {}", e), true);
            }
        }
    }

    /// Rename `old_name` → `new_name` within the current directory.
    pub fn op_rename(&self, old_name: &str, new_name: &str) {
        let Some(cwd) = self.model.cwd() else { return; };
        let src = cwd.join(old_name);
        let dst = cwd.join(new_name);
        self.spawn_op(Operation::Rename { src, dst });
    }

    /// Kick off a single-shot op (rename / one-file). For multi-source
    /// batches see [`run_batch`].
    fn spawn_op(&self, op: Operation) {
        let ctx = self.clone_for_worker();
        thread::Builder::new()
            .name("navigator-rclone-op".into())
            .spawn(move || ctx.run_single(op))
            .expect("spawn rclone op thread");
    }
}

/// Thread-safe context passed to worker closures. Every field is cheap to
/// clone; none of them borrows from `AppState`.
#[derive(Clone)]
struct WorkerCtx {
    rclone: RcloneDriver,
    speech: Sender<crate::speech::Utterance>,
    scan_tx: Sender<ScanCmd>,
    refresh_target: Option<NavPath>,
    hwnd: Option<HwndSend>,
    progress: Option<crate::progress::ProgressHandle>,
    /// Seconds between prism progress utterances. `0` disables periodic
    /// speech and only the final "done" / error announcement is emitted.
    announce_interval_secs: u32,
    /// Weak handle back to the owning AppState. Workers use it to schedule
    /// UI-thread follow-ups such as arming `pending_focus` before the
    /// refresh scan fires. Weak so a dropped app doesn't keep the state
    /// alive via the worker thread.
    state: Weak<AppState>,
}

impl WorkerCtx {
    fn say(&self, text: impl Into<String>, interrupt: bool) {
        let _ = self.speech.try_send(crate::speech::Utterance { text: text.into(), interrupt });
    }

    fn refresh(&self) {
        if let (Some(path), Some(hwnd)) = (self.refresh_target.clone(), self.hwnd) {
            let _ = self.scan_tx.send(ScanCmd::List(path, hwnd));
        }
    }

    fn run_single(self, op: Operation) {
        let ok = self.run_one(op);
        self.say(if ok { "done" } else { "operation failed" }, !ok);
        self.refresh();
    }

    fn run_batch(self, sources: Vec<NavPath>, dest_dir: NavPath, cut: bool) {
        use crate::preflight::{prompt_item, unique_numbered_path, BatchDecision, ItemChoice};

        let total = sources.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        let mut renamed = 0u32;

        // Sticky decision after the user ticks "apply to all". `None` means
        // we still ask per-conflict.
        let mut sticky: Option<ItemChoice> = None;
        // Count how many conflicts remain so the dialog can show progress.
        let mut remaining_conflicts: usize = sources.iter()
            .filter(|s| dest_dir.join(s.file_name()).as_path().exists())
            .count();

        for (i, src) in sources.into_iter().enumerate() {
            let dst_name = src.file_name().to_string();
            let dst = dest_dir.join(&dst_name);
            let dst_exists = dst.as_path().exists();

            let choice = if !dst_exists {
                ItemChoice::Overwrite // no conflict
            } else if let Some(s) = sticky {
                s
            } else {
                let BatchDecision { choice, sticky: s } =
                    prompt_item(self.hwnd, &src, &dst, remaining_conflicts);
                if s { sticky = Some(choice); }
                remaining_conflicts = remaining_conflicts.saturating_sub(1);
                choice
            };

            match choice {
                ItemChoice::Cancel => {
                    self.say("cancelled", true);
                    break;
                }
                ItemChoice::Skip => {
                    if dst_exists {
                        skipped += 1;
                        self.say(format!("skipped {}", dst_name), false);
                    }
                    continue;
                }
                ItemChoice::Overwrite | ItemChoice::Rename => { /* fall through */ }
            }

            let (op, effective_name) = if matches!(choice, ItemChoice::Rename) && dst_exists {
                // Pick a fresh sibling name and drive the op through
                // Rename/CopyTo so rclone writes to the new path instead
                // of clobbering the existing one.
                let new_dst_pb = unique_numbered_path(dst.as_path());
                match NavPath::new(new_dst_pb.clone()) {
                    Ok(new_dst) => {
                        let new_name = new_dst.file_name().to_string();
                        renamed += 1;
                        let op = if cut {
                            Operation::Rename { src, dst: new_dst }
                        } else {
                            Operation::CopyTo { src, dst: new_dst }
                        };
                        (op, new_name)
                    }
                    Err(_) => {
                        // Could not construct a valid NavPath — skip this
                        // item so we don't accidentally overwrite.
                        skipped += 1;
                        self.say(format!("skipped {} (rename failed)", dst_name), false);
                        continue;
                    }
                }
            } else {
                let policy = OverwritePolicy::Always;
                let op = if cut {
                    Operation::Rename { src, dst }
                } else {
                    Operation::Copy {
                        sources: vec![src],
                        dest_dir: dest_dir.clone(),
                        policy,
                    }
                };
                (op, dst_name)
            };

            self.say(format!("{} of {}: {}", i + 1, total, effective_name), false);
            if !self.run_one(op) {
                failed += 1;
            }
        }
        let msg = match (failed, skipped, renamed) {
            (0, 0, 0) => format!("done — {} items", total),
            (0, 0, r) => format!("done — {} items, {} renamed", total, r),
            (0, s, 0) => format!("done — {} items, {} skipped", total - s as usize, s),
            (0, s, r) => format!("done — {} items, {} skipped, {} renamed", total - s as usize, s, r),
            (f, s, _) => format!("finished with {} failures, {} skipped, out of {}", f, s, total),
        };
        self.say(msg, failed > 0);
        self.refresh();
    }

    /// Reverse a paste. For copy-mode, delete each created entry. For
    /// cut-mode, move each `created[i]` back to `originals[i]`. Missing
    /// paths are skipped silently — the paste may have been partially
    /// rejected (user clicked Skip) or another process may have already
    /// cleaned things up. All results fold into a single summary.
    fn run_revert_paste(
        self,
        created: Vec<NavPath>,
        originals: Vec<NavPath>,
        cut_mode: bool,
    ) {
        let total = created.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        for (i, c) in created.iter().enumerate() {
            if !c.as_path().exists() {
                skipped += 1;
                continue;
            }
            let op = if cut_mode {
                Operation::Rename { src: c.clone(), dst: originals[i].clone() }
            } else {
                Operation::Delete { targets: vec![c.clone()] }
            };
            self.say(format!("undo {} of {}: {}", i + 1, total, c.file_name()), false);
            if !self.run_one(op) { failed += 1; }
        }
        let msg = if failed == 0 && skipped == 0 {
            format!("undo done — {} items", total)
        } else if failed == 0 {
            format!("undo done — {} items, {} missing skipped", total - skipped as usize, skipped)
        } else {
            format!("undo finished with {} failures, {} skipped, out of {}", failed, skipped, total)
        };
        self.say(msg, failed > 0);
        self.refresh();
    }

    /// Rename each target into the staging trash folder instead of
    /// rclone-purging it. The pair list is (trash_path, original) so a
    /// future undo can move each entry back to its original location.
    fn run_trash_batch(self, pairs: Vec<(NavPath, NavPath)>) {
        let total = pairs.len();
        let mut failed = 0u32;
        for (i, (trash, original)) in pairs.into_iter().enumerate() {
            self.say(format!("deleting {} of {}: {}", i + 1, total, original.file_name()), false);
            let op = Operation::Rename { src: original, dst: trash };
            if !self.run_one(op) { failed += 1; }
        }
        let msg = if failed == 0 {
            format!("deleted {} items (undoable)", total)
        } else {
            format!("delete finished with {} failures out of {}", failed, total)
        };
        self.say(msg, failed > 0);
        self.refresh();
    }

    /// Reverse a delete. Each `(trash, original)` gets renamed back. If
    /// the original path has been repopulated by something new, we skip
    /// that entry rather than clobbering the user's fresh file.
    fn run_revert_delete(self, pairs: Vec<(NavPath, NavPath)>) {
        let total = pairs.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        // First successfully restored original — used to re-focus the row
        // after the refresh so the user lands back on (one of) the
        // undeleted items.
        let mut first_restored: Option<NavPath> = None;
        for (i, (trash, original)) in pairs.into_iter().enumerate() {
            if !trash.as_path().exists() {
                skipped += 1;
                continue;
            }
            if original.as_path().exists() {
                // New item at original path — don't overwrite.
                skipped += 1;
                continue;
            }
            self.say(format!("restoring {} of {}: {}", i + 1, total, original.file_name()), false);
            if !self.run_one(Operation::Rename { src: trash, dst: original.clone() }) {
                failed += 1;
            } else if first_restored.is_none() {
                first_restored = Some(original);
            }
        }
        let msg = if failed == 0 && skipped == 0 {
            format!("restored {} items", total)
        } else if failed == 0 {
            format!("restored {} items, {} skipped", total - skipped as usize, skipped)
        } else {
            format!("restore finished with {} failures, {} skipped, out of {}", failed, skipped, total)
        };
        self.say(msg, failed > 0);
        // Arm pending_focus so the post-listing hook lands the caret on
        // the restored row. Only fires when the refresh target is the same
        // directory the item was restored into, since refocus_after_up
        // matches by filename within the new listing.
        if let (Some(state), Some(target)) = (self.state.upgrade(), first_restored) {
            state.set_pending_focus(target);
        }
        self.refresh();
    }

    /// Run one rclone process synchronously. Returns `true` on success.
    /// Errors are always surfaced via a modal dialog, regardless of the
    /// progress-window preference.
    fn run_one(&self, op: Operation) -> bool {
        let handle = match self.rclone.spawn(op) {
            Ok(h) => h,
            Err(e) => {
                error!("rclone spawn: {e}");
                self.say(format!("failed to start: {e}"), true);
                crate::dialogs::show_error(self.hwnd, "rclone failed to start", &e.to_string());
                return false;
            }
        };

        // Wire the progress window if enabled. We can't open windows from
        // a worker thread, so the UI thread opens it via a synchronous
        // SendMessage when the first op starts — but for now we just
        // route via the handle which the caller attached before spawn.
        let progress = self.progress.clone();

        // Throttle periodic speech announcements. `0` = disabled, only
        // emit the final outcome; otherwise announce once per interval.
        let interval = if self.announce_interval_secs == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(self.announce_interval_secs as u64))
        };
        let mut last_spoken = std::time::Instant::now();

        for ev in handle.events.iter() {
            match ev {
                navigator_rclone::op::OpEvent::Progress { bytes_done, bytes_total, current } => {
                    if let Some(p) = progress.as_ref() {
                        p.post_status(current.as_deref().unwrap_or(""), bytes_done, bytes_total);
                    }
                    if let Some(iv) = interval {
                        let now = std::time::Instant::now();
                        if now.duration_since(last_spoken) >= iv {
                            last_spoken = now;
                            let msg = if bytes_total > 0 {
                                let pct = (bytes_done as f64 / bytes_total as f64 * 100.0) as u32;
                                match current.as_deref() {
                                    Some(name) if !name.is_empty() =>
                                        format!("{} percent, {}", pct, name),
                                    _ => format!("{} percent", pct),
                                }
                            } else {
                                match current.as_deref() {
                                    Some(name) if !name.is_empty() => name.to_string(),
                                    _ => String::new(),
                                }
                            };
                            if !msg.is_empty() {
                                let _ = self.speech.try_send(crate::speech::Utterance {
                                    text: msg, interrupt: false,
                                });
                            }
                        }
                    }
                }
                navigator_rclone::op::OpEvent::Log(ev) => {
                    if let Some(p) = progress.as_ref() {
                        p.post_log(&format!(
                            "[{:?}] {}",
                            ev.level.unwrap_or(navigator_rclone::log::LogLevel::Info),
                            ev.msg,
                        ));
                    }
                }
                navigator_rclone::op::OpEvent::Done { success, stderr_tail } => {
                    if let Some(p) = progress.as_ref() { p.post_done(success); }
                    if !success {
                        let tail_lines: Vec<&str> = stderr_tail.lines().rev().take(10).collect();
                        let tail = tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
                        error!("rclone: {tail}");
                        crate::dialogs::show_error(
                            self.hwnd,
                            "File operation failed",
                            if tail.is_empty() { "rclone reported an error" } else { &tail },
                        );
                    }
                    return success;
                }
            }
        }
        false
    }
}

/// Stat a single child by name and return its [`Entry`]. Used by the file
/// watcher when a newly created file needs to join the virtual listing.
fn single_entry(root: &NavPath, name: &str) -> Option<navigator_core::Entry> {
    // Re-use `read_dir` and find the matching name. One directory scan is
    // cheap and gives us full attributes without an extra Win32 path.
    match navigator_fs::read_dir(root) {
        Ok(entries) => entries.into_iter().find(|e| e.name == name),
        Err(_) => None,
    }
}

/// Place UTF-16 text on the Windows clipboard.
///
/// Walks through the standard clipboard handshake:
///   1. `OpenClipboard(hwnd)` — acquires the global lock.
///   2. `EmptyClipboard` — drops previous owner's data.
///   3. Allocate GMEM_MOVEABLE, copy the UTF-16 bytes + NUL.
///   4. `SetClipboardData(CF_UNICODETEXT, hmem)` — ownership of hmem passes
///      to the system; *we must not* GlobalFree it on success.
///   5. `CloseClipboard`.
fn set_clipboard_text(hwnd: Option<windows::Win32::Foundation::HWND>, text: &str) -> std::io::Result<()> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        OpenClipboard(hwnd).map_err(io_err)?;
        // Guarded block: ensure CloseClipboard fires even on error paths.
        let result = (|| -> std::io::Result<()> {
            EmptyClipboard().map_err(io_err)?;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(io_err)?;
            if hmem.is_invalid() {
                return Err(std::io::Error::other("GlobalAlloc returned null"));
            }
            let dst = GlobalLock(hmem) as *mut u16;
            if dst.is_null() {
                let _ = GlobalFree(Some(hmem));
                return Err(std::io::Error::other("GlobalLock returned null"));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(hmem);

            match SetClipboardData(CF_UNICODETEXT.0.into(), Some(HANDLE(hmem.0))) {
                Ok(_) => Ok(()), // ownership transferred — don't free.
                Err(e) => {
                    let _ = GlobalFree(Some(hmem));
                    Err(io_err(e))
                }
            }
        })();
        let _ = CloseClipboard();
        result
    }
}

fn io_err(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(format!("{}", e))
}

fn scan_worker(rx: crossbeam_channel::Receiver<ScanCmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ScanCmd::Shutdown => break,
            ScanCmd::List(path, hwnd) => {
                // ThisPC sentinel → enumerate drives rather than reading a
                // real directory. Same flow on the UI side: entries end up
                // in the virtual listview just like regular files.
                let entries = if path.is_this_pc() {
                    navigator_fs::list_drives()
                } else if path.is_unc_host_only() {
                    // Host-only UNC (`\\host`) — `FindFirstFileW` rejects
                    // it with ERROR_BAD_NETPATH. Enumerate shares on the
                    // host instead; entries carry bare share names which
                    // the UI joins onto the host path on activate.
                    let host = path.unc_host().unwrap_or_default();
                    navigator_fs::list_shares(&host)
                } else {
                    match read_dir(&path) {
                        Ok(e) => e,
                        Err(e) => {
                            error!("read_dir {}: {}", path, e);
                            // Surface the failure to the UI so the user
                            // sees an error dialog instead of a silent
                            // "0 items" listing.
                            let payload = Box::into_raw(
                                Box::new((path.clone(), e.to_string())),
                            ) as isize;
                            unsafe {
                                let _ = PostMessageW(
                                    Some(hwnd.0),
                                    WMAPP_DIR_ERROR,
                                    WPARAM(0),
                                    LPARAM(payload),
                                );
                            }
                            continue;
                        }
                    }
                };
                let payload = Box::into_raw(Box::new((path, entries))) as isize;
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd.0),
                        WMAPP_DIR_LISTED,
                        WPARAM(0),
                        LPARAM(payload),
                    );
                }
            }
            ScanCmd::Search { root, query, hwnd } => {
                // Cap results so a search in `C:\` doesn't balloon into
                // hundreds of thousands of rows. The model is virtual so
                // large lists are cheap, but beyond a few thousand the
                // search is no longer a useful form of navigation.
                const MAX_RESULTS: usize = 5_000;
                let entries = navigator_fs::search_recursive(&root, &query, MAX_RESULTS);
                let payload = Box::into_raw(Box::new((root, query, entries))) as isize;
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd.0),
                        WMAPP_SEARCH_RESULTS,
                        WPARAM(0),
                        LPARAM(payload),
                    );
                }
            }
        }
    }
}

pub fn run(cfg: AppConfig) -> windows::core::Result<i32> {
    let state = AppState::new(&cfg);
    state.bootstrap_plugins();
    let window = create_window(state.clone())?;
    let rc = run_message_loop(window.hwnd);
    Ok(rc)
}
