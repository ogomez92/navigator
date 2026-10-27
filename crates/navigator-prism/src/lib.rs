//! Safe wrapper around the prism C library.
//!
//! `prism` exposes a registry of backends (NVDA, JAWS, SAPI, OneCore, UIA, …).
//! The typical usage from an app is "pick the best one, speak strings at it",
//! which is exactly what [`Speaker::best`] does.

#![cfg(windows)]

pub mod sys;

use std::ffi::{CString, NulError};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use sys as ffi;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, thiserror::Error)]
pub enum PrismError {
    #[error("prism already initialized in this process")]
    AlreadyInitialized,
    #[error("prism init returned null")]
    InitFailed,
    /// The `prism.dll` that loaded is not the version these bindings were
    /// built for. Refused *before* any call that depends on struct layout:
    /// `prism_config_init` returns `PrismConfig` by value, so a DLL with a
    /// different layout writes through a return pointer the caller never
    /// passed — silent memory corruption, not an error. See `sys.rs`.
    #[error(
        "prism.dll is version {found}, but navigator was built for {built}; \
         the DLL next to the executable must match"
    )]
    VersionMismatch { built: &'static str, found: String },
    #[error("no screen reader / TTS backend available")]
    NoBackend,
    #[error("prism backend returned code {code} ({name})")]
    Backend { code: i32, name: &'static str },
    #[error("text contained interior NUL byte")]
    Nul(#[from] NulError),
}

pub type Result<T> = std::result::Result<T, PrismError>;

/// Lifetime handle for the prism library. Dropping it calls `prism_shutdown`.
///
/// Prism is a singleton in the C sense — only one context at a time. We guard
/// that with an atomic flag so a misuse is surfaced rather than UB.
pub struct Prism {
    ctx: NonNull<ffi::PrismContext>,
}

unsafe impl Send for Prism {}
unsafe impl Sync for Prism {}

/// The prism version the bindings in `sys.rs` match, taken from the
/// vendored distribution's `prism_version.h` at build time.
pub const BUILT_AGAINST: &str = env!("PRISM_VERSION_STRING");

/// Version of the `prism.dll` the loader actually bound, looked up without
/// assuming anything about its ABI. `prism_version_string` is resolved by
/// name because a DLL older than 0.18 does not export it — and that is
/// precisely the one whose `PrismConfig` layout would corrupt the stack.
fn loaded_dll_version() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
        fn GetProcAddress(
            module: *mut std::ffi::c_void,
            name: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_void;
    }
    let name: Vec<u16> = "prism.dll".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let module = GetModuleHandleW(name.as_ptr());
        if module.is_null() {
            return None;
        }
        let sym = GetProcAddress(module, c"prism_version_string".as_ptr());
        if sym.is_null() {
            return None;
        }
        let f: unsafe extern "C" fn() -> *const std::ffi::c_char = std::mem::transmute(sym);
        let p = f();
        if p.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

/// `0.18.3` and `0.18.9` share an ABI; `0.11.6` and `0.18.3` do not.
fn same_major_minor(a: &str, b: &str) -> bool {
    let key = |v: &str| {
        let mut it = v.split('.');
        (it.next().map(str::to_owned), it.next().map(str::to_owned))
    };
    key(a) == key(b)
}

impl Prism {
    pub fn init() -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return Err(PrismError::AlreadyInitialized);
        }
        let found = loaded_dll_version().unwrap_or_else(|| "older than 0.18".to_string());
        if !same_major_minor(&found, BUILT_AGAINST) {
            INITIALIZED.store(false, Ordering::Release);
            return Err(PrismError::VersionMismatch {
                built: BUILT_AGAINST,
                found,
            });
        }
        unsafe {
            let mut cfg = ffi::prism_config_init();
            let raw = ffi::prism_init(&mut cfg);
            match NonNull::new(raw) {
                Some(ctx) => Ok(Self { ctx }),
                None => {
                    INITIALIZED.store(false, Ordering::Release);
                    Err(PrismError::InitFailed)
                }
            }
        }
    }

    /// Acquire the best available speaker for this session.
    pub fn best_speaker(&self) -> Result<Speaker> {
        unsafe {
            let raw = ffi::prism_registry_acquire_best(self.ctx.as_ptr());
            let backend = NonNull::new(raw).ok_or(PrismError::NoBackend)?;
            Ok(Speaker { backend })
        }
    }
}

impl Drop for Prism {
    fn drop(&mut self) {
        unsafe { ffi::prism_shutdown(self.ctx.as_ptr()) }
        INITIALIZED.store(false, Ordering::Release);
    }
}

/// A single backend handle. `speak`/`stop` on this are thread-safe from the
/// prism side but we still wrap it in a `Mutex` in the host where multiple
/// producers exist (plugins, worker threads, UI thread).
pub struct Speaker {
    backend: NonNull<ffi::PrismBackend>,
}

unsafe impl Send for Speaker {}

impl Speaker {
    /// Speak `text`. When `interrupt` is true, cancels any in-flight speech.
    pub fn speak(&self, text: &str, interrupt: bool) -> Result<()> {
        let c = CString::new(text)?;
        unsafe {
            let err = ffi::prism_backend_speak(self.backend.as_ptr(), c.as_ptr(), interrupt);
            check(err)
        }
    }

    pub fn stop(&self) -> Result<()> {
        unsafe { check(ffi::prism_backend_stop(self.backend.as_ptr())) }
    }

    pub fn name(&self) -> &'static str {
        unsafe {
            let p = ffi::prism_backend_name(self.backend.as_ptr());
            if p.is_null() {
                return "";
            }
            std::ffi::CStr::from_ptr(p).to_str().unwrap_or("")
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        unsafe { ffi::prism_backend_free(self.backend.as_ptr()) }
    }
}

fn check(err: ffi::PrismError) -> Result<()> {
    if err == ffi::PRISM_OK {
        Ok(())
    } else {
        let name = unsafe {
            let p = ffi::prism_error_string(err);
            if p.is_null() {
                "unknown"
            } else {
                std::ffi::CStr::from_ptr(p).to_str().unwrap_or("unknown")
            }
        };
        // The str we return has 'static lifetime for formatting; safe because
        // prism_error_string returns pointers into static storage.
        let name_static: &'static str = unsafe { std::mem::transmute::<&str, &'static str>(name) };
        Err(PrismError::Backend {
            code: err,
            name: name_static,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_compares_major_and_minor_only() {
        assert!(same_major_minor("0.18.3", "0.18.9"));
        assert!(!same_major_minor("0.11.6", "0.18.3"));
        assert!(!same_major_minor("older than 0.18", "0.18.3"));
        assert!(!same_major_minor("1.18.3", "0.18.3"));
    }

    /// The DLL the test binary loaded is the vendored one, so the gate
    /// must pass it and `init` must get as far as prism itself.
    #[test]
    fn the_vendored_dll_passes_the_version_gate() {
        let found = loaded_dll_version().expect("vendored prism exports its version");
        assert!(
            same_major_minor(&found, BUILT_AGAINST),
            "{found} vs {BUILT_AGAINST}"
        );
        match Prism::init() {
            Ok(_) | Err(PrismError::InitFailed) | Err(PrismError::AlreadyInitialized) => {}
            Err(e) => panic!("{e}"),
        }
    }
}
