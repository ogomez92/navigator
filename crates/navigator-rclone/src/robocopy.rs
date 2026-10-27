//! robocopy driver: the native engine for transfers whose endpoints are
//! both on a filesystem — local drives and UNC shares.
//!
//! It runs the same [`Operation`]s as the rclone driver and hands back the
//! same [`OpHandle`] / [`OpEvent`] stream, so everything above it
//! (progress, cancel, error reporting, conflict prompts) is shared. Which
//! engine runs an op is decided by [`RobocopyDriver::handles`]; anything it
//! declines — remotes, renames, deletes, a move within one volume — stays
//! on rclone.
//!
//! Everything here was measured against a real robocopy rather than read
//! off its help text, and several of the results are not guessable:
//!
//! * **Output is read from `/UNILOG`, never from stdout.** Piped stdout is
//!   OEM-codepage text — `｜` arrives as `|`, accents as mojibake — and
//!   `/UNICODE` only prefixes a BOM to that same text. The log file is
//!   genuine UTF-16 and can be read while robocopy is still writing it, so
//!   [`LogTail`] polls it.
//! * **Nothing is parsed by its words.** Class labels ("New File", "Older")
//!   and messages are localised; `/NC` drops the labels, and every line is
//!   recognised by shape: tab-separated size + full path (`/FP /BYTES`),
//!   a bare percentage, or `ERROR <code> (0x…)`, which robocopy keeps in
//!   English. Whether an entry is a transfer or a mirror prune is decided
//!   by which *root* its path sits under, not by its label.
//! * **Modes are flags; see [`mode_flags`].** Robocopy has a `Modified`
//!   class (same size and mtime, different change time) that only `/IM`
//!   copies, so Replace needs `/IS /IT /IM` to re-copy everything.
//! * **A move leaves identical files behind unless told not to.** `/MOV`
//!   skips a file that is already identical at the destination and keeps
//!   it in the source. A move that leaves copies behind is not a move, so
//!   moving modes add `/IS /IT /IM`. Files a mode *protects* (a newer
//!   destination under Update, any existing one under Add new only) do
//!   stay in the source — unlike rclone's `move --update`, which deletes
//!   the skipped source and with it the only copy of that version.
//! * **A killed robocopy leaves full-size garbage.** With `/MT` the
//!   destination is preallocated at full length and its mtime is only set
//!   when the copy completes, so a cancelled file looks complete and, under
//!   Update, *newer than its source* — skipped by every later paste. See
//!   [`remove_partials`].

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;

use navigator_core::{ConflictMode, NavPath};

use crate::error::{ErrorKind, MAX_DETAIL_LINES, RcloneError};
use crate::log::{LogEvent, LogLevel};
use crate::op::{
    ConflictReport, OpEvent, OpHandle, Operation, PreflightReport, Progress, mode_of, set_mode,
    victims,
};

/// Prefix of the `/UNILOG` files in `%TEMP%`. Named here, where they are
/// produced; the GUI's startup sweep imports it so the two cannot drift.
/// Files are `<prefix><pid>-<seq>.log`.
pub const LOG_PREFIX: &str = "navigator-robocopy-";

/// Default `/MT:n`. Same as the rclone driver's `--transfers` default,
/// and the GUI feeds both from one setting.
pub const DEFAULT_THREADS: u32 = 8;

static LOG_SEQ: AtomicU64 = AtomicU64::new(1);

/// How often the log is read and progress reported.
const POLL: Duration = Duration::from_millis(200);

/// Command-line budget for a batch's file filters. Windows caps a command
/// line at 32 767 UTF-16 units; this leaves room for both roots and every
/// flag. A batch longer than this runs as several invocations.
const ARGV_BUDGET: usize = 24_000;

/// Flags on every invocation.
///
/// `/R:1 /W:1` because robocopy's default is a million retries 30 s apart
/// — a locked file would hang the paste for a year. `/NJH /NJS` drop the
/// localised header and summary, `/NC` the localised class column;
/// `/BYTES /FP` make every entry a parseable size and full path. `/NDL`
/// hides directory lines except mirror prunes (`*EXTRA Dir`), which are
/// the ones wanted. `/XJ` skips junctions and symlinks, as rclone's local
/// backend does: following them can copy the target's whole tree, or loop
/// forever through a profile's legacy `Application Data` junctions.
const COMMON: [&str; 9] = [
    "/R:1", "/W:1", "/NJH", "/NJS", "/NC", "/BYTES", "/FP", "/NDL", "/XJ",
];

#[derive(Debug, Clone)]
pub struct RobocopyDriver {
    exe: PathBuf,
    threads: u32,
    /// Flags appended to every step — a saved job's `/XD` / `/XF`
    /// excludes. Empty for a paste.
    extra: Vec<OsString>,
    /// Copy symlinks and junctions as links (`/SL /SJ`) instead of
    /// skipping them (`/XJ`). A saved job's choice; a paste skips them.
    links: bool,
}

/// What a step needs to become a command line, besides itself.
#[derive(Debug, Clone)]
struct Runner {
    exe: PathBuf,
    extra: Vec<OsString>,
    links: bool,
}

impl Default for RobocopyDriver {
    fn default() -> Self {
        Self::from_path()
    }
}

impl RobocopyDriver {
    /// `robocopy.exe` from PATH — it ships in System32.
    pub fn from_path() -> Self {
        Self {
            exe: PathBuf::from("robocopy"),
            threads: DEFAULT_THREADS,
            extra: Vec::new(),
            links: false,
        }
    }

    /// Flags added to every invocation after the step's own and the
    /// common set: a job's `/XD name /XF name` excludes.
    pub fn with_extra_flags(mut self, flags: Vec<String>) -> Self {
        self.extra = flags.into_iter().map(OsString::from).collect();
        self
    }

    /// Store symlinks and junctions as links (`/SL /SJ`) rather than
    /// skipping them.
    pub fn with_links(mut self, links: bool) -> Self {
        self.links = links;
        self
    }

    fn runner(&self) -> Runner {
        Runner {
            exe: self.exe.clone(),
            extra: self.extra.clone(),
            links: self.links,
        }
    }

    /// `/MT:n` for runs with more than one file. Clamped to robocopy's own
    /// `1..=128`.
    pub fn with_threads(mut self, n: u32) -> Self {
        self.threads = n.clamp(1, 128);
        self
    }

    pub fn threads(&self) -> u32 {
        self.threads
    }

    /// Whether this engine should run `op`.
    ///
    /// Only plain transfers between two filesystem paths qualify. A move
    /// additionally has to cross volumes: within one volume rclone moves
    /// by renaming, which is instant, while robocopy always copies the
    /// bytes and then deletes the source.
    pub fn handles(op: &Operation) -> bool {
        match op {
            Operation::Copy {
                sources, dest_dir, ..
            } => single_ok(sources, dest_dir),
            Operation::Move {
                sources, dest_dir, ..
            } => single_ok(sources, dest_dir) && crosses_volume(&sources[0], dest_dir),
            Operation::CopyBatch {
                src_root,
                names,
                dest_dir,
                ..
            } => batch_ok(src_root, names, dest_dir),
            Operation::MoveBatch {
                src_root,
                names,
                dest_dir,
                ..
            } => batch_ok(src_root, names, dest_dir) && crosses_volume(src_root, dest_dir),
            Operation::SyncDir { src, dst, .. } => is_fs_path(src) && is_fs_path(dst),
            _ => false,
        }
    }

    /// List-only (`/L`) pass: what `op` would transfer and, for a mirror,
    /// prune. Paths are relative to the source and destination roots of
    /// the step that reported them — a file's name for a file copy, a path
    /// inside the tree for a folder — the same shape rclone's dry-run gives.
    pub fn preflight(&self, op: &Operation) -> std::io::Result<PreflightReport> {
        let steps = plan(op, source_is_dir(op));
        if steps.is_empty() {
            return Err(std::io::Error::other("robocopy cannot run this operation"));
        }
        let slot = Arc::new(Mutex::new(None));
        let mut report = PreflightReport::default();
        for step in &steps {
            let mut log = RunLog::new(step, true);
            let running = start(&self.runner(), step, true, 1, &slot)?;
            let (exit, stdout) = running.pump(
                &slot,
                |l| {
                    log.feed(l);
                },
                || {},
            );
            match exit {
                Exit::Code(c) if c < 8 => {}
                Exit::Code(c) => {
                    let err = std::mem::take(&mut log.errors).into_error(Some(c), &stdout);
                    return Err(std::io::Error::other(err.summary()));
                }
                Exit::Killed => return Err(std::io::Error::other("robocopy was stopped")),
            }
            report
                .would_transfer
                .extend(log.transfers.into_iter().map(|(rel, _)| PathBuf::from(rel)));
            report
                .would_delete
                .extend(log.deletes.into_iter().map(PathBuf::from));
        }
        Ok(report)
    }

    /// Existing destinations `op` would overwrite or (mirror) delete.
    /// Same two-pass diff as [`crate::RcloneDriver::conflicts`]: the chosen
    /// mode against the same op forced to Add new only.
    ///
    /// A move is detected as the equivalent copy. Detection asks what
    /// happens *at the destination*, and a moving op re-copies identical
    /// files (see the module docs) — which writes nothing that differs, and
    /// must not be listed as a file about to be destroyed.
    pub fn conflicts(&self, op: &Operation) -> std::io::Result<ConflictReport> {
        if mode_of(op) == Some(ConflictMode::AddNewOnly) {
            return Ok(ConflictReport::default());
        }
        let as_copy = copy_equivalent(op);
        let full = self.preflight(&as_copy)?;
        let mut additive = as_copy.clone();
        set_mode(&mut additive, ConflictMode::AddNewOnly);
        let additive = self.preflight(&additive)?;
        Ok(ConflictReport {
            overwrites: victims(&full.would_transfer, &additive.would_transfer),
            deletes: full.would_delete,
            missing_sources: Vec::new(),
        })
    }

    /// Start `op`. The first robocopy process is started before this
    /// returns, so a missing executable is an `Err` here — the caller can
    /// fall back to rclone — rather than a failure event later.
    ///
    /// Runs a list-only pass over every step first: robocopy announces no
    /// totals, and without them the job's percentage could not move until
    /// the end. The pass costs a directory walk, which also warms the
    /// cache for the real one.
    pub fn spawn(&self, op: Operation) -> std::io::Result<OpHandle> {
        let steps = plan(&op, source_is_dir(&op));
        if steps.is_empty() {
            return Err(std::io::Error::other("robocopy cannot run this operation"));
        }
        let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let runner = self.runner();
        let first = start(&runner, &steps[0], true, 1, &slot)?;
        tracing::info!(
            target: "robocopy.spawn",
            "robocopy job: {} step(s), first: {}",
            steps.len(),
            describe(&steps[0])
        );

        let (tx, rx) = unbounded::<OpEvent>();
        let threads = self.threads;
        let slot_w = Arc::clone(&slot);
        let cancelled_w = Arc::clone(&cancelled);
        thread::Builder::new()
            .name("navigator-robocopy".into())
            .spawn(move || run_job(runner, threads, steps, first, slot_w, cancelled_w, tx))?;
        Ok(OpHandle::from_parts(rx, slot, cancelled))
    }
}

// --- routing -----------------------------------------------------------

/// A path robocopy can be handed: a drive or UNC path, never one of the
/// navigator sentinels, a remote, a bare `\\host`, or a `\\?\` / `\\.\`
/// spelling (robocopy's handling of those is not something to bet a
/// paste on — rclone takes them).
fn is_fs_path(p: &NavPath) -> bool {
    if p.is_remote() || p.is_remotes_root() || p.is_this_pc() || p.is_unc_host_only() {
        return false;
    }
    let s = p.as_path().to_string_lossy();
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") || s.starts_with("//") {
        return false;
    }
    volume_key(p.as_path()).is_some()
}

fn single_ok(sources: &[NavPath], dest_dir: &NavPath) -> bool {
    // rclone's Copy/Move only ever reads `sources.first()`; holding the
    // robocopy path to exactly one keeps the two engines' meaning equal.
    let [src] = sources else {
        return false;
    };
    is_fs_path(src) && is_fs_path(dest_dir) && name_ok(src.file_name())
}

fn batch_ok(src_root: &NavPath, names: &[String], dest_dir: &NavPath) -> bool {
    !names.is_empty()
        && is_fs_path(src_root)
        && is_fs_path(dest_dir)
        && names.iter().all(|n| name_ok(n))
}

/// A name usable as a robocopy file filter. Filters are wildcards, so `*`
/// and `?` would match other files; Windows forbids both in a filename,
/// but a name carrying one is refused here rather than trusted. An empty
/// name is the dangerous one — see [`plan`].
fn name_ok(n: &str) -> bool {
    !n.is_empty() && !n.contains(['*', '?', '\\', '/', '"', '\n', '\r'])
}

/// The volume a path lives on: `C:` for a drive, `\\host\share`
/// (lower-cased) for a share. `None` for anything else.
pub fn volume_key(p: &Path) -> Option<String> {
    match p.components().next()? {
        Component::Prefix(pre) => match pre.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                Some(format!("{}:", (d as char).to_ascii_uppercase()))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(
                format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                )
                .to_lowercase(),
            ),
            _ => None,
        },
        _ => None,
    }
}

/// `true` only when both volumes are known and differ. An unknown volume
/// answers `false`, which keeps the move on rclone — correct either way,
/// and never slower than it used to be.
fn crosses_volume(a: &NavPath, b: &NavPath) -> bool {
    match (volume_key(a.as_path()), volume_key(b.as_path())) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
}

fn source_is_dir(op: &Operation) -> bool {
    match op {
        Operation::Copy { sources, .. } | Operation::Move { sources, .. } => {
            sources.first().is_some_and(|s| s.as_path().is_dir())
        }
        // Defined as a folder-to-folder copy; a file source is the
        // caller's mistake and robocopy reports it as such.
        Operation::SyncDir { .. } => true,
        _ => false,
    }
}

/// `op` as the copy that writes the same destination. Mirror cannot move
/// (see [`mode_flags`]), so a moving Mirror detects as the Replace it runs.
fn copy_equivalent(op: &Operation) -> Operation {
    let degrade = |m: ConflictMode| {
        if m == ConflictMode::Mirror {
            ConflictMode::Replace
        } else {
            m
        }
    };
    match op.clone() {
        Operation::Move {
            sources,
            dest_dir,
            mode,
        } => Operation::Copy {
            sources,
            dest_dir,
            mode: degrade(mode),
        },
        Operation::MoveBatch {
            src_root,
            list_file,
            names,
            dest_dir,
            mode,
        } => Operation::CopyBatch {
            src_root,
            list_file,
            names,
            dest_dir,
            mode: degrade(mode),
        },
        other => other,
    }
}

// --- planning ----------------------------------------------------------

/// One robocopy invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Source folder.
    pub src: PathBuf,
    /// Destination folder.
    pub dst: PathBuf,
    /// File filters. Empty means "the whole tree" and is only ever produced
    /// for a folder source — a file step with no filter would copy every
    /// file in its parent folder.
    pub files: Vec<String>,
    pub flags: Vec<&'static str>,
}

impl Step {
    /// Full argument list. `list_only` adds `/L`; `threads > 1` adds
    /// `/MT:n` (never to a listing, where it buys nothing).
    pub fn args(&self, list_only: bool, threads: u32, log: Option<&Path>) -> Vec<OsString> {
        self.args_with(list_only, threads, log, &[], false)
    }

    /// [`Self::args`] plus a driver's `extra` flags, with `/XJ` replaced
    /// by `/SL /SJ` when `links` is set.
    pub fn args_with(
        &self,
        list_only: bool,
        threads: u32,
        log: Option<&Path>,
        extra: &[OsString],
        links: bool,
    ) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![dir_arg(&self.src), dir_arg(&self.dst)];
        out.extend(self.files.iter().map(OsString::from));
        out.extend(self.flags.iter().map(OsString::from));
        for c in COMMON {
            if links && c == "/XJ" {
                out.push("/SL".into());
                out.push("/SJ".into());
            } else {
                out.push(c.into());
            }
        }
        out.extend(extra.iter().cloned());
        if list_only {
            out.push("/L".into());
        } else if threads > 1 {
            out.push(format!("/MT:{threads}").into());
        }
        if let Some(l) = log {
            let mut a = OsString::from("/UNILOG:");
            a.push(l.as_os_str());
            out.push(a);
        }
        out
    }
}

/// The robocopy flags that express `mode`.
///
/// | Mode | Flags | Why |
/// |---|---|---|
/// | Add new only | `/XC /XN /XO` | exclude every class that exists at the destination |
/// | Update | `/XO` | skip a source older than its destination; a same-time size change still copies, like `--update` |
/// | Replace | `/IS /IT /IM` | include Same, Tweaked and Modified — the three classes copied by nothing else |
/// | Mirror | `/MIR` | copy what differs and purge what the source lacks |
///
/// `tree` is a folder source (adds `/E`); `moving` adds `/MOVE` (tree) or
/// `/MOV` (files). Mirror only runs on a folder copy: a move has no
/// mirror, and `/PURGE` beside a file filter would empty the destination
/// of every other file — both degrade to Replace, as the rclone driver
/// does. Moving modes other than Add new only add `/IS /IT /IM` so an
/// already-identical file still leaves the source.
pub fn mode_flags(mode: ConflictMode, tree: bool, moving: bool) -> Vec<&'static str> {
    let mode = if mode == ConflictMode::Mirror && (moving || !tree) {
        ConflictMode::Replace
    } else {
        mode
    };
    let mut f: Vec<&'static str> = Vec::new();
    match mode {
        ConflictMode::AddNewOnly => f.extend(["/XC", "/XN", "/XO"]),
        ConflictMode::Update => f.push("/XO"),
        ConflictMode::Replace => f.extend(["/IS", "/IT", "/IM"]),
        ConflictMode::Mirror => f.push("/MIR"),
    }
    if mode != ConflictMode::Mirror {
        if tree {
            f.push("/E");
        }
        // Without this robocopy still *lists* every destination-only file
        // as `*EXTRA` — it just doesn't delete it. On a destination with
        // thousands of unrelated files that is thousands of lines read for
        // nothing, and each one looks exactly like a mirror prune.
        f.push("/XX");
    }
    if moving {
        f.push(if tree { "/MOVE" } else { "/MOV" });
        if mode != ConflictMode::AddNewOnly {
            for x in ["/IS", "/IT", "/IM"] {
                if !f.contains(&x) {
                    f.push(x);
                }
            }
        }
    }
    f
}

/// The invocations `op` turns into. Empty for anything this engine does
/// not run. Pure: `src_is_dir` is passed in.
pub fn plan(op: &Operation, src_is_dir: bool) -> Vec<Step> {
    match op {
        Operation::Copy {
            sources,
            dest_dir,
            mode,
        } => plan_single(sources, dest_dir, *mode, src_is_dir, false),
        Operation::Move {
            sources,
            dest_dir,
            mode,
        } => plan_single(sources, dest_dir, *mode, src_is_dir, true),
        Operation::CopyBatch {
            src_root,
            names,
            dest_dir,
            mode,
            ..
        } => plan_batch(src_root, names, dest_dir, *mode, false),
        Operation::MoveBatch {
            src_root,
            names,
            dest_dir,
            mode,
            ..
        } => plan_batch(src_root, names, dest_dir, *mode, true),
        // The folder itself is the destination: no name appended, no
        // filter — the whole tree, as a folder `Copy` step minus the join.
        Operation::SyncDir { src, dst, mode } => vec![Step {
            src: src.as_path().to_path_buf(),
            dst: dst.as_path().to_path_buf(),
            files: Vec::new(),
            flags: mode_flags(*mode, true, false),
        }],
        _ => Vec::new(),
    }
}

fn plan_single(
    sources: &[NavPath],
    dest_dir: &NavPath,
    mode: ConflictMode,
    is_dir: bool,
    moving: bool,
) -> Vec<Step> {
    let Some(src) = sources.first() else {
        return Vec::new();
    };
    let name = src.file_name();
    if !name_ok(name) {
        return Vec::new();
    }
    if is_dir {
        return vec![Step {
            src: src.as_path().to_path_buf(),
            dst: dest_dir.join(name).into_inner(),
            files: Vec::new(),
            flags: mode_flags(mode, true, moving),
        }];
    }
    let Some(parent) = src.as_path().parent() else {
        return Vec::new();
    };
    vec![Step {
        src: parent.to_path_buf(),
        dst: dest_dir.as_path().to_path_buf(),
        files: vec![name.to_string()],
        flags: mode_flags(mode, false, moving),
    }]
}

fn plan_batch(
    src_root: &NavPath,
    names: &[String],
    dest_dir: &NavPath,
    mode: ConflictMode,
    moving: bool,
) -> Vec<Step> {
    // No names means no filter, and no filter means every file in the
    // folder. Never.
    if names.is_empty() || !names.iter().all(|n| name_ok(n)) {
        return Vec::new();
    }
    let flags = mode_flags(mode, false, moving);
    let base = src_root.as_path().as_os_str().len() + dest_dir.as_path().as_os_str().len() + 400;
    let mut steps = Vec::new();
    let mut chunk: Vec<String> = Vec::new();
    let mut len = base;
    for n in names {
        let cost = n.len() + 3;
        if !chunk.is_empty() && len + cost > ARGV_BUDGET {
            steps.push(Step {
                src: src_root.as_path().to_path_buf(),
                dst: dest_dir.as_path().to_path_buf(),
                files: std::mem::take(&mut chunk),
                flags: flags.clone(),
            });
            len = base;
        }
        chunk.push(n.clone());
        len += cost;
    }
    steps.push(Step {
        src: src_root.as_path().to_path_buf(),
        dst: dest_dir.as_path().to_path_buf(),
        files: chunk,
        flags,
    });
    steps
}

/// A folder argument without a trailing separator — `"C:\x\"` is the
/// classic robocopy quoting trap — except a drive root, where `C:` alone
/// would mean "the current directory on C:".
fn dir_arg(p: &Path) -> OsString {
    let s = p.as_os_str().to_string_lossy();
    let trimmed = s.trim_end_matches(['\\', '/']);
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        return format!("{trimmed}\\").into();
    }
    if trimmed.is_empty() || trimmed.len() == s.len() {
        return p.as_os_str().to_owned();
    }
    trimmed.into()
}

/// The prefix robocopy prints in front of everything under `p` with `/FP`.
fn root_prefix(p: &Path) -> String {
    let mut s = dir_arg(p).to_string_lossy().into_owned();
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

fn describe(step: &Step) -> String {
    step.args(false, 1, None)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

// --- running -----------------------------------------------------------

enum Exit {
    Code(i32),
    /// The canceller took the child out of the slot and killed it.
    Killed,
}

struct Running {
    tail: LogTail,
    stdout: thread::JoinHandle<Vec<u8>>,
}

/// Start one invocation and park its child in `slot`, where the
/// [`crate::Canceller`] can reach it.
fn start(
    runner: &Runner,
    step: &Step,
    list_only: bool,
    threads: u32,
    slot: &Mutex<Option<Child>>,
) -> std::io::Result<Running> {
    let log = std::env::temp_dir().join(format!(
        "{}{}-{}.log",
        LOG_PREFIX,
        std::process::id(),
        LOG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&log);
    let mut cmd = Command::new(&runner.exe);
    cmd.args(step.args_with(list_only, threads, Some(&log), &runner.extra, runner.links));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    tracing::debug!(
        target: "robocopy.spawn",
        "robocopy {}",
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut child = cmd.spawn()?;
    crate::op::kill_child_with_process(&child);
    // Usage errors never reach the log — robocopy refuses before opening
    // it — so stdout is kept for the failure report. It is OEM text and
    // only ever shown as a last resort.
    let mut out = child.stdout.take();
    let stdout = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut buf);
        }
        buf
    });
    *slot.lock() = Some(child);
    Ok(Running {
        tail: LogTail::new(log),
        stdout,
    })
}

impl Running {
    /// Read the log until the child exits, feeding each line to `on_line`
    /// and calling `on_tick` once per poll. Returns how it ended and its
    /// stdout, which only matters for a run robocopy refused outright.
    ///
    /// Never holds the slot's lock across a wait: the canceller takes the
    /// same lock to kill the child, and a lock held for the life of the
    /// process would make Cancel block until the copy finished by itself.
    fn pump(
        mut self,
        slot: &Mutex<Option<Child>>,
        mut on_line: impl FnMut(&str),
        mut on_tick: impl FnMut(),
    ) -> (Exit, String) {
        let exit = loop {
            self.tail.poll(&mut on_line);
            let done = {
                let mut g = slot.lock();
                match g.as_mut() {
                    None => Some(Exit::Killed),
                    Some(c) => match c.try_wait() {
                        Ok(Some(s)) => {
                            g.take();
                            Some(Exit::Code(s.code().unwrap_or(-1)))
                        }
                        Ok(None) => None,
                        Err(_) => {
                            if let Some(mut c) = g.take() {
                                let _ = c.kill();
                            }
                            Some(Exit::Killed)
                        }
                    },
                }
            };
            if let Some(e) = done {
                break e;
            }
            on_tick();
            thread::sleep(POLL);
        };
        self.tail.finish(&mut on_line);
        self.tail.remove();
        let stdout = self
            .stdout
            .join()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (exit, stdout)
    }
}

/// The whole job, on its own thread: totals pass, then the real runs.
#[allow(clippy::too_many_arguments)]
fn run_job(
    runner: Runner,
    threads: u32,
    steps: Vec<Step>,
    first: Running,
    slot: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    tx: Sender<OpEvent>,
) {
    let is_cancelled = || cancelled.load(Ordering::Acquire);

    // Totals pass. Its errors are discarded: the real run hits the same
    // ones and reports them itself.
    let mut planned: Vec<Vec<(String, u64)>> = Vec::with_capacity(steps.len());
    let mut pending_first = Some(first);
    for step in &steps {
        if is_cancelled() {
            break;
        }
        let running = match pending_first.take() {
            Some(r) => r,
            None => match start(&runner, step, true, 1, &slot) {
                Ok(r) => r,
                Err(_) => {
                    planned.push(Vec::new());
                    continue;
                }
            },
        };
        let mut log = RunLog::new(step, true);
        let _ = running.pump(
            &slot,
            |l| {
                log.feed(l);
            },
            || {},
        );
        planned.push(log.transfers);
    }
    let bytes_total: u64 = planned.iter().flatten().map(|(_, s)| s).sum();
    let files_total: u64 = planned.iter().map(|p| p.len() as u64).sum();

    let mut tally = Tally::default();
    let mut errors = Errors::default();
    let mut worst: i32 = 0;
    let mut killed = false;
    let mut stdout_tail = String::new();
    let run_started = Instant::now();
    let mut last_sent: Option<(u64, u64, Option<String>)> = None;

    for (i, step) in steps.iter().enumerate() {
        if is_cancelled() {
            killed = true;
            break;
        }
        let step_files = planned.get(i).map_or(0, |p| p.len());
        // `/MT` writes a file's line only once it finishes, so a lone big
        // file would show no progress at all; one file gets nothing from
        // threads anyway.
        let t = if step_files > 1 { threads } else { 1 };
        let wall_start = SystemTime::now();
        let running = match start(&runner, step, false, t, &slot) {
            Ok(r) => r,
            Err(e) => {
                errors.raw(format!("could not start robocopy: {e}"));
                worst = worst.max(16);
                continue;
            }
        };
        let mut log = RunLog::new(step, false);
        // Both callbacks read the tally; only one runs at a time.
        let shared = std::cell::RefCell::new(&mut tally);
        let (exit, out) = running.pump(
            &slot,
            |l| {
                let mut t = shared.borrow_mut();
                if let Some(ev) = log.feed_into(l, &mut t, &mut errors) {
                    let _ = tx.send(OpEvent::Log(ev));
                }
            },
            || {
                let t = shared.borrow();
                let p = snapshot(&t, bytes_total, files_total, run_started);
                let key = (p.bytes_done, p.files_done, p.current.clone());
                if last_sent.as_ref() != Some(&key) {
                    let _ = tx.send(OpEvent::Progress(p));
                    last_sent = Some(key);
                }
            },
        );
        tally.complete();
        // A run shorter than one poll never ticked at all; the job's
        // percentage must still reach this step's end.
        let _ = tx.send(OpEvent::Progress(snapshot(
            &tally,
            bytes_total,
            files_total,
            run_started,
        )));
        match exit {
            Exit::Code(c) => {
                worst = worst.max(c);
                if c >= 8 {
                    stdout_tail = out;
                }
            }
            Exit::Killed => {
                killed = true;
                let removed =
                    remove_partials(step, planned.get(i).map_or(&[][..], |p| p), wall_start);
                if removed > 0 {
                    tracing::info!("robocopy cancelled; removed {removed} partial file(s)");
                }
                break;
            }
        }
    }

    let success = !killed && worst < 8;
    let exit_code = (!killed).then_some(worst);
    let error = (!success).then(|| {
        if killed {
            RcloneError {
                kind: ErrorKind::Other,
                message: "cancelled".into(),
                object: None,
                exit_code: None,
                detail: Vec::new(),
            }
        } else {
            errors.into_error(exit_code, &stdout_tail)
        }
    });
    let _ = tx.send(OpEvent::Done {
        success,
        exit_code,
        error,
    });
}

fn snapshot(t: &Tally, bytes_total: u64, files_total: u64, started: Instant) -> Progress {
    let bytes = t.bytes();
    let elapsed = started.elapsed().as_secs_f64();
    let speed = if elapsed > 0.5 {
        bytes as f64 / elapsed
    } else {
        0.0
    };
    Progress {
        bytes_done: bytes,
        bytes_total,
        files_done: t.files_done,
        files_total,
        speed_bps: speed,
        eta_secs: (speed > 0.0 && bytes_total > bytes)
            .then(|| ((bytes_total - bytes) as f64 / speed) as u64)
            .filter(|e| *e > 0),
        current: Some(t.current_name()).filter(|c| !c.is_empty()),
    }
}

/// Delete destination files a killed run left half-written.
///
/// robocopy sets a destination's mtime to the source's only when the copy
/// *finishes*, and under `/MT` it preallocates the file at full size. So a
/// file it was in the middle of — or had merely opened — is the source's
/// size with an mtime from this run, and nothing downstream can tell it
/// from a good copy: Update would skip it forever as "newer".
///
/// The rule therefore keys on time: a planned file whose destination was
/// written after the run began and whose mtime still differs from the
/// source's is one robocopy never finished. A destination it had not
/// reached keeps its old mtime and is left alone; a finished one carries
/// the source's mtime. An overwrite that was cut short has already
/// destroyed the old contents, so removing the remains loses nothing more.
fn remove_partials(step: &Step, planned: &[(String, u64)], run_started: SystemTime) -> usize {
    let since = run_started
        .checked_sub(Duration::from_secs(2))
        .unwrap_or(run_started);
    let mut removed = 0;
    for (rel, _) in planned {
        let dst = step.dst.join(rel);
        let src = step.src.join(rel);
        let Some(d) = std::fs::metadata(&dst).ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if d < since {
            continue;
        }
        let s = std::fs::metadata(&src).ok().and_then(|m| m.modified().ok());
        let finished = s.is_some_and(|s| {
            let gap = if d > s {
                d.duration_since(s)
            } else {
                s.duration_since(d)
            };
            gap.map(|g| g <= Duration::from_secs(2)).unwrap_or(false)
        });
        if !finished && std::fs::remove_file(&dst).is_ok() {
            removed += 1;
        }
    }
    removed
}

// --- reading the log ---------------------------------------------------

/// Incremental reader for a `/UNILOG` file that robocopy is still writing.
struct LogTail {
    path: PathBuf,
    file: Option<std::fs::File>,
    bytes: Vec<u8>,
    partial: String,
    started: bool,
}

impl LogTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            bytes: Vec::new(),
            partial: String::new(),
            started: false,
        }
    }

    /// Read whatever has been appended and emit every completed line.
    /// Lines end at `\r` as well as `\n`: robocopy rewrites its percentage
    /// in place with bare carriage returns.
    fn poll(&mut self, on_line: &mut impl FnMut(&str)) {
        if self.file.is_none() {
            // Created by robocopy shortly after it starts; until then
            // there is simply nothing to read yet.
            self.file = std::fs::File::open(&self.path).ok();
        }
        let Some(f) = self.file.as_mut() else {
            return;
        };
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() || buf.is_empty() {
            return;
        }
        self.bytes.extend_from_slice(&buf);
        let text = decode_utf16_prefix(&mut self.bytes);
        let text = if self.started {
            text
        } else {
            self.started = true;
            text.strip_prefix('\u{FEFF}')
                .map(str::to_owned)
                .unwrap_or(text)
        };
        self.partial.push_str(&text);
        while let Some(i) = self.partial.find(['\r', '\n']) {
            let line: String = self.partial[..i].to_string();
            self.partial.drain(..=i);
            on_line(&line);
        }
    }

    fn finish(&mut self, on_line: &mut impl FnMut(&str)) {
        self.poll(on_line);
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            on_line(&line);
        }
    }

    fn remove(&mut self) {
        self.file = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Decode as much of `bytes` as forms whole UTF-16LE characters, leaving
/// an odd trailing byte or a dangling high surrogate for the next read.
fn decode_utf16_prefix(bytes: &mut Vec<u8>) -> String {
    let mut even = bytes.len() & !1;
    if even >= 2 {
        let last = u16::from_le_bytes([bytes[even - 2], bytes[even - 1]]);
        if (0xD800..0xDC00).contains(&last) {
            even -= 2;
        }
    }
    let units: Vec<u16> = bytes[..even]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    bytes.drain(..even);
    char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect()
}

/// What one log line is, by shape alone.
#[derive(Debug, Clone, PartialEq)]
enum Line<'a> {
    /// `\t…\t<size>\t<full path>`. A trailing `\` marks a directory.
    Entry {
        size: i64,
        path: &'a str,
    },
    /// An in-place progress update, `  45.2%`.
    Percent(f64),
    /// `<date> <time> ERROR <code> (0x…) <action> <path>`. `text` is what
    /// follows the hex code. The next non-blank line is the message.
    Error {
        code: u32,
        text: &'a str,
    },
    Blank,
    Other(&'a str),
}

fn classify(line: &str) -> Line<'_> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Line::Blank;
    }
    if let Some(n) = trimmed.strip_suffix('%')
        && let Ok(p) = n.trim().parse::<f64>()
    {
        return Line::Percent(p);
    }
    if line.starts_with('\t') {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 3 {
            let path = fields[fields.len() - 1];
            if let Ok(size) = fields[fields.len() - 2].trim().parse::<i64>()
                && !path.is_empty()
            {
                return Line::Entry { size, path };
            }
        }
    }
    if let Some(i) = line.find(" ERROR ") {
        let rest = &line[i + 7..];
        let mut it = rest.splitn(3, ' ');
        if let (Some(code), Some(hex)) = (it.next(), it.next())
            && hex.starts_with("(0x")
            && let Ok(code) = code.parse::<u32>()
        {
            return Line::Error {
                code,
                text: it.next().unwrap_or("").trim(),
            };
        }
    }
    Line::Other(trimmed)
}

fn strip_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let head = path.get(..root.len())?;
    head.eq_ignore_ascii_case(root).then(|| &path[root.len()..])
}

/// Per-invocation reader state.
struct RunLog {
    src: String,
    dst: String,
    /// The step prunes the destination. Only then is an entry under the
    /// destination root a deletion; otherwise it is merely robocopy
    /// reporting an extra it will leave alone.
    purges: bool,
    /// Collect `transfers` / `deletes` (listing passes only; a real run
    /// of a million files has no use for the list).
    keep: bool,
    transfers: Vec<(String, u64)>,
    deletes: Vec<String>,
    /// Pruned folders, lower-cased with a trailing `\`, so the files
    /// inside one are not listed a second time.
    pruned_dirs: Vec<String>,
    errors: Errors,
    expect_message: bool,
}

impl RunLog {
    fn new(step: &Step, keep: bool) -> Self {
        Self {
            src: root_prefix(&step.src),
            dst: root_prefix(&step.dst),
            purges: step.flags.contains(&"/MIR"),
            keep,
            transfers: Vec::new(),
            deletes: Vec::new(),
            pruned_dirs: Vec::new(),
            errors: Errors::default(),
            expect_message: false,
        }
    }

    /// Feed a listing-pass line.
    fn feed(&mut self, line: &str) -> Option<LogEvent> {
        let mut tally = Tally::default();
        let mut errors = std::mem::take(&mut self.errors);
        let ev = self.feed_into(line, &mut tally, &mut errors);
        self.errors = errors;
        ev
    }

    /// Feed one line, updating the job's tally and errors. Returns a log
    /// record worth showing in the progress window, if any.
    fn feed_into(
        &mut self,
        line: &str,
        tally: &mut Tally,
        errors: &mut Errors,
    ) -> Option<LogEvent> {
        match classify(line) {
            Line::Blank => None,
            Line::Percent(p) => {
                tally.percent(p);
                None
            }
            Line::Error { code, text } => {
                let object = self.relative(text).map(str::to_owned);
                errors.begin(code, text, object);
                self.expect_message = true;
                None
            }
            Line::Entry { size, path } => {
                self.expect_message = false;
                errors.flush();
                self.entry(size, path, tally)
            }
            Line::Other(text) => {
                if self.expect_message {
                    self.expect_message = false;
                    let detail = errors.message(text);
                    return Some(event(LogLevel::Error, detail, None));
                }
                None
            }
        }
    }

    /// The part of `text` from the first root onwards, relative to it.
    fn relative<'a>(&self, text: &'a str) -> Option<&'a str> {
        let lower = text.to_lowercase();
        for root in self.roots_longest_first() {
            if let Some(i) = lower.find(&root.to_lowercase())
                && text.is_char_boundary(i)
            {
                return strip_root(&text[i..], root);
            }
        }
        None
    }

    fn roots_longest_first(&self) -> [&str; 2] {
        if self.dst.len() >= self.src.len() {
            [&self.dst, &self.src]
        } else {
            [&self.src, &self.dst]
        }
    }

    /// Which root `path` is under — `true` for the destination — and the
    /// path relative to it.
    fn locate<'a>(&self, path: &'a str) -> Option<(bool, &'a str)> {
        self.roots_longest_first().into_iter().find_map(|root| {
            strip_root(path, root).map(|rel| (std::ptr::eq(root, self.dst.as_str()), rel))
        })
    }

    fn entry(&mut self, size: i64, path: &str, tally: &mut Tally) -> Option<LogEvent> {
        let is_dir = path.ends_with('\\');
        let (under_dst, rel) = self.locate(path)?;
        if under_dst {
            if !self.purges {
                return None;
            }
            // Under the destination root of a mirror: a prune.
            let lower = rel.to_lowercase();
            if self
                .pruned_dirs
                .iter()
                .any(|d| lower.starts_with(d.as_str()))
            {
                return None;
            }
            let shown = rel.trim_end_matches('\\').to_string();
            if is_dir {
                self.pruned_dirs.push(lower);
            }
            if self.keep {
                self.deletes.push(shown.clone());
            }
            return Some(event(
                LogLevel::Info,
                format!("deleted {shown}"),
                Some(shown),
            ));
        }
        if is_dir {
            return None;
        }
        let size = size.max(0) as u64;
        tally.start(path, size);
        if self.keep {
            self.transfers.push((rel.to_string(), size));
        }
        Some(event(
            LogLevel::Info,
            rel.to_string(),
            Some(rel.to_string()),
        ))
    }
}

fn event(level: LogLevel, msg: String, object: Option<String>) -> LogEvent {
    LogEvent {
        level: Some(level),
        msg,
        source: None,
        object,
        object_type: None,
        skipped: None,
        stats: None,
    }
}

/// Job progress, folded from the file and percentage lines of every run.
#[derive(Debug, Default)]
struct Tally {
    files_done: u64,
    bytes_done: u64,
    current: Option<Current>,
}

#[derive(Debug)]
struct Current {
    path: String,
    size: u64,
    fraction: f64,
}

impl Tally {
    /// A file line. The same path again is robocopy retrying it — its
    /// progress restarts, but it is still one file.
    fn start(&mut self, path: &str, size: u64) {
        if let Some(c) = self.current.as_mut()
            && c.path == path
        {
            c.fraction = 0.0;
            return;
        }
        self.complete();
        self.current = Some(Current {
            path: path.to_string(),
            size,
            fraction: 0.0,
        });
    }

    fn percent(&mut self, pct: f64) {
        if let Some(c) = self.current.as_mut() {
            c.fraction = (pct / 100.0).clamp(0.0, 1.0);
        }
        if pct >= 100.0 {
            self.complete();
        }
    }

    /// Close out the current file. Also what a zero-byte file gets — it
    /// prints no percentage at all — and a failed one, which the user is
    /// no longer waiting on.
    fn complete(&mut self) {
        if let Some(c) = self.current.take() {
            self.files_done += 1;
            self.bytes_done += c.size;
        }
    }

    fn bytes(&self) -> u64 {
        self.bytes_done
            + self
                .current
                .as_ref()
                .map_or(0, |c| (c.size as f64 * c.fraction) as u64)
    }

    fn current_name(&self) -> String {
        self.current
            .as_ref()
            .and_then(|c| c.path.rsplit('\\').next())
            .unwrap_or_default()
            .to_string()
    }
}

/// Failures, distilled into one [`RcloneError`] the way
/// [`crate::ErrorCollector`] distils rclone's.
#[derive(Debug, Default)]
struct Errors {
    /// Code, message and object of the first failure.
    first: Option<(u32, String, Option<String>)>,
    /// Error line awaiting its message.
    pending: Option<(u32, String, Option<String>)>,
    detail: Vec<String>,
    seen: HashSet<String>,
    dropped: usize,
}

impl Errors {
    fn begin(&mut self, code: u32, text: &str, object: Option<String>) {
        self.flush();
        self.pending = Some((code, text.to_string(), object));
    }

    /// The message line for the pending error. Returns the detail line.
    fn message(&mut self, msg: &str) -> String {
        let Some((code, text, object)) = self.pending.take() else {
            return msg.to_string();
        };
        let line = format!("{text}: {msg}");
        if self.first.is_none() {
            self.first = Some((code, msg.to_string(), object));
        }
        self.push(line.clone());
        line
    }

    /// An error line whose message never came.
    fn flush(&mut self) {
        if let Some((code, text, object)) = self.pending.take() {
            if self.first.is_none() {
                self.first = Some((code, format!("error {code}"), object));
            }
            self.push(text);
        }
    }

    fn raw(&mut self, line: String) {
        if self.first.is_none() {
            self.first = Some((0, line.clone(), None));
        }
        self.push(line);
    }

    /// Retries print the same failure again; it is one failure.
    fn push(&mut self, line: String) {
        if !self.seen.insert(line.clone()) {
            return;
        }
        if self.detail.len() >= MAX_DETAIL_LINES {
            self.dropped += 1;
            return;
        }
        self.detail.push(line);
    }

    fn into_error(mut self, exit_code: Option<i32>, stdout: &str) -> RcloneError {
        self.flush();
        if self.dropped > 0 {
            self.detail.push(format!("… and {} more", self.dropped));
        }
        let (kind, message, object) = match self.first {
            Some((code, msg, obj)) => (kind_for(code), msg, obj),
            None => {
                // Nothing in the log: robocopy rejected the command line
                // before opening it. Its complaint is on stdout.
                let said: Vec<&str> = stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.contains("ERROR"))
                    .collect();
                let msg = if said.is_empty() {
                    match exit_code {
                        Some(c) => format!("robocopy failed (exit code {c})"),
                        None => "robocopy failed".into(),
                    }
                } else {
                    said.join(" ")
                };
                (ErrorKind::Other, msg, None)
            }
        };
        RcloneError {
            kind,
            message,
            object,
            exit_code,
            detail: self.detail,
        }
    }
}

/// Win32 error code → the kind the UI phrases. Messages are localised, so
/// the code is the only stable thing robocopy gives us.
fn kind_for(code: u32) -> ErrorKind {
    match code {
        2 | 3 | 15 | 161 => ErrorKind::NotFound,
        5 | 19 => ErrorKind::PermissionDenied,
        39 | 112 => ErrorKind::OutOfSpace,
        80 | 183 => ErrorKind::AlreadyExists,
        145 => ErrorKind::NotEmpty,
        267 => ErrorKind::WrongKind,
        51 | 53 | 59 | 64 | 67 | 1231 | 1232 => ErrorKind::Network,
        86 | 1219 | 1326 => ErrorKind::Auth,
        _ => ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(s: &str) -> NavPath {
        NavPath::new(s).unwrap()
    }

    fn copy(src: &str, dst: &str, mode: ConflictMode) -> Operation {
        Operation::Copy {
            sources: vec![np(src)],
            dest_dir: np(dst),
            mode,
        }
    }

    fn mv(src: &str, dst: &str, mode: ConflictMode) -> Operation {
        Operation::Move {
            sources: vec![np(src)],
            dest_dir: np(dst),
            mode,
        }
    }

    fn batch(cut: bool, names: &[&str], dst: &str) -> Operation {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        if cut {
            Operation::MoveBatch {
                src_root: np("C:\\src"),
                list_file: PathBuf::from("C:\\tmp\\l.txt"),
                names,
                dest_dir: np(dst),
                mode: ConflictMode::Update,
            }
        } else {
            Operation::CopyBatch {
                src_root: np("C:\\src"),
                list_file: PathBuf::from("C:\\tmp\\l.txt"),
                names,
                dest_dir: np(dst),
                mode: ConflictMode::Update,
            }
        }
    }

    /// The engine split. Remotes and the ops that are not transfers stay
    /// on rclone; so does a move within one volume, which rclone does by
    /// renaming.
    #[test]
    fn routes_only_filesystem_transfers() {
        use ConflictMode::Update;
        assert!(RobocopyDriver::handles(&copy("C:\\a\\x", "D:\\b", Update)));
        assert!(RobocopyDriver::handles(&copy("C:\\a\\x", "C:\\b", Update)));
        assert!(RobocopyDriver::handles(&copy(
            "\\\\nas\\media\\x",
            "C:\\b",
            Update
        )));
        assert!(RobocopyDriver::handles(&mv("C:\\a\\x", "D:\\b", Update)));
        assert!(RobocopyDriver::handles(&mv(
            "C:\\a\\x",
            "\\\\nas\\media",
            Update
        )));
        assert!(
            !RobocopyDriver::handles(&mv("C:\\a\\x", "C:\\b", Update)),
            "same-volume move is a rename — rclone"
        );
        assert!(!RobocopyDriver::handles(&mv(
            "\\\\nas\\media\\x",
            "\\\\NAS\\Media\\y",
            Update
        )));
        assert!(!RobocopyDriver::handles(&Operation::Copy {
            sources: vec![np("C:\\a\\x")],
            dest_dir: NavPath::remote("gdrive", "x"),
            mode: Update,
        }));
        assert!(!RobocopyDriver::handles(&Operation::Copy {
            sources: vec![NavPath::remote("gdrive", "x")],
            dest_dir: np("C:\\b"),
            mode: Update,
        }));
        assert!(!RobocopyDriver::handles(&Operation::Rename {
            src: np("C:\\a"),
            dst: np("D:\\a"),
        }));
        assert!(!RobocopyDriver::handles(&Operation::CopyTo {
            src: np("C:\\a"),
            dst: np("D:\\b"),
        }));
        assert!(RobocopyDriver::handles(&batch(false, &["a", "b"], "C:\\d")));
        assert!(!RobocopyDriver::handles(&batch(true, &["a", "b"], "C:\\d")));
        assert!(RobocopyDriver::handles(&batch(true, &["a", "b"], "D:\\d")));
    }

    /// A batch with no names would run robocopy with no file filter, which
    /// copies (or moves!) every file in the source folder.
    #[test]
    fn sync_dir_runs_between_filesystem_paths_as_a_whole_tree() {
        let op = Operation::SyncDir {
            src: np(r"D:\"),
            dst: np(r"B:\data"),
            mode: ConflictMode::Mirror,
        };
        assert!(RobocopyDriver::handles(&op));
        let steps = plan(&op, true);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].src, PathBuf::from(r"D:\"));
        assert_eq!(steps[0].dst, PathBuf::from(r"B:\data"));
        assert!(steps[0].files.is_empty());
        assert!(steps[0].flags.contains(&"/MIR"));
        // Remotes stay on rclone.
        let remote = Operation::SyncDir {
            src: np(r"D:\x"),
            dst: NavPath::new("sb:/home/vol").unwrap(),
            mode: ConflictMode::Update,
        };
        assert!(!RobocopyDriver::handles(&remote));
    }

    #[test]
    fn extra_flags_and_links_reach_the_command_line() {
        let s = step("C:\\s", "D:\\d");
        let extra = [OsString::from("/XD"), OsString::from("node_modules")];
        let args: Vec<String> = s
            .args_with(false, 1, None, &extra, true)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a == "/XJ"), "{args:?}");
        assert!(args.iter().any(|a| a == "/SL") && args.iter().any(|a| a == "/SJ"));
        let i = args.iter().position(|a| a == "/XD").unwrap();
        assert_eq!(args[i + 1], "node_modules");
        let plain: Vec<String> = s
            .args(false, 1, None)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(plain.iter().any(|a| a == "/XJ"));
    }

    #[test]
    fn a_batch_without_names_is_never_robocopy() {
        assert!(!RobocopyDriver::handles(&batch(false, &[], "D:\\d")));
        assert!(plan(&batch(false, &[], "D:\\d"), false).is_empty());
    }

    #[test]
    fn wildcard_names_are_refused() {
        assert!(!RobocopyDriver::handles(&batch(
            false,
            &["a", "*.txt"],
            "D:\\d"
        )));
        assert!(!RobocopyDriver::handles(&copy(
            "C:\\a\\x?",
            "D:\\b",
            ConflictMode::Update
        )));
    }

    #[test]
    fn a_drive_root_source_is_refused() {
        assert!(!RobocopyDriver::handles(&copy(
            "C:\\",
            "D:\\b",
            ConflictMode::Update
        )));
    }

    #[test]
    fn volume_keys() {
        assert_eq!(volume_key(Path::new("c:\\x")).as_deref(), Some("C:"));
        assert_eq!(
            volume_key(Path::new("\\\\NAS\\Media\\x")).as_deref(),
            Some("\\\\nas\\media")
        );
        assert_eq!(volume_key(Path::new("relative")), None);
    }

    /// The mode table, literally — it is the whole contract with robocopy.
    #[test]
    fn copy_modes_map_to_flags() {
        use ConflictMode::*;
        assert_eq!(
            mode_flags(AddNewOnly, true, false),
            ["/XC", "/XN", "/XO", "/E", "/XX"]
        );
        assert_eq!(mode_flags(Update, true, false), ["/XO", "/E", "/XX"]);
        assert_eq!(
            mode_flags(Replace, true, false),
            ["/IS", "/IT", "/IM", "/E", "/XX"]
        );
        assert_eq!(mode_flags(Mirror, true, false), ["/MIR"]);
        assert_eq!(mode_flags(Update, false, false), ["/XO", "/XX"]);
    }

    /// `/PURGE` (inside `/MIR`) beside a file filter deletes every other
    /// file in the destination folder, and a move has no mirror. Both
    /// degrade to Replace.
    #[test]
    fn mirror_only_ever_purges_a_folder_copy() {
        use ConflictMode::*;
        for (tree, moving) in [(false, false), (true, true), (false, true)] {
            let f = mode_flags(Mirror, tree, moving);
            assert!(
                !f.contains(&"/MIR") && !f.contains(&"/PURGE"),
                "tree={tree} moving={moving}: {f:?}"
            );
            assert!(f.contains(&"/IS"));
        }
        for step in plan(&batch(false, &["a", "b"], "D:\\d"), false) {
            assert!(!step.flags.contains(&"/MIR"));
        }
        let mut op = batch(false, &["a", "b"], "D:\\d");
        set_mode(&mut op, Mirror);
        for step in plan(&op, false) {
            assert!(!step.flags.contains(&"/MIR"));
        }
        let file_mirror = plan(&copy("C:\\a\\x.txt", "D:\\b", Mirror), false);
        assert!(!file_mirror[0].flags.contains(&"/MIR"));
    }

    /// `/MOV` keeps a file that is already identical at the destination in
    /// the source. Moving modes therefore re-include those classes —
    /// except Add new only, whose whole point is to leave existing
    /// destinations (and so their sources) alone.
    #[test]
    fn moving_modes_include_identical_files() {
        use ConflictMode::*;
        assert_eq!(
            mode_flags(Update, true, true),
            ["/XO", "/E", "/XX", "/MOVE", "/IS", "/IT", "/IM"]
        );
        assert_eq!(
            mode_flags(Update, false, true),
            ["/XO", "/XX", "/MOV", "/IS", "/IT", "/IM"]
        );
        assert_eq!(
            mode_flags(AddNewOnly, false, true),
            ["/XC", "/XN", "/XO", "/XX", "/MOV"]
        );
        assert_eq!(
            mode_flags(Replace, true, true),
            ["/IS", "/IT", "/IM", "/E", "/XX", "/MOVE"]
        );
    }

    #[test]
    fn a_folder_copies_into_its_own_name() {
        let steps = plan(&copy("C:\\a\\photos", "D:\\b", ConflictMode::Update), true);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].src, PathBuf::from("C:\\a\\photos"));
        assert_eq!(steps[0].dst, PathBuf::from("D:\\b\\photos"));
        assert!(steps[0].files.is_empty());
    }

    #[test]
    fn a_file_copies_by_filter_from_its_parent() {
        let steps = plan(&copy("C:\\a\\x.txt", "D:\\b", ConflictMode::Update), false);
        assert_eq!(steps[0].src, PathBuf::from("C:\\a"));
        assert_eq!(steps[0].dst, PathBuf::from("D:\\b"));
        assert_eq!(steps[0].files, ["x.txt"]);
        assert!(!steps[0].flags.contains(&"/E"));
    }

    /// No step may carry an empty filter unless it is a folder copy.
    #[test]
    fn only_folder_steps_lack_a_filter() {
        for (op, dir) in [
            (copy("C:\\a\\x", "D:\\b", ConflictMode::Update), false),
            (batch(false, &["a", "b", "c"], "D:\\d"), false),
            (batch(true, &["a"], "D:\\d"), false),
        ] {
            for s in plan(&op, dir) {
                assert!(!s.files.is_empty(), "{op:?}");
            }
        }
    }

    /// A batch too long for one command line splits; nothing is lost and
    /// every chunk still has a filter.
    #[test]
    fn long_batches_split_under_the_command_line_limit() {
        let names: Vec<String> = (0..3000)
            .map(|i| format!("file number {i:05}.txt"))
            .collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let steps = plan(&batch(false, &refs, "D:\\d"), false);
        assert!(steps.len() > 1);
        let all: Vec<&String> = steps.iter().flat_map(|s| &s.files).collect();
        assert_eq!(all.len(), names.len());
        for s in &steps {
            assert!(!s.files.is_empty());
            let len: usize = s
                .args(false, 8, Some(Path::new("C:\\t\\x.log")))
                .iter()
                .map(|a| a.len() + 3)
                .sum();
            assert!(len < 32_000, "{len}");
        }
    }

    #[test]
    fn folder_arguments_drop_the_trailing_separator_except_at_a_drive_root() {
        assert_eq!(dir_arg(Path::new("C:\\a\\")), OsString::from("C:\\a"));
        assert_eq!(dir_arg(Path::new("C:\\a")), OsString::from("C:\\a"));
        assert_eq!(dir_arg(Path::new("C:\\")), OsString::from("C:\\"));
        assert_eq!(
            dir_arg(Path::new("\\\\nas\\share\\")),
            OsString::from("\\\\nas\\share")
        );
        assert_eq!(root_prefix(Path::new("C:\\")), "C:\\");
        assert_eq!(root_prefix(Path::new("C:\\a")), "C:\\a\\");
    }

    #[test]
    fn listing_never_multithreads() {
        let s = &plan(&copy("C:\\a\\x", "D:\\b", ConflictMode::Update), true)[0];
        let listed: Vec<String> = s
            .args(true, 8, None)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(listed.contains(&"/L".to_string()));
        assert!(!listed.iter().any(|a| a.starts_with("/MT")));
        let real: Vec<String> = s
            .args(false, 8, None)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(real.contains(&"/MT:8".to_string()));
        assert!(!real.contains(&"/L".to_string()));
    }

    // Verbatim shapes from a Spanish-locale robocopy, /NC /FP /BYTES.

    #[test]
    fn classifies_lines_by_shape() {
        assert_eq!(
            classify("\t  \t\t   12345\tC:\\s\\a b.txt"),
            Line::Entry {
                size: 12345,
                path: "C:\\s\\a b.txt"
            }
        );
        assert_eq!(
            classify("\t      -1\tD:\\d\\extra_dir\\"),
            Line::Entry {
                size: -1,
                path: "D:\\d\\extra_dir\\"
            }
        );
        assert_eq!(classify("  45.2%  "), Line::Percent(45.2));
        assert_eq!(classify("100%  "), Line::Percent(100.0));
        assert_eq!(
            classify("2026/10/14 14:44:48 ERROR 32 (0x00000020) Copiando archivo C:\\s\\l.txt"),
            Line::Error {
                code: 32,
                text: "Copiando archivo C:\\s\\l.txt"
            }
        );
        assert_eq!(classify(""), Line::Blank);
        assert!(matches!(
            classify("Esperando 1 segundos... Reintentando..."),
            Line::Other(_)
        ));
    }

    fn step(src: &str, dst: &str) -> Step {
        Step {
            src: PathBuf::from(src),
            dst: PathBuf::from(dst),
            files: Vec::new(),
            flags: vec!["/MIR"],
        }
    }

    /// robocopy reports a destination-only file even when it will not
    /// delete it. Outside a mirror that is not a deletion, and must never
    /// reach a "this will be deleted" dialog.
    #[test]
    fn extras_are_only_deletions_in_a_mirror() {
        let mut s = step("C:\\s", "D:\\d");
        s.flags = vec!["/XO", "/E", "/XX"];
        let mut log = RunLog::new(&s, true);
        log.feed("\t  \t\t       2\tD:\\d\\extra.txt");
        log.feed("\t  \t\t       6\tC:\\s\\a.txt");
        assert!(log.deletes.is_empty());
        assert_eq!(log.transfers.len(), 1);
    }

    /// Transfers and prunes are told apart by root, not by the localised
    /// class label; files inside a pruned folder are not listed twice.
    #[test]
    fn listing_splits_transfers_from_prunes_by_root() {
        let mut log = RunLog::new(&step("C:\\s", "D:\\d"), true);
        for l in [
            "\t      -1\tD:\\d\\extra_dir\\",
            "\t  \t\t       2\tD:\\d\\extra_dir\\e.txt",
            "\t  \t\t       2\tD:\\d\\extra.txt",
            "\t  \t\t       6\tC:\\s\\a.txt",
            "\t  \t\t       8\tC:\\s\\sub\\ñandú ｜x.txt",
        ] {
            log.feed(l);
        }
        assert_eq!(
            log.transfers,
            vec![
                ("a.txt".to_string(), 6),
                ("sub\\ñandú ｜x.txt".to_string(), 8)
            ]
        );
        assert_eq!(log.deletes, vec!["extra_dir", "extra.txt"]);
    }

    /// A destination inside the source (or the reverse) must still sort
    /// each path under the root it really belongs to.
    #[test]
    fn nested_roots_match_the_longest_first() {
        let mut log = RunLog::new(&step("C:\\s", "C:\\s\\backup"), true);
        log.feed("\t  \t\t       1\tC:\\s\\backup\\old.txt");
        log.feed("\t  \t\t       1\tC:\\s\\a.txt");
        assert_eq!(log.deletes, vec!["old.txt"]);
        assert_eq!(log.transfers, vec![("a.txt".to_string(), 1)]);
    }

    /// The real retry sequence for a locked file: the file line repeats,
    /// each attempt logs the same error, and the message sits on the line
    /// after a blank one. One failure, one file, and the message is the
    /// localised sentence robocopy gave.
    #[test]
    fn a_retried_failure_is_one_error_and_one_file() {
        let mut log = RunLog::new(&step("C:\\s", "D:\\d"), false);
        let mut tally = Tally::default();
        let mut errors = Errors::default();
        for l in [
            "\t  \t\t       3\tC:\\s\\locked.txt",
            "2026/10/14 14:44:48 ERROR 32 (0x00000020) Copiando archivo C:\\s\\locked.txt",
            "El proceso no tiene acceso al archivo porque está siendo utilizado por otro proceso.",
            "",
            "Esperando 1 segundos... Reintentando...",
            "\t  \t\t       3\tC:\\s\\locked.txt",
            "2026/10/14 14:44:49 ERROR 32 (0x00000020) Copiando archivo C:\\s\\locked.txt",
            "",
            "El proceso no tiene acceso al archivo porque está siendo utilizado por otro proceso.",
            "ERROR: SE SUPERÓ EL LÍMITE DE REINTENTOS.",
            "\t  \t\t       3\tC:\\s\\ok.txt",
            "100%  ",
        ] {
            log.feed_into(l, &mut tally, &mut errors);
        }
        tally.complete();
        assert_eq!(tally.files_done, 2);
        assert_eq!(tally.bytes(), 6);
        let e = errors.into_error(Some(9), "");
        assert_eq!(e.object.as_deref(), Some("locked.txt"));
        assert!(e.message.starts_with("El proceso no tiene acceso"));
        assert_eq!(e.detail.len(), 1, "{:?}", e.detail);
        assert_eq!(e.kind, ErrorKind::Other);
    }

    #[test]
    fn error_codes_classify() {
        assert_eq!(kind_for(5), ErrorKind::PermissionDenied);
        assert_eq!(kind_for(2), ErrorKind::NotFound);
        assert_eq!(kind_for(112), ErrorKind::OutOfSpace);
    }

    /// A rejected command line never opens the log; stdout is all there is.
    #[test]
    fn a_usage_error_reports_robocopy_own_words() {
        let e = Errors::default().into_error(
            Some(16),
            "\r\n ERROR : Invalid Parameter #3 : \"/BOGUS\"\r\n\r\n Simple Usage :: ROBOCOPY",
        );
        assert_eq!(e.message, "ERROR : Invalid Parameter #3 : \"/BOGUS\"");
    }

    #[test]
    fn progress_counts_partial_bytes_and_zero_byte_files() {
        let mut t = Tally::default();
        t.start("C:\\s\\big", 1000);
        t.percent(40.0);
        assert_eq!(t.bytes(), 400);
        assert_eq!(t.current_name(), "big");
        t.percent(100.0);
        t.start("C:\\s\\empty", 0);
        t.start("C:\\s\\next", 10);
        assert_eq!(
            t.files_done, 2,
            "the empty file printed no 100% but is done"
        );
        assert_eq!(t.bytes(), 1000);
    }

    #[test]
    fn utf16_decoding_keeps_split_characters_for_the_next_read() {
        // "a😀" = 61 00 | 3D D8 00 DE — split inside the surrogate pair,
        // then inside a code unit.
        let mut b = vec![0x61, 0x00, 0x3D, 0xD8];
        assert_eq!(decode_utf16_prefix(&mut b), "a");
        b.push(0x00);
        assert_eq!(decode_utf16_prefix(&mut b), "");
        b.push(0xDE);
        assert_eq!(decode_utf16_prefix(&mut b), "😀");
        assert!(b.is_empty());
    }

    #[test]
    fn moves_detect_as_the_copy_they_write() {
        let op = mv("C:\\a\\x", "D:\\b", ConflictMode::Mirror);
        match copy_equivalent(&op) {
            Operation::Copy { mode, .. } => assert_eq!(mode, ConflictMode::Replace),
            other => panic!("{other:?}"),
        }
    }
}
