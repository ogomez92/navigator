//! Saved jobs: "copy these items to that place", recorded once and run on
//! demand from Tools → Jobs. The in-app form of a backup alias — the kind
//! of `rclone sync d:\ sb:/home/vol/data` or `rustic backup` one-liner
//! people keep in a shell profile — without a scheduler: navigator is not
//! a daemon, so a job runs when the user asks and reports when it is done.
//!
//! Jobs live in `config.toml` as `[[jobs]]` so they travel with the rest
//! of the settings and are hand-editable. Paths are stored as the address
//! bar shows them (`D:\photos`, `mac:Downloads`, `rustic:D:\repo`) — the
//! GUI parses them back with `NavPath::new`, so a hand-edited entry can
//! use any spelling the address bar accepts.

use navigator_core::ConflictMode;
use serde::{Deserialize, Serialize};

/// What the destination of a job is, derived from its spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Copy every source into a folder (local, UNC or rclone remote) with
    /// a [`ConflictMode`], on the same engines a paste uses.
    Copy,
    /// `rustic backup` of every source into the repository named by a
    /// `rustic:<repo>` destination — one snapshot per run, tagged with the
    /// job's name.
    RusticBackup,
}

/// Prefix that marks a destination as a rustic repository. Same spelling
/// the address bar uses for browsing one, so a job can be created by
/// standing on the snapshot list.
pub const RUSTIC_DEST_PREFIX: &str = "rustic:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    /// Unique, user-chosen. Also the rustic tag (`job:<name>`).
    pub name: String,
    /// What to copy or back up, address-bar spelling, one per entry. A
    /// drive root (`D:\`) is allowed and lands in `<dest>\D` unless
    /// `contents_only` is set.
    pub sources: Vec<String>,
    /// Where it goes: a folder, or `rustic:<repo>`.
    pub dest: String,
    /// Copy jobs only. What happens to destination files that already
    /// exist; `Mirror` also deletes destination files the sources lack.
    pub mode: ConflictMode,
    /// Copy jobs only. Copy what is *inside* each source straight into the
    /// destination instead of into `<dest>\<source name>` — the
    /// `robocopy d:\ b:\data /MIR` shape. Several sources then share one
    /// folder, so with `Mirror` only one source is allowed (the second
    /// sync would delete the first's files).
    pub contents_only: bool,
    /// Store symlinks and junctions as links instead of skipping them
    /// (rclone `-l`, robocopy `/SL /SJ`). Rustic always stores links.
    pub links: bool,
    /// Names or globs to leave out, matched at any depth: `node_modules`,
    /// `*.tmp`, `System Volume Information`. Translated per engine by the
    /// GUI (`--exclude name` + `--exclude name/**`, `/XD` + `/XF`,
    /// `--iglob !name`).
    pub excludes: Vec<String>,
    /// Rustic jobs only: extra snapshot tags besides `job:<name>`.
    pub tags: Vec<String>,
    /// Outcome of the most recent run, for the picker. Written by the job
    /// runner, never by the editor.
    pub last_run: Option<JobRun>,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            name: String::new(),
            sources: Vec::new(),
            dest: String::new(),
            mode: ConflictMode::Update,
            contents_only: false,
            links: false,
            excludes: Vec::new(),
            tags: Vec::new(),
            last_run: None,
        }
    }
}

/// How the last run of a job went — enough for a one-line label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JobRun {
    /// Unix seconds when the run finished.
    pub ts: u64,
    /// Every item succeeded.
    pub ok: bool,
    /// One sentence: `3 items, 1.2 GB transferred` / `1 of 3 failed`.
    pub summary: String,
}

impl Job {
    pub fn kind(&self) -> JobKind {
        if is_rustic_dest(&self.dest) {
            JobKind::RusticBackup
        } else {
            JobKind::Copy
        }
    }

    /// The repository spec of a rustic job (`D:\repo`,
    /// `rclone:sb:/home/rustic`), or `None` for a copy job.
    pub fn rustic_repo(&self) -> Option<&str> {
        self.dest
            .strip_prefix(RUSTIC_DEST_PREFIX)
            .map(str::trim)
            .filter(|r| !r.is_empty())
    }

    /// Why this job cannot be saved, if anything. `taken` lists the names
    /// of the other jobs. Pure, so the dialog and a hand-edited config are
    /// judged by the same rules.
    pub fn validate(&self, taken: &[String]) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("the job needs a name".into());
        }
        if taken.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            return Err(format!("a job called {name:?} already exists"));
        }
        if self.sources.iter().all(|s| s.trim().is_empty()) {
            return Err("the job has no sources".into());
        }
        if self.dest.trim().is_empty() {
            return Err("the job needs a destination".into());
        }
        if self.dest.trim().starts_with(RUSTIC_DEST_PREFIX) && self.rustic_repo().is_none() {
            return Err("the rustic destination names no repository".into());
        }
        if self.kind() == JobKind::Copy
            && self.contents_only
            && self.mode == ConflictMode::Mirror
            && self.sources.iter().filter(|s| !s.trim().is_empty()).count() > 1
        {
            return Err(
                "Mirror with \"contents only\" takes one source: the second source would delete the first's files"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Whether a destination string names a rustic repository rather than a
/// folder. `rustic:` followed by nothing is not one.
pub fn is_rustic_dest(dest: &str) -> bool {
    dest.strip_prefix(RUSTIC_DEST_PREFIX)
        .is_some_and(|r| !r.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(dest: &str) -> Job {
        Job {
            name: "nightly".into(),
            sources: vec![r"D:\photos".into()],
            dest: dest.into(),
            ..Job::default()
        }
    }

    #[test]
    fn kind_follows_the_destination_spelling() {
        assert_eq!(job(r"T:\backups").kind(), JobKind::Copy);
        assert_eq!(job("sb:/home/vol").kind(), JobKind::Copy);
        assert_eq!(job(r"rustic:D:\repo").kind(), JobKind::RusticBackup);
        assert_eq!(
            job("rustic:rclone:sb:/home/rustic").rustic_repo(),
            Some("rclone:sb:/home/rustic")
        );
        // A bare prefix is not a repository.
        assert_eq!(job("rustic:").kind(), JobKind::Copy);
        assert!(job("rustic:").validate(&[]).is_err());
    }

    #[test]
    fn validation_catches_the_empty_and_the_duplicate() {
        let mut j = job(r"T:\backups");
        assert!(j.validate(&[]).is_ok());
        assert!(j.validate(&["Nightly".into()]).is_err(), "names fold case");
        j.name.clear();
        assert!(j.validate(&[]).is_err());
        let mut j = job(r"T:\backups");
        j.sources = vec!["  ".into()];
        assert!(j.validate(&[]).is_err());
        let mut j = job("");
        j.dest.clear();
        assert!(j.validate(&[]).is_err());
    }

    #[test]
    fn mirror_into_one_folder_takes_a_single_source() {
        let mut j = job(r"T:\data");
        j.contents_only = true;
        j.mode = ConflictMode::Mirror;
        assert!(
            j.validate(&[]).is_ok(),
            "one source is the backup_drive shape"
        );
        j.sources.push(r"E:\".into());
        assert!(j.validate(&[]).is_err());
        j.mode = ConflictMode::Update;
        assert!(
            j.validate(&[]).is_ok(),
            "merging two sources is fine without pruning"
        );
    }

    #[test]
    fn a_partial_toml_entry_fills_in_defaults() {
        let j: Job = toml::from_str(
            r#"
            name = "songs"
            sources = ['d:\radio\songs']
            dest = "sb:/home/radio/songs"
            "#,
        )
        .unwrap();
        assert_eq!(j.mode, ConflictMode::Update);
        assert!(!j.contents_only && !j.links);
        assert!(j.excludes.is_empty() && j.tags.is_empty());
        assert!(j.last_run.is_none());
    }
}
