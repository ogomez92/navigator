//! Saved jobs (Tools → Jobs): the pure half. Planning what a job runs,
//! translating its excludes for each engine, rendering the report, and the
//! naming rule Duplicate (Ctrl+Shift+D) uses. No HWND, no process, no
//! filesystem except [`save_report`] — everything else is unit-tested
//! here and driven from `app.rs`.
//!
//! A job is `navigator_config::Job`: sources + destination + mode, saved in
//! `config.toml`. Running one is a paste with the conflict decision made
//! in advance, so it reuses the paste's engines and progress; the report
//! is what a paste never needed — a window listing, item by item, what
//! moved, how much, how long, and what went wrong, built from nothing but
//! the engines' own output.

use std::path::PathBuf;

use navigator_config::{Job, JobKind};
use navigator_core::{ConflictMode, NavPath};
use navigator_rclone::rustic::BackupSummary;
use navigator_rclone::{LogLevel, Operation};

/// The address-bar spelling of a path, which is also how a job stores it:
/// `rustic:D:\repo`, `mac:Downloads`, `D:\photos`.
pub fn path_display(p: &NavPath) -> String {
    if let Some(r) = p.rustic_display() {
        return r;
    }
    if let Some(a) = p.rclone_arg() {
        return a;
    }
    p.to_string()
}

/// Parse a stored path back. Any spelling the address bar takes.
pub fn parse_path(s: &str) -> Result<NavPath, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty path".into());
    }
    NavPath::new(PathBuf::from(t)).map_err(|e| format!("{t}: {e}"))
}

/// The folder name a source gets under a copy job's destination. Usually
/// its own name; a drive root has none, so `D:\` becomes `D` — the same
/// shape a rustic snapshot tree uses (`C\Users\…`).
pub fn item_name(src: &NavPath) -> String {
    let n = src.file_name();
    if !n.is_empty() {
        return n.to_string();
    }
    let s = src.to_string();
    let t = s.trim_end_matches(['\\', '/']);
    if let Some(letter) = t.strip_suffix(':') {
        return letter.to_ascii_uppercase();
    }
    t.rsplit(['\\', '/'])
        .find(|c| !c.is_empty())
        .unwrap_or("root")
        .to_string()
}

/// One source of a copy job, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyItem {
    pub src: NavPath,
    /// For a folder: the folder its contents land in. For a file: the
    /// folder it is copied into.
    pub dst: NavPath,
    pub is_dir: bool,
}

impl CopyItem {
    /// `src → dst`, for the report.
    pub fn label(&self) -> String {
        if self.is_dir {
            format!("{} → {}", path_display(&self.src), path_display(&self.dst))
        } else {
            format!(
                "{} → {}",
                path_display(&self.src),
                path_display(&self.dst.join(self.src.file_name()))
            )
        }
    }
}

/// Resolve a copy job's sources against `dest`. Returns the items to run
/// and, separately, the sources that cannot run with the reason — a
/// stored path that no longer parses, a local path that is gone. `is_dir`
/// is passed in so this stays pure; the caller stats (or asks rclone).
pub fn plan_copy(
    job: &Job,
    dest: &NavPath,
    is_dir: impl Fn(&NavPath) -> bool,
) -> (Vec<CopyItem>, Vec<(String, String)>) {
    let mut items = Vec::new();
    let mut problems = Vec::new();
    for raw in job.sources.iter().filter(|s| !s.trim().is_empty()) {
        let src = match parse_path(raw) {
            Ok(p) => p,
            Err(e) => {
                problems.push((raw.trim().to_string(), format!("not a valid path ({e})")));
                continue;
            }
        };
        if !src.is_remote() && !src.is_rustic() && !src.as_path().exists() {
            problems.push((raw.trim().to_string(), "not found".into()));
            continue;
        }
        if src.is_rustic() {
            problems.push((
                raw.trim().to_string(),
                "a path inside a backup; copy and paste it instead".into(),
            ));
            continue;
        }
        let dir = is_dir(&src);
        let dst = if dir && !job.contents_only {
            dest.join(&item_name(&src))
        } else {
            dest.clone()
        };
        items.push(CopyItem {
            src,
            dst,
            is_dir: dir,
        });
    }
    (items, problems)
}

/// The operation that runs one copy item under `mode`. A folder is a
/// [`Operation::SyncDir`] into its own destination; a file is a plain
/// [`Operation::Copy`] into the folder, which keeps its name.
pub fn copy_operation(item: &CopyItem, mode: ConflictMode) -> Operation {
    if item.is_dir {
        Operation::SyncDir {
            src: item.src.clone(),
            dst: item.dst.clone(),
            mode,
        }
    } else {
        Operation::Copy {
            sources: vec![item.src.clone()],
            dest_dir: item.dst.clone(),
            // A single file has nothing to prune; `op_args` would degrade
            // it anyway, but say so here rather than rely on that.
            mode: if mode == ConflictMode::Mirror {
                ConflictMode::Replace
            } else {
                mode
            },
        }
    }
}

// --- excludes, per engine ----------------------------------------------------

fn clean_excludes(ex: &[String]) -> impl Iterator<Item = &str> {
    ex.iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
}

/// rclone `--exclude` flags. A bare name (`node_modules`, `*.tmp`) excludes
/// both an entry so named and, as `name/**`, everything inside a folder so
/// named, at any depth — the two lines an `exclude.txt` carries for each
/// folder. A pattern that already contains a `/` is passed as written.
pub fn rclone_exclude_args(ex: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in clean_excludes(ex) {
        out.push("--exclude".into());
        out.push(p.to_string());
        if !p.contains('/') {
            out.push("--exclude".into());
            out.push(format!("{p}/**"));
        }
    }
    out
}

/// robocopy `/XD` (folders) and `/XF` (files) lists. robocopy matches a
/// bare name at any depth, so a trailing `/**` is dropped and a pattern
/// with a `/` is respelled with backslashes.
pub fn robocopy_exclude_flags(ex: &[String]) -> Vec<String> {
    let names: Vec<String> = clean_excludes(ex)
        .map(|p| p.strip_suffix("/**").unwrap_or(p).replace('/', "\\"))
        .filter(|p| !p.is_empty())
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["/XD".to_string()];
    out.extend(names.iter().cloned());
    out.push("/XF".into());
    out.extend(names);
    out
}

/// rustic `--iglob` patterns (the driver prefixes `!`). A bare name
/// matches at any depth, so a trailing `/**` is dropped.
pub fn rustic_exclude_globs(ex: &[String]) -> Vec<String> {
    clean_excludes(ex)
        .map(|p| p.strip_suffix("/**").unwrap_or(p).to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Extra rclone flags for a job: excludes plus `-l` when links are kept.
pub fn rclone_job_args(job: &Job) -> Vec<String> {
    let mut out = rclone_exclude_args(&job.excludes);
    if job.links {
        out.push("-l".into());
    }
    out
}

/// Tags a rustic job's snapshot carries: `job:<name>` first, then the
/// user's own.
pub fn rustic_tags(job: &Job) -> Vec<String> {
    let mut out = vec![format!("job:{}", job.name.trim())];
    out.extend(
        job.tags
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
    );
    out
}

/// Split a `tags` field as typed: commas, semicolons or whitespace.
pub fn split_tags(s: &str) -> Vec<String> {
    s.split([',', ';', ' ', '\t'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// One entry per non-blank line.
pub fn split_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

// --- the report ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Nothing changed since the last snapshot, so rustic wrote none.
    Unchanged,
    /// Not attempted, with the reason.
    Skipped(String),
    /// Attempted and failed, with the engine's reason.
    Failed(String),
    Cancelled,
}

/// What happened to one item of the job.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemReport {
    /// `source → destination`.
    pub label: String,
    /// `rclone`, `robocopy`, `rustic`, or empty when nothing ran.
    pub engine: &'static str,
    pub outcome: Outcome,
    /// Files and bytes the engine reported moving.
    pub files: u64,
    pub bytes: u64,
    pub secs: u64,
    /// Lines worth reading, in order: the engine's warnings and errors,
    /// a rustic summary, a failure's supporting lines.
    pub detail: Vec<String>,
}

/// Everything the window shows about one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub name: String,
    pub kind: JobKind,
    pub dest: String,
    pub mode: ConflictMode,
    pub contents_only: bool,
    pub links: bool,
    pub excludes: Vec<String>,
    /// Unix seconds.
    pub started: u64,
    pub finished: u64,
    pub items: Vec<ItemReport>,
    /// Where the text was also written, once it has been.
    pub saved_to: Option<PathBuf>,
}

impl Report {
    pub fn new(job: &Job, started: u64) -> Self {
        Self {
            name: job.name.clone(),
            kind: job.kind(),
            dest: job.dest.clone(),
            mode: job.mode,
            contents_only: job.contents_only,
            links: job.links,
            excludes: job.excludes.clone(),
            started,
            finished: started,
            items: Vec::new(),
            saved_to: None,
        }
    }

    pub fn ok_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, Outcome::Ok | Outcome::Unchanged))
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, Outcome::Failed(_) | Outcome::Skipped(_)))
            .count()
    }

    pub fn cancelled(&self) -> bool {
        self.items.iter().any(|i| i.outcome == Outcome::Cancelled)
    }

    /// Every item ran and succeeded.
    pub fn all_ok(&self) -> bool {
        !self.items.is_empty() && self.failed_count() == 0 && !self.cancelled()
    }

    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }

    pub fn files(&self) -> u64 {
        self.items.iter().map(|i| i.files).sum()
    }

    /// The sentence spoken when the job ends, and stored as its last run.
    pub fn summary(&self) -> String {
        let n = self.items.len();
        let items = if n == 1 {
            "1 item".to_string()
        } else {
            format!("{n} items")
        };
        if self.cancelled() {
            return format!(
                "job {} cancelled after {} of {items}",
                self.name,
                self.ok_count()
            );
        }
        let moved = match self.kind {
            JobKind::RusticBackup => {
                if self.items.iter().all(|i| i.outcome == Outcome::Unchanged) {
                    "nothing changed since the last snapshot".to_string()
                } else {
                    format!("{} added", crate::listview::format_size(self.bytes()))
                }
            }
            JobKind::Copy => format!(
                "{} files, {} transferred",
                self.files(),
                crate::listview::format_size(self.bytes())
            ),
        };
        let failed = self.failed_count();
        if failed == 0 {
            format!("job {} done: {items}, {moved}", self.name)
        } else {
            format!("job {}: {failed} of {items} failed; {moved}", self.name)
        }
    }

    /// The window text.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let line = |out: &mut String, s: &str| {
            out.push_str(s);
            out.push('\n');
        };
        line(&mut out, &format!("Job: {}", self.name));
        line(
            &mut out,
            &format!(
                "Result: {}",
                if self.cancelled() {
                    "CANCELLED".to_string()
                } else if self.failed_count() == 0 {
                    format!("OK — {} of {} items", self.ok_count(), self.items.len())
                } else {
                    format!(
                        "PROBLEMS — {} of {} items failed or were skipped",
                        self.failed_count(),
                        self.items.len()
                    )
                }
            ),
        );
        let secs = self.finished.saturating_sub(self.started);
        line(
            &mut out,
            &format!(
                "Started: {}   Finished: {}   Took: {}",
                crate::clipboard::format_ts(self.started),
                crate::clipboard::format_ts(self.finished),
                crate::narrate::human_secs(secs)
            ),
        );
        match self.kind {
            JobKind::Copy => {
                line(&mut out, &format!("Destination: {}", self.dest));
                line(
                    &mut out,
                    &format!("Mode: {} — {}", self.mode.label(), self.mode.description()),
                );
                if self.contents_only {
                    line(
                        &mut out,
                        "Contents only: each source's contents go straight into the destination.",
                    );
                }
                line(
                    &mut out,
                    &format!(
                        "Transferred: {} files, {}",
                        self.files(),
                        crate::listview::format_size(self.bytes())
                    ),
                );
            }
            JobKind::RusticBackup => {
                line(&mut out, &format!("Repository: {}", self.dest));
            }
        }
        if self.links {
            line(&mut out, "Symlinks and junctions: stored as links.");
        }
        if !self.excludes.is_empty() {
            line(&mut out, &format!("Excluded: {}", self.excludes.join(", ")));
        }
        out.push('\n');
        for (i, it) in self.items.iter().enumerate() {
            let status = match &it.outcome {
                Outcome::Ok => "OK".to_string(),
                Outcome::Unchanged => "OK — nothing changed, no new snapshot".to_string(),
                Outcome::Skipped(r) => format!("SKIPPED — {r}"),
                Outcome::Failed(r) => format!("FAILED — {r}"),
                Outcome::Cancelled => "CANCELLED".to_string(),
            };
            line(&mut out, &format!("{}. {}", i + 1, it.label));
            let mut facts = vec![status];
            if !it.engine.is_empty() {
                facts.push(it.engine.to_string());
            }
            if matches!(
                it.outcome,
                Outcome::Ok | Outcome::Failed(_) | Outcome::Cancelled
            ) && !it.engine.is_empty()
            {
                facts.push(format!(
                    "{} files, {} in {}",
                    it.files,
                    crate::listview::format_size(it.bytes),
                    crate::narrate::human_secs(it.secs)
                ));
            }
            line(&mut out, &format!("   {}", facts.join(" · ")));
            for d in &it.detail {
                line(&mut out, &format!("   {d}"));
            }
        }
        if let Some(p) = &self.saved_to {
            out.push('\n');
            line(&mut out, &format!("Saved to {}", p.display()));
        }
        out
    }
}

/// Lines a rustic summary contributes to its item's detail.
pub fn describe_backup(s: &BackupSummary) -> Vec<String> {
    let fmt = crate::listview::format_size;
    let mut out = Vec::new();
    match &s.snapshot_id {
        Some(id) => out.push(format!("Snapshot {}", &id[..id.len().min(8)])),
        None => out.push("No snapshot written: nothing changed since the previous one".into()),
    }
    out.push(format!(
        "Files: {} new, {} changed, {} unmodified ({} processed, {})",
        s.files_new,
        s.files_changed,
        s.files_unmodified,
        s.total_files_processed,
        fmt(s.total_bytes_processed)
    ));
    out.push(format!(
        "Folders: {} new, {} changed, {} unmodified",
        s.dirs_new, s.dirs_changed, s.dirs_unmodified
    ));
    out.push(format!(
        "Data added: {} ({} after compression) in {}",
        fmt(s.data_added),
        fmt(s.data_added_packed),
        crate::narrate::human_secs(s.total_duration.round() as u64)
    ));
    out
}

/// The transcript lines worth putting in a report: warnings and errors
/// first, then a capped slice of the rest, with a count of what was left
/// out so silence can't be read as "nothing happened".
pub fn transcript_detail(lines: &[(LogLevel, String)], max_info: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (lvl, msg) in lines {
        if matches!(
            lvl,
            LogLevel::Warning | LogLevel::Error | LogLevel::Critical
        ) {
            out.push(format!("{}: {msg}", level_word(*lvl)));
        }
    }
    let info: Vec<&String> = lines
        .iter()
        .filter(|(l, _)| !matches!(l, LogLevel::Warning | LogLevel::Error | LogLevel::Critical))
        .map(|(_, m)| m)
        .collect();
    for m in info.iter().take(max_info) {
        out.push((*m).clone());
    }
    if info.len() > max_info {
        out.push(format!("… and {} more log lines", info.len() - max_info));
    }
    out
}

fn level_word(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Warning => "warning",
        LogLevel::Error | LogLevel::Critical => "error",
        _ => "note",
    }
}

/// Write the report text next to the exe, under `job_reports/`. Returns
/// the path, or `None` if it could not be written (the window still
/// shows it).
pub fn save_report(name: &str, started: u64, text: &str) -> Option<PathBuf> {
    let dir = navigator_config::exe_dir().ok()?.join(REPORTS_DIR);
    std::fs::create_dir_all(&dir).ok()?;
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}_{started}.txt"));
    std::fs::write(&path, text).ok()?;
    Some(path)
}

/// Folder under `<exe_dir>` holding one text file per job run.
pub const REPORTS_DIR: &str = "job_reports";

/// One line for the jobs list.
pub fn job_label(j: &Job) -> String {
    let n = j.sources.iter().filter(|s| !s.trim().is_empty()).count();
    let what = if n == 1 {
        j.sources
            .iter()
            .find(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        format!("{n} items")
    };
    let how = match j.kind() {
        JobKind::RusticBackup => "rustic backup".to_string(),
        JobKind::Copy => j.mode.label().to_lowercase(),
    };
    let last = match &j.last_run {
        None => "never run".to_string(),
        Some(r) => format!(
            "last run {}: {}",
            crate::clipboard::format_ts(r.ts),
            if r.ok { "OK" } else { "problems" }
        ),
    };
    format!("{} — {what} → {} ({how}) — {last}", j.name, j.dest)
}

// --- Duplicate -----------------------------------------------------------

/// `name` with `tag` appended before a file's extension: `notes.txt` →
/// `notes-ab12cd34.txt`, `archive.tar.gz` → `archive.tar-ab12cd34.gz`.
/// A folder, an extensionless name and a dotfile take it at the end.
pub fn duplicate_name(name: &str, is_dir: bool, tag: &str) -> String {
    if !is_dir
        && let Some(dot) = name.rfind('.')
        && dot > 0
        && dot + 1 < name.len()
    {
        return format!("{}-{tag}{}", &name[..dot], &name[dot..]);
    }
    format!("{name}-{tag}")
}

/// Eight hex characters that will not repeat within a session: a mix of
/// the clock, the process id and a counter, scrambled.
pub fn short_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut x =
        nanos ^ ((std::process::id() as u64) << 32) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64 finaliser.
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    format!("{:08x}", (x as u32) ^ ((x >> 32) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(s: &str) -> NavPath {
        NavPath::new(s).unwrap()
    }

    fn job(sources: &[&str], dest: &str) -> Job {
        Job {
            name: "nightly".into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            dest: dest.into(),
            ..Job::default()
        }
    }

    #[test]
    fn item_names_cover_roots() {
        assert_eq!(item_name(&np(r"D:\photos")), "photos");
        assert_eq!(item_name(&np(r"D:\")), "D");
        assert_eq!(item_name(&np(r"d:\")), "D");
        assert_eq!(item_name(&np(r"\\host\share\")), "share");
        assert_eq!(item_name(&np("mac:")), "mac");
        assert_eq!(item_name(&np("mac:Downloads")), "Downloads");
    }

    #[test]
    fn display_round_trips_through_parse() {
        for s in [
            r"D:\photos",
            "mac:Downloads",
            r"rustic:D:\repo",
            r"\\host\share\x",
        ] {
            let p = parse_path(s).unwrap();
            assert_eq!(parse_path(&path_display(&p)).unwrap(), p, "{s}");
        }
        assert!(parse_path("").is_err());
        assert!(parse_path("relative\\thing").is_err());
    }

    #[test]
    fn folders_land_in_their_own_name_unless_contents_only() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("photos");
        std::fs::create_dir_all(&src).unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        let dest = np(r"T:\backups");
        let mut j = job(
            &[
                &src.to_string_lossy(),
                &file.to_string_lossy(),
                r"Q:\does\not\exist",
                "",
            ],
            r"T:\backups",
        );
        let (items, problems) = plan_copy(&j, &dest, |p| p.as_path().is_dir());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].dst, dest.join("photos"));
        assert!(items[0].is_dir);
        assert_eq!(items[1].dst, dest, "a file goes into the folder");
        assert!(!items[1].is_dir);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].1, "not found");

        j.contents_only = true;
        let (items, _) = plan_copy(&j, &dest, |p| p.as_path().is_dir());
        assert_eq!(items[0].dst, dest, "contents only: no folder per source");

        match copy_operation(&items[0], ConflictMode::Mirror) {
            Operation::SyncDir { dst, mode, .. } => {
                assert_eq!(dst, dest);
                assert_eq!(mode, ConflictMode::Mirror);
            }
            other => panic!("{other:?}"),
        }
        match copy_operation(&items[1], ConflictMode::Mirror) {
            Operation::Copy { mode, dest_dir, .. } => {
                assert_eq!(mode, ConflictMode::Replace, "a file cannot mirror");
                assert_eq!(dest_dir, dest);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn remote_sources_are_planned_without_a_stat() {
        let j = job(&["mac:Downloads"], r"T:\backups");
        let (items, problems) = plan_copy(&j, &np(r"T:\backups"), |_| true);
        assert!(problems.is_empty());
        assert_eq!(items[0].dst, np(r"T:\backups").join("Downloads"));
    }

    #[test]
    fn excludes_translate_per_engine() {
        let ex: Vec<String> = ["node_modules", " ", "#comment", "*.tmp", "cache/**", "a/b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            rclone_exclude_args(&ex),
            vec![
                "--exclude",
                "node_modules",
                "--exclude",
                "node_modules/**",
                "--exclude",
                "*.tmp",
                "--exclude",
                "*.tmp/**",
                "--exclude",
                "cache/**",
                "--exclude",
                "a/b",
            ]
        );
        assert_eq!(
            robocopy_exclude_flags(&ex),
            vec![
                "/XD",
                "node_modules",
                "*.tmp",
                "cache",
                "a\\b",
                "/XF",
                "node_modules",
                "*.tmp",
                "cache",
                "a\\b"
            ]
        );
        assert_eq!(
            rustic_exclude_globs(&ex),
            vec!["node_modules", "*.tmp", "cache", "a/b"]
        );
        assert!(robocopy_exclude_flags(&[]).is_empty());
        let mut j = job(&["x"], "y");
        j.excludes = vec!["n".into()];
        j.links = true;
        assert_eq!(
            rclone_job_args(&j),
            vec!["--exclude", "n", "--exclude", "n/**", "-l"]
        );
    }

    #[test]
    fn rustic_tags_lead_with_the_job_name() {
        let mut j = job(&["x"], r"rustic:D:\repo");
        j.tags = vec!["drive".into(), " ".into(), "data".into()];
        assert_eq!(rustic_tags(&j), vec!["job:nightly", "drive", "data"]);
        assert_eq!(split_tags("a, b;c  d"), vec!["a", "b", "c", "d"]);
        assert_eq!(split_lines("a\r\n\r\n  b \n"), vec!["a", "b"]);
    }

    #[test]
    fn report_summary_and_rendering_name_the_failures() {
        let j = job(&[r"D:\a", r"D:\b"], r"T:\backups");
        let mut r = Report::new(&j, 1_700_000_000);
        r.finished = r.started + 125;
        r.items.push(ItemReport {
            label: "D:\\a → T:\\backups\\a".into(),
            engine: "robocopy",
            outcome: Outcome::Ok,
            files: 10,
            bytes: 2048,
            secs: 100,
            detail: vec![],
        });
        r.items.push(ItemReport {
            label: "D:\\b → T:\\backups\\b".into(),
            engine: "rclone",
            outcome: Outcome::Failed("permission denied".into()),
            files: 0,
            bytes: 0,
            secs: 25,
            detail: vec!["error: Failed to copy: access denied".into()],
        });
        assert!(!r.all_ok());
        assert_eq!(
            r.summary(),
            "job nightly: 1 of 2 items failed; 10 files, 2.0 KB transferred"
        );
        let text = r.render();
        assert!(text.contains("PROBLEMS — 1 of 2 items"), "{text}");
        assert!(text.contains("2. D:\\b → T:\\backups\\b"), "{text}");
        assert!(text.contains("FAILED — permission denied"), "{text}");
        assert!(text.contains("Took: 2 minutes"), "{text}");
        assert!(text.contains("Mode: Update"), "{text}");

        let mut ok = Report::new(&j, 10);
        ok.items.push(r.items[0].clone());
        assert!(ok.all_ok());
        assert_eq!(
            ok.summary(),
            "job nightly done: 1 item, 10 files, 2.0 KB transferred"
        );
    }

    #[test]
    fn a_skipped_rustic_run_reads_as_unchanged() {
        let j = job(&[r"D:\a"], r"rustic:D:\repo");
        let mut r = Report::new(&j, 10);
        r.items.push(ItemReport {
            label: "1 source → rustic:D:\\repo".into(),
            engine: "rustic",
            outcome: Outcome::Unchanged,
            files: 0,
            bytes: 0,
            secs: 3,
            detail: describe_backup(&BackupSummary {
                files_unmodified: 4,
                total_files_processed: 4,
                ..BackupSummary::default()
            }),
        });
        assert!(r.all_ok());
        assert_eq!(
            r.summary(),
            "job nightly done: 1 item, nothing changed since the last snapshot"
        );
        let text = r.render();
        assert!(text.contains("No snapshot written"), "{text}");
        assert!(text.contains("Repository: rustic:D:\\repo"), "{text}");
    }

    #[test]
    fn transcript_puts_problems_first_and_caps_the_rest() {
        let lines: Vec<(LogLevel, String)> = (0..5)
            .map(|i| (LogLevel::Info, format!("copied {i}")))
            .chain(std::iter::once((LogLevel::Warning, "slow".into())))
            .chain(std::iter::once((LogLevel::Error, "denied".into())))
            .collect();
        let d = transcript_detail(&lines, 2);
        assert_eq!(
            d,
            vec![
                "warning: slow",
                "error: denied",
                "copied 0",
                "copied 1",
                "… and 3 more log lines"
            ]
        );
    }

    #[test]
    fn duplicate_names_keep_the_extension_last() {
        assert_eq!(
            duplicate_name("notes.txt", false, "ab12cd34"),
            "notes-ab12cd34.txt"
        );
        assert_eq!(
            duplicate_name("archive.tar.gz", false, "ab12cd34"),
            "archive.tar-ab12cd34.gz"
        );
        assert_eq!(
            duplicate_name("README", false, "ab12cd34"),
            "README-ab12cd34"
        );
        assert_eq!(
            duplicate_name(".gitignore", false, "ab12cd34"),
            ".gitignore-ab12cd34"
        );
        assert_eq!(
            duplicate_name("trailing.", false, "ab12cd34"),
            "trailing.-ab12cd34"
        );
        assert_eq!(
            duplicate_name("my.folder", true, "ab12cd34"),
            "my.folder-ab12cd34"
        );
    }

    #[test]
    fn short_uids_are_eight_hex_chars_and_distinct() {
        let a = short_uid();
        let b = short_uid();
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn job_labels_say_when_it_last_ran() {
        let mut j = job(&[r"D:\a", r"D:\b"], r"T:\backups");
        assert_eq!(
            job_label(&j),
            "nightly — 2 items → T:\\backups (update) — never run"
        );
        j.last_run = Some(navigator_config::JobRun {
            ts: 1_700_000_000,
            ok: false,
            summary: "x".into(),
        });
        assert!(job_label(&j).ends_with(": problems"));
        let r = job(&[r"D:\a"], r"rustic:D:\repo");
        assert!(job_label(&r).contains("D:\\a → rustic:D:\\repo (rustic backup)"));
    }
}
