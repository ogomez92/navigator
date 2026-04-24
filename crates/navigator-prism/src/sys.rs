//! Hand-written bindings for `prism.h`. Only covers the subset the app uses.
//!
//! `prism.h` reviewed at v0.11.6. If the upstream ABI changes, regenerate via
//! bindgen — keeping this hand-written saves a proc-macro / clang dependency
//! for a 20-function surface.

#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::{c_char, c_int};

pub type PrismError = c_int;

pub const PRISM_OK: PrismError = 0;

#[repr(C)]
pub struct PrismContext { _private: [u8; 0] }
#[repr(C)]
pub struct PrismBackend { _private: [u8; 0] }

pub type PrismBackendId = u64;

#[repr(C)]
pub struct PrismConfig {
    pub version: u8,
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
