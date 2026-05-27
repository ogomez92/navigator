//! Find the prism library and link it.
//!
//! Resolution order for the base directory:
//!   1. `PRISM_DIR` env var (must contain `include/prism.h` + a `dynamic`/`static` tree)
//!   2. Default: `D:\code\libs\prism\prism-windows-x64`
//!
//! Static linking is selected with `--features static`; otherwise the DLL
//! import lib is used and `prism.dll` is copied next to the final binary.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=PRISM_DIR");
    println!("cargo:rerun-if-changed=build.rs");

    if cfg!(not(target_os = "windows")) {
        panic!("navigator-prism currently targets Windows only");
    }

    let base: PathBuf = env::var_os("PRISM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"D:\code\libs\prism\prism-windows-x64"));

    let profile = env::var("PROFILE").unwrap_or_else(|_| "release".into());
    let flavor = if profile == "debug" {
        "debug"
    } else {
        "release"
    };

    let (lib_dir, link_kind): (PathBuf, &str) = if cfg!(feature = "static") {
        (base.join("static").join(flavor).join("lib"), "static")
    } else {
        (base.join("dynamic").join(flavor).join("lib"), "dylib")
    };

    if !lib_dir.join("prism.lib").is_file() {
        panic!("prism.lib not found under {}", lib_dir.display());
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib={link_kind}=prism");

    // For dynamic linking, stage prism.dll alongside the target binary so the
    // process can find it without touching PATH.
    if link_kind == "dylib" {
        let bin_src = base
            .join("dynamic")
            .join(flavor)
            .join("bin")
            .join("prism.dll");
        if bin_src.is_file() {
            if let Some(out_dir) = target_bin_dir() {
                let dst = out_dir.join("prism.dll");
                let _ = fs::create_dir_all(&out_dir);
                if let Err(e) = fs::copy(&bin_src, &dst) {
                    println!(
                        "cargo:warning=failed to copy prism.dll to {}: {e}",
                        dst.display()
                    );
                }
            }
        } else {
            println!("cargo:warning=prism.dll missing at {}", bin_src.display());
        }
    }
}

fn target_bin_dir() -> Option<PathBuf> {
    // OUT_DIR = target/<profile>/build/<crate>-<hash>/out
    // Go up to target/<profile>.
    let out_dir = PathBuf::from(env::var_os("OUT_DIR")?);
    let mut p: &Path = &out_dir;
    for _ in 0..3 {
        p = p.parent()?;
    }
    Some(p.to_path_buf())
}
