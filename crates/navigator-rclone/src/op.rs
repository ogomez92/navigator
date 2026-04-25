//! rclone operations and process driver.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde::Deserialize;

use navigator_core::{Entry, EntryKind, FileTime, NavPath};

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
    /// Create a directory. `rclone mkdir` succeeds when the target already
    /// exists on most backends; callers that want strict "must be new"
    /// semantics check ahead of time.
    Mkdir { dir: NavPath },
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

    /// Path to the `rclone` executable (or just `"rclone"` when relying
    /// on PATH lookup). Exposed so the GUI can rebuild a full argv for
    /// out-of-band invocations like the UAC-elevated retry path, which
    /// can't go through `spawn` because `ShellExecuteEx` doesn't pipe.
    pub fn exe(&self) -> &Path { &self.exe }

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

        // Echo the spawned argv so log readers can reproduce the failure
        // by hand. Without this the only signal is rclone's own JSON log,
        // which never tells us what flags / paths it was actually given.
        let argv: Vec<String> = std::iter::once(self.exe.to_string_lossy().into_owned())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        tracing::info!(target: "rclone.spawn", op_id = id, "rclone spawn: {}", argv.join(" "));

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

    /// Build a plain (non-JSON-log) command for one-shot queries like
    /// `listremotes` / `lsjson` — they return tidy stdout and would only
    /// be obscured by our transfer-tuning / JSON-log flags.
    fn plain_command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }

    /// Enumerate every remote configured in `rclone.conf`. Names come back
    /// without the trailing `:`. rclone auto-discovers the config file
    /// (`%APPDATA%\rclone\rclone.conf` on Windows), so we don't pass
    /// `--config`.
    pub fn listremotes(&self) -> std::io::Result<Vec<String>> {
        let out = self.plain_command().arg("listremotes").output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "rclone listremotes failed: {}",
                String::from_utf8_lossy(&out.stderr).trim(),
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                if l.is_empty() { return None; }
                Some(l.trim_end_matches(':').to_string())
            })
            .collect())
    }

    /// List one directory via `rclone lsjson <target>`. `target` is the
    /// rclone-form CLI argument (`remote:`, `remote:sub/path`, or
    /// `remote:/abs/path`) — built by the caller from
    /// [`navigator_core::NavPath::rclone_arg`] so absolute-vs-relative
    /// sub-paths survive intact. Returned entries carry the same shape
    /// as `navigator-fs::read_dir`.
    pub fn lsjson(&self, target: &str) -> std::io::Result<Vec<Entry>> {
        let out = self.plain_command().arg("lsjson").arg(target).output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "rclone lsjson {} failed: {}",
                target,
                String::from_utf8_lossy(&out.stderr).trim(),
            )));
        }
        let items: Vec<LsItem> = serde_json::from_slice(&out.stdout)
            .map_err(|e| std::io::Error::other(format!("lsjson parse: {}", e)))?;
        Ok(items
            .into_iter()
            .map(|i| Entry {
                kind: if i.is_dir { EntryKind::Directory } else { EntryKind::File },
                size: if i.size < 0 { 0 } else { i.size as u64 },
                modified: i.mod_time.as_deref().and_then(parse_rfc3339_filetime).unwrap_or_default(),
                created: FileTime::default(),
                attrs: 0,
                hidden: false,
                system: false,
                name: i.name,
            })
            .collect())
    }
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct LsItem {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Size", default)]
    size: i64,
    #[serde(rename = "IsDir", default)]
    is_dir: bool,
    #[serde(rename = "ModTime", default)]
    mod_time: Option<String>,
}

/// Parse an RFC3339 timestamp (what rclone emits for `ModTime`) into a
/// Windows FILETIME. Tolerant — unknown fractional seconds / timezones
/// degrade gracefully to the nearest whole second, since the UI only
/// ever renders this at second granularity. Returns `None` if the string
/// doesn't even look like a date.
fn parse_rfc3339_filetime(s: &str) -> Option<FileTime> {
    // Expect at minimum `YYYY-MM-DDTHH:MM:SS`.
    let bytes = s.as_bytes();
    if bytes.len() < 19 { return None; }
    let year: i32 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    if bytes[4] != b'-' { return None; }
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    if bytes[7] != b'-' { return None; }
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    // Accept `T` or ` ` as the date/time separator (rclone uses `T`).
    if !(bytes[10] == b'T' || bytes[10] == b' ') { return None; }
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    if bytes[13] != b':' { return None; }
    let minute: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    if bytes[16] != b':' { return None; }
    let second: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    let days = days_from_civil(year, month, day);
    let unix_secs = days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64;
    let ticks = unix_secs
        .saturating_mul(10_000_000)
        .saturating_add(FileTime::UNIX_EPOCH_TICKS as i64);
    if ticks < 0 { return None; }
    Some(FileTime(ticks as u64))
}

/// Howard Hinnant's `days_from_civil`: day count from 1970-01-01 for the
/// proleptic Gregorian calendar. Handles any year/month/day without the
/// boilerplate of a full date library.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400) as i64;
    let yoe = (y - (era as i32) * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
    for arg in op_args(op, dry_run) {
        cmd.arg(arg);
    }
}

/// Local filesystem path that an [`Operation`] would write into, if any.
/// Returned for ops whose destination is on the local filesystem so the
/// GUI can probe it for write permission after a failure. `None` means
/// the op is purely remote (no local write to gate on UAC) or the op
/// kind doesn't have a meaningful local dest (e.g. Delete to a remote).
pub fn local_dest_dir(op: &Operation) -> Option<std::path::PathBuf> {
    let nav = match op {
        Operation::Copy { dest_dir, .. } => dest_dir,
        Operation::Move { dest_dir, .. } => dest_dir,
        Operation::Rename { dst, .. } => return dst
            .as_path()
            .parent()
            .filter(|_| !dst.is_remote())
            .map(|p| p.to_path_buf()),
        Operation::CopyTo { dst, .. } => return dst
            .as_path()
            .parent()
            .filter(|_| !dst.is_remote())
            .map(|p| p.to_path_buf()),
        Operation::Mkdir { dir } => return dir
            .as_path()
            .parent()
            .filter(|_| !dir.is_remote())
            .map(|p| p.to_path_buf()),
        Operation::Delete { .. } => return None,
    };
    if nav.is_remote() { return None; }
    Some(nav.as_path().to_path_buf())
}

/// Build the verb + flag + path arg list for an [`Operation`]. Shared by
/// `push_op_args` (Command) and the elevated-retry path (which has to
/// hand a quoted argv to `ShellExecuteEx` because that API can't pipe
/// stdout / stderr).
pub fn op_args(op: &Operation, dry_run: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if dry_run { out.push("--dry-run".into()); }
    match op {
        Operation::Copy { sources, dest_dir, policy } => {
            apply_policy_to(&mut out, *policy);
            if let Some(src) = sources.first() {
                let dest = dest_dir.join(src.file_name());
                out.push("copyto".into());
                out.push(nav_arg(src));
                out.push(nav_arg(&dest));
            }
        }
        Operation::Move { sources, dest_dir, policy } => {
            apply_policy_to(&mut out, *policy);
            if let Some(src) = sources.first() {
                let dest = dest_dir.join(src.file_name());
                out.push("moveto".into());
                out.push(nav_arg(src));
                out.push(nav_arg(&dest));
            }
        }
        Operation::Rename { src, dst } => {
            // `moveto` with exact paths renames atomically on the same volume
            // and falls back to copy+delete across volumes — rclone handles
            // both under one command.
            out.push("moveto".into());
            out.push(nav_arg(src));
            out.push(nav_arg(dst));
        }
        Operation::CopyTo { src, dst } => {
            out.push("copyto".into());
            out.push(nav_arg(src));
            out.push(nav_arg(dst));
        }
        Operation::Delete { targets } => {
            if let Some(t) = targets.first() {
                out.push("purge".into());
                out.push(nav_arg(t));
            }
        }
        Operation::Mkdir { dir } => {
            out.push("mkdir".into());
            out.push(nav_arg(dir));
        }
    }
    out
}

/// Turn a `NavPath` into the string rclone wants on the command line.
/// Remote paths are rewritten as `remote:sub/path`; local paths fall
/// through to `path_arg` (which already handles Windows `C:\...`).
fn nav_arg(p: &NavPath) -> String {
    if let Some(s) = p.rclone_arg() {
        return s;
    }
    path_arg(p.as_path())
}

fn apply_policy_to(out: &mut Vec<String>, policy: OverwritePolicy) {
    match policy {
        OverwritePolicy::Never => out.push("--ignore-existing".into()),
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
