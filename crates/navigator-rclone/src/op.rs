//! rclone operations and process driver.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;

use navigator_core::{ConflictMode, Entry, EntryKind, FileTime, NavPath};

use crate::log::{LogEvent, LogLevel};

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Windows job-object plumbing so every spawned rclone child is killed
/// when the navigator process exits. Without it, closing the window
/// mid-copy leaves rclone.exe running detached (Windows does not kill
/// children with their parent) — the transfer keeps going with no UI,
/// no progress, and no completion bookkeeping. The job is created once,
/// flagged `KILL_ON_JOB_CLOSE`, and never closed by us: when the process
/// exits its only handle closes and the OS terminates the children.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    /// Lazily create the process-wide job. Returns the raw handle value
    /// (as `isize` so it can live in a `OnceLock` — `HANDLE` isn't `Send`).
    /// `0` means creation failed; callers then no-op so a missing job never
    /// breaks file operations.
    fn job_handle() -> isize {
        static JOB: OnceLock<isize> = OnceLock::new();
        *JOB.get_or_init(|| unsafe {
            let Ok(h) = CreateJobObjectW(None, PCWSTR::null()) else {
                return 0;
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok.is_err() {
                let _ = windows::Win32::Foundation::CloseHandle(h);
                return 0;
            }
            // Deliberately leak `h` — it must stay open for the whole
            // process lifetime so the kill-on-close semantics fire on exit.
            h.0 as isize
        })
    }

    /// Assign `child` to the kill-on-close job. Best-effort: failures are
    /// swallowed (the child simply keeps the old orphan behaviour).
    pub fn assign(child: &Child) {
        let raw = job_handle();
        if raw == 0 {
            return;
        }
        unsafe {
            let _ = AssignProcessToJobObject(
                HANDLE(raw as *mut core::ffi::c_void),
                HANDLE(child.as_raw_handle()),
            );
        }
    }
}

#[derive(Debug, Clone)]
pub enum Operation {
    Copy {
        sources: Vec<NavPath>,
        dest_dir: NavPath,
        mode: ConflictMode,
    },
    Move {
        sources: Vec<NavPath>,
        dest_dir: NavPath,
        mode: ConflictMode,
    },
    /// Copy many files that share a source directory in **one** rclone
    /// invocation, via `--files-from`.
    ///
    /// This exists because `Copy` is one process per item: `op_args` only
    /// reads `sources.first()`, so a 200-file paste was 200 sequential
    /// spawns and `--transfers N` had a single file to work with, i.e.
    /// nothing to parallelise. Measured through this driver on 200 small
    /// files: 28.98 s of process churn versus 0.26 s batched.
    ///
    /// **Only regular files may be listed.** `--files-from` silently
    /// ignores directory entries — no transfer, no warning, and exit code
    /// 0 — so routing a folder through here loses it while reporting
    /// success. Callers partition via `navigator_gui::batch`; the
    /// `files_from_silently_ignores_directories` test pins the rclone
    /// behaviour that makes this a hard rule rather than a preference.
    ///
    /// `list_file` is a UTF-8 file of source-relative names, one per line,
    /// owned by the caller and kept alive for the whole operation.
    /// Destinations land directly in `dest_dir` (this is `copy`, not
    /// `copyto`, so no source basename is appended).
    CopyBatch {
        src_root: NavPath,
        list_file: PathBuf,
        dest_dir: NavPath,
        mode: ConflictMode,
    },
    /// Move counterpart of [`Operation::CopyBatch`]. Same rules.
    MoveBatch {
        src_root: NavPath,
        list_file: PathBuf,
        dest_dir: NavPath,
        mode: ConflictMode,
    },
    /// Single-source rename to an exact destination path. Used by F2 and
    /// anywhere we need the target filename to differ from the source.
    Rename { src: NavPath, dst: NavPath },
    /// Single-source copy to an exact destination path. Used when the
    /// preflight "Append number" choice gives a copy a new target name
    /// — the caller has already resolved the final filename, so rclone
    /// just runs `copyto` against that path.
    CopyTo { src: NavPath, dst: NavPath },
    Delete {
        targets: Vec<NavPath>,
        /// Whether the (single) target is a directory. rclone's delete
        /// verbs are strictly complementary: `purge` removes a directory
        /// and its contents but rejects a file ("is a file not a
        /// directory"), while `deletefile` removes a single file but
        /// rejects a directory. The caller must classify the target so
        /// `op_args` can pick the right verb.
        is_dir: bool,
    },
    /// Create a directory. `rclone mkdir` succeeds when the target already
    /// exists on most backends; callers that want strict "must be new"
    /// semantics check ahead of time.
    Mkdir { dir: NavPath },
    /// Create an empty file (or bump its mtime if it already exists).
    /// `rclone touch` is the cross-backend equivalent of Unix `touch`, so
    /// this works for local and remote endpoints alike.
    Touch { file: NavPath },
}

/// What a `--dry-run` pass says an operation would do. Returned from
/// [`RcloneDriver::preflight`].
///
/// Note `would_transfer` is *everything* rclone would write, including
/// brand-new files — it is not a conflict list on its own. Conflicts come
/// from [`RcloneDriver::conflicts`], which diffs two passes.
#[derive(Debug, Default, Clone)]
pub struct PreflightReport {
    /// Destination-relative paths rclone would copy or move.
    pub would_transfer: Vec<PathBuf>,
    /// Destination-relative paths rclone would delete. Only ever populated
    /// for `Mirror` (`rclone sync`), which prunes what the source lacks.
    pub would_delete: Vec<PathBuf>,
    pub missing_sources: Vec<PathBuf>,
    pub raw_log: Vec<LogEvent>,
}

/// What a paste is about to destroy. Empty in every field means the
/// operation is purely additive and can run without confirmation.
#[derive(Debug, Default, Clone)]
pub struct ConflictReport {
    /// Existing destinations that would be overwritten.
    pub overwrites: Vec<PathBuf>,
    /// Destinations that would be deleted because the source lacks them.
    /// Only `Mirror` produces these, and they are the reason it is gated
    /// behind Paste special — the user never selected these files.
    pub deletes: Vec<PathBuf>,
    pub missing_sources: Vec<PathBuf>,
}

impl ConflictReport {
    /// `true` when nothing at the destination would be harmed.
    pub fn is_empty(&self) -> bool {
        self.overwrites.is_empty() && self.deletes.is_empty()
    }

    /// Total count of destination items at risk.
    pub fn len(&self) -> usize {
        self.overwrites.len() + self.deletes.len()
    }
}

/// Read the [`ConflictMode`] out of an operation, if it carries one.
fn mode_of(op: &Operation) -> Option<ConflictMode> {
    match op {
        Operation::Copy { mode, .. }
        | Operation::Move { mode, .. }
        | Operation::CopyBatch { mode, .. }
        | Operation::MoveBatch { mode, .. } => Some(*mode),
        _ => None,
    }
}

/// Force an operation onto a different [`ConflictMode`]. No-op for ops
/// that have no mode. Used to build the second dry-run pass.
fn set_mode(op: &mut Operation, new: ConflictMode) {
    match op {
        Operation::Copy { mode, .. }
        | Operation::Move { mode, .. }
        | Operation::CopyBatch { mode, .. }
        | Operation::MoveBatch { mode, .. } => *mode = new,
        _ => {}
    }
}

/// Destinations that already exist *and* would still be written under the
/// caller's chosen mode — i.e. the files a paste is about to destroy.
///
/// Derived by diffing two dry-run passes rather than by probing the
/// filesystem: `full` is the chosen mode, `additive` is the same operation
/// forced to [`ConflictMode::AddNewOnly`]. Anything in `full` but not in
/// `additive` was excluded purely because the destination already exists,
/// which is exactly the definition of a conflict. Diffing keeps this
/// backend-agnostic — it works for a remote destination with no `lsjson`
/// round-trip and no path arithmetic to get wrong.
pub fn victims(full: &[PathBuf], additive: &[PathBuf]) -> Vec<PathBuf> {
    let additive: std::collections::HashSet<&Path> = additive.iter().map(|p| p.as_path()).collect();
    full.iter()
        .filter(|p| !additive.contains(p.as_path()))
        .cloned()
        .collect()
}

#[derive(Debug, Clone)]
pub enum OpEvent {
    Log(LogEvent),
    Progress {
        bytes_done: u64,
        bytes_total: u64,
        current: Option<String>,
    },
    Done {
        success: bool,
        stderr_tail: String,
    },
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
        Self {
            exe: PathBuf::from("rclone"),
            transfers: DEFAULT_TRANSFERS,
        }
    }
    pub fn with_exe(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            transfers: DEFAULT_TRANSFERS,
        }
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
    pub fn transfers(&self) -> u32 {
        self.transfers
    }

    /// Path to the `rclone` executable (or just `"rclone"` when relying
    /// on PATH lookup). Exposed so the GUI can rebuild a full argv for
    /// out-of-band invocations like the UAC-elevated retry path, which
    /// can't go through `spawn` because `ShellExecuteEx` doesn't pipe.
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Run the operation under `--dry-run` and collect destinations that
    /// would be overwritten. Blocks until rclone finishes.
    pub fn preflight(&self, op: &Operation) -> std::io::Result<PreflightReport> {
        let mut cmd = self.base_command();
        push_op_args(&mut cmd, op, /*dry_run=*/ true);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        #[cfg(windows)]
        job::assign(&child);
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let mut report = PreflightReport::default();

        let mut parse = |line: &str| {
            if let Ok(ev) = serde_json::from_str::<LogEvent>(line) {
                // Read the structured `skipped` field, not `msg`. rclone
                // renamed the human text ("Would copy" → "Skipped copy as
                // --dry-run is set") and the old substring match had been
                // matching nothing for releases.
                if let Some(obj) = ev.object.as_ref() {
                    if ev.is_dry_run("copy") || ev.is_dry_run("move") {
                        // rclone prints destination-relative paths; the
                        // caller has enough context to absolutize.
                        report.would_transfer.push(PathBuf::from(obj));
                    } else if ev.is_dry_run("delete") {
                        report.would_delete.push(PathBuf::from(obj));
                    }
                }
                if ev.msg.contains("not found")
                    && matches!(ev.level, Some(LogLevel::Error))
                    && let Some(obj) = ev.object.as_ref()
                {
                    report.missing_sources.push(PathBuf::from(obj));
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

    /// Conflict report for `op`: which existing destinations the chosen
    /// mode would overwrite, and (for `Mirror`) which it would delete.
    ///
    /// Runs two `--dry-run` passes — the caller's mode, then the same
    /// operation forced to [`ConflictMode::AddNewOnly`] — and diffs them
    /// via [`victims`]. Two passes because rclone will not tell us *why*
    /// it chose to transfer something; excluding-by-existence is the only
    /// signal that isolates conflicts from brand-new files, and it is the
    /// same answer for local and remote destinations.
    ///
    /// [`ConflictMode::AddNewOnly`] short-circuits to an empty report: by
    /// definition it never writes over anything, so there is nothing to
    /// warn about and no reason to pay for the passes.
    pub fn conflicts(&self, op: &Operation) -> std::io::Result<ConflictReport> {
        if mode_of(op) == Some(ConflictMode::AddNewOnly) {
            return Ok(ConflictReport::default());
        }
        let full = self.preflight(op)?;
        let mut additive = op.clone();
        set_mode(&mut additive, ConflictMode::AddNewOnly);
        let additive = self.preflight(&additive)?;
        Ok(ConflictReport {
            overwrites: victims(&full.would_transfer, &additive.would_transfer),
            deletes: full.would_delete,
            missing_sources: full.missing_sources,
        })
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
        // Tie this rclone child to the process-wide job object so it dies
        // with the navigator process instead of orphaning a half-finished
        // copy/move when the user closes the window mid-op.
        #[cfg(windows)]
        job::assign(&child);
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
            let _ = tx.send(OpEvent::Done {
                success,
                stderr_tail: tail,
            });
        });

        Ok(OpHandle {
            id,
            events: rx,
            child: child_slot,
            cancelled,
        })
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
            "--log-level".into(),
            "INFO".into(),
            "--stats".into(),
            "1s".into(),
            "--stats-log-level".into(),
            "NOTICE".into(),
            "--stats-one-line".into(),
            "--transfers".into(),
            self.transfers.to_string(),
            // Treat local paths literally. rclone's default Windows local
            // encoding maps shell-invalid chars (|, ?, *, :, ...) to their
            // full-width Unicode equivalents, so a file literally named with
            // a full-width `｜` (U+FF5C) on disk gets re-encoded to ASCII `|`
            // and rclone then can't find it ("directory not found"). We hand
            // rclone the exact on-disk name (local listings come from
            // navigator-fs / FindFirstFileW, not rclone), so disabling the
            // encoding makes those names round-trip. Only affects the *local*
            // backend; remote backends keep their own encoding.
            "--local-encoding".into(),
            "None".into(),
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
                if l.is_empty() {
                    return None;
                }
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
                kind: if i.is_dir {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                size: if i.size < 0 { 0 } else { i.size as u64 },
                modified: i
                    .mod_time
                    .as_deref()
                    .and_then(parse_rfc3339_filetime)
                    .unwrap_or_default(),
                created: FileTime::default(),
                attrs: 0,
                hidden: false,
                system: false,
                name: i.name,
            })
            .collect())
    }

    /// Stat a single remote path via `rclone lsjson --stat -M --no-modtime=false <target>`.
    /// Returns `None` if the target doesn't exist. Metadata is whatever the
    /// backend exposes (sftp/local return mode/uid/gid; cloud backends often
    /// return little-to-nothing — see [`RemoteStat::metadata`]).
    pub fn stat(&self, target: &str) -> std::io::Result<Option<RemoteStat>> {
        let out = self
            .plain_command()
            .arg("lsjson")
            .arg("--stat")
            .arg("-M")
            .arg(target)
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "rclone lsjson --stat {} failed: {}",
                target,
                String::from_utf8_lossy(&out.stderr).trim(),
            )));
        }
        // `--stat` emits a single JSON object; some rclone versions still
        // wrap it in an array. Tolerate both.
        let trimmed = out
            .stdout
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect::<Vec<u8>>();
        let v: Value = match serde_json::from_slice(&trimmed) {
            Ok(v) => v,
            Err(e) => return Err(std::io::Error::other(format!("stat parse: {}", e))),
        };
        let obj = match v {
            Value::Array(arr) => arr.into_iter().next(),
            Value::Object(_) => Some(v),
            _ => None,
        };
        let Some(item) = obj else {
            return Ok(None);
        };
        let item: LsItemFull = serde_json::from_value(item)
            .map_err(|e| std::io::Error::other(format!("stat parse: {}", e)))?;
        Ok(Some(RemoteStat::from_full(item)))
    }

    /// Recursive count + byte total via `rclone size --json <target>`.
    /// `count` is the number of files (not directories); `bytes` is the
    /// recursive total. `sizeless` is the number of objects whose size
    /// rclone couldn't determine (some cloud backends).
    pub fn size(&self, target: &str) -> std::io::Result<RemoteSize> {
        let out = self
            .plain_command()
            .arg("size")
            .arg("--json")
            .arg(target)
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "rclone size {} failed: {}",
                target,
                String::from_utf8_lossy(&out.stderr).trim(),
            )));
        }
        let s: RemoteSize = serde_json::from_slice(&out.stdout)
            .map_err(|e| std::io::Error::other(format!("size parse: {}", e)))?;
        Ok(s)
    }
}

/// Subset of `rclone lsjson --stat -M` output. Metadata is only populated
/// for backends that expose it (sftp, local, smb partial, ...). Empty
/// metadata is the common case for cloud backends like Drive / S3.
#[derive(Debug, Clone, Default)]
pub struct RemoteStat {
    pub name: String,
    pub size: i64,
    pub is_dir: bool,
    pub mod_time: Option<String>,
    pub mime_type: Option<String>,
    /// Raw key/value pairs as rclone reports them. Common keys: `mode`,
    /// `uid`, `gid`, `mtime`, `atime`, `btime`, `link-target`. Values are
    /// kept as strings to avoid lossy parsing for unknown keys.
    pub metadata: BTreeMap<String, String>,
}

impl RemoteStat {
    fn from_full(i: LsItemFull) -> Self {
        let metadata = i
            .metadata
            .map(|m| {
                m.into_iter()
                    .map(|(k, v)| {
                        let v = match v {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        (k, v)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: i.name,
            size: i.size,
            is_dir: i.is_dir,
            mod_time: i.mod_time,
            mime_type: i.mime_type,
            metadata,
        }
    }

    /// Best-effort UNIX mode lookup. rclone's `mode` is a decimal string
    /// of the full `st_mode` (file-type bits + permission bits). Returns
    /// the decoded `u32` or `None` if the metadata is missing / unparsable.
    pub fn unix_mode(&self) -> Option<u32> {
        let raw = self.metadata.get("mode")?;
        // rclone emits decimal; some backends emit octal with a leading 0.
        if let Some(stripped) = raw.strip_prefix('0').filter(|s| !s.is_empty())
            && let Ok(v) = u32::from_str_radix(stripped, 8)
        {
            return Some(v);
        }
        raw.parse::<u32>().ok()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RemoteSize {
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub bytes: i64,
    #[serde(default)]
    pub sizeless: i64,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct LsItemFull {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Size", default)]
    size: i64,
    #[serde(rename = "IsDir", default)]
    is_dir: bool,
    #[serde(rename = "ModTime", default)]
    mod_time: Option<String>,
    #[serde(rename = "MimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "Metadata", default)]
    metadata: Option<serde_json::Map<String, Value>>,
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
    if bytes.len() < 19 {
        return None;
    }
    let year: i32 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    if bytes[4] != b'-' {
        return None;
    }
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    if bytes[7] != b'-' {
        return None;
    }
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    // Accept `T` or ` ` as the date/time separator (rclone uses `T`).
    if !(bytes[10] == b'T' || bytes[10] == b' ') {
        return None;
    }
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    if bytes[13] != b':' {
        return None;
    }
    let minute: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    if bytes[16] != b':' {
        return None;
    }
    let second: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    let days = days_from_civil(year, month, day);
    let unix_secs = days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64;
    let ticks = unix_secs
        .saturating_mul(10_000_000)
        .saturating_add(FileTime::UNIX_EPOCH_TICKS as i64);
    if ticks < 0 {
        return None;
    }
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
            skipped: None,
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
        Operation::CopyBatch { dest_dir, .. } => dest_dir,
        Operation::MoveBatch { dest_dir, .. } => dest_dir,
        Operation::Rename { dst, .. } => {
            return dst
                .as_path()
                .parent()
                .filter(|_| !dst.is_remote())
                .map(|p| p.to_path_buf());
        }
        Operation::CopyTo { dst, .. } => {
            return dst
                .as_path()
                .parent()
                .filter(|_| !dst.is_remote())
                .map(|p| p.to_path_buf());
        }
        Operation::Mkdir { dir } => {
            return dir
                .as_path()
                .parent()
                .filter(|_| !dir.is_remote())
                .map(|p| p.to_path_buf());
        }
        Operation::Touch { file } => {
            return file
                .as_path()
                .parent()
                .filter(|_| !file.is_remote())
                .map(|p| p.to_path_buf());
        }
        Operation::Delete { .. } => return None,
    };
    if nav.is_remote() {
        return None;
    }
    Some(nav.as_path().to_path_buf())
}

/// Build the verb + flag + path arg list for an [`Operation`]. Shared by
/// `push_op_args` (Command) and the elevated-retry path (which has to
/// hand a quoted argv to `ShellExecuteEx` because that API can't pipe
/// stdout / stderr).
pub fn op_args(op: &Operation, dry_run: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if dry_run {
        out.push("--dry-run".into());
    }
    match op {
        Operation::Copy {
            sources,
            dest_dir,
            mode,
        } => {
            out.extend(mode.flags().iter().map(|s| s.to_string()));
            if let Some(src) = sources.first() {
                let dest = dest_dir.join(src.file_name());
                // Mirror is the one mode that changes the verb: `sync`
                // prunes destination entries the source lacks, which no
                // combination of copy flags can express.
                out.push(if *mode == ConflictMode::Mirror {
                    "sync".into()
                } else {
                    "copyto".into()
                });
                out.push(nav_arg(src));
                out.push(nav_arg(&dest));
            }
        }
        Operation::Move {
            sources,
            dest_dir,
            mode,
        } => {
            // Mirror has no move equivalent — rclone has no verb that both
            // prunes the destination and empties the source. It degrades to
            // `Replace` here so the state is defined rather than
            // surprising; the UI also hides Mirror for a cut clipboard, so
            // this path is belt-and-braces.
            let mode = if *mode == ConflictMode::Mirror {
                ConflictMode::Replace
            } else {
                *mode
            };
            out.extend(mode.flags().iter().map(|s| s.to_string()));
            if let Some(src) = sources.first() {
                let dest = dest_dir.join(src.file_name());
                out.push("moveto".into());
                out.push(nav_arg(src));
                out.push(nav_arg(&dest));
            }
        }
        Operation::CopyBatch {
            src_root,
            list_file,
            dest_dir,
            mode,
        }
        | Operation::MoveBatch {
            src_root,
            list_file,
            dest_dir,
            mode,
        } => {
            // Mirror must never reach a batch: `sync --files-from` would
            // consider only the listed names and prune everything else at
            // the destination — deleting files the user never selected and
            // never saw a confirm for. Callers gate on this; degrading to
            // Replace here means a mistake costs an extra overwrite rather
            // than an unannounced mass delete.
            let mode = if *mode == ConflictMode::Mirror {
                ConflictMode::Replace
            } else {
                *mode
            };
            out.extend(mode.flags().iter().map(|s| s.to_string()));
            out.push("--files-from".into());
            out.push(list_file.to_string_lossy().into_owned());
            // `copy`/`move`, not `copyto`/`moveto`: with --files-from the
            // listed relative paths are reproduced under dest_dir as-is.
            out.push(
                if matches!(op, Operation::CopyBatch { .. }) {
                    "copy"
                } else {
                    "move"
                }
                .into(),
            );
            out.push(nav_arg(src_root));
            out.push(nav_arg(dest_dir));
        }
        Operation::Rename { src, dst } => {
            out.push("moveto".into());
            out.push(nav_arg(src));
            out.push(nav_arg(dst));
        }
        Operation::CopyTo { src, dst } => {
            out.push("copyto".into());
            out.push(nav_arg(src));
            out.push(nav_arg(dst));
        }
        Operation::Delete { targets, is_dir } => {
            if let Some(t) = targets.first() {
                out.push(if *is_dir { "purge" } else { "deletefile" }.into());
                out.push(nav_arg(t));
            }
        }
        Operation::Mkdir { dir } => {
            out.push("mkdir".into());
            out.push(nav_arg(dir));
        }
        Operation::Touch { file } => {
            out.push("touch".into());
            out.push(nav_arg(file));
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

fn path_arg(p: &Path) -> String {
    // Local-only for now; rclone accepts plain Windows paths when the
    // colon-in-drive isn't mistaken for a remote. A leading `./` would
    // disambiguate but breaks absolute paths, so we rely on the `C:\...`
    // form which rclone recognises as local.
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote `Delete` of a directory must emit `purge`; a file must
    /// emit `deletefile`. The verbs are not interchangeable — `purge`
    /// rejects a file ("is a file not a directory") and `deletefile`
    /// rejects a directory — so picking by `is_dir` is the whole fix.
    #[test]
    fn delete_picks_verb_by_kind() {
        let file = NavPath::new("gdrive:notes/todo.txt").unwrap();
        let args = op_args(
            &Operation::Delete {
                targets: vec![file],
                is_dir: false,
            },
            false,
        );
        assert_eq!(args, vec!["deletefile", "gdrive:notes/todo.txt"]);

        let dir = NavPath::new("gdrive:notes").unwrap();
        let args = op_args(
            &Operation::Delete {
                targets: vec![dir],
                is_dir: true,
            },
            false,
        );
        assert_eq!(args, vec!["purge", "gdrive:notes"]);
    }

    fn copy_with(mode: ConflictMode) -> Vec<String> {
        op_args(
            &Operation::Copy {
                sources: vec![NavPath::new("C:\\a\\photos").unwrap()],
                dest_dir: NavPath::new("C:\\b").unwrap(),
                mode,
            },
            false,
        )
    }

    /// Each mode contributes exactly one flag (or none) ahead of the verb.
    /// These flag names are the entire contract with rclone's conflict
    /// behaviour, so assert the full argv rather than just "contains".
    #[test]
    fn copy_modes_map_to_flags() {
        assert_eq!(
            copy_with(ConflictMode::AddNewOnly),
            vec![
                "--ignore-existing",
                "copyto",
                "C:\\a\\photos",
                "C:\\b\\photos"
            ]
        );
        assert_eq!(
            copy_with(ConflictMode::Update),
            vec!["--update", "copyto", "C:\\a\\photos", "C:\\b\\photos"]
        );
        assert_eq!(
            copy_with(ConflictMode::Replace),
            vec!["--ignore-times", "copyto", "C:\\a\\photos", "C:\\b\\photos"]
        );
    }

    /// Mirror is the only mode that swaps the verb — `sync` instead of
    /// `copyto` — and it adds no flags, because it wants rclone's default
    /// size+mtime comparison for the files it does copy.
    #[test]
    fn mirror_switches_verb_to_sync() {
        assert_eq!(
            copy_with(ConflictMode::Mirror),
            vec!["sync", "C:\\a\\photos", "C:\\b\\photos"]
        );
    }

    /// A move can never mirror: rclone has no verb that prunes the
    /// destination *and* empties the source. Rather than emit a `sync`
    /// that silently leaves the source in place, Mirror degrades to
    /// Replace so the behaviour is defined.
    #[test]
    fn move_degrades_mirror_to_replace() {
        let args = op_args(
            &Operation::Move {
                sources: vec![NavPath::new("C:\\a\\photos").unwrap()],
                dest_dir: NavPath::new("C:\\b").unwrap(),
                mode: ConflictMode::Mirror,
            },
            false,
        );
        assert_eq!(
            args,
            vec!["--ignore-times", "moveto", "C:\\a\\photos", "C:\\b\\photos"]
        );
    }

    /// `--dry-run` must lead the argv so it applies to the whole
    /// invocation regardless of mode flags.
    #[test]
    fn dry_run_flag_leads() {
        let args = op_args(
            &Operation::Copy {
                sources: vec![NavPath::new("C:\\a\\f.txt").unwrap()],
                dest_dir: NavPath::new("C:\\b").unwrap(),
                mode: ConflictMode::Update,
            },
            true,
        );
        assert_eq!(args[0], "--dry-run");
        assert_eq!(args[1], "--update");
    }

    fn batch_args(mode: ConflictMode, cut: bool) -> Vec<String> {
        let mk = |mode| {
            if cut {
                Operation::MoveBatch {
                    src_root: NavPath::new("C:\\src").unwrap(),
                    list_file: PathBuf::from("C:\\tmp\\list.txt"),
                    dest_dir: NavPath::new("C:\\dst").unwrap(),
                    mode,
                }
            } else {
                Operation::CopyBatch {
                    src_root: NavPath::new("C:\\src").unwrap(),
                    list_file: PathBuf::from("C:\\tmp\\list.txt"),
                    dest_dir: NavPath::new("C:\\dst").unwrap(),
                    mode,
                }
            }
        };
        op_args(&mk(mode), false)
    }

    /// The batched verb must be `copy`/`move`, never `copyto`/`moveto`:
    /// with `--files-from` the listed names are reproduced directly under
    /// the destination, whereas `copyto` would treat the destination as a
    /// single target path and bury everything a level deep.
    #[test]
    fn batch_uses_copy_not_copyto() {
        assert_eq!(
            batch_args(ConflictMode::Update, false),
            vec![
                "--update",
                "--files-from",
                "C:\\tmp\\list.txt",
                "copy",
                "C:\\src",
                "C:\\dst"
            ]
        );
        assert_eq!(
            batch_args(ConflictMode::Update, true),
            vec![
                "--update",
                "--files-from",
                "C:\\tmp\\list.txt",
                "move",
                "C:\\src",
                "C:\\dst"
            ]
        );
    }

    /// Mode flags have to survive the `--files-from` rearrangement, or a
    /// batched paste would silently ignore the user's conflict choice.
    #[test]
    fn batch_carries_mode_flags() {
        assert_eq!(
            batch_args(ConflictMode::AddNewOnly, false)[0],
            "--ignore-existing"
        );
        assert_eq!(
            batch_args(ConflictMode::Replace, false)[0],
            "--ignore-times"
        );
    }

    /// `sync --files-from` would consider only the listed names and prune
    /// everything else at the destination — an unannounced mass delete of
    /// files the user never selected. Callers gate Mirror out of batching;
    /// this asserts the driver refuses to emit it even if one slips through.
    #[test]
    fn batch_never_emits_sync_for_mirror() {
        let args = batch_args(ConflictMode::Mirror, false);
        assert!(
            !args.iter().any(|a| a == "sync"),
            "a batched Mirror must never become sync: {:?}",
            args
        );
        assert_eq!(args[0], "--ignore-times", "it degrades to Replace");
        assert!(args.contains(&"copy".to_string()));
    }

    /// The victim diff is the heart of conflict detection: paths present
    /// in the full pass but absent from the `--ignore-existing` pass were
    /// excluded *because the destination exists*, so they are exactly the
    /// files about to be overwritten.
    #[test]
    fn victims_are_the_difference_between_passes() {
        let full = vec![
            PathBuf::from("brandnew.txt"),
            PathBuf::from("differs.txt"),
            PathBuf::from("sub/also-differs.txt"),
        ];
        let additive = vec![PathBuf::from("brandnew.txt")];
        assert_eq!(
            victims(&full, &additive),
            vec![
                PathBuf::from("differs.txt"),
                PathBuf::from("sub/also-differs.txt")
            ]
        );
    }

    /// An all-new paste has no conflicts even though the full pass lists
    /// every file — the two passes agree, so the difference is empty.
    #[test]
    fn victims_empty_when_passes_agree() {
        let full = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        assert!(victims(&full, &full).is_empty());
    }

    /// Nothing transferable at all (everything identical) is also no
    /// conflict — an empty full pass can never produce victims.
    #[test]
    fn victims_empty_when_nothing_would_transfer() {
        assert!(victims(&[], &[PathBuf::from("a.txt")]).is_empty());
    }

    /// Every destination existing means every transfer is a conflict.
    #[test]
    fn victims_all_when_additive_pass_is_empty() {
        let full = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        assert_eq!(victims(&full, &[]), full);
    }

    /// `AddNewOnly` cannot overwrite anything, so `conflicts` must not
    /// even spawn rclone for it. Asserted via the mode reader that
    /// short-circuit depends on.
    #[test]
    fn mode_of_reads_transfer_ops_only() {
        let copy = Operation::Copy {
            sources: vec![NavPath::new("C:\\a").unwrap()],
            dest_dir: NavPath::new("C:\\b").unwrap(),
            mode: ConflictMode::AddNewOnly,
        };
        assert_eq!(mode_of(&copy), Some(ConflictMode::AddNewOnly));
        assert_eq!(
            mode_of(&Operation::Mkdir {
                dir: NavPath::new("C:\\a").unwrap()
            }),
            None
        );
    }

    /// The second dry-run pass is built by rewriting the mode in place;
    /// if `set_mode` missed a variant the diff would compare a pass
    /// against itself and report zero conflicts every time.
    #[test]
    fn set_mode_rewrites_copy_and_move() {
        let mut copy = Operation::Copy {
            sources: vec![NavPath::new("C:\\a").unwrap()],
            dest_dir: NavPath::new("C:\\b").unwrap(),
            mode: ConflictMode::Replace,
        };
        set_mode(&mut copy, ConflictMode::AddNewOnly);
        assert_eq!(mode_of(&copy), Some(ConflictMode::AddNewOnly));

        let mut mv = Operation::Move {
            sources: vec![NavPath::new("C:\\a").unwrap()],
            dest_dir: NavPath::new("C:\\b").unwrap(),
            mode: ConflictMode::Update,
        };
        set_mode(&mut mv, ConflictMode::AddNewOnly);
        assert_eq!(mode_of(&mv), Some(ConflictMode::AddNewOnly));
    }

    /// Regression guard for the bug this rewrite fixed: the dry-run parser
    /// keyed off `msg.contains("Would copy")`, which rclone stopped
    /// emitting, so conflict detection silently reported nothing. Parse
    /// the structured `skipped` field instead.
    #[test]
    fn dry_run_records_are_read_from_skipped_field() {
        let line = r#"{"level":"notice","msg":"Skipped copy as --dry-run is set (size 10)","skipped":"copy","object":"differs.txt"}"#;
        let ev: LogEvent = serde_json::from_str(line).unwrap();
        assert!(ev.is_dry_run("copy"));
        assert!(!ev.is_dry_run("delete"));
        assert_eq!(ev.object.as_deref(), Some("differs.txt"));

        let del = r#"{"level":"notice","msg":"Skipped delete as --dry-run is set (size 5)","skipped":"delete","object":"only-at-dest.txt"}"#;
        let ev: LogEvent = serde_json::from_str(del).unwrap();
        assert!(ev.is_dry_run("delete"));
        assert!(!ev.is_dry_run("copy"));

        // A stats record has no `skipped` field and must not be mistaken
        // for a dry-run entry.
        let stats = r#"{"level":"notice","msg":"Transferred: 11 B","stats":{"bytes":11}}"#;
        let ev: LogEvent = serde_json::from_str(stats).unwrap();
        assert!(!ev.is_dry_run("copy"));
    }
}
