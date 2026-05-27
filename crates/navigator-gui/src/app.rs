//! Top-level application glue: owns the model, the speech sink, the
//! background scan worker, and the clipboard for cut/copy.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::thread;

use crossbeam_channel::{Sender, unbounded};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tracing::{error, warn};

use navigator_config::ConfigHandle;
use navigator_core::NavPath;
use navigator_fs::read_dir;
use navigator_plugin_api::host::HostCallbacks;
use navigator_rclone::{Operation, OverwritePolicy, RcloneDriver, op::OpEvent};

use crate::plugins::{Host as PluginHost, PluginRegistry};
use crate::remote_cache::RemoteCache;

use crate::history::History;
use crate::model::{Filter, Model};
use crate::speech::SpeechSink;
use crate::window::{
    HwndSend, WMAPP_DIR_ERROR, WMAPP_DIR_LISTED, WMAPP_SEARCH_RESULTS, create as create_window,
    run_message_loop,
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
    Search {
        root: NavPath,
        query: String,
        hwnd: HwndSend,
    },
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
    /// Process-wide cache for files downloaded from rclone remotes so
    /// they can be opened in local apps. See `remote_cache.rs`.
    pub remote_cache: Arc<RemoteCache>,
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
        // Clone the driver into the scan worker so it can run
        // `rclone lsjson` / `listremotes` for remote browsing. Transfer
        // concurrency is irrelevant for those one-shot commands, so we
        // don't re-read config here.
        let scan_rclone = RcloneDriver::from_path();
        thread::Builder::new()
            .name("navigator-scan".into())
            .spawn(move || scan_worker(rx, scan_rclone))
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

        // Wire the rclone driver with the configured `--transfers` value
        // up-front. Changing the setting later rebuilds the driver via
        // `AppState::set_rclone_transfers`, so all ops — including the
        // ones already queued in a worker — pick up the new value.
        let transfers = cfg.config.read().rclone.transfers_clamped();
        let me = Arc::new(Self {
            initial_path: cfg.initial_path.clone(),
            model,
            speech: SpeechSink::start(),
            rclone: RcloneDriver::from_path().with_transfers(transfers),
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
            remote_cache: Arc::new(RemoteCache::new()),
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
                    let Some(s) = weak.upgrade() else {
                        break;
                    };
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
        let Some(h) = self.hwnd() else {
            return;
        };
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
        // Virtual views (This PC, Remotes root) have no real directory
        // to watch; remote sub-paths live behind an rclone remote and
        // likewise can't be locally watched.
        if path.is_this_pc() || path.is_remotes_root() || path.is_remote() {
            *self.watcher.lock() = None;
            return;
        }
        let Some(hwnd) = self.hwnd() else {
            return;
        };
        match crate::watcher::watch(path.clone(), hwnd) {
            Ok(w) => {
                *self.watcher.lock() = Some(w);
            }
            Err(e) => {
                warn!("file watcher failed: {e}");
                *self.watcher.lock() = None;
            }
        }
    }

    /// Fold a filesystem change event into the model.
    pub fn on_watch_event(&self, root: NavPath, ev: crate::watcher::WatchEvent) {
        // Only consume events for the currently-displayed directory; a
        // stale event from a previous cwd should not mutate the new view.
        if self.model.cwd().as_ref() != Some(&root) {
            return;
        }
        if self.model.is_search_mode() {
            return;
        }

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
                if let Some(f) = from {
                    self.model.remove_by_name(&f);
                }
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
        tracing::info!(
            "run_action: {:?} cmd={:?} args={:?}",
            action.name,
            action.command,
            action.args
        );
        let mut paths = self.model.selected_paths();
        tracing::info!("run_action: {} selected path(s)", paths.len());
        if paths.is_empty() {
            // Fall back to the focused row if there is no selection.
            let sel = self.model.selection_snapshot();
            tracing::info!(
                "run_action: selection snapshot focus={:?} len={}",
                sel.focus(),
                sel.len()
            );
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
        let targets: &[NavPath] = if action.single {
            &paths[..1]
        } else {
            &paths[..]
        };
        for p in targets {
            tracing::info!(
                "run_action: spawning {:?} for target {:?}",
                action.command,
                p.to_string()
            );
            match crate::actions::spawn_action(action, p) {
                Ok(()) => tracing::info!("run_action: spawn OK"),
                Err(e) => {
                    error!("action {:?} failed: {}", action.name, e);
                    crate::dialogs::show_error(
                        self.hwnd(),
                        "Action failed",
                        &format!("{}: {}", action.name, e),
                    );
                }
            }
        }
    }

    pub fn set_hwnd(&self, hwnd: HWND) {
        *self.hwnd.lock() = Some(HwndSend(hwnd));
    }
    fn hwnd(&self) -> Option<HwndSend> {
        *self.hwnd.lock()
    }

    /// Public accessor for the main-window HWND. Modules that schedule
    /// UI-thread work (accelerator rebuild, error dialogs) need it.
    pub fn main_hwnd(&self) -> Option<HWND> {
        self.hwnd().map(|h| h.0)
    }

    pub fn say(&self, text: &str, interrupt: bool) {
        self.speech.say(text, interrupt);
    }

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
    ///
    /// Same-letter fallback: when the buffer is a run of one character
    /// (e.g. "aa", "aaa") and no entry starts with the run, cycle through
    /// entries starting with that single letter from the current focus.
    /// Buffer collapses to a single char on a successful fallback so
    /// subsequent same-letter presses keep cycling.
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
        if let Some(idx) = self.model.find_prefix(&prefix, None) {
            return Some(idx);
        }
        let ch_lc = ch.to_ascii_lowercase();
        let prefix_len = prefix.chars().count();
        let all_same = prefix.chars().all(|c| c.to_ascii_lowercase() == ch_lc);
        if all_same && prefix_len > 1 {
            let from = self.model.selection_snapshot().focus();
            let single = ch.to_string();
            if let Some(idx) = self.model.find_prefix(&single, from) {
                let mut g = self.type_ahead.lock();
                g.0.clear();
                g.0.push(ch);
                g.1 = std::time::Instant::now();
                return Some(idx);
            }
        }
        None
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
        if target.is_this_pc() {
            self.navigate(target);
            return;
        }
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
            0 => {
                self.say("nothing selected, cannot set hotspot", false);
                return;
            }
            n => {
                self.say(
                    &format!("{} items selected, hotspot needs exactly one", n),
                    false,
                );
                return;
            }
        };

        let display = target.to_string();
        self.config.with_mut(|c| {
            if idx < c.hotspots.len() {
                c.hotspots[idx] = display.clone();
            }
        });
        let _ = self.config.save();
        self.say(
            &format!("hotspot {} set to {}", slot, target.file_name()),
            false,
        );
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
        let Some(h) = self.hwnd() else {
            return;
        };
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
        self.config
            .with_mut(|c| c.general.show_hidden = filter.show_hidden);
        let _ = self.config.save();
        self.refresh_count_on_control(count);
        self.say(
            if filter.show_hidden {
                "showing hidden files"
            } else {
                "hiding hidden files"
            },
            false,
        );
    }

    pub fn set_sort_mode(&self, mode: navigator_config::SortMode) {
        let mut s = self.model.sort();
        s.mode = mode;
        self.model.set_sort(s);
        self.config.with_mut(|c| c.general.sort_mode = mode);
        let _ = self.config.save();
        self.say(
            &format!(
                "sort by {}",
                match mode {
                    navigator_config::SortMode::Name => "name",
                    navigator_config::SortMode::Size => "size",
                    navigator_config::SortMode::Type => "type",
                    navigator_config::SortMode::Modified => "date modified",
                    navigator_config::SortMode::Created => "date created",
                }
            ),
            false,
        );
        self.refresh();
    }

    pub fn toggle_sort_descending(&self) {
        let mut s = self.model.sort();
        s.descending = !s.descending;
        self.model.set_sort(s);
        self.config
            .with_mut(|c| c.general.sort_descending = s.descending);
        let _ = self.config.save();
        self.say(
            if s.descending {
                "descending"
            } else {
                "ascending"
            },
            false,
        );
        self.refresh();
    }

    /// Kick off a recursive search from `root` for `query` (case-insensitive
    /// substring match on file/directory names). Runs on the scan worker
    /// thread; results land back via WMAPP_SEARCH_RESULTS.
    pub fn start_search(&self, root: NavPath, query: String) {
        let Some(hwnd) = self.hwnd() else {
            return;
        };
        let _ = self.scan_tx.send(ScanCmd::Search { root, query, hwnd });
        self.say("searching", false);
    }

    pub fn toggle_system(&self) {
        let mut filter = self.model.filter();
        filter.show_system = !filter.show_system;
        let count = self.model.set_filter(filter);
        self.config
            .with_mut(|c| c.general.show_system = filter.show_system);
        let _ = self.config.save();
        self.refresh_count_on_control(count);
        self.say(
            if filter.show_system {
                "showing system files"
            } else {
                "hiding system files"
            },
            false,
        );
    }

    /// Post a synthetic `WMAPP_DIR_LISTED` with the current listing so the
    /// window updates its virtual count without touching the filesystem.
    fn refresh_count_on_control(&self, _count: usize) {
        // Simplest path: just re-emit the current scan by navigating to it
        // again. Cheap because `read_dir` at the cwd is already hot cache.
        if let Some(cwd) = self.model.cwd() {
            self.navigate(cwd);
        }
    }

    pub fn open_file(&self, path: NavPath) {
        if path.is_remote() {
            self.open_remote_file(path);
            return;
        }
        shell_open(path.as_path());
    }

    /// Download a remote file into the staging cache, then hand it to
    /// ShellExecute. Returns immediately — the download runs on a worker
    /// thread so the UI stays responsive. Once the staged file is live,
    /// `RemoteCache` arms its watcher so post-open edits can prompt an
    /// upload back.
    fn open_remote_file(&self, remote: NavPath) {
        let Some((name, sub)) = remote.remote_parts() else {
            return;
        };
        if sub.is_empty() {
            self.say("can't open a remote root as a file", true);
            return;
        }
        let Some(hwnd) = self.hwnd() else {
            return;
        };

        let staged = self.remote_cache.stage_path_for(&name, &sub);
        let Ok(staged_nav) = NavPath::new(staged.clone()) else {
            self.say("remote cache path invalid", true);
            return;
        };

        let speech = self.speech.handle();
        let rclone = self.rclone.clone();
        let cache = Arc::clone(&self.remote_cache);
        let remote_for_thread = remote.clone();

        let _ = speech.send(crate::speech::Utterance {
            text: format!("downloading {}", remote.file_name()),
            interrupt: false,
        });

        std::thread::Builder::new()
            .name("navigator-remote-open".into())
            .spawn(move || {
                let op = Operation::CopyTo {
                    src: remote_for_thread.clone(),
                    dst: staged_nav,
                };
                let handle = match rclone.spawn(op) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = speech.send(crate::speech::Utterance {
                            text: format!("download failed: {}", e),
                            interrupt: true,
                        });
                        return;
                    }
                };
                let mut success = false;
                for ev in handle.events.iter() {
                    if let OpEvent::Done {
                        success: ok,
                        stderr_tail,
                    } = ev
                    {
                        success = ok;
                        if !ok {
                            let tail = stderr_tail.lines().next_back().unwrap_or("").to_string();
                            let _ = speech.send(crate::speech::Utterance {
                                text: format!(
                                    "download failed: {}",
                                    if tail.is_empty() {
                                        "see log".into()
                                    } else {
                                        tail
                                    }
                                ),
                                interrupt: true,
                            });
                        }
                        break;
                    }
                }
                if !success {
                    return;
                }

                cache.register(staged.clone(), remote_for_thread.clone(), hwnd);
                let _ = speech.send(crate::speech::Utterance {
                    text: format!("opening {}", remote_for_thread.file_name()),
                    interrupt: false,
                });
                shell_open(&staged);
            })
            .expect("spawn remote-open worker");
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
            UndoAction::Paste {
                created,
                originals,
                cut_mode,
            } => {
                self.say(
                    &format!("undo: reverting paste of {} items", created.len(),),
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
        if paths.is_empty() {
            self.say("nothing selected", false);
            return;
        }
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
        if paths.is_empty() {
            self.say("nothing selected", false);
            return;
        }
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
        if incoming.is_empty() {
            self.say("nothing selected", false);
            return;
        }

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
                kind: if cut_mode {
                    "cut".into()
                } else {
                    "copy".into()
                },
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
                kind: if cut_mode {
                    "append-cut".into()
                } else {
                    "append-copy".into()
                },
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
        let Some(dest) = self.model.cwd() else {
            return;
        };
        let clip = crate::clipboard::load_clip();
        if clip.sources.is_empty() {
            self.say("clipboard empty", false);
            return;
        }

        // Rehydrate string paths to NavPaths; skip any that are no longer
        // absolute (manually-edited file, mount unplugged, etc.).
        let sources: Vec<NavPath> = clip
            .sources
            .iter()
            .filter_map(|s| NavPath::new(PathBuf::from(s)).ok())
            .collect();
        if sources.is_empty() {
            self.say("clipboard paths invalid", false);
            return;
        }

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
        let created: Vec<NavPath> = sources.iter().map(|s| dest.join(s.file_name())).collect();
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
        if paths.is_empty() {
            self.say("nothing selected", false);
            return;
        }
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

        // Split by endpoint. Remote paths can't go to a local `.trash`
        // dir — rclone would have to cross the boundary — and rclone's
        // own purge is irreversible, so we confirm + skip the undo
        // stack. Local paths still route through the trash flow.
        let (remote_targets, local): (Vec<NavPath>, Vec<NavPath>) =
            paths.iter().cloned().partition(|p| p.is_remote());

        if !remote_targets.is_empty() {
            if !confirm_remote_delete(self.main_hwnd(), &remote_targets) {
                // User cancelled. Don't touch local either — avoids a
                // half-delete where they confirmed one endpoint and not
                // the other. Clear pending_focus since no op will fire.
                self.pending_focus.lock().take();
                return;
            }
            self.spawn_remote_purge(remote_targets);
        }

        if local.is_empty() {
            return;
        }

        // Move each local target to `<volume_root>/.trash/<ts>_<n>/<basename>`
        // on the same drive. Same-volume keeps the rename atomic.
        let mut pairs: Vec<(NavPath, NavPath)> = Vec::with_capacity(local.len());
        for p in &local {
            let Some(trash_dir) = make_trash_dir_on_volume_of(p) else {
                self.say(
                    &format!("failed to create trash dir for {}", p.file_name()),
                    true,
                );
                continue;
            };
            let trash_path = trash_dir.join(p.file_name());
            pairs.push((trash_path, p.clone()));
        }
        if pairs.is_empty() {
            self.say("delete targets resolved to nothing", true);
            return;
        }
        self.push_undo(UndoAction::Delete {
            pairs: pairs.clone(),
        });

        let state = self.clone_for_worker();
        std::thread::Builder::new()
            .name("navigator-batch-delete".into())
            .spawn(move || state.run_trash_batch(pairs))
            .expect("spawn delete batch");
    }

    /// Permanently delete every `<drive>\.trash` directory on every
    /// connected local drive. Walks each candidate trash to compute the
    /// space it occupies, surfaces a per-drive breakdown in a Yes/No
    /// confirmation, and on Yes spawns a worker that runs
    /// `remove_dir_all` per drive. After completion any in-memory
    /// `UndoAction::Delete` entries are dropped because their staged
    /// paths no longer exist. This is the one path that bypasses the
    /// usual rclone-driven mutation flow — trash dirs are an internal
    /// implementation detail, not user-visible files, so a direct
    /// `std::fs` call is fine and avoids spinning rclone up just to
    /// purge a local folder.
    pub fn op_empty_trash(&self) {
        let drives = navigator_fs::list_drives();
        let mut entries: Vec<(PathBuf, String, u64)> = Vec::new();
        let mut total: u64 = 0;
        for d in drives {
            let Some(root_str) = navigator_fs::drive_path_from_display(&d.name) else {
                continue;
            };
            let trash = PathBuf::from(&root_str).join(".trash");
            if !trash.exists() {
                continue;
            }
            let nav = match NavPath::new(&trash) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let stats = crate::props::compute_folder_stats(&nav);
            total = total.saturating_add(stats.total_size);
            entries.push((trash, d.name, stats.total_size));
        }

        if entries.is_empty() {
            self.say(".trash is already empty on all drives", false);
            return;
        }

        let mut body = String::from(
            "Permanently delete .trash on the following drives?\n\
             This cannot be undone.\n\n",
        );
        for (_, label, size) in &entries {
            body.push_str(&format!(
                "• {} — {}\n",
                label,
                crate::listview::format_size(*size),
            ));
        }
        body.push_str(&format!(
            "\nTotal to free: {}",
            crate::listview::format_size(total),
        ));

        if !confirm_empty_trash(self.main_hwnd(), &body) {
            return;
        }

        let speech = self.speech.handle();
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);
        let dirs: Vec<PathBuf> = entries.into_iter().map(|(p, _, _)| p).collect();
        let total_freed = total;
        std::thread::Builder::new()
            .name("navigator-empty-trash".into())
            .spawn(move || {
                let mut ok = 0u32;
                let mut failed = 0u32;
                for d in &dirs {
                    match std::fs::remove_dir_all(d) {
                        Ok(()) => ok += 1,
                        Err(e) => {
                            failed += 1;
                            let _ = speech.send(crate::speech::Utterance {
                                text: format!("failed to empty {}: {}", d.display(), e),
                                interrupt: true,
                            });
                        }
                    }
                }
                let msg = if failed == 0 {
                    format!(
                        "emptied .trash on {} drive(s); {} freed",
                        ok,
                        crate::listview::format_size(total_freed),
                    )
                } else {
                    format!("emptied {}, {} failed", ok, failed)
                };
                let _ = speech.send(crate::speech::Utterance {
                    text: msg,
                    interrupt: failed > 0,
                });
                if let Some(state) = state_weak.upgrade() {
                    state
                        .undo_stack
                        .lock()
                        .retain(|u| !matches!(u, UndoAction::Delete { .. }));
                    state.refresh();
                }
            })
            .expect("spawn empty-trash worker");
    }

    /// Fire `rclone purge` once per remote target on a background
    /// thread. No undo — rclone purge is destructive, and most backends
    /// (S3 without versioning, SFTP, WebDAV) have no recovery path. UI
    /// refreshes when the last target finishes.
    fn spawn_remote_purge(&self, targets: Vec<NavPath>) {
        let rclone = self.rclone.clone();
        let speech = self.speech.handle();
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);
        let parent_hint = targets.first().and_then(|p| p.parent());

        let _ = speech.send(crate::speech::Utterance {
            text: format!("deleting {} remote item(s)", targets.len()),
            interrupt: false,
        });

        std::thread::Builder::new()
            .name("navigator-remote-purge".into())
            .spawn(move || {
                let mut ok_count = 0usize;
                let mut fail_count = 0usize;
                for t in &targets {
                    let op = Operation::Delete {
                        targets: vec![t.clone()],
                    };
                    let handle = match rclone.spawn(op) {
                        Ok(h) => h,
                        Err(e) => {
                            fail_count += 1;
                            let _ = speech.send(crate::speech::Utterance {
                                text: format!("delete failed: {}", e),
                                interrupt: true,
                            });
                            continue;
                        }
                    };
                    for ev in handle.events.iter() {
                        if let OpEvent::Done { success, .. } = ev {
                            if success {
                                ok_count += 1;
                            } else {
                                fail_count += 1;
                            }
                            break;
                        }
                    }
                }
                let _ = speech.send(crate::speech::Utterance {
                    text: if fail_count == 0 {
                        format!("deleted {} remote item(s)", ok_count)
                    } else {
                        format!("deleted {}, {} failed", ok_count, fail_count)
                    },
                    interrupt: fail_count > 0,
                });
                if let Some(state) = state_weak.upgrade() {
                    if let (Some(cwd), Some(parent)) = (state.model.cwd(), parent_hint) {
                        if cwd == parent {
                            state.refresh();
                        }
                    }
                }
            })
            .expect("spawn remote-purge worker");
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
        if total == 0 || sel.is_empty() {
            return None;
        }

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
        let (present, missing): (Vec<String>, Vec<String>) = entry
            .sources
            .iter()
            .cloned()
            .partition(|p| std::path::Path::new(p).exists());
        if present.is_empty() {
            self.say(
                &format!("all {} paths missing; clipboard unchanged", missing.len(),),
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
            self.say(
                &format!("{} items restored to clipboard", present.len()),
                false,
            );
        } else {
            self.say(
                &format!(
                    "{} items restored, {} missing skipped",
                    present.len(),
                    missing.len(),
                ),
                false,
            );
        }
    }

    /// Cheap snapshot of the bits worker threads need. Keeps `AppState` out
    /// of the closure so we don't leak `Arc<Self>` into threads that only
    /// need to speak + spawn rclone.
    fn clone_for_worker(&self) -> WorkerCtx {
        let (progress_on, announce_interval_secs, transfers) = {
            let g = self.config.read();
            (
                g.rclone.progress_window,
                g.general.announce_interval_secs,
                g.rclone.transfers_clamped(),
            )
        };
        let progress = if progress_on {
            self.hwnd().and_then(|h| crate::progress::open(h.0).ok())
        } else {
            None
        };
        // Rebuild the driver each spawn with the current `--transfers`
        // so a config change inside Options takes effect on the next op
        // without needing to restart the app.
        WorkerCtx {
            rclone: self.rclone.clone().with_transfers(transfers),
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
        let text = paths
            .iter()
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

    /// Spawn a fresh navigator instance pointed at the *containing folder*
    /// of the focused entry, then announce. Used by Ctrl+Enter — handy in
    /// search results where each row may live in a different subdirectory.
    /// Remote paths translate back to `name:sub` form via `rclone_arg` so
    /// the new instance picks the path up via the same syntax the address
    /// bar accepts.
    pub fn op_open_containing_new_window(&self) {
        let sel = self.model.selection_snapshot();
        let Some(idx) = sel.focus() else {
            self.say("nothing focused", false);
            return;
        };
        let Some(entry) = self.model.get(idx) else {
            return;
        };
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        if cwd.is_this_pc() || cwd.is_remotes_root() {
            self.say("no containing folder here", false);
            return;
        }
        let full = cwd.join(&entry.name);
        let Some(parent) = full.parent() else {
            self.say("no parent folder", false);
            return;
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                self.say(&format!("locate exe failed: {}", e), true);
                return;
            }
        };
        let arg = if parent.is_remote() {
            match parent.rclone_arg() {
                Some(s) => s,
                None => parent.to_string(),
            }
        } else {
            parent.to_string()
        };
        match std::process::Command::new(&exe).arg(&arg).spawn() {
            Ok(_) => self.say("opened in new window", false),
            Err(e) => self.say(&format!("new window failed: {}", e), true),
        }
    }

    /// Copy the current selection to the *real* Windows clipboard as
    /// `CF_HDROP` (file-handle list) plus a `Preferred DropEffect = COPY`
    /// hint, so a subsequent paste in Explorer / dialogs / other apps
    /// reproduces the files. Distinct from `op_copy`, which only writes
    /// our private file-backed clipboard. Remote (rclone) paths are
    /// rejected — Explorer can't resolve `\\?\NavigatorRemote\...`.
    pub fn op_copy_to_clipboard(&self) {
        let paths = self.model.selected_paths();
        if paths.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        if paths.iter().any(|p| p.is_remote()) {
            self.say("can't copy remote paths to OS clipboard", true);
            return;
        }
        let n = paths.len();
        let os_paths: Vec<std::path::PathBuf> =
            paths.iter().map(|p| p.as_path().to_path_buf()).collect();
        match set_clipboard_hdrop(self.main_hwnd(), &os_paths) {
            Ok(()) => {
                let msg = if n == 1 {
                    "1 item on OS clipboard".to_string()
                } else {
                    format!("{} items on OS clipboard", n)
                };
                self.say(&msg, false);
            }
            Err(e) => {
                self.say(&format!("OS clipboard failed: {}", e), true);
            }
        }
    }

    /// Extract every selected archive 7-Zip can open, using `7z.exe` on
    /// `PATH`. The set is filtered to known-extractable extensions
    /// before validating the binary so the user gets the more useful
    /// "nothing extractable selected" error instead of "7z missing".
    /// Behaviour (delete after, wrapper folder) is read from the
    /// `[extraction]` config section. Runs on a worker; the file
    /// watcher folds the new entries into the listing automatically so
    /// the user doesn't need to refresh.
    pub fn op_extract(&self) {
        let selection = self.model.selected_paths();
        if selection.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        let local: Vec<navigator_core::NavPath> =
            selection.into_iter().filter(|p| !p.is_remote()).collect();
        let extractable = crate::extract::filter_extractable(&local);
        if extractable.is_empty() {
            self.say("no extractable archives selected", true);
            return;
        }
        let seven_zip = match crate::extract::find_7z() {
            Some(p) => p,
            None => {
                self.say(
                    "7z not found on PATH; install 7-Zip to extract archives",
                    true,
                );
                return;
            }
        };
        let opts = self.config.read().extraction;
        let speech = self.speech.handle();
        let total = extractable.len();
        self.say(&format!("extracting {} archive(s)", total), false);
        std::thread::Builder::new()
            .name("navigator-extract".into())
            .spawn(move || crate::extract::run_extract(extractable, opts, seven_zip, speech))
            .expect("spawn extract worker");
    }

    /// Show the read-only properties viewer for the focused entry. For
    /// directories the recursive size / counts / extension histogram
    /// come from a worker thread so a giant tree doesn't freeze the UI;
    /// the viewer only opens once the scan finishes.
    pub fn op_show_properties(&self) {
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        let sel = self.model.selection_snapshot();
        let Some(idx) = sel.focus() else {
            self.say("no item focused", false);
            return;
        };
        let Some(entry) = self.model.get(idx) else {
            return;
        };
        let path = cwd.join(&entry.name);
        let Some(hwnd) = self.hwnd() else {
            return;
        };

        let is_dir = entry.is_dir();
        let title = format!("Properties — {}", entry.name);
        if is_dir {
            self.say(&format!("scanning {}…", entry.name), false);
        }

        if path.is_remote() {
            let rclone = self.rclone.clone();
            std::thread::Builder::new()
                .name("navigator-properties-remote".into())
                .spawn(move || {
                    let arg = path.rclone_arg().unwrap_or_default();
                    let stat = rclone.stat(&arg).ok().flatten();
                    let size = if is_dir { rclone.size(&arg).ok() } else { None };
                    let body = crate::props::format_remote_properties(
                        &entry,
                        &path,
                        stat.as_ref(),
                        size.as_ref(),
                    );
                    post_viewer(hwnd, title, body);
                })
                .expect("spawn remote properties worker");
            return;
        }

        std::thread::Builder::new()
            .name("navigator-properties".into())
            .spawn(move || {
                let stats = if is_dir {
                    Some(crate::props::compute_folder_stats(&path))
                } else {
                    None
                };
                let body = crate::props::format_properties(&entry, &path, stats.as_ref());
                post_viewer(hwnd, title, body);
            })
            .expect("spawn properties worker");
    }

    /// Recursively enumerate the focused folder (or the current folder
    /// if a file is focused) and show the TOML tree dump in the viewer.
    /// Runs on a worker thread for the same reason as properties.
    pub fn op_dump_tree(&self) {
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        let sel = self.model.selection_snapshot();
        // Prefer the focused entry if it's a directory; otherwise dump
        // the current folder itself.
        let target = sel
            .focus()
            .and_then(|i| self.model.get(i))
            .filter(|e| e.is_dir())
            .map(|e| cwd.join(&e.name))
            .unwrap_or(cwd);
        if target.is_this_pc() {
            self.say("can't dump This PC", true);
            return;
        }
        let Some(hwnd) = self.hwnd() else {
            return;
        };
        let label = target.file_name().to_string();
        let title = format!(
            "Tree — {}",
            if label.is_empty() {
                target.to_string()
            } else {
                label
            }
        );
        self.say("dumping tree…", false);
        std::thread::Builder::new()
            .name("navigator-dump-tree".into())
            .spawn(move || {
                let body = crate::props::dump_tree_toml(&target);
                post_viewer(hwnd, title, body);
            })
            .expect("spawn dump-tree worker");
    }

    /// Rename `old_name` → `new_name` within the current directory. Arms
    /// `pending_focus` so the caret lands on the renamed row after the
    /// post-op refresh — without it the listing rebuild defaults to row 0.
    pub fn op_rename(&self, old_name: &str, new_name: &str) {
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        let src = cwd.join(old_name);
        let dst = cwd.join(new_name);
        self.set_pending_focus(dst.clone());
        self.spawn_op(Operation::Rename { src, dst });
    }

    /// Create an empty folder named `name` inside the current directory.
    /// The caller is responsible for prompting the user for the name (see
    /// `new_folder` dialog) — this method just validates the context and
    /// fires the op. Pending focus is armed so the newly created row gets
    /// the caret after the post-op refresh.
    pub fn op_new_folder(&self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            self.say("folder name empty", true);
            return;
        }
        if name.contains(['\\', '/', ':']) || name == "." || name == ".." {
            self.say("invalid folder name", true);
            return;
        }
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        if cwd.is_this_pc() || cwd.is_remotes_root() {
            self.say("cannot create folder here", true);
            return;
        }
        let dst = cwd.join(&name);
        if !dst.is_remote() && dst.as_path().exists() {
            self.say(&format!("{} already exists", name), true);
            return;
        }
        self.set_pending_focus(dst.clone());
        self.spawn_op(Operation::Mkdir { dir: dst });
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

    /// Upload a staged remote file back to its origin. Runs on a worker
    /// so the UI doesn't block, and updates the cache record on success
    /// so the next save re-prompts instead of re-uploading silently.
    pub fn op_remote_upload(&self, staged: PathBuf, remote: NavPath) {
        let Ok(staged_nav) = NavPath::new(staged.clone()) else {
            self.say("cache path invalid", true);
            return;
        };
        let speech = self.speech.handle();
        let rclone = self.rclone.clone();
        let cache = Arc::clone(&self.remote_cache);
        let remote_display = remote.rclone_arg().unwrap_or_else(|| remote.to_string());
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);

        let _ = speech.send(crate::speech::Utterance {
            text: format!("uploading to {}", remote_display),
            interrupt: false,
        });

        std::thread::Builder::new()
            .name("navigator-remote-upload".into())
            .spawn(move || {
                let op = Operation::CopyTo {
                    src: staged_nav,
                    dst: remote.clone(),
                };
                let handle = match rclone.spawn(op) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = speech.send(crate::speech::Utterance {
                            text: format!("upload failed: {}", e),
                            interrupt: true,
                        });
                        cache.finish_prompt(&staged, None);
                        return;
                    }
                };
                let mut success = false;
                let mut tail = String::new();
                for ev in handle.events.iter() {
                    if let OpEvent::Done {
                        success: ok,
                        stderr_tail,
                    } = ev
                    {
                        success = ok;
                        tail = stderr_tail;
                        break;
                    }
                }
                if success {
                    let mtime = staged.metadata().ok().and_then(|m| m.modified().ok());
                    cache.finish_prompt(&staged, mtime);
                    let _ = speech.send(crate::speech::Utterance {
                        text: format!("uploaded to {}", remote_display),
                        interrupt: false,
                    });
                    // Refresh the current view so the listing picks up
                    // the new mtime/size, and arm `pending_focus` with
                    // the uploaded file so the caret lands on it after
                    // the rescan instead of snapping to row 0. Only
                    // refresh if cwd matches the remote's parent — the
                    // user may have navigated away during upload.
                    if let Some(state) = state_weak.upgrade() {
                        if let Some(cwd) = state.model.cwd() {
                            if let Some(parent) = remote.parent() {
                                if cwd == parent {
                                    state.set_pending_focus(remote.clone());
                                    state.refresh();
                                }
                            }
                        }
                    }
                } else {
                    cache.finish_prompt(&staged, None);
                    let last = tail.lines().next_back().unwrap_or("").to_string();
                    let _ = speech.send(crate::speech::Utterance {
                        text: format!(
                            "upload failed: {}",
                            if last.is_empty() {
                                "see log".into()
                            } else {
                                last
                            }
                        ),
                        interrupt: true,
                    });
                }
            })
            .expect("spawn remote-upload worker");
    }
}

/// Confirm a permanent remote delete. Remote paths can't go through the
/// local `.trash/` flow (cross-endpoint rename doesn't exist) and
/// rclone's `purge` is irreversible on most backends, so we warn +
/// require explicit Yes. Defaults to No.
fn confirm_remote_delete(parent: Option<HWND>, targets: &[NavPath]) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO,
        MessageBoxW,
    };
    use windows::core::PCWSTR;

    let preview: Vec<String> = targets
        .iter()
        .take(10)
        .map(|p| p.rclone_arg().unwrap_or_else(|| p.to_string()))
        .collect();
    let extra = if targets.len() > preview.len() {
        format!("\n… and {} more", targets.len() - preview.len())
    } else {
        String::new()
    };
    let body = format!(
        "Permanently delete {} item(s) from the remote?\n\
         rclone purge cannot be undone.\n\n\
         {}{}",
        targets.len(),
        preview.join("\n"),
        extra,
    );
    let title_w: Vec<u16> = "Delete from remote?".encode_utf16().chain([0]).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain([0]).collect();
    let is_foreground = parent
        .map(|h| unsafe { GetForegroundWindow() } == h)
        .unwrap_or(false);
    let mut flags = MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2;
    if is_foreground {
        flags |= MB_SETFOREGROUND;
    }
    let rc = unsafe {
        MessageBoxW(
            parent,
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            flags,
        )
        .0
    };
    rc == IDYES.0
}

/// Confirm a permanent trash purge across all drives. Defaults to No.
/// `body` is built by the caller because it lists per-drive sizes that
/// only the trash-walking pass knows about.
fn confirm_empty_trash(parent: Option<HWND>, body: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO,
        MessageBoxW,
    };
    use windows::core::PCWSTR;

    let title_w: Vec<u16> = "Empty .trash on all drives?"
        .encode_utf16()
        .chain([0])
        .collect();
    let body_w: Vec<u16> = body.encode_utf16().chain([0]).collect();
    let is_foreground = parent
        .map(|h| unsafe { GetForegroundWindow() } == h)
        .unwrap_or(false);
    let mut flags = MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2;
    if is_foreground {
        flags |= MB_SETFOREGROUND;
    }
    let rc = unsafe {
        MessageBoxW(
            parent,
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            flags,
        )
        .0
    };
    rc == IDYES.0
}

/// Ask the Windows shell to open `path` with its default handler. Used
/// for both local files and files downloaded out of rclone remotes.
fn shell_open(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let path_w: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let dir_w: Option<Vec<u16>> = path
        .parent()
        .map(|p| p.as_os_str().encode_wide().chain([0]).collect());
    let dir_ptr = dir_w
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            dir_ptr,
            SW_SHOWNORMAL,
        );
    }
}

/// Post a `(title, body)` payload to the main window so the viewer opens
/// on the UI thread. Heap-leaks a `Box` that the window proc reclaims.
fn post_viewer(hwnd: HwndSend, title: String, body: String) {
    let payload = Box::into_raw(Box::new((title, body)));
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd.0),
            crate::window::WMAPP_VIEWER_SHOW,
            WPARAM(0),
            LPARAM(payload as isize),
        );
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
        let _ = self.speech.try_send(crate::speech::Utterance {
            text: text.into(),
            interrupt,
        });
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
        use crate::preflight::{BatchDecision, ItemChoice, prompt_item, unique_numbered_path};

        let total = sources.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        let mut renamed = 0u32;
        // First path that was actually produced in `dest_dir` — used to
        // land focus on it after the refresh so the user sees where the
        // paste ended up. Rename-on-conflict stores the fresh sibling.
        let mut first_created: Option<NavPath> = None;

        // Sticky decision after the user ticks "apply to all". `None` means
        // we still ask per-conflict.
        let mut sticky: Option<ItemChoice> = None;
        // Count how many conflicts remain so the dialog can show progress.
        let mut remaining_conflicts: usize = sources
            .iter()
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
                if s {
                    sticky = Some(choice);
                }
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
            } else if first_created.is_none() {
                first_created = Some(dest_dir.join(&effective_name));
            }
        }
        // Arm pending_focus before the refresh so refocus_after_up can
        // land the caret on the newly pasted row by filename. Matches the
        // behaviour of undo-delete for a consistent "where did it go" UX.
        if let (Some(state), Some(target)) = (self.state.upgrade(), first_created) {
            state.set_pending_focus(target);
        }
        let msg = match (failed, skipped, renamed) {
            (0, 0, 0) => format!("done — {} items", total),
            (0, 0, r) => format!("done — {} items, {} renamed", total, r),
            (0, s, 0) => format!("done — {} items, {} skipped", total - s as usize, s),
            (0, s, r) => format!(
                "done — {} items, {} skipped, {} renamed",
                total - s as usize,
                s,
                r
            ),
            (f, s, _) => format!(
                "finished with {} failures, {} skipped, out of {}",
                f, s, total
            ),
        };
        self.say(msg, failed > 0);
        self.refresh();
    }

    /// Reverse a paste. For copy-mode, delete each created entry. For
    /// cut-mode, move each `created[i]` back to `originals[i]`. Missing
    /// paths are skipped silently — the paste may have been partially
    /// rejected (user clicked Skip) or another process may have already
    /// cleaned things up. All results fold into a single summary.
    fn run_revert_paste(self, created: Vec<NavPath>, originals: Vec<NavPath>, cut_mode: bool) {
        let total = created.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        for (i, c) in created.iter().enumerate() {
            if !c.as_path().exists() {
                skipped += 1;
                continue;
            }
            let op = if cut_mode {
                Operation::Rename {
                    src: c.clone(),
                    dst: originals[i].clone(),
                }
            } else {
                Operation::Delete {
                    targets: vec![c.clone()],
                }
            };
            self.say(
                format!("undo {} of {}: {}", i + 1, total, c.file_name()),
                false,
            );
            if !self.run_one(op) {
                failed += 1;
            }
        }
        let msg = if failed == 0 && skipped == 0 {
            format!("undo done — {} items", total)
        } else if failed == 0 {
            format!(
                "undo done — {} items, {} missing skipped",
                total - skipped as usize,
                skipped
            )
        } else {
            format!(
                "undo finished with {} failures, {} skipped, out of {}",
                failed, skipped, total
            )
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
            self.say(
                format!("deleting {} of {}: {}", i + 1, total, original.file_name()),
                false,
            );
            let op = Operation::Rename {
                src: original,
                dst: trash,
            };
            if !self.run_one(op) {
                failed += 1;
            }
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
            self.say(
                format!("restoring {} of {}: {}", i + 1, total, original.file_name()),
                false,
            );
            if !self.run_one(Operation::Rename {
                src: trash,
                dst: original.clone(),
            }) {
                failed += 1;
            } else if first_restored.is_none() {
                first_restored = Some(original);
            }
        }
        let msg = if failed == 0 && skipped == 0 {
            format!("restored {} items", total)
        } else if failed == 0 {
            format!(
                "restored {} items, {} skipped",
                total - skipped as usize,
                skipped
            )
        } else {
            format!(
                "restore finished with {} failures, {} skipped, out of {}",
                failed, skipped, total
            )
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
        let op_for_retry = op.clone();
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
            Some(std::time::Duration::from_secs(
                self.announce_interval_secs as u64,
            ))
        };
        let mut last_spoken = std::time::Instant::now();

        for ev in handle.events.iter() {
            match ev {
                navigator_rclone::op::OpEvent::Progress {
                    bytes_done,
                    bytes_total,
                    current,
                } => {
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
                                    Some(name) if !name.is_empty() => {
                                        format!("{} percent, {}", pct, name)
                                    }
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
                                    text: msg,
                                    interrupt: false,
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
                navigator_rclone::op::OpEvent::Done {
                    success,
                    stderr_tail,
                } => {
                    if success {
                        prune_empty_src_dirs(&op_for_retry);
                        if let Some(p) = progress.as_ref() {
                            p.post_done(true);
                        }
                        return true;
                    }
                    // Failed. If the tail looks like a Windows ACL
                    // denial (writes to C:\, Program Files, etc.), retry
                    // under UAC. The UAC prompt itself is the user
                    // confirmation — no extra dialog. Don't loop more
                    // than once: if the elevated retry also fails, the
                    // problem isn't permission.
                    let tail_lines: Vec<&str> = stderr_tail.lines().rev().take(10).collect();
                    let tail = tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
                    error!("rclone: {tail}");

                    // Probe the op's local destination for write
                    // permission rather than grepping rclone's tail —
                    // rclone's error string is locale-translated and
                    // lies on protected-root writes (says "file not
                    // found" instead of "access denied"). Asking the OS
                    // directly via a tiny test write gives an
                    // unambiguous `PermissionDenied` errno when ACL is
                    // the cause. Probe runs only on failure, so the
                    // happy path doesn't write a probe per op.
                    let needs_elevation = navigator_rclone::op::local_dest_dir(&op_for_retry)
                        .map(|d| {
                            matches!(
                                probe_write_access(&d).err().map(|e| e.kind()),
                                Some(std::io::ErrorKind::PermissionDenied)
                            )
                        })
                        .unwrap_or(false);
                    if needs_elevation {
                        let _ = self.speech.try_send(crate::speech::Utterance {
                            text: "permission denied, retrying as administrator".into(),
                            interrupt: false,
                        });
                        match crate::elevated::run(&self.rclone, &op_for_retry) {
                            Ok(out) if out.success => {
                                prune_empty_src_dirs(&op_for_retry);
                                if let Some(p) = progress.as_ref() {
                                    p.post_done(true);
                                }
                                return true;
                            }
                            Ok(out) => {
                                if let Some(p) = progress.as_ref() {
                                    p.post_done(false);
                                }
                                let elev_tail = out
                                    .log_tail
                                    .lines()
                                    .rev()
                                    .take(10)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                let body = if elev_tail.is_empty() {
                                    tail.clone()
                                } else {
                                    elev_tail
                                };
                                crate::dialogs::show_error(
                                    self.hwnd,
                                    "File operation failed (even as administrator)",
                                    if body.is_empty() {
                                        "rclone reported an error"
                                    } else {
                                        &body
                                    },
                                );
                                return false;
                            }
                            Err(e) => {
                                // ShellExecuteEx itself failed (UAC
                                // declined, exe missing). Fall through
                                // to the normal failure dialog with the
                                // original tail.
                                error!("elevated retry: {e}");
                            }
                        }
                    }

                    if let Some(p) = progress.as_ref() {
                        p.post_done(false);
                    }
                    crate::dialogs::show_error(
                        self.hwnd,
                        "File operation failed",
                        if tail.is_empty() {
                            "rclone reported an error"
                        } else {
                            &tail
                        },
                    );
                    return false;
                }
            }
        }
        false
    }
}

/// Drop a zero-byte file in `dir` and immediately remove it. Returns
/// the OS error verbatim — callers care about `ErrorKind::PermissionDenied`
/// to distinguish ACL-protected dirs (Windows `C:\` root, `Program
/// Files`, etc.) from missing parents and other write failures. PID +
/// nanos in the name keep concurrent probes from peer instances /
/// threads from colliding.
fn probe_write_access(dir: &std::path::Path) -> std::io::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(".navigator-probe-{}-{}", std::process::id(), nanos,));
    let f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)?;
    drop(f);
    let _ = std::fs::remove_file(&probe);
    Ok(())
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
fn set_clipboard_text(
    hwnd: Option<windows::Win32::Foundation::HWND>,
    text: &str,
) -> std::io::Result<()> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
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

/// Place a `CF_HDROP` (Windows file-handle list) on the clipboard,
/// plus a `Preferred DropEffect = COPY` hint. Pasting the result in
/// Explorer / open-file dialogs / other apps reproduces the files via
/// the shell's normal copy machinery.
///
/// Blob layout for the DROPFILES handle:
///   * `DROPFILES` header (20 bytes, packed) — `pFiles = 20`, `fWide = 1`
///   * UTF-16 paths concatenated, each NUL-terminated
///   * one extra `0u16` so the list ends in a double-NUL
///
/// The "Preferred DropEffect" registered format carries a single
/// `DWORD` (`DROPEFFECT_COPY = 1`), telling the receiver to treat the
/// drop as a copy rather than guessing from same-vs-cross-volume rules.
fn set_clipboard_hdrop(
    hwnd: Option<windows::Win32::Foundation::HWND>,
    paths: &[std::path::PathBuf],
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::DROPFILES;
    use windows::core::PCWSTR;

    // Build the wide string list: <path>\0<path>\0...\0
    let mut wide_list: Vec<u16> = Vec::new();
    for p in paths {
        for unit in p.as_os_str().encode_wide() {
            wide_list.push(unit);
        }
        wide_list.push(0);
    }
    wide_list.push(0); // double-NUL terminator

    let header_size = std::mem::size_of::<DROPFILES>();
    let payload_bytes = wide_list.len() * std::mem::size_of::<u16>();
    let total = header_size + payload_bytes;

    unsafe {
        OpenClipboard(hwnd).map_err(io_err)?;
        let result = (|| -> std::io::Result<()> {
            EmptyClipboard().map_err(io_err)?;

            // --- CF_HDROP block ---
            let hmem = GlobalAlloc(GMEM_MOVEABLE, total).map_err(io_err)?;
            if hmem.is_invalid() {
                return Err(std::io::Error::other("GlobalAlloc returned null"));
            }
            let base = GlobalLock(hmem) as *mut u8;
            if base.is_null() {
                let _ = GlobalFree(Some(hmem));
                return Err(std::io::Error::other("GlobalLock returned null"));
            }
            let header = DROPFILES {
                pFiles: header_size as u32,
                pt: windows::Win32::Foundation::POINT { x: 0, y: 0 },
                fNC: false.into(),
                fWide: true.into(),
            };
            std::ptr::copy_nonoverlapping(
                (&header as *const DROPFILES) as *const u8,
                base,
                header_size,
            );
            std::ptr::copy_nonoverlapping(
                wide_list.as_ptr(),
                base.add(header_size) as *mut u16,
                wide_list.len(),
            );
            let _ = GlobalUnlock(hmem);
            if let Err(e) = SetClipboardData(u32::from(CF_HDROP.0), Some(HANDLE(hmem.0))) {
                let _ = GlobalFree(Some(hmem));
                return Err(io_err(e));
            }

            // --- Preferred DropEffect = DROPEFFECT_COPY (1) ---
            // Failure here is non-fatal: paste targets fall back to
            // their default behaviour (usually copy across drives,
            // move within the same volume).
            let fmt_name: Vec<u16> = "Preferred DropEffect"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let fmt = RegisterClipboardFormatW(PCWSTR(fmt_name.as_ptr()));
            if fmt != 0 {
                let dword_bytes = std::mem::size_of::<u32>();
                if let Ok(hde) = GlobalAlloc(GMEM_MOVEABLE, dword_bytes)
                    && !hde.is_invalid()
                {
                    let dst = GlobalLock(hde) as *mut u32;
                    if !dst.is_null() {
                        *dst = 1; // DROPEFFECT_COPY
                        let _ = GlobalUnlock(hde);
                        if SetClipboardData(fmt, Some(HANDLE(hde.0))).is_err() {
                            let _ = GlobalFree(Some(hde));
                        }
                    } else {
                        let _ = GlobalFree(Some(hde));
                    }
                }
            }

            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

fn io_err(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(format!("{}", e))
}

fn scan_worker(rx: crossbeam_channel::Receiver<ScanCmd>, rclone: RcloneDriver) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ScanCmd::Shutdown => break,
            ScanCmd::List(path, hwnd) => {
                // ThisPC sentinel → enumerate drives rather than reading a
                // real directory. Same flow on the UI side: entries end up
                // in the virtual listview just like regular files.
                let entries = if path.is_this_pc() {
                    navigator_fs::list_drives()
                } else if path.is_remotes_root() {
                    match rclone.listremotes() {
                        Ok(names) => names
                            .into_iter()
                            .map(|n| navigator_core::Entry {
                                name: n,
                                kind: navigator_core::EntryKind::Directory,
                                size: 0,
                                modified: navigator_core::FileTime::default(),
                                created: navigator_core::FileTime::default(),
                                attrs: 0,
                                hidden: false,
                                system: false,
                            })
                            .collect(),
                        Err(e) => {
                            error!("rclone listremotes: {}", e);
                            let payload =
                                Box::into_raw(Box::new((path.clone(), e.to_string()))) as isize;
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
                } else if path.is_remote() {
                    let target = path.rclone_arg().unwrap_or_default();
                    match rclone.lsjson(&target) {
                        Ok(e) => e,
                        Err(e) => {
                            error!("rclone lsjson {}: {}", target, e);
                            let payload =
                                Box::into_raw(Box::new((path.clone(), e.to_string()))) as isize;
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
                            let payload =
                                Box::into_raw(Box::new((path.clone(), e.to_string()))) as isize;
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
                    let _ =
                        PostMessageW(Some(hwnd.0), WMAPP_DIR_LISTED, WPARAM(0), LPARAM(payload));
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

/// Recursively delete empty subdirectories under `path`, then `path`
/// itself if it ends up empty. No-op on non-directory or missing paths.
/// Returns true when `path` was successfully removed.
fn remove_empty_tree(path: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for e in entries.flatten() {
        let pe = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let _ = remove_empty_tree(&pe);
        }
    }
    std::fs::remove_dir(path).is_ok()
}

/// Post-success cleanup for `Move` / `Rename` ops on local sources.
/// `rclone moveto` on a directory degrades to per-file copy+delete on
/// cross-volume / cross-backend moves and leaves the emptied source
/// tree behind. Walk the tree and prune empty dirs. No-op for files,
/// remote sources, or other op kinds.
fn prune_empty_src_dirs(op: &Operation) {
    let srcs: Vec<&NavPath> = match op {
        Operation::Move { sources, .. } => sources.iter().collect(),
        Operation::Rename { src, .. } => vec![src],
        _ => return,
    };
    for s in srcs {
        if s.is_remote() {
            continue;
        }
        let p = s.as_path();
        if p.is_dir() {
            let _ = remove_empty_tree(p);
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

#[cfg(test)]
mod prune_tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_dir_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("empty");
        fs::create_dir(&target).unwrap();
        assert!(remove_empty_tree(&target));
        assert!(!target.exists());
    }

    #[test]
    fn nested_empty_tree_fully_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        fs::create_dir_all(root.join("b").join("c")).unwrap();
        fs::create_dir(root.join("d")).unwrap();
        assert!(remove_empty_tree(&root));
        assert!(!root.exists());
    }

    #[test]
    fn tree_with_files_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("keep.txt"), b"x").unwrap();
        fs::create_dir(root.join("emptysib")).unwrap();
        assert!(!remove_empty_tree(&root));
        assert!(
            root.join("sub").join("keep.txt").exists(),
            "files must survive cleanup"
        );
        assert!(
            !root.join("emptysib").exists(),
            "empty siblings still pruned bottom-up"
        );
    }

    #[test]
    fn missing_path_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does_not_exist");
        assert!(!remove_empty_tree(&bogus));
    }

    #[test]
    fn prune_skips_file_src() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("file.txt");
        fs::write(&src, b"x").unwrap();
        let dst = tmp.path().join("renamed.txt");
        let op = Operation::Rename {
            src: NavPath::new(&src).unwrap(),
            dst: NavPath::new(&dst).unwrap(),
        };
        prune_empty_src_dirs(&op);
        assert!(src.exists(), "file source must be untouched");
    }

    #[test]
    fn prune_removes_empty_dir_src() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("emptydir");
        fs::create_dir_all(src.join("inner")).unwrap();
        let dst = tmp.path().join("moved");
        let op = Operation::Rename {
            src: NavPath::new(&src).unwrap(),
            dst: NavPath::new(&dst).unwrap(),
        };
        prune_empty_src_dirs(&op);
        assert!(!src.exists(), "empty src tree must be pruned");
    }

    #[test]
    fn prune_ignores_unrelated_ops() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("survivor");
        fs::create_dir(&dir).unwrap();
        let op = Operation::Delete {
            targets: vec![NavPath::new(&dir).unwrap()],
        };
        prune_empty_src_dirs(&op);
        assert!(dir.exists(), "Delete op must not trigger src cleanup");
    }
}
