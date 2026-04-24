//! Binary entry point.
//!
//! Intentionally thin — configuration parsing, tracing setup, then hand off
//! to `navigator_gui::run`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

use navigator_core::NavPath;
use navigator_gui::{run, AppConfig};

fn main() -> anyhow_lite::Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let initial_path = args
        .first()
        .and_then(|p| NavPath::new(PathBuf::from(p)).ok())
        .unwrap_or_else(NavPath::default_root);

    let mut cfg = AppConfig::with_defaults();
    cfg.initial_path = initial_path;
    let rc = run(cfg).map_err(|e| anyhow_lite::anyhow(e.to_string()))?;
    std::process::exit(rc);
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("NAVIGATOR_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Zero-dep error propagation so we don't pull `anyhow` just for `main`.
mod anyhow_lite {
    #[derive(Debug)]
    pub struct Error(pub String);
    impl std::fmt::Display for Error { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) } }
    impl std::error::Error for Error {}
    pub type Result<T> = std::result::Result<T, Error>;
    pub fn anyhow(s: impl Into<String>) -> Error { Error(s.into()) }
}
