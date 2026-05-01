//! Archive extraction via the standalone `7z.exe` on `PATH`.
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
    "7z", "zip", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "tbz",
    "xz", "txz", "lzma", "tlz", "lz", "lz4", "zst", "zstd", "tzst",
    // Microsoft / installer formats.
    "cab", "msi", "msm", "msp", "wim", "swm", "esd", "exe",
    // Legacy Unix / minor archivers.
    "arj", "lzh", "lha", "z", "taz", "rpm", "deb", "cpio", "ar",
    "xar", "pkg", "cpgz", "chm", "epub", "apk", "jar", "war", "ear",
    "xpi", "ipa", "ppmd",
    // Disk images / filesystems 7-Zip exposes as archives.
    "iso", "img", "dmg", "hfs", "ntfs", "fat", "vhd", "vhdx",
    "vmdk", "vdi", "qcow", "qcow2", "udf", "squashfs", "cramfs",
    "ext", "ext2", "ext3", "ext4", "apm", "mbr", "gpt",
];

/// True if `path` has an extension recognised by [`EXTRACTABLE_EXTENSIONS`].
/// Case-insensitive on the extension; the path itself is not touched.
pub fn is_extractable(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else { return false; };
    let lower = ext.to_ascii_lowercase();
    EXTRACTABLE_EXTENSIONS.iter().any(|e| *e == lower.as_str())
}

/// Locate `7z.exe` on `PATH`. Returns `None` if not installed.
pub fn find_7z() -> Option<PathBuf> {
    which_in_path("7z.exe").or_else(|| which_in_path("7z"))
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
        let Some(rest) = line.strip_prefix("Path = ") else { continue; };
        let first = rest.trim().split(['/', '\\']).next().unwrap_or("");
        if !first.is_empty() {
            tops.insert(first.to_string());
        }
    }
    let n = tops.len();
    let only = if n == 1 { tops.into_iter().next() } else { None };
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
        if !is_extractable(p) { break; }
        match p.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() && s != name => name = s.to_string(),
            _ => break,
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

/// Filter `paths` down to those with an extension 7z handles.
pub fn filter_extractable(paths: &[NavPath]) -> Vec<NavPath> {
    paths.iter().filter(|p| is_extractable(p.as_path())).cloned().collect()
}

/// Extract every entry in `sources` using `seven_zip`. Reports progress
/// (`extracting file X of Y: name`) via `speech` and announces a final
/// summary. On per-archive success and `opts.delete_when_extracted`,
/// removes the source file. Failures never trigger a delete.
///
/// Designed to run on a worker thread; takes everything by value so it
/// holds no references back into `AppState`.
pub fn run_extract(
    sources: Vec<NavPath>,
    opts: Extraction,
    seven_zip: PathBuf,
    speech: Sender<Utterance>,
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
        let dest = if wrapping { unique_dest(raw_dest) } else { raw_dest };

        if wrapping {
            if let Err(e) = std::fs::create_dir_all(&dest) {
                warn!("create_dir_all {:?}: {}", dest, e);
                let _ = speech.try_send(Utterance {
                    text: format!("can't create folder for {}", label),
                    interrupt: true,
                });
                failed += 1;
                continue;
            }
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
        let status = cmd.status();

        match status {
            Ok(s) if s.success() => {
                ok += 1;
                if opts.delete_when_extracted {
                    if let Err(e) = std::fs::remove_file(&archive_path) {
                        warn!("delete {:?}: {}", archive_path, e);
                        let _ = speech.try_send(Utterance {
                            text: format!("extracted {} but couldn't delete archive", label),
                            interrupt: true,
                        });
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

    let summary = if failed == 0 {
        format!("extracted {} of {}", ok, total)
    } else {
        format!("extracted {} of {}, {} failed", ok, total, failed)
    };
    let _ = speech.try_send(Utterance { text: summary, interrupt: failed > 0 });
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
    let out = cmd.output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_top_level_count(&stdout).0)
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
    fn unique_dest_passes_through_when_free() {
        let tmp = std::env::temp_dir().join(format!("nav-extract-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let result = unique_dest(tmp.clone());
        assert_eq!(result, tmp);
    }

    #[test]
    fn unique_dest_appends_counter_on_collision() {
        let base = std::env::temp_dir().join(format!("nav-extract-collide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let result = unique_dest(base.clone());
        assert_ne!(result, base);
        assert!(
            result.file_name().unwrap().to_string_lossy().ends_with("(1)"),
            "expected suffix ` (1)`, got {:?}", result,
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
