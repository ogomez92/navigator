//! Logic for user-defined shortcut actions, split out so it can be unit
//! tested without a GUI. The runtime wires spawn via [`spawn_action`]; the
//! placeholder substitution sits in [`expand_args`] so tests can hit it
//! directly.

use std::path::Path;

use navigator_config::ShortcutAction;
use navigator_core::NavPath;

/// Substitute placeholders in `template` using `target`.
///
///   `{path}`    full path of `target`
///   `{folder}`  directory the user likely wants as CWD: `target` itself if
///               it is a directory, otherwise its parent
///   `{parent}`  parent directory of `target` (always)
///   `{name}`    filename component of `target`
///
/// `is_dir` is passed explicitly so tests don't need a real filesystem.
pub fn expand(template: &str, target: &Path, is_dir: bool) -> String {
    let parent = target.parent().unwrap_or(target);
    let folder = if is_dir { target } else { parent };
    let name = target.file_name().and_then(|s| s.to_str()).unwrap_or("");
    template
        .replace("{path}", &target.to_string_lossy())
        .replace("{folder}", &folder.to_string_lossy())
        .replace("{parent}", &parent.to_string_lossy())
        .replace("{name}", name)
}

/// Expand every arg in `action.args` against `target`. Returns owned
/// strings so the caller can push them into `std::process::Command`.
pub fn expand_args(action: &ShortcutAction, target: &Path, is_dir: bool) -> Vec<String> {
    action
        .args
        .iter()
        .map(|a| expand(a, target, is_dir))
        .collect()
}

/// Launch the configured program. Probes `target` with `Path::is_dir` to
/// pick the `{folder}` substitution. Returns any I/O error from `spawn`.
pub fn spawn_action(action: &ShortcutAction, target: &NavPath) -> std::io::Result<()> {
    let p = target.as_path();
    let is_dir = p.is_dir();
    let args = expand_args(action, p, is_dir);
    std::process::Command::new(&action.command)
        .args(&args)
        .spawn()
        .map(|_| ())
}
