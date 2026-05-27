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

impl Prism {
    pub fn init() -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return Err(PrismError::AlreadyInitialized);
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
