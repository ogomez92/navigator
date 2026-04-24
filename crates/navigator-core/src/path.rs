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

impl NavPath {
    pub fn new(p: impl Into<PathBuf>) -> crate::Result<Self> {
        let p = p.into();
        if p.is_absolute() {
            return Ok(Self(p));
        }
        // UNC share root without a trailing separator — `\\host\share` or
        // `//host/share` — is NOT `is_absolute()` in Rust's book because it
        // has a prefix but no root component. Retry with a trailing `\`,
        // which makes the same path absolute. Users typing an IP-based
        // share (e.g. `\\100.86.173.34\media`) hit this constantly.
        let s = p.to_string_lossy();
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
            let trimmed: String = s.chars()
                .map(|c| if c == '/' { '\\' } else { c })
                .collect();
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
        if !(s.starts_with(r"\\") || s.starts_with("//")) { return false; }
        let body: String = s.chars()
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
        if !self.is_unc_host_only() { return None; }
        let s = self.0.to_string_lossy();
        let body: String = s.chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .collect::<String>()
            .trim_start_matches('\\')
            .trim_end_matches('\\')
            .to_string();
        Some(body)
    }

    /// Virtual root showing all connected drives + network locations.
    pub fn this_pc() -> Self { Self(PathBuf::from(THIS_PC_SENTINEL)) }

    pub fn is_this_pc(&self) -> bool {
        self.0.as_os_str() == THIS_PC_SENTINEL
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

    pub fn as_path(&self) -> &Path { &self.0 }
    pub fn into_inner(self) -> PathBuf { self.0 }

    /// Parent folder, or `None` if already at a filesystem root (or at the
    /// This PC virtual root). The caller decides whether to treat `None`
    /// as "stop" or as "go to This PC".
    pub fn parent(&self) -> Option<Self> {
        if self.is_this_pc() { return None; }
        self.0.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| Self(p.to_path_buf()))
    }

    /// Join a child name (no separators in `name`).
    pub fn join(&self, name: &str) -> Self {
        Self(self.0.join(name))
    }

    pub fn file_name(&self) -> &str {
        self.0.file_name().and_then(|s| s.to_str()).unwrap_or_default()
    }
}

impl AsRef<Path> for NavPath {
    fn as_ref(&self) -> &Path { &self.0 }
}

impl std::fmt::Display for NavPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
