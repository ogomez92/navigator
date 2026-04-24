//! Embed a Windows manifest + schedule a post-link copy of the release
//! binary to `C:\Users\Nitropc\stuff\bin\x.exe`.
//!
//! Cargo has no post-build hook: `build.rs` runs *before* rustc links
//! the binary. To get the copy done anyway we self-spawn the build-script
//! binary as a detached watcher. When invoked with the
//! `NAVIGATOR_RELEASE_WATCH` env var set, `main()` enters a polling loop
//! that waits for `target/release/navigator.exe`'s mtime to bump past a
//! baseline and copies it, then exits. No PowerShell, no shell parsing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use embed_manifest::{embed_manifest, new_manifest};
use embed_manifest::manifest::{ActiveCodePage, DpiAwareness, SupportedOS};

const WATCH_ENV: &str = "NAVIGATOR_RELEASE_WATCH";
const DST: &str = r"C:\Users\Nitropc\stuff\bin\x.exe";

fn main() {
    // Watcher path: the build-script binary was re-spawned by an earlier
    // invocation of ourselves. Run the poll loop and exit without
    // touching any cargo:* directives (those belong only to the real
    // build.rs run and would confuse cargo if emitted here).
    if std::env::var(WATCH_ENV).is_ok() {
        let src = std::env::var("NAVIGATOR_RELEASE_SRC").ok().map(PathBuf::from);
        let dst = std::env::var("NAVIGATOR_RELEASE_DST")
            .ok()
            .unwrap_or_else(|| DST.to_string());
        let log = std::env::var("NAVIGATOR_RELEASE_LOG").ok().map(PathBuf::from);
        if let Some(src) = src {
            run_watcher(&src, Path::new(&dst), log.as_deref());
        }
        return;
    }

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let manifest = new_manifest("Anthropic.Navigator")
        .active_code_page(ActiveCodePage::Utf8)
        .dpi_awareness(DpiAwareness::PerMonitorV2)
        .supported_os(SupportedOS::Windows10..=SupportedOS::Windows10);
    embed_manifest(manifest).expect("embed manifest");
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run this build script whenever the linked binary's mtime
    // changes. Cargo otherwise skips build.rs when only a workspace dep
    // rebuilt — navigator relinks, but without the rerun directive the
    // watcher wouldn't spawn and x.exe would stay stale.
    println!("cargo:rerun-if-changed=../../target/release/navigator.exe");

    spawn_release_copy_watcher();
}

fn spawn_release_copy_watcher() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let out_dir = match std::env::var("OUT_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => return,
    };
    // OUT_DIR = <target>/release/build/navigator-XXXX/out. Walk up three
    // to reach the profile dir holding the linked binary.
    let Some(release_dir) = out_dir.ancestors().nth(3).map(Path::to_path_buf) else {
        return;
    };
    let src = release_dir.join("navigator.exe");
    let log = std::env::temp_dir().join("navigator-release-copy.log");
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            println!("cargo:warning=release-copy: current_exe: {}", e);
            return;
        }
    };

    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS severs the child from cargo's console so it
    // outlives the build script. CREATE_NO_WINDOW prevents a flash.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    match std::process::Command::new(&exe)
        .env(WATCH_ENV, "1")
        .env("NAVIGATOR_RELEASE_SRC", &src)
        .env("NAVIGATOR_RELEASE_DST", DST)
        .env("NAVIGATOR_RELEASE_LOG", &log)
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {
            println!(
                "cargo:warning=release-copy: watcher spawned (log: {})",
                log.display()
            );
        }
        Err(e) => {
            println!("cargo:warning=release-copy: spawn failed: {}", e);
        }
    }
}

/// Poll `src` until its mtime bumps past baseline, then copy to `dst`.
/// Gives up after 5 minutes so a failed / cancelled build doesn't leave
/// a ghost process. All activity appends to `log` so stalled copies are
/// post-mortem-able without touching cargo output.
fn run_watcher(src: &Path, dst: &Path, log: Option<&Path>) {
    let log_line = |msg: &str| {
        let Some(p) = log else { return; };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{}  {}", unix_ms(), msg);
        }
    };

    log_line(&format!(
        "watcher start pid={} src={} dst={}",
        std::process::id(),
        src.display(),
        dst.display()
    ));

    if let Some(parent) = dst.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log_line(&format!("mkdir {}: {}", parent.display(), e));
                return;
            }
        }
    }

    let baseline = mtime(src);
    log_line(&format!("baseline = {:?}", baseline));

    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if let Some(cur) = mtime(src) {
            let fresh = match baseline {
                None => true,
                Some(b) => cur > b,
            };
            if fresh {
                match std::fs::copy(src, dst) {
                    Ok(n) => {
                        log_line(&format!("copied {} bytes -> {}", n, dst.display()));
                        return;
                    }
                    Err(e) => {
                        log_line(&format!("copy failed: {}", e));
                        // Retry — linker may still hold a lock briefly.
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    log_line("timed out waiting for mtime bump");
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
