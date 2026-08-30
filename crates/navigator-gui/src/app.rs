//! Top-level application glue: owns the model, the speech sink, the
//! background scan worker, and the clipboard for cut/copy.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, unbounded};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tracing::{error, warn};

use navigator_config::{ConfigHandle, SoundEvent};
use navigator_core::{ConflictMode, NavPath};
use navigator_fs::read_dir;
use navigator_plugin_api::host::HostCallbacks;
use navigator_rclone::{Operation, RcloneDriver, op::OpEvent};

use crate::plugins::{Host as PluginHost, PluginRegistry};
use crate::remote_cache::RemoteCache;

use crate::history::History;
use crate::model::{Filter, Model};
use crate::sound::SoundPlayer;
use crate::speech::SpeechSink;
use crate::window::{
    HwndSend, WMAPP_DIR_ERROR, WMAPP_DIR_LISTED, WMAPP_SEARCH_RESULTS, create as create_window,
    run_message_loop,
};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Disambiguates concurrent `--files-from` list files. Two pastes can be in
/// flight at once (each batch op gets its own worker thread), and a shared
/// filename would have one clobber the other's list mid-run.
static BATCH_LIST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// Directory listing. The `Sort` rides along so the worker can order
    /// the entries before posting them — sorting a large folder is n log n
    /// with a case-folding pass, and doing it in the `WMAPP_DIR_LISTED`
    /// handler stalled the message pump for exactly as long.
    List(NavPath, HwndSend, crate::model::Sort),
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
    /// Event-sound player. Resolves `SoundEvent` → `.wav` through the same
    /// `ConfigHandle` on every call, so an Options change applies to the
    /// very next event with no restart and no re-wiring.
    pub sound: SoundPlayer,
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
    /// The folder the user last *asked* for, while its listing is still
    /// being scanned. `None` once that listing (or its error) lands.
    ///
    /// A navigation is a queued scan, so for the whole gap between the
    /// keypress and the listing arriving, `model.cwd()` still names the
    /// folder the user is leaving. A worker that consults `cwd()` in that
    /// window concludes the user is still where they were and re-lists it
    /// — and because the scan queue is FIFO, that listing lands *after*
    /// the one the user asked for and silently drags them back. See
    /// [`AppState::is_viewing`].
    nav_target: Mutex<Option<NavPath>>,
    /// Sound the *next* `navigate` should play, consumed and reset to
    /// [`SoundEvent::Navigate`] on every call. Every route into a folder
    /// funnels through `navigate`, so "went up" / "went back" / "went
    /// forward" can only be told apart by the caller — same reason
    /// `suppress_history` exists two fields up, and set at the same call
    /// sites.
    next_nav_sound: Mutex<SoundEvent>,
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
    /// Count of mutating rclone operations currently running (copy/move/
    /// delete/trash, plus remote stage/upload). Each worker holds an
    /// [`OpGuard`] for its lifetime, so this is accurate even on a panic.
    /// The window-close handler reads it to warn the user that closing
    /// will kill in-flight transfers (the job object terminates the
    /// rclone children on exit — see `navigator-rclone`).
    ops_in_flight: Arc<AtomicUsize>,
}

/// RAII counter for in-flight operations. Increments on construction,
/// decrements on drop — so a worker thread that finishes (or unwinds)
/// always leaves the count balanced. Cloned from `AppState.ops_in_flight`
/// into each worker via `AppState::op_guard`.
pub struct OpGuard(Arc<AtomicUsize>);

impl OpGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        OpGuard(counter)
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

const UNDO_STACK_MAX: usize = 50;

/// The undo targets for one paste, published by the paste worker.
///
/// Undo may only ever delete a destination the paste itself created, so
/// the list has to be filtered by "did this already exist?" — one stat per
/// clipboard entry. That pass used to run on the UI thread inside
/// `Ctrl+V`, which is exactly the stall the user feels when pasting a
/// large clipboard or pasting onto a slow share.
///
/// It cannot simply be dropped, and it cannot be deferred to the end of
/// the paste either: once files start landing, "already existed" answers
/// yes for the paste's own output. So the worker runs it as its first act
/// and publishes the result here, while the undo entry itself is pushed
/// before the worker spawns — preserving the rule that Ctrl+Z can reach
/// an in-flight paste. An undo that lands in the gap gets told to try
/// again rather than being handed an unfiltered list, because an
/// unfiltered list deletes precisely the files the conflict mode chose to
/// protect.
#[derive(Debug, Default)]
struct PastePlan(Mutex<Option<(Vec<NavPath>, Vec<NavPath>)>>);

impl PastePlan {
    /// Publish the filtered `(created, originals)` pairs. Called once, by
    /// the worker, before any transfer starts.
    fn publish(&self, created: Vec<NavPath>, originals: Vec<NavPath>) {
        *self.0.lock() = Some((created, originals));
    }

    /// `None` until the worker has published — the caller must then leave
    /// the undo entry in place and ask the user to retry.
    fn targets(&self) -> Option<(Vec<NavPath>, Vec<NavPath>)> {
        self.0.lock().clone()
    }
}

/// Record enough to reverse a prior operation. Paste reversal shells out
/// to a worker thread like the forward op does, so the UI stays
/// responsive and progress announcements flow through the usual channel.
#[derive(Debug, Clone)]
enum UndoAction {
    /// Reverse a copy / cut / append-clipboard — just restore the
    /// previous clip file.
    ClipChange { prev: crate::clipboard::ClipFile },
    /// Reverse a paste. `plan` resolves to `created[i]` — the new path at
    /// dest for `originals[i]`. Copy-mode undo deletes `created`; cut-mode
    /// undo moves each `created[i]` back to `originals[i]`.
    Paste {
        plan: Arc<PastePlan>,
        cut_mode: bool,
    },
    /// Reverse a delete. Each `(trash_path, original)` pair was the
    /// target of a trash-rename during `op_delete`; undo moves the
    /// trash entry back to its original path.
    Delete { pairs: Vec<(NavPath, NavPath)> },
}

/// Which folder the user is effectively in, given an in-flight
/// navigation target and the listing currently on screen.
///
/// The in-flight target wins whenever there is one. Between the keypress
/// and the listing arriving, the user has already left as far as they are
/// concerned, and anything that consults the screen instead will act on
/// the folder they are walking away from. Pure so the rule can be pinned
/// by a test without a window.
pub fn effective_folder<'a>(
    nav_target: Option<&'a NavPath>,
    cwd: Option<&'a NavPath>,
) -> Option<&'a NavPath> {
    nav_target.or(cwd)
}

/// Does `target` name a row of the listing for `cwd`?
///
/// `pending_focus` is armed *before* the operation that justifies it runs
/// — `op_delete` picks the row that will survive, a paste worker names the
/// file it created — so by the time a listing arrives to consume it, the
/// user may be in a different folder entirely. Applying it there lands the
/// caret on a coincidentally same-named row, or on row 0. A target belongs
/// to exactly one listing: the one for its parent folder.
///
/// Drive roots (`D:\`) have no parent and belong to the This PC listing.
pub fn focus_target_belongs(cwd: &NavPath, target: &NavPath) -> bool {
    if cwd.is_this_pc() {
        return target.parent().is_none();
    }
    target.parent().as_ref() == Some(cwd)
}

/// Find the filesystem-root of `path` — `C:\` for a drive-letter path,
/// `\\host\share\` for UNC, `/` on non-Windows. Pure (no IO); surfaced
/// as its own function for testability.
pub fn volume_root_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors().last().map(|p| p.to_path_buf())
}

/// Name a fresh trash subdirectory on the same drive/volume as `path`,
/// e.g. `C:\.trash\<ts>_<n>\`. Keeping trash on the same volume means
/// `Operation::Rename` is a true O(1) move instead of a cross-drive
/// copy+delete, and keeps each drive self-contained (unplugging the
/// drive doesn't strand trash on another volume). Dir name is
/// `<unix_ts>_<counter>` — counter is monotonic within the process so
/// rapid successive deletes don't collide.
///
/// **Naming only — no IO.** `op_delete` needs these paths on the UI
/// thread (they go into the undo entry before the worker spawns), but
/// creating them there meant one `create_dir_all` syscall per selected
/// item inside the message pump; deleting a few thousand files visibly
/// hung the window before the delete had even started. The directory is
/// created by the worker in `run_trash_batch`, immediately before the
/// rename that needs it.
///
/// **Never names a trash dir on a UNC share.** `volume_root_of` resolves
/// `\\host\share\dir\file` to `\\host\share\`, so this used to happily
/// return a path that littered a `.trash` folder at the root of a file
/// server we don't own — and an SMB server that flags dot-prefixed names
/// hidden (macOS, Samba) then hid it from the user looking for their
/// file. `op_delete` routes UNC targets to the shell before reaching
/// here; the guard is so a future caller can't reintroduce the litter by
/// forgetting to.
pub fn trash_dir_on_volume_of(path: &NavPath) -> Option<NavPath> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if path.is_unc() {
        return None;
    }
    let ts = crate::clipboard::now_ts();
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let root = volume_root_of(path.as_path())?;
    let dir = root.join(".trash").join(format!("{}_{}", ts, n));
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
            sound: SoundPlayer::start(cfg.config.clone()),
            rclone: RcloneDriver::from_path().with_transfers(transfers),
            config: cfg.config.clone(),
            plugin_reg: OnceCell::new(),
            hwnd: Mutex::new(None),
            scan_tx: tx,
            history: Mutex::new(History::default()),
            suppress_history: Mutex::new(false),
            watcher: Mutex::new(None),
            pending_focus: Mutex::new(None),
            nav_target: Mutex::new(None),
            // Seeded with Startup rather than Navigate: window creation
            // navigates to the initial path, and that first listing *is*
            // the app starting. Playing both would mean the startup chime
            // and the folder chime cutting each other off on every launch.
            next_nav_sound: Mutex::new(SoundEvent::Startup),
            type_ahead: Mutex::new((String::new(), std::time::Instant::now())),
            undo_stack: Mutex::new(Vec::new()),
            self_weak: OnceCell::new(),
            remote_cache: Arc::new(RemoteCache::new()),
            ops_in_flight: Arc::new(AtomicUsize::new(0)),
        });
        let _ = me.self_weak.set(Arc::downgrade(&me));
        me
    }

    /// Number of mutating rclone operations currently running. The
    /// window-close handler reads this to warn before tearing down
    /// in-flight transfers.
    pub fn ops_in_flight(&self) -> usize {
        self.ops_in_flight.load(Ordering::SeqCst)
    }

    /// Mint an [`OpGuard`] tied to this state's in-flight counter. Held by
    /// each op worker for its lifetime so `ops_in_flight` stays accurate.
    fn op_guard(&self) -> OpGuard {
        OpGuard::new(self.ops_in_flight.clone())
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
                if let Some(new) = single_entry(&root, &name)
                    && let Some(vis_idx) = self.model.update_entry(&name, new)
                {
                    self.invalidate_row(vis_idx);
                }
            }
            crate::watcher::WatchEvent::Renamed { from, to } => {
                if let Some(f) = from {
                    self.model.remove_by_name(&f);
                }
                if let Some(t) = to
                    && let Some(e) = single_entry(&root, &t)
                {
                    if append_mode {
                        self.model.append_entries(vec![e]);
                    } else {
                        self.refresh();
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
            if let (Some(idx), Some(cwd)) = (sel.focus(), self.model.cwd())
                && let Some(e) = self.model.get(idx)
            {
                let p = cwd.join(&e.name);
                tracing::info!("run_action: fallback to focused entry {:?}", p.to_string());
                paths.push(p);
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

    /// Play the sound mapped to `ev`, if any. No-op when sounds are off or
    /// the event has no file assigned.
    pub fn play(&self, ev: SoundEvent) {
        self.sound.play(ev);
    }

    /// Arm the sound the next `navigate` will play instead of the default
    /// [`SoundEvent::Navigate`]. Set by `navigate_up` / `go_back` /
    /// `go_forward` immediately before they call `navigate`.
    fn set_next_nav_sound(&self, ev: SoundEvent) {
        *self.next_nav_sound.lock() = ev;
    }

    pub fn navigate(&self, path: NavPath) {
        let Some(hwnd) = self.hwnd() else {
            warn!("navigate before hwnd set; dropping");
            return;
        };
        // Consume the armed cue unconditionally — even on the silent
        // paths below — so a refresh can never leave a stale "went back"
        // primed for the next real navigation.
        let cue = std::mem::replace(&mut *self.next_nav_sound.lock(), SoundEvent::Navigate);
        // `refresh()`, the sort/filter toggles and the Options View page
        // all re-navigate to the folder already on screen. That is a
        // redraw, not a move, and chiming on it would make every F5 and
        // every completed file operation sound like a navigation.
        if self.model.cwd().as_ref() != Some(&path) {
            self.play(cue);
        }
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
        // Publish the destination *before* queueing the scan: from here
        // until the listing lands, this — not `model.cwd()` — is the
        // folder the user considers themselves to be in.
        *self.nav_target.lock() = Some(path.clone());
        let _ = self
            .scan_tx
            .send(ScanCmd::List(path, hwnd, self.model.sort()));
    }

    /// Retire the in-flight navigation once `landed` has been listed (or
    /// has failed to list). Only clears a target it actually matches, so
    /// the older of two queued navigations can't cancel the newer one's
    /// claim on the way past.
    pub fn settle_nav_target(&self, landed: &NavPath) {
        let mut t = self.nav_target.lock();
        if t.as_ref() == Some(landed) {
            *t = None;
        }
    }

    /// Is `dir` the folder the user is looking at — or the one they are
    /// about to be looking at, because they have already asked for it and
    /// the scan is still running?
    ///
    /// This is the question a finishing worker must ask before touching
    /// the listing. Answering it with `model.cwd()` alone is what made a
    /// long paste yank the user back to the folder they started it in.
    pub fn is_viewing(&self, dir: &NavPath) -> bool {
        let target = self.nav_target.lock();
        effective_folder(target.as_ref(), self.model.cwd().as_ref()) == Some(dir)
    }

    /// Re-list `dir` because a background operation just changed it —
    /// **only** if the user is still there. Returns whether it ran.
    ///
    /// A worker must never re-list the folder it captured at spawn time
    /// unconditionally: the user is free to walk away while a copy runs,
    /// and a listing posted for the old folder replaces whatever they
    /// navigated to. Nothing about a finished operation entitles it to
    /// move the user.
    pub fn refresh_dir(&self, dir: &NavPath) -> bool {
        if !self.is_viewing(dir) {
            return false;
        }
        self.rescan(dir.clone());
        true
    }

    /// Queue a listing of `dir` with none of the navigation ceremony — no
    /// history entry, no navigation cue, no type-ahead reset. Re-reading
    /// the folder you are already standing in is a redraw, not a move.
    fn rescan(&self, dir: NavPath) {
        let Some(hwnd) = self.hwnd() else {
            return;
        };
        let _ = self
            .scan_tx
            .send(ScanCmd::List(dir, hwnd, self.model.sort()));
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
    /// A single-character buffer is treated as "advance to the next entry
    /// starting with this letter", so it cycles from the current focus —
    /// that's what makes the first press move forward (e.g. focused on
    /// `he`, press `h` → `ho`, not back to `ha`) as well as repeated
    /// presses step through every match. A multi-character buffer is a real
    /// typed prefix and searches from the top for its first match.
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
        // Single letter → cycle from the current focus (Explorer parity);
        // multi-char prefix → first match from the top.
        let from = if prefix.chars().count() == 1 {
            self.model.selection_snapshot().focus()
        } else {
            None
        };
        if let Some(idx) = self.model.find_prefix(&prefix, from) {
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
                self.set_next_nav_sound(SoundEvent::Back);
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
                self.set_next_nav_sound(SoundEvent::Forward);
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

    /// Jump straight to the virtual "This PC" drive list from wherever we
    /// are — the file-manager sense of "home", not the user profile
    /// folder. Unlike `navigate_up` this doesn't walk the tree one level
    /// at a time.
    ///
    /// Focus lands on the drive we came from rather than row 0: the same
    /// `pending_focus` slot `navigate_up` uses, matched by
    /// `refocus_after_up`'s `drive_path_from_display` inverse. A remote or
    /// the Remotes listing has no drive to match, so those fall through to
    /// the default first-row focus.
    pub fn go_this_pc(&self) {
        let target = NavPath::this_pc();
        let Some(cwd) = self.model.cwd() else {
            self.navigate(target);
            return;
        };
        if cwd.is_this_pc() {
            self.say("already at this pc", false);
            return;
        }
        if !cwd.is_remote()
            && !cwd.is_remotes_root()
            && let Some(root) = volume_root_of(cwd.as_path()).and_then(|p| NavPath::new(p).ok())
        {
            self.set_pending_focus(root);
        }
        self.set_next_nav_sound(SoundEvent::NavigateUp);
        self.navigate(target);
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
            self.set_next_nav_sound(SoundEvent::NavigateUp);
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
        let guard = self.op_guard();

        let _ = speech.send(crate::speech::Utterance {
            text: format!("downloading {}", remote.file_name()),
            interrupt: false,
        });

        std::thread::Builder::new()
            .name("navigator-remote-open".into())
            .spawn(move || {
                let _guard = guard;
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
                        success: ok, error, ..
                    } = ev
                    {
                        success = ok;
                        if !ok {
                            let why = error
                                .as_ref()
                                .map(|e| e.summary())
                                .unwrap_or_else(|| "see log".into());
                            if let Some(e) = error.as_ref() {
                                error!("remote download: {}", e.log_line());
                            }
                            let _ = speech.send(crate::speech::Utterance {
                                text: format!("download failed: {}", why),
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
            UndoAction::Paste { plan, cut_mode } => {
                let Some((created, originals)) = plan.targets() else {
                    // The worker has not finished deciding which
                    // destinations this paste is allowed to remove. Put
                    // the entry back so the next Ctrl+Z gets it, rather
                    // than reverting a superset and deleting files the
                    // conflict mode deliberately spared.
                    self.push_undo(UndoAction::Paste { plan, cut_mode });
                    self.say("paste still starting — try undo again in a moment", true);
                    return;
                };
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
        self.play(SoundEvent::Copy);
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
        self.play(SoundEvent::Cut);
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
            self.play(if cut_mode {
                SoundEvent::Cut
            } else {
                SoundEvent::Copy
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
            self.play(if cut_mode {
                SoundEvent::Cut
            } else {
                SoundEvent::Copy
            });
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
        // `None` mode = use the configured default and only prompt if the
        // dry-run proves something would be destroyed.
        self.paste_with_mode(None);
    }

    /// Paste special (Ctrl+Shift+V): ask for the conflict mode up front,
    /// then paste in it without the after-the-fact confirmation — the user
    /// has already made the destructive choice explicitly. Mirror is only
    /// reachable from here, and only for a copy clipboard.
    pub fn op_paste_special(&self) {
        let Some(dest) = self.model.cwd() else {
            return;
        };
        let clip = crate::clipboard::load_clip();
        if clip.sources.is_empty() {
            self.say("clipboard empty", false);
            return;
        }
        let default = self.config.read().rclone.on_conflict;
        match crate::preflight::prompt_mode(
            self.hwnd(),
            default,
            /*allow_mirror=*/ !clip.cut,
            /*allow_keep_both=*/ !dest.is_remote(),
        ) {
            crate::preflight::PasteChoice::Cancel => self.say("cancelled", false),
            choice => self.paste_with_mode(Some(choice)),
        }
    }

    /// Shared body of paste and paste-special. `choice` is `None` for a
    /// plain paste (resolve later, only if needed) or `Some` when the user
    /// has already picked explicitly.
    fn paste_with_mode(&self, choice: Option<crate::preflight::PasteChoice>) {
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
        // paste even if it's still in flight.
        //
        // **Undo may only ever delete a destination this paste created.**
        // Destinations that already exist have to be filtered out, because
        // every conflict mode can decline to write one: `AddNewOnly` skips
        // it, `Update` spares it when it is newer, `Replace` overwrites it
        // (and we keep no backup, so undo cannot restore it anyway), and
        // `KeepBoth` writes a numbered sibling and leaves it untouched.
        // Without that filter Ctrl+Z deleted exactly the files the chosen
        // mode had deliberately protected — the precise inverse of intent,
        // and unrecoverable.
        //
        // The filter itself is a stat per clipboard entry, so it runs on
        // the paste worker (see [`PastePlan`]) — one Ctrl+V should not
        // block the message pump for the length of the clipboard. What is
        // pushed here is the empty plan the worker fills in.
        let plan = Arc::new(PastePlan::default());
        self.push_undo(UndoAction::Paste {
            plan: plan.clone(),
            cut_mode: clip.cut,
        });

        // Fan out to one Operation per source — a single OpHandle per file
        // keeps stats attribution clean and lets the progress window show
        // "file N of M" without having to rebuild rclone's stat stream.
        let cut = clip.cut;
        // Mirror is never a plain-paste mode, even if `config.toml` names it.
        // It deletes destination files the user never selected, so it has to
        // come from an explicit Paste special choice — a stray Ctrl+V must
        // not be able to trigger it because of a setting edited weeks ago.
        // (It is also meaningless on a file selection: `rclone sync a.txt
        // b.txt` just fails with "Failed to create file system".)
        let default_mode = match self.config.read().rclone.on_conflict {
            ConflictMode::Mirror => ConflictMode::Update,
            m => m,
        };
        let state = self.clone_for_worker();
        std::thread::Builder::new()
            .name("navigator-batch-op".into())
            .spawn(move || state.run_batch(sources, dest, cut, default_mode, choice, plan))
            .expect("spawn batch worker");
    }

    pub fn op_delete(&self) {
        let selected = self.model.selected_paths_with_kind();
        if selected.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        let paths: Vec<NavPath> = selected.iter().map(|(p, _)| p.clone()).collect();
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

        // Split by endpoint, three ways.
        //
        // Remote paths can't go to a local `.trash` dir — rclone would
        // have to cross the boundary — and rclone's own purge is
        // irreversible, so we confirm + skip the undo stack. We carry the
        // per-entry directory flag through so the remote worker can pick
        // `purge` (dirs) vs `deletefile` (files).
        //
        // UNC paths go to the Windows shell. `volume_root_of` resolves
        // `\\host\share\` as a volume, so the trash rename *worked* — it
        // just worked by creating a `.trash` directory at the root of
        // somebody else's file server, which is not ours to litter, and
        // which a macOS or Samba SMB server hands back flagged hidden so
        // the user cannot even find where the file went. The shell gives
        // Explorer's behaviour and Explorer's "permanently delete?"
        // prompt instead. No undo entry: the file is gone, or it is in
        // the Recycle Bin, and either way our trash never held it.
        //
        // Local targets keep the trash flow, and drop the directory flag
        // — they route through a rename, not rclone.
        let mut remote_targets: Vec<(NavPath, bool)> = Vec::new();
        let mut unc: Vec<NavPath> = Vec::new();
        let mut local: Vec<NavPath> = Vec::new();
        for (p, is_dir) in selected {
            if p.is_remote() {
                remote_targets.push((p, is_dir));
            } else if p.is_unc() {
                unc.push(p);
            } else {
                local.push(p);
            }
        }

        if !remote_targets.is_empty() {
            let remote_paths: Vec<NavPath> =
                remote_targets.iter().map(|(p, _)| p.clone()).collect();
            if !confirm_remote_delete(self.main_hwnd(), &remote_paths) {
                // User cancelled. Don't touch local either — avoids a
                // half-delete where they confirmed one endpoint and not
                // the other. Clear pending_focus since no op will fire.
                self.pending_focus.lock().take();
                return;
            }
            self.spawn_remote_purge(remote_targets);
        }

        if !unc.is_empty() {
            self.spawn_shell_delete(unc);
        }

        if local.is_empty() {
            return;
        }

        // Move each local target to `<volume_root>/.trash/<ts>_<n>/<basename>`
        // on the same drive. Same-volume keeps the rename atomic.
        // Naming only; `run_trash_batch` creates each directory just
        // before the rename that lands in it. Doing the `create_dir_all`
        // here cost one syscall per selected item on the UI thread.
        let mut pairs: Vec<(NavPath, NavPath)> = Vec::with_capacity(local.len());
        for p in &local {
            let Some(trash_dir) = trash_dir_on_volume_of(p) else {
                self.say(
                    &format!("failed to resolve trash dir for {}", p.file_name()),
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

    /// Survey every `<drive>\.trash` so the confirmation can name what it
    /// is about to destroy, then hand the result to the UI thread.
    ///
    /// The survey walks every staged file on every drive to total up the
    /// reclaimable space — unbounded work, and it used to run inline on
    /// the UI thread with the app frozen until it finished. It now runs on
    /// a worker and posts [`WMAPP_EMPTY_TRASH_SURVEYED`] back; the
    /// confirmation and the deletion itself continue from
    /// [`Self::confirm_empty_trash_survey`].
    pub fn op_empty_trash(&self) {
        let Some(hwnd) = self.hwnd() else {
            return;
        };
        let speech = self.speech.handle();
        self.say("checking trash…", false);
        std::thread::Builder::new()
            .name("navigator-trash-survey".into())
            .spawn(move || {
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
                    let _ = speech.send(crate::speech::Utterance {
                        text: ".trash is already empty on all drives".into(),
                        interrupt: false,
                    });
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

                let dirs: Vec<PathBuf> = entries.into_iter().map(|(p, _, _)| p).collect();
                post_empty_trash_survey(hwnd, dirs, body, total);
            })
            .expect("spawn trash-survey worker");
    }

    /// UI-thread half of [`Self::op_empty_trash`]: confirm the surveyed
    /// breakdown and, on Yes, spawn the worker that runs `remove_dir_all`
    /// per drive. After completion any in-memory `UndoAction::Delete`
    /// entries are dropped because their staged paths no longer exist.
    /// This is the one path that bypasses the usual rclone-driven mutation
    /// flow — trash dirs are an internal implementation detail, not
    /// user-visible files, so a direct `std::fs` call is fine and avoids
    /// spinning rclone up just to purge a local folder.
    pub fn confirm_empty_trash_survey(&self, dirs: Vec<PathBuf>, body: String, total: u64) {
        if dirs.is_empty() {
            return;
        }
        if !confirm_empty_trash(self.main_hwnd(), &body) {
            return;
        }

        let speech = self.speech.handle();
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);
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

    /// Hand UNC targets to the Windows shell's delete engine in a
    /// detached child process (see [`crate::shell_op`]).
    ///
    /// This is the network-share half of [`Self::op_delete`]. The trash
    /// flow is deliberately not reachable from here: staging to
    /// `<volume_root>\.trash` on a share means creating a directory at
    /// the root of a server we don't own, and an SMB server that flags
    /// dot-prefixed names hidden (macOS and Samba both do) then hides it
    /// from the very user who needs to find their file.
    ///
    /// The shell prompts before it deletes — a share has no Recycle Bin,
    /// so `FOF_ALLOWUNDO` degrades to "are you sure you want to
    /// permanently delete". That prompt *is* the confirmation, which is
    /// why there is no `MessageBoxW` here the way there is for a remote
    /// purge; asking twice for the same delete is how a user learns to
    /// hit Enter on dialogs without reading them.
    ///
    /// No undo entry is pushed. Nothing of ours holds the file.
    fn spawn_shell_delete(&self, targets: Vec<NavPath>) {
        let paths: Vec<PathBuf> = targets.iter().map(|p| p.as_path().to_path_buf()).collect();
        let n = paths.len();

        let child =
            match crate::shell_op::spawn_detached(&paths, crate::shell_op::ShellVerb::Delete, None)
            {
                Ok(c) => c,
                Err(e) => {
                    self.say(&format!("delete failed to start: {}", e), true);
                    return;
                }
            };

        self.say(
            &format!(
                "deleting {} network {} via the shell",
                n,
                if n == 1 { "item" } else { "items" },
            ),
            false,
        );

        // Same reaper shape as the detached paste: it exists for the
        // spoken outcome and the refresh, not to keep the child alive.
        let speech = self.speech.handle();
        let sound = self.sound.clone();
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);
        // The folder the deleted items came out of — the only listing this
        // op invalidates, and only worth re-reading if the user is still
        // looking at it when the shell finally finishes.
        let parent_hint = targets.first().and_then(|p| p.parent());
        std::thread::Builder::new()
            .name("navigator-shell-op-reaper".into())
            .spawn(move || {
                let mut child = child;
                let code = match child.wait() {
                    Ok(s) => s.code().unwrap_or(crate::shell_op::EXIT_FAILED),
                    Err(e) => {
                        tracing::error!("shell-op delete wait: {}", e);
                        crate::shell_op::EXIT_FAILED
                    }
                };
                let (text, bad) = match code {
                    crate::shell_op::EXIT_OK if n == 1 => ("1 item deleted".to_string(), false),
                    crate::shell_op::EXIT_OK => (format!("{} items deleted", n), false),
                    // The shell reports "user said No at the prompt" and
                    // "user hit Cancel mid-run" identically, so this
                    // covers both. Neither is a failure.
                    crate::shell_op::EXIT_ABORTED => ("delete cancelled".to_string(), false),
                    _ => ("delete failed".to_string(), true),
                };
                sound.play(match code {
                    crate::shell_op::EXIT_OK => SoundEvent::DeleteDone,
                    crate::shell_op::EXIT_ABORTED => SoundEvent::Cancelled,
                    _ => SoundEvent::Error,
                });
                let _ = speech.send(crate::speech::Utterance {
                    text,
                    interrupt: bad,
                });
                if let Some(state) = state_weak.upgrade()
                    && let Some(parent) = parent_hint
                {
                    state.refresh_dir(&parent);
                }
            })
            .expect("spawn shell-op delete reaper");
    }

    /// Fire `rclone purge` once per remote target on a background
    /// thread. No undo — rclone purge is destructive, and most backends
    /// (S3 without versioning, SFTP, WebDAV) have no recovery path. UI
    /// refreshes when the last target finishes.
    fn spawn_remote_purge(&self, targets: Vec<(NavPath, bool)>) {
        let rclone = self.rclone.clone();
        let speech = self.speech.handle();
        let sound = self.sound.clone();
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);
        let parent_hint = targets.first().and_then(|(p, _)| p.parent());

        let _ = speech.send(crate::speech::Utterance {
            text: format!("deleting {} remote item(s)", targets.len()),
            interrupt: false,
        });

        std::thread::Builder::new()
            .name("navigator-remote-purge".into())
            .spawn(move || {
                let mut ok_count = 0usize;
                let mut fail_count = 0usize;
                let mut why = String::new();
                for (t, is_dir) in &targets {
                    let op = Operation::Delete {
                        targets: vec![t.clone()],
                        is_dir: *is_dir,
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
                        if let OpEvent::Done { success, error, .. } = ev {
                            if success {
                                ok_count += 1;
                            } else {
                                fail_count += 1;
                                // Keep the first reason: a remote delete
                                // that fails wholesale (expired token, no
                                // network) fails identically for every
                                // item, and "3 failed" alone gives the
                                // user nothing to act on.
                                if why.is_empty()
                                    && let Some(e) = error.as_ref()
                                {
                                    error!("remote delete: {}", e.log_line());
                                    why = e.summary();
                                }
                            }
                            break;
                        }
                    }
                }
                sound.play(if fail_count == 0 {
                    SoundEvent::DeleteDone
                } else {
                    SoundEvent::Error
                });
                let _ = speech.send(crate::speech::Utterance {
                    text: match (fail_count, why.is_empty()) {
                        (0, _) => format!("deleted {} remote item(s)", ok_count),
                        (n, false) => format!("deleted {}, {} failed: {}", ok_count, n, why),
                        (n, true) => format!("deleted {}, {} failed", ok_count, n),
                    },
                    interrupt: fail_count > 0,
                });
                if let Some(state) = state_weak.upgrade()
                    && let Some(parent) = parent_hint
                {
                    state.refresh_dir(&parent);
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
            sound: self.sound.clone(),
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
                // Remote paths must cross as rclone `name:sub` syntax — the
                // raw `\\?\NavigatorRemote\...` sentinel is useless in the Run
                // dialog or a CLI arg. Local paths copy verbatim.
                let s = p.rclone_arg().unwrap_or_else(|| p.to_string());
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
        // DROPEFFECT_COPY = 1 — a paste reproduces the files.
        match set_clipboard_hdrop(self.main_hwnd(), &os_paths, 1) {
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

    /// Cut the current selection to the *real* Windows clipboard as
    /// `CF_HDROP` with a `Preferred DropEffect = MOVE` hint, so a
    /// subsequent paste (in Explorer or navigator's own
    /// `op_paste_from_clipboard`) moves the files instead of copying.
    /// The mirror of `op_copy_to_clipboard`; remote (rclone) paths are
    /// rejected — the shell can't resolve `\\?\NavigatorRemote\...`.
    pub fn op_cut_to_clipboard(&self) {
        let paths = self.model.selected_paths();
        if paths.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        if paths.iter().any(|p| p.is_remote()) {
            self.say("can't cut remote paths to OS clipboard", true);
            return;
        }
        let n = paths.len();
        let os_paths: Vec<std::path::PathBuf> =
            paths.iter().map(|p| p.as_path().to_path_buf()).collect();
        // DROPEFFECT_MOVE = 2 — a paste relocates the files.
        match set_clipboard_hdrop(self.main_hwnd(), &os_paths, 2) {
            Ok(()) => {
                let msg = if n == 1 {
                    "1 item cut to OS clipboard".to_string()
                } else {
                    format!("{} items cut to OS clipboard", n)
                };
                self.say(&msg, false);
            }
            Err(e) => {
                self.say(&format!("OS clipboard failed: {}", e), true);
            }
        }
    }

    /// Paste whatever files sit on the real Windows clipboard
    /// (`CF_HDROP`) into the current folder using the Windows shell copy
    /// engine (`SHFileOperationW`), honouring the clipboard's `Preferred
    /// DropEffect` (move vs copy). Deliberately bypasses rclone — the
    /// whole point is to hand large batches to the shell so antivirus
    /// heuristics don't flag rclone streaming thousands of files.
    ///
    /// The transfer runs in a **detached child process** (see
    /// [`crate::shell_op`]), not here. `SHFileOperationW` is synchronous,
    /// so calling it from the window procedure froze the whole app for the
    /// length of the copy *and* tied the copy's life to navigator's:
    /// closing the window killed it outright. Now this function only reads
    /// the clipboard and launches the helper, and a small reaper thread
    /// announces the outcome if we are still around to hear it.
    pub fn op_paste_from_clipboard(&self) {
        let Some(dest) = self.model.cwd() else {
            return;
        };
        // The shell copy engine only understands real local directories.
        if dest.is_remote() || dest.is_this_pc() || dest.is_remotes_root() {
            self.say("can't paste here", true);
            return;
        }

        let (sources, is_move) = match get_clipboard_hdrop(self.main_hwnd()) {
            Ok(v) => v,
            Err(e) => {
                self.say(&format!("OS clipboard read failed: {}", e), true);
                return;
            }
        };
        if sources.is_empty() {
            self.say("no files on OS clipboard", false);
            return;
        }

        let n = sources.len();
        let dest_path = dest.as_path().to_path_buf();
        let verb = if is_move {
            crate::shell_op::ShellVerb::Move
        } else {
            crate::shell_op::ShellVerb::Copy
        };
        let child = match crate::shell_op::spawn_detached(&sources, verb, Some(&dest_path)) {
            Ok(c) => c,
            Err(e) => {
                self.say(&format!("paste failed to start: {}", e), true);
                return;
            }
        };

        self.play(SoundEvent::PasteStart);
        self.say(
            &format!(
                "{} {} {} in a separate process",
                if is_move { "moving" } else { "copying" },
                n,
                if n == 1 { "item" } else { "items" },
            ),
            false,
        );

        // Reaper: purely for the spoken summary and the closing refresh.
        // It is not what keeps the transfer alive — the child owns itself —
        // so if navigator exits first the copy simply finishes unannounced.
        let speech = self.speech.handle();
        let sound = self.sound.clone();
        let state_weak = self.self_weak.get().cloned().unwrap_or_else(Weak::new);
        // Only the destination listing changed, and a detached shell copy
        // can run for hours — long enough that the user is very likely
        // somewhere else by the time this fires.
        let dest_dir = dest.clone();
        std::thread::Builder::new()
            .name("navigator-shell-op-reaper".into())
            .spawn(move || {
                let mut child = child;
                let code = match child.wait() {
                    Ok(s) => s.code().unwrap_or(crate::shell_op::EXIT_FAILED),
                    Err(e) => {
                        tracing::error!("shell-op wait: {}", e);
                        crate::shell_op::EXIT_FAILED
                    }
                };
                let verb = if is_move { "moved" } else { "copied" };
                let (text, bad) = match code {
                    crate::shell_op::EXIT_OK if n == 1 => (format!("1 item {}", verb), false),
                    crate::shell_op::EXIT_OK => (format!("{} items {}", n, verb), false),
                    crate::shell_op::EXIT_ABORTED => ("paste cancelled".to_string(), false),
                    _ => ("paste failed".to_string(), true),
                };
                sound.play(match code {
                    crate::shell_op::EXIT_OK if is_move => SoundEvent::MoveDone,
                    crate::shell_op::EXIT_OK => SoundEvent::CopyDone,
                    crate::shell_op::EXIT_ABORTED => SoundEvent::Cancelled,
                    _ => SoundEvent::Error,
                });
                let _ = speech.send(crate::speech::Utterance {
                    text,
                    interrupt: bad,
                });
                if let Some(state) = state_weak.upgrade() {
                    state.refresh_dir(&dest_dir);
                }
            })
            .expect("spawn shell-op reaper");
    }

    /// Extract the selected archives with `7z.exe`.
    ///
    /// Two shapes, and the selection decides which:
    ///
    /// * **Files** are extracted as picked, using the full
    ///   [`EXTRACTABLE_EXTENSIONS`](crate::extract::EXTRACTABLE_EXTENSIONS)
    ///   set — pointing at a specific `.exe` or `.iso` is an explicit act.
    /// * **Folders** become sweep roots: the worker walks the tree and
    ///   extracts every archive it finds, each one *in place* next to
    ///   itself, so a folder of nested downloads unpacks in one gesture.
    ///   A sweep sees only [`SWEEP_EXTENSIONS`](crate::extract::SWEEP_EXTENSIONS)
    ///   and always confirms first — see [`Self::confirm_extract_survey`].
    ///
    /// The walk is unbounded IO, so it runs on `navigator-extract-survey`
    /// and posts [`WMAPP_EXTRACT_SURVEYED`](crate::window::WMAPP_EXTRACT_SURVEYED)
    /// back rather than stalling the message pump. A selection of plain
    /// files skips the survey entirely and keeps the old zero-dialog path.
    ///
    /// Behaviour (delete after, wrapper folder) is read from the
    /// `[extraction]` config section. The file watcher folds new entries
    /// into the listing, so no refresh is needed.
    pub fn op_extract(&self) {
        let selection = self.model.selected_paths_with_kind();
        if selection.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        // Remote items are dropped wholesale: 7z can't read the synthetic
        // remote path, and a remote folder can't be walked with `read_dir`
        // either.
        let local: Vec<(navigator_core::NavPath, bool)> = selection
            .into_iter()
            .filter(|(p, _)| !p.is_remote())
            .collect();
        let (direct, roots) = crate::extract::split_extract_selection(&local);
        if direct.is_empty() && roots.is_empty() {
            self.say("no extractable archives selected", true);
            return;
        }
        let seven_zip = match crate::extract::find_7z() {
            Some(p) => p,
            None => {
                self.say("7z not found; install 7-Zip to extract archives", true);
                return;
            }
        };

        if roots.is_empty() {
            self.spawn_extract(direct, seven_zip);
            return;
        }

        let Some(hwnd) = self.hwnd() else { return };
        let speech = self.speech.handle();
        let delete_after = self.config.read().extraction.delete_when_extracted;
        self.say("searching for archives\u{2026}", false);
        std::thread::Builder::new()
            .name("navigator-extract-survey".into())
            .spawn(move || {
                let mut swept: Vec<navigator_core::NavPath> = Vec::new();
                for root in &roots {
                    for found in crate::extract::sweep_archives(root.as_path()) {
                        if let Ok(nav) = NavPath::new(found) {
                            swept.push(nav);
                        }
                    }
                }
                let targets = crate::extract::merge_targets(direct, swept);
                if targets.is_empty() {
                    let _ = speech.send(crate::speech::Utterance {
                        text: "no archives found".into(),
                        interrupt: true,
                    });
                    return;
                }
                let body = crate::extract::sweep_confirm_body(&targets, &roots, delete_after);
                post_extract_survey(hwnd, targets, seven_zip, body);
            })
            .expect("spawn extract-survey worker");
    }

    /// UI-thread half of a recursive [`Self::op_extract`]: confirm what the
    /// sweep found, then run it.
    ///
    /// The confirmation is not ceremony. A sweep is one keystroke on a
    /// folder row, `[extraction] delete_when_extracted` defaults to on,
    /// and that delete is a plain `remove_file` — it does not stage to
    /// `.trash`, so there is no undo. Ctrl+E on a large tree is therefore
    /// an unrecoverable action whose scope the user cannot see from the
    /// listing, so they get the count before it runs. Defaults to No, like
    /// every other destructive confirm here.
    pub fn confirm_extract_survey(
        &self,
        targets: Vec<navigator_core::NavPath>,
        seven_zip: PathBuf,
        body: String,
    ) {
        if targets.is_empty() {
            return;
        }
        if !confirm_extract_sweep(self.main_hwnd(), &body) {
            self.say("extract cancelled", false);
            return;
        }
        self.spawn_extract(targets, seven_zip);
    }

    /// Spawn the extract worker for an already-resolved archive list.
    ///
    /// Holds an [`OpGuard`] for the worker's lifetime so `ops_in_flight`
    /// counts it: extraction is a long-running file operation like any
    /// other, and without the guard Alt+F4 closed the window mid-extract
    /// with no warning at all — killing 7z (it shares the rclone job) and
    /// skipping the archive purge.
    fn spawn_extract(&self, targets: Vec<navigator_core::NavPath>, seven_zip: PathBuf) {
        let opts = self.config.read().extraction;
        let speech = self.speech.handle();
        let sound = self.sound.clone();
        let guard = self.op_guard();
        let total = targets.len();
        self.say(&format!("extracting {} archive(s)", total), false);
        std::thread::Builder::new()
            .name("navigator-extract".into())
            .spawn(move || {
                let _guard = guard;
                crate::extract::run_extract(targets, opts, seven_zip, speech, sound)
            })
            .expect("spawn extract worker");
    }

    /// Compress the selected file(s)/folder(s) into a single sibling `.zip`
    /// via `7z` on PATH. A lone selected folder is zipped by its contents
    /// (no wrapping subfolder); any other selection keeps each item's name
    /// inside the one archive. The archive is named after the focused entry.
    /// Remote (rclone) selections are skipped — 7z can't read
    /// `\\?\NavigatorRemote\...`. Originals are never deleted. Runs on a
    /// worker; the file watcher folds the new `.zip` into the listing so no
    /// refresh is needed.
    pub fn op_zip(&self) {
        let selection = self.model.selected_paths();
        if selection.is_empty() {
            self.say("nothing selected", false);
            return;
        }
        let local: Vec<navigator_core::NavPath> =
            selection.into_iter().filter(|p| !p.is_remote()).collect();
        if local.is_empty() {
            self.say("can't zip remote items", true);
            return;
        }
        let seven_zip = match crate::extract::find_7z() {
            Some(p) => p,
            None => {
                self.say("7z not found; install 7-Zip to zip files", true);
                return;
            }
        };
        // The archive is named after the focused row when it's part of the
        // local selection; otherwise fall back to the first selected item.
        let focused = self.model.cwd().and_then(|cwd| {
            let idx = self.model.selection_snapshot().focus()?;
            let entry = self.model.get(idx)?;
            Some(cwd.join(&entry.name))
        });
        let primary = focused
            .filter(|p| local.iter().any(|l| l == p))
            .unwrap_or_else(|| local[0].clone());
        let speech = self.speech.handle();
        let sound = self.sound.clone();
        let total = local.len();
        self.say(&format!("zipping {} item(s)", total), false);
        std::thread::Builder::new()
            .name("navigator-zip".into())
            .spawn(move || crate::extract::run_zip(local, primary, seven_zip, speech, sound))
            .expect("spawn zip worker");
    }

    /// Show the read-only properties viewer for the focused entry. For
    /// directories the recursive size / counts / extension histogram
    /// come from a worker thread so a giant tree doesn't freeze the UI;
    /// the viewer only opens once the scan finishes.
    ///
    /// Three routes, and picking the wrong one fails *quietly*: a This PC
    /// row is a display string (`"D: (Data)"`), not a path — `cwd.join` on
    /// it builds `\\?\NavigatorThisPC\D: (Data)`, which no filesystem can
    /// stat, so the folder walk found nothing and rendered a page of
    /// zeroes. Drives answer from `navigator_fs::drive_info` instead, and
    /// remotes from rclone (same class of bug — see `op_dump_tree`).
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

        // This PC: the row names a volume, so report the volume. No walk —
        // capacity comes from the OS in constant time, and the only
        // enumeration is one non-recursive listing of the root.
        if cwd.is_this_pc() {
            let display = entry.name.clone();
            let Some(root) = navigator_fs::drive_path_from_display(&display) else {
                self.say("not a drive", true);
                return;
            };
            std::thread::Builder::new()
                .name("navigator-properties-drive".into())
                .spawn(move || {
                    let info = navigator_fs::drive_info(&root);
                    let top = NavPath::new(&root)
                        .ok()
                        .and_then(|p| crate::props::top_level_counts(&p));
                    let body = crate::props::format_drive_properties(&display, &info, top.as_ref());
                    post_viewer(hwnd, title, body);
                })
                .expect("spawn drive properties worker");
            return;
        }

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
    ///
    /// Remote targets take a separate route: `props::dump_tree_toml` walks
    /// with `FindFirstFileExW`, which cannot see a `\\?\NavigatorRemote\…`
    /// path and reported an empty tree for every remote. One
    /// `rclone lsjson --recursive` replaces the whole walk.
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
        // The remotes sentinel is a list of configured remotes, not a
        // directory — there is nothing below it to walk.
        if target.is_remotes_root() {
            self.say("can't dump the remotes list", true);
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

        if target.is_remote() {
            let rclone = self.rclone.clone();
            std::thread::Builder::new()
                .name("navigator-dump-tree-remote".into())
                .spawn(move || {
                    let arg = target.rclone_arg().unwrap_or_default();
                    let body = match rclone.lsjson_recursive(&arg) {
                        Ok(items) => crate::props::dump_tree_toml_remote(&target, &items),
                        Err(e) => crate::props::dump_tree_toml_error(&target, &e.to_string()),
                    };
                    post_viewer(hwnd, title, body);
                })
                .expect("spawn remote dump-tree worker");
            return;
        }

        std::thread::Builder::new()
            .name("navigator-dump-tree".into())
            .spawn(move || {
                let body = crate::props::dump_tree_toml(&target);
                post_viewer(hwnd, title, body);
            })
            .expect("spawn dump-tree worker");
    }

    /// File → Compare trees…: walk the folder the user is standing in,
    /// diff it against a tree they paste in, and show the result in the
    /// viewer.
    ///
    /// The prompt is modal on the UI thread (it is one paste box), but the
    /// walk, the parse and the diff all run on a worker — the walk is
    /// unbounded IO and the paste can be megabytes, neither of which
    /// belongs in the message pump.
    ///
    /// Remote roots branch to `lsjson --recursive` for the same reason
    /// `op_dump_tree` does: `walk_tree` uses `FindFirstFileExW`, which
    /// cannot see a `\\?\NavigatorRemote\…` path and would report the
    /// whole remote as missing rather than failing loudly.
    pub fn op_compare_trees(&self) {
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        if cwd.is_this_pc() {
            self.say("can't compare This PC", true);
            return;
        }
        if cwd.is_remotes_root() {
            self.say("can't compare the remotes list", true);
            return;
        }
        let Some(hwnd) = self.hwnd() else {
            return;
        };
        let label = cwd.rclone_arg().unwrap_or_else(|| cwd.to_string());
        let Some(text) = crate::compare_dialog::open(hwnd.0, &label) else {
            return;
        };
        let title = format!("Compare — {label}");
        self.say("comparing trees…", false);

        if cwd.is_remote() {
            let rclone = self.rclone.clone();
            std::thread::Builder::new()
                .name("navigator-compare-remote".into())
                .spawn(move || {
                    let arg = cwd.rclone_arg().unwrap_or_default();
                    let body = match rclone.lsjson_recursive(&arg) {
                        Ok(items) => crate::compare::compare_against_text(
                            crate::compare::tree_from_remote_items(label, &items),
                            &text,
                        ),
                        Err(e) => crate::compare::format_error(&label, &e.to_string()),
                    };
                    post_viewer(hwnd, title, body);
                })
                .expect("spawn remote compare worker");
            return;
        }

        std::thread::Builder::new()
            .name("navigator-compare".into())
            .spawn(move || {
                let (entries, errors) = crate::props::walk_tree(&cwd);
                let mut here = crate::compare::Tree::new(label, entries);
                here.errors = errors;
                let body = crate::compare::compare_against_text(here, &text);
                post_viewer(hwnd, title, body);
            })
            .expect("spawn compare worker");
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

    /// Create an empty file named `name` inside the current directory and
    /// open it in the OS default app for its type. The caller prompts for
    /// the name (see the `new_folder` dialog in `File` mode). A `.` segment
    /// is required so the shell has a type to resolve. Pending focus is
    /// armed so the created row gets the caret after the post-op refresh.
    pub fn op_new_file(&self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            self.say("file name empty", true);
            return;
        }
        if name.contains(['\\', '/', ':']) || name == "." || name == ".." {
            self.say("invalid file name", true);
            return;
        }
        // Require a non-empty segment after the final dot so ShellExecute
        // can resolve a handler. Accepts `notes.txt`, `archive.tar.gz`, and
        // dotfiles like `.gitignore`; rejects `notes` (no dot) and `notes.`
        // (trailing dot, empty extension).
        if !name.contains('.') || name.ends_with('.') {
            self.say("file name needs an extension, like notes.txt", true);
            return;
        }
        let Some(cwd) = self.model.cwd() else {
            return;
        };
        if cwd.is_this_pc() || cwd.is_remotes_root() {
            self.say("cannot create file here", true);
            return;
        }
        let dst = cwd.join(&name);
        // If it already exists locally, don't clobber its contents — just
        // open the existing file (`rclone touch` would only bump mtime).
        if !dst.is_remote() && dst.as_path().exists() {
            self.say(&format!("{} already exists, opening", name), false);
            self.open_file(dst);
            return;
        }
        self.set_pending_focus(dst.clone());
        self.spawn_new_file(dst);
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

    /// Touch a new empty file via rclone, then open it once it exists.
    /// Separate from [`spawn_op`] because the post-op step (ShellExecute
    /// the freshly created file) isn't part of the generic op flow.
    fn spawn_new_file(&self, file: NavPath) {
        let ctx = self.clone_for_worker();
        thread::Builder::new()
            .name("navigator-new-file".into())
            .spawn(move || ctx.run_new_file(file))
            .expect("spawn new-file thread");
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
        let guard = self.op_guard();

        let _ = speech.send(crate::speech::Utterance {
            text: format!("uploading to {}", remote_display),
            interrupt: false,
        });

        std::thread::Builder::new()
            .name("navigator-remote-upload".into())
            .spawn(move || {
                let _guard = guard;
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
                let mut why = String::new();
                for ev in handle.events.iter() {
                    if let OpEvent::Done {
                        success: ok, error, ..
                    } = ev
                    {
                        success = ok;
                        if let Some(e) = error.as_ref() {
                            error!("remote upload: {}", e.log_line());
                            why = e.summary();
                        }
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
                    // Re-list the remote folder so it picks up the new
                    // mtime/size, and arm `pending_focus` with the
                    // uploaded file so the caret lands on it instead of
                    // snapping to row 0 — but only while the user is
                    // still in that folder. They are free to browse
                    // elsewhere while an upload runs, and neither the
                    // listing nor the caret is ours to move once they do.
                    if let Some(state) = state_weak.upgrade()
                        && let Some(parent) = remote.parent()
                        && state.is_viewing(&parent)
                    {
                        state.set_pending_focus(remote.clone());
                        state.refresh_dir(&parent);
                    }
                } else {
                    cache.finish_prompt(&staged, None);
                    let _ = speech.send(crate::speech::Utterance {
                        text: format!(
                            "upload failed: {}",
                            if why.is_empty() { "see log" } else { &why }
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

/// Confirm a recursive extract. Yes/No, defaulting to No: the sweep can
/// reach an entire drive from one keystroke on a folder row, and with
/// `delete_when_extracted` on (the default) the archives it finds are
/// gone for good afterwards. Same safe-default rule the paste-conflict
/// dialogs follow.
fn confirm_extract_sweep(parent: Option<HWND>, body: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO,
        MessageBoxW,
    };
    use windows::core::PCWSTR;

    let title_w: Vec<u16> = "Extract archives?".encode_utf16().chain([0]).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain([0]).collect();
    let is_foreground = parent
        .map(|h| unsafe { GetForegroundWindow() } == h)
        .unwrap_or(false);
    let mut flags = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2;
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
/// Hand a finished empty-trash survey to the UI thread. Takes `HwndSend`
/// by value on purpose: reaching into `hwnd.0` from inside a worker
/// closure makes Rust capture the bare `HWND`, which is not `Send`.
fn post_empty_trash_survey(hwnd: HwndSend, dirs: Vec<PathBuf>, body: String, total: u64) {
    let payload = Box::into_raw(Box::new((dirs, body, total)));
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd.0),
            crate::window::WMAPP_EMPTY_TRASH_SURVEYED,
            WPARAM(0),
            LPARAM(payload as isize),
        );
    }
}

/// Hand a finished recursive-extract survey to the UI thread. Same
/// leak-a-`Box` shape as [`post_empty_trash_survey`]; the window proc
/// reclaims it.
fn post_extract_survey(
    hwnd: HwndSend,
    targets: Vec<navigator_core::NavPath>,
    seven_zip: PathBuf,
    body: String,
) {
    let payload = Box::into_raw(Box::new((targets, seven_zip, body)));
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd.0),
            crate::window::WMAPP_EXTRACT_SURVEYED,
            WPARAM(0),
            LPARAM(payload as isize),
        );
    }
}

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
    /// Event sounds. Carried by value like the speech sender so a worker
    /// never has to upgrade the `Weak<AppState>` just to chime.
    sound: SoundPlayer,
    /// Folder this operation acts on, captured when the worker span. Only
    /// ever re-listed through [`WorkerCtx::refresh_with_focus`], which
    /// checks the user is still there first — see the note on that method.
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

/// Progress reporting for one *user action*, across every rclone
/// invocation that action takes.
///
/// This is the piece batching removed. A paste used to narrate its own
/// item loop (`"1 of 200: a.txt"`, `"2 of 200: b.txt"`, …); collapsing 200
/// items into one `--files-from` call deleted the loop and with it every
/// utterance between "copying" and "done". Reporting per invocation
/// instead would be just as wrong in the other direction — a three-folder
/// paste would run 0–100% three times.
///
/// So the meter belongs to the job: each invocation declares its weight in
/// job units and feeds its own fraction in, and [`crate::narrate`] turns
/// that into one monotonic percentage, one spoken cadence, and one window.
struct OpProgress {
    /// Present-tense verb for the window caption ("Copying").
    verb: &'static str,
    window: Option<crate::progress::ProgressHandle>,
    speech: Sender<crate::speech::Utterance>,
    meter: crate::narrate::Meter,
    /// `None` when periodic speech is switched off; the window (and the
    /// caller's completion summary) still report.
    cadence: Option<crate::narrate::Cadence>,
    /// Set by the progress window's Cancel button. Checked between
    /// invocations so cancelling a 200-item paste stops the paste, not
    /// just the file in flight.
    cancelled: Arc<AtomicBool>,
    /// File the window's "Current:" line shows. Primary source is the
    /// stats record's `transferring` list (what is moving *now*); ordinary
    /// log records fill in between ticks with the last file that finished.
    /// Reading the stats record's top-level `object` — which does not
    /// exist — is what left this line permanently blank.
    current_file: String,
    stats: navigator_rclone::Progress,
    /// Parent for the failure dialog. The job reports from the worker
    /// thread it ran on, like every other dialog `WorkerCtx` opens.
    hwnd: Option<HwndSend>,
    /// Everything that went wrong, reported once by [`Self::finish`].
    ///
    /// **A user action is not an rclone invocation** — the same rule the
    /// meter enforces for progress. Each invocation used to open its own
    /// modal error dialog, so deleting five files that were already gone
    /// meant five identical dialogs, each blocking the worker until
    /// dismissed, and a batch that failed per-group put one up mid-job
    /// while later groups were still running.
    failures: Vec<crate::narrate::Failure>,
}

impl OpProgress {
    fn new(ctx: &WorkerCtx, verb: &'static str, total_units: u64) -> Self {
        let cadence = match ctx.announce_interval_secs {
            0 => None,
            n => Some(crate::narrate::Cadence::new(
                Duration::from_secs(n as u64),
                Instant::now(),
            )),
        };
        let me = Self {
            verb,
            window: ctx.progress.clone(),
            speech: ctx.speech.clone(),
            meter: crate::narrate::Meter::new(total_units),
            cadence,
            cancelled: Arc::new(AtomicBool::new(false)),
            current_file: String::new(),
            stats: navigator_rclone::Progress::default(),
            hwnd: ctx.hwnd,
            failures: Vec::new(),
        };
        if let Some(w) = me.window.as_ref() {
            w.post_begin(&crate::narrate::window_title(verb, &me.meter));
        }
        me
    }

    /// Speak the opening line. Immediate feedback that the keystroke
    /// registered, before rclone has produced a single statistic.
    fn opening(&self, gerund: &str, first: Option<&str>) {
        let _ = self.speech.try_send(crate::speech::Utterance {
            text: crate::narrate::opening(gerund, self.meter.total(), first),
            interrupt: false,
        });
    }

    /// Start an invocation worth `weight` job units.
    fn begin(&mut self, weight: u64) {
        self.meter.begin(weight);
        self.stats = navigator_rclone::Progress::default();
        self.current_file.clear();
    }

    /// Fold the finished invocation into the job total. Called for
    /// failures too — a failed item is one the user is no longer waiting
    /// on, and freezing the percentage on it would be a worse lie than
    /// counting it.
    fn end(&mut self) {
        self.meter.finish();
        self.emit();
    }

    /// Account for units the job decided not to run at all (a skipped
    /// item, a missing source). Without this the percentage stalls short
    /// of 100 on a job that did everything it meant to.
    fn skip(&mut self, units: u64) {
        self.begin(units);
        self.end();
    }

    fn on_progress(&mut self, p: navigator_rclone::Progress) {
        if let Some(f) = p.fraction() {
            self.meter.set_fraction(f);
        }
        if let Some(name) = p.current.as_deref().filter(|n| !n.is_empty()) {
            self.current_file = name.to_string();
        }
        self.stats = p;
        self.emit();
    }

    /// Feed one rclone log record. Returns the line to append to the
    /// window's log pane, or `None` for records that would only be noise —
    /// the once-a-second stats ticks, which the labels already render.
    fn on_log(&mut self, ev: &navigator_rclone::LogEvent) -> Option<String> {
        if ev.stats.is_some() {
            return None;
        }
        if let Some(obj) = ev.object.as_deref().filter(|o| !o.is_empty()) {
            self.current_file = obj.to_string();
        }
        Some(format!(
            "[{:?}] {}",
            ev.level.unwrap_or(navigator_rclone::LogLevel::Info),
            ev.msg
        ))
    }

    /// Push the current state to the window and, if the cadence allows,
    /// to speech.
    fn emit(&mut self) {
        if let Some(w) = self.window.as_ref() {
            w.post_status(crate::progress::Status {
                title: crate::narrate::window_title(self.verb, &self.meter),
                current: self.current_file.clone(),
                detail: crate::narrate::window_status(&self.meter, &self.stats),
                percent: self.meter.percent(),
            });
        }
        let (Some(cadence), Some(text)) =
            (self.cadence.as_mut(), crate::narrate::phrase(&self.meter))
        else {
            return;
        };
        if cadence.due(Instant::now(), &text) {
            let _ = self.speech.try_send(crate::speech::Utterance {
                text,
                interrupt: false,
            });
        }
    }

    fn log_line(&self, line: &str) {
        if let Some(w) = self.window.as_ref() {
            w.post_log(line);
        }
    }

    /// Wire the window's Cancel button to this invocation's child process.
    /// Re-armed per invocation: the button has to kill whichever rclone is
    /// running *now*, and set the job flag so the loop stops rather than
    /// marching on to the next item.
    fn arm_cancel(&self, canceller: navigator_rclone::Canceller) {
        let Some(w) = self.window.as_ref() else {
            return;
        };
        let flag = Arc::clone(&self.cancelled);
        w.set_cancel(move || {
            flag.store(true, Ordering::Release);
            canceller.cancel();
        });
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Record an rclone failure against the job. `subject` is the job's
    /// own name for the item — better than anything in the log, which
    /// carries `\\?\` absolute paths and Go filesystem descriptions.
    fn record_failure(&mut self, err: &navigator_rclone::RcloneError, subject: Option<String>) {
        error!("rclone: {}", err.log_line());
        self.failures
            .push(crate::narrate::Failure::from_rclone(err, subject));
    }

    /// As [`Self::record_failure`], but for a failure that survived a UAC
    /// retry. The user approved elevation and it *still* failed, which is
    /// a different problem from a plain denial and needs to say so —
    /// otherwise the report reads as "permission denied" to someone who
    /// just granted permission.
    fn record_elevated_failure(
        &mut self,
        err: &navigator_rclone::RcloneError,
        subject: Option<String>,
    ) {
        error!("rclone (elevated): {}", err.log_line());
        let mut f = crate::narrate::Failure::from_rclone(err, subject);
        f.reason = format!("{} even as administrator", f.reason);
        self.failures.push(f);
    }

    /// Record a failure the app detected itself, with no rclone error
    /// behind it — a `--files-from` list we couldn't stage, or a batch
    /// that exited 0 with destinations missing. Without this those
    /// failures reach `finish` invisibly and the job closes clean.
    fn record_problem(&mut self, reason: impl Into<String>, subject: Option<String>) {
        let f = crate::narrate::Failure::app(reason, subject);
        error!("operation failed: {} {:?}", f.reason, f.subject);
        self.failures.push(f);
    }

    /// Close the job out in the window and report whatever went wrong.
    /// Called once, by whoever owns the job — never by `run_op`, which
    /// would flip the window to "Done." after the first of several
    /// invocations.
    ///
    /// A cancelled job is never "Done." no matter what the last invocation
    /// returned, so the flag overrides the caller's verdict here rather
    /// than at each of the four call sites. A cancelled job also reports
    /// nothing: the failures it collected are the user's own doing.
    fn finish(&mut self, success: bool) {
        if let Some(w) = self.window.as_ref() {
            w.clear_cancel();
            w.post_done(success && !self.cancelled());
        }
        if self.cancelled() {
            self.failures.clear();
            return;
        }
        let Some(report) =
            crate::narrate::failure_report(self.verb, self.meter.total(), &self.failures)
        else {
            return;
        };
        self.failures.clear();
        // Speak before the dialog: the headline is short and the dialog
        // steals focus the moment it opens.
        let _ = self.speech.try_send(crate::speech::Utterance {
            text: report.headline.clone(),
            interrupt: true,
        });
        crate::dialogs::show_error(self.hwnd, &report.title, &report.body);
    }
}

impl WorkerCtx {
    fn say(&self, text: impl Into<String>, interrupt: bool) {
        let _ = self.speech.try_send(crate::speech::Utterance {
            text: text.into(),
            interrupt,
        });
    }

    fn play(&self, ev: SoundEvent) {
        self.sound.play(ev);
    }

    /// Sound for a finished job: the outcome always wins over the verb, so
    /// a failed or cancelled copy can never chime like a successful one.
    fn play_outcome(&self, cancelled: bool, failed: bool, done: SoundEvent) {
        self.play(if cancelled {
            SoundEvent::Cancelled
        } else if failed {
            SoundEvent::Error
        } else {
            done
        });
    }

    fn refresh(&self) {
        self.refresh_with_focus(None);
    }

    /// Bring the operation's folder up to date, optionally landing the
    /// caret on `focus` (a path the op just created or restored).
    ///
    /// **Both are skipped when the user has moved on.** `refresh_target`
    /// is the folder captured when the worker span, and this used to post
    /// that listing unconditionally — so finishing a long copy dragged the
    /// user out of whatever folder they had since browsed to, which is
    /// indistinguishable from the app navigating on its own. `pending_focus`
    /// is gated by the same check for the same reason: armed for a folder
    /// the user is no longer in, it is consumed by the *next* listing
    /// anywhere and either matches a same-named row by accident or throws
    /// the caret to row 0.
    fn refresh_with_focus(&self, focus: Option<NavPath>) {
        let (Some(dir), Some(state)) = (self.refresh_target.as_ref(), self.state.upgrade()) else {
            return;
        };
        if !state.is_viewing(dir) {
            return;
        }
        if let Some(target) = focus {
            state.set_pending_focus(target);
        }
        state.refresh_dir(dir);
    }

    fn run_single(self, op: Operation) {
        let _guard = self.state.upgrade().map(|s| s.op_guard());
        // Pick the completion sound before the op consumes it. `Mkdir` and
        // `Touch` are both "something new appeared"; `Rename` is its own
        // event because F2 is a distinct enough gesture to want distinct
        // feedback.
        let done = match &op {
            Operation::Rename { .. } => SoundEvent::RenameDone,
            Operation::Mkdir { .. } | Operation::Touch { .. } => SoundEvent::NewItem,
            _ => SoundEvent::CopyDone,
        };
        let ok = self.run_one(op);
        self.play_outcome(/*cancelled=*/ false, !ok, done);
        self.say(if ok { "done" } else { "operation failed" }, !ok);
        self.refresh();
    }

    /// Create `file` via `rclone touch`, then hand it to the shell so the
    /// OS default app for its extension opens it. `pending_focus` was armed
    /// by the caller, so the post-op refresh lands the caret on the new row
    /// (local paths only — remote opens stage a download asynchronously).
    fn run_new_file(self, file: NavPath) {
        let _guard = self.state.upgrade().map(|s| s.op_guard());
        let ok = self.run_one(Operation::Touch { file: file.clone() });
        self.play_outcome(/*cancelled=*/ false, !ok, SoundEvent::NewItem);
        if ok {
            self.say(format!("created {}", file.file_name()), false);
            if let Some(state) = self.state.upgrade() {
                state.open_file(file);
            }
        } else {
            self.say("could not create file", true);
        }
        self.refresh();
    }

    /// Decide how a plain paste should handle conflicts, prompting only if
    /// the chosen mode would actually destroy something.
    ///
    /// Cheap in the common case. Only a source whose destination name
    /// already exists can possibly conflict, so a paste with no name
    /// collisions returns immediately without spawning rclone at all — and
    /// a non-destructive mode never needs to ask. Dry-run passes are paid
    /// for only on the handful of items that genuinely collide.
    fn resolve_conflicts(
        &self,
        sources: &[NavPath],
        dest_dir: &NavPath,
        cut: bool,
        mode: ConflictMode,
    ) -> crate::preflight::PasteChoice {
        use crate::preflight::{PasteChoice, conflict_candidates, prompt_conflicts};

        if !mode.is_destructive() {
            return PasteChoice::Mode(mode);
        }
        let candidates = conflict_candidates(sources, dest_dir);
        if candidates.is_empty() {
            return PasteChoice::Mode(mode);
        }

        let mut overwrites: Vec<String> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();
        // If detection itself fails we must not assume "no conflict" — fall
        // back to naming the colliding top-level items, which we know exist.
        let mut detection_failed = false;

        // We are about to spawn dry-runs, and rclone has to scan both trees
        // before either answers. On a large or remote paste that is seconds
        // of silence between Ctrl+V and anything happening, which reads as
        // a dropped keystroke — say what we're waiting for.
        self.say("checking destination", false);

        // Detection is two dry-run spawns per operation, so it has to batch
        // for the same reason the transfer does: overwriting 200 files would
        // otherwise cost 400 spawns before a single byte moves. Groups run
        // as one `--files-from` dry-run pair; directories stay per-item.
        // Mirror never batches (`sync --files-from` would report pruning
        // everything unlisted), so it takes the per-item path here too.
        let part = if mode == ConflictMode::Mirror {
            crate::batch::Partition {
                groups: Vec::new(),
                singles: candidates.clone(),
            }
        } else {
            // Must be `path_is_dir`, not `as_path().is_dir()`: the latter
            // returns false for every remote path, so a remote *directory*
            // would join a `--files-from` list, which rclone ignores
            // silently — detection would report no conflicts and the real
            // (correctly per-item) run would then overwrite the folder's
            // contents with no dialog. Same predicate as `run_batch`.
            crate::batch::partition(&candidates, |s| self.path_is_dir(s))
        };

        for group in &part.groups {
            let seq = BATCH_LIST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Ok(list) = crate::batch::TempList::write(&group.names, seq) else {
                detection_failed = true;
                continue;
            };
            let op = if cut {
                Operation::MoveBatch {
                    src_root: group.src_root.clone(),
                    list_file: list.path().to_path_buf(),
                    dest_dir: dest_dir.clone(),
                    mode,
                }
            } else {
                Operation::CopyBatch {
                    src_root: group.src_root.clone(),
                    list_file: list.path().to_path_buf(),
                    dest_dir: dest_dir.clone(),
                    mode,
                }
            };
            match self.rclone.conflicts(&op) {
                Ok(report) => {
                    // With --files-from the reported objects are the listed
                    // names themselves, so they need no prefixing.
                    overwrites.extend(
                        report
                            .overwrites
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned()),
                    );
                    deletes.extend(
                        report
                            .deletes
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned()),
                    );
                }
                Err(e) => {
                    tracing::warn!("batched conflict detection failed: {}", e);
                    detection_failed = true;
                }
            }
        }

        for src in &part.singles {
            let op = if cut {
                Operation::Move {
                    sources: vec![src.clone()],
                    dest_dir: dest_dir.clone(),
                    mode,
                }
            } else {
                Operation::Copy {
                    sources: vec![src.clone()],
                    dest_dir: dest_dir.clone(),
                    mode,
                }
            };
            match self.rclone.conflicts(&op) {
                Ok(report) => {
                    // rclone reports destination-relative object paths: the
                    // bare filename for a single-file copy, or a path inside
                    // the tree for a directory copy. Prefix the latter so the
                    // dialog shows where the file actually lives.
                    let is_dir = self.path_is_dir(src);
                    let name = src.file_name().to_string();
                    let render = |p: &std::path::PathBuf| -> String {
                        let rel = p.to_string_lossy();
                        if is_dir {
                            format!("{}\\{}", name, rel.replace('/', "\\"))
                        } else {
                            name.clone()
                        }
                    };
                    overwrites.extend(report.overwrites.iter().map(&render));
                    deletes.extend(report.deletes.iter().map(&render));
                }
                Err(e) => {
                    tracing::warn!("conflict detection failed for {}: {}", src, e);
                    detection_failed = true;
                }
            }
        }

        if detection_failed && overwrites.is_empty() {
            overwrites = candidates
                .iter()
                .map(|s| s.file_name().to_string())
                .collect();
        }
        if overwrites.is_empty() && deletes.is_empty() {
            // Everything that collides is either identical or protected by
            // the mode (e.g. a newer destination under Update). Nothing to
            // warn about — run without a dialog.
            return PasteChoice::Mode(mode);
        }
        prompt_conflicts(
            self.hwnd,
            mode,
            &overwrites,
            &deletes,
            // Keep-both needs a true existence check to pick `foo (1)`, and
            // `unique_numbered_path` can only probe the local filesystem. On
            // a remote destination it would return the name unchanged and
            // the item would quietly fall through to an additive copy (i.e.
            // skip) instead of keeping both — so don't offer it there.
            /*keep_both_offered=*/
            !dest_dir.is_remote(),
        )
    }

    /// Copy or move one batchable group in a single rclone invocation.
    ///
    /// Returns `(all_landed, first_destination)`. The temp list file lives
    /// exactly as long as this call — `TempList` removes it on drop.
    ///
    /// **Why the post-run verification.** `rclone copy --files-from` exits 0
    /// when a listed entry produces nothing: a directory (silently ignored)
    /// or a name that no longer resolves both look like success. The exit
    /// code alone would let a paste report "done" having moved nothing.
    /// `batch::partition` already keeps directories out, but for a local
    /// destination a cheap `exists()` per name proves it rather than
    /// trusting it. A remote destination skips the check — probing it means
    /// an `lsjson` round-trip per group, which would give back the latency
    /// this whole path exists to remove — and falls back to the exit code.
    fn run_group(
        &self,
        group: &crate::batch::FileGroup,
        dest_dir: &NavPath,
        cut: bool,
        mode: ConflictMode,
        total: usize,
        prog: &mut OpProgress,
    ) -> (bool, Option<NavPath>) {
        let seq = BATCH_LIST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let list = match crate::batch::TempList::write(&group.names, seq) {
            Ok(l) => l,
            Err(e) => {
                // Can't stage the list — report failure rather than
                // silently falling back and copying nothing. The group's
                // units still have to leave the meter, or the paste's
                // percentage stalls short of 100 forever.
                tracing::error!("could not write --files-from list: {e}");
                prog.skip(group.names.len() as u64);
                prog.record_problem(
                    format!("could not stage the batch list: {e}"),
                    op_subject(&Operation::Mkdir {
                        dir: dest_dir.clone(),
                    }),
                );
                return (false, None);
            }
        };

        let op = if cut {
            Operation::MoveBatch {
                src_root: group.src_root.clone(),
                list_file: list.path().to_path_buf(),
                dest_dir: dest_dir.clone(),
                mode,
            }
        } else {
            Operation::CopyBatch {
                src_root: group.src_root.clone(),
                list_file: list.path().to_path_buf(),
                dest_dir: dest_dir.clone(),
                mode,
            }
        };

        // The group is worth one job unit per listed file, so its internal
        // progress moves the job's percentage in proportion to the share
        // of the paste it actually carries.
        prog.begin(group.names.len() as u64);
        let exit_ok = self.run_op(op, prog);
        prog.end();
        let first = group
            .names
            .first()
            .map(|n| dest_dir.join(n))
            .filter(|p| p.as_path().exists() || dest_dir.is_remote());

        if dest_dir.is_remote() {
            return (exit_ok, first);
        }
        let missing: Vec<&String> = group
            .names
            .iter()
            .filter(|n| !dest_dir.join(n).as_path().exists())
            .collect();
        if !missing.is_empty() {
            tracing::error!(
                "batch of {} reported exit ok={} but {} destination(s) are missing, e.g. {:?}",
                group.names.len(),
                exit_ok,
                missing.len(),
                missing.iter().take(3).collect::<Vec<_>>()
            );
            // Exit code 0 with nothing at the destination: rclone reads a
            // directory in a `--files-from` list, transfers nothing and
            // reports success. There is no rclone error to distil, so the
            // job hears about it from us or not at all.
            prog.record_problem(
                format!("{} of {} items did not arrive", missing.len(), total),
                missing.first().map(|n| n.to_string()),
            );
            return (false, first);
        }
        (exit_ok, first)
    }

    /// Run a paste. `default_mode` is the configured [`ConflictMode`];
    /// `preset` is `Some` when Paste special already asked the user, in
    /// which case no further prompt appears.
    ///
    /// Conflict handling is decided once for the whole batch, not per item.
    /// For a plain paste we ask rclone (via two `--dry-run` passes) what the
    /// chosen mode would actually destroy; if the answer is "nothing", the
    /// paste runs with no dialog at all, which is the common case.
    fn run_batch(
        self,
        sources: Vec<NavPath>,
        dest_dir: NavPath,
        cut: bool,
        default_mode: ConflictMode,
        preset: Option<crate::preflight::PasteChoice>,
        plan: Arc<PastePlan>,
    ) {
        let _guard = self.state.upgrade().map(|s| s.op_guard());
        use crate::preflight::{PasteChoice, top_level_conflicts, unique_numbered_path};

        // Resolve the undo targets first, before anything is written and
        // before the conflict dialog can block on the user. Everything
        // that already exists is excluded: undo is only ever allowed to
        // remove a destination this paste brought into being. Pairs are
        // filtered together so `created[i]` / `originals[i]` stay aligned —
        // `run_revert_paste` indexes them in lockstep for cut-mode
        // restores.
        let (undo_created, undo_originals): (Vec<NavPath>, Vec<NavPath>) = sources
            .iter()
            .map(|s| (dest_dir.join(s.file_name()), s.clone()))
            .filter(|(d, _)| !d.as_path().exists())
            .unzip();
        plan.publish(undo_created, undo_originals);

        let total = sources.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        let mut renamed = 0u32;
        // First path that was actually produced in `dest_dir` — used to
        // land focus on it after the refresh so the user sees where the
        // paste ended up. Rename-on-conflict stores the fresh sibling.
        let mut first_created: Option<NavPath> = None;

        let choice = match preset {
            // Paste special already got an explicit answer; don't second-
            // guess it with another dialog.
            Some(c) => c,
            None => self.resolve_conflicts(&sources, &dest_dir, cut, default_mode),
        };
        let mode = match choice {
            PasteChoice::Cancel => {
                self.play(SoundEvent::Cancelled);
                self.say("cancelled", false);
                return;
            }
            PasteChoice::KeepBoth => None,
            PasteChoice::Mode(m) => Some(m),
        };
        // Announced only once the paste is actually going ahead — a paste
        // the user backs out of at the conflict dialog must not have
        // chimed as if it started.
        self.play(SoundEvent::PasteStart);

        // One meter for the whole paste, however many rclone invocations it
        // turns into below. Armed after the conflict decision so a paste the
        // user cancels never announces itself as starting.
        let mut prog = OpProgress::new(&self, if cut { "Moving" } else { "Copying" }, total as u64);
        prog.opening(
            if cut { "moving" } else { "copying" },
            sources.first().map(|s| s.file_name()),
        );

        // Keep-both only renames the top-level items that actually collide;
        // everything else pastes normally.
        let colliding: std::collections::HashSet<String> = if mode.is_none() {
            top_level_conflicts(&sources, &dest_dir)
                .iter()
                .map(|s| s.file_name().to_string())
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        // Collapse plain file copies into one `--files-from` invocation per
        // source folder. Skipped entirely for Keep-both (every item needs
        // its own renamed destination) and for Mirror (`sync --files-from`
        // would prune everything unlisted at the destination). Directories
        // always stay per-item: `--files-from` ignores them silently.
        let sources = match mode {
            Some(m) if m != ConflictMode::Mirror => {
                let part = crate::batch::partition(&sources, |s| self.path_is_dir(s));
                for group in &part.groups {
                    if prog.cancelled() {
                        break;
                    }
                    let n = group.names.len();
                    let (ok, first) = self.run_group(group, &dest_dir, cut, m, total, &mut prog);
                    if ok {
                        if first_created.is_none() {
                            first_created = first;
                        }
                    } else {
                        failed += n as u32;
                    }
                }
                if !part.groups.is_empty() {
                    tracing::info!(
                        "paste batched {} items into {} rclone invocation(s)",
                        total,
                        part.invocations()
                    );
                }
                part.singles
            }
            _ => sources,
        };

        for src in sources.into_iter() {
            if prog.cancelled() {
                break;
            }
            let dst_name = src.file_name().to_string();
            let dst = dest_dir.join(&dst_name);

            let (op, effective_name) = match mode {
                // Keep both: give this item a fresh numbered sibling name
                // and drive it through Rename/CopyTo so rclone writes to the
                // new path instead of merging into the existing one.
                None if colliding.contains(&dst_name) => {
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
                            // Could not construct a valid NavPath — skip so
                            // we cannot accidentally overwrite.
                            skipped += 1;
                            prog.skip(1);
                            self.say(format!("skipped {} (rename failed)", dst_name), false);
                            continue;
                        }
                    }
                }
                // Non-colliding item under Keep-both: a plain additive copy
                // is exactly right, and cannot touch anything.
                None => {
                    let op = if cut {
                        Operation::Rename { src, dst }
                    } else {
                        Operation::Copy {
                            sources: vec![src],
                            dest_dir: dest_dir.clone(),
                            mode: ConflictMode::AddNewOnly,
                        }
                    };
                    (op, dst_name)
                }
                Some(m) => {
                    let op = if cut {
                        Operation::Move {
                            sources: vec![src],
                            dest_dir: dest_dir.clone(),
                            mode: m,
                        }
                    } else {
                        Operation::Copy {
                            sources: vec![src],
                            dest_dir: dest_dir.clone(),
                            mode: m,
                        }
                    };
                    (op, dst_name)
                }
            };

            // A directory is one job unit no matter how many files rclone
            // finds inside it; its own byte fraction moves that unit.
            prog.begin(1);
            let ok = self.run_op(op, &mut prog);
            prog.end();
            if !ok {
                failed += 1;
            } else if first_created.is_none() {
                first_created = Some(dest_dir.join(&effective_name));
            }
        }
        // `refresh_with_focus` arms pending_focus so refocus_after_up can
        // land the caret on the newly pasted row by filename — matching
        // undo-delete for a consistent "where did it go" UX — and skips
        // both that and the refresh if the user has since walked away.
        let done = if cut {
            SoundEvent::MoveDone
        } else {
            SoundEvent::CopyDone
        };
        if prog.cancelled() {
            prog.finish(false);
            self.play_outcome(/*cancelled=*/ true, failed > 0, done);
            self.say("cancelled", true);
            self.refresh();
            return;
        }
        prog.finish(failed == 0);
        self.play_outcome(/*cancelled=*/ false, failed > 0, done);
        self.say(
            crate::preflight::paste_summary(mode, total, failed, skipped, renamed),
            failed > 0,
        );
        self.refresh_with_focus(first_created);
    }

    /// Reverse a paste. For copy-mode, delete each created entry. For
    /// cut-mode, move each `created[i]` back to `originals[i]`. Missing
    /// paths are skipped silently — the paste may have been partially
    /// rejected (user clicked Skip) or another process may have already
    /// cleaned things up. All results fold into a single summary.
    fn run_revert_paste(self, created: Vec<NavPath>, originals: Vec<NavPath>, cut_mode: bool) {
        let _guard = self.state.upgrade().map(|s| s.op_guard());
        let total = created.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        let mut prog = OpProgress::new(
            &self,
            if cut_mode { "Moving" } else { "Deleting" },
            total as u64,
        );
        prog.opening("undoing", created.first().map(|c| c.file_name()));
        for (i, c) in created.iter().enumerate() {
            if prog.cancelled() {
                break;
            }
            if !c.as_path().exists() {
                skipped += 1;
                prog.skip(1);
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
                    is_dir: self.path_is_dir(c),
                }
            };
            prog.begin(1);
            let ok = self.run_op(op, &mut prog);
            prog.end();
            if !ok {
                failed += 1;
            }
        }
        prog.finish(failed == 0);
        self.play_outcome(prog.cancelled(), failed > 0, SoundEvent::UndoDone);
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
        let _guard = self.state.upgrade().map(|s| s.op_guard());
        let total = pairs.len();
        let mut failed = 0u32;
        // Each rename is instant and moves no bytes, so rclone has no
        // stats to report — the meter runs purely on completed items,
        // which is what makes "120 of 200" the only sensible narration
        // here.
        let mut prog = OpProgress::new(&self, "Deleting", total as u64);
        prog.opening(
            "deleting",
            pairs.first().map(|(_, original)| original.file_name()),
        );
        for (trash, original) in pairs.into_iter() {
            if prog.cancelled() {
                break;
            }
            // `op_delete` only *named* the staging directory; create it
            // here so the syscall lands on this thread rather than in the
            // message pump. rclone's `moveto` will not create a missing
            // parent for us.
            if let Some(parent) = trash.parent()
                && let Err(e) = std::fs::create_dir_all(parent.as_path())
            {
                tracing::error!("create trash dir {:?}: {}", parent.to_string(), e);
                failed += 1;
                prog.skip(1);
                continue;
            }
            let op = Operation::Rename {
                src: original,
                dst: trash,
            };
            prog.begin(1);
            let ok = self.run_op(op, &mut prog);
            prog.end();
            if !ok {
                failed += 1;
            }
        }
        prog.finish(failed == 0);
        self.play_outcome(prog.cancelled(), failed > 0, SoundEvent::DeleteDone);
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
        let _guard = self.state.upgrade().map(|s| s.op_guard());
        let total = pairs.len();
        let mut failed = 0u32;
        let mut skipped = 0u32;
        // First successfully restored original — used to re-focus the row
        // after the refresh so the user lands back on (one of) the
        // undeleted items.
        let mut first_restored: Option<NavPath> = None;
        let mut prog = OpProgress::new(&self, "Restoring", total as u64);
        prog.opening(
            "restoring",
            pairs.first().map(|(_, original)| original.file_name()),
        );
        for (trash, original) in pairs.into_iter() {
            if prog.cancelled() {
                break;
            }
            if !trash.as_path().exists() {
                skipped += 1;
                prog.skip(1);
                continue;
            }
            if original.as_path().exists() {
                // New item at original path — don't overwrite.
                skipped += 1;
                prog.skip(1);
                continue;
            }
            prog.begin(1);
            let ok = self.run_op(
                Operation::Rename {
                    src: trash,
                    dst: original.clone(),
                },
                &mut prog,
            );
            prog.end();
            if !ok {
                failed += 1;
            } else if first_restored.is_none() {
                first_restored = Some(original);
            }
        }
        prog.finish(failed == 0);
        self.play_outcome(prog.cancelled(), failed > 0, SoundEvent::UndoDone);
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
        // Arms pending_focus so the post-listing hook lands the caret on
        // the restored row — refocus_after_up matches by filename within
        // the new listing, so this is only meaningful (and only happens)
        // when the user is still in the folder it was restored into.
        self.refresh_with_focus(first_restored);
    }

    /// Classify a path as a directory so a delete can pick the right
    /// rclone verb (`purge` for dirs, `deletefile` for files). Local
    /// paths consult the filesystem directly; remote paths fall back to
    /// an `rclone lsjson --stat`. Defaults to `false` (treat as a file)
    /// when the remote stat can't answer — a wrong guess just surfaces
    /// rclone's own "is a file/directory" error rather than mis-deleting.
    fn path_is_dir(&self, p: &NavPath) -> bool {
        if !p.is_remote() {
            return p.as_path().is_dir();
        }
        p.rclone_arg()
            .and_then(|arg| self.rclone.stat(&arg).ok().flatten())
            .map(|s| s.is_dir)
            .unwrap_or(false)
    }

    /// Run one rclone process synchronously as a whole, self-contained
    /// job. Returns `true` on success.
    ///
    /// For anything that takes more than one invocation — a paste, a batch
    /// delete — use [`run_op`](Self::run_op) with a job-level
    /// [`OpProgress`] instead, so the progress the user hears counts the
    /// action rather than restarting at each process.
    fn run_one(&self, op: Operation) -> bool {
        let mut prog = OpProgress::new(self, op_verb(&op), 1);
        prog.begin(1);
        let ok = self.run_op(op, &mut prog);
        prog.end();
        prog.finish(ok);
        ok
    }

    /// Run one rclone process synchronously, reporting into an existing
    /// job. The caller owns `prog`: it must have called
    /// [`OpProgress::begin`] with this invocation's weight beforehand and
    /// [`OpProgress::end`] afterwards, and it — not this function — posts
    /// the job's completion.
    ///
    /// Errors are always surfaced via a modal dialog, regardless of the
    /// progress-window preference.
    fn run_op(&self, op: Operation, prog: &mut OpProgress) -> bool {
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
        // Point the window's Cancel button at this child. Re-armed every
        // invocation; without it the button was decorative — nothing ever
        // installed a callback.
        prog.arm_cancel(handle.canceller());

        for ev in handle.events.iter() {
            match ev {
                navigator_rclone::op::OpEvent::Progress(p) => prog.on_progress(p),
                navigator_rclone::op::OpEvent::Log(ev) => {
                    if let Some(line) = prog.on_log(&ev) {
                        prog.log_line(&line);
                    }
                }
                navigator_rclone::op::OpEvent::Done {
                    success,
                    exit_code,
                    error: rclone_error,
                } => {
                    if success {
                        prune_empty_src_dirs(&op_for_retry);
                        return true;
                    }
                    // A cancelled child exits non-zero. That is the user's
                    // own doing, so it gets neither an error dialog nor a
                    // UAC retry.
                    if prog.cancelled() {
                        return false;
                    }
                    // Failed. If this was a Windows ACL denial (writes to
                    // C:\, Program Files, etc.), retry under UAC. The UAC
                    // prompt itself is the user confirmation — no extra
                    // dialog. Don't loop more than once: if the elevated
                    // retry also fails, the problem isn't permission.
                    let subject = op_subject(&op_for_retry);

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
                                return true;
                            }
                            Ok(out) => {
                                // The elevated child couldn't be piped, so
                                // it logged to a file — but it is the same
                                // rclone log, and it goes through the same
                                // distiller so an elevated failure reads
                                // like any other.
                                let elevated = navigator_rclone::RcloneError::from_log_text(
                                    &out.log_tail,
                                    out.exit_code,
                                );
                                let err = if elevated.message.is_empty() {
                                    rclone_error.unwrap_or(elevated)
                                } else {
                                    elevated
                                };
                                prog.record_elevated_failure(&err, subject);
                                return false;
                            }
                            Err(e) => {
                                // ShellExecuteEx itself failed (UAC
                                // declined, exe missing). Fall through to
                                // the ordinary failure report with the
                                // unelevated error.
                                error!("elevated retry: {e}");
                            }
                        }
                    }

                    match rclone_error {
                        Some(e) => prog.record_failure(&e, subject),
                        // `Done` only omits the error when the op
                        // succeeded, so this is unreachable in practice —
                        // but a failure that reports nothing at all is the
                        // one outcome the user can't act on.
                        None => prog.record_problem(
                            match exit_code {
                                Some(c) => format!("rclone exited with code {c}"),
                                None => "rclone failed".into(),
                            },
                            subject,
                        ),
                    }
                    return false;
                }
            }
        }
        false
    }
}

/// Present-tense verb for the progress window's caption. Used for one-off
/// operations, where the op itself is the only clue about what the user
/// asked for; batch jobs name their own verb up front.
fn op_verb(op: &Operation) -> &'static str {
    match op {
        Operation::Copy { .. } | Operation::CopyBatch { .. } | Operation::CopyTo { .. } => {
            "Copying"
        }
        Operation::Move { .. } | Operation::MoveBatch { .. } => "Moving",
        // A rename is how we stage a trash-delete and how we undo one, so
        // "Moving" is the honest label for all of them.
        Operation::Rename { .. } => "Moving",
        Operation::Delete { .. } => "Deleting",
        Operation::Mkdir { .. } | Operation::Touch { .. } => "Creating",
    }
}

/// The item a failed operation should name, in the user's terms.
///
/// rclone names things too, but badly for this purpose: a `\\?\`-prefixed
/// absolute path, or a Go filesystem description ("Local file system at
/// //?/C:/…"). The op knows the filename the user actually selected.
///
/// A batch has no single subject — its whole point is that many files
/// share one invocation — so it reports the destination folder instead of
/// picking one of its members arbitrarily.
fn op_subject(op: &Operation) -> Option<String> {
    let name = |p: &NavPath| Some(p.file_name().to_string()).filter(|s| !s.is_empty());
    match op {
        Operation::Copy { sources, .. } | Operation::Move { sources, .. } => {
            sources.first().and_then(name)
        }
        Operation::CopyBatch { dest_dir, .. } | Operation::MoveBatch { dest_dir, .. } => {
            name(dest_dir)
        }
        Operation::Rename { src, .. } => name(src),
        Operation::CopyTo { src, .. } => name(src),
        Operation::Delete { targets, .. } => targets.first().and_then(name),
        Operation::Mkdir { dir } => name(dir),
        Operation::Touch { file } => name(file),
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
///
/// **One stat, not a directory scan.** This used to `read_dir` the parent
/// and search it for `name`, which made the watcher cost O(entries in the
/// folder) *per changed file* — on the UI thread. Extracting or pasting N
/// files into a folder of M entries was N×M work in the message pump, and
/// a few thousand of each froze the window for the whole operation.
fn single_entry(root: &NavPath, name: &str) -> Option<navigator_core::Entry> {
    navigator_fs::stat_entry(root.join(name).as_path())
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
pub(crate) fn set_clipboard_text(
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
/// plus a `Preferred DropEffect` hint. Pasting the result in Explorer /
/// open-file dialogs / other apps reproduces the files via the shell's
/// normal copy machinery.
///
/// Blob layout for the DROPFILES handle:
///   * `DROPFILES` header (20 bytes, packed) — `pFiles = 20`, `fWide = 1`
///   * UTF-16 paths concatenated, each NUL-terminated
///   * one extra `0u16` so the list ends in a double-NUL
///
/// The "Preferred DropEffect" registered format carries a single
/// `DWORD` — `drop_effect` — telling the receiver whether to copy
/// (`DROPEFFECT_COPY = 1`) or move (`DROPEFFECT_MOVE = 2`) on paste
/// rather than guessing from same-vs-cross-volume rules.
fn set_clipboard_hdrop(
    hwnd: Option<windows::Win32::Foundation::HWND>,
    paths: &[std::path::PathBuf],
    drop_effect: u32,
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
                        *dst = drop_effect; // DROPEFFECT_COPY (1) / MOVE (2)
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

/// Read a `CF_HDROP` file list off the Windows clipboard along with the
/// `Preferred DropEffect` flag. Returns `(paths, is_move)` — `is_move`
/// is `true` only when the source tagged the clip as a pure MOVE
/// (`DROPEFFECT_MOVE` set, `DROPEFFECT_COPY` clear), matching how
/// Explorer distinguishes a Cut from a Copy. Anything else defaults to
/// copy, which never loses data.
///
/// When no `CF_HDROP` is present (e.g. only text is on the clipboard)
/// the returned path list is empty — the caller announces that. All
/// reads happen while the clipboard is open; the `HDROP` handle is
/// system-owned and must not outlive `CloseClipboard`, so paths are
/// copied out eagerly.
fn get_clipboard_hdrop(
    hwnd: Option<windows::Win32::Foundation::HWND>,
) -> std::io::Result<(Vec<std::path::PathBuf>, bool)> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    use windows::core::PCWSTR;

    unsafe {
        // Cheap pre-check that doesn't require taking the global lock.
        if IsClipboardFormatAvailable(u32::from(CF_HDROP.0)).is_err() {
            return Ok((Vec::new(), false));
        }
        OpenClipboard(hwnd).map_err(io_err)?;
        let result = (|| -> std::io::Result<(Vec<std::path::PathBuf>, bool)> {
            let handle = GetClipboardData(u32::from(CF_HDROP.0)).map_err(io_err)?;
            let hdrop = HDROP(handle.0);

            // 0xFFFF_FFFF asks DragQueryFile for the file count.
            let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                // With no buffer, DragQueryFile returns the length in
                // wide chars (excluding the NUL) — path lengths can
                // exceed MAX_PATH, so size the buffer per entry.
                let len = DragQueryFileW(hdrop, i, None);
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len as usize + 1];
                let copied = DragQueryFileW(hdrop, i, Some(&mut buf));
                if copied == 0 {
                    continue;
                }
                let s = std::ffi::OsString::from_wide(&buf[..copied as usize]);
                paths.push(std::path::PathBuf::from(s));
            }

            // Preferred DropEffect — a single DWORD in a registered
            // format. Absent format ⇒ copy (the conventional default).
            let mut is_move = false;
            let fmt_name: Vec<u16> = "Preferred DropEffect"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let fmt = RegisterClipboardFormatW(PCWSTR(fmt_name.as_ptr()));
            if fmt != 0
                && IsClipboardFormatAvailable(fmt).is_ok()
                && let Ok(h) = GetClipboardData(fmt)
            {
                let p = GlobalLock(HGLOBAL(h.0)) as *const u32;
                if !p.is_null() {
                    let effect = *p;
                    // DROPEFFECT_MOVE = 2, DROPEFFECT_COPY = 1. Treat as a
                    // move only when MOVE is set and COPY is not — any copy
                    // hint (or ambiguous combo) falls back to copy.
                    is_move = (effect & 2) != 0 && (effect & 1) == 0;
                    let _ = GlobalUnlock(HGLOBAL(h.0));
                }
            }

            Ok((paths, is_move))
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
            ScanCmd::List(path, hwnd, sort) => {
                // ThisPC sentinel → enumerate drives rather than reading a
                // real directory. Same flow on the UI side: entries end up
                // in the virtual listview just like regular files.
                let mut entries = if path.is_this_pc() {
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
                // Sort here, not in the WMAPP_DIR_LISTED handler. The
                // handler runs in the message pump, so a big folder's
                // n log n was time the window spent unresponsive right
                // when the user had just asked to go somewhere. The tag
                // travels with the payload so the model can tell whether
                // this order still matches the current preference.
                crate::model::sort_entries(&mut entries, sort);
                let payload = Box::into_raw(Box::new((path, entries, sort))) as isize;
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
    // Backstop for the temp files an operation could not delete itself —
    // a killed process, an abort, a `process::exit` that skipped a
    // destructor. Runs on its own thread and nothing waits for it.
    crate::tempsweep::spawn();
    let state = AppState::new(&cfg);
    state.bootstrap_plugins();
    let window = create_window(state.clone())?;
    let rc = run_message_loop(window.hwnd);
    // Operation history is written on its own thread so a big copy doesn't
    // stall Ctrl+C; `main` exits the process immediately after this
    // returns, so drain the queue before that kills the writer.
    crate::clipboard::flush_history(std::time::Duration::from_secs(2));
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
            is_dir: true,
        };
        prune_empty_src_dirs(&op);
        assert!(dir.exists(), "Delete op must not trigger src cleanup");
    }
}
