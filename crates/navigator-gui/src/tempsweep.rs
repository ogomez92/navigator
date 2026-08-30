//! Reaping our own leftovers in `%TEMP%`.
//!
//! Three operations stage a file in the system temp directory: the
//! `--files-from` list a batched paste hands rclone
//! ([`crate::batch::TempList`]), the source list a detached shell copy
//! hands its helper process ([`crate::shell_op`]), and the `--log-file` an
//! elevated retry redirects rclone to ([`crate::elevated`]). Each of those
//! deletes its own file on the normal path, and that stays the primary
//! cleanup — this module is the backstop for the paths where a destructor
//! never runs:
//!
//! - `main` ends the process with `std::process::exit`, so a `TempList`
//!   still held by a worker at that moment is never dropped;
//! - the release profile is `panic = "abort"`, which unwinds nothing;
//! - a crash, a kill from Task Manager, or a machine losing power takes
//!   the file with it either way.
//!
//! So navigator sweeps at startup instead. The names carry the pid of the
//! process that wrote them, which is what makes this safe to do while a
//! peer instance is mid-copy: a file is only reaped once its owner is
//! gone. See [`is_stale`] for the whole rule.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Filename prefixes owned by navigator. Each producer builds its name
/// from the constant here rather than its own literal, so the sweeper and
/// the thing it sweeps cannot drift apart.
pub const FILES_FROM_PREFIX: &str = "navigator-files-from-";
pub const SHELL_OP_PREFIX: &str = "navigator-shellop-";
pub const ELEVATED_LOG_PREFIX: &str = "navigator-elevated-";

const PREFIXES: [&str; 3] = [FILES_FROM_PREFIX, SHELL_OP_PREFIX, ELEVATED_LOG_PREFIX];

/// How long a file is left alone regardless of who owns it.
///
/// A detached shell copy is handed its list and then *outlives its parent*
/// by design — the parent can exit within milliseconds of `CreateProcess`.
/// A second navigator starting in that window would see a list owned by an
/// already-dead pid and delete it out from under a helper that had not
/// read it yet. Ten minutes is far past the moment `run_helper` reads its
/// list, and far short of anything a user would notice.
const GRACE: Duration = Duration::from_secs(10 * 60);

/// Backstop for a pid the OS has since handed to some unrelated live
/// process. Nothing of ours reads one of these files days after writing
/// it — rclone reads `--files-from` once, at startup.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The pid embedded in one of our temp filenames, if the name is ours.
///
/// Every producer formats `<prefix><pid>-…` or `<prefix><pid>.log`, so the
/// owner is the run of digits directly after the prefix.
pub fn owner_pid(name: &str) -> Option<u32> {
    let rest = PREFIXES.iter().find_map(|p| name.strip_prefix(p))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Whether `name` may be deleted. Pure, so the rule is testable without a
/// temp directory or a real process table.
///
/// `alive` answers "is this pid running?"; `age` is how long ago the file
/// was last written. Anything not ours is kept, and so is anything young,
/// still owned by a running process, or written by this very process — an
/// in-flight paste of our own must survive its own startup sweep on the
/// day a recycled pid comes back around to us.
pub fn is_stale(name: &str, age: Duration, our_pid: u32, alive: impl Fn(u32) -> bool) -> bool {
    let Some(pid) = owner_pid(name) else {
        return false;
    };
    if age < GRACE {
        return false;
    }
    if age >= MAX_AGE {
        return true;
    }
    pid != our_pid && !alive(pid)
}

/// Delete every stale navigator artifact in `dir`. Returns how many went.
///
/// Errors are logged and skipped: the temp directory is shared with every
/// other program on the machine, so an entry we cannot stat or remove is
/// routine rather than a failure of the sweep.
pub fn sweep_dir(dir: &Path, our_pid: u32, alive: impl Fn(u32) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut reaped = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if owner_pid(name).is_none() {
            continue;
        }
        // Files only: every artifact is one, and a directory that happens
        // to match a prefix is not something we put there.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if !is_stale(name, age, our_pid, &alive) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {
                tracing::debug!("temp sweep: removed {:?}", entry.path());
                reaped += 1;
            }
            Err(e) => tracing::debug!("temp sweep: cannot remove {:?}: {}", entry.path(), e),
        }
    }
    reaped
}

/// Sweep `%TEMP%` on a background thread.
///
/// Off the UI thread because it is a `read_dir` of a directory that is
/// routinely enormous and may sit on a slow volume — the same rule the
/// rest of the app follows for unbounded IO. Fire-and-forget: nothing
/// waits for it and nothing is announced, since the user did not ask.
pub fn spawn() {
    if let Err(e) = std::thread::Builder::new()
        .name("navigator-temp-sweep".into())
        .spawn(|| {
            let n = sweep_dir(&std::env::temp_dir(), std::process::id(), pid_is_alive);
            if n > 0 {
                tracing::info!("temp sweep: removed {n} stale file(s)");
            }
        })
    {
        tracing::debug!("temp sweep: not started: {e}");
    }
}

/// Is `pid` a process that currently exists?
///
/// A handle we cannot open counts as dead, which is the safe way round:
/// the worst case is a file that survives one more startup.
fn pid_is_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code: u32 = 0;
        // A pid can be open-able while the process is already gone, so ask
        // for the exit code rather than trusting the handle alone.
        let running = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Duration = Duration::from_secs(3600);
    const DEAD: fn(u32) -> bool = |_| false;
    const LIVE: fn(u32) -> bool = |_| true;

    #[test]
    fn owner_pid_reads_every_producer_name() {
        assert_eq!(owner_pid("navigator-files-from-1234-7.txt"), Some(1234));
        assert_eq!(owner_pid("navigator-shellop-99-0.txt"), Some(99));
        assert_eq!(owner_pid("navigator-elevated-4321.log"), Some(4321));
    }

    /// The temp directory belongs to the whole machine. Anything we did
    /// not name is not ours to delete — including the test fixtures this
    /// repo's own suites leave behind, and other programs' work.
    #[test]
    fn foreign_files_are_never_touched() {
        assert_eq!(owner_pid("chrome_BITS_1234.tmp"), None);
        assert_eq!(owner_pid("navigator-preflight-99-0"), None);
        assert!(!is_stale("chrome_BITS_1234.tmp", OLD, 1, DEAD));
        assert!(!is_stale("navigator-preflight-99-0", OLD, 1, DEAD));
    }

    #[test]
    fn a_dead_owner_is_reaped_once_the_grace_period_passes() {
        assert!(is_stale("navigator-files-from-1234-0.txt", OLD, 1, DEAD));
    }

    /// The window this grace period exists for: a detached shell copy
    /// outlives the navigator that spawned it, so a list whose owner is
    /// already gone may still be seconds away from being read.
    #[test]
    fn a_fresh_file_survives_even_with_a_dead_owner() {
        let age = Duration::from_secs(5);
        assert!(!is_stale("navigator-shellop-1234-0.txt", age, 1, DEAD));
    }

    /// A peer instance mid-paste owns a live pid, and rclone is reading
    /// that very list.
    #[test]
    fn a_live_owner_keeps_its_file() {
        assert!(!is_stale("navigator-files-from-1234-0.txt", OLD, 1, LIVE));
    }

    /// Our own in-flight operation, in the case where the OS has reissued
    /// a previous run's pid to us.
    #[test]
    fn our_own_pid_is_never_reaped() {
        assert!(!is_stale(
            "navigator-files-from-1234-0.txt",
            OLD,
            1234,
            DEAD
        ));
    }

    /// Pids get reused, so "the owner is alive" cannot be the only way
    /// out or an unlucky file would sit in `%TEMP%` for good.
    #[test]
    fn age_overrides_a_live_but_reused_pid() {
        let age = MAX_AGE + Duration::from_secs(1);
        assert!(is_stale("navigator-files-from-1234-0.txt", age, 1, LIVE));
    }

    #[test]
    fn sweep_removes_only_the_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = tmp.path().join("navigator-files-from-1234-0.txt");
        let ours = tmp.path().join("navigator-files-from-4242-0.txt");
        let foreign = tmp.path().join("something-else.tmp");
        for p in [&stale, &ours, &foreign] {
            std::fs::write(p, b"x").unwrap();
        }
        // Backdate everything past the grace period.
        let old = SystemTime::now() - (GRACE + Duration::from_secs(60));
        for p in [&stale, &ours, &foreign] {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(old).unwrap();
        }

        assert_eq!(sweep_dir(tmp.path(), 4242, |_| false), 1);
        assert!(!stale.exists(), "a dead owner's list must go");
        assert!(ours.exists(), "our own list must survive");
        assert!(foreign.exists(), "another program's file is not ours");
    }
}
