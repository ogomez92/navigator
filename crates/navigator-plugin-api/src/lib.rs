//! Plugin ABI for navigator.
//!
//! Plugins are `cdylib` crates that expose a single `extern "C"` symbol,
//! [`navigator_plugin_entry`]. The host loads the library with `libloading`
//! and calls that function, passing a pointer to a host-owned [`HostApi`].
//! The plugin fills in a [`Plugin`] vtable and returns a handle.
//!
//! Everything crossing the boundary uses:
//!   * plain `#[repr(C)]` structs of function pointers,
//!   * UTF-8 strings as `*const u8 + len` pairs (no `String` or `&str`),
//!   * extern "C" calling convention.
//!
//! That keeps the ABI stable across rustc versions and avoids dragging
//! `std` allocator identity into plugin code.

pub mod abi;
pub mod host;
pub mod loader;

pub use abi::{ABI_VERSION, HostApi, Plugin, PluginInfo, Str};
pub use loader::{LoadedPlugin, PluginLoader};

/// The expected entry point name plugins must export.
pub const ENTRY_SYMBOL: &[u8] = b"navigator_plugin_entry\0";
