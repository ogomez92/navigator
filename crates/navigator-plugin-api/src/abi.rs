//! The C ABI types shared between host and plugin.

use std::ffi::c_void;

/// Bump this *every* time a field or signature in this module changes.
/// The host refuses to load plugins whose `abi_version` does not match.
pub const ABI_VERSION: u32 = 1;

/// Borrowed UTF-8 string passed across the ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Str {
    pub ptr: *const u8,
    pub len: usize,
}

impl Str {
    pub fn from(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// # Safety
    /// `ptr..ptr+len` must be a valid UTF-8 byte range, valid for reads for
    /// the lifetime of the returned reference.
    pub unsafe fn as_str<'a>(&self) -> &'a str {
        if self.ptr.is_null() || self.len == 0 {
            return "";
        }
        let slice = unsafe { std::slice::from_raw_parts(self.ptr, self.len) };
        std::str::from_utf8(slice).unwrap_or("")
    }
}

/// Metadata plugins hand back to the host for display and diagnostics.
#[repr(C)]
pub struct PluginInfo {
    pub abi_version: u32,
    pub name: Str,
    pub version: Str,
    pub description: Str,
}

/// Host-side vtable handed to the plugin. Callers invoke these to ask the
/// host to speak text, log, or navigate.
#[repr(C)]
pub struct HostApi {
    pub abi_version: u32,

    /// Host-owned opaque context passed back as the first arg of every
    /// function pointer below.
    pub host: *mut c_void,

    pub speak: unsafe extern "C" fn(host: *mut c_void, text: Str, interrupt: bool),
    pub log: unsafe extern "C" fn(host: *mut c_void, level: u8, msg: Str),
    pub navigate: unsafe extern "C" fn(host: *mut c_void, path: Str),
}

/// Plugin-side vtable. Filled in by the plugin during `navigator_plugin_entry`.
#[repr(C)]
pub struct Plugin {
    pub abi_version: u32,

    /// Plugin-owned opaque context passed back as the first arg of every
    /// function pointer below.
    pub plugin: *mut c_void,

    pub info: unsafe extern "C" fn(plugin: *mut c_void) -> PluginInfo,

    /// Optional hook called when the user navigates into a directory.
    /// Pass `None` by setting to a no-op fn; the host won't null-check.
    pub on_navigated: unsafe extern "C" fn(plugin: *mut c_void, path: Str),

    /// Optional hook called when the focus changes in the listing.
    pub on_focused: unsafe extern "C" fn(plugin: *mut c_void, path: Str, name: Str, index: usize),

    /// Called once as the plugin is being unloaded. Plugin must drop its
    /// context here; host then closes the DLL.
    pub shutdown: unsafe extern "C" fn(plugin: *mut c_void),
}

/// Signature of the symbol every plugin exports.
///
/// On success, the plugin initialises `out` with its vtable and returns `0`.
/// Any non-zero return signals failure and the host unloads the DLL.
pub type PluginEntry = unsafe extern "C" fn(api: *const HostApi, out: *mut Plugin) -> i32;
