//! Archive extraction via the standalone `7z.exe` (found on `PATH`, or
//! failing that in the default 7-Zip install dirs — see [`find_7z`]).
//!
//! Pure logic — extension classification, top-level entry parsing, and
//! the wrapper-folder decision — lives here so it can be unit-tested
//! without spawning processes. The actual extraction worker
//! ([`run_extract`]) is also here but receives every dependency
//! (speech sender, config snapshot, 7z path) by value so it can be
//! exercised from `op_extract` in `app.rs` without touching `AppState`.
//!
//! 7z is shelled out to rather than linked because the standalone
//! binary already supports every format the user cares about, ships
//! with a stable command-line, and keeps us off LGPL/etc. dependencies.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crossbeam_channel::Sender;
use tracing::warn;

use navigator_config::Extraction;
use navigator_core::NavPath;

use crate::speech::Utterance;

/// Lowercase, dot-less extensions the bundled 7-Zip can open. Sourced
/// from the upstream "Supported formats" list. We err on the side of
/// inclusion — if 7-Zip refuses an obscure container we surface the
/// failure as a normal extract error instead of pre-filtering it out.
pub const EXTRACTABLE_EXTENSIONS: &[&str] = &[
    // Native 7-Zip + the common general-purpose archives.
    "7z", "zip", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "tbz", "xz", "txz", "lzma", "tlz", "lz",
    "lz4", "zst", "zstd", "tzst", // Microsoft / installer formats.
    "cab", "msi", "msm", "msp", "wim", "swm", "esd", "exe",
    // Legacy Unix / minor archivers.
    "arj", "lzh", "lha", "z", "taz", "rpm", "deb", "cpio", "ar", "xar", "pkg", "cpgz", "chm",
    "epub", "apk", "jar", "war", "ear", "xpi", "ipa", "ppmd",
    // Disk images / filesystems 7-Zip exposes as archives.
    "iso", "img", "dmg", "hfs", "ntfs", "fat", "vhd", "vhdx", "vmdk", "vdi", "qcow", "qcow2", "udf",
    "squashfs", "cramfs", "ext", "ext2", "ext3", "ext4", "apm", "mbr", "gpt",
];

/// True if `name`'s final extension is one of `set`. Case-insensitive;
/// `set` must be lowercase and dotless (pinned by
/// `extractable_extensions_are_lowercase_and_dotless`).
fn has_ext(name: &str, set: &[&str]) -> bool {
    let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    set.contains(&ext.to_ascii_lowercase().as_str())
}

/// True if `path` has an extension recognised by
/// [`EXTRACTABLE_EXTENSIONS`], or is a numbered split volume such as
/// `movie.7z.001` (see [`split_volume`]) — whose extension is `001`, in
/// no extension list anywhere, which is exactly why that case is asked
/// first rather than folded into the table.
/// Case-insensitive on the extension; the path itself is not touched.
pub fn is_extractable(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    split_volume(name).is_some() || has_ext(name, EXTRACTABLE_EXTENSIONS)
}

/// The subset of [`EXTRACTABLE_EXTENSIONS`] the *recursive sweep* is
/// allowed to pick up. Everything here is unambiguously a container
/// whose whole reason to exist is to be unpacked.
///
/// The full list is deliberately wider than this one, and that width is
/// only safe for an item the user pointed at directly. 7-Zip will
/// happily open an `.exe` (SFX), a `.jar`, an `.msi`, an `.apk`, an
/// `.iso` — so a sweep of a source tree or a program folder would
/// "extract" every executable and library it found, and with
/// `[extraction] delete_when_extracted` on by default it would then
/// delete them. A recursive gesture must not be able to do that, so the
/// sweep sees archives only. Selecting the `.exe` itself still works.
pub const SWEEP_EXTENSIONS: &[&str] = &[
    "7z", "zip", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "tbz", "xz", "txz", "lzma", "tlz", "lz",
    "lz4", "zst", "zstd", "tzst", "cab", "arj", "lzh", "lha", "z", "taz", "cpio", "cpgz", "xar",
];

/// True if `path` is an archive the recursive sweep may collect. Always
/// implies [`is_extractable`]; the reverse does not hold.
pub fn is_sweepable(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    // A split set is swept only when what it joins back into is itself a
    // sweepable archive: `foo.7z.001` yes, `foo.mkv.001` no. Joining a
    // split media file is a fine thing to ask for by pointing at it, but
    // a recursive walk must not decide to do it and then — with the
    // default `delete_when_extracted` — purge the parts.
    match split_volume(name) {
        Some((base, _)) => has_ext(base, SWEEP_EXTENSIONS),
        None => has_ext(name, SWEEP_EXTENSIONS),
    }
}

/// Split `name` into `(stem, volume number)` when it is a modern
/// multi-volume rar part — `movie.part03.rar` → `("movie", 3)`.
/// Case-insensitive; `None` for an ordinary `.rar`.
pub fn rar_part(name: &str) -> Option<(&str, u32)> {
    let lower = name.to_ascii_lowercase();
    let body = lower.strip_suffix(".rar")?;
    let dot = body.rfind('.')?;
    let tail = &body[dot + 1..];
    let digits = tail.strip_prefix("part")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&name[..dot], digits.parse().ok()?))
}

/// Split `name` into `(stem, volume number)` when it is an *old-style*
/// rar continuation volume — `movie.r07` → `("movie", 7)`. These are the
/// files that accompany a plain `movie.rar`; 7-Zip finds them itself
/// from the first volume, so they are never extracted on their own.
fn old_rar_volume(name: &str) -> Option<(&str, u32)> {
    let dot = name.rfind('.')?;
    let tail = &name[dot + 1..];
    let digits = tail.strip_prefix(['r', 'R'])?;
    if digits.len() < 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&name[..dot], digits.parse().ok()?))
}

/// Split `name` into `(base, volume number)` when it is a numbered
/// *split* volume — `movie.7z.001` → `("movie.7z", 1)`, `data.001` →
/// `("data", 1)`. This is 7-Zip's `-v` output (and what every download
/// site splits large uploads into); the base keeps its own extension
/// because it is the file the parts join back into.
///
/// Three digits minimum, which is what 7-Zip writes (`.001`…`.999`,
/// then `.1000`). Requiring them keeps an ordinary `report.2` or a
/// versioned `lib.so.6` out of the archive set.
pub fn split_volume(name: &str) -> Option<(&str, u32)> {
    let dot = name.rfind('.')?;
    let tail = &name[dot + 1..];
    if tail.len() < 3 || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let base = &name[..dot];
    if base.is_empty() {
        return None;
    }
    Some((base, tail.parse().ok()?))
}

/// True for a multi-volume part that is *not* the first one. The sweep
/// skips these: 7-Zip pulls the whole set in from part 1, so extracting
/// them individually is one guaranteed failure per part — and, worse,
/// each failure reports as a failed archive the user has to interpret.
pub fn is_continuation_volume(name: &str) -> bool {
    rar_part(name).is_some_and(|(_, n)| n != 1) || split_volume(name).is_some_and(|(_, n)| n != 1)
}

/// Which multi-volume set `name` belongs to, as `(set, volume number)`.
/// The set string carries its scheme so a `movie.part1.rar` and a
/// `movie.002` sharing a folder can't be read as parts of each other.
fn volume_key(name: &str) -> Option<(String, u32)> {
    if let Some((stem, n)) = rar_part(name) {
        return Some((format!("rar:{}", stem.to_ascii_lowercase()), n));
    }
    let (base, n) = split_volume(name)?;
    Some((format!("split:{}", base.to_ascii_lowercase()), n))
}

/// Every filename that belongs to `archive_name`'s volume set, given the
/// names sitting next to it. Deleting after a successful extraction has
/// to remove the *set*, not the one file 7-Zip was pointed at: purging
/// `movie.part1.rar` alone leaves eight orphaned parts that are now
/// unopenable, which is the same as not purging at all.
///
/// Pure — the caller does the one `read_dir` and passes the names in.
/// The returned list always contains `archive_name` itself.
pub fn volume_set<'a>(
    archive_name: &str,
    siblings: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut out = vec![archive_name.to_string()];
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    if let Some((base, _)) = split_volume(archive_name) {
        // `base.001` — collect every other numbered part of the same base.
        for name in siblings {
            if eq(name, archive_name) {
                continue;
            }
            if split_volume(name).is_some_and(|(b, _)| eq(b, base)) {
                out.push(name.to_string());
            }
        }
    } else if let Some((stem, _)) = rar_part(archive_name) {
        // `stem.partN.rar` — collect every other part of the same stem.
        for name in siblings {
            if eq(name, archive_name) {
                continue;
            }
            if rar_part(name).is_some_and(|(s, _)| eq(s, stem)) {
                out.push(name.to_string());
            }
        }
    } else if archive_name.to_ascii_lowercase().ends_with(".rar") {
        // Plain `stem.rar` — old-style continuations are `stem.r00`…
        let stem = &archive_name[..archive_name.len() - 4];
        for name in siblings {
            if eq(name, archive_name) {
                continue;
            }
            if old_rar_volume(name).is_some_and(|(s, _)| eq(s, stem)) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Walk `root` depth-first and collect every archive the sweep is
/// allowed to take (see [`SWEEP_EXTENSIONS`]). Unreadable subtrees are
/// skipped rather than failing the whole walk.
///
/// Reparse points are never descended: `DirEntry::file_type` reports a
/// junction or symlink as neither file nor directory, so a self-
/// referential link can't spin the walk forever. Same reasoning as
/// `navigator_fs::search_recursive`.
///
/// Unbounded IO — worker threads only, never the message pump.
pub fn sweep_archives(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue; // unreadable sub-tree; skip
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && is_sweepable(&path) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !is_continuation_volume(&name) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

/// Locate `7z.exe`. Tries `PATH` first, then the well-known 7-Zip
/// install directories. Returns `None` if not installed.
///
/// The fallback exists because `PATH` here is the environment block this
/// process *inherited at launch*. Windows never refreshes a running
/// process's block when `PATH` changes — it only broadcasts
/// `WM_SETTINGCHANGE` — so a navigator started from a shell/Explorer
/// session that predates the 7-Zip install sees a stale `PATH` and the
/// lookup fails even though a fresh shell finds 7z fine. Checking the
/// standard install dirs makes Extract work anyway.
pub fn find_7z() -> Option<PathBuf> {
    // Cached: the lookup stats every directory on PATH, and both callers
    // (`op_extract`, `op_zip`) run it on the UI thread purely to decide
    // whether to report "7z not found" before spawning their worker.
    // 7-Zip does not move mid-session; a user who installs it while
    // navigator is running restarts, same as for any other PATH change.
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            which_in_path("7z.exe")
                .or_else(|| which_in_path("7z"))
                .or_else(find_7z_in_install_dirs)
        })
        .clone()
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Directories a default 7-Zip install writes `7z.exe` into, in the
/// order we should prefer them (64-bit machine-wide, then 32-bit, then
/// the per-user install the MSI offers). Built from environment
/// variables rather than hardcoded `C:\` so localized / relocated
/// `Program Files` still resolve.
fn install_dir_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            dirs.push(PathBuf::from(base).join("7-Zip"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Programs").join("7-Zip"));
    }
    dirs.dedup();
    dirs
}

fn find_7z_in_install_dirs() -> Option<PathBuf> {
    install_dir_candidates()
        .into_iter()
        .map(|d| d.join("7z.exe"))
        .find(|c| c.is_file())
}

/// Parse the stdout of `7z l -slt -ba -- archive` to count distinct
/// top-level entries. Each entry block contains a `Path = ...` line; we
/// take the first segment (split on `/` or `\`) and dedupe. The single
/// returned name (when the count is 1) lets the caller decide whether
/// the wrapper would just duplicate the archive's own folder.
pub fn parse_top_level_count(stdout: &str) -> (usize, Option<String>) {
    use std::collections::BTreeSet;
    let mut tops: BTreeSet<String> = BTreeSet::new();
    for raw in stdout.lines() {
        let line = raw.trim_start();
        let Some(rest) = line.strip_prefix("Path = ") else {
            continue;
        };
        let first = rest.trim().split(['/', '\\']).next().unwrap_or("");
        if !first.is_empty() {
            tops.insert(first.to_string());
        }
    }
    let n = tops.len();
    let only = if n == 1 {
        tops.into_iter().next()
    } else {
        None
    };
    (n, only)
}

/// Strip every recognised archive extension off `archive` so layered
/// names like `foo.tar.gz` collapse to `foo` instead of `foo.tar`.
/// Falls back to whatever the OS returns from `file_stem` for
/// unrecognised extensions.
pub fn archive_stem(archive: &Path) -> String {
    let mut name: String = archive
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    loop {
        let p = Path::new(&name);
        if !is_extractable(p) {
            break;
        }
        match p.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() && s != name => name = s.to_string(),
            _ => break,
        }
    }
    // `movie.part1.rar` has already lost its `.rar`; leaving `.part1` on
    // the wrapper folder names the extraction after one volume of a set
    // that produced all of them.
    if let Some(dot) = name.rfind('.') {
        let tail = name[dot + 1..].to_ascii_lowercase();
        if let Some(digits) = tail.strip_prefix("part")
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            name.truncate(dot);
        }
    }
    name
}

/// Pick the directory `7z x -o<dest>` should write into for a given
/// archive. Pure (no IO except the dedupe collision check the caller
/// must do separately if it wants conflict-free output).
///
/// Rules:
///   * `create_folder = false` → extract straight into `parent_dir`.
///   * Archive already wraps (top_level_count <= 1) → extract straight
///     into `parent_dir` so we don't get the `name/name/...` double.
///   * Otherwise wrap in `parent_dir/<archive_stem>`.
///
/// Conflict avoidance lives in [`unique_dest`] so this function stays
/// pure and trivially testable.
pub fn decide_dest(
    archive: &Path,
    parent_dir: &Path,
    top_level_count: usize,
    create_folder: bool,
) -> PathBuf {
    if !create_folder || top_level_count <= 1 {
        return parent_dir.to_path_buf();
    }
    parent_dir.join(archive_stem(archive))
}

/// Append ` (n)` until the path no longer exists. Used so a second
/// extraction of the same archive doesn't merge into the previous
/// extracted tree. Caps at 999 attempts; if every slot is taken the
/// raw candidate is returned and 7z will overwrite (its `-y` flag is
/// passed by the worker anyway).
pub fn unique_dest(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let parent = match candidate.parent() {
        Some(p) => p.to_path_buf(),
        None => return candidate,
    };
    let stem = candidate
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted")
        .to_string();
    for n in 1..1000 {
        let c = parent.join(format!("{} ({})", stem, n));
        if !c.exists() {
            return c;
        }
    }
    candidate
}

/// Drop the continuation volumes of every set whose *first* part is also
/// in the list. Selecting the whole folder is the normal gesture when it
/// holds nothing but `movie.7z.001`…`.008`, and 7-Zip has already pulled
/// those parts in from the first one — so without this the user gets one
/// real extraction followed by seven guaranteed failures to interpret.
///
/// A continuation picked *without* its first part is kept, so the error
/// names the file the user actually chose instead of the selection
/// coming back empty as "no extractable archives selected".
fn drop_covered_volumes(archives: Vec<NavPath>) -> Vec<NavPath> {
    let firsts: std::collections::HashSet<String> = archives
        .iter()
        .filter_map(|p| volume_key(p.file_name()))
        .filter(|(_, n)| *n == 1)
        .map(|(set, _)| set)
        .collect();
    if firsts.is_empty() {
        return archives;
    }
    archives
        .into_iter()
        .filter(|p| match volume_key(p.file_name()) {
            Some((set, n)) => n == 1 || !firsts.contains(&set),
            None => true,
        })
        .collect()
}

/// Split an Extract selection into the archives to unpack directly and
/// the folders to sweep recursively.
///
/// `items` is `(path, is_dir)` straight off the model, so the directory
/// flag costs no `stat` — it is what the listing already told us. Files
/// keep the *full* [`EXTRACTABLE_EXTENSIONS`] set (the user pointed at
/// that exact file); folders become sweep roots, which see only
/// [`SWEEP_EXTENSIONS`].
///
/// Pure: the walk itself is [`sweep_archives`], on a worker.
pub fn split_extract_selection(items: &[(NavPath, bool)]) -> (Vec<NavPath>, Vec<NavPath>) {
    let mut archives = Vec::new();
    let mut roots = Vec::new();
    for (path, is_dir) in items {
        if *is_dir {
            roots.push(path.clone());
        } else if is_extractable(path.as_path()) {
            archives.push(path.clone());
        }
    }
    let archives = drop_covered_volumes(archives);
    (archives, roots)
}

/// Merge directly-selected archives with swept ones into the final job
/// list. Sorted and deduplicated, because two nested sweep roots (or a
/// root plus an archive inside it) would otherwise queue the same file
/// twice — and the second pass would extract an archive the first one
/// had already deleted.
pub fn merge_targets(direct: Vec<NavPath>, swept: Vec<NavPath>) -> Vec<NavPath> {
    let mut all: Vec<NavPath> = direct;
    all.extend(swept);
    all.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    all.dedup_by(|a, b| a.as_path() == b.as_path());
    all
}

/// The files to delete once `archive` has been extracted: the archive
/// plus the rest of its volume set. One `read_dir` of the parent feeds
/// the pure [`volume_set`]; if the directory can't be read we fall back
/// to the archive alone rather than guessing at names.
pub fn purge_targets(archive: &Path) -> Vec<PathBuf> {
    let Some(parent) = archive.parent() else {
        return vec![archive.to_path_buf()];
    };
    let Some(name) = archive.file_name().and_then(|s| s.to_str()) else {
        return vec![archive.to_path_buf()];
    };
    let Ok(rd) = std::fs::read_dir(parent) else {
        return vec![archive.to_path_buf()];
    };
    let siblings: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    volume_set(name, siblings.iter().map(|s| s.as_str()))
        .into_iter()
        .map(|n| parent.join(n))
        .collect()
}

/// How many swept archives the confirmation lists by name before it
/// summarises the rest. Long enough to recognise a folder's worth of
/// downloads, short enough that a screen reader reaches the buttons.
const SWEEP_PREVIEW_LIMIT: usize = 12;

/// Render the confirmation body for a recursive extract.
///
/// Each archive is shown relative to the sweep root it came from, so a
/// tree of same-named `archive.zip` files is still distinguishable. Pure
/// (it does no IO) so the wording and the truncation are unit-testable.
pub fn sweep_confirm_body(targets: &[NavPath], roots: &[NavPath], delete_after: bool) -> String {
    let where_ = roots
        .iter()
        .map(|r| r.file_name().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut body = format!(
        "Extract {} archive{} found in {}?\n\n",
        targets.len(),
        if targets.len() == 1 { "" } else { "s" },
        if where_.is_empty() {
            "the selection"
        } else {
            &where_
        },
    );
    for t in targets.iter().take(SWEEP_PREVIEW_LIMIT) {
        body.push_str(&format!("\u{2022} {}\n", sweep_label(t, roots)));
    }
    if targets.len() > SWEEP_PREVIEW_LIMIT {
        body.push_str(&format!(
            "\u{2026} and {} more\n",
            targets.len() - SWEEP_PREVIEW_LIMIT,
        ));
    }
    body.push_str("\nEach one is extracted next to itself.");
    if delete_after {
        // The purge is a plain `remove_file`, not a move to `.trash` -
        // there is no undo, so the dialog has to say so.
        body.push_str("\nArchives are deleted afterwards; this cannot be undone.");
    }
    body
}

/// Display name for one swept archive: its path relative to whichever
/// sweep root contains it, falling back to the bare filename.
fn sweep_label(target: &NavPath, roots: &[NavPath]) -> String {
    for r in roots {
        if let Ok(rel) = target.as_path().strip_prefix(r.as_path()) {
            return format!("{}\\{}", r.file_name(), rel.display());
        }
    }
    target.file_name().to_string()
}

/// Extract every entry in `sources` using `seven_zip`, each one into its
/// own parent directory — so a list swept out of a folder tree unpacks in
/// place. Reports progress (`extracting file X of Y: name`) via `speech`
/// and announces a final summary.
///
/// On per-archive success and `opts.delete_when_extracted`, removes the
/// archive *and the rest of its volume set* (see [`volume_set`]).
/// Failures never trigger a delete.
///
/// Designed to run on a worker thread; takes everything by value so it
/// holds no references back into `AppState`.
pub fn run_extract(
    sources: Vec<NavPath>,
    opts: Extraction,
    seven_zip: PathBuf,
    speech: Sender<Utterance>,
    sound: crate::sound::SoundPlayer,
) {
    let total = sources.len();
    let mut ok = 0usize;
    let mut failed = 0usize;

    for (i, src) in sources.iter().enumerate() {
        let label = src.file_name().to_string();
        let _ = speech.try_send(Utterance {
            text: format!("extracting file {} of {}: {}", i + 1, total, label),
            interrupt: false,
        });

        let archive_path = src.as_path().to_path_buf();
        let parent = src
            .parent()
            .map(|p| p.as_path().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let top_count = match list_top_level_count(&seven_zip, &archive_path) {
            Ok(n) => n,
            Err(e) => {
                warn!("7z list {:?}: {}", archive_path, e);
                let _ = speech.try_send(Utterance {
                    text: format!("listing {} failed", label),
                    interrupt: true,
                });
                failed += 1;
                continue;
            }
        };

        let raw_dest = decide_dest(&archive_path, &parent, top_count, opts.create_folder);
        // Only dedupe when we're creating a wrapper folder; extracting
        // straight into the cwd would otherwise spawn endless dupes.
        let wrapping = raw_dest != parent;
        let dest = if wrapping {
            unique_dest(raw_dest)
        } else {
            raw_dest
        };

        if wrapping && let Err(e) = std::fs::create_dir_all(&dest) {
            warn!("create_dir_all {:?}: {}", dest, e);
            let _ = speech.try_send(Utterance {
                text: format!("can't create folder for {}", label),
                interrupt: true,
            });
            failed += 1;
            continue;
        }

        let mut cmd = Command::new(&seven_zip);
        cmd.arg("x")
            .arg("-y")
            .arg(format!("-o{}", dest.display()))
            .arg("--")
            .arg(&archive_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_console(&mut cmd);
        let status = run_child(&mut cmd);

        match status {
            Ok(s) if s.success() => {
                ok += 1;
                if opts.delete_when_extracted {
                    // Delete the whole volume set, not just the file 7z
                    // was pointed at — see `volume_set`.
                    for victim in purge_targets(&archive_path) {
                        if let Err(e) = std::fs::remove_file(&victim) {
                            warn!("delete {:?}: {}", victim, e);
                            let _ = speech.try_send(Utterance {
                                text: format!("extracted {} but couldn't delete archive", label),
                                interrupt: true,
                            });
                        }
                    }
                }
            }
            Ok(s) => {
                warn!("7z exit {} for {:?}", s.code().unwrap_or(-1), archive_path);
                failed += 1;
                let _ = speech.try_send(Utterance {
                    text: format!("extracting {} failed", label),
                    interrupt: true,
                });
            }
            Err(e) => {
                warn!("7z spawn: {}", e);
                failed += 1;
                let _ = speech.try_send(Utterance {
                    text: format!("7z failed to start: {}", e),
                    interrupt: true,
                });
            }
        }
    }

    sound.play(if failed == 0 {
        navigator_config::SoundEvent::ExtractDone
    } else {
        navigator_config::SoundEvent::Error
    });
    let summary = if failed == 0 {
        format!("extracted {} of {}", ok, total)
    } else {
        format!("extracted {} of {}, {} failed", ok, total, failed)
    };
    let _ = speech.try_send(Utterance {
        text: summary,
        interrupt: failed > 0,
    });
}

/// Pick the `.zip` path produced when compressing `src`. The name is the
/// source's file stem with a `.zip` extension, placed next to the source —
/// e.g. `report.txt` → `report.zip`, folder `docs` → `docs.zip` (Explorer's
/// "Send to → Compressed folder" parity). Pure; collision avoidance is the
/// caller's job via [`unique_dest`].
pub fn zip_dest(src: &Path) -> PathBuf {
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .or_else(|| src.file_name().and_then(|s| s.to_str()))
        .unwrap_or("archive");
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.zip"))
}

/// The `7z a` invocation a zip operation resolves to. Built by [`plan_zip`]
/// so the cwd / inputs / dest decision is unit-testable without spawning 7z.
#[derive(Debug, PartialEq, Eq)]
pub struct ZipPlan {
    /// Directory 7z runs from (`current_dir`); inputs are relative to it.
    pub cwd: PathBuf,
    /// Output archive (sibling of the primary item). Pre-dedupe — the
    /// worker still runs it through [`unique_dest`].
    pub dest: PathBuf,
    /// Source arguments passed to 7z, relative to `cwd`.
    pub inputs: Vec<OsString>,
}

/// Decide how to compress a selection into a single `.zip`.
///
/// * `primary` — the entry the archive is named after (the focused row);
///   `dest` is always its sibling `<stem>.zip`.
/// * `single_folder` — true *only* when the selection is exactly one
///   directory. Then 7z runs from inside that folder and adds `*`, so the
///   folder's contents land at the archive root (no wrapping subfolder).
/// * Otherwise (2+ items, or one folder plus files, or a lone file) every
///   entry in `items` is added by basename from the common parent, so each
///   folder shows up as a subfolder inside the one archive.
///
/// Pure — collision avoidance is the caller's job via [`unique_dest`].
pub fn plan_zip(primary: &Path, items: &[PathBuf], single_folder: bool) -> ZipPlan {
    let dest = zip_dest(primary);
    if single_folder {
        ZipPlan {
            cwd: primary.to_path_buf(),
            dest,
            inputs: vec![OsString::from("*")],
        }
    } else {
        let parent = primary
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let inputs = items
            .iter()
            .filter_map(|p| p.file_name().map(|s| s.to_os_string()))
            .collect();
        ZipPlan {
            cwd: parent,
            dest,
            inputs,
        }
    }
}

/// Compress `items` into a single sibling `.zip` using `seven_zip`. A lone
/// selected folder is zipped by its contents (root of the archive); any
/// other selection keeps each item's name (folders become subfolders).
/// `primary` (the focused entry) names the archive. Originals are NEVER
/// deleted. Announces the result via `speech`. Takes everything by value so
/// it holds no `AppState` reference.
pub fn run_zip(
    items: Vec<NavPath>,
    primary: NavPath,
    seven_zip: PathBuf,
    speech: Sender<Utterance>,
    sound: crate::sound::SoundPlayer,
) {
    let item_paths: Vec<PathBuf> = items.iter().map(|p| p.as_path().to_path_buf()).collect();
    let single_folder = item_paths.len() == 1 && item_paths[0].is_dir();

    let plan = plan_zip(primary.as_path(), &item_paths, single_folder);
    let dest = unique_dest(plan.dest);
    let dest_label = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("archive.zip")
        .to_string();

    let mut cmd = Command::new(&seven_zip);
    cmd.current_dir(&plan.cwd)
        .arg("a")
        .arg("-tzip")
        .arg("-y")
        .arg("--")
        .arg(&dest);
    for input in &plan.inputs {
        cmd.arg(input);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    no_console(&mut cmd);

    match run_child(&mut cmd) {
        Ok(s) if s.success() => {
            sound.play(navigator_config::SoundEvent::ZipDone);
            let _ = speech.try_send(Utterance {
                text: format!("created {}", dest_label),
                interrupt: false,
            });
        }
        Ok(s) => {
            warn!("7z zip exit {} into {:?}", s.code().unwrap_or(-1), dest);
            sound.play(navigator_config::SoundEvent::Error);
            let _ = speech.try_send(Utterance {
                text: "zip failed".into(),
                interrupt: true,
            });
        }
        Err(e) => {
            warn!("7z spawn: {}", e);
            sound.play(navigator_config::SoundEvent::Error);
            let _ = speech.try_send(Utterance {
                text: format!("7z failed to start: {}", e),
                interrupt: true,
            });
        }
    }
}

fn list_top_level_count(seven_zip: &Path, archive: &Path) -> std::io::Result<usize> {
    let mut cmd = Command::new(seven_zip);
    cmd.arg("l")
        .arg("-slt")
        .arg("-ba")
        .arg("--")
        .arg(archive)
        .stderr(Stdio::null());
    no_console(&mut cmd);
    cmd.stdout(Stdio::piped());
    let child = cmd.spawn()?;
    navigator_rclone::kill_child_with_process(&child);
    let out = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_top_level_count(&stdout).0)
}

/// Spawn `cmd` and wait for it, first tying the child to the process-wide
/// kill-on-close job.
///
/// `Command::status()` would be shorter, but it hands back no `Child` to
/// register, and an unregistered 7z outlives us: Windows does not kill
/// children with their parent, so closing navigator mid-extract left an
/// invisible `7z.exe` unpacking into a folder nobody was watching — and
/// then *not* deleting the archive, because the worker that owns that
/// half of the job had died with the window. Same job the rclone driver
/// uses, so the close warning tells the truth for both.
fn run_child(cmd: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    let mut child = cmd.spawn()?;
    navigator_rclone::kill_child_with_process(&child);
    child.wait()
}

/// Suppress the transient console window 7z would otherwise pop. Same
/// `CREATE_NO_WINDOW` flag the rclone driver uses for its child
/// processes; without it the 7z window steals focus from the listview
/// for every archive.
#[cfg(windows)]
fn no_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractable_extensions_are_lowercase_and_dotless() {
        for ext in EXTRACTABLE_EXTENSIONS {
            assert!(!ext.is_empty(), "empty extension entry");
            assert!(!ext.starts_with('.'), "{ext}: lead dot");
            assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext}: not lowercase");
        }
    }

    #[test]
    fn is_extractable_handles_case_and_unknowns() {
        assert!(is_extractable(Path::new("a.zip")));
        assert!(is_extractable(Path::new("A.ZIP")));
        assert!(is_extractable(Path::new("foo/bar.tar.gz")));
        assert!(is_extractable(Path::new("disc.iso")));
        assert!(!is_extractable(Path::new("notes.txt")));
        assert!(!is_extractable(Path::new("noext")));
        assert!(!is_extractable(Path::new("README")));
    }

    #[test]
    fn archive_stem_strips_layered_extensions() {
        assert_eq!(archive_stem(Path::new("foo.tar.gz")), "foo");
        assert_eq!(archive_stem(Path::new("backup.zip")), "backup");
        assert_eq!(archive_stem(Path::new("a.b.7z")), "a.b");
        // Unknown trailing ext is left alone — caller is expected to
        // pre-filter via `is_extractable`, but we still tolerate it.
        assert_eq!(archive_stem(Path::new("notes.txt")), "notes.txt");
        // Mixed: archive ext on top, unknown ext under → strip the
        // outer one, then stop.
        assert_eq!(archive_stem(Path::new("archive.txt.zip")), "archive.txt");
    }

    #[test]
    fn parse_top_level_count_dedupes_first_segment() {
        let listing = "\
Path = root/inner/file1
Size = 1

Path = root/inner/file2
Size = 2

Path = root/other
Size = 3
";
        let (n, only) = parse_top_level_count(listing);
        assert_eq!(n, 1);
        assert_eq!(only.as_deref(), Some("root"));
    }

    #[test]
    fn parse_top_level_count_multiple_tops() {
        let listing = "\
Path = a/file
Path = b/file
Path = c
";
        let (n, only) = parse_top_level_count(listing);
        assert_eq!(n, 3);
        assert!(only.is_none());
    }

    #[test]
    fn parse_top_level_count_handles_backslashes() {
        let listing = "Path = root\\sub\\thing\nPath = root\\other\n";
        let (n, _) = parse_top_level_count(listing);
        assert_eq!(n, 1);
    }

    #[test]
    fn parse_top_level_count_ignores_non_path_lines() {
        let listing = "\
----------
Type = zip
Solid = -

Path = only/here
Size = 0
";
        let (n, only) = parse_top_level_count(listing);
        assert_eq!(n, 1);
        assert_eq!(only.as_deref(), Some("only"));
    }

    #[test]
    fn decide_dest_skips_wrap_when_already_wrapped() {
        let archive = Path::new("/parent/foo.zip");
        let parent = Path::new("/parent");
        // Single top-level → use parent regardless of the toggle.
        assert_eq!(decide_dest(archive, parent, 1, true), parent);
        assert_eq!(decide_dest(archive, parent, 0, true), parent);
    }

    #[test]
    fn decide_dest_wraps_when_loose() {
        let archive = Path::new("/parent/foo.zip");
        let parent = Path::new("/parent");
        let dest = decide_dest(archive, parent, 5, true);
        assert_eq!(dest, parent.join("foo"));
    }

    #[test]
    fn decide_dest_off_never_wraps() {
        let archive = Path::new("/parent/foo.zip");
        let parent = Path::new("/parent");
        assert_eq!(decide_dest(archive, parent, 5, false), parent);
        assert_eq!(decide_dest(archive, parent, 1, false), parent);
    }

    #[test]
    fn zip_dest_names_sibling_zip() {
        assert_eq!(
            zip_dest(Path::new("/p/report.txt")),
            Path::new("/p/report.zip")
        );
        assert_eq!(zip_dest(Path::new("/p/docs")), Path::new("/p/docs.zip"));
        // Layered extension: only the outer one is dropped (file_stem).
        assert_eq!(
            zip_dest(Path::new("/p/a.tar.gz")),
            Path::new("/p/a.tar.zip")
        );
    }

    fn inputs_as_strings(plan: &ZipPlan) -> Vec<String> {
        plan.inputs
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn plan_zip_single_folder_zips_contents() {
        let folder = Path::new("/p/docs");
        let items = [folder.to_path_buf()];
        let plan = plan_zip(folder, &items, true);
        // cwd is the folder itself and we add `*`, so contents land at root.
        assert_eq!(plan.cwd, folder);
        assert_eq!(plan.dest, Path::new("/p/docs.zip"));
        assert_eq!(inputs_as_strings(&plan), vec!["*".to_string()]);
    }

    #[test]
    fn plan_zip_folder_plus_file_wraps_each() {
        let primary = Path::new("/p/docs");
        let items = [
            Path::new("/p/docs").to_path_buf(),
            Path::new("/p/notes.txt").to_path_buf(),
        ];
        // Not a single folder → combined archive named after the primary,
        // each item added by basename from the shared parent.
        let plan = plan_zip(primary, &items, false);
        assert_eq!(plan.cwd, Path::new("/p"));
        assert_eq!(plan.dest, Path::new("/p/docs.zip"));
        assert_eq!(inputs_as_strings(&plan), vec!["docs", "notes.txt"]);
    }

    #[test]
    fn plan_zip_two_folders_wraps_each() {
        let primary = Path::new("/p/docs");
        let items = [
            Path::new("/p/docs").to_path_buf(),
            Path::new("/p/pics").to_path_buf(),
        ];
        let plan = plan_zip(primary, &items, false);
        assert_eq!(plan.cwd, Path::new("/p"));
        assert_eq!(plan.dest, Path::new("/p/docs.zip"));
        assert_eq!(inputs_as_strings(&plan), vec!["docs", "pics"]);
    }

    #[test]
    fn plan_zip_single_file_zips_the_file_at_root() {
        let file = Path::new("/p/report.txt");
        let items = [file.to_path_buf()];
        // A lone file is the combined branch (single_folder == false), so it
        // is added by basename → archive holds the file at its root.
        let plan = plan_zip(file, &items, false);
        assert_eq!(plan.cwd, Path::new("/p"));
        assert_eq!(plan.dest, Path::new("/p/report.zip"));
        assert_eq!(inputs_as_strings(&plan), vec!["report.txt"]);
    }

    fn nav(p: &str) -> NavPath {
        NavPath::new(p).unwrap()
    }

    #[test]
    fn sweep_set_is_a_strict_subset_of_the_extractable_set() {
        for ext in SWEEP_EXTENSIONS {
            assert!(
                EXTRACTABLE_EXTENSIONS.contains(ext),
                "{ext} is sweepable but not extractable",
            );
        }
        // The width of EXTRACTABLE_EXTENSIONS is only safe for a file the
        // user pointed at. A recursive sweep that took these would
        // "extract" every binary in a tree and, with the default
        // delete-after-extract, delete them.
        for ext in [
            "exe", "msi", "jar", "apk", "iso", "dmg", "vhd", "chm", "epub",
        ] {
            assert!(
                !SWEEP_EXTENSIONS.contains(&ext),
                "{ext} must never be swept recursively",
            );
        }
    }

    #[test]
    fn rar_part_parses_modern_volume_names() {
        assert_eq!(rar_part("movie.part1.rar"), Some(("movie", 1)));
        assert_eq!(rar_part("movie.part03.rar"), Some(("movie", 3)));
        assert_eq!(rar_part("Movie.PART2.RAR"), Some(("Movie", 2)));
        assert_eq!(rar_part("a.b.part7.rar"), Some(("a.b", 7)));
        assert_eq!(rar_part("movie.rar"), None);
        assert_eq!(rar_part("movie.partx.rar"), None);
        assert_eq!(rar_part("movie.part1.zip"), None);
    }

    #[test]
    fn the_sweep_takes_only_the_first_volume_of_a_set() {
        // 7-Zip pulls the whole set in from part 1; queueing the others
        // is one guaranteed failure per part.
        assert!(!is_continuation_volume("movie.part1.rar"));
        assert!(is_continuation_volume("movie.part2.rar"));
        assert!(is_continuation_volume("movie.part10.rar"));
        assert!(!is_continuation_volume("movie.rar"));
        assert!(!is_continuation_volume("notes.zip"));
    }

    #[test]
    fn volume_set_purges_every_modern_part() {
        let siblings = [
            "movie.part1.rar",
            "movie.part2.rar",
            "movie.part3.rar",
            "other.part1.rar",
            "readme.txt",
        ];
        let mut got = volume_set("movie.part1.rar", siblings);
        got.sort();
        assert_eq!(
            got,
            vec!["movie.part1.rar", "movie.part2.rar", "movie.part3.rar"],
        );
    }

    #[test]
    fn split_volume_parses_numbered_parts() {
        // The extension is `001` — in no extension table anywhere, which
        // is why this is asked before the table is consulted.
        assert_eq!(split_volume("movie.7z.001"), Some(("movie.7z", 1)));
        assert_eq!(
            split_volume("Sketchbook.7Z.012"),
            Some(("Sketchbook.7Z", 12))
        );
        assert_eq!(split_volume("data.1000"), Some(("data", 1000)));
        assert_eq!(
            split_volume("archive.tar.gz.003"),
            Some(("archive.tar.gz", 3))
        );
        // Fewer than three digits is an ordinary name, not a volume.
        assert_eq!(split_volume("report.2"), None);
        assert_eq!(split_volume("lib.so.6"), None);
        assert_eq!(split_volume("movie.7z"), None);
        assert_eq!(split_volume("notes.00a"), None);
        assert_eq!(split_volume(".001"), None);
    }

    #[test]
    fn a_split_archive_is_extractable_and_sweepable_by_what_it_joins_into() {
        // The reported bug: `7z x` opens this happily, we refused it.
        assert!(is_extractable(Path::new(
            r"F:\audiogamesounds\Sketchbook Your World.7z.001"
        )));
        assert!(is_extractable(Path::new("movie.rar.001")));
        // A plain split of any file joins back fine when pointed at...
        assert!(is_extractable(Path::new("movie.mkv.001")));
        // ...but the sweep only takes sets that rebuild an archive, or a
        // walk of a downloads tree would join and then purge media.
        assert!(is_sweepable(Path::new("movie.7z.001")));
        assert!(!is_sweepable(Path::new("movie.mkv.001")));
        assert!(!is_sweepable(Path::new("setup.exe.001")));
    }

    #[test]
    fn the_sweep_takes_only_the_first_split_volume() {
        assert!(!is_continuation_volume("movie.7z.001"));
        assert!(is_continuation_volume("movie.7z.002"));
        assert!(is_continuation_volume("movie.7z.010"));
        assert!(!is_continuation_volume("movie.7z"));
    }

    #[test]
    fn volume_set_purges_every_numbered_part() {
        // Purging only `.001` leaves parts nothing can open — the same
        // as not purging at all.
        let siblings = [
            "movie.7z.001",
            "movie.7z.002",
            "movie.7z.003",
            "other.7z.001",
            "movie.7z.txt",
        ];
        let mut got = volume_set("movie.7z.001", siblings);
        got.sort();
        assert_eq!(got, vec!["movie.7z.001", "movie.7z.002", "movie.7z.003"],);
    }

    #[test]
    fn archive_stem_drops_a_split_suffix() {
        assert_eq!(archive_stem(Path::new("movie.7z.001")), "movie");
        assert_eq!(archive_stem(Path::new("archive.tar.gz.002")), "archive");
        assert_eq!(archive_stem(Path::new("data.001")), "data");
    }

    #[test]
    fn a_selected_volume_set_extracts_once_not_once_per_part() {
        // Ctrl+A over a folder holding one split archive selects every
        // part; only the first can be opened.
        let items: Vec<(NavPath, bool)> = vec![
            (nav(r"C:\p\movie.7z.001"), false),
            (nav(r"C:\p\movie.7z.002"), false),
            (nav(r"C:\p\movie.7z.003"), false),
            (nav(r"C:\p\show.part1.rar"), false),
            (nav(r"C:\p\show.part2.rar"), false),
            (nav(r"C:\p\loose.zip"), false),
        ];
        let (direct, _) = split_extract_selection(&items);
        assert_eq!(
            direct,
            vec![
                nav(r"C:\p\movie.7z.001"),
                nav(r"C:\p\show.part1.rar"),
                nav(r"C:\p\loose.zip"),
            ],
        );
    }

    #[test]
    fn a_continuation_selected_alone_is_still_attempted() {
        // Dropping it would report "no extractable archives selected"
        // about a file the user is looking straight at.
        let items = vec![(nav(r"C:\p\movie.7z.002"), false)];
        let (direct, _) = split_extract_selection(&items);
        assert_eq!(direct, vec![nav(r"C:\p\movie.7z.002")]);
    }

    #[test]
    fn volume_set_purges_old_style_continuations() {
        let siblings = ["movie.rar", "movie.r00", "movie.r01", "unrelated.r00"];
        let mut got = volume_set("movie.rar", siblings);
        got.sort();
        assert_eq!(got, vec!["movie.r00", "movie.r01", "movie.rar"]);
    }

    #[test]
    fn volume_set_of_an_ordinary_archive_is_just_itself() {
        let siblings = ["a.zip", "b.zip", "a.r00"];
        assert_eq!(volume_set("a.zip", siblings), vec!["a.zip"]);
    }

    #[test]
    fn archive_stem_drops_a_volume_suffix() {
        // Otherwise the wrapper folder is named after one volume of a set
        // that produced all of them.
        assert_eq!(archive_stem(Path::new("movie.part1.rar")), "movie");
        assert_eq!(archive_stem(Path::new("movie.part12.rar")), "movie");
        assert_eq!(archive_stem(Path::new("movie.rar")), "movie");
        // `part` without digits is a real name, not a volume marker.
        assert_eq!(archive_stem(Path::new("the.part.zip")), "the.part");
    }

    #[test]
    fn split_extract_selection_routes_folders_to_the_sweep() {
        let items = vec![
            (nav(r"C:\p\a.zip"), false),
            (nav(r"C:\p\notes.txt"), false),
            (nav(r"C:\p\downloads"), true),
            // A directly picked `.exe` is honoured; the sweep would not
            // have taken it.
            (nav(r"C:\p\setup.exe"), false),
        ];
        let (direct, roots) = split_extract_selection(&items);
        assert_eq!(direct, vec![nav(r"C:\p\a.zip"), nav(r"C:\p\setup.exe")]);
        assert_eq!(roots, vec![nav(r"C:\p\downloads")]);
    }

    #[test]
    fn merge_targets_dedupes_overlapping_roots() {
        // A folder and an archive inside it can both be selected; running
        // the same archive twice would re-extract a file the first pass
        // had already deleted.
        let direct = vec![nav(r"C:\p\dl\a.zip")];
        let swept = vec![nav(r"C:\p\dl\a.zip"), nav(r"C:\p\dl\sub\b.7z")];
        assert_eq!(
            merge_targets(direct, swept),
            vec![nav(r"C:\p\dl\a.zip"), nav(r"C:\p\dl\sub\b.7z")],
        );
    }

    #[test]
    fn sweep_confirm_body_names_the_scope_and_the_purge() {
        let roots = [nav(r"C:\p\dl")];
        let targets = [nav(r"C:\p\dl\a.zip"), nav(r"C:\p\dl\sub\b.7z")];
        let body = sweep_confirm_body(&targets, &roots, true);
        assert!(
            body.starts_with("Extract 2 archives found in dl?"),
            "{body}"
        );
        assert!(body.contains(r"dl\sub\b.7z"), "{body}");
        assert!(body.contains("cannot be undone"), "{body}");
        // No purge configured -> no irreversibility warning.
        let body = sweep_confirm_body(&targets, &roots, false);
        assert!(!body.contains("cannot be undone"), "{body}");
    }

    #[test]
    fn sweep_confirm_body_truncates_a_long_list() {
        let roots = [nav(r"C:\p\dl")];
        let targets: Vec<NavPath> = (0..30)
            .map(|i| nav(&format!(r"C:\p\dl\a{i}.zip")))
            .collect();
        let body = sweep_confirm_body(&targets, &roots, false);
        assert!(
            body.starts_with("Extract 30 archives found in dl?"),
            "{body}"
        );
        assert!(
            body.contains(&format!("and {} more", 30 - SWEEP_PREVIEW_LIMIT)),
            "{body}",
        );
    }

    #[test]
    fn sweep_archives_walks_subfolders_and_skips_non_archives() {
        // Guarded, so a failing assertion below cannot strand the tree.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let deep = root.join("sub").join("deeper");
        std::fs::create_dir_all(&deep).unwrap();
        for (dir, name) in [
            (root, "top.zip"),
            (root, "notes.txt"),
            // Directly selectable, never swept.
            (root, "setup.exe"),
            (deep.as_path(), "nested.7z"),
            (deep.as_path(), "movie.part1.rar"),
            (deep.as_path(), "movie.part2.rar"),
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let found = sweep_archives(root);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"top.zip".to_string()), "{names:?}");
        assert!(names.contains(&"nested.7z".to_string()), "{names:?}");
        assert!(names.contains(&"movie.part1.rar".to_string()), "{names:?}");
        assert!(!names.contains(&"movie.part2.rar".to_string()), "{names:?}");
        assert!(!names.contains(&"notes.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"setup.exe".to_string()), "{names:?}");
    }

    #[test]
    fn purge_targets_names_the_whole_volume_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for name in ["m.part1.rar", "m.part2.rar", "m.part3.rar", "keep.zip"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let mut got: Vec<String> = purge_targets(&root.join("m.part1.rar"))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["m.part1.rar", "m.part2.rar", "m.part3.rar"]);
    }

    #[test]
    fn unique_dest_passes_through_when_free() {
        let tmp = tempfile::tempdir().unwrap();
        let free = tmp.path().join("archive");
        let result = unique_dest(free.clone());
        assert_eq!(result, free);
    }

    #[test]
    fn unique_dest_appends_counter_on_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("archive");
        std::fs::create_dir_all(&base).unwrap();
        let result = unique_dest(base.clone());
        assert_ne!(result, base);
        assert!(
            result
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("(1)"),
            "expected suffix ` (1)`, got {:?}",
            result,
        );
    }
}
