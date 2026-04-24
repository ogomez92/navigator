//! Portable configuration for navigator.
//!
//! Stored next to the executable: `<exe_dir>/config.toml`. Plugins sit in
//! `<exe_dir>/plugins/*.dll`. Nothing touches `%APPDATA%`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub mod shortcuts;

pub use shortcuts::{HOTSPOT_COUNT, InternalCommand, ShortcutAction, ShortcutChord};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },
    #[error("parse {path}: {source}")]
    Parse { path: PathBuf, #[source] source: toml::de::Error },
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("cannot resolve exe directory: {0}")]
    ExeDir(std::io::Error),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Root config struct. Fields default so partial files still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub plugins: Plugins,
    pub rclone: Rclone,
    pub recent_paths: Vec<String>,
    #[serde(default = "shortcuts::default_actions")]
    pub shortcuts: Vec<ShortcutAction>,
    /// Saved hotspot targets, one per slot (1..=HOTSPOT_COUNT). An empty
    /// string means the slot is empty; next press will record the current
    /// single selection into it. Strings are absolute paths to the entry
    /// itself — the parent folder is derived on invocation so the listview
    /// can focus the entry by filename. (Stored as `Vec<String>` rather
    /// than `Vec<Option<String>>` because TOML arrays can't hold `None`.)
    #[serde(default = "default_hotspots")]
    pub hotspots: Vec<String>,
}

fn default_hotspots() -> Vec<String> {
    vec![String::new(); HOTSPOT_COUNT as usize]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            plugins: Plugins::default(),
            rclone: Rclone::default(),
            recent_paths: Vec::new(),
            shortcuts: shortcuts::default_actions(),
            hotspots: default_hotspots(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub show_hidden: bool,
    pub show_system: bool,
    /// Seconds between periodic progress announcements through prism.
    /// `0` = only announce on completion.
    pub announce_interval_secs: u32,
    /// Display times as "5 minutes ago" instead of "2026-04-22 14:07".
    pub show_relative_dates: bool,
    /// When a file appears while a directory is being shown, append it at
    /// the bottom instead of re-sorting into its sorted position — this is
    /// the default Explorer behaviour and avoids visual reflow under the
    /// user's cursor.
    pub new_items_at_bottom: bool,
    /// Sort key for directory listings. Persisted across runs.
    pub sort_mode: SortMode,
    /// If true, sort in descending order.
    pub sort_descending: bool,
    /// Per-column visibility. The Name column is always shown; the rest
    /// can be toggled from Options → Columns. Sort mode is independent —
    /// e.g. you can sort by type without the Type column visible.
    pub columns: Columns,
}

impl Default for General {
    fn default() -> Self {
        Self {
            show_hidden: false,
            show_system: false,
            announce_interval_secs: 0,
            show_relative_dates: false,
            new_items_at_bottom: true,
            sort_mode: SortMode::Name,
            sort_descending: false,
            columns: Columns::default(),
        }
    }
}

/// rclone-specific knobs. Kept in their own section because they drive
/// the external `rclone` binary's flags rather than GUI behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Rclone {
    /// `true` = show the progress window while a copy/move/delete runs.
    /// Errors still surface as a dialog regardless. Moved here from
    /// `general` because its audience is the rclone pipeline.
    pub progress_window: bool,
    /// Maps to rclone's `--transfers N`. Caps concurrent file transfers
    /// within one op; higher values speed up many-small-files copies over
    /// fast links but can saturate slow disks or network shares. rclone's
    /// native default is 4; ours is 8 based on typical SSD throughput.
    /// Clamped to `1..=64` at load time so a junk value cannot brick ops.
    pub transfers: u32,
}

impl Default for Rclone {
    fn default() -> Self {
        Self {
            progress_window: false,
            transfers: 8,
        }
    }
}

impl Rclone {
    /// Clamped transfers value suitable for handing straight to the
    /// driver. Zero / absurd values get normalised to the default so a
    /// hand-edited config can't wedge the pipeline.
    pub fn transfers_clamped(&self) -> u32 {
        self.transfers.clamp(1, 64)
    }
}

/// Which ListView columns to display. Name is always shown. Each flag
/// defaults to `true` so pre-existing configs (which lack this section)
/// keep the historical four-column view unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Columns {
    pub show_size: bool,
    pub show_type: bool,
    pub show_modified: bool,
}

impl Default for Columns {
    fn default() -> Self {
        Self { show_size: true, show_type: true, show_modified: true }
    }
}

/// How directory entries are ordered. Folder-first tiebreak is always
/// applied before the chosen key, matching Explorer's behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortMode {
    Name,
    Size,
    Type,
    Modified,
    Created,
}

impl Default for SortMode {
    fn default() -> Self { SortMode::Name }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugins {
    pub entries: Vec<PluginEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Filename only, e.g. `sample.dll`. Resolved relative to `<exe_dir>/plugins`.
    pub file: String,
    pub enabled: bool,
}

/// Thread-safe, clone-able config handle. Snapshot reads are cheap; writes
/// mutate in place and persist on `save()`.
#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<Config>>,
    path: PathBuf,
}

impl ConfigHandle {
    pub fn load_or_default() -> Self {
        let path = config_path_or_default();
        let mut created = false;
        let inner = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!("config parse error at {}: {}; using defaults", path.display(), e);
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("no config at {}; writing defaults", path.display());
                created = true;
                Config::default()
            }
            Err(e) => {
                warn!("config read error at {}: {}; using defaults", path.display(), e);
                Config::default()
            }
        };
        let handle = Self { inner: Arc::new(RwLock::new(inner)), path };
        // First run: write a default config.toml next to the exe so the
        // user can discover the file, hand-edit it, or just confirm where
        // settings live. Failure here is logged but non-fatal — the app
        // still runs against the in-memory defaults.
        if created {
            if let Err(e) = handle.save() {
                warn!("failed to write default config: {}", e);
            }
        }
        handle
    }

    pub fn path(&self) -> &Path { &self.path }

    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, Config> { self.inner.read() }

    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Config) -> R) -> R {
        let mut g = self.inner.write();
        f(&mut g)
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&*self.inner.read())?;
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&self.path, text)
            .map_err(|e| ConfigError::Io { path: self.path.clone(), source: e })
    }

    pub fn push_recent(&self, path: &str, max: usize) {
        self.with_mut(|cfg| {
            cfg.recent_paths.retain(|p| p != path);
            cfg.recent_paths.insert(0, path.to_string());
            if cfg.recent_paths.len() > max { cfg.recent_paths.truncate(max); }
        });
    }
}

/// Directory containing the running executable.
pub fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(ConfigError::ExeDir)?;
    Ok(exe.parent().unwrap_or(Path::new(".")).to_path_buf())
}

/// Config file path. Falls back to current working directory if exe path
/// resolution fails — the caller still gets a usable handle.
pub fn config_path_or_default() -> PathBuf {
    exe_dir().unwrap_or_else(|_| PathBuf::from(".")).join("config.toml")
}

/// Directory plugins are loaded from.
pub fn plugin_dir() -> PathBuf {
    exe_dir().unwrap_or_else(|_| PathBuf::from(".")).join("plugins")
}

