//! Example plugin: announces every directory the user navigates into.
//!
//! Build with `cargo build -p navigator-sample-plugin` — the resulting
//! `navigator_sample_plugin.dll` can be dropped into the plugin directory.

use std::ffi::c_void;
use std::sync::Mutex;

use navigator_plugin_api::abi::{ABI_VERSION, HostApi, Plugin, PluginInfo, Str};

/// Plugin state. Holds a pointer to the host API so callbacks can speak back.
struct State {
    host: *const HostApi,
    nav_count: Mutex<u64>,
}

// Host pointer doesn't leave the UI thread in practice, but the state is
// shared via the vtable — mark it thread-safe explicitly.
unsafe impl Send for State {}
unsafe impl Sync for State {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_plugin_entry(api: *const HostApi, out: *mut Plugin) -> i32 {
    if api.is_null() || out.is_null() {
        return 1;
    }
    let api_ref = unsafe { &*api };
    if api_ref.abi_version != ABI_VERSION {
        return 2;
    }

    let state = Box::new(State {
        host: api,
        nav_count: Mutex::new(0),
    });
    let state_ptr = Box::into_raw(state) as *mut c_void;

    unsafe {
        *out = Plugin {
            abi_version: ABI_VERSION,
            plugin: state_ptr,
            info,
            on_navigated,
            on_focused,
            shutdown,
        };
    }
    0
}

unsafe extern "C" fn info(_plugin: *mut c_void) -> PluginInfo {
    PluginInfo {
        abi_version: ABI_VERSION,
        name: Str::from("sample"),
        version: Str::from(env!("CARGO_PKG_VERSION")),
        description: Str::from("Announces navigations; demonstrates the plugin ABI"),
    }
}

unsafe extern "C" fn on_navigated(plugin: *mut c_void, path: Str) {
    if plugin.is_null() {
        return;
    }
    let state = unsafe { &*(plugin as *const State) };
    let mut count = state.nav_count.lock().unwrap();
    *count += 1;

    // Call back into the host to speak.
    if !state.host.is_null() {
        let host = unsafe { &*state.host };
        let path_str = unsafe { path.as_str() };
        let msg = format!("plugin: visited {} (nav #{})", path_str, *count);
        unsafe {
            (host.speak)(host.host, Str::from(&msg), false);
        }
    }
}

unsafe extern "C" fn on_focused(_plugin: *mut c_void, _path: Str, _name: Str, _idx: usize) {
    // Intentionally silent; on_navigated is enough to demonstrate the wiring.
}

unsafe extern "C" fn shutdown(plugin: *mut c_void) {
    if plugin.is_null() {
        return;
    }
    // Reclaim the box; drops State and any state inside.
    let _ = unsafe { Box::from_raw(plugin as *mut State) };
}
