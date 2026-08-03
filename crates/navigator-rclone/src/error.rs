//! Turning rclone's JSON log into one sentence a person can act on.
//!
//! A failed operation is not one error record, it is a *stream* of them,
//! and the sentence the user needs is buried. Deleting a file that isn't
//! there produces five JSON objects, each carrying a timestamp, a Go
//! source location and an `objectType`, three of them the same retry
//! repeated:
//!
//! ```text
//! {"time":"…","level":"error","msg":"Attempt 1/3 failed with 1 errors and: C:/x/nope.txt is a directory or doesn't exist: object not found","source":"slog/logger.go:256"}
//! {"time":"…","level":"error","msg":"Attempt 2/3 failed with 1 errors and: …"}
//! {"time":"…","level":"error","msg":"Attempt 3/3 failed with 1 errors and: …"}
//! {"time":"…","level":"notice","msg":"0 B / 0 B, -, 0 B/s, ETA -","stats":{…,"lastError":"…"}}
//! {"time":"…","level":"notice","msg":"Failed to deletefile: C:/x/nope.txt is a directory or doesn't exist: object not found"}
//! ```
//!
//! Handing the tail of that to a dialog — which is what we used to do —
//! shows the user a wall of JSON with the answer at the far end of the
//! last line, and reads aloud to a screen reader as a timestamp followed
//! by a Go file name. [`ErrorCollector`] watches the stream instead and
//! [`ErrorCollector::finish`] distils it to a [`RcloneError`]: the real
//! message with the wrappers peeled off, a [`ErrorKind`] the UI can phrase
//! in its own words, and the supporting lines kept separately for the
//! details pane.
//!
//! Three facts about the stream drive the design, all pinned by tests
//! against verbatim rclone 1.73.5 output:
//!
//!   * **The summary line is `notice`, not `error`.** Filtering on level
//!     alone throws away the one record that names the operation.
//!   * **Retries repeat verbatim.** `--retries 3` means the same sentence
//!     three times; deduping is what keeps the details pane readable.
//!   * **A configuration failure is `critical` and never retried**, so it
//!     has no `Failed to …` summary at all — the `critical` line itself is
//!     the whole report.

use crate::log::{LogEvent, LogLevel};

/// What went wrong, in terms the UI can phrase itself.
///
/// Deliberately coarse: it exists so the app can say "not found" instead
/// of `object not found` / `directory not found` / `The system cannot find
/// the file specified.` — three spellings of one fact, depending on
/// backend and verb. The verbatim rclone text is always kept alongside in
/// [`RcloneError::message`] for anyone who wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    NotEmpty,
    AlreadyExists,
    /// The destination path exists as a file where a directory was needed
    /// (or the reverse) — a wrong-verb / wrong-target error, not a
    /// missing one.
    WrongKind,
    /// Named a remote that isn't in `rclone.conf`.
    NoSuchRemote,
    OutOfSpace,
    /// Refused by the backend: bad or expired credentials, forbidden.
    Auth,
    /// Couldn't reach the backend at all.
    Network,
    /// rclone rejected the command line. Ours to fix, not the user's.
    Usage,
    Other,
}

impl ErrorKind {
    /// Short phrase for speech and dialog headlines. Lowercase so it can
    /// be dropped into a larger sentence.
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "not found",
            ErrorKind::PermissionDenied => "permission denied",
            ErrorKind::NotEmpty => "folder is not empty",
            ErrorKind::AlreadyExists => "already exists",
            ErrorKind::WrongKind => "wrong kind of target",
            ErrorKind::NoSuchRemote => "no such remote",
            ErrorKind::OutOfSpace => "not enough space",
            ErrorKind::Auth => "access refused",
            ErrorKind::Network => "network error",
            ErrorKind::Usage => "bad rclone command",
            ErrorKind::Other => "failed",
        }
    }
}

/// One rclone invocation's failure, distilled.
#[derive(Debug, Clone)]
pub struct RcloneError {
    pub kind: ErrorKind,
    /// The real error text with rclone's wrappers removed — no timestamp,
    /// no `Attempt 2/3 failed with 1 errors and:`, no `\\?\` path prefix.
    /// Empty when rclone died without saying anything (killed, or a crash
    /// before it logged), in which case [`Self::summary`] falls back to
    /// the exit code.
    pub message: String,
    /// Path or remote rclone blamed, when it named one *usefully*. Go `Fs`
    /// descriptions (`Local file system at //?/C:/…`) are dropped — see
    /// [`useful_object`].
    pub object: Option<String>,
    /// Process exit code. rclone documents these, and 3 / 4 (directory /
    /// file not found) classify failures whose text we don't recognise.
    pub exit_code: Option<i32>,
    /// Every distinct error line, in order, for a details pane. Deduped,
    /// so three identical retries appear once, and capped at
    /// [`MAX_DETAIL_LINES`].
    pub detail: Vec<String>,
}

/// Distinct error lines kept for the details pane. A `sync` of a large
/// tree against a dead backend can log an error per object; the first
/// handful explain it and the rest are the same sentence with different
/// paths.
pub const MAX_DETAIL_LINES: usize = 20;

impl RcloneError {
    /// What went wrong, with no mention of *what it happened to*. Use
    /// this when the caller names the item itself — it knows the file the
    /// user selected, where this type knows a `\\?\` absolute path.
    ///
    /// [`ErrorKind::Other`] means we didn't recognise the text, so the
    /// text itself is all we have to offer — better a raw rclone sentence
    /// than the useless word "failed".
    pub fn reason(&self) -> String {
        if self.kind != ErrorKind::Other {
            return self.kind.label().to_string();
        }
        if !self.message.is_empty() {
            return self.message.clone();
        }
        match self.exit_code {
            Some(c) => format!("rclone exited with code {c}"),
            None => "rclone failed".into(),
        }
    }

    /// One short line naming what went wrong and, when rclone named one
    /// usefully, what to. For callers with nothing better to say — the
    /// ones that speak a failure on their own rather than feeding a job
    /// report.
    pub fn summary(&self) -> String {
        match self
            .object
            .as_deref()
            .filter(|_| self.kind != ErrorKind::Other)
        {
            Some(o) => format!("{}: {}", self.reason(), o),
            None => self.reason(),
        }
    }

    /// Everything worth logging, on one line. Used for the `tracing`
    /// record — the details pane gets [`Self::detail`] unjoined.
    pub fn log_line(&self) -> String {
        let mut s = format!("{:?}", self.kind);
        if let Some(c) = self.exit_code {
            s.push_str(&format!(" (exit {c})"));
        }
        if !self.message.is_empty() {
            s.push_str(": ");
            s.push_str(&self.message);
        }
        for extra in self.detail.iter().filter(|d| **d != self.message) {
            s.push_str(" | ");
            s.push_str(extra);
        }
        s
    }

    /// Distil a whole log *file* — the elevated retry path can't pipe
    /// stdout, so it redirects rclone to `--log-file` and reads the text
    /// afterwards. Same distillation as the streaming path, so an
    /// elevated failure reads identically to an ordinary one.
    pub fn from_log_text(text: &str, exit_code: Option<i32>) -> Self {
        let mut c = ErrorCollector::default();
        for line in text.lines() {
            c.observe_line(line);
        }
        c.finish(exit_code)
    }
}

/// Watches an operation's log stream and remembers only what a failure
/// report would need. Fed by both reader threads, so it is cheap to
/// update and holds nothing per successful transfer.
#[derive(Debug, Default)]
pub struct ErrorCollector {
    lines: Vec<Entry>,
    /// Latest end-of-run verdict, held apart from `lines` so the capacity
    /// cap can never evict it. Exempting verdicts from the cap instead
    /// would leave the cap unenforced: rclone logs a per-object failure
    /// as `Failed to copy: …`, which is verdict-shaped, so a storm of
    /// them is *all* verdicts.
    verdict: Option<Entry>,
    /// `stats.lastError` from the closing stats tick — rclone's own
    /// summary of the run, and the fallback when no verdict survived.
    last_error: Option<String>,
    /// Distinct lines seen but not kept, so the details pane can say how
    /// much it is hiding rather than trailing off.
    dropped: usize,
}

#[derive(Debug)]
struct Entry {
    text: String,
    object: Option<String>,
    /// rclone's own end-of-run verdict (`Failed to copyto: …`, or any
    /// `critical` record). Preferred over the per-attempt noise, because
    /// it is the line that survived all the retries.
    verdict: bool,
}

impl ErrorCollector {
    /// Feed one parsed log record. Records that aren't errors cost a
    /// couple of comparisons and are dropped.
    pub fn observe(&mut self, ev: &LogEvent) {
        if let Some(stats) = ev.stats.as_ref() {
            // A stats tick is never itself an error, but the closing one
            // carries rclone's last error verbatim.
            if let Some(e) = stats.lastError.as_deref().filter(|e| !e.is_empty()) {
                self.last_error = Some(clean(e).0);
            }
            return;
        }
        let level = ev.level.unwrap_or(LogLevel::Info);
        let verdict = matches!(level, LogLevel::Critical) || ev.msg.starts_with("Failed to ");
        // The end-of-run summary is logged at NOTICE, so level alone is
        // not enough to find the one line that matters.
        if !verdict && !matches!(level, LogLevel::Error) {
            return;
        }
        let (text, obj) = clean(&ev.msg);
        let object = obj.or_else(|| useful_object(ev.object.as_deref(), ev.object_type.as_deref()));
        self.push(Entry {
            text,
            object,
            verdict,
        });
    }

    /// Feed one raw output line — JSON when it parses, otherwise the line
    /// itself. rclone emits plain text for a few startup failures (bad
    /// flags) before the JSON logger is installed, and those are exactly
    /// the failures we most need to show.
    pub fn observe_line(&mut self, line: &str) {
        match serde_json::from_str::<LogEvent>(line) {
            Ok(ev) => self.observe(&ev),
            Err(_) => self.observe_raw(line),
        }
    }

    /// Feed a non-JSON output line.
    pub fn observe_raw(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let (text, object) = clean(line);
        self.push(Entry {
            text,
            object,
            verdict: false,
        });
    }

    fn push(&mut self, e: Entry) {
        if e.text.is_empty() {
            return;
        }
        if e.verdict {
            self.verdict = Some(Entry {
                text: e.text.clone(),
                object: e.object.clone(),
                verdict: true,
            });
        }
        if self.lines.iter().any(|k| k.text == e.text) {
            // Retries repeat the sentence verbatim; keep one.
            return;
        }
        if self.lines.len() >= MAX_DETAIL_LINES {
            self.dropped += 1;
            return;
        }
        self.lines.push(e);
    }

    /// `true` when nothing error-shaped has been seen. Lets a caller skip
    /// building a report for an operation that failed silently.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.verdict.is_none() && self.last_error.is_none()
    }

    /// Distil everything seen into one error. Call only for a run that
    /// actually failed — a successful op logs no errors, so the result
    /// would be an empty [`ErrorKind::Other`].
    ///
    /// The primary message is chosen in order of how much rclone had
    /// figured out when it wrote the line: its end-of-run verdict, then
    /// its own `lastError`, then any error line that isn't a per-attempt
    /// repeat, then whatever is left.
    pub fn finish(&self, exit_code: Option<i32>) -> RcloneError {
        let (message, object) = if let Some(v) = self.verdict.as_ref() {
            (v.text.clone(), v.object.clone())
        } else if let Some(last) = self.last_error.clone() {
            (last, None)
        } else if let Some(e) = self.lines.last() {
            (e.text.clone(), e.object.clone())
        } else {
            (String::new(), None)
        };
        let mut detail: Vec<String> = self.lines.iter().map(|e| e.text.clone()).collect();
        if self.dropped > 0 {
            detail.push(format!("… and {} more", self.dropped));
        }
        RcloneError {
            kind: classify(&message, exit_code),
            message,
            object,
            exit_code,
            detail,
        }
    }
}

/// Peel rclone's wrappers off a message and normalise the paths inside
/// it. Returns the core sentence plus any path rclone named in a wrapper
/// it built (`Failed to create file system for "X": …`).
fn clean(msg: &str) -> (String, Option<String>) {
    let mut msg = msg.trim();
    let mut object: Option<&str> = None;
    // Wrappers nest — a `Failed to …` verdict can wrap a `Failed to
    // create file system …` cause — so keep peeling. Four passes is well
    // past anything rclone emits and cannot loop.
    for _ in 0..4 {
        match strip_once(msg) {
            Some((rest, obj)) => {
                msg = rest.trim();
                object = obj.or(object);
            }
            None => break,
        }
    }
    (readable_paths(msg), object.map(readable_paths))
}

/// Remove one layer of rclone wrapper. `None` when `msg` is already the
/// real thing.
fn strip_once(msg: &str) -> Option<(&str, Option<&str>)> {
    // "Attempt 2/3 failed with 1 errors and: <real error>"
    if let Some(rest) = msg.strip_prefix("Attempt ")
        && let Some(i) = rest.find(" and: ")
        && rest[..i].contains(" failed with ")
    {
        return Some((&rest[i + " and: ".len()..], None));
    }

    let after = msg.strip_prefix("Failed to ")?;

    // "Failed to create file system for destination \"X\": <cause>"
    for lead in [
        "create file system for destination ",
        "create file system for source ",
        "create file system for ",
        "list ",
    ] {
        if let Some(rest) = after.strip_prefix(lead)
            && let Some((obj, cause)) = split_quoted(rest)
        {
            return Some((cause, Some(obj).filter(|o| !o.is_empty())));
        }
    }

    let (head, tail) = after.split_once(": ")?;
    // "Failed to deletefile: <cause>" — a bare verb, so the rest is the
    // real error.
    if !head.contains(' ') {
        return Some((tail, None));
    }
    // "Failed to purge with 4 errors: last error was: <cause>"
    if head.contains(" with ")
        && let Some(rest) = tail.strip_prefix("last error was: ")
    {
        return Some((rest, None));
    }
    None
}

/// Split `"quoted": rest` into the quoted part and the rest.
fn split_quoted(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('"')?;
    let end = rest.find("\": ")?;
    Some((&rest[..end], &rest[end + 3..]))
}

/// Drop the Win32 `\\?\` long-path prefix rclone shows in raw OS errors.
/// It is noise in a dialog and, read aloud, four punctuation marks before
/// the user hears a single letter of the path.
fn readable_paths(s: &str) -> String {
    s.replace("\\\\?\\", "").replace("//?/", "")
}

/// `object` is only worth showing when it names a path. rclone also uses
/// the field for Go `Fs` descriptions ("Local file system at //?/C:/x")
/// and for the empty-string root, neither of which means anything to a
/// user looking at a file manager.
fn useful_object(object: Option<&str>, object_type: Option<&str>) -> Option<String> {
    let o = object?.trim();
    if o.is_empty() || o.starts_with("Local file system at ") {
        return None;
    }
    if object_type.is_some_and(|t| t.ends_with(".Fs") || t == "string") {
        return None;
    }
    Some(readable_paths(o))
}

/// Classify a distilled message. Substring matching over rclone's and
/// Windows' own wording, with the exit code as the fallback: rclone
/// documents 3 as "directory not found" and 4 as "file not found", which
/// covers backends whose phrasing we've never seen.
fn classify(message: &str, exit_code: Option<i32>) -> ErrorKind {
    let m = message.to_ascii_lowercase();
    let has = |needle: &str| m.contains(needle);

    // Order matters: "is a file not a directory" also contains "not a
    // directory", and a full disk on Windows reports "There is not enough
    // space on the disk" which contains neither "quota" nor "no space".
    if has("is a file not a directory")
        || has("is a directory not a file")
        || has("destination is a file")
        || has("not a directory")
    {
        return ErrorKind::WrongKind;
    }
    if has("didn't find section in config file") || has("unknown remote") {
        return ErrorKind::NoSuchRemote;
    }
    if has("access is denied")
        || has("permission denied")
        || has("access denied")
        || has("operation not permitted")
    {
        return ErrorKind::PermissionDenied;
    }
    if has("not enough space") || has("no space left") || has("quota") || has("disk full") {
        return ErrorKind::OutOfSpace;
    }
    if has("directory not empty") || has("not empty") {
        return ErrorKind::NotEmpty;
    }
    if has("already exists") || has("file exists") {
        return ErrorKind::AlreadyExists;
    }
    if has("not found")
        || has("no such file")
        || has("cannot find the file")
        || has("cannot find the path")
        || has("doesn't exist")
        || has("does not exist")
    {
        return ErrorKind::NotFound;
    }
    if has("unauthorized")
        || has("401")
        || has("403")
        || has("forbidden")
        || has("invalid credentials")
        || has("authentication")
        || has("token expired")
    {
        return ErrorKind::Auth;
    }
    if has("no such host")
        || has("connection refused")
        || has("connection reset")
        || has("i/o timeout")
        || has("network is unreachable")
        || has("timeout awaiting")
        || has("tls handshake")
    {
        return ErrorKind::Network;
    }
    match exit_code {
        // rclone's documented exit codes. 3 and 4 are the two that say
        // something we can't always read out of the text.
        Some(3) | Some(4) => ErrorKind::NotFound,
        Some(5) => ErrorKind::Network,
        _ => ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a collector verbatim rclone output, one line per element.
    fn distil(lines: &[&str], exit: i32) -> RcloneError {
        let mut c = ErrorCollector::default();
        for l in lines {
            c.observe_line(l);
        }
        c.finish(Some(exit))
    }

    /// Verbatim rclone 1.73.5 output for `deletefile` on a path that
    /// isn't there — the case that motivated all of this. Five records,
    /// three of them the same retry, and the answer is the last eight
    /// words of the last one.
    #[test]
    fn deleting_a_missing_file_distils_to_not_found() {
        let e = distil(
            &[
                r#"{"time":"2026-08-03T12:30:37.6417213+02:00","level":"error","msg":"Attempt 1/3 failed with 1 errors and: C:/x/nope.txt is a directory or doesn't exist: object not found","source":"slog/logger.go:256"}"#,
                r#"{"time":"2026-08-03T12:30:37.6670845+02:00","level":"error","msg":"Attempt 2/3 failed with 1 errors and: C:/x/nope.txt is a directory or doesn't exist: object not found","source":"slog/logger.go:256"}"#,
                r#"{"time":"2026-08-03T12:30:37.6670845+02:00","level":"error","msg":"Attempt 3/3 failed with 1 errors and: C:/x/nope.txt is a directory or doesn't exist: object not found","source":"slog/logger.go:256"}"#,
                r#"{"time":"2026-08-03T12:30:37.6670845+02:00","level":"notice","msg":"          0 B / 0 B, -, 0 B/s, ETA -\n","stats":{"bytes":0,"errors":1,"lastError":"C:/x/nope.txt is a directory or doesn't exist: object not found","retryError":true},"source":"slog/logger.go:256"}"#,
                r#"{"time":"2026-08-03T12:30:37.6670845+02:00","level":"notice","msg":"Failed to deletefile: C:/x/nope.txt is a directory or doesn't exist: object not found","source":"slog/logger.go:256"}"#,
            ],
            4,
        );
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(
            e.message,
            "C:/x/nope.txt is a directory or doesn't exist: object not found"
        );
        assert_eq!(e.summary(), "not found");
        // Three identical retries plus the verdict collapse to one line.
        assert_eq!(e.detail.len(), 1, "retries must dedupe: {:?}", e.detail);
    }

    /// Verbatim output for `copyto` from a source that doesn't exist.
    /// Two distinct sentences here, and the useful one is the verdict —
    /// picking "last line wins" would work by luck, picking "first error
    /// wins" would report the Go-level `error reading source root
    /// directory` instead.
    #[test]
    fn copying_a_missing_source_reports_the_verdict_not_the_first_error() {
        let e = distil(
            &[
                r#"{"time":"2026-08-03T12:30:46.3896623+02:00","level":"error","msg":"error reading source root directory: directory not found","object":"Local file system at //?/C:/x/nope.txt","objectType":"*local.Fs","source":"slog/logger.go:256"}"#,
                r#"{"time":"2026-08-03T12:30:46.4138233+02:00","level":"error","msg":"Attempt 1/3 failed with 1 errors and: directory not found","source":"slog/logger.go:256"}"#,
                r#"{"time":"2026-08-03T12:30:46.4148516+02:00","level":"notice","msg":"Failed to copyto: directory not found","source":"slog/logger.go:256"}"#,
            ],
            3,
        );
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(e.message, "directory not found");
        // The Go `Fs` description is not a path the user can look at.
        assert_eq!(e.object, None);
        assert_eq!(
            e.detail,
            vec![
                "error reading source root directory: directory not found",
                "directory not found",
            ]
        );
    }

    /// Verbatim `purge` of a missing directory: four errors per attempt,
    /// twelve records in all, and a verdict in rclone's other summary
    /// spelling — `with N errors: last error was:`.
    #[test]
    fn purge_verdict_with_an_error_count_is_unwrapped() {
        let e = distil(
            &[
                r#"{"level":"error","msg":"error listing: directory not found","object":"","objectType":"string"}"#,
                r#"{"level":"error","msg":"Failed to list \"\": directory not found","object":"Local file system at //?/C:/x/nodir","objectType":"*local.Fs"}"#,
                r#"{"level":"error","msg":"Failed to rmdir: GetFileAttributesEx \\\\?\\C:\\x\\nodir: The system cannot find the file specified.","object":"","objectType":"string"}"#,
                r#"{"level":"notice","msg":"Failed to purge with 4 errors: last error was: failed to remove directories: GetFileAttributesEx \\\\?\\C:\\x\\nodir: The system cannot find the file specified."}"#,
            ],
            1,
        );
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(
            e.message,
            "failed to remove directories: GetFileAttributesEx C:\\x\\nodir: The system cannot find the file specified."
        );
        assert_eq!(e.summary(), "not found");
        // `\\?\` is gone from the details too.
        assert!(
            !e.detail.iter().any(|d| d.contains("\\\\?\\")),
            "{:?}",
            e.detail
        );
    }

    /// A misconfigured remote fails *before* any retry, so there is no
    /// `Failed to …` verdict at all — one `critical` record is the whole
    /// report, and level alone has to be enough to keep it.
    #[test]
    fn an_unknown_remote_is_recognised_from_a_lone_critical_record() {
        let e = distil(
            &[
                r#"{"time":"2026-08-03T12:30:49.2947248+02:00","level":"critical","msg":"Failed to create file system for destination \"nosuchremote:x/y.txt\": didn't find section in config file (\"nosuchremote\")","source":"slog/logger.go:256"}"#,
            ],
            1,
        );
        assert_eq!(e.kind, ErrorKind::NoSuchRemote);
        assert_eq!(
            e.message,
            "didn't find section in config file (\"nosuchremote\")"
        );
        assert_eq!(e.object.as_deref(), Some("nosuchremote:x/y.txt"));
        assert_eq!(e.summary(), "no such remote: nosuchremote:x/y.txt");
    }

    /// `purge` aimed at a file — the verb-vs-target mismatch that
    /// `Operation::Delete { is_dir }` exists to prevent. It must not read
    /// as "not found": the file is right there.
    #[test]
    fn purging_a_file_is_a_wrong_kind_error_not_a_missing_one() {
        let e = distil(
            &[
                r#"{"level":"critical","msg":"Failed to create file system for \"C:/x/a.txt\": is a file not a directory"}"#,
            ],
            1,
        );
        assert_eq!(e.kind, ErrorKind::WrongKind);
        assert_eq!(e.message, "is a file not a directory");
        assert_eq!(e.object.as_deref(), Some("C:/x/a.txt"));
    }

    /// Windows says "Access is denied." where POSIX says "permission
    /// denied"; both have to reach the same kind, because the UAC retry
    /// and the wording the user hears key off it.
    #[test]
    fn windows_and_posix_denials_agree() {
        let win = distil(
            &[
                r#"{"level":"notice","msg":"Failed to copyto: open \\\\?\\C:\\Windows\\x.txt: Access is denied."}"#,
            ],
            1,
        );
        assert_eq!(win.kind, ErrorKind::PermissionDenied);
        assert_eq!(win.summary(), "permission denied");

        let posix = distil(
            &[r#"{"level":"notice","msg":"Failed to copyto: open /etc/x: permission denied"}"#],
            1,
        );
        assert_eq!(posix.kind, ErrorKind::PermissionDenied);
    }

    /// Nothing on stdout or stderr at all — a killed or crashed child.
    /// The exit code is then the only evidence, and the summary must
    /// still be a sentence rather than an empty string.
    #[test]
    fn a_silent_failure_falls_back_to_the_exit_code() {
        let silent = distil(&[], 4);
        assert_eq!(silent.kind, ErrorKind::NotFound, "exit 4 is file not found");
        assert!(silent.message.is_empty());
        assert_eq!(silent.summary(), "not found");

        let unknown = distil(&[], 2);
        assert_eq!(unknown.kind, ErrorKind::Other);
        assert_eq!(unknown.summary(), "rclone exited with code 2");
    }

    /// rclone rejects bad flags before its JSON logger exists, so those
    /// failures arrive as plain text. Dropping unparsable lines would
    /// make our own argv bugs look like silent failures.
    #[test]
    fn non_json_output_is_kept_verbatim() {
        let e = distil(&["Error: unknown flag: --nonsense"], 1);
        assert_eq!(e.message, "Error: unknown flag: --nonsense");
        // Unrecognised text is surfaced as-is rather than as "failed".
        assert_eq!(e.summary(), "Error: unknown flag: --nonsense");
    }

    /// An error rclone attaches to a real object should name it, since
    /// the object is the one thing the user recognises in a batch.
    #[test]
    fn an_object_level_error_keeps_its_path() {
        let e = distil(
            &[
                r#"{"level":"error","msg":"Failed to copy: operation not permitted","object":"sub/report.txt","objectType":"*local.Object"}"#,
            ],
            1,
        );
        assert_eq!(e.kind, ErrorKind::PermissionDenied);
        assert_eq!(e.summary(), "permission denied: sub/report.txt");
    }

    /// The details pane is bounded, but silently truncating it would read
    /// as "that was everything". A per-object failure storm has to say
    /// how much it is hiding.
    #[test]
    fn the_details_pane_is_capped_and_says_so() {
        let lines: Vec<String> = (0..40)
            .map(|i| format!(r#"{{"level":"error","msg":"Failed to copy: file {i} is broken"}}"#))
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let e = distil(&refs, 1);
        assert_eq!(e.detail.len(), MAX_DETAIL_LINES + 1);
        assert_eq!(e.detail.last().unwrap(), "… and 20 more");
    }

    /// The stats tick carries `lastError`, which is the only summary a
    /// cancelled-mid-run or `--retries 1` op may leave behind.
    #[test]
    fn stats_last_error_is_the_fallback_message() {
        let e = distil(
            &[
                r#"{"level":"notice","msg":"0 B / 0 B","stats":{"bytes":0,"errors":1,"lastError":"couldn't connect: no such host"}}"#,
            ],
            1,
        );
        assert_eq!(e.kind, ErrorKind::Network);
        assert_eq!(e.message, "couldn't connect: no such host");
    }

    /// A successful run logs nothing error-shaped; the driver uses this
    /// to skip building a report at all.
    #[test]
    fn a_clean_run_collects_nothing() {
        let mut c = ErrorCollector::default();
        c.observe_line(r#"{"level":"info","msg":"Copied (new)","size":4,"object":"a.txt","objectType":"*local.Object"}"#);
        c.observe_line(
            r#"{"level":"notice","msg":"8 B / 8 B, 100%","stats":{"bytes":8,"errors":0}}"#,
        );
        assert!(c.is_empty());
    }
}
