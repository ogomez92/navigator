//! Host-side plugin discovery + dynamic loading.

use std::fs;
use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};

use crate::abi::{ABI_VERSION, HostApi, Plugin, PluginEntry};

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("loading {0}: {1}")]
    Load(PathBuf, #[source] libloading::Error),
    #[error("{0} missing entry symbol `navigator_plugin_entry`: {1}")]
    MissingEntry(PathBuf, #[source] libloading::Error),
    #[error("{path} entry returned non-zero code {code}")]
    EntryFailed { path: PathBuf, code: i32 },
    #[error("{path} ABI mismatch: plugin={plugin}, host={host}")]
    AbiMismatch {
        path: PathBuf,
        plugin: u32,
        host: u32,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub struct LoadedPlugin {
    pub path: PathBuf,
    pub vtable: Plugin,
    // Keep the library alive for as long as vtable pointers might be called.
    _lib: Library,
}

impl LoadedPlugin {
    pub fn info(&self) -> crate::abi::PluginInfo {
        unsafe { (self.vtable.info)(self.vtable.plugin) }
    }

    pub fn on_navigated(&self, path: &str) {
        unsafe { (self.vtable.on_navigated)(self.vtable.plugin, crate::abi::Str::from(path)) }
    }

    pub fn on_focused(&self, path: &str, name: &str, index: usize) {
        unsafe {
            (self.vtable.on_focused)(
                self.vtable.plugin,
                crate::abi::Str::from(path),
                crate::abi::Str::from(name),
                index,
            )
        }
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        unsafe { (self.vtable.shutdown)(self.vtable.plugin) }
    }
}

pub struct PluginLoader {
    pub host_api: HostApi,
}

impl PluginLoader {
    pub fn new(host_api: HostApi) -> Self {
        Self { host_api }
    }

    /// Load every `.dll` found directly under `dir`.
    pub fn load_directory(&self, dir: &Path) -> Vec<Result<LoadedPlugin, PluginError>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("dll") {
                out.push(self.load_one(&path));
            }
        }
        out
    }

    pub fn load_one(&self, path: &Path) -> Result<LoadedPlugin, PluginError> {
        // Safety: loading arbitrary DLLs is inherently unsafe (they run
        // constructor code). Callers must only point at trusted plugin dirs.
        let lib =
            unsafe { Library::new(path) }.map_err(|e| PluginError::Load(path.to_path_buf(), e))?;

        let entry: Symbol<PluginEntry> = unsafe { lib.get(crate::ENTRY_SYMBOL) }
            .map_err(|e| PluginError::MissingEntry(path.to_path_buf(), e))?;

        // `Plugin` contains function pointers, which mustn't be zeroed.
        // Use MaybeUninit; the plugin's entry function is responsible for
        // writing every field before returning 0.
        let mut vtable_uninit: std::mem::MaybeUninit<Plugin> = std::mem::MaybeUninit::uninit();
        let rc = unsafe { entry(&self.host_api, vtable_uninit.as_mut_ptr()) };
        if rc != 0 {
            return Err(PluginError::EntryFailed {
                path: path.to_path_buf(),
                code: rc,
            });
        }
        let vtable = unsafe { vtable_uninit.assume_init() };
        if vtable.abi_version != ABI_VERSION {
            return Err(PluginError::AbiMismatch {
                path: path.to_path_buf(),
                plugin: vtable.abi_version,
                host: ABI_VERSION,
            });
        }
        // `entry`'s borrow of `lib` ends here (NLL) — `vtable` is an owned
        // copy — so `lib` can move into the result below.
        Ok(LoadedPlugin {
            path: path.to_path_buf(),
            vtable,
            _lib: lib,
        })
    }
}
