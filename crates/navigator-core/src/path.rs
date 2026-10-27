use std::path::{Path, PathBuf};

/// An absolute path normalized to forward `\` separators.
///
/// Kept as [`PathBuf`] so it hands back directly to `std::fs` or Win32.
/// Constructors reject relative paths — the GUI only navigates absolutes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NavPath(PathBuf);

/// Sentinel path used to represent the "This PC" virtual root — the
/// drives-and-network view that sits above any drive letter. Chosen so it
/// never collides with a real filesystem path (device namespace is
/// reserved).
pub const THIS_PC_SENTINEL: &str = r"\\?\NavigatorThisPC";

/// Sentinel for the "Remotes" virtual root — the listing of every rclone
/// remote configured in `rclone.conf`. Activating an entry from here drops
/// into that remote's root (see [`REMOTE_PREFIX`]).
pub const REMOTES_ROOT_SENTINEL: &str = r"\\?\NavigatorRemotes";

/// Prefix encoding a remote path inside a `NavPath`. Layout is
/// `\\?\NavigatorRemote\<remote-name>\<sub\path>` — backslash separated so
/// `Path::parent` / `Path::join` behave, remote name carrying no colon so
/// it round-trips cleanly. The trailing `\` is part of the prefix for
/// `starts_with` checks; the remote root itself is `\\?\NavigatorRemote\<name>`.
pub const REMOTE_PREFIX: &str = r"\\?\NavigatorRemote\";

/// Sibling of [`REMOTE_PREFIX`] for remote paths whose sub-path was
/// written with a leading `/`. Some rclone backends distinguish
/// `mac:/etc` (filesystem root on a Linux box) from `mac:etc`
/// (relative to the user's home directory) — collapsing both into the
/// same storage form would silently change which file rclone touches.
/// We pick a distinct prefix so `parent` / `join` behave naturally
/// without smuggling a flag inside the sub-path. The remote root
/// `mac:/` (abs prefix, empty sub) is `\\?\NavigatorRemoteAbs\<name>`.
pub const REMOTE_ABS_PREFIX: &str = r"\\?\NavigatorRemoteAbs\";

/// Prefix encoding a path inside a rustic backup repository. Layout is
/// `\\?\NavigatorRustic\<repo>[\<snapshot>[\<path\in\snapshot>]]`:
///
///   * `<repo>` is the repository exactly as rustic's `-r` takes it, with
///     `%`, `\`, `/` and `:` percent-escaped so it stays one component
///     (see [`encode_rustic_repo`]). The repo root lists snapshots.
///   * `<snapshot>` is a snapshot *row name* — a human-readable label
///     whose last space-separated token is the snapshot id. The label is
///     what the user reads; only the id ever reaches rustic.
///   * the rest is the path inside the snapshot's tree (`C\Users\…` for a
///     Windows backup), backslash separated like everything else here so
///     `parent` / `join` stay string operations.
///
/// Everything under this prefix is **read-only**: a backup is made on the
/// command line, and navigator only browses it and restores out of it.
pub const RUSTIC_PREFIX: &str = r"\\?\NavigatorRustic\";

/// What the address bar and title show for a rustic path, and what
/// [`NavPath::new`] parses back: `rustic:<repo>` for the snapshot list,
/// then ` › <snapshot> › <path>` as the user descends.
pub const RUSTIC_DISPLAY_PREFIX: &str = "rustic:";
const RUSTIC_DISPLAY_SEP: &str = " › ";

/// The three parts of a rustic path. See [`RUSTIC_PREFIX`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RusticParts {
    /// Repository spec, decoded — what goes after `rustic -r`.
    pub repo: String,
    /// Snapshot row name; `None` at the repository root.
    pub snapshot: Option<String>,
    /// Forward-slash path inside the snapshot; empty at the snapshot root.
    pub sub: String,
}

/// Escape a repository spec into a single path component. A local repo is
/// `D:\backups\repo`; a remote one is `rclone:remote:path` or
/// `sftp:user@host:/srv/repo` — every one of them carries separators or
/// colons that would otherwise split or confuse the component.
pub fn encode_rustic_repo(repo: &str) -> String {
    let mut out = String::with_capacity(repo.len());
    for c in repo.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\\' => out.push_str("%5C"),
            '/' => out.push_str("%2F"),
            ':' => out.push_str("%3A"),
            c => out.push(c),
        }
    }
    out
}

/// Inverse of [`encode_rustic_repo`]. Unknown escapes are kept literally
/// rather than rejected — the input only ever comes from the encoder.
pub fn decode_rustic_repo(enc: &str) -> String {
    let mut out = String::with_capacity(enc.len());
    let mut rest = enc;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let esc = rest.get(i..i + 3).unwrap_or(&rest[i..]);
        match esc {
            "%25" => out.push('%'),
            "%5C" | "%5c" => out.push('\\'),
            "%2F" | "%2f" => out.push('/'),
            "%3A" | "%3a" => out.push(':'),
            other => out.push_str(other),
        }
        rest = &rest[i + esc.len()..];
    }
    out.push_str(rest);
    out
}

/// Spell a local repository the way rustic needs it.
///
/// rustic reads `<word>:` as a backend name, so `C:/backups/repo` is
/// rejected as "backend type `C` is not supported" — measured against
/// rustic 0.11. Only the backslash spelling of a drive path is taken as a
/// local directory, so forward slashes in a drive or UNC path are turned
/// around here. Anything else (`rclone:…`, `sftp:…`, `local:…`) is a
/// backend spec and passes through untouched.
pub fn normalize_rustic_repo(repo: &str) -> String {
    let b = repo.as_bytes();
    let is_drive = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    let is_unc = repo.starts_with("//") || repo.starts_with(r"\\");
    let s = if is_drive || is_unc {
        repo.replace('/', "\\")
    } else {
        repo.to_string()
    };
    // `D:\repo\` and `D:\repo` are one repository; keep a single spelling
    // so the two don't become two cache keys and two password prompts.
    if s.len() > 3 && (s.ends_with('\\') || s.ends_with('/')) && (is_drive || is_unc) {
        s.trim_end_matches(['\\', '/']).to_string()
    } else {
        s
    }
}

/// rustic's backend prefix for a repository reached through rclone. rustic
/// runs `rclone serve restic <rest>` itself and talks to that, so any
/// remote navigator can browse is a remote a repository can live on.
pub const RUSTIC_RCLONE_BACKEND: &str = "rclone:";

/// The repository spec for a folder the user is looking at: a local folder
/// as-is (respelled by [`normalize_rustic_repo`]), an rclone remote folder
/// as `rclone:<remote:path>`. `None` for a sentinel — This PC, the remotes
/// list, or somewhere already inside a backup.
pub fn rustic_repo_spec(folder: &NavPath) -> Option<String> {
    if folder.is_remote() {
        return folder
            .rclone_arg()
            .map(|a| format!("{RUSTIC_RCLONE_BACKEND}{a}"));
    }
    if folder.is_this_pc() || folder.is_remotes_root() || folder.is_rustic() {
        return None;
    }
    Some(normalize_rustic_repo(&folder.to_string()))
}

/// Where repository `repo` lives as a browsable folder — the inverse of
/// [`rustic_repo_spec`]. A local repository is its own path; an
/// `rclone:<remote:path>` one is that remote folder. `None` for any other
/// backend (`sftp:`, `rest:`, …), which has nowhere to browse to.
///
/// rustic itself reads `rclone:C:\…` as a *local* path (measured, 0.11.4),
/// so an rclone spec counts only when the rest parses as a remote.
pub fn rustic_repo_location(repo: &str) -> Option<NavPath> {
    if let Some(rest) = repo.strip_prefix(RUSTIC_RCLONE_BACKEND) {
        return NavPath::new(PathBuf::from(rest))
            .ok()
            .filter(NavPath::is_remote);
    }
    Path::new(repo)
        .is_absolute()
        .then(|| NavPath(PathBuf::from(repo)))
}

/// Parse the display form (`rustic:D:\repo › snapshot › C\Users`) back
/// into a rustic path, or `None` when `s` isn't one.
///
/// `rustic:photos` stays an rclone remote called `rustic`: the text after
/// the prefix must look like a repository — a drive or UNC path, another
/// backend spec (it contains a colon), or the display separator.
fn parse_rustic_display(s: &str) -> Option<NavPath> {
    let rest = s.strip_prefix(RUSTIC_DISPLAY_PREFIX)?;
    let mut parts = rest.split(RUSTIC_DISPLAY_SEP);
    let repo = parts.next()?.trim();
    let b = repo.as_bytes();
    let looks_like_repo = rest.contains(RUSTIC_DISPLAY_SEP)
        || repo.starts_with(r"\\")
        || repo.starts_with("//")
        || repo.contains(':')
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    if repo.is_empty() || !looks_like_repo {
        return None;
    }
    let repo = normalize_rustic_repo(repo);
    let Some(snapshot) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(NavPath::rustic_repo(&repo));
    };
    let sub = parts.collect::<Vec<_>>().join(RUSTIC_DISPLAY_SEP);
    Some(NavPath::rustic(&repo, snapshot, sub.trim()))
}

impl NavPath {
    pub fn new(p: impl Into<PathBuf>) -> crate::Result<Self> {
        let p = p.into();
        if p.is_absolute() {
            return Ok(Self(p));
        }
        let s = p.to_string_lossy();
        // Checked before the rclone branch: `rustic:D:\repo` would
        // otherwise parse as an rclone remote named `rustic`.
        if let Some(r) = parse_rustic_display(&s) {
            return Ok(r);
        }
        // rclone-style remote syntax — `name:` or `name:sub/path`. Treat
        // this as a first-class NavPath so the address bar can accept
        // `gdrive:` / `gdrive:photos/2024` without special casing. A bare
        // one-letter prefix is a Windows drive (`C:foo`) so skip it; the
        // caller either provided a root already (is_absolute handled it)
        // or wants the relative-path error that comes at the end.
        if let Some(colon) = s.find(':') {
            let name = &s[..colon];
            let rest = &s[colon + 1..];
            let looks_like_drive = name.len() == 1
                && name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic())
                    .unwrap_or(false);
            let valid_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
            // Bare drive prefix (`c:` / `C:`) — Rust treats this as
            // drive-relative and rejects, but users typing `c:` in the
            // address bar mean the drive root. Also accept `c:/` and
            // `c:\` (already absolute via the early return, but be
            // defensive). Drive-relative paths with content like `c:foo`
            // are still rejected — those are an editor concept we don't
            // implement.
            if looks_like_drive && (rest.is_empty() || rest == "/" || rest == "\\") {
                let alt = PathBuf::from(format!("{}:\\", name));
                if alt.is_absolute() {
                    return Ok(Self(alt));
                }
            }
            if valid_name && !looks_like_drive {
                // A leading `/` or `\` in the sub-path is significant for
                // some rclone backends (sftp / local on Linux): `mac:/etc`
                // is the filesystem root, `mac:etc` is relative to the
                // remote's home. Preserve that distinction by routing to
                // `remote_abs` when the sub starts with a separator.
                let starts_abs = rest.starts_with('/') || rest.starts_with('\\');
                let sub = rest.replace('\\', "/");
                let sub = sub.trim_start_matches('/');
                return Ok(if starts_abs {
                    Self::remote_abs(name, sub)
                } else {
                    Self::remote(name, sub)
                });
            }
        }
        // UNC share root without a trailing separator — `\\host\share` or
        // `//host/share` — is NOT `is_absolute()` in Rust's book because it
        // has a prefix but no root component. Retry with a trailing `\`,
        // which makes the same path absolute. Users typing an IP-based
        // share (e.g. `\\100.86.173.34\media`) hit this constantly.
        let is_unc = s.starts_with(r"\\") || s.starts_with("//");
        let has_trailing = s.ends_with('\\') || s.ends_with('/');
        if is_unc && !has_trailing {
            let alt = PathBuf::from(format!("{}\\", s));
            if alt.is_absolute() {
                return Ok(Self(alt));
            }
        }
        // Host-only UNC like `\\host` or `\\100.86.173.34` (no share
        // component at all). Rust never considers this absolute no matter
        // how we massage the separator, because there's no share segment.
        // Accept it anyway — the scanner recognises host-only UNCs via
        // `is_unc_host_only` and enumerates the host's network shares
        // (NetShareEnum) instead of calling `read_dir`.
        if is_unc {
            let trimmed: String = s.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
            let body = trimmed.trim_start_matches('\\').trim_end_matches('\\');
            if !body.is_empty() && !body.contains('\\') {
                // Exactly one non-empty segment after the `\\` prefix =
                // host-only UNC. Normalise to `\\host` (no trailing).
                return Ok(Self(PathBuf::from(format!(r"\\{}", body))));
            }
        }
        Err(crate::Error::NotAbsolute(p))
    }

    /// `true` for a UNC network path — `\\host\share\...`, the host-only
    /// `\\host` form, or the extended-length `\\?\UNC\host\share`.
    ///
    /// Delete branches on this: a network share must never get a
    /// `<volume_root>\.trash` staging directory. `volume_root_of` happily
    /// resolves `\\host\share\` as a volume, so the trash rename used to
    /// "work" — it dropped a `.trash` folder at the root of somebody
    /// else's file server, and on a macOS SMB share the dot-prefix comes
    /// back flagged hidden so the user could not even see where their
    /// file went.
    ///
    /// The sentinels this type uses for This PC and for rclone remotes
    /// also start with two backslashes, so they are excluded explicitly:
    /// a sentinel is not a network location. `\\.\` (the device
    /// namespace) is out for the same reason.
    pub fn is_unc(&self) -> bool {
        let s: String = self
            .0
            .to_string_lossy()
            .chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .collect();
        if !s.starts_with(r"\\") {
            return false;
        }
        // `\\?\UNC\host\share` is the extended-length spelling of a share
        // and *is* a network path; every other `\\?\` (our sentinels,
        // extended-length local paths) and all of `\\.\` is not.
        if s.starts_with(r"\\?\UNC\") {
            return true;
        }
        !(s.starts_with(r"\\?\") || s.starts_with(r"\\.\"))
    }

    /// `true` for a host-only UNC path (`\\host` or `\\1.2.3.4`) with no
    /// share component. The scanner routes these to share enumeration
    /// instead of `read_dir`, because `FindFirstFileW` on a bare host
    /// always fails with `ERROR_BAD_NETPATH`.
    pub fn is_unc_host_only(&self) -> bool {
        let s = self.0.to_string_lossy();
        if !(s.starts_with(r"\\") || s.starts_with("//")) {
            return false;
        }
        let body: String = s
            .chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .collect::<String>()
            .trim_start_matches('\\')
            .trim_end_matches('\\')
            .to_string();
        !body.is_empty() && !body.contains('\\')
    }

    /// The host portion of a host-only UNC path (`\\host` → `"host"`).
    /// Returns `None` for non-UNC or fully-qualified shares.
    pub fn unc_host(&self) -> Option<String> {
        if !self.is_unc_host_only() {
            return None;
        }
        let s = self.0.to_string_lossy();
        let body: String = s
            .chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .collect::<String>()
            .trim_start_matches('\\')
            .trim_end_matches('\\')
            .to_string();
        Some(body)
    }

    /// Virtual root showing all connected drives + network locations.
    pub fn this_pc() -> Self {
        Self(PathBuf::from(THIS_PC_SENTINEL))
    }

    pub fn is_this_pc(&self) -> bool {
        self.0.as_os_str() == THIS_PC_SENTINEL
    }

    /// Virtual root listing every configured rclone remote.
    pub fn remotes_root() -> Self {
        Self(PathBuf::from(REMOTES_ROOT_SENTINEL))
    }

    pub fn is_remotes_root(&self) -> bool {
        self.0.as_os_str() == REMOTES_ROOT_SENTINEL
    }

    /// Build a NavPath pointing into remote `name` at sub-path `sub`
    /// (forward-slash separated, empty for the remote root). The stored
    /// form normalises separators to `\` so `Path::join` / `Path::parent`
    /// still work.
    pub fn remote(name: &str, sub: &str) -> Self {
        Self::remote_with_prefix(REMOTE_PREFIX, name, sub)
    }

    /// Like [`remote`], but the resulting path round-trips through
    /// [`rclone_arg`] as `name:/sub` rather than `name:sub`. Use when the
    /// user explicitly wrote a leading `/` (or `\`) before the sub-path.
    pub fn remote_abs(name: &str, sub: &str) -> Self {
        Self::remote_with_prefix(REMOTE_ABS_PREFIX, name, sub)
    }

    fn remote_with_prefix(prefix: &str, name: &str, sub: &str) -> Self {
        let mut s = String::from(prefix);
        s.push_str(name);
        let sub = sub.trim_matches(|c| c == '/' || c == '\\');
        if !sub.is_empty() {
            s.push('\\');
            for c in sub.chars() {
                s.push(if c == '/' { '\\' } else { c });
            }
        }
        Self(PathBuf::from(s))
    }

    pub fn is_remote(&self) -> bool {
        let s = self.0.to_string_lossy();
        s.starts_with(REMOTE_PREFIX) || s.starts_with(REMOTE_ABS_PREFIX)
    }

    /// `true` if this remote path was constructed with [`remote_abs`] —
    /// i.e. the original user input had a leading `/` after the colon.
    pub fn is_remote_abs(&self) -> bool {
        self.0.to_string_lossy().starts_with(REMOTE_ABS_PREFIX)
    }

    /// `(remote-name, forward-slash sub-path)` for remote paths, or `None`.
    pub fn remote_parts(&self) -> Option<(String, String)> {
        let s = self.0.to_string_lossy().into_owned();
        let tail = s
            .strip_prefix(REMOTE_ABS_PREFIX)
            .or_else(|| s.strip_prefix(REMOTE_PREFIX))?;
        let (name, rest) = match tail.find('\\') {
            Some(i) => (&tail[..i], &tail[i + 1..]),
            None => (tail, ""),
        };
        Some((name.to_string(), rest.replace('\\', "/")))
    }

    /// `true` for exactly the remote root (`remote:` or `remote:/` with
    /// no sub-path).
    pub fn is_remote_root(&self) -> bool {
        match self.remote_parts() {
            Some((_, sub)) => sub.is_empty(),
            None => false,
        }
    }

    /// CLI argument form for rclone (`remote:sub/path` or
    /// `remote:/sub/path` for absolute remote paths). `None` for
    /// non-remote paths — local paths cross the CLI as plain strings
    /// via `as_path`.
    pub fn rclone_arg(&self) -> Option<String> {
        let (name, sub) = self.remote_parts()?;
        let abs = self.is_remote_abs();
        Some(match (sub.is_empty(), abs) {
            (true, false) => format!("{}:", name),
            (true, true) => format!("{}:/", name),
            (false, false) => format!("{}:{}", name, sub),
            (false, true) => format!("{}:/{}", name, sub),
        })
    }

    /// The snapshot list of rustic repository `repo` (as rustic's `-r`
    /// takes it). See [`RUSTIC_PREFIX`].
    pub fn rustic_repo(repo: &str) -> Self {
        let mut s = String::from(RUSTIC_PREFIX);
        s.push_str(&encode_rustic_repo(repo));
        Self(PathBuf::from(s))
    }

    /// A path inside snapshot `snapshot` (a row name) of `repo`. `sub` is
    /// the path in the snapshot tree, either separator, empty for its root.
    pub fn rustic(repo: &str, snapshot: &str, sub: &str) -> Self {
        let mut s = String::from(RUSTIC_PREFIX);
        s.push_str(&encode_rustic_repo(repo));
        s.push('\\');
        s.push_str(snapshot);
        let sub = sub.trim_matches(|c| c == '/' || c == '\\');
        if !sub.is_empty() {
            s.push('\\');
            for c in sub.chars() {
                s.push(if c == '/' { '\\' } else { c });
            }
        }
        Self(PathBuf::from(s))
    }

    /// `true` anywhere inside a rustic repository — the snapshot list,
    /// a snapshot, or a path within one. All of it is read-only.
    pub fn is_rustic(&self) -> bool {
        self.0.to_string_lossy().starts_with(RUSTIC_PREFIX)
    }

    pub fn rustic_parts(&self) -> Option<RusticParts> {
        let s = self.0.to_string_lossy();
        let tail = s.strip_prefix(RUSTIC_PREFIX)?;
        let mut it = tail.splitn(3, '\\');
        let repo = decode_rustic_repo(it.next()?);
        if repo.is_empty() {
            return None;
        }
        let snapshot = it.next().filter(|s| !s.is_empty()).map(str::to_string);
        let sub = it.next().unwrap_or("").replace('\\', "/");
        Some(RusticParts {
            repo,
            snapshot,
            sub,
        })
    }

    /// `true` for a repository's snapshot list.
    pub fn is_rustic_repo_root(&self) -> bool {
        self.rustic_parts().is_some_and(|p| p.snapshot.is_none())
    }

    /// Address-bar / title form, `rustic:<repo> › <snapshot> › <path>`.
    /// [`NavPath::new`] parses it back. `None` for non-rustic paths.
    pub fn rustic_display(&self) -> Option<String> {
        let p = self.rustic_parts()?;
        let mut out = format!("{RUSTIC_DISPLAY_PREFIX}{}", p.repo);
        if let Some(snap) = p.snapshot {
            out.push_str(RUSTIC_DISPLAY_SEP);
            out.push_str(&snap);
            if !p.sub.is_empty() {
                out.push_str(RUSTIC_DISPLAY_SEP);
                out.push_str(&p.sub.replace('/', "\\"));
            }
        }
        Some(out)
    }

    /// Root path: drive root on Windows (`C:\`), `/` elsewhere.
    pub fn default_root() -> Self {
        #[cfg(windows)]
        {
            let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            let mut s = drive;
            s.push('\\');
            Self(PathBuf::from(s))
        }
        #[cfg(not(windows))]
        {
            Self(PathBuf::from("/"))
        }
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// Parent folder, or `None` if already at a filesystem root (or at the
    /// This PC virtual root). The caller decides whether to treat `None`
    /// as "stop" or as "go to This PC".
    pub fn parent(&self) -> Option<Self> {
        if self.is_this_pc() {
            return None;
        }
        if self.is_remotes_root() {
            return None;
        }
        if let Some(parts) = self.rustic_parts() {
            // Inside a snapshot: trim one component, and from the
            // snapshot root step out to the snapshot list. From the list
            // itself, the parent is the folder holding the repository —
            // local or on an rclone remote — where the user would look for
            // it in a plain listing. Any other backend has nowhere to go.
            let s = self.0.to_string_lossy().into_owned();
            if parts.snapshot.is_some() {
                let idx = s.rfind('\\')?;
                return Some(Self(PathBuf::from(&s[..idx])));
            }
            return rustic_repo_location(&parts.repo)?.parent();
        }
        if self.is_remote() {
            // At a remote root, step out to the Remotes listing; inside a
            // remote, trim one sub-path component. Using the raw string
            // keeps us independent of `PathBuf::parent`'s verbatim-prefix
            // behaviour. The same logic applies to both relative and
            // absolute remote prefixes — we just preserve whichever the
            // path was built with.
            let s = self.0.to_string_lossy().into_owned();
            let (prefix, tail) = if let Some(t) = s.strip_prefix(REMOTE_ABS_PREFIX) {
                (REMOTE_ABS_PREFIX, t)
            } else if let Some(t) = s.strip_prefix(REMOTE_PREFIX) {
                (REMOTE_PREFIX, t)
            } else {
                return None;
            };
            if !tail.contains('\\') {
                return Some(Self::remotes_root());
            }
            let idx = tail.rfind('\\')?;
            let mut out = String::from(prefix);
            out.push_str(&tail[..idx]);
            return Some(Self(PathBuf::from(out)));
        }
        self.0
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| Self(p.to_path_buf()))
    }

    /// Join a child name (no separators in `name`).
    pub fn join(&self, name: &str) -> Self {
        // Remotes root entries are remote names — joining "gdrive" builds
        // the remote root, not `\\?\NavigatorRemotes\gdrive`.
        if self.is_remotes_root() {
            return Self::remote(name, "");
        }
        // Plain concatenation for rustic paths: `PathBuf::join` replaces
        // the whole path when the pushed name looks absolute or carries a
        // drive prefix, and a snapshot tree is free to contain either.
        if self.is_rustic() {
            let mut s = self.0.to_string_lossy().into_owned();
            s.push('\\');
            s.push_str(name);
            return Self(PathBuf::from(s));
        }
        Self(self.0.join(name))
    }

    pub fn file_name(&self) -> &str {
        self.0
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
    }
}

impl AsRef<Path> for NavPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for NavPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_roundtrip() {
        let p = NavPath::remote("gdrive", "photos/2024");
        assert!(p.is_remote());
        assert!(!p.is_remote_root());
        assert_eq!(
            p.remote_parts(),
            Some(("gdrive".into(), "photos/2024".into()))
        );
        assert_eq!(p.rclone_arg().as_deref(), Some("gdrive:photos/2024"));
        assert_eq!(p.file_name(), "2024");
    }

    #[test]
    fn remote_root_parent_is_remotes_root() {
        let p = NavPath::remote("gdrive", "");
        assert!(p.is_remote_root());
        assert_eq!(p.rclone_arg().as_deref(), Some("gdrive:"));
        let parent = p.parent().unwrap();
        assert!(parent.is_remotes_root());
    }

    #[test]
    fn remote_subpath_parent_trims_one() {
        let p = NavPath::remote("s3", "bucket/a/b");
        let parent = p.parent().unwrap();
        assert_eq!(parent.rclone_arg().as_deref(), Some("s3:bucket/a"));
    }

    #[test]
    fn new_accepts_rclone_syntax() {
        let p = NavPath::new("gdrive:").unwrap();
        assert!(p.is_remote_root());
        let p = NavPath::new("gdrive:photos/2024").unwrap();
        assert_eq!(p.rclone_arg().as_deref(), Some("gdrive:photos/2024"));
    }

    #[test]
    fn new_rejects_single_letter_drive() {
        // `C:foo` is a Windows drive-relative path, not a rclone remote.
        assert!(NavPath::new("C:foo").is_err());
    }

    #[test]
    fn remotes_root_join_builds_remote() {
        let p = NavPath::remotes_root().join("gdrive");
        assert!(p.is_remote_root());
        assert_eq!(p.rclone_arg().as_deref(), Some("gdrive:"));
    }

    #[test]
    fn bare_drive_letter_resolves_to_root() {
        // `c:` alone is drive-relative per Rust, so NavPath::new used to
        // reject it. Users typing this in the address bar mean the drive
        // root — accept it as `C:\`. Casing is preserved.
        let p = NavPath::new("c:").unwrap();
        assert_eq!(p.as_path(), std::path::Path::new("c:\\"));
        let p = NavPath::new("D:").unwrap();
        assert_eq!(p.as_path(), std::path::Path::new("D:\\"));
        // Slash variants too.
        let p = NavPath::new("e:/").unwrap();
        assert_eq!(p.as_path(), std::path::Path::new("e:\\"));
        // Drive-relative paths with content stay rejected — they're an
        // editor concept we don't want to silently rewrite.
        assert!(NavPath::new("c:foo").is_err());
    }

    #[test]
    fn remote_with_leading_slash_is_absolute() {
        // `mac:/users` must round-trip with the slash intact — for sftp
        // and friends that's the filesystem root, not the user's home.
        let p = NavPath::new("mac:/users").unwrap();
        assert!(p.is_remote_abs());
        assert_eq!(p.rclone_arg().as_deref(), Some("mac:/users"));
        assert_eq!(p.remote_parts(), Some(("mac".into(), "users".into())));

        // Without the slash we keep the historical relative form.
        let p = NavPath::new("mac:users").unwrap();
        assert!(!p.is_remote_abs());
        assert_eq!(p.rclone_arg().as_deref(), Some("mac:users"));
    }

    const SNAP: &str = "2026-10-22 10:13 data 4f13f557";

    #[test]
    fn rustic_repo_spec_survives_as_one_component() {
        for repo in [
            r"D:\backups\repo",
            "rclone:mac:backups/repo",
            "sftp:me@host:/srv/repo",
            r"\\nas\share\repo",
            "odd%name:x",
        ] {
            let p = NavPath::rustic_repo(repo);
            assert!(p.is_rustic() && p.is_rustic_repo_root(), "{repo}");
            let parts = p.rustic_parts().unwrap();
            assert_eq!(parts.repo, repo);
            assert_eq!(parts.snapshot, None);
            assert!(!p.is_unc(), "a rustic path is not a network location");
            assert!(!p.is_remote());
        }
    }

    #[test]
    fn rustic_join_and_parent_walk_the_tree() {
        let root = NavPath::rustic_repo(r"D:\backups\repo");
        let snap = root.join(SNAP);
        assert_eq!(snap.rustic_parts().unwrap().snapshot.as_deref(), Some(SNAP));
        assert_eq!(snap.file_name(), SNAP);
        let deep = snap.join("C").join("Users").join("a.txt");
        let parts = deep.rustic_parts().unwrap();
        assert_eq!(parts.sub, "C/Users/a.txt");
        assert_eq!(deep.file_name(), "a.txt");
        assert_eq!(
            deep,
            NavPath::rustic(r"D:\backups\repo", SNAP, "C/Users/a.txt")
        );

        let up = deep.parent().unwrap();
        assert_eq!(up.rustic_parts().unwrap().sub, "C/Users");
        let up = up.parent().unwrap().parent().unwrap();
        assert_eq!(up, snap);
        assert_eq!(up.parent().unwrap(), root);
        // A local repository's parent is the folder holding it.
        assert_eq!(root.parent().unwrap().as_path(), Path::new(r"D:\backups"));
        // An rclone repository's parent is the remote folder holding it.
        let up = NavPath::rustic_repo("rclone:sb:/home/rustic")
            .parent()
            .unwrap();
        assert_eq!(up.rclone_arg().as_deref(), Some("sb:/home"));
        let up = NavPath::rustic_repo("rclone:mac:repo").parent().unwrap();
        assert!(up.is_remote_root());
        // Any other backend has nowhere to browse to.
        assert!(
            NavPath::rustic_repo("sftp:me@host:/srv/repo")
                .parent()
                .is_none()
        );
    }

    #[test]
    fn rustic_repo_spec_and_location_are_inverses() {
        for (folder, spec) in [
            (
                NavPath::new("sb:/home/rustic").unwrap(),
                "rclone:sb:/home/rustic",
            ),
            (
                NavPath::new("mac:backups/repo").unwrap(),
                "rclone:mac:backups/repo",
            ),
            (NavPath::new("mac:").unwrap(), "rclone:mac:"),
            (
                NavPath::new(r"D:\backups\repo").unwrap(),
                r"D:\backups\repo",
            ),
        ] {
            assert_eq!(rustic_repo_spec(&folder).as_deref(), Some(spec));
            assert_eq!(rustic_repo_location(spec), Some(folder), "{spec}");
        }
        assert_eq!(rustic_repo_spec(&NavPath::this_pc()), None);
        assert_eq!(rustic_repo_spec(&NavPath::remotes_root()), None);
        assert_eq!(rustic_repo_spec(&NavPath::rustic_repo(r"D:\r")), None);
        // rustic reads `rclone:C:\…` as a local path; it is no remote.
        assert_eq!(rustic_repo_location(r"rclone:C:\r"), None);
        assert_eq!(rustic_repo_location("rest:http://host/"), None);
    }

    #[test]
    fn rustic_join_never_restarts_the_path() {
        // `PathBuf::join("D:x")` would replace everything with `D:x`.
        let snap = NavPath::rustic(r"D:\r", SNAP, "");
        let p = snap.join("D:x");
        assert_eq!(p.rustic_parts().unwrap().sub, "D:x");
    }

    #[test]
    fn rustic_display_round_trips_through_new() {
        for p in [
            NavPath::rustic_repo(r"D:\backups\repo"),
            NavPath::rustic(r"D:\backups\repo", SNAP, ""),
            NavPath::rustic(r"D:\backups\repo", SNAP, "C/Users/with space"),
            NavPath::rustic_repo("rclone:mac:repo"),
        ] {
            let shown = p.rustic_display().unwrap();
            assert_eq!(NavPath::new(&shown).unwrap(), p, "{shown}");
        }
        assert_eq!(
            NavPath::rustic(r"D:\r", SNAP, "C/Users")
                .rustic_display()
                .unwrap(),
            format!(r"rustic:D:\r › {SNAP} › C\Users")
        );
    }

    #[test]
    fn rustic_prefix_does_not_steal_an_rclone_remote_named_rustic() {
        let p = NavPath::new("rustic:photos").unwrap();
        assert!(p.is_remote());
        assert_eq!(p.rclone_arg().as_deref(), Some("rustic:photos"));
    }

    #[test]
    fn rustic_local_repo_spelling_is_normalised() {
        // rustic parses `D:/x` as a backend called `D`.
        assert_eq!(
            normalize_rustic_repo("D:/backups/repo/"),
            r"D:\backups\repo"
        );
        assert_eq!(normalize_rustic_repo(r"D:\"), r"D:\");
        assert_eq!(normalize_rustic_repo("//nas/share/r"), r"\\nas\share\r");
        assert_eq!(normalize_rustic_repo("rclone:mac:a/b"), "rclone:mac:a/b");
        let p = NavPath::new("rustic:D:/backups/repo").unwrap();
        assert_eq!(p.rustic_parts().unwrap().repo, r"D:\backups\repo");
    }

    #[test]
    fn remote_abs_parent_chain() {
        let p = NavPath::new("mac:/users/me").unwrap();
        let parent = p.parent().unwrap();
        assert!(parent.is_remote_abs());
        assert_eq!(parent.rclone_arg().as_deref(), Some("mac:/users"));
        let parent = parent.parent().unwrap();
        // mac:/users → mac:/  (absolute root, still meaningful).
        assert!(parent.is_remote_abs());
        assert!(parent.is_remote_root());
        assert_eq!(parent.rclone_arg().as_deref(), Some("mac:/"));
        // mac:/ → Remotes virtual listing.
        let parent = parent.parent().unwrap();
        assert!(parent.is_remotes_root());
    }
}
