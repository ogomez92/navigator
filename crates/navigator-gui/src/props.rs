//! Pure computation behind Alt+Enter (properties) and Alt+L (tree dump).
//!
//! Kept separate from the ops / viewer layer so the expensive walk and
//! the output formatting are both unit-testable without a live HWND or
//! speech sink. The ops module just calls into here on a worker thread
//! and hands the resulting string to `viewer::show`.

use std::collections::BTreeMap;
use std::path::Path;

use navigator_core::{Entry, EntryKind, NavPath};
use navigator_fs::{DriveInfo, read_dir};
use navigator_rclone::{RemoteSize, RemoteStat, RemoteTreeItem};

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
            Err(_) => {
                stats.errors += 1;
                continue;
            }
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
        EntryKind::File => "File",
        EntryKind::Directory => "Directory",
        EntryKind::Symlink => "Symlink / reparse point",
        EntryKind::Other => "Other",
    };
    s.push_str(&format!("Name:      {}\n", entry.name));
    s.push_str(&format!("Path:      {}\n", path));
    s.push_str(&format!("Type:      {}\n", kind));
    if matches!(
        entry.kind,
        EntryKind::File | EntryKind::Other | EntryKind::Symlink
    ) {
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
    s.push_str(&format!(
        "Size:      {}\n",
        format_size_with_bytes(header_size)
    ));
    s.push_str(&format!(
        "Modified:  {}\n",
        fmt_time_or_dash(entry.modified.0),
    ));
    s.push_str(&format!(
        "Created:   {}\n",
        fmt_time_or_dash(entry.created.0),
    ));
    s.push_str(&format!(
        "Attrs:     {}\n",
        format_attrs(entry.attrs, entry.hidden, entry.system)
    ));

    if let Some(st) = stats {
        s.push('\n');
        s.push_str("--- Folder contents (recursive) ---\n");
        s.push_str(&format!("Files:     {}\n", st.file_count));
        s.push_str(&format!("Folders:   {}\n", st.dir_count));
        s.push_str(&format!(
            "Total:     {}\n",
            format_size_with_bytes(st.total_size)
        ));
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

/// One non-recursive listing of a directory: how many entries sit directly
/// inside it, and how many bytes the loose files there account for.
///
/// This is what a drive gets instead of [`compute_folder_stats`]. Walking a
/// whole volume to answer Alt+Enter is unbounded work for a number the OS
/// already knows better than we do (the recursive tally would only count
/// what the user has permission to read, so it would disagree with the
/// capacity figures on every system drive).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TopLevel {
    pub files: u64,
    pub dirs: u64,
    /// Bytes of the files sitting directly in the folder — explicitly not
    /// a recursive total, and labelled as such in the output.
    pub file_bytes: u64,
    /// How many of the above are hidden or system entries, so the count
    /// can be reconciled with what the listing actually shows.
    pub hidden: u64,
}

/// Tally the immediate children of `dir`. `None` when the directory can't
/// be read at all (an empty CD bay, a disconnected share).
pub fn top_level_counts(dir: &NavPath) -> Option<TopLevel> {
    let entries = read_dir(dir).ok()?;
    let mut t = TopLevel::default();
    for e in entries {
        if e.hidden || e.system {
            t.hidden += 1;
        }
        match e.kind {
            EntryKind::Directory => t.dirs += 1,
            _ => {
                t.files += 1;
                t.file_bytes = t.file_bytes.saturating_add(e.size);
            }
        }
    }
    Some(t)
}

/// Build the properties text for a whole volume. `display` is the This PC
/// row the user pressed Alt+Enter on (`"D: (Data)"`), kept verbatim so the
/// screen names the same thing the listing did.
///
/// Everything here is a constant-time query — no tree walk — which is why
/// the folder section reports the root's *immediate* children only.
pub fn format_drive_properties(display: &str, info: &DriveInfo, top: Option<&TopLevel>) -> String {
    let mut s = String::new();
    s.push_str(&format!("Name:      {}\n", display));
    s.push_str(&format!("Path:      {}\n", info.root));
    s.push_str(&format!("Type:      {}\n", info.kind));
    if info.volume_info_ok {
        s.push_str(&format!(
            "Label:     {}\n",
            if info.label.is_empty() {
                "(none)"
            } else {
                &info.label
            }
        ));
        s.push_str(&format!("File system: {}\n", info.file_system));
    }

    // An empty bay or an offline share answers nothing. Say so — a screen
    // of zeroes reads as "this drive is empty", which is a different fact.
    if info.is_unavailable() {
        s.push('\n');
        s.push_str("Drive not ready — no media, or the volume is offline.\n");
        return s;
    }

    if let Some(sp) = info.space {
        let pct = sp.used_percent();
        s.push('\n');
        s.push_str("--- Capacity ---\n");
        s.push_str(&format!(
            "Total:     {}\n",
            format_size_with_bytes(sp.total)
        ));
        s.push_str(&format!(
            "Used:      {}  ({:.1}%)\n",
            format_size_with_bytes(sp.used()),
            pct
        ));
        s.push_str(&format!(
            "Free:      {}  ({:.1}%)\n",
            format_size_with_bytes(sp.free),
            100.0 - pct
        ));
        // Only worth a line when a quota actually bites; otherwise it is
        // the same number twice.
        if sp.available != sp.free {
            s.push_str(&format!(
                "Free to you: {}  (disk quota in effect)\n",
                format_size_with_bytes(sp.available)
            ));
        }
        if let Some(c) = info.cluster {
            s.push_str(&format!(
                "Cluster:   {} bytes ({} bytes/sector × {} sectors)\n",
                c.allocation_unit(),
                c.bytes_per_sector,
                c.sectors_per_cluster
            ));
        }
    }

    s.push('\n');
    s.push_str("--- Root folder (top level only) ---\n");
    match top {
        Some(t) => {
            s.push_str(&format!("Folders:   {}\n", t.dirs));
            s.push_str(&format!("Files:     {}\n", t.files));
            s.push_str(&format!(
                "Loose files: {}  (not recursive)\n",
                format_size_with_bytes(t.file_bytes)
            ));
            if t.hidden > 0 {
                s.push_str(&format!("Hidden / system entries: {}\n", t.hidden));
            }
        }
        None => s.push_str("(root folder could not be read)\n"),
    }

    // Identity and filesystem trivia come last: the viewer is read line by
    // line, and capacity is what the user opened this screen for. Features
    // are one joined line rather than sixteen for the same reason — that is
    // sixteen rows to arrow past for a detail nobody came here to find.
    if info.volume_info_ok {
        s.push('\n');
        s.push_str("--- Volume ---\n");
        s.push_str(&format!(
            "Serial:    {}\n",
            format_volume_serial(info.serial)
        ));
        if info.max_component_len > 0 {
            s.push_str(&format!("Max name:  {} chars\n", info.max_component_len));
        }
        if !info.volume_guid.is_empty() {
            s.push_str(&format!("Volume ID: {}\n", info.volume_guid));
        }
        s.push_str(&format!("Flags:     0x{:08X}\n", info.flags));
        let features = format_volume_flags(info.flags);
        if !features.is_empty() {
            s.push_str(&format!("Features:  {}\n", features.join(", ")));
        }
    }
    s
}

/// Windows renders a volume serial as two hex groups, `A1B2-C3D4`, and
/// that is the form users see in `dir` / disk tools — matching it means a
/// serial read out here can be compared with one read out anywhere else.
pub fn format_volume_serial(serial: u32) -> String {
    format!("{:04X}-{:04X}", (serial >> 16) & 0xFFFF, serial & 0xFFFF)
}

/// Decode the `FILE_*` filesystem capability flags `GetVolumeInformationW`
/// returns. Only the bits a user can act on are named; the raw value is
/// printed alongside for anything not covered.
pub fn format_volume_flags(flags: u32) -> Vec<&'static str> {
    const NAMED: &[(u32, &str)] = &[
        (0x0000_0001, "case-sensitive search"),
        (0x0000_0002, "case-preserved names"),
        (0x0000_0004, "unicode filenames"),
        (0x0000_0008, "persistent ACLs"),
        (0x0000_0010, "per-file compression"),
        (0x0000_0020, "disk quotas"),
        (0x0000_0040, "sparse files"),
        (0x0000_0080, "reparse points"),
        (0x0000_0100, "remote storage"),
        (0x0000_8000, "volume is compressed"),
        (0x0001_0000, "object IDs"),
        (0x0002_0000, "encryption (EFS)"),
        (0x0004_0000, "named streams"),
        (0x0008_0000, "READ-ONLY volume"),
        (0x0010_0000, "write-once (sequential)"),
        (0x0020_0000, "transactions"),
        (0x0040_0000, "hard links"),
        (0x0080_0000, "extended attributes"),
        (0x0100_0000, "open by file ID"),
        (0x0200_0000, "USN journal"),
        (0x0400_0000, "integrity streams"),
        (0x0800_0000, "block cloning"),
        (0x2000_0000, "DAX (direct access) volume"),
        (0x4000_0000, "cloud file ghosting"),
    ];
    NAMED
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect()
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
    let kind = if entry.is_dir() {
        "Directory (remote)"
    } else {
        "File (remote)"
    };
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
    s.push_str(&format!(
        "Size:      {}\n",
        format_size_with_bytes(header_size)
    ));
    let mod_str = stat
        .and_then(|st| st.mod_time.as_deref())
        .map(|s| s.to_string())
        .unwrap_or_else(|| fmt_time_or_dash(entry.modified.0));
    s.push_str(&format!("Modified:  {}\n", mod_str));
    if let Some(st) = stat
        && let Some(mime) = st.mime_type.as_deref().filter(|m| !m.is_empty())
    {
        s.push_str(&format!("MIME:      {}\n", mime));
    }

    if let Some(st) = stat {
        let mode = st.unix_mode();
        if mode.is_some() || !st.metadata.is_empty() {
            s.push('\n');
            s.push_str("--- Unix metadata ---\n");
            if let Some(m) = mode {
                s.push_str(&format!(
                    "Mode:      {} (0o{:o})\n",
                    format_unix_mode(m),
                    m & 0o7777
                ));
            }
            for key in [
                "uid",
                "gid",
                "mtime",
                "atime",
                "btime",
                "link-target",
                "owner",
                "group",
            ] {
                if let Some(v) = st.metadata.get(key) {
                    s.push_str(&format!("{:<10} {}\n", format!("{}:", key), v));
                }
            }
            // Dump remaining metadata keys we didn't render above so nothing
            // useful is hidden — sorted for stable output.
            let known: &[&str] = &[
                "mode",
                "uid",
                "gid",
                "mtime",
                "atime",
                "btime",
                "link-target",
                "owner",
                "group",
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
        s.push_str(&format!(
            "Total:     {}\n",
            format_size_with_bytes(sz.bytes.max(0) as u64)
        ));
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
            (true, _, true) => exec_letter,
            (true, _, false) => exec_letter.to_ascii_uppercase(),
            (_, true, true) => 't',
            (_, true, false) => 'T',
            (_, _, true) => 'x',
            (_, _, false) => '-',
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

/// Recursive enumeration of a **local** tree → TOML. Dirs and files come
/// out as two separate sorted arrays of relative paths with forward
/// slashes, plus a header block with totals. Parsable by any TOML library;
/// friendly to diff.
///
/// Remote paths must not come here: this walks with
/// `navigator_fs::read_dir`, i.e. `FindFirstFileExW` on the synthetic
/// `\\?\NavigatorRemote\…` string, which fails at the root and yields a
/// tree of zeroes. `AppState::op_dump_tree` routes them to
/// [`dump_tree_toml_remote`] instead.
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
            Err(_) => {
                errors += 1;
                continue;
            }
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
    render_tree_toml(root, dirs, files, total_size, errors, None)
}

/// Same output as [`dump_tree_toml`], built from one
/// `rclone lsjson --recursive` walk instead of a filesystem enumeration.
/// `items` already carry root-relative forward-slashed paths, so there is
/// no path arithmetic to redo here — which is also why this half is pure
/// and unit-testable without an rclone binary.
pub fn dump_tree_toml_remote(root: &NavPath, items: &[RemoteTreeItem]) -> String {
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut total_size: u64 = 0;
    for i in items {
        if i.is_dir {
            dirs.push(i.path.clone());
        } else {
            files.push(i.path.clone());
            total_size = total_size.saturating_add(i.size);
        }
    }
    render_tree_toml(root, dirs, files, total_size, 0, None)
}

/// Render a *failed* walk. An rclone error must never be formatted as an
/// empty tree — zero counts with no explanation is exactly the symptom
/// that made a remote dump look like an empty folder.
pub fn dump_tree_toml_error(root: &NavPath, err: &str) -> String {
    render_tree_toml(root, Vec::new(), Vec::new(), 0, 1, Some(err))
}

/// Shared formatter for both walks so their output can't drift apart.
/// Sorting happens here — every caller wants the same stable order.
fn render_tree_toml(
    root: &NavPath,
    mut dirs: Vec<String>,
    mut files: Vec<String>,
    total_size: u64,
    errors: u64,
    error_msg: Option<&str>,
) -> String {
    dirs.sort();
    files.sort();

    // Remote roots display in rclone form (`mac:Downloads`) rather than as
    // the internal `\\?\NavigatorRemote\…` sentinel — same rule the title
    // bar and address bar follow.
    let label = root.rclone_arg().unwrap_or_else(|| root.to_string());

    let mut s = String::new();
    s.push_str(&format!("root = {}\n", toml_string(&label)));
    s.push_str(&format!("dir_count = {}\n", dirs.len()));
    s.push_str(&format!("file_count = {}\n", files.len()));
    s.push_str(&format!("total_size = {}\n", total_size));
    if errors > 0 {
        s.push_str(&format!("errors = {}\n", errors));
    }
    if let Some(msg) = error_msg {
        s.push_str(&format!("error = {}\n", toml_string(msg)));
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
            '"' => out.push_str("\\\""),
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
    if ticks == 0 {
        return "—".to_string();
    }
    crate::listview::format_filetime(ticks)
}

fn format_attrs(attrs: u32, hidden: bool, system: bool) -> String {
    // Low-cost decoder for the bits users actually care about. Keeps the
    // numeric value too so power users can still cross-check.
    let mut flags: Vec<&str> = Vec::new();
    if attrs & 0x0001 != 0 {
        flags.push("readonly");
    }
    if hidden {
        flags.push("hidden");
    }
    if system {
        flags.push("system");
    }
    if attrs & 0x0010 != 0 {
        flags.push("directory");
    }
    if attrs & 0x0020 != 0 {
        flags.push("archive");
    }
    if attrs & 0x0400 != 0 {
        flags.push("reparse");
    }
    if attrs & 0x0800 != 0 {
        flags.push("compressed");
    }
    if attrs & 0x4000 != 0 {
        flags.push("encrypted");
    }
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
        fn path(&self) -> &Path {
            &self.0
        }
        fn nav(&self) -> NavPath {
            NavPath::new(self.0.clone()).unwrap()
        }
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
        assert_eq!(ext_of("README"), "");
        assert_eq!(ext_of(".gitignore"), "");
        assert_eq!(ext_of("archive.tar.gz"), "gz");
    }

    #[test]
    fn toml_string_escapes_metacharacters() {
        assert_eq!(toml_string("foo"), "\"foo\"");
        assert_eq!(toml_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string("line\nbreak"), "\"line\\nbreak\"");
    }

    #[test]
    fn folder_stats_totals_files_dirs_and_sizes() {
        let td = TempDir::new();
        write(&td.path().join("a.txt"), b"hello"); // 5 bytes
        write(&td.path().join("b.rs"), b"fn main(){}"); // 11 bytes
        write(&td.path().join("sub/c.txt"), b"world!"); // 6 bytes
        write(&td.path().join("sub/nested/d.md"), b"# hi"); // 4 bytes
        fs::create_dir_all(td.path().join("empty_dir")).unwrap();

        let stats = compute_folder_stats(&td.nav());
        assert_eq!(stats.file_count, 4);
        // sub, sub/nested, empty_dir
        assert_eq!(stats.dir_count, 3);
        assert_eq!(stats.total_size, 5 + 11 + 6 + 4);
        assert_eq!(stats.errors, 0);

        // Two .txt, one .rs, one .md — sorted by count desc, ties by ext asc.
        let counts: std::collections::HashMap<&str, u64> = stats
            .by_ext
            .iter()
            .map(|e| (e.ext.as_str(), e.count))
            .collect();
        assert_eq!(counts.get("txt"), Some(&2));
        assert_eq!(counts.get("rs"), Some(&1));
        assert_eq!(counts.get("md"), Some(&1));
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
        assert!(
            out.contains("file_count = 2"),
            "missing file_count in:\n{out}"
        );
        assert!(
            out.contains("dir_count = 2"),
            "missing dir_count in:\n{out}"
        );
        assert!(
            out.contains("total_size = 3"),
            "missing total_size in:\n{out}"
        );
        // Arrays — relative paths, forward slashes, sorted.
        assert!(out.contains("\"a.txt\""), "missing a.txt in:\n{out}");
        assert!(
            out.contains("\"sub/b.txt\""),
            "missing sub/b.txt in:\n{out}"
        );
        assert!(out.contains("\"sub\""), "missing sub dir in:\n{out}");
        assert!(
            out.contains("\"emptydir\""),
            "missing emptydir dir in:\n{out}"
        );
        // Relative paths must use forward slashes — only the `root` line
        // is allowed to carry an escaped Windows path. Check the array
        // lines rather than the whole blob.
        for line in out.lines().filter(|l| l.trim_start().starts_with('"')) {
            assert!(
                !line.contains("\\\\"),
                "relative path has backslashes: {line}"
            );
        }
    }

    fn tree_item(path: &str, is_dir: bool, size: u64) -> RemoteTreeItem {
        RemoteTreeItem {
            path: path.into(),
            is_dir,
            size,
        }
    }

    /// The remote dump is fed by `lsjson --recursive`, so it must split
    /// dirs from files, total only file bytes, and label the root in rclone
    /// form — never the `\\?\NavigatorRemote\…` sentinel the user never
    /// typed.
    #[test]
    fn dump_tree_toml_remote_splits_dirs_from_files_and_labels_root() {
        let root = NavPath::remote("mac", "Downloads");
        let items = vec![
            tree_item("a.txt", false, 1),
            tree_item("sub", true, 0),
            tree_item("sub/b.txt", false, 2),
            tree_item("emptydir", true, 0),
        ];
        let out = dump_tree_toml_remote(&root, &items);
        assert!(out.contains("root = \"mac:Downloads\""), "root in:\n{out}");
        assert!(!out.contains("NavigatorRemote"), "sentinel leaked:\n{out}");
        assert!(out.contains("file_count = 2"), "file_count in:\n{out}");
        assert!(out.contains("dir_count = 2"), "dir_count in:\n{out}");
        assert!(out.contains("total_size = 3"), "total_size in:\n{out}");
        assert!(out.contains("\"sub/b.txt\""), "nested file in:\n{out}");
        assert!(out.contains("\"emptydir\""), "empty dir in:\n{out}");
        // A successful walk carries no error keys, however empty it is.
        assert!(!out.contains("errors ="), "spurious errors in:\n{out}");
    }

    /// A failed rclone walk must not render as a well-formed empty tree —
    /// that indistinguishability is the original bug. The error text has to
    /// appear in the dump itself, since the viewer is all the user sees.
    #[test]
    fn dump_tree_toml_error_is_distinguishable_from_an_empty_tree() {
        let root = NavPath::remote("mac", "Downloads");
        let empty = dump_tree_toml_remote(&root, &[]);
        let failed = dump_tree_toml_error(&root, "rclone lsjson --recursive mac:Downloads failed");
        assert!(empty.contains("file_count = 0"));
        assert!(failed.contains("file_count = 0"));
        assert_ne!(empty, failed);
        assert!(failed.contains("errors = 1"), "errors in:\n{failed}");
        assert!(
            failed.contains("error = \"rclone lsjson --recursive mac:Downloads failed\""),
            "message in:\n{failed}"
        );
    }

    #[test]
    fn format_properties_for_file_contains_size_and_type() {
        let e = Entry {
            name: "hello.txt".into(),
            kind: EntryKind::File,
            size: 42,
            modified: navigator_core::FileTime(0),
            created: navigator_core::FileTime(0),
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

    fn drive(space: Option<navigator_fs::DriveSpace>) -> DriveInfo {
        DriveInfo {
            root: r"D:\".into(),
            label: "Data".into(),
            drive_type: 3,
            kind: "Local Disk",
            file_system: "NTFS".into(),
            serial: 0xA1B2_C3D4,
            max_component_len: 255,
            flags: 0x0000_0002 | 0x0004_0000,
            volume_guid: r"\\?\Volume{deadbeef-0000-0000-0000-000000000000}\".into(),
            volume_info_ok: true,
            space,
            cluster: Some(navigator_fs::ClusterInfo {
                bytes_per_sector: 512,
                sectors_per_cluster: 8,
            }),
        }
    }

    #[test]
    fn volume_serial_uses_the_windows_two_group_form() {
        assert_eq!(format_volume_serial(0xA1B2_C3D4), "A1B2-C3D4");
        assert_eq!(format_volume_serial(0), "0000-0000");
        assert_eq!(format_volume_serial(0x0000_00FF), "0000-00FF");
    }

    #[test]
    fn volume_flags_decode_only_the_bits_that_are_set() {
        let f = format_volume_flags(0x0000_0002 | 0x0008_0000);
        assert!(f.contains(&"case-preserved names"), "{f:?}");
        assert!(f.contains(&"READ-ONLY volume"), "{f:?}");
        assert!(!f.contains(&"hard links"), "{f:?}");
        assert!(format_volume_flags(0).is_empty());
    }

    /// The headline numbers for a drive are capacity, and they must be
    /// derived from the volume query rather than a walk — used is
    /// total − free, and the percentages are consistent with them.
    #[test]
    fn drive_properties_report_capacity_and_free_space() {
        let space = navigator_fs::DriveSpace {
            total: 1000,
            free: 250,
            available: 250,
        };
        let s = format_drive_properties("D: (Data)", &drive(Some(space)), None);
        assert!(s.contains("Name:      D: (Data)"), "{s}");
        assert!(s.contains("Path:      D:\\"), "{s}");
        assert!(s.contains("Type:      Local Disk"), "{s}");
        assert!(s.contains("File system: NTFS"), "{s}");
        assert!(s.contains("Total:     1000 bytes"), "{s}");
        assert!(s.contains("Used:      750 bytes  (75.0%)"), "{s}");
        assert!(s.contains("Free:      250 bytes  (25.0%)"), "{s}");
        assert!(s.contains("Serial:    A1B2-C3D4"), "{s}");
        assert!(s.contains("Cluster:   4096 bytes"), "{s}");
        assert!(s.contains("named streams"), "{s}");
        // No quota → the "free to you" line would just repeat Free.
        assert!(!s.contains("Free to you"), "{s}");
        // Never a recursive tally — that is the whole point of this path.
        assert!(!s.contains("Folder contents (recursive)"), "{s}");
    }

    /// A quota'd volume is the only case where "free" and "free to you"
    /// differ, and hiding the difference would misreport how much the user
    /// can actually write.
    #[test]
    fn drive_properties_call_out_a_quota() {
        let space = navigator_fs::DriveSpace {
            total: 1000,
            free: 400,
            available: 100,
        };
        let s = format_drive_properties("D: (Data)", &drive(Some(space)), None);
        assert!(s.contains("Free to you: 100 bytes"), "{s}");
        assert!(s.contains("disk quota in effect"), "{s}");
    }

    /// An empty bay must say so. Rendering zeroes would be indistinguishable
    /// from a genuinely empty disk — the same trap `dump_tree_toml_error`
    /// exists to avoid.
    #[test]
    fn an_unreadable_drive_says_so_instead_of_showing_zeroes() {
        let info = DriveInfo {
            root: r"E:\".into(),
            kind: "CD Drive",
            drive_type: 5,
            ..Default::default()
        };
        let s = format_drive_properties("E: (CD Drive)", &info, None);
        assert!(s.contains("Drive not ready"), "{s}");
        assert!(!s.contains("Total:"), "no capacity section:\n{s}");
        assert!(!s.contains("0 bytes"), "no zero figures:\n{s}");
    }

    /// The root listing is explicitly one level deep, and the output has to
    /// say that so the numbers aren't read as a whole-disk tally.
    #[test]
    fn drive_properties_report_the_root_listing_as_non_recursive() {
        let space = navigator_fs::DriveSpace {
            total: 1000,
            free: 250,
            available: 250,
        };
        let top = TopLevel {
            files: 3,
            dirs: 12,
            file_bytes: 4096,
            hidden: 2,
        };
        let s = format_drive_properties("D: (Data)", &drive(Some(space)), Some(&top));
        assert!(s.contains("Root folder (top level only)"), "{s}");
        assert!(s.contains("Folders:   12"), "{s}");
        assert!(s.contains("Files:     3"), "{s}");
        assert!(s.contains("not recursive"), "{s}");
        assert!(s.contains("Hidden / system entries: 2"), "{s}");
        // Order is deliberate: the viewer is read top-down, so capacity
        // comes before the listing and volume trivia comes last.
        let capacity = s.find("--- Capacity ---").expect("capacity section");
        let root = s.find("--- Root folder").expect("root section");
        let volume = s.find("--- Volume ---").expect("volume section");
        assert!(capacity < root && root < volume, "section order:\n{s}");
    }

    #[test]
    fn top_level_counts_do_not_recurse() {
        let td = TempDir::new();
        write(&td.path().join("a.txt"), b"hello"); // 5 bytes
        write(&td.path().join("b.bin"), b"xy"); // 2 bytes
        write(&td.path().join("sub/deep.txt"), b"ignored"); // must not count
        fs::create_dir_all(td.path().join("empty")).unwrap();

        let t = top_level_counts(&td.nav()).expect("readable dir");
        assert_eq!(t.files, 2, "only the two root files: {t:?}");
        assert_eq!(t.dirs, 2, "sub + empty: {t:?}");
        assert_eq!(t.file_bytes, 7, "root files only, no descent: {t:?}");
    }

    #[test]
    fn format_properties_for_folder_shows_recursive_stats() {
        let e = Entry {
            name: "sub".into(),
            kind: EntryKind::Directory,
            size: 0,
            modified: navigator_core::FileTime(0),
            created: navigator_core::FileTime(0),
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
                ExtStat {
                    ext: "txt".into(),
                    count: 2,
                    size: 20,
                },
                ExtStat {
                    ext: "rs".into(),
                    count: 1,
                    size: 1214,
                },
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
        assert!(
            s.contains("Size:      1.2 KB (1234 bytes)"),
            "expected recursive size in header, got:\n{s}"
        );
        assert!(
            !s.contains("Size:      0 bytes"),
            "folder header must not show 0 bytes when stats present:\n{s}"
        );
    }
}
