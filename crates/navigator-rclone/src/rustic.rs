//! rustic backup repositories: list snapshots, browse them, restore out of
//! them — and, for a saved job ([`Operation::RusticBackup`]), take a
//! snapshot. Browsing never writes; the one writer is the backup, which
//! only adds a snapshot and runs only when the user runs a job.
//!
//! Everything is a `rustic` child process, like rclone and robocopy, and
//! every fact below was measured against rustic 0.11.4:
//!
//!   * **Listing is `cat tree <snap>:<path>`**, one directory per call. It
//!     answers with the raw tree blob as JSON — name, type, size, mtime,
//!     ctime per node. `ls --json` prints bare names (no size, no type) and
//!     `ls -l` is a localised text table with minute-precision UTC dates,
//!     so neither is usable. ~0.3 s per call on a local repo, almost all of
//!     it opening the repository, so listings are cached: a snapshot never
//!     changes, so its listings never go stale.
//!   * **A drive path is not a path to rustic.** It reads `<word>:` as a
//!     backend, so `C:/repo` is "backend `C` not supported" and a local
//!     path passed where a snapshot is accepted (`diff`'s second argument)
//!     parses as snapshot `C`. Repos are normalised to backslashes (see
//!     `navigator_core::path::normalize_rustic_repo`), and `diff` runs in
//!     the destination's parent with a `.\name` argument.
//!   * **Restoring over a type change crashes rustic** (a local folder
//!     where the snapshot has a file, or the reverse). Callers exclude the
//!     `T` entries [`RusticDriver::diff`] reports; see [`DiffEntry`].
//!   * **No password source, no terminal** → `IO error: not a terminal`.
//!     A wrong one → "No suitable key found" (error C002). Either way we
//!     ask the user through the installed [`Prompter`] and retry.
//!   * **`RUSTIC_PASSWORD` and `RUSTIC_PASSWORD_FILE` together are a usage
//!     error**, so whichever one we set, the others are cleared from the
//!     child's environment. A password from the environment *does*
//!     override one from a profile's `[repository]` section.
//!   * `restic` reads these repositories too, but restoring a
//!     rustic-written Windows snapshot with it marks every file and folder
//!     read-only: rustic records mode 0 for Windows files and restic
//!     applies it. That is why this driver is rustic's, not restic's.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use crossbeam_channel::unbounded;
use parking_lot::Mutex;
use serde::Deserialize;

use navigator_core::path::RusticParts;
use navigator_core::{Entry, EntryKind, FileTime, NavPath};

use crate::error::{ErrorKind, RcloneError};
use crate::log::{LogEvent, LogLevel};
use crate::op::{OpEvent, OpHandle, Operation, Progress, kill_child_with_process};

/// Temp-file prefix for the `--glob-file` a restore with excludes writes.
/// Exported for `navigator-gui`'s startup sweep.
pub const GLOB_PREFIX: &str = "navigator-rustic-globs-";

static GLOB_SEQ: AtomicU64 = AtomicU64::new(0);

/// How navigator runs rustic. Read from `[rustic]` in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RusticSettings {
    /// The binary, `rustic` on `PATH` by default.
    pub exe: PathBuf,
    /// rustic config profile (`-P`). Empty = rustic's own default, which
    /// reads `rustic.toml` if there is one.
    pub profile: String,
    /// Password file handed to every repository (`RUSTIC_PASSWORD_FILE`).
    /// Empty = rely on the profile / environment, then ask.
    pub password_file: String,
}

impl Default for RusticSettings {
    fn default() -> Self {
        Self {
            exe: PathBuf::from("rustic"),
            profile: String::new(),
            password_file: String::new(),
        }
    }
}

/// Asks the user for a repository's password. `retry` is `true` when the
/// last attempt was refused, so the dialog can say so. `None` = cancelled.
///
/// Called from whichever thread runs rustic — the scan worker, a paste
/// worker — and expected to block until the user answers.
pub type Prompter = Arc<dyn Fn(&str, bool) -> Option<String> + Send + Sync>;

/// State shared by every clone of a [`RusticDriver`]: what the user typed,
/// what we have listed, and how to ask. Per process, never persisted — a
/// password typed into navigator lives exactly as long as navigator.
#[derive(Default)]
struct Session {
    settings: Mutex<RusticSettings>,
    passwords: Mutex<HashMap<String, String>>,
    /// Repositories some command has already opened this session, so a
    /// restore can skip its access check.
    opened: Mutex<HashSet<String>>,
    snapshots: Mutex<HashMap<String, Vec<Snapshot>>>,
    /// `(repo, snapshot id, sub)` → that directory's nodes.
    trees: Mutex<HashMap<TreeKey, Arc<Vec<Node>>>>,
    prompter: Mutex<Option<Prompter>>,
    /// Held while a password dialog is up, so two workers that hit the
    /// same locked repository ask once rather than stacking two dialogs.
    asking: Mutex<()>,
}

/// `(repo, snapshot id, path in the snapshot)`.
type TreeKey = (String, String, String);

/// The rustic engine. Cheap to clone; clones share the session.
#[derive(Clone, Default)]
pub struct RusticDriver {
    session: Arc<Session>,
}

impl std::fmt::Debug for RusticDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RusticDriver")
            .field("settings", &*self.session.settings.lock())
            .finish()
    }
}

/// Why a rustic command failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RusticError {
    /// The repository needed a password and the user cancelled the prompt
    /// (or there was nobody to ask).
    PasswordCancelled,
    /// The path is not in the snapshot, or the snapshot is not in the repo.
    NotFound(String),
    /// rustic could not be started at all.
    Spawn(String),
    /// Nothing at the repository location — `rustic init` has not been
    /// run there. Carries rustic's sentence, which names the location.
    NoRepository(String),
    /// Anything else, as rustic phrased it.
    Failed {
        message: String,
        exit_code: Option<i32>,
    },
}

/// Spoken / shown when a password prompt is dismissed. The listing error
/// handler compares against it so a cancel is acknowledged, not reported.
pub const PASSWORD_CANCELLED: &str = "no password given";

impl std::fmt::Display for RusticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RusticError::PasswordCancelled => f.write_str(PASSWORD_CANCELLED),
            RusticError::NotFound(m) => write!(f, "not found: {m}"),
            RusticError::Spawn(m) => write!(f, "could not start rustic: {m}"),
            RusticError::NoRepository(m) => f.write_str(m),
            RusticError::Failed { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for RusticError {}

impl RusticError {
    /// The same failure in the shape the job reporting already speaks.
    pub fn to_rclone_error(&self) -> RcloneError {
        let (kind, message, exit_code) = match self {
            RusticError::PasswordCancelled => (ErrorKind::Auth, self.to_string(), None),
            RusticError::NotFound(m) => (ErrorKind::NotFound, m.clone(), None),
            RusticError::Spawn(_) => (ErrorKind::Other, self.to_string(), None),
            RusticError::NoRepository(m) => (ErrorKind::NotFound, m.clone(), None),
            RusticError::Failed { message, exit_code } => {
                (ErrorKind::Other, message.clone(), *exit_code)
            }
        };
        RcloneError {
            kind,
            message: message.clone(),
            object: None,
            exit_code,
            detail: vec![message],
        }
    }
}

/// One snapshot, from `rustic snapshots --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub time: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub program_version: String,
    #[serde(default)]
    pub summary: Option<SnapshotSummary>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SnapshotSummary {
    #[serde(default)]
    pub total_files_processed: u64,
    #[serde(default)]
    pub total_dirs_processed: u64,
    #[serde(default)]
    pub total_bytes_processed: u64,
    #[serde(default)]
    pub data_added: u64,
}

/// One node of a snapshot tree, from `rustic cat tree`.
#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: Option<String>,
    #[serde(default)]
    pub ctime: Option<String>,
    #[serde(default)]
    pub linktarget: Option<String>,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }

    pub fn to_entry(&self) -> Entry {
        let time = |t: &Option<String>| t.as_deref().and_then(parse_time).unwrap_or_default();
        Entry {
            name: self.name.clone(),
            kind: match self.kind.as_str() {
                "dir" => EntryKind::Directory,
                "file" => EntryKind::File,
                "symlink" => EntryKind::Symlink,
                _ => EntryKind::Other,
            },
            size: if self.kind == "file" { self.size } else { 0 },
            modified: time(&self.mtime),
            // On Windows rustic records the creation time as `ctime`.
            created: time(&self.ctime),
            attrs: 0,
            hidden: false,
            system: false,
        }
    }
}

#[derive(Deserialize)]
struct Tree {
    #[serde(default)]
    nodes: Vec<Node>,
}

/// One line of `rustic diff <snapshot path> <local path>`.
///
/// The snapshot is the reference and the local copy is "new", so from a
/// restore's point of view:
///
/// | status | meaning | a restore would… |
/// |---|---|---|
/// | `M` | contents differ | **overwrite** the local file |
/// | `T` | file here, folder there (or the reverse) | **crash rustic** — exclude it |
/// | `-` | only in the snapshot | create it |
/// | `+` | only local | leave it, or delete it under `--delete` |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub status: char,
    /// Relative to the compared root, `\`-separated as rustic prints it.
    pub path: String,
}

impl RusticDriver {
    pub fn new(settings: RusticSettings) -> Self {
        let d = Self::default();
        *d.session.settings.lock() = settings;
        d
    }

    /// Replace the settings; applies to the next command.
    pub fn set_settings(&self, settings: RusticSettings) {
        let mut cur = self.session.settings.lock();
        if *cur != settings {
            *cur = settings;
            // A different password file or profile can change what opens.
            self.session.opened.lock().clear();
        }
    }

    pub fn settings(&self) -> RusticSettings {
        self.session.settings.lock().clone()
    }

    /// Install the password dialog. Until one is installed a locked
    /// repository fails with [`RusticError::PasswordCancelled`].
    pub fn set_prompter(&self, p: Prompter) {
        *self.session.prompter.lock() = Some(p);
    }

    /// Store `password` for `repo` for the rest of this process.
    pub fn remember_password(&self, repo: &str, password: String) {
        self.session
            .passwords
            .lock()
            .insert(repo.to_string(), password);
    }

    /// Every snapshot in `repo`, oldest first. Always asks rustic — a new
    /// backup can land at any moment — and refreshes the cached copy.
    pub fn snapshots(&self, repo: &str) -> Result<Vec<Snapshot>, RusticError> {
        let out = self.run(
            repo,
            &["snapshots".into(), "--json".into(), "-g".into(), "".into()],
            None,
        )?;
        let snaps = parse_snapshots(&out).map_err(|e| RusticError::Failed {
            message: format!("unreadable snapshot list: {e}"),
            exit_code: None,
        })?;
        self.session
            .snapshots
            .lock()
            .insert(repo.to_string(), snaps.clone());
        Ok(snaps)
    }

    /// The snapshot a row name points at, from the last listing of `repo`.
    pub fn cached_snapshot(&self, repo: &str, row_name: &str) -> Option<Snapshot> {
        let id = snapshot_id_of(row_name)?;
        self.session
            .snapshots
            .lock()
            .get(repo)?
            .iter()
            .find(|s| s.id.starts_with(id))
            .cloned()
    }

    /// The nodes of one directory inside a snapshot. Cached for the life
    /// of the process — a snapshot is immutable.
    pub fn tree(
        &self,
        repo: &str,
        snapshot_id: &str,
        sub: &str,
    ) -> Result<Arc<Vec<Node>>, RusticError> {
        let key = (repo.to_string(), snapshot_id.to_string(), sub.to_string());
        if let Some(t) = self.session.trees.lock().get(&key) {
            return Ok(Arc::clone(t));
        }
        let out = self.run(
            repo,
            &["cat".into(), "tree".into(), snapshot_arg(snapshot_id, sub)],
            None,
        )?;
        let tree: Tree = serde_json::from_slice(&out).map_err(|e| RusticError::Failed {
            message: format!("unreadable tree listing: {e}"),
            exit_code: None,
        })?;
        let nodes = Arc::new(tree.nodes);
        self.session.trees.lock().insert(key, Arc::clone(&nodes));
        Ok(nodes)
    }

    /// List any rustic path: the snapshot list at a repository root, or a
    /// directory's entries inside a snapshot.
    pub fn list(&self, path: &NavPath) -> Result<Vec<Entry>, RusticError> {
        let parts = path
            .rustic_parts()
            .ok_or_else(|| RusticError::NotFound(path.to_string()))?;
        let Some(row) = parts.snapshot.as_deref() else {
            let snaps = self.snapshots(&parts.repo)?;
            return Ok(snapshot_entries(&snaps));
        };
        let id = snapshot_id_of(row).ok_or_else(|| RusticError::NotFound(row.to_string()))?;
        Ok(self
            .tree(&parts.repo, id, &parts.sub)?
            .iter()
            .map(Node::to_entry)
            .collect())
    }

    /// The node `path` names, looked up in its parent's listing. `None`
    /// for a repository or snapshot root, which are not nodes.
    pub fn node(&self, path: &NavPath) -> Result<Option<Node>, RusticError> {
        let Some(parts) = path.rustic_parts() else {
            return Ok(None);
        };
        let (Some(row), false) = (parts.snapshot.as_deref(), parts.sub.is_empty()) else {
            return Ok(None);
        };
        let id = snapshot_id_of(row).ok_or_else(|| RusticError::NotFound(row.to_string()))?;
        let (parent, name) = match parts.sub.rfind('/') {
            Some(i) => (&parts.sub[..i], &parts.sub[i + 1..]),
            None => ("", parts.sub.as_str()),
        };
        Ok(self
            .tree(&parts.repo, id, parent)?
            .iter()
            .find(|n| n.name == name)
            .cloned())
    }

    /// Is `path` a folder? Snapshot and repository roots are.
    pub fn is_dir(&self, path: &NavPath) -> Result<bool, RusticError> {
        match path.rustic_parts() {
            Some(p) if p.snapshot.is_none() || p.sub.is_empty() => Ok(true),
            Some(_) => Ok(self.node(path)?.is_some_and(|n| n.is_dir())),
            None => Ok(path.as_path().is_dir()),
        }
    }

    /// Compare `src` (inside a snapshot) with the local path `local`, which
    /// must exist. See [`DiffEntry`] for what each line means to a restore.
    pub fn diff(&self, src: &NavPath, local: &Path) -> Result<Vec<DiffEntry>, RusticError> {
        let (repo, arg) = restore_source(src)?;
        let (Some(parent), Some(name)) = (local.parent(), local.file_name()) else {
            return Err(RusticError::NotFound(local.display().to_string()));
        };
        // A drive path here would parse as snapshot `C`; relative to the
        // parent it is just a path.
        let mut rel = OsString::from(".\\");
        rel.push(name);
        let out = self.run_os(&repo, vec!["diff".into(), arg.into(), rel], Some(parent))?;
        Ok(parse_diff(&String::from_utf8_lossy(&out)))
    }

    /// Make sure `repo` opens, asking for its password if needed. Cheap
    /// once anything has opened it this session.
    pub fn ensure_open(&self, repo: &str) -> Result<(), RusticError> {
        if self.session.opened.lock().contains(repo) {
            return Ok(());
        }
        self.run(repo, &["cat".into(), "config".into()], None)
            .map(|_| ())
    }

    /// Run a [`Operation::RusticRestore`]. Returns the same handle and
    /// event stream as the other engines, so progress, cancel and failure
    /// reporting are shared.
    ///
    /// The password is settled *before* the restore starts (it may prompt,
    /// so call this from a worker): a streaming child can't be re-run
    /// halfway through when it turns out to need one.
    pub fn spawn(&self, op: Operation) -> std::io::Result<OpHandle> {
        match op {
            Operation::RusticRestore {
                src,
                dst,
                excludes,
                delete,
            } => self.spawn_restore(src, dst, excludes, delete),
            Operation::RusticBackup {
                repo,
                sources,
                tags,
                excludes,
            } => self.spawn_backup(repo, sources, tags, excludes),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "the rustic driver only restores and backs up",
            )),
        }
    }

    fn spawn_restore(
        &self,
        src: NavPath,
        dst: NavPath,
        excludes: Vec<String>,
        delete: bool,
    ) -> std::io::Result<OpHandle> {
        let io = |e: RusticError| std::io::Error::other(e.to_string());
        let (repo, arg) = restore_source(&src).map_err(io)?;
        if dst.is_remote() || dst.is_rustic() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "a restore can only write to a local folder",
            ));
        }
        self.ensure_open(&repo).map_err(io)?;

        let glob = if excludes.is_empty() {
            None
        } else {
            Some(GlobFile::write(&excludes)?)
        };
        let mut args: Vec<OsString> = vec!["restore".into(), "--json-progress".into()];
        if delete {
            args.push("--delete".into());
        }
        if let Some(g) = glob.as_ref() {
            args.push("--glob-file".into());
            args.push(g.0.clone().into_os_string());
        }
        args.push(arg.into());
        args.push(dst.as_path().as_os_str().to_owned());

        let dst_path = dst.as_path().to_path_buf();
        // Everything at `dst` is ours to clean up after a cancel only when
        // nothing was there before we started.
        let dst_is_new = std::fs::symlink_metadata(&dst_path).is_err();

        let mut cmd = self.command(&repo);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        tracing::info!(target: "rustic.spawn", "rustic restore {:?}", args);
        let mut child = cmd.spawn()?;
        kill_child_with_process(&child);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, rx) = unbounded::<OpEvent>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

        let tx_out = tx.clone();
        let reader_out = thread::spawn(move || {
            let Some(out) = stdout else { return };
            let mut files_total = 0u64;
            for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                if let Some(n) = parse_files_to_restore(&line) {
                    files_total = n;
                }
                match parse_status(&line) {
                    Some(mut p) => {
                        p.files_total = files_total;
                        let _ = tx_out.send(OpEvent::Progress(p));
                    }
                    None if !line.trim().is_empty() => {
                        let _ = tx_out.send(OpEvent::Log(log_event(&line)));
                    }
                    None => {}
                }
            }
        });
        let reader_err = thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut text);
            }
            text
        });

        let wait_slot = Arc::clone(&slot);
        let wait_cancelled = Arc::clone(&cancelled);
        thread::spawn(move || {
            // Poll rather than block in `wait`: the canceller takes the
            // child out of this slot to kill it, and holding the lock
            // across a blocking wait would leave it nothing to take.
            let status = loop {
                let mut g = wait_slot.lock();
                let Some(c) = g.as_mut() else {
                    break None;
                };
                match c.try_wait() {
                    Ok(Some(s)) => break Some(s),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                drop(g);
                thread::sleep(std::time::Duration::from_millis(50));
            };
            let _ = reader_out.join();
            let err_text = reader_err.join().unwrap_or_default();
            drop(glob);

            let was_cancelled = wait_cancelled.load(Ordering::Acquire);
            if was_cancelled && dst_is_new {
                // rustic writes straight into the destination, so a killed
                // restore leaves half-written files behind. When the
                // destination didn't exist before, all of it is ours.
                remove_partial(&dst_path);
            }
            let success = !was_cancelled && status.is_some_and(|s| s.success());
            let exit_code = status.and_then(|s| s.code());
            let error =
                (!success).then(|| classify_failure(&err_text, exit_code).to_rclone_error());
            let _ = tx.send(OpEvent::Done {
                success,
                exit_code,
                error,
            });
        });

        Ok(OpHandle::from_parts(rx, slot, cancelled))
    }

    /// Run an [`Operation::RusticBackup`]: `rustic backup --json
    /// --skip-if-unchanged --tag … --iglob !… -- <sources>`, with
    /// `--json-progress` so progress arrives as the same status lines a
    /// restore emits (measured, 0.11.4: with that flag the run prints
    /// NDJSON — `status` lines, then one `summary` line carrying the
    /// counts and the new `snapshot_id`; a run `--skip-if-unchanged`
    /// skipped prints the summary without an id).
    ///
    /// The summary line is forwarded verbatim as an INFO [`OpEvent::Log`],
    /// for [`parse_backup_summary`]. `[WARN]` / `[ERROR]` lines from stderr
    /// — unreadable files, usually — are forwarded as they arrive at their
    /// own level, so a report can list them.
    fn spawn_backup(
        &self,
        repo: String,
        sources: Vec<NavPath>,
        tags: Vec<String>,
        excludes: Vec<String>,
    ) -> std::io::Result<OpHandle> {
        let io = |e: RusticError| std::io::Error::other(e.to_string());
        if sources.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "nothing to back up",
            ));
        }
        if let Some(bad) = sources.iter().find(|s| s.is_remote() || s.is_rustic()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("rustic backs up local paths only, not {bad}"),
            ));
        }
        self.ensure_open(&repo).map_err(io)?;

        let mut args: Vec<OsString> = vec![
            "--json-progress".into(),
            "backup".into(),
            "--json".into(),
            "--skip-if-unchanged".into(),
        ];
        for t in tags.iter().filter(|t| !t.trim().is_empty()) {
            args.push("--tag".into());
            args.push(t.trim().into());
        }
        for g in excludes.iter().filter(|g| !g.trim().is_empty()) {
            args.push("--iglob".into());
            args.push(format!("!{}", g.trim()).into());
        }
        args.push("--".into());
        for src in &sources {
            args.push(src.as_path().as_os_str().to_owned());
        }

        let mut cmd = self.command(&repo);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        tracing::info!(target: "rustic.spawn", "rustic backup {:?}", args);
        let mut child = cmd.spawn()?;
        kill_child_with_process(&child);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, rx) = unbounded::<OpEvent>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

        let tx_out = tx.clone();
        let reader_out = thread::spawn(move || {
            let Some(out) = stdout else { return };
            for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                match parse_status(&line) {
                    Some(p) => {
                        let _ = tx_out.send(OpEvent::Progress(p));
                    }
                    None if !line.trim().is_empty() => {
                        let _ = tx_out.send(OpEvent::Log(log_event_at(LogLevel::Info, &line)));
                    }
                    None => {}
                }
            }
        });
        let tx_err = tx.clone();
        let reader_err = thread::spawn(move || {
            let mut text = String::new();
            let Some(err) = stderr else { return text };
            for line in BufReader::new(err).lines().map_while(|l| l.ok()) {
                let clean = strip_ansi(&line);
                let t = clean.trim();
                if t.is_empty() {
                    continue;
                }
                let level = if t.starts_with("[WARN]") {
                    Some(LogLevel::Warning)
                } else if t.starts_with("[ERROR]") {
                    Some(LogLevel::Error)
                } else {
                    None
                };
                if let Some(level) = level {
                    let _ = tx_err.send(OpEvent::Log(log_event_at(level, t)));
                }
                text.push_str(&clean);
                text.push('\n');
            }
            text
        });

        let wait_slot = Arc::clone(&slot);
        let wait_cancelled = Arc::clone(&cancelled);
        thread::spawn(move || {
            let status = loop {
                let mut g = wait_slot.lock();
                let Some(c) = g.as_mut() else {
                    break None;
                };
                match c.try_wait() {
                    Ok(Some(s)) => break Some(s),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                drop(g);
                thread::sleep(std::time::Duration::from_millis(50));
            };
            let _ = reader_out.join();
            let err_text = reader_err.join().unwrap_or_default();
            let was_cancelled = wait_cancelled.load(Ordering::Acquire);
            let success = !was_cancelled && status.is_some_and(|s| s.success());
            let exit_code = status.and_then(|s| s.code());
            let error = (!success).then(|| {
                if was_cancelled {
                    RusticError::Failed {
                        message: "cancelled".into(),
                        exit_code,
                    }
                    .to_rclone_error()
                } else {
                    classify_failure(&err_text, exit_code).to_rclone_error()
                }
            });
            let _ = tx.send(OpEvent::Done {
                success,
                exit_code,
                error,
            });
        });

        Ok(OpHandle::from_parts(rx, slot, cancelled))
    }

    /// `rustic init` at `repo`. Asks for the new password through the
    /// prompter like any other command — rustic, given no password source
    /// and no console, fails exactly as it does for a locked repository,
    /// and the password the user then types becomes the repository's key.
    pub fn init(&self, repo: &str) -> Result<(), RusticError> {
        self.run(repo, &["init".into()], None).map(|_| ())
    }

    // --- running -------------------------------------------------------

    fn command(&self, repo: &str) -> Command {
        let settings = self.session.settings.lock().clone();
        let mut cmd = Command::new(&settings.exe);
        if !settings.profile.is_empty() {
            cmd.arg("-P").arg(&settings.profile);
        }
        cmd.arg("-r").arg(repo);
        cmd.args(["--log-level", "warn"]);
        // rustic colours its errors even into a pipe.
        cmd.env("NO_COLOR", "1");
        const ALL_SOURCES: [&str; 6] = [
            "RUSTIC_PASSWORD",
            "RUSTIC_PASSWORD_FILE",
            "RUSTIC_PASSWORD_COMMAND",
            "RUSTIC_KEY",
            "RUSTIC_KEY_FILE",
            "RUSTIC_KEY_COMMAND",
        ];
        let typed = self.session.passwords.lock().get(repo).cloned();
        let keep = if let Some(pw) = typed {
            cmd.env("RUSTIC_PASSWORD", pw);
            Some("RUSTIC_PASSWORD")
        } else if !settings.password_file.is_empty() {
            cmd.env("RUSTIC_PASSWORD_FILE", &settings.password_file);
            Some("RUSTIC_PASSWORD_FILE")
        } else {
            None
        };
        // Setting one source and inheriting another is a usage error.
        if let Some(k) = keep {
            for v in ALL_SOURCES.iter().filter(|v| **v != k) {
                cmd.env_remove(v);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }

    fn run(&self, repo: &str, args: &[String], cwd: Option<&Path>) -> Result<Vec<u8>, RusticError> {
        self.run_os(repo, args.iter().map(OsString::from).collect(), cwd)
    }

    /// Run one command to completion, asking for a password and retrying
    /// for as long as the repository refuses and the user keeps answering.
    fn run_os(
        &self,
        repo: &str,
        args: Vec<OsString>,
        cwd: Option<&Path>,
    ) -> Result<Vec<u8>, RusticError> {
        loop {
            let tried = self.session.passwords.lock().get(repo).cloned();
            let mut cmd = self.command(repo);
            cmd.args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(d) = cwd {
                cmd.current_dir(d);
            }
            let child = cmd.spawn().map_err(|e| RusticError::Spawn(e.to_string()))?;
            kill_child_with_process(&child);
            let out = child
                .wait_with_output()
                .map_err(|e| RusticError::Spawn(e.to_string()))?;
            if out.status.success() {
                self.session.opened.lock().insert(repo.to_string());
                return Ok(out.stdout);
            }
            let err_text = String::from_utf8_lossy(&out.stderr);
            let failure = classify_failure(&err_text, out.status.code());
            tracing::debug!(target: "rustic", "rustic {:?} refused: {}", args, error_message(&err_text));
            let Failure::Password { refused } = failure_kind(&err_text) else {
                tracing::warn!("rustic {:?} failed: {}", args, failure);
                return Err(failure);
            };
            self.ask_password(repo, tried, refused)?;
        }
    }

    /// Get a (new) password for `repo` into the session, or fail.
    fn ask_password(
        &self,
        repo: &str,
        tried: Option<String>,
        refused: bool,
    ) -> Result<(), RusticError> {
        let _one_dialog = self.session.asking.lock();
        // Another worker may have been answered while we waited for the
        // dialog lock — try what it got before asking again.
        let now = self.session.passwords.lock().get(repo).cloned();
        if now.is_some() && now != tried {
            return Ok(());
        }
        let prompter = self.session.prompter.lock().clone();
        let Some(prompter) = prompter else {
            return Err(RusticError::PasswordCancelled);
        };
        // Only say "not accepted" about a password *we* handed over. With
        // no console, rustic falls back to reading the password from
        // stdin — which is null — so a repository nobody gave a password
        // to fails exactly like a wrong one (measured: "No suitable key
        // found"), and the very first prompt would open by blaming the
        // user for a password they never typed.
        let ours = tried.is_some() || !self.session.settings.lock().password_file.is_empty();
        let retry = refused && ours;
        match prompter(repo, retry) {
            Some(pw) => {
                self.remember_password(repo, pw);
                Ok(())
            }
            None => Err(RusticError::PasswordCancelled),
        }
    }
}

/// `<snapshot id>[:<path>]`, the form every snapshot-reading command takes.
fn snapshot_arg(id: &str, sub: &str) -> String {
    if sub.is_empty() {
        id.to_string()
    } else {
        format!("{id}:{sub}")
    }
}

/// `(repo, "<id>:<path>")` for something to restore or diff.
fn restore_source(src: &NavPath) -> Result<(String, String), RusticError> {
    let RusticParts {
        repo,
        snapshot,
        sub,
    } = src
        .rustic_parts()
        .ok_or_else(|| RusticError::NotFound(src.to_string()))?;
    let row = snapshot.ok_or_else(|| {
        RusticError::NotFound("a repository root is not something to restore".into())
    })?;
    let id = snapshot_id_of(&row).ok_or_else(|| RusticError::NotFound(row.clone()))?;
    Ok((repo, snapshot_arg(id, &sub)))
}

fn remove_partial(p: &Path) {
    let r = match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => return,
    };
    if let Err(e) = r {
        tracing::warn!("could not remove partial restore {}: {e}", p.display());
    }
}

/// The `--glob-file` of a restore with excludes. Deleted on drop; named
/// with [`GLOB_PREFIX`] so a crash is swept at the next start.
struct GlobFile(PathBuf);

impl GlobFile {
    fn write(excludes: &[String]) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "{}{}-{}.txt",
            GLOB_PREFIX,
            std::process::id(),
            GLOB_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut body = String::new();
        for e in excludes {
            // An excluded folder's contents are matched on their own, so
            // excluding `nested` alone still restores `nested\big.bin` —
            // which, under a local *file* called `nested`, crashes rustic.
            let g = exclude_glob(e);
            body.push_str(&g);
            body.push('\n');
            body.push_str(&g);
            body.push_str("/**\n");
        }
        std::fs::write(&path, body)?;
        Ok(Self(path))
    }
}

impl Drop for GlobFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// --- pure helpers ------------------------------------------------------

/// An anchored exclude for `rel` (relative to the restore root, either
/// separator) in rustic's glob syntax.
///
/// Patterns are gitignore-style with `!` meaning *exclude*. Every glob
/// metacharacter a Windows name can carry is wrapped in a one-character
/// class (`[` → `[[]`) — backslash escaping is off on Windows, and a name
/// like `report [final].txt` would otherwise match `report f.txt` and
/// friends rather than itself.
pub fn exclude_glob(rel: &str) -> String {
    let mut out = String::from("!/");
    let rel = rel.trim_start_matches(['/', '\\']);
    for c in rel.chars() {
        match c {
            '\\' | '/' => out.push('/'),
            '[' | ']' | '*' | '?' | '{' | '}' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out
}

/// The snapshot id a row name ends with. Row names are built by
/// [`snapshot_row_name`]; a bare id typed into the address bar works too.
pub fn snapshot_id_of(row: &str) -> Option<&str> {
    let id = row.split_whitespace().last()?;
    (id.len() >= 4 && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id)
}

/// The name a snapshot gets as a row: `2026-10-22 10:13 Documents [daily] 4f13f557`.
///
/// Read left to right, so it leads with what a person picks a backup by —
/// when — then what it holds, its tags, and ends with the id, which is the
/// only part rustic is ever given ([`snapshot_id_of`]). Date-first also
/// makes the default name sort chronological. `host` is added only when a
/// repository holds more than one machine's backups.
pub fn snapshot_row_name(s: &Snapshot, show_host: bool) -> String {
    let mut parts: Vec<String> = vec![s.time.get(..16).unwrap_or(&s.time).replace('T', " ")];
    let what = if !s.label.is_empty() {
        s.label.clone()
    } else {
        s.paths
            .iter()
            .map(|p| path_basename(p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !what.is_empty() {
        parts.push(what);
    }
    if !s.tags.is_empty() {
        parts.push(format!("[{}]", s.tags.join(", ")));
    }
    if show_host && !s.hostname.is_empty() {
        parts.push(format!("@{}", s.hostname));
    }
    parts.push(s.id.get(..8).unwrap_or(&s.id).to_string());
    // The name becomes one path component and is shown with ` › ` between
    // levels, so neither a separator nor that arrow may appear inside it.
    parts
        .join(" ")
        .chars()
        .map(|c| match c {
            '\\' | '/' => '-',
            '›' => '>',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

fn path_basename(p: &str) -> String {
    let t = p.trim_end_matches(['\\', '/']);
    let base = t.rsplit(['\\', '/']).next().unwrap_or(t);
    if base.is_empty() {
        p.to_string()
    } else {
        base.to_string()
    }
}

/// Snapshot rows for a repository root.
pub fn snapshot_entries(snaps: &[Snapshot]) -> Vec<Entry> {
    let hosts: HashSet<&str> = snaps.iter().map(|s| s.hostname.as_str()).collect();
    let show_host = hosts.len() > 1;
    snaps
        .iter()
        .map(|s| Entry {
            name: snapshot_row_name(s, show_host),
            kind: EntryKind::Directory,
            size: s.summary.as_ref().map_or(0, |m| m.total_bytes_processed),
            modified: parse_time(&s.time).unwrap_or_default(),
            created: parse_time(&s.time).unwrap_or_default(),
            attrs: 0,
            hidden: false,
            system: false,
        })
        .collect()
}

/// Where a backed-up path lives inside the snapshot tree, `/`-separated.
///
/// rustic stores `C:\Users\me` under `C/Users/me` (the drive colon goes),
/// and `/home/me` under `home/me`. `None` for anything else.
pub fn tree_path_of(backed_up: &str) -> Option<String> {
    let b = backed_up.as_bytes();
    let rest = if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        format!("{}{}", &backed_up[..1], &backed_up[2..])
    } else if backed_up.starts_with('/') {
        backed_up.to_string()
    } else {
        return None;
    };
    let parts: Vec<&str> = rest.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    Some(parts.join("/"))
}

/// The folder to open when entering snapshot `s`: what it backed up — or,
/// for a snapshot of several paths, the deepest folder holding them all.
/// Empty (the snapshot root) when the paths share nothing.
pub fn snapshot_start(s: &Snapshot) -> String {
    let mut common: Option<Vec<String>> = None;
    for p in &s.paths {
        let Some(t) = tree_path_of(p) else {
            return String::new();
        };
        let comps: Vec<String> = t
            .split('/')
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect();
        common = Some(match common {
            None => comps,
            Some(c) => c
                .into_iter()
                .zip(comps)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

/// Parse `snapshots --json`. With `-g ""` rustic prints a flat array; a
/// profile or a future version could group them, so both shapes are read.
pub fn parse_snapshots(out: &[u8]) -> Result<Vec<Snapshot>, serde_json::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Item {
        Group { snapshots: Vec<Snapshot> },
        One(Box<Snapshot>),
    }
    let items: Vec<Item> = serde_json::from_slice(out)?;
    let mut snaps: Vec<Snapshot> = items
        .into_iter()
        .flat_map(|i| match i {
            Item::Group { snapshots } => snapshots,
            Item::One(s) => vec![*s],
        })
        .collect();
    snaps.sort_by(|a, b| {
        parse_time(&a.time)
            .cmp(&parse_time(&b.time))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(snaps)
}

/// Parse `rustic diff` output. Names are printed as Rust debug strings
/// (`"nested\\big.bin"`); summary lines are skipped.
pub fn parse_diff(out: &str) -> Vec<DiffEntry> {
    let mut v = Vec::new();
    for line in out.lines() {
        let mut chars = line.chars();
        let Some(status) = chars.next() else { continue };
        if !matches!(status, '+' | '-' | 'M' | 'T' | 'U') {
            continue;
        }
        let rest = chars.as_str().trim_start();
        if !rest.starts_with('"') {
            continue;
        }
        if let Some(path) = unquote_debug(rest) {
            v.push(DiffEntry { status, path });
        }
    }
    v
}

/// Undo Rust's `{:?}` for a string: `"a\\b \"q\" \u{1b}"` → `a\b "q" ␛`.
fn unquote_debug(s: &str) -> Option<String> {
    let body = s.trim().strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(body.len());
    let mut it = body.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '0' => out.push('\0'),
            'u' => {
                if it.next()? != '{' {
                    return None;
                }
                let hex: String = it.by_ref().take_while(|c| *c != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// One `--json-progress` status line as a [`Progress`].
fn parse_status(line: &str) -> Option<Progress> {
    #[derive(Deserialize)]
    struct Status {
        message_type: String,
        #[serde(default)]
        seconds_elapsed: f64,
        #[serde(default)]
        total_bytes: u64,
        #[serde(default)]
        bytes_done: u64,
    }
    let s: Status = serde_json::from_str(line.trim()).ok()?;
    if s.message_type != "status" {
        return None;
    }
    let speed = if s.seconds_elapsed > 0.0 {
        s.bytes_done as f64 / s.seconds_elapsed
    } else {
        0.0
    };
    let eta = (speed > 0.0 && s.total_bytes >= s.bytes_done)
        .then(|| ((s.total_bytes - s.bytes_done) as f64 / speed) as u64);
    Some(Progress {
        bytes_done: s.bytes_done,
        bytes_total: s.total_bytes,
        files_done: 0,
        files_total: 0,
        speed_bps: speed,
        eta_secs: eta,
        current: None,
    })
}

/// The `summary` line `rustic backup --json-progress` ends with.
///
/// `snapshot_id` is `None` when `--skip-if-unchanged` found nothing to
/// write — the counts are still reported, and that is how a report can
/// say "nothing changed" rather than "nothing happened".
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BackupSummary {
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unmodified: u64,
    pub dirs_new: u64,
    pub dirs_changed: u64,
    pub dirs_unmodified: u64,
    /// Bytes of new data before compression.
    pub data_added: u64,
    /// Bytes actually written to the repository.
    pub data_added_packed: u64,
    pub total_files_processed: u64,
    pub total_bytes_processed: u64,
    /// Seconds.
    pub total_duration: f64,
    pub snapshot_id: Option<String>,
}

/// Parse one stdout line of a backup as its summary, if that is what it
/// is. Status lines and anything else return `None`.
pub fn parse_backup_summary(line: &str) -> Option<BackupSummary> {
    #[derive(Deserialize)]
    struct Tagged {
        message_type: String,
        #[serde(flatten)]
        summary: BackupSummary,
    }
    let t: Tagged = serde_json::from_str(line.trim()).ok()?;
    (t.message_type == "summary").then_some(t.summary)
}

/// Files a restore will write, from its plan line:
/// `Files:  3 to restore, 0 unchanged, 0 verified, 1 to modify, 0 additional`.
fn parse_files_to_restore(line: &str) -> Option<u64> {
    let rest = line.trim().strip_prefix("Files:")?;
    let mut total = 0;
    for part in rest.split(',') {
        let part = part.trim();
        let (n, what) = part.split_once(' ')?;
        if what == "to restore" || what == "to modify" {
            total += n.parse::<u64>().ok()?;
        }
    }
    Some(total)
}

fn log_event(line: &str) -> LogEvent {
    log_event_at(LogLevel::Info, line)
}

fn log_event_at(level: LogLevel, line: &str) -> LogEvent {
    LogEvent {
        level: Some(level),
        msg: line.to_string(),
        source: None,
        object: None,
        object_type: None,
        skipped: None,
        stats: None,
    }
}

enum Failure {
    Password { refused: bool },
    Other,
}

fn failure_kind(stderr: &str) -> Failure {
    let t = strip_ansi(stderr);
    // No password source and nowhere to prompt.
    if t.contains("not a terminal") {
        return Failure::Password { refused: false };
    }
    if t.contains("No suitable key found")
        || t.contains("errors/C002")
        || t.contains("seems to be incorrect")
    {
        return Failure::Password { refused: true };
    }
    Failure::Other
}

/// Distil rustic's stderr into a [`RusticError`].
pub fn classify_failure(stderr: &str, exit_code: Option<i32>) -> RusticError {
    let message = error_message(stderr);
    if let Failure::Password { .. } = failure_kind(stderr) {
        return RusticError::Failed {
            message: "the repository password was not accepted".into(),
            exit_code,
        };
    }
    if message.contains("not found in tree") || message.starts_with("No suitable id found") {
        return RusticError::NotFound(message);
    }
    // Measured (0.11.4): `No repository config file found for
    // `local:D:\x`. Please check the repository.`
    if message.contains("No repository config file found") {
        return RusticError::NoRepository(message);
    }
    let message = if message.is_empty() {
        match exit_code {
            Some(c) => format!("rustic exited with code {c}"),
            None => "rustic was stopped".into(),
        }
    } else {
        message
    };
    RusticError::Failed { message, exit_code }
}

/// The sentence a person needs out of rustic's error report.
///
/// A core error is a banner, then `Message:`, the sentence, a blank line
/// and pages of boilerplate ("We believe this is a bug…", a backtrace
/// note). A CLI error is one `error: …` line. A panic prints "rustic-rs had
/// a problem and crashed".
pub fn error_message(stderr: &str) -> String {
    let t = strip_ansi(stderr);
    let lines: Vec<&str> = t.lines().map(str::trim).collect();
    if let Some(i) = lines.iter().position(|l| *l == "Message:") {
        let msg: Vec<&str> = lines[i + 1..]
            .iter()
            .take_while(|l| !l.is_empty())
            .copied()
            .collect();
        if !msg.is_empty() {
            return msg.join(" ");
        }
    }
    if lines
        .iter()
        .any(|l| l.contains("had a problem and crashed"))
    {
        return "rustic crashed".into();
    }
    for l in &lines {
        if let Some(rest) = l.strip_prefix("error:") {
            let rest = rest.trim();
            if !rest.starts_with("`rustic_core` experienced") {
                return rest.to_string();
            }
        }
    }
    lines
        .iter()
        .rev()
        .find(|l| {
            !l.is_empty()
                && !l.starts_with('[')
                && !l.starts_with("Backtrace")
                && !l.contains("RUST_BACKTRACE")
        })
        .map(|l| l.to_string())
        .unwrap_or_default()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' && it.peek() == Some(&'[') {
            for n in it.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// RFC 3339 with fraction and offset — `2026-10-22T10:13:23.0410089+02:00`
/// — as a UTC [`FileTime`]. rustic writes local time with its offset, and
/// dropping the offset would shift every snapshot by the time zone.
pub fn parse_time(s: &str) -> Option<FileTime> {
    let base = crate::op::parse_rfc3339_filetime(s)?;
    let rest = s.get(19..)?;
    let (frac, zone) = match rest.strip_prefix('.') {
        Some(r) => {
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            (&r[..end], &r[end..])
        }
        None => ("", rest),
    };
    // 100 ns ticks from the first seven fraction digits.
    let mut ticks: i64 = 0;
    for i in 0..7 {
        ticks = ticks * 10 + frac.as_bytes().get(i).map_or(0, |d| (d - b'0') as i64);
    }
    let offset_secs: i64 = match zone.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let hh: i64 = zone.get(1..3)?.parse().ok()?;
            let mm: i64 = zone.get(4..6).and_then(|m| m.parse().ok()).unwrap_or(0);
            let secs = hh * 3600 + mm * 60;
            if sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    let t = base.0 as i64 + ticks - offset_secs * 10_000_000;
    (t >= 0).then_some(FileTime(t as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(time: &str, paths: &[&str]) -> Snapshot {
        Snapshot {
            id: "4f13f55765d6ebef4a5ca0a5ef46cc203001e59a4108574a327b4fb71bac5efb".into(),
            time: time.into(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            hostname: "DESKTOP".into(),
            label: String::new(),
            tags: vec![],
            description: None,
            program_version: String::new(),
            summary: None,
        }
    }

    #[test]
    fn row_name_leads_with_time_and_ends_with_the_id() {
        let mut s = snap(
            "2026-10-22T10:13:23.0410089+02:00",
            &[r"C:\Users\me\Documents"],
        );
        assert_eq!(
            snapshot_row_name(&s, false),
            "2026-10-22 10:13 Documents 4f13f557"
        );
        s.tags = vec!["daily".into(), "x".into()];
        assert_eq!(
            snapshot_row_name(&s, true),
            "2026-10-22 10:13 Documents [daily, x] @DESKTOP 4f13f557"
        );
        assert_eq!(
            snapshot_id_of(&snapshot_row_name(&s, true)),
            Some("4f13f557")
        );
        s.label = "game/saves › x".into();
        assert_eq!(
            snapshot_row_name(&s, false),
            "2026-10-22 10:13 game-saves > x [daily, x] 4f13f557"
        );
        assert_eq!(snapshot_id_of("4f13f557"), Some("4f13f557"));
        assert_eq!(snapshot_id_of("2026-10-22 10:13 notes"), None);
    }

    #[test]
    fn time_offset_is_applied() {
        let local = parse_time("2026-10-22T10:13:23.5+02:00").unwrap();
        let utc = parse_time("2026-10-22T08:13:23Z").unwrap();
        assert_eq!(local.0 - utc.0, 5_000_000);
        assert_eq!(
            parse_time("2026-10-22T08:13:23-01:30").unwrap().0 - utc.0,
            5_400 * 10_000_000
        );
    }

    #[test]
    fn backed_up_paths_map_to_tree_paths() {
        assert_eq!(tree_path_of(r"C:\Users\me").as_deref(), Some("C/Users/me"));
        assert_eq!(tree_path_of(r"C:\").as_deref(), Some("C"));
        assert_eq!(tree_path_of("/home/me/").as_deref(), Some("home/me"));
        assert_eq!(tree_path_of("relative"), None);
        let s = snap(
            "2026-01-01T00:00:00Z",
            &[r"C:\Users\me\a", r"C:\Users\me\b\c"],
        );
        assert_eq!(snapshot_start(&s), "C/Users/me");
        let s = snap("2026-01-01T00:00:00Z", &[r"C:\x", r"D:\y"]);
        assert_eq!(snapshot_start(&s), "");
        let s = snap("2026-01-01T00:00:00Z", &[r"D:\games\saves"]);
        assert_eq!(snapshot_start(&s), "D/games/saves");
    }

    #[test]
    fn diff_lines_are_parsed_with_debug_escapes() {
        let out = "M    \"b.md\"\n+    \"extra.txt\"\nT    \"nested\"\n-    \"nested\\\\big.bin\"\n-    \"＂q＂ \\\"x\\\" \\u{1b}.txt\"\nFiles   : 1 =, 1 +, 0 -, 2 M, 0 U\nDirs    : 1 =\n";
        let d = parse_diff(out);
        let got: Vec<(char, &str)> = d.iter().map(|e| (e.status, e.path.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ('M', "b.md"),
                ('+', "extra.txt"),
                ('T', "nested"),
                ('-', r"nested\big.bin"),
                ('-', "＂q＂ \"x\" \u{1b}.txt"),
            ]
        );
    }

    #[test]
    fn excludes_are_anchored_and_escaped() {
        assert_eq!(exclude_glob(r"nested\big.bin"), "!/nested/big.bin");
        assert_eq!(exclude_glob("a [1]{x}.txt"), "!/a [[]1[]][{]x[}].txt");
        assert_eq!(exclude_glob("/b.md"), "!/b.md");
    }

    #[test]
    fn progress_and_plan_lines() {
        let p = parse_status(r#"{"message_type":"status","seconds_elapsed":2,"percent_done":0.5,"total_bytes":100,"bytes_done":50}"#).unwrap();
        assert_eq!(
            (p.bytes_done, p.bytes_total, p.eta_secs),
            (50, 100, Some(2))
        );
        assert!(parse_status("restore done.").is_none());
        assert_eq!(
            parse_files_to_restore(
                "Files:  3 to restore, 0 unchanged, 0 verified, 1 to modify, 0 additional"
            ),
            Some(4)
        );
        assert_eq!(parse_files_to_restore("Dirs:   1 to restore"), None);
    }

    #[test]
    fn backup_summary_is_read_from_the_json_progress_stream() {
        // Verbatim rustic 0.11.4 output for a run that wrote a snapshot…
        let line = r#"{"message_type":"summary","files_new":1,"files_changed":0,"files_unmodified":3,"dirs_new":0,"dirs_changed":13,"dirs_unmodified":1,"data_blobs":4,"tree_blobs":13,"data_added":9003239,"data_added_packed":9002994,"total_files_processed":4,"total_bytes_processed":12000012,"total_duration":0.0628283,"snapshot_id":"e7bc6ede3c86ba17f4edb913a5905b990b7a48388a1ca9e44e3fa62b9797bb00"}"#;
        let s = parse_backup_summary(line).expect("summary");
        assert_eq!(s.files_new, 1);
        assert_eq!(s.files_unmodified, 3);
        assert_eq!(s.data_added_packed, 9002994);
        assert_eq!(s.total_bytes_processed, 12000012);
        assert_eq!(
            s.snapshot_id.as_deref().map(|id| &id[..8]),
            Some("e7bc6ede")
        );
        // …and one `--skip-if-unchanged` skipped: no id, counts intact.
        let skipped = r#"{"message_type":"summary","files_new":0,"files_changed":0,"files_unmodified":4,"total_files_processed":4,"total_bytes_processed":12,"total_duration":0.01}"#;
        let s = parse_backup_summary(skipped).expect("summary");
        assert!(s.snapshot_id.is_none());
        assert_eq!(s.files_unmodified, 4);
        // A status line is not a summary.
        assert!(parse_backup_summary(
            r#"{"message_type":"status","seconds_elapsed":0,"percent_done":0.0,"total_bytes":12,"bytes_done":0}"#
        )
        .is_none());
    }

    #[test]
    fn a_missing_repository_is_its_own_error() {
        let stderr = "error: `rustic_core` experienced an error related to `the configuration`.\n\nMessage:\nNo repository config file found for `local:D:\\nope`. Please check the repository.\n\n";
        match classify_failure(stderr, Some(1)) {
            RusticError::NoRepository(m) => assert!(m.contains("D:\\nope"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn errors_are_distilled() {
        let core = "\u{1b}[0m\u{1b}[31merror:\u{1b}[0m `rustic_core` experienced an error related to `internal operations`.\n\nMessage:\nNode `Nope` not found in tree.\n\nWe believe this is a bug, please report it\n\nBacktrace:\ndisabled backtrace (set 'RUST_BACKTRACE=\"1\"' environment variable to enable)\n";
        assert_eq!(error_message(core), "Node `Nope` not found in tree.");
        assert!(matches!(
            classify_failure(core, Some(1)),
            RusticError::NotFound(_)
        ));
        let cli = "[INFO] using no config file\nerror: IO error: not a terminal\n";
        assert_eq!(error_message(cli), "IO error: not a terminal");
        assert!(matches!(
            failure_kind(cli),
            Failure::Password { refused: false }
        ));
        let wrong = "error: `rustic_core` experienced an error related to `credentials handling`.\n\nMessage:\nThe password that has been entered, seems to be incorrect. No suitable key found for the given password.\n";
        assert!(matches!(
            failure_kind(wrong),
            Failure::Password { refused: true }
        ));
        let crash = "rustic-rs had a problem and crashed. To help us diagnose\n";
        assert_eq!(error_message(crash), "rustic crashed");
    }

    #[test]
    fn snapshot_list_reads_flat_and_grouped_json() {
        let flat = br#"[{"id":"bb","time":"2026-02-01T00:00:00Z"},{"id":"aa","time":"2026-01-01T00:00:00Z","paths":["C:\\x"]}]"#;
        let s = parse_snapshots(flat).unwrap();
        assert_eq!(
            s.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["aa", "bb"]
        );
        let grouped = br#"[{"group_key":{"hostname":"h"},"snapshots":[{"id":"cc","time":"2026-01-01T00:00:00Z"}]}]"#;
        assert_eq!(parse_snapshots(grouped).unwrap()[0].id, "cc");
    }
}
