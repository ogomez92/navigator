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

impl NavPath {
    pub fn new(p: impl Into<PathBuf>) -> crate::Result<Self> {
        let p = p.into();
        if p.is_absolute() {
            return Ok(Self(p));
        }
        let s = p.to_string_lossy();
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
