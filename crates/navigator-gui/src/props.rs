//! Pure computation behind Alt+Enter (properties) and Alt+L (tree dump).
//!
//! Kept separate from the ops / viewer layer so the expensive walk and
//! the output formatting are both unit-testable without a live HWND or
//! speech sink. The ops module just calls into here on a worker thread
//! and hands the resulting string to `viewer::show`.

use std::collections::BTreeMap;
use std::path::Path;

use navigator_core::{Entry, EntryKind, NavPath};
use navigator_fs::read_dir;
use navigator_rclone::{RemoteSize, RemoteStat};

/// Recursive tally across every file below `root`. Unreadable sub-trees
/// are counted in `errors` and skipped; we never bail on a partial scan
/// because a single permission-denied dir shouldn't hide the rest of the
/// stats.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FolderStats {
    pub file_count: u64,
    pub dir_count: u64,
    pub total_size: u64,
    /// Per-extension breakdown (lowercased; `""` for extensionless files).
    /// Sorted by `count` descending with ties broken by extension for a
    /// stable display order.
    pub by_ext: Vec<ExtStat>,
    /// Number of sub-directories that failed to enumerate (permission
    /// denied, reparse point loop, etc.). Zero is the common case.
    pub errors: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtStat {
    pub ext: String,
    pub count: u64,
    pub size: u64,
}

/// Walk everything under `root` with an explicit stack (no recursion so
/// a deep tree can't blow the process stack). Skips reparse points so
/// we don't follow symlinks — cheap cycle guard and matches what
/// `navigator_fs::search_recursive` already does.
pub fn compute_folder_stats(root: &NavPath) -> FolderStats {
    let mut stats = FolderStats::default();
    let mut hist: BTreeMap<String, (u64, u64)> = BTreeMap::new();

    let mut stack: Vec<NavPath> = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match read_dir(&dir) {
            Ok(v) => v,
            Err(_) => { stats.errors += 1; continue; }
        };
        for e in entries {
            match e.kind {
                EntryKind::Directory => {
                    stats.dir_count += 1;
                    stack.push(dir.join(&e.name));
                }
                EntryKind::Symlink => {
                    // Count but do not recurse. Size from FIND_DATA is
                    // the reparse point size (usually 0); leave it as-is.
                    stats.file_count += 1;
                    stats.total_size = stats.total_size.saturating_add(e.size);
                    let ext = ext_of(&e.name);
                    let slot = hist.entry(ext).or_default();
                    slot.0 += 1;
                    slot.1 = slot.1.saturating_add(e.size);
                }
                EntryKind::File | EntryKind::Other => {
                    stats.file_count += 1;
                    stats.total_size = stats.total_size.saturating_add(e.size);
                    let ext = ext_of(&e.name);
                    let slot = hist.entry(ext).or_default();
                    slot.0 += 1;
                    slot.1 = slot.1.saturating_add(e.size);
                }
            }
        }
    }

    let mut by_ext: Vec<ExtStat> = hist
        .into_iter()
        .map(|(ext, (count, size))| ExtStat { ext, count, size })
        .collect();
    by_ext.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.ext.cmp(&b.ext)));
    stats.by_ext = by_ext;
    stats
}

/// Lower-case extension (without the leading `.`) or empty string for
/// files with no dot at all or a trailing-dot-only name.
pub fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Build the properties text for a single focused `entry` rooted at
/// `path`. For a folder, `stats` carries the recursive tally; for a
/// file it's `None`.
pub fn format_properties(entry: &Entry, path: &NavPath, stats: Option<&FolderStats>) -> String {
    let mut s = String::new();
    let kind = match entry.kind {
        EntryKind::File      => "File",
        EntryKind::Directory => "Directory",
        EntryKind::Symlink   => "Symlink / reparse point",
        EntryKind::Other     => "Other",
    };
    s.push_str(&format!("Name:      {}\n", entry.name));
    s.push_str(&format!("Path:      {}\n", path));
    s.push_str(&format!("Type:      {}\n", kind));
    if matches!(entry.kind, EntryKind::File | EntryKind::Other | EntryKind::Symlink) {
        let ext = ext_of(&entry.name);
        if !ext.is_empty() {
            s.push_str(&format!("Extension: .{}\n", ext));
        }
    }
    // For a directory the `WIN32_FIND_DATAW` size field is zero — use
    // the recursive total we already computed instead so the header
    // isn't lying ("Size: 0 bytes" on a folder full of files).
    let header_size = match (entry.is_dir(), stats) {
        (true, Some(st)) => st.total_size,
        _ => entry.size,
    };
    s.push_str(&format!("Size:      {}\n", format_size_with_bytes(header_size)));
    s.push_str(&format!(
        "Modified:  {}\n",
        fmt_time_or_dash(entry.modified.0),
    ));
    s.push_str(&format!(
        "Created:   {}\n",
        fmt_time_or_dash(entry.created.0),
    ));
    s.push_str(&format!("Attrs:     {}\n", format_attrs(entry.attrs, entry.hidden, entry.system)));

    if let Some(st) = stats {
        s.push('\n');
        s.push_str("--- Folder contents (recursive) ---\n");
        s.push_str(&format!("Files:     {}\n", st.file_count));
        s.push_str(&format!("Folders:   {}\n", st.dir_count));
        s.push_str(&format!("Total:     {}\n", format_size_with_bytes(st.total_size)));
        if st.errors > 0 {
            s.push_str(&format!("Unreadable subfolders: {}\n", st.errors));
        }
        if !st.by_ext.is_empty() {
            s.push_str("\nBy extension (count, size):\n");
            let width = st
                .by_ext
                .iter()
                .map(|x| if x.ext.is_empty() { 9 } else { x.ext.len() + 1 })
                .max()
                .unwrap_or(0);
            for e in &st.by_ext {
                let label = if e.ext.is_empty() {
                    "(no ext)".to_string()
                } else {
                    format!(".{}", e.ext)
                };
                s.push_str(&format!(
                    "  {label:<width$}  {count:>8}   {size}\n",
                    label = label,
                    width = width,
                    count = e.count,
                    size = format_size_with_bytes(e.size),
                ));
            }
        }
    }
    s
}

/// Build the properties text for a remote `entry` at `path`. `stat` is the
/// `rclone lsjson --stat -M` result for the path itself; `size` is
/// `rclone size --json` for directories. Either may be `None` when the
/// rclone call failed — we still emit the header from the cached `Entry`.
pub fn format_remote_properties(
    entry: &Entry,
    path: &NavPath,
    stat: Option<&RemoteStat>,
    size: Option<&RemoteSize>,
) -> String {
    let mut s = String::new();
    let kind = if entry.is_dir() { "Directory (remote)" } else { "File (remote)" };
    s.push_str(&format!("Name:      {}\n", entry.name));
    s.push_str(&format!("Path:      {}\n", path));
    s.push_str(&format!("Type:      {}\n", kind));
    if !entry.is_dir() {
        let ext = ext_of(&entry.name);
        if !ext.is_empty() {
            s.push_str(&format!("Extension: .{}\n", ext));
        }
    }
    let header_size: u64 = if entry.is_dir() {
        size.map(|sz| sz.bytes.max(0) as u64).unwrap_or(0)
    } else {
        stat.map(|st| st.size.max(0) as u64).unwrap_or(entry.size)
    };
    s.push_str(&format!("Size:      {}\n", format_size_with_bytes(header_size)));
    let mod_str = stat
        .and_then(|st| st.mod_time.as_deref())
        .map(|s| s.to_string())
        .unwrap_or_else(|| fmt_time_or_dash(entry.modified.0));
    s.push_str(&format!("Modified:  {}\n", mod_str));
    if let Some(st) = stat {
        if let Some(mime) = st.mime_type.as_deref().filter(|m| !m.is_empty()) {
            s.push_str(&format!("MIME:      {}\n", mime));
        }
    }

    if let Some(st) = stat {
        let mode = st.unix_mode();
        if mode.is_some() || !st.metadata.is_empty() {
            s.push('\n');
            s.push_str("--- Unix metadata ---\n");
            if let Some(m) = mode {
                s.push_str(&format!(
                    "Mode:      {} ({})\n",
                    format_unix_mode(m),
                    format!("0o{:o}", m & 0o7777)
                ));
            }
            for key in ["uid", "gid", "mtime", "atime", "btime", "link-target", "owner", "group"] {
                if let Some(v) = st.metadata.get(key) {
                    s.push_str(&format!("{:<10} {}\n", format!("{}:", key), v));
                }
            }
            // Dump remaining metadata keys we didn't render above so nothing
            // useful is hidden — sorted for stable output.
            let known: &[&str] = &[
                "mode", "uid", "gid", "mtime", "atime", "btime", "link-target", "owner", "group",
            ];
            let mut extras: Vec<(&String, &String)> = st
                .metadata
                .iter()
                .filter(|(k, _)| !known.contains(&k.as_str()))
                .collect();
            extras.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in extras {
                s.push_str(&format!("{:<10} {}\n", format!("{}:", k), v));
            }
        }
    }

    if let Some(sz) = size {
        s.push('\n');
        s.push_str("--- Folder contents (recursive) ---\n");
        s.push_str(&format!("Files:     {}\n", sz.count.max(0)));
        s.push_str(&format!("Total:     {}\n", format_size_with_bytes(sz.bytes.max(0) as u64)));
        if sz.sizeless > 0 {
            s.push_str(&format!("Sizeless objects: {}\n", sz.sizeless));
        }
    }

    if stat.is_none() {
        s.push('\n');
        s.push_str("(rclone stat failed — header values reflect the cached listing only)\n");
    }
    s
}

/// Render a UNIX `st_mode` value the way `ls -l` does, e.g. `-rwxr-xr-x`.
/// Honors the file-type bits if present (file/dir/symlink/etc) and the
/// suid/sgid/sticky bits.
pub fn format_unix_mode(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o060000 => 'b',
        0o020000 => 'c',
        0o010000 => 'p',
        0o140000 => 's',
        0o100000 => '-',
        _ => '?',
    };
    let perm = |bits: u32, suid: bool, sticky: bool, exec_letter: char| {
        let r = if bits & 0o4 != 0 { 'r' } else { '-' };
        let w = if bits & 0o2 != 0 { 'w' } else { '-' };
        let x = bits & 0o1 != 0;
        let last = match (suid, sticky, x) {
            (true, _, true)   => exec_letter,
            (true, _, false)  => exec_letter.to_ascii_uppercase(),
            (_, true, true)   => 't',
            (_, true, false)  => 'T',
            (_, _, true)      => 'x',
            (_, _, false)     => '-',
        };
        format!("{r}{w}{last}")
    };
    let mut out = String::with_capacity(10);
    out.push(kind);
    out.push_str(&perm((mode >> 6) & 0o7, mode & 0o4000 != 0, false, 's'));
    out.push_str(&perm((mode >> 3) & 0o7, mode & 0o2000 != 0, false, 's'));
    out.push_str(&perm(mode & 0o7, false, mode & 0o1000 != 0, 'x'));
    out
}

/// Recursive enumeration → TOML. Dirs and files come out as two separate
/// sorted arrays of relative paths with forward slashes, plus a header
/// block with totals. Parsable by any TOML library; friendly to diff.
pub fn dump_tree_toml(root: &NavPath) -> String {
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut total_size: u64 = 0;
    let mut errors: u64 = 0;

    let root_path = root.as_path().to_path_buf();
    let mut stack: Vec<NavPath> = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match read_dir(&dir) {
            Ok(v) => v,
            Err(_) => { errors += 1; continue; }
        };
        for e in entries {
            let full = dir.join(&e.name);
            let rel = relativize(&root_path, full.as_path());
            match e.kind {
                EntryKind::Directory => {
                    dirs.push(rel);
                    stack.push(full);
                }
                EntryKind::Symlink | EntryKind::File | EntryKind::Other => {
                    files.push(rel);
                    total_size = total_size.saturating_add(e.size);
                }
            }
        }
    }
    dirs.sort();
    files.sort();

    let mut s = String::new();
    s.push_str(&format!("root = {}\n", toml_string(&root.to_string())));
    s.push_str(&format!("dir_count = {}\n", dirs.len()));
    s.push_str(&format!("file_count = {}\n", files.len()));
    s.push_str(&format!("total_size = {}\n", total_size));
    if errors > 0 {
        s.push_str(&format!("errors = {}\n", errors));
    }
    s.push_str("\ndirs = [\n");
    for d in &dirs {
        s.push_str(&format!("  {},\n", toml_string(d)));
    }
    s.push_str("]\n\nfiles = [\n");
    for f in &files {
        s.push_str(&format!("  {},\n", toml_string(f)));
    }
    s.push_str("]\n");
    s
}

/// Render a path relative to `root` using forward slashes. Falls back to
/// the full path when the prefix strip fails (cross-volume junction etc.).
fn relativize(root: &Path, full: &Path) -> String {
    let rel = full.strip_prefix(root).unwrap_or(full);
    let mut s = rel.to_string_lossy().to_string();
    // Normalise to forward slashes — TOML doesn't care, but it keeps
    // backslash escaping simpler in the output.
    s = s.replace('\\', "/");
    s
}

/// TOML basic-string with `"` and `\` escaped. Good enough for filesystem
/// paths; non-printables on Windows paths are already disallowed by the OS.
pub fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"'  => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn format_size_with_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{} bytes", n)
    } else {
        format!("{} ({} bytes)", crate::listview::format_size(n), n)
    }
}

fn fmt_time_or_dash(ticks: u64) -> String {
    if ticks == 0 { return "—".to_string(); }
    crate::listview::format_filetime(ticks)
}

fn format_attrs(attrs: u32, hidden: bool, system: bool) -> String {
    // Low-cost decoder for the bits users actually care about. Keeps the
    // numeric value too so power users can still cross-check.
    let mut flags: Vec<&str> = Vec::new();
    if attrs & 0x0001 != 0 { flags.push("readonly"); }
    if hidden             { flags.push("hidden"); }
    if system             { flags.push("system"); }
    if attrs & 0x0010 != 0 { flags.push("directory"); }
    if attrs & 0x0020 != 0 { flags.push("archive"); }
    if attrs & 0x0400 != 0 { flags.push("reparse"); }
    if attrs & 0x0800 != 0 { flags.push("compressed"); }
    if attrs & 0x4000 != 0 { flags.push("encrypted"); }
    if flags.is_empty() {
        format!("0x{:04X}", attrs)
    } else {
        format!("0x{:04X} ({})", attrs, flags.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Allocate an empty directory under the OS temp dir. Unique per call
    /// so parallel test runs don't collide, and `Drop` cleans up so the
    /// temp tree doesn't leak when a test passes.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("nav-props-test-{}-{}", ts, n));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path { &self.0 }
        fn nav(&self) -> NavPath { NavPath::new(self.0.clone()).unwrap() }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(p: &Path, bytes: &[u8]) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, bytes).unwrap();
    }

    #[test]
    fn ext_of_handles_common_cases() {
        assert_eq!(ext_of("foo.TXT"), "txt");
        assert_eq!(ext_of("README"),  "");
        assert_eq!(ext_of(".gitignore"), "");
        assert_eq!(ext_of("archive.tar.gz"), "gz");
    }

    #[test]
    fn toml_string_escapes_metacharacters() {
        assert_eq!(toml_string("foo"),           "\"foo\"");
        assert_eq!(toml_string("a\\b"),          "\"a\\\\b\"");
        assert_eq!(toml_string("a\"b"),          "\"a\\\"b\"");
        assert_eq!(toml_string("line\nbreak"),   "\"line\\nbreak\"");
    }

    #[test]
    fn folder_stats_totals_files_dirs_and_sizes() {
        let td = TempDir::new();
        write(&td.path().join("a.txt"),            b"hello");         // 5 bytes
        write(&td.path().join("b.rs"),             b"fn main(){}");    // 11 bytes
        write(&td.path().join("sub/c.txt"),        b"world!");         // 6 bytes
        write(&td.path().join("sub/nested/d.md"),  b"# hi");           // 4 bytes
        fs::create_dir_all(td.path().join("empty_dir")).unwrap();

        let stats = compute_folder_stats(&td.nav());
        assert_eq!(stats.file_count, 4);
        // sub, sub/nested, empty_dir
        assert_eq!(stats.dir_count, 3);
        assert_eq!(stats.total_size, 5 + 11 + 6 + 4);
        assert_eq!(stats.errors, 0);

        // Two .txt, one .rs, one .md — sorted by count desc, ties by ext asc.
        let counts: std::collections::HashMap<&str, u64> =
            stats.by_ext.iter().map(|e| (e.ext.as_str(), e.count)).collect();
        assert_eq!(counts.get("txt"), Some(&2));
        assert_eq!(counts.get("rs"),  Some(&1));
        assert_eq!(counts.get("md"),  Some(&1));
        assert_eq!(stats.by_ext.first().map(|e| e.ext.as_str()), Some("txt"));
    }

    #[test]
    fn dump_tree_toml_lists_all_paths_with_forward_slashes() {
        let td = TempDir::new();
        write(&td.path().join("a.txt"), b"x");
        write(&td.path().join("sub/b.txt"), b"yy");
        fs::create_dir_all(td.path().join("emptydir")).unwrap();

        let out = dump_tree_toml(&td.nav());
        // Header.
        assert!(out.contains("file_count = 2"),  "missing file_count in:\n{out}");
        assert!(out.contains("dir_count = 2"),   "missing dir_count in:\n{out}");
        assert!(out.contains("total_size = 3"),  "missing total_size in:\n{out}");
        // Arrays — relative paths, forward slashes, sorted.
        assert!(out.contains("\"a.txt\""),        "missing a.txt in:\n{out}");
        assert!(out.contains("\"sub/b.txt\""),    "missing sub/b.txt in:\n{out}");
        assert!(out.contains("\"sub\""),          "missing sub dir in:\n{out}");
        assert!(out.contains("\"emptydir\""),     "missing emptydir dir in:\n{out}");
        // Relative paths must use forward slashes — only the `root` line
        // is allowed to carry an escaped Windows path. Check the array
        // lines rather than the whole blob.
        for line in out.lines().filter(|l| l.trim_start().starts_with('"')) {
            assert!(!line.contains("\\\\"), "relative path has backslashes: {line}");
        }
    }

    #[test]
    fn format_properties_for_file_contains_size_and_type() {
        let e = Entry {
            name: "hello.txt".into(),
            kind: EntryKind::File,
            size: 42,
            modified: navigator_core::FileTime(0),
            created:  navigator_core::FileTime(0),
            attrs: 0x20,
            hidden: false,
            system: false,
        };
        let p = NavPath::new(r"C:\tmp\hello.txt").unwrap();
        let s = format_properties(&e, &p, None);
        assert!(s.contains("Name:      hello.txt"));
        assert!(s.contains("Type:      File"));
        assert!(s.contains("Extension: .txt"));
        assert!(s.contains("42 bytes"));
        // No folder summary for a file.
        assert!(!s.contains("Folder contents"));
    }

    #[test]
    fn format_properties_for_folder_shows_recursive_stats() {
        let e = Entry {
            name: "sub".into(),
            kind: EntryKind::Directory,
            size: 0,
            modified: navigator_core::FileTime(0),
            created:  navigator_core::FileTime(0),
            attrs: 0x10,
            hidden: false,
            system: false,
        };
        let p = NavPath::new(r"C:\tmp\sub").unwrap();
        let stats = FolderStats {
            file_count: 3,
            dir_count: 1,
            total_size: 1234,
            errors: 0,
            by_ext: vec![
                ExtStat { ext: "txt".into(), count: 2, size: 20 },
                ExtStat { ext: "rs".into(),  count: 1, size: 1214 },
            ],
        };
        let s = format_properties(&e, &p, Some(&stats));
        assert!(s.contains("Files:     3"));
        assert!(s.contains("Folders:   1"));
        assert!(s.contains("By extension"));
        assert!(s.contains(".txt"));
        assert!(s.contains(".rs"));
        // Header Size line must report the recursive total, not the
        // zero that WIN32_FIND_DATAW reports for a directory.
        assert!(s.contains("Size:      1.2 KB (1234 bytes)"),
            "expected recursive size in header, got:\n{s}");
        assert!(!s.contains("Size:      0 bytes"),
            "folder header must not show 0 bytes when stats present:\n{s}");
    }
}
