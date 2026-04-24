//! rclone operations and process driver.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;

use navigator_core::NavPath;

use crate::log::{LogEvent, LogLevel};

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// Fail the operation if any destination already exists.
    Never,
    /// Always overwrite (rclone default behavior for `copyto`).
    Always,
    /// Caller has already resolved conflicts via a pre-flight pass.
    Resolved,
}

#[derive(Debug, Clone)]
pub enum Operation {
    Copy { sources: Vec<NavPath>, dest_dir: NavPath, policy: OverwritePolicy },
    Move { sources: Vec<NavPath>, dest_dir: NavPath, policy: OverwritePolicy },
    /// Single-source rename to an exact destination path. Used by F2 and
    /// anywhere we need the target filename to differ from the source.
    Rename { src: NavPath, dst: NavPath },
    /// Single-source copy to an exact destination path. Used when the
    /// preflight "Append number" choice gives a copy a new target name
    /// — the caller has already resolved the final filename, so rclone
    /// just runs `copyto` against that path.
    CopyTo { src: NavPath, dst: NavPath },
    Delete { targets: Vec<NavPath> },
}

/// Paths that would be overwritten by a copy/move. Returned from
/// [`RcloneDriver::preflight`] so the UI can ask the user.
#[derive(Debug, Default, Clone)]
pub struct PreflightReport {
    pub would_overwrite: Vec<PathBuf>,
    pub missing_sources: Vec<PathBuf>,
    pub raw_log: Vec<LogEvent>,
}

#[derive(Debug, Clone)]
pub enum OpEvent {
    Log(LogEvent),
    Progress { bytes_done: u64, bytes_total: u64, current: Option<String> },
    Done { success: bool, stderr_tail: String },
}

/// Handle returned by [`RcloneDriver::spawn`]. Dropping it does *not* kill
/// the child; call [`cancel`](Self::cancel) to do that explicitly.
pub struct OpHandle {
    pub id: u64,
    pub events: Receiver<OpEvent>,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl OpHandle {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.kill();
        }
    }
}

#[derive(Debug, Clone)]
pub struct RcloneDriver {
    exe: PathBuf,
    /// Concurrent file transfers within one op. Passed as `--transfers N`
    /// on every spawn. `1..=64`; zero is silently promoted to 1 so an
    /// accidental config value cannot produce `--transfers 0` which rclone
    /// rejects. Default 8.
    transfers: u32,
}

/// Default concurrent-transfers count when no config overrides it. Chosen
/// higher than rclone's own default (4) because a typical SSD saturates
/// closer to 8 parallel streams; config can override for slow disks.
pub const DEFAULT_TRANSFERS: u32 = 8;

impl RcloneDriver {
    /// Look up `rclone` on PATH. Caller can override by passing a custom path.
    pub fn from_path() -> Self {
        Self { exe: PathBuf::from("rclone"), transfers: DEFAULT_TRANSFERS }
    }
    pub fn with_exe(exe: impl Into<PathBuf>) -> Self {
        Self { exe: exe.into(), transfers: DEFAULT_TRANSFERS }
    }

    /// Override the `--transfers N` value used for every spawned op.
    /// Values below `1` are clamped up; callers typically read from
    /// config via `Rclone::transfers_clamped`.
    pub fn with_transfers(mut self, n: u32) -> Self {
        self.transfers = n.max(1);
        self
    }

    /// Current `--transfers` value. Exposed so tests + UI can round-trip
    /// the value without reaching into private state.
    pub fn transfers(&self) -> u32 { self.transfers }

    /// Run the operation under `--dry-run` and collect destinations that
    /// would be overwritten. Blocks until rclone finishes.
    pub fn preflight(&self, op: &Operation) -> std::io::Result<PreflightReport> {
        let mut cmd = self.base_command();
        push_op_args(&mut cmd, op, /*dry_run=*/ true);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let mut report = PreflightReport::default();

        let mut parse = |line: &str| {
            if let Ok(ev) = serde_json::from_str::<LogEvent>(line) {
                if ev.msg.contains("Would copy") || ev.msg.contains("Would move") {
                    if let Some(obj) = ev.object.as_ref() {
                        // rclone prints relative paths; the caller has enough
                        // context to absolutize if needed.
                        report.would_overwrite.push(PathBuf::from(obj));
                    }
                }
                if ev.msg.contains("not found") && matches!(ev.level, Some(LogLevel::Error)) {
                    if let Some(obj) = ev.object.as_ref() {
                        report.missing_sources.push(PathBuf::from(obj));
                    }
                }
                report.raw_log.push(ev);
            }
        };

        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            parse(&line);
        }
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            parse(&line);
        }
        let _ = child.wait()?;
        Ok(report)
    }

    /// Kick off the real operation. Returns immediately with a handle; all
    /// progress and completion flows through `handle.events`.
    pub fn spawn(&self, op: Operation) -> std::io::Result<OpHandle> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut cmd = self.base_command();
        push_op_args(&mut cmd, &op, /*dry_run=*/ false);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (tx, rx) = unbounded::<OpEvent>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let child_slot = Arc::new(Mutex::new(Some(child)));

        let tx_out = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                forward_line(&tx_out, &line);
            }
        });
        let (err_sink_tx, err_sink_rx) = bounded::<String>(1024);
        let tx_err = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                let _ = err_sink_tx.try_send(line.clone());
                forward_line(&tx_err, &line);
            }
        });

        // Waiter thread: wait for exit, fold tail of stderr into Done.
        let child_for_wait = Arc::clone(&child_slot);
        thread::spawn(move || {
            let status = {
                let mut guard = child_for_wait.lock();
                match guard.as_mut() {
                    Some(c) => c.wait().ok(),
                    None => None,
                }
            };
            let success = status.map(|s| s.success()).unwrap_or(false);
            let mut tail = String::new();
            while let Ok(line) = err_sink_rx.try_recv() {
                tail.push_str(&line);
                tail.push('\n');
            }
            let _ = tx.send(OpEvent::Done { success, stderr_tail: tail });
        });

        Ok(OpHandle { id, events: rx, child: child_slot, cancelled })
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.args(self.base_args());
        // Hide the console that Windows would otherwise allocate for a
        // console-subsystem child (rclone) launched from a GUI parent.
        // Without CREATE_NO_WINDOW, every copy/cut/delete pops a cmd
        // window and steals focus. Piped stdout/stderr still work.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }

    /// Arg list that every `rclone` invocation gets, before the
    /// operation-specific verb + paths. Exposed (as plain `Vec<String>`)
    /// so tests can assert that `--transfers N` survives the driver
    /// plumbing without having to spawn a real process.
    pub fn base_args(&self) -> Vec<String> {
        vec![
            "--use-json-log".into(),
            "--log-level".into(), "INFO".into(),
            "--stats".into(), "1s".into(),
            "--stats-log-level".into(), "NOTICE".into(),
            "--stats-one-line".into(),
            "--transfers".into(), self.transfers.to_string(),
        ]
    }
}

fn forward_line(tx: &Sender<OpEvent>, line: &str) {
    let Ok(ev) = serde_json::from_str::<LogEvent>(line) else {
        // Non-JSON lines happen for banners/warnings; wrap them.
        let _ = tx.send(OpEvent::Log(LogEvent {
            level: None,
            msg: line.to_string(),
            source: None,
            object: None,
            object_type: None,
            stats: None,
        }));
        return;
    };

    if let Some(s) = ev.stats.as_ref() {
        let _ = tx.send(OpEvent::Progress {
            bytes_done: s.bytes,
            bytes_total: s.totalBytes,
            current: ev.object.clone(),
        });
    }
    let _ = tx.send(OpEvent::Log(ev));
}

fn push_op_args(cmd: &mut Command, op: &Operation, dry_run: bool) {
    if dry_run { cmd.arg("--dry-run"); }
    match op {
        Operation::Copy { sources, dest_dir, policy } => {
            // For a single file we prefer `copyto` so rclone keeps the name.
            // For a set, loop with `copyto` per source — consistent semantics,
            // easier progress attribution.
            apply_policy(cmd, *policy);
            // One-shot: current UI calls us once per source. Multi-source
            // batching is a follow-up when we wire up progress aggregation.
            if let Some(src) = sources.first() {
                let dest = dest_dir.join(src.file_name());
                cmd.arg("copyto").arg(path_arg(src.as_path())).arg(path_arg(dest.as_path()));
            }
        }
        Operation::Move { sources, dest_dir, policy } => {
            apply_policy(cmd, *policy);
            if let Some(src) = sources.first() {
                let dest = dest_dir.join(src.file_name());
                cmd.arg("moveto").arg(path_arg(src.as_path())).arg(path_arg(dest.as_path()));
            }
        }
        Operation::Rename { src, dst } => {
            // `moveto` with exact paths renames atomically on the same volume
            // and falls back to copy+delete across volumes — rclone handles
            // both under one command.
            cmd.arg("moveto").arg(path_arg(src.as_path())).arg(path_arg(dst.as_path()));
        }
        Operation::CopyTo { src, dst } => {
            cmd.arg("copyto").arg(path_arg(src.as_path())).arg(path_arg(dst.as_path()));
        }
        Operation::Delete { targets } => {
            if let Some(t) = targets.first() {
                cmd.arg("purge").arg(path_arg(t.as_path()));
            }
        }
    }
}

fn apply_policy(cmd: &mut Command, policy: OverwritePolicy) {
    match policy {
        OverwritePolicy::Never => { cmd.arg("--ignore-existing"); }
        OverwritePolicy::Always | OverwritePolicy::Resolved => { /* rclone default */ }
    }
}

fn path_arg(p: &Path) -> String {
    // Local-only for now; rclone accepts plain Windows paths when the
    // colon-in-drive isn't mistaken for a remote. A leading `./` would
    // disambiguate but breaks absolute paths, so we rely on the `C:\...`
    // form which rclone recognises as local.
    p.to_string_lossy().into_owned()
}
