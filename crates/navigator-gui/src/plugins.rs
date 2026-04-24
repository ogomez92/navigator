//! Plugin autoload + event dispatch.
//!
//! Plugins live next to the executable under `plugins/*.dll`. We build a
//! `HostApi` that forwards `speak`/`log`/`navigate` back into [`AppState`]
//! through the [`navigator_plugin_api::host::HostCallbacks`] trait.
//!
//! This module is Send/Sync: the set of loaded plugins is wrapped in a
//! `Mutex` so the UI thread can call `dispatch_navigated` / `dispatch_focused`
//! while a background thread loads more.

use std::ffi::c_void;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{info, warn};

use navigator_plugin_api::abi::HostApi;
use navigator_plugin_api::host::{HostCallbacks, LogLevel};
use navigator_plugin_api::{LoadedPlugin, PluginLoader};

/// Host-side callbacks exposed to plugins.
pub struct Host {
    speak_tx: crossbeam_channel::Sender<crate::speech::Utterance>,
    nav_tx: crossbeam_channel::Sender<navigator_core::NavPath>,
}

impl Host {
    pub fn new(
        speak_tx: crossbeam_channel::Sender<crate::speech::Utterance>,
        nav_tx: crossbeam_channel::Sender<navigator_core::NavPath>,
    ) -> Self {
        Self { speak_tx, nav_tx }
    }
}

impl HostCallbacks for Host {
    fn speak(&self, text: &str, interrupt: bool) {
        let _ = self.speak_tx.try_send(crate::speech::Utterance {
            text: text.to_string(), interrupt,
        });
    }

    fn log(&self, level: LogLevel, msg: &str) {
        match level {
            LogLevel::Error => tracing::error!(target: "plugin", "{msg}"),
            LogLevel::Warn  => tracing::warn! (target: "plugin", "{msg}"),
            LogLevel::Info  => tracing::info! (target: "plugin", "{msg}"),
            LogLevel::Debug => tracing::debug!(target: "plugin", "{msg}"),
            LogLevel::Trace => tracing::trace!(target: "plugin", "{msg}"),
        }
    }

    fn navigate(&self, path: &str) {
        if let Ok(np) = navigator_core::NavPath::new(std::path::PathBuf::from(path)) {
            let _ = self.nav_tx.try_send(np);
        }
    }
}

pub struct PluginRegistry {
    plugins: Mutex<Vec<LoadedPlugin>>,
    // Kept alive for the `host` pointer inside HostApi.
    _host_box: Box<Arc<dyn HostCallbacks>>,
    host_api: HostApi,
}

impl PluginRegistry {
    pub fn new(host: Arc<dyn HostCallbacks>) -> Self {
        // Leak the Arc pointer through a stable address. The thunks in
        // `make_host_api` downcast it back to `&Arc<dyn HostCallbacks>`.
        let host_box: Box<Arc<dyn HostCallbacks>> = Box::new(host);
        let host_ptr = (&*host_box as *const Arc<dyn HostCallbacks>) as *mut c_void;
        let host_api = navigator_plugin_api::host::make_host_api(host_ptr);
        Self {
            plugins: Mutex::new(Vec::new()),
            _host_box: host_box,
            host_api,
        }
    }

    /// Load every `.dll` directly under `dir`. Errors are logged but do
    /// not fail the call — a broken plugin should not prevent the rest
    /// from loading.
    pub fn load_from_dir(&self, dir: &std::path::Path) {
        if !dir.is_dir() { return; }
        let loader = PluginLoader::new(self.host_api_clone());
        for result in loader.load_directory(dir) {
            match result {
                Ok(p) => {
                    let info = p.info();
                    let name = unsafe { info.name.as_str() };
                    let version = unsafe { info.version.as_str() };
                    info!("loaded plugin: {} v{} from {}", name, version, p.path.display());
                    self.plugins.lock().push(p);
                }
                Err(e) => warn!("plugin load failed: {e}"),
            }
        }
    }

    fn host_api_clone(&self) -> HostApi {
        // `HostApi` is trivially copy-by-value — function pointers + a raw
        // `host` pointer. Cloning it means recreating the same aggregate.
        HostApi {
            abi_version: self.host_api.abi_version,
            host: self.host_api.host,
            speak: self.host_api.speak,
            log: self.host_api.log,
            navigate: self.host_api.navigate,
        }
    }

    pub fn dispatch_navigated(&self, path: &str) {
        for p in self.plugins.lock().iter() { p.on_navigated(path); }
    }

    pub fn dispatch_focused(&self, path: &str, name: &str, idx: usize) {
        for p in self.plugins.lock().iter() { p.on_focused(path, name, idx); }
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.lock().iter().map(|p| {
            let info = p.info();
            unsafe { info.name.as_str() }.to_string()
        }).collect()
    }
}

// HostApi contains raw pointers into our own `host_box` — safe across
// threads because we never free it while plugins are loaded.
unsafe impl Send for PluginRegistry {}
unsafe impl Sync for PluginRegistry {}
