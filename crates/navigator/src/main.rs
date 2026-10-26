//! Binary entry point.
//!
//! Intentionally thin — configuration parsing, tracing setup, then hand off
//! to `navigator_gui::run`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

use navigator_core::NavPath;
use navigator_gui::{AppConfig, run};

fn main() -> anyhow_lite::Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--shell-op` re-executions are not file explorers: navigator spawns
    // itself this way so a Windows shell copy/move outlives the window
    // that started it. Checked before anything else so no config is read
    // and no HWND is ever created for the helper.
    if let Some(code) = navigator_gui::try_run_shell_op(&args) {
        std::process::exit(code);
    }
    if let Some(code) = navigator_gui::try_run_drive_users_helper(&args) {
        std::process::exit(code);
    }

    let initial_path = match parse_args(&args) {
        ParsedArgs::Path(p) => p,
        ParsedArgs::Help => {
            print_usage();
            return Ok(());
        }
        ParsedArgs::BadArg(msg) => {
            eprintln!("navigator: {}\n", msg);
            print_usage();
            std::process::exit(2);
        }
    };

    let mut cfg = AppConfig::with_defaults();
    cfg.initial_path = initial_path;
    let rc = run(cfg).map_err(|e| anyhow_lite::anyhow(e.to_string()))?;
    std::process::exit(rc);
}

enum ParsedArgs {
    Path(NavPath),
    Help,
    BadArg(String),
}

/// Parse CLI args. Recognised forms:
///   navigator                               — default drive root
///   navigator <path>                        — absolute/relative local path or `remote:sub`
///   navigator -r <remote[:sub]>             — explicit remote (name-only OK)
///   navigator --remote <remote[:sub]>       — long form of -r
///   navigator --rustic <repo>               — browse a rustic backup repository
///   navigator -h | --help                   — usage
/// A bare `mac:downloads` works without `-r` because `NavPath::new` already
/// accepts rclone syntax; `-r` is just a discoverable shortcut and also
/// lets the user pass a bare remote name (`-r mac` → `mac:`).
fn parse_args(args: &[String]) -> ParsedArgs {
    let Some(first) = args.first() else {
        return ParsedArgs::Path(NavPath::default_root());
    };
    match first.as_str() {
        "-h" | "--help" => ParsedArgs::Help,
        "-r" | "--remote" => {
            let Some(v) = args.get(1) else {
                return ParsedArgs::BadArg(format!("{} needs a remote argument", first));
            };
            // Bare `mac` becomes `mac:` so `-r mac` drops at the remote
            // root; `mac:sub/path` already parses correctly.
            let spec = if v.contains(':') {
                v.clone()
            } else {
                format!("{}:", v)
            };
            match NavPath::new(PathBuf::from(&spec)) {
                Ok(p) => ParsedArgs::Path(p),
                Err(e) => ParsedArgs::BadArg(format!("invalid remote {:?}: {}", spec, e)),
            }
        }
        "--rustic" => match args.get(1) {
            Some(v) => ParsedArgs::Path(NavPath::rustic_repo(&rustic_repo_arg(v))),
            None => ParsedArgs::BadArg("--rustic needs a repository argument".into()),
        },
        _ => match parse_path_arg(first) {
            Ok(p) => ParsedArgs::Path(p),
            Err(_) => ParsedArgs::Path(NavPath::default_root()),
        },
    }
}

fn parse_path_arg(raw: &str) -> navigator_core::Result<NavPath> {
    let path = PathBuf::from(raw);
    match NavPath::new(path.clone()) {
        Ok(p) => Ok(p),
        Err(_) if should_resolve_relative(raw, &path) => {
            let abs = std::path::absolute(&path)
                .map_err(|source| navigator_core::Error::io(path.clone(), source))?;
            NavPath::new(abs)
        }
        Err(e) => Err(e),
    }
}

/// The repository `--rustic` names, spelled the way rustic takes `-r`.
///
/// Deliberately not checked: whether it really is a repository is found
/// out by the first listing, which has to open it anyway — probing here
/// would only add a startup delay to say the same thing. A local folder,
/// relative or absolute, becomes an absolute backslashed path (rustic
/// reads `D:/x` as a backend called `D`); a backend spec such as
/// `rclone:remote:path` or `sftp:host:/repo` passes through untouched.
fn rustic_repo_arg(raw: &str) -> String {
    let b = raw.as_bytes();
    let is_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    if !is_drive && raw.contains(':') {
        return raw.to_string();
    }
    let abs = std::path::absolute(raw)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string());
    navigator_core::path::normalize_rustic_repo(&abs)
}

fn should_resolve_relative(raw: &str, path: &std::path::Path) -> bool {
    !path.is_absolute() && !raw.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn remote_of(p: &ParsedArgs) -> Option<String> {
        match p {
            ParsedArgs::Path(np) => np.rclone_arg(),
            _ => None,
        }
    }

    #[test]
    fn no_args_is_default_root() {
        let r = parse_args(&[]);
        let ParsedArgs::Path(p) = r else {
            panic!("expected Path")
        };
        assert_eq!(p, NavPath::default_root());
    }

    #[test]
    fn help_short_and_long() {
        assert!(matches!(parse_args(&strings(&["-h"])), ParsedArgs::Help));
        assert!(matches!(
            parse_args(&strings(&["--help"])),
            ParsedArgs::Help
        ));
    }

    #[test]
    fn remote_flag_with_subpath() {
        let r = parse_args(&strings(&["-r", "mac:Downloads/incoming"]));
        assert_eq!(remote_of(&r).as_deref(), Some("mac:Downloads/incoming"));
    }

    #[test]
    fn remote_flag_preserves_leading_slash() {
        // `-r mac:/users` is filesystem-absolute on sftp/local backends;
        // dropping the slash silently changes which directory rclone hits.
        let r = parse_args(&strings(&["-r", "mac:/users"]));
        assert_eq!(remote_of(&r).as_deref(), Some("mac:/users"));
    }

    #[test]
    fn remote_flag_bare_name_becomes_root() {
        let r = parse_args(&strings(&["--remote", "gdrive"]));
        assert_eq!(remote_of(&r).as_deref(), Some("gdrive:"));
    }

    #[test]
    fn remote_flag_missing_value_is_error() {
        assert!(matches!(
            parse_args(&strings(&["-r"])),
            ParsedArgs::BadArg(_)
        ));
    }

    #[test]
    fn bare_remote_syntax_works_without_flag() {
        let r = parse_args(&strings(&["mac:Downloads"]));
        assert_eq!(remote_of(&r).as_deref(), Some("mac:Downloads"));
    }

    #[test]
    fn bare_absolute_local_path() {
        let r = parse_args(&strings(&[r"C:\Users"]));
        let ParsedArgs::Path(p) = r else {
            panic!("expected Path")
        };
        assert!(!p.is_remote());
    }

    #[test]
    fn relative_dot_resolves_from_current_dir() {
        let r = parse_args(&strings(&["."]));
        let ParsedArgs::Path(p) = r else {
            panic!("expected Path")
        };
        assert_eq!(p.as_path(), std::path::absolute(".").unwrap());
    }

    #[test]
    fn relative_child_resolves_from_current_dir() {
        let r = parse_args(&strings(&["src"]));
        let ParsedArgs::Path(p) = r else {
            panic!("expected Path")
        };
        assert_eq!(p.as_path(), std::path::absolute("src").unwrap());
    }

    fn rustic_repo_of(p: &ParsedArgs) -> Option<String> {
        match p {
            ParsedArgs::Path(np) if np.is_rustic_repo_root() => np.rustic_parts().map(|r| r.repo),
            _ => None,
        }
    }

    #[test]
    fn rustic_flag_opens_the_snapshot_list() {
        let r = parse_args(&strings(&["--rustic", r"D:\backups\repo"]));
        assert_eq!(rustic_repo_of(&r).as_deref(), Some(r"D:\backups\repo"));
        // Forward slashes and a trailing separator are respelled for rustic.
        let r = parse_args(&strings(&["--rustic", "D:/backups/repo/"]));
        assert_eq!(rustic_repo_of(&r).as_deref(), Some(r"D:\backups\repo"));
        // Relative to the current directory, like a plain path argument.
        let r = parse_args(&strings(&["--rustic", "repo"]));
        let want = std::path::absolute("repo").unwrap();
        assert_eq!(rustic_repo_of(&r).as_deref(), want.to_str());
        // A backend spec passes through.
        let r = parse_args(&strings(&["--rustic", "rclone:nas:backups"]));
        assert_eq!(rustic_repo_of(&r).as_deref(), Some("rclone:nas:backups"));
        assert!(matches!(parse_args(&strings(&["--rustic"])), ParsedArgs::BadArg(_)));
    }

    #[test]
    fn drive_relative_path_is_not_treated_as_cli_relative() {
        let r = parse_args(&strings(&["C:foo"]));
        let ParsedArgs::Path(p) = r else {
            panic!("expected Path")
        };
        assert_eq!(p, NavPath::default_root());
    }
}

fn print_usage() {
    eprintln!(
        "navigator — accessible Windows file explorer\n\
         \n\
         USAGE:\n    \
            navigator [OPTIONS] [PATH]\n\
         \n\
         ARGS:\n    \
            <PATH>                Local path (C:\\foo, .) or rclone remote (mac:downloads)\n\
         \n\
         OPTIONS:\n    \
            -r, --remote <SPEC>   Open an rclone remote. SPEC is `name` or `name:sub/path`\n    \
            --rustic <REPO>       Browse a rustic backup repository (read-only; copy to restore)\n    \
            -h, --help            Print this help\n\
         \n\
         EXAMPLES:\n    \
            navigator\n    \
            navigator .\n    \
            navigator C:\\Users\\me\\Downloads\n    \
            navigator -r mac:Downloads/incoming\n    \
            navigator -r gdrive\n    \
            navigator mac:Downloads/incoming\n    \
            navigator --rustic D:\\backups\\repo\n"
    );
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_env("NAVIGATOR_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Zero-dep error propagation so we don't pull `anyhow` just for `main`.
mod anyhow_lite {
    #[derive(Debug)]
    pub struct Error(pub String);
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Error {}
    pub type Result<T> = std::result::Result<T, Error>;
    pub fn anyhow(s: impl Into<String>) -> Error {
        Error(s.into())
    }
}
