//! Hand-written bindings for `prism.h`. Only covers the subset the app uses.
//!
//! `prism.h` reviewed at v0.18.3 (the vendored copy). If the upstream ABI
//! changes, regenerate via bindgen — keeping this hand-written saves a
//! proc-macro / clang dependency for a 20-function surface.
//!
//! **`PrismConfig` must match the DLL byte for byte.** `prism_config_init`
//! returns it *by value*: on x64 a struct of 1, 2, 4 or 8 bytes comes back
//! in RAX, anything bigger is written through a hidden pointer the caller
//! passes in RCX. The 0.11 binding was the one-byte `{ version }`, so a
//! 0.18 `prism.dll` (56 bytes) wrote its config through whatever RCX held
//! — silent stack corruption that, measured, wiped the app's start path
//! and then failed `prism_init` on the garbage it read back. There is no
//! error path for this; the only defence is keeping the struct current.

#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::{c_char, c_int, c_void};

pub type PrismError = c_int;

pub const PRISM_OK: PrismError = 0;

#[repr(C)]
pub struct PrismContext {
    _private: [u8; 0],
}
#[repr(C)]
pub struct PrismBackend {
    _private: [u8; 0],
}

pub type PrismBackendId = u64;

#[repr(C)]
pub struct PrismRegistry {
    _private: [u8; 0],
}

pub type PrismAvailabilityCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        backend: PrismBackendId,
        name: *const c_char,
        available: bool,
    ),
>;

pub type PrismAvailabilityBaselineCallback = Option<unsafe extern "C" fn(userdata: *mut c_void)>;

/// Mirrors `PrismConfig` in `prism.h` 0.18.3 — 56 bytes on x64. Only
/// `prism_config_init` fills it; the app never sets a field.
#[repr(C)]
pub struct PrismConfig {
    pub version: u8,
    pub registry: *mut PrismRegistry,
    pub availability_callback: PrismAvailabilityCallback,
    pub availability_userdata: *mut c_void,
    pub availability_poll_interval_ms: u32,
    pub availability_debounce_samples: u32,
    pub availability_backoff_max_ms: u32,
    pub availability_auto_power_manage: bool,
    pub availability_baseline_callback: PrismAvailabilityBaselineCallback,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size the 0.18.3 header's layout comes to on x64, pinned so a
    /// field added upstream can't be missed here — see the module doc for
    /// what a mismatch does.
    #[test]
    fn prism_config_matches_the_0_18_layout() {
        assert_eq!(std::mem::size_of::<PrismConfig>(), 56);
        assert_eq!(std::mem::offset_of!(PrismConfig, registry), 8);
        assert_eq!(
            std::mem::offset_of!(PrismConfig, availability_poll_interval_ms),
            32
        );
        assert_eq!(
            std::mem::offset_of!(PrismConfig, availability_baseline_callback),
            48
        );
    }
}

#[link(name = "prism")]
unsafe extern "C" {
    pub fn prism_config_init() -> PrismConfig;
    pub fn prism_init(cfg: *mut PrismConfig) -> *mut PrismContext;
    pub fn prism_shutdown(ctx: *mut PrismContext);

    pub fn prism_registry_acquire_best(ctx: *mut PrismContext) -> *mut PrismBackend;
    pub fn prism_registry_create_best(ctx: *mut PrismContext) -> *mut PrismBackend;

    pub fn prism_backend_free(backend: *mut PrismBackend);
    pub fn prism_backend_name(backend: *mut PrismBackend) -> *const c_char;
    pub fn prism_backend_speak(
        backend: *mut PrismBackend,
        text: *const c_char,
        interrupt: bool,
    ) -> PrismError;
    pub fn prism_backend_stop(backend: *mut PrismBackend) -> PrismError;

    pub fn prism_error_string(err: PrismError) -> *const c_char;
}
