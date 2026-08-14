//! TEMPORARY harness — replays a navigator paste against a real remote,
//! through the real driver, so the "checking destination" step runs the
//! exact code the GUI worker runs. Delete once the transfer is verified.
//!
//! Usage: cargo run --release -p navigator-rclone --example live_paste --
//!            <dest> <src>...

use std::io::Write;
use std::time::{Duration, Instant};

use navigator_core::{ConflictMode, NavPath};
use navigator_rclone::op::OpEvent;
use navigator_rclone::{Operation, RcloneDriver};

const MODE: ConflictMode = ConflictMode::Update; // config default `on_conflict`
const ATTEMPTS: usize = 5;

fn say(s: impl AsRef<str>) {
    println!("{}", s.as_ref());
    let _ = std::io::stdout().flush();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dest, sources) = args.split_first().expect("usage: <dest> <src>...");
    let dest_dir = NavPath::new(dest.clone()).expect("dest");
    let sources: Vec<NavPath> = sources
        .iter()
        .map(|s| NavPath::new(s.clone()).expect("src"))
        .collect();

    let driver = RcloneDriver::from_path();
    say(format!(
        "paste -> {} ({} sources, mode {:?})",
        dest_dir.rclone_arg().unwrap_or_else(|| dest_dir.to_string()),
        sources.len(),
        MODE
    ));

    // ---- Phase 1: "checking destination" -------------------------------
    // WorkerCtx::resolve_conflicts. The destination is remote, so
    // `conflict_candidates` returns every source; all four are directories
    // so none of them batch. Two `--dry-run` passes each. This is the step
    // that used to deadlock.
    say("\n== checking destination ==");
    let t0 = Instant::now();
    let mut overwrites = 0usize;
    for src in &sources {
        let t = Instant::now();
        let op = Operation::Copy {
            sources: vec![src.clone()],
            dest_dir: dest_dir.clone(),
            mode: MODE,
        };
        match driver.conflicts(&op) {
            Ok(r) => {
                overwrites += r.overwrites.len();
                say(format!(
                    "  {:>7.1}s  {:4} would-overwrite  {}",
                    t.elapsed().as_secs_f64(),
                    r.overwrites.len(),
                    src.file_name()
                ));
            }
            Err(e) => say(format!("  FAILED  {}: {e}", src.file_name())),
        }
    }
    say(format!(
        "checked in {:.1}s — {} conflicts (no dialog needed)",
        t0.elapsed().as_secs_f64(),
        overwrites
    ));

    // ---- Phase 2: the transfer -----------------------------------------
    // run_batch's per-directory arm: one `copyto` per folder.
    say("\n== transferring ==");
    let mut failed: Vec<String> = Vec::new();
    for src in &sources {
        let name = src.file_name().to_string();
        let mut ok = false;
        for attempt in 1..=ATTEMPTS {
            say(format!("-- {name} (attempt {attempt}/{ATTEMPTS})"));
            let op = Operation::Copy {
                sources: vec![src.clone()],
                dest_dir: dest_dir.clone(),
                mode: MODE,
            };
            let handle = match driver.spawn(op) {
                Ok(h) => h,
                Err(e) => {
                    say(format!("   spawn failed: {e}"));
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let start = Instant::now();
            let mut last = Instant::now();
            for ev in handle.events.iter() {
                match ev {
                    OpEvent::Progress(p) => {
                        if last.elapsed() >= Duration::from_secs(15) {
                            last = Instant::now();
                            say(format!(
                                "   {:>3}%  {}/{} files  {:.1}/{:.1} MB  {:.1} MB/s",
                                p.fraction().map(|f| (f * 100.0) as u32).unwrap_or(0),
                                p.files_done,
                                p.files_total,
                                p.bytes_done as f64 / 1e6,
                                p.bytes_total as f64 / 1e6,
                                p.speed_bps / 1e6,
                            ));
                        }
                    }
                    OpEvent::Done {
                        success,
                        exit_code,
                        error,
                    } => {
                        if success {
                            ok = true;
                            say(format!("   done in {:.1}s", start.elapsed().as_secs_f64()));
                        } else {
                            say(format!(
                                "   FAILED exit={:?}: {}",
                                exit_code,
                                error.map(|e| e.summary()).unwrap_or_default()
                            ));
                        }
                        break;
                    }
                    OpEvent::Log(_) => {}
                }
            }
            if ok {
                break;
            }
            std::thread::sleep(Duration::from_secs(10));
        }
        if !ok {
            failed.push(name);
        }
    }

    if failed.is_empty() {
        say("\nALL SOURCES TRANSFERRED");
    } else {
        say(format!("\nSTILL FAILING: {failed:?}"));
        std::process::exit(1);
    }
}
