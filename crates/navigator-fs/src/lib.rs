//! Directory scanning and path helpers.
//!
//! Uses `FindFirstFileExW` / `FindNextFileW` directly — on Windows, `std::fs`
//! already wraps these, but doing it ourselves lets us:
//!   * return in one syscall per entry with size + mtime + attrs attached
//!   * pass `FindExInfoBasic` (no short 8.3 names, ~20% faster)
//!   * opt into `FIND_FIRST_EX_LARGE_FETCH` for big directories.

#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use navigator_core::{Entry, EntryKind, Error, FileTime, NavPath, Result};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetLogicalDriveStringsW,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
};

// `SetThreadErrorMode` lives under `Win32_System_Diagnostics_Debug` in
// windows-sys — a whole feature pulled in for one call, so it is declared
// by hand for the same reason the `DRIVE_*` constants below are inlined.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetThreadErrorMode(new_mode: u32, old_mode: *mut u32) -> i32;
}

/// `SEM_FAILCRITICALERRORS` — suppress the "There is no disk in drive E:"
/// modal the OS otherwise puts up when a volume query hits an empty
/// removable bay. See [`ErrorModeGuard`].
const SEM_FAILCRITICALERRORS: u32 = 0x0001;

/// Suppresses the hard-error dialog for the current thread and restores
/// the previous mode on drop.
///
/// Every volume query here (`GetVolumeInformationW`, `GetDiskFreeSpaceExW`,
/// …) against an empty card reader or CD bay pops a *system modal* asking
/// the user to insert a disk, and the calling thread blocks until it is
/// dismissed. Enumerating This PC would therefore stall the scan worker on
/// a machine that merely has an empty slot. The thread-scoped variant is
/// deliberate — `SetErrorMode` is process-wide and would change behaviour
/// for the UI thread and every plugin along with it.
struct ErrorModeGuard(u32);

impl ErrorModeGuard {
    fn new() -> Self {
        let mut old: u32 = 0;
        unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, &mut old) };
        Self(old)
    }
}

impl Drop for ErrorModeGuard {
    fn drop(&mut self) {
        unsafe { SetThreadErrorMode(self.0, std::ptr::null_mut()) };
    }
}

// `GetDriveType` return values. Hard-coded here because the named
// constants live under `System_WindowsProgramming` — a feature we'd
// otherwise pull in for five integers.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SYSTEM, FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
};

pub fn read_dir(path: &NavPath) -> Result<Vec<Entry>> {
    read_dir_impl(path.as_path()).map_err(|e| Error::io(path.as_path(), e))
}

/// Enumerate visible file shares on `host` via `NetShareEnum` (level 1,
/// so we get the `STYPE_` kind alongside the name). Admin (`C$`, `ADMIN$`,
/// `IPC$`) and printer shares are filtered out — they appear with the
/// `STYPE_SPECIAL` high bit set and are not useful to a non-admin
/// browsing experience.
///
/// Each surviving share becomes a `Directory` Entry whose `name` is the
/// share's bare label (e.g. `"media"`); the UI resolves clicks by
/// joining host + name into a real `\\host\share` NavPath.
///
/// Failures surface as an empty list rather than an error — the caller
/// (scan worker on a host-only UNC cwd) treats "no shares" identically
/// whether the host refused us or genuinely has none.
pub fn list_shares(host: &str) -> Vec<Entry> {
    // windows-sys 0.61 puts NetShareEnum under Storage::FileSystem and
    // NetApiBufferFree under NetworkManagement::NetManagement. Both live
    // in netapi32.dll at runtime; the split is purely bindings-side.
    use windows_sys::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
    use windows_sys::Win32::Storage::FileSystem::{
        NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_SPECIAL,
    };

    let mut out: Vec<Entry> = Vec::new();
    if host.is_empty() {
        return out;
    }

    // UNC hosts must be passed as wide UNC (`\\host`) to NetShareEnum per
    // MSDN examples — a bare `host` works in practice on modern Windows
    // but the `\\` prefix is the documented form.
    let server_wide: Vec<u16> = format!(r"\\{}", host)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut entries_read: u32 = 0;
    let mut total_entries: u32 = 0;
    let mut resume_handle: u32 = 0;

    let rc = unsafe {
        NetShareEnum(
            server_wide.as_ptr(),
            1,
            &mut buf as *mut *mut u8,
            0xFFFF_FFFF, // MAX_PREFERRED_LENGTH — server picks buffer size.
            &mut entries_read,
            &mut total_entries,
            &mut resume_handle,
        )
    };
    if rc != 0 || buf.is_null() {
        return out;
    }

    // `buf` points at a packed array of `SHARE_INFO_1`. Walk it by
    // pointer arithmetic; the struct holds two PWSTRs (netname + remark)
    // that point *into* the same buffer, so we copy strings out before
    // `NetApiBufferFree` reclaims the memory.
    let infos: *const SHARE_INFO_1 = buf as *const SHARE_INFO_1;
    for i in 0..(entries_read as usize) {
        let info = unsafe { &*infos.add(i) };
        let kind = info.shi1_type;
        // High bit (`STYPE_SPECIAL`) tags admin + IPC shares. Printers
        // are `STYPE_PRINTQ` (= 1). Only keep disk shares.
        if (kind & STYPE_SPECIAL) != 0 {
            continue;
        }
        if (kind & 0xFF) != STYPE_DISKTREE {
            continue;
        }

        let name = unsafe { pwstr_to_string(info.shi1_netname) };
        if name.is_empty() {
            continue;
        }
        out.push(Entry {
            name,
            kind: EntryKind::Directory,
            size: 0,
            modified: FileTime::default(),
            created: FileTime::default(),
            attrs: 0,
            hidden: false,
            system: false,
        });
    }

    unsafe {
        NetApiBufferFree(buf as *mut _);
    }
    out.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    out
}

/// Read a null-terminated UTF-16 buffer into an owned `String`. Copes
/// with a null pointer by returning an empty string.
unsafe fn pwstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
            if len > 4096 {
                break;
            }
        }
        let slice = std::slice::from_raw_parts(p, len);
        String::from_utf16_lossy(slice)
    }
}

/// Enumerate all drive letters currently mounted, returned as virtual
/// [`Entry`] items suitable for populating the "This PC" view.
///
/// Each drive becomes a `Directory`-kind entry whose `name` is
/// `"<letter>: (<label>)"` — **letter first**, unlike Explorer. A screen
/// reader reads the row left to right, and the letter is the part the user
/// is navigating by; putting the label first meant listening past a
/// variable-length name to reach it, and it made the alphabetical sort key
/// the label rather than the drive letter.
///
/// Drives with no volume label fall back to the drive kind — `E: (CD
/// Drive)` — so the parenthesised half is never empty.
///
/// Empty drives (e.g. CD drives with no disc) still show up — `GetDriveType`
/// reports them — which matches Explorer's behaviour and lets the user see
/// that the bay exists.
pub fn list_drives() -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    // Reading the label of an empty removable bay is a hard error; without
    // this the enumeration blocks on a system modal. See `ErrorModeGuard`.
    let _quiet = ErrorModeGuard::new();
    // 104 bytes (26 drives × 4 chars "A:\0") is plenty. Over-allocate a
    // touch to tolerate weird configurations.
    let mut buf = [0u16; 512];
    let written = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) };
    if written == 0 {
        return out;
    }

    // The buffer is a double-null-terminated sequence of null-terminated
    // strings. Walk it by scanning runs of non-zero u16.
    let mut i = 0usize;
    while i < written as usize {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if i == start {
            break;
        }
        let drive_wide = &buf[start..i];
        i += 1; // skip the terminator for next iteration

        let path_str = String::from_utf16_lossy(drive_wide);

        // Volume label, if we can read it. Unreadable drives (e.g. empty
        // card readers) still show up; we just leave the label blank.
        let mut label_buf = [0u16; 256];
        let mut drive_c: Vec<u16> = drive_wide.to_vec();
        drive_c.push(0);
        let got_label = unsafe {
            GetVolumeInformationW(
                drive_c.as_ptr(),
                label_buf.as_mut_ptr(),
                label_buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        let label_len = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
        let label: String = if got_label != 0 && label_len > 0 {
            String::from_utf16_lossy(&label_buf[..label_len])
        } else {
            String::new()
        };

        let drive_type = unsafe { GetDriveTypeW(drive_c.as_ptr()) };
        let kind_word = drive_kind_word(drive_type);
        let spec = path_str.trim_end_matches('\\');
        let display = if label.is_empty() {
            format!("{} ({})", spec, kind_word)
        } else {
            format!("{} ({})", spec, label)
        };

        out.push(Entry {
            name: display,
            kind: EntryKind::Directory,
            size: 0,
            modified: navigator_core::FileTime::default(),
            created: navigator_core::FileTime::default(),
            attrs: 0,
            hidden: false,
            system: false,
        });
    }
    out
}

/// Human word for a `GetDriveTypeW` result. Shared by [`list_drives`] and
/// [`drive_info`] so the This PC row and the properties screen can't drift
/// into calling the same volume two different things.
pub fn drive_kind_word(drive_type: u32) -> &'static str {
    match drive_type {
        DRIVE_FIXED => "Local Disk",
        DRIVE_REMOVABLE => "Removable Disk",
        DRIVE_CDROM => "CD Drive",
        DRIVE_REMOTE => "Network Drive",
        DRIVE_RAMDISK => "RAM Disk",
        _ => "Drive",
    }
}

/// Parse a drive-entry display name back to its root path. The virtual
/// [`Entry`] produced by [`list_drives`] leads with the drive spec
/// (`"C: (Windows)"`); opening it needs the `C:\` form.
///
/// Only the leading token is considered, so a *real* folder that happens to
/// be named `Foo (C:)` can never be mistaken for a volume.
pub fn drive_path_from_display(display: &str) -> Option<String> {
    let spec = display.split_whitespace().next()?;
    let mut chars = spec.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next() != Some(':') || chars.next().is_some() {
        return None;
    }
    // Restore trailing separator so the path is an absolute drive root.
    Some(format!("{}:\\", letter))
}

/// Capacity figures for a volume, in bytes.
///
/// `free` is what the volume has left; `available` is what the *calling
/// user* may still write, which is smaller when a disk quota applies. They
/// are equal on an unquota'd volume, which is the common case.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DriveSpace {
    pub total: u64,
    pub free: u64,
    pub available: u64,
}

impl DriveSpace {
    /// Bytes in use — `total - free`, saturating, since the two figures
    /// come from one call and can't legitimately invert.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Percentage of the volume in use, 0 when the total is unknown.
    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.used() as f64) * 100.0 / (self.total as f64)
    }
}

/// Cluster geometry from `GetDiskFreeSpaceW`.
///
/// Deliberately carries only the two figures that stay honest on a large
/// volume: that call also reports cluster *counts*, but they are 32-bit and
/// documented to saturate past ~2 TB, so reporting them would be a lie on
/// exactly the disks people ask about.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClusterInfo {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
}

impl ClusterInfo {
    /// Allocation unit size in bytes — the granularity every file on the
    /// volume rounds up to.
    pub fn allocation_unit(&self) -> u64 {
        (self.bytes_per_sector as u64) * (self.sectors_per_cluster as u64)
    }
}

/// Everything the OS will tell us about a volume **without enumerating
/// it**: identity, filesystem features and capacity.
///
/// This is the answer to "properties on a drive", where a recursive walk is
/// both the wrong number (it would count only what the user can read) and
/// unbounded work. Every field here comes from a constant-time call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DriveInfo {
    /// Root path as queried, e.g. `C:\`.
    pub root: String,
    /// Volume label, empty when the volume has none or is unreadable.
    pub label: String,
    /// `GetDriveTypeW` result and its human word.
    pub drive_type: u32,
    pub kind: &'static str,
    /// `NTFS`, `exFAT`, … — empty when the volume info call failed.
    pub file_system: String,
    /// Volume serial, rendered `XXXX-XXXX` by the UI.
    pub serial: u32,
    /// Longest filename component the filesystem accepts (255 on NTFS).
    pub max_component_len: u32,
    /// `FILE_*` filesystem capability flags; decoded by the UI.
    pub flags: u32,
    /// `\\?\Volume{GUID}\` mount-point name, empty when unavailable.
    pub volume_guid: String,
    /// False when `GetVolumeInformationW` failed — no media, or a
    /// disconnected network drive. Label / filesystem / flags are then
    /// meaningless rather than merely empty.
    pub volume_info_ok: bool,
    /// `None` when the drive has no media or the query was denied.
    pub space: Option<DriveSpace>,
    /// `None` under the same conditions as `space`.
    pub cluster: Option<ClusterInfo>,
}

impl DriveInfo {
    /// True when nothing could be read at all — an empty bay or an offline
    /// share. Lets the UI say so instead of rendering a screen of zeroes.
    pub fn is_unavailable(&self) -> bool {
        !self.volume_info_ok && self.space.is_none()
    }
}

/// Query one volume by root path (`"D:\"`, with or without the trailing
/// separator).
///
/// Four constant-time syscalls, no directory enumeration — safe to call for
/// a drive holding millions of files. Individual calls are allowed to fail
/// independently: an unformatted volume answers `GetDiskFreeSpaceExW` and
/// not `GetVolumeInformationW`, an offline share answers neither, and both
/// cases must still produce a screen the user can read.
pub fn drive_info(root: &str) -> DriveInfo {
    let root = format!("{}\\", root.trim_end_matches(['\\', '/']));
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

    // Same hazard as `list_drives`: an empty removable bay turns every one
    // of these queries into a system modal without this guard.
    let _quiet = ErrorModeGuard::new();

    let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
    let mut info = DriveInfo {
        root: root.clone(),
        drive_type,
        kind: drive_kind_word(drive_type),
        ..Default::default()
    };

    let mut label_buf = [0u16; 256];
    let mut fs_buf = [0u16; 64];
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            label_buf.as_mut_ptr(),
            label_buf.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_buf.as_mut_ptr(),
            fs_buf.len() as u32,
        )
    };
    if ok != 0 {
        info.volume_info_ok = true;
        info.label = wide_to_string(&label_buf);
        info.file_system = wide_to_string(&fs_buf);
        info.serial = serial;
        info.max_component_len = max_component;
        info.flags = flags;
    }

    let mut avail: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok != 0 {
        info.space = Some(DriveSpace {
            total,
            free,
            available: avail,
        });
    }

    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok != 0 && bytes_per_sector != 0 {
        info.cluster = Some(ClusterInfo {
            bytes_per_sector,
            sectors_per_cluster,
        });
    }

    // `\\?\Volume{…}\` — the identity that survives a drive-letter change,
    // and the only way to tell two removable disks apart in a log.
    let mut guid_buf = [0u16; 64];
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(
            wide.as_ptr(),
            guid_buf.as_mut_ptr(),
            guid_buf.len() as u32,
        )
    };
    if ok != 0 {
        info.volume_guid = wide_to_string(&guid_buf);
    }

    info
}

/// Decode a null-terminated wide buffer we own (as opposed to a pointer
/// handed back by the OS — that is [`pwstr_to_string`]'s job).
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Stat a single path and return its [`Entry`]. One `FindFirstFileExW`
/// against the exact path — **no directory enumeration**, so the cost is
/// independent of how many siblings the file has.
///
/// This exists for the file watcher, which fires once per changed file.
/// Answering each event with a `read_dir` of the parent made a paste or
/// extract of N files into a folder of M entries cost N×M work on the UI
/// thread; a folder with a few thousand entries froze the window solid
/// for the duration. Keep this O(1) — do not "simplify" it back into a
/// scan-and-find.
///
/// Returns `None` for a path that no longer exists (the common race: the
/// file was created and deleted before we got the notification), for a
/// bare drive root (`FindFirstFile` cannot stat one), and for `.` / `..`.
pub fn stat_entry(path: &Path) -> Option<Entry> {
    // A trailing separator makes FindFirstFile fail with
    // ERROR_INVALID_NAME — normalise before encoding.
    let mut wide: Vec<u16> = to_long_path(path);
    while matches!(wide.last(), Some(&c) if c == b'\\' as u16 || c == b'/' as u16) {
        wide.pop();
    }
    if wide.is_empty() {
        return None;
    }
    wide.push(0);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            wide.as_ptr(),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe { FindClose(handle) };
    entry_from_find_data(&data)
}

/// Recursively search `root` for entries whose filename matches `query`
/// (ASCII case-insensitive). Returned entries have `name` set to the
/// path *relative* to `root`, using `\\` as separator — so the GUI can
/// open them via `root.join(&name)` without knowing the subdirectory.
///
/// Matching strategy is inferred from the query shape:
///   * Wildcards `*` / `?` present  → glob match against the full
///     filename (anchored both ends). `*.png`, `foo?.txt`, `*report*`.
///   * Leading `.` and no wildcards → extension filter. `.png` matches
///     any file whose extension equals `png`.
///   * Otherwise                    → ASCII case-insensitive substring,
///     same behaviour as the original implementation.
///
/// `max_results` bounds memory for huge trees; callers typically cap at a
/// few thousand matches.
pub fn search_recursive(root: &NavPath, query: &str, max_results: usize) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    if query.is_empty() {
        return out;
    }
    let matcher = Matcher::new(query);

    let mut stack: Vec<std::path::PathBuf> = vec![root.as_path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max_results {
            break;
        }
        let entries = match read_dir_impl(&dir) {
            Ok(e) => e,
            Err(_) => continue, // unreadable sub-tree; skip
        };
        for entry in entries {
            // Recurse into real directories only (not reparse points, to
            // avoid symlink cycles that would hang the search).
            if matches!(entry.kind, navigator_core::EntryKind::Directory) {
                stack.push(dir.join(&entry.name));
            }
            if matcher.matches(&entry.name) {
                // Rewrite `name` to the path relative to `root`, so the
                // GUI can display + open it without a separate column.
                let full = dir.join(&entry.name);
                let rel = match full.strip_prefix(root.as_path()) {
                    Ok(r) => r.to_string_lossy().into_owned(),
                    Err(_) => entry.name.clone(),
                };
                out.push(Entry { name: rel, ..entry });
                if out.len() >= max_results {
                    break;
                }
            }
        }
    }
    out
}

/// Filename matcher inferred from the user's query string.
enum Matcher {
    Substring(String),
    Extension(String),
    Glob(Vec<GlobToken>),
}

#[derive(Debug, PartialEq, Eq)]
enum GlobToken {
    /// `*` — match any (possibly empty) run of characters.
    Star,
    /// `?` — match exactly one character.
    Any,
    /// Literal lowercased character.
    Char(char),
}

impl Matcher {
    fn new(query: &str) -> Self {
        let lower = query.to_ascii_lowercase();
        let has_glob = lower.chars().any(|c| c == '*' || c == '?');
        if has_glob {
            return Matcher::Glob(parse_glob(&lower));
        }
        // `.png`, `.tar.gz` — extension-only filter.
        if let Some(ext) = lower.strip_prefix('.')
            && !ext.is_empty()
            && !ext.contains('.')
        {
            return Matcher::Extension(ext.to_string());
        }
        Matcher::Substring(lower)
    }

    fn matches(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        match self {
            Matcher::Substring(s) => lower.contains(s.as_str()),
            Matcher::Extension(ext) => std::path::Path::new(&lower)
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| e == ext.as_str())
                .unwrap_or(false),
            Matcher::Glob(tokens) => glob_match(tokens, &lower),
        }
    }
}

fn parse_glob(pattern: &str) -> Vec<GlobToken> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut last_star = false;
    for c in pattern.chars() {
        match c {
            '*' => {
                // Collapse runs of `*` so the matcher's worst case stays
                // linear in pattern length.
                if !last_star {
                    out.push(GlobToken::Star);
                    last_star = true;
                }
            }
            '?' => {
                out.push(GlobToken::Any);
                last_star = false;
            }
            other => {
                out.push(GlobToken::Char(other));
                last_star = false;
            }
        }
    }
    out
}

/// Iterative glob matcher with `*` backtracking. Linear in `text.len()`
/// when the pattern has no `*`; otherwise O(p*t) worst case.
fn glob_match(pattern: &[GlobToken], text: &str) -> bool {
    let bytes: Vec<char> = text.chars().collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star: Option<(usize, usize)> = None;
    while ti < bytes.len() {
        match pattern.get(pi) {
            Some(GlobToken::Char(c)) if *c == bytes[ti] => {
                pi += 1;
                ti += 1;
            }
            Some(GlobToken::Any) => {
                pi += 1;
                ti += 1;
            }
            Some(GlobToken::Star) => {
                star = Some((pi, ti));
                pi += 1;
            }
            _ => {
                if let Some((sp, st)) = star {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, ti));
                } else {
                    return false;
                }
            }
        }
    }
    while matches!(pattern.get(pi), Some(GlobToken::Star)) {
        pi += 1;
    }
    pi == pattern.len()
}

fn read_dir_impl(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut pattern: Vec<u16> = to_long_path(dir);
    // Append `\*` — required for FindFirstFileW to enumerate.
    if !matches!(pattern.last(), Some(&c) if c == b'\\' as u16 || c == b'/' as u16) {
        pattern.push(b'\\' as u16);
    }
    pattern.push(b'*' as u16);
    pattern.push(0);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(Vec::new());
        }
        return Err(std::io::Error::from_raw_os_error(code as i32));
    }

    // Large allocations get amortized by the Vec's own growth strategy; no
    // magic up-front size since we don't know the count.
    let mut out: Vec<Entry> = Vec::with_capacity(128);

    loop {
        if let Some(entry) = entry_from_find_data(&data) {
            out.push(entry);
        }
        let ok = unsafe { FindNextFileW(handle, &raw mut data) };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_NO_MORE_FILES {
                unsafe { FindClose(handle) };
                return Err(std::io::Error::from_raw_os_error(code as i32));
            }
            break;
        }
    }
    unsafe { FindClose(handle) };
    Ok(out)
}

fn entry_from_find_data(d: &WIN32_FIND_DATAW) -> Option<Entry> {
    let name = read_wstr(&d.cFileName);
    if name == "." || name == ".." {
        return None;
    }
    let attrs = d.dwFileAttributes;
    let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        EntryKind::Symlink
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
    let modified = FileTime(
        ((d.ftLastWriteTime.dwHighDateTime as u64) << 32) | d.ftLastWriteTime.dwLowDateTime as u64,
    );
    let created = FileTime(
        ((d.ftCreationTime.dwHighDateTime as u64) << 32) | d.ftCreationTime.dwLowDateTime as u64,
    );
    let hidden = attrs & FILE_ATTRIBUTE_HIDDEN != 0;
    let system = attrs & FILE_ATTRIBUTE_SYSTEM != 0;
    Some(Entry {
        name,
        kind,
        size,
        modified,
        created,
        attrs,
        hidden,
        system,
    })
}

fn read_wstr(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// UTF-16 encode `p`, prepending `\\?\` when it would exceed MAX_PATH. The
/// prefix bypasses the legacy 260-char limit.
fn to_long_path(p: &Path) -> Vec<u16> {
    const LIMIT: usize = 247; // conservative; MAX_PATH - some headroom for the \*
    let raw: Vec<u16> = p.as_os_str().encode_wide().collect();
    let needs_prefix = raw.len() >= LIMIT && !starts_with_long_prefix(&raw);
    if !needs_prefix {
        return raw;
    }
    let mut out: Vec<u16> = Vec::with_capacity(raw.len() + 4);
    for c in r"\\?\".encode_utf16() {
        out.push(c);
    }
    out.extend_from_slice(&raw);
    out
}

fn starts_with_long_prefix(s: &[u16]) -> bool {
    let pre: Vec<u16> = r"\\?\".encode_utf16().collect();
    s.len() >= pre.len() && s[..pre.len()] == pre[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_query_matches_anywhere_case_insensitive() {
        let m = Matcher::new("Report");
        assert!(m.matches("annual_report.txt"));
        assert!(m.matches("REPORTS"));
        assert!(!m.matches("notes.md"));
    }

    #[test]
    fn extension_query_matches_only_that_extension() {
        let m = Matcher::new(".png");
        assert!(m.matches("logo.PNG"));
        assert!(m.matches("a.b.png"));
        assert!(!m.matches("png.txt"));
        assert!(!m.matches("readme"));
    }

    #[test]
    fn glob_star_matches_extension_pattern() {
        let m = Matcher::new("*.png");
        assert!(m.matches("logo.png"));
        assert!(m.matches("a.b.png"));
        assert!(!m.matches("png"));
        assert!(!m.matches("logo.pngx"));
    }

    #[test]
    fn glob_question_matches_single_char() {
        let m = Matcher::new("v?.txt");
        assert!(m.matches("v1.txt"));
        assert!(m.matches("vA.txt"));
        assert!(!m.matches("v.txt"));
        assert!(!m.matches("v12.txt"));
    }

    #[test]
    fn glob_internal_star_matches_filler() {
        let m = Matcher::new("a*z.log");
        assert!(m.matches("az.log"));
        assert!(m.matches("amiddleZ.log"));
        assert!(!m.matches("amiddle.log"));
    }

    #[test]
    fn dotted_query_with_inner_dot_falls_back_to_substring() {
        let m = Matcher::new(".tar.gz");
        // Two dots → treat as substring, not extension filter.
        assert!(m.matches("backup.tar.gz"));
        assert!(!m.matches("readme.gz"));
    }
}
