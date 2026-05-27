//! Helpers for building a concrete [`HostApi`] from host-side closures.
//!
//! The host provides a context that knows how to speak, log, and navigate.
//! This module wires that context to `extern "C"` thunks so plugins can call
//! it safely.

use std::ffi::c_void;

use crate::abi::{ABI_VERSION, HostApi, Str};

/// What the host has to implement for plugins to talk to it.
pub trait HostCallbacks: Send + Sync + 'static {
    fn speak(&self, text: &str, interrupt: bool);
    fn log(&self, level: LogLevel, msg: &str);
    fn navigate(&self, path: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Trace,
            1 => Self::Debug,
            2 => Self::Info,
            3 => Self::Warn,
            _ => Self::Error,
        }
    }
}

/// Build a `HostApi` whose thunks downcast `host` back to `&dyn HostCallbacks`.
///
/// # Safety
/// The caller must ensure the `Arc` stays alive longer than every plugin that
/// receives this `HostApi`. In practice the host leaks the `Arc` into a
/// `Box<Arc<dyn HostCallbacks>>` raw pointer it never frees.
pub fn make_host_api(host: *mut c_void) -> HostApi {
    HostApi {
        abi_version: ABI_VERSION,
        host,
        speak: thunk_speak,
        log: thunk_log,
        navigate: thunk_navigate,
    }
}

unsafe extern "C" fn thunk_speak(host: *mut c_void, text: Str, interrupt: bool) {
    let cb = unsafe { &*(host as *const std::sync::Arc<dyn HostCallbacks>) };
    let s = unsafe { text.as_str() };
    cb.speak(s, interrupt);
}

unsafe extern "C" fn thunk_log(host: *mut c_void, level: u8, msg: Str) {
    let cb = unsafe { &*(host as *const std::sync::Arc<dyn HostCallbacks>) };
    let s = unsafe { msg.as_str() };
    cb.log(LogLevel::from_u8(level), s);
}

unsafe extern "C" fn thunk_navigate(host: *mut c_void, path: Str) {
    let cb = unsafe { &*(host as *const std::sync::Arc<dyn HostCallbacks>) };
    let s = unsafe { path.as_str() };
    cb.navigate(s);
}
