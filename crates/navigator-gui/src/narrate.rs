//! How a long file operation reports itself: what to say, and when.
//!
//! One user action (a paste, a delete) is *not* one rclone invocation. A
//! paste of 200 files from one folder is a single `--files-from` call; a
//! paste of three folders is three calls; a delete of 200 items is 200
//! renames. The old code narrated at the item level — `"1 of 200: a.txt"`,
//! `"2 of 200: b.txt"`, … — which batching then silenced entirely, because
//! there is no longer an item loop to hang an utterance on.
//!
//! Speaking every file was never right anyway: at ~2 seconds an utterance,
//! 200 of them outlast the copy. What a screen-reader user actually wants
//! from a running transfer is a periodic "how far along, how much left",
//! and silence when the answer hasn't changed.
//!
//! So this module models the *job*, not the invocation:
//!
//! * [`Meter`] aggregates however many rclone calls the job takes into one
//!   monotonic 0–100%. Each invocation carries a **weight** in job units
//!   (a group of 40 files weighs 40, a directory weighs 1) and contributes
//!   `weight × its own fraction`, so the percentage never jumps backwards
//!   when one call ends and the next begins.
//! * [`Cadence`] rate-limits speech: nothing at all for the first interval
//!   (so a fast copy stays silent bar its summary), then at most one
//!   utterance per interval, and never the same sentence twice in a row —
//!   a stalled transfer goes quiet instead of chanting "45 percent".
//!
//! Everything here is pure and unit-tested. The Win32 side (posting to the
//! progress window, pushing to the speech sink) lives in `app.rs`'s
//! `OpProgress`, which drives these types.

use std::time::{Duration, Instant};

/// Job-level progress across however many rclone invocations one user
/// action takes.
///
/// "Units" are whatever the caller counts: files for a paste, items for a
/// delete. `total` is known up front; each invocation announces the share
/// of it that it covers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meter {
    total: u64,
    /// Units fully accounted for by invocations that have finished.
    done: u64,
    /// Units the in-flight invocation is worth.
    cur_weight: u64,
    /// How far into the in-flight invocation we are, `0.0..=1.0`.
    cur_fraction: f64,
}

impl Meter {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            ..Default::default()
        }
    }

    /// Start an invocation worth `weight` job units.
    pub fn begin(&mut self, weight: u64) {
        self.cur_weight = weight;
        self.cur_fraction = 0.0;
    }

    /// Update how far into the in-flight invocation we are.
    ///
    /// Clamped and monotonic within the invocation: rclone's byte totals
    /// grow while it scans, which would otherwise walk the percentage
    /// backwards mid-transfer.
    pub fn set_fraction(&mut self, f: f64) {
        let f = f.clamp(0.0, 1.0);
        if f > self.cur_fraction {
            self.cur_fraction = f;
        }
    }

    /// Fold the in-flight invocation into the completed total, whether it
    /// succeeded or not — a failed item is still one the user is no longer
    /// waiting on.
    pub fn finish(&mut self) {
        self.done = (self.done + self.cur_weight).min(self.total);
        self.cur_weight = 0;
        self.cur_fraction = 0.0;
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Units done, counting the in-flight invocation's share. Floored, so
    /// it only claims a unit that is genuinely finished.
    pub fn units_done(&self) -> u64 {
        let partial = self.cur_weight as f64 * self.cur_fraction;
        (self.done + partial as u64).min(self.total)
    }

    /// Whole-percent completion of the whole job, or `None` when there is
    /// nothing to measure against.
    ///
    /// Computed from the unfloored fraction so a single-unit job (one big
    /// file) still moves: flooring first would pin it at 0% until the
    /// moment it finished.
    pub fn percent(&self) -> Option<u32> {
        if self.total == 0 {
            return None;
        }
        let exact = self.done as f64 + self.cur_weight as f64 * self.cur_fraction;
        Some(((exact / self.total as f64) * 100.0).clamp(0.0, 100.0) as u32)
    }
}

/// What to say about a job **in flight**, or `None` when there is nothing
/// worth saying.
///
/// Deliberately terse — this is spoken over the top of whatever the user is
/// doing, once every few seconds. The filename is *not* included: it
/// changes faster than speech can keep up and is the thing batching exists
/// to stop announcing. The progress window carries the detail instead.
///
/// A completed job says nothing: every caller follows it immediately with
/// a summary ("done — 200 items, update"), and rclone's closing stats tick
/// would otherwise get "100 percent, 200 of 200" in just ahead of it.
pub fn phrase(m: &Meter) -> Option<String> {
    let pct = m.percent();
    if pct == Some(100) {
        return None;
    }
    let counts = m.total() > 1;
    match (pct, counts) {
        (Some(p), true) => Some(format!(
            "{} percent, {} of {}",
            p,
            m.units_done(),
            m.total()
        )),
        (Some(p), false) => Some(format!("{} percent", p)),
        (None, true) => Some(format!("{} of {}", m.units_done(), m.total())),
        (None, false) => None,
    }
}

/// Opening line for a job. Spoken once so the keystroke is acknowledged
/// immediately, before rclone has anything to report.
pub fn opening(verb: &str, total: u64, first: Option<&str>) -> String {
    match (total, first) {
        (1, Some(name)) => format!("{} {}", verb, name),
        (1, None) => verb.to_string(),
        (n, _) => format!("{} {} items", verb, n),
    }
}

/// Rate limiter for spoken progress.
///
/// Holds the last utterance as well as the last instant so an unchanged
/// message is skipped: a stalled or idling transfer then falls silent
/// rather than repeating itself every interval, and resumes the moment the
/// numbers move again.
#[derive(Debug, Clone)]
pub struct Cadence {
    interval: Duration,
    /// Instant the next utterance is measured from — the job start until
    /// something has been spoken. Doubles as the initial grace period, so
    /// an operation that finishes inside one interval says nothing at all
    /// beyond its summary.
    since: Instant,
    last_phrase: Option<String>,
}

impl Cadence {
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            since: now,
            last_phrase: None,
        }
    }

    /// Should `phrase` be spoken at `now`? Records the utterance when it
    /// returns `true`, so callers must only call this when they are
    /// actually going to speak.
    pub fn due(&mut self, now: Instant, phrase: &str) -> bool {
        if phrase.is_empty() || now.duration_since(self.since) < self.interval {
            return false;
        }
        if self.last_phrase.as_deref() == Some(phrase) {
            return false;
        }
        self.since = now;
        self.last_phrase = Some(phrase.to_string());
        true
    }
}

/// One thing that went wrong inside a job.
///
/// Built from a [`navigator_rclone::RcloneError`], or by hand for a
/// failure rclone never saw (a `--files-from` list we couldn't stage, a
/// batch that exited 0 with destinations missing).
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// Short phrase naming the problem: "not found", "permission denied".
    pub reason: String,
    /// What it happened to, when the job knows. The job usually does and
    /// rclone usually doesn't — rclone names a `\\?\`-prefixed absolute
    /// path or a Go `Fs` description, where the caller has the filename
    /// the user selected.
    pub subject: Option<String>,
    /// Supporting rclone lines for the details block.
    pub detail: Vec<String>,
}

impl Failure {
    /// A failure the app itself detected, with no rclone error behind it.
    pub fn app(reason: impl Into<String>, subject: Option<String>) -> Self {
        Self {
            reason: reason.into(),
            subject,
            detail: Vec::new(),
        }
    }

    /// Adopt a distilled rclone error. `subject` is the caller's name for
    /// the item, which beats anything in the log — rclone's own `object`
    /// is used only as the fallback.
    ///
    /// Takes `reason()` rather than `summary()`: the subject lives in its
    /// own field here, and `summary()` folds it into the sentence, so
    /// using it would name the item twice ("not found: C:\x\notes.txt,
    /// notes.txt") and defeat the grouping — two files failing the same
    /// way would look like two different reasons.
    pub fn from_rclone(e: &navigator_rclone::RcloneError, subject: Option<String>) -> Self {
        Self {
            reason: e.reason(),
            subject: subject.or_else(|| e.object.clone()),
            detail: e.detail.clone(),
        }
    }
}

/// What to tell the user about a job that failed: one dialog, one spoken
/// line, however many invocations went wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureReport {
    /// Dialog caption.
    pub title: String,
    /// One line, spoken and repeated at the top of the body.
    pub headline: String,
    pub body: String,
}

/// Subjects listed per distinct reason before the line trails off. Enough
/// to recognise which files are affected without turning the dialog into
/// a file listing.
const MAX_SUBJECTS: usize = 8;
/// rclone detail lines carried into the dialog.
const MAX_DETAIL: usize = 8;

/// Collapse a job's failures into one report, or `None` when nothing
/// failed.
///
/// **Errors belong to the job, not the invocation** — the same rule
/// [`Meter`] enforces for progress. Reporting per invocation meant a
/// five-item delete of files that were already gone put up five identical
/// modal dialogs, each blocking the worker until dismissed. Identical
/// reasons collapse to one line with a count, so the common case (one
/// cause, many items) reads as one sentence.
pub fn failure_report(verb: &str, total: u64, failures: &[Failure]) -> Option<FailureReport> {
    if failures.is_empty() {
        return None;
    }
    let noun = past_tense(verb);

    // Group by reason, first-seen order. Not a HashMap: the order the
    // failures happened in is the order they make sense in.
    let mut groups: Vec<(&str, Vec<&str>, usize)> = Vec::new();
    for f in failures {
        let subject = f.subject.as_deref();
        match groups.iter_mut().find(|(r, _, _)| *r == f.reason) {
            Some((_, subjects, count)) => {
                *count += 1;
                if let Some(s) = subject {
                    subjects.push(s);
                }
            }
            None => groups.push((f.reason.as_str(), subject.into_iter().collect(), 1)),
        }
    }

    let n = failures.len();
    let headline = match (n, groups.len()) {
        // One failure: name it and what it happened to.
        (1, _) => {
            let (reason, subjects, _) = &groups[0];
            match subjects.first() {
                Some(s) => format!("{} failed: {}, {}", noun, reason, s),
                None => format!("{} failed: {}", noun, reason),
            }
        }
        // Many failures, one cause: say the cause once.
        (n, 1) => format!(
            "{} failed for {}: {}",
            noun,
            count_of(n, total),
            groups[0].0
        ),
        // Many causes: the body enumerates them.
        (n, g) => format!(
            "{} failed for {}, {} different errors",
            noun,
            count_of(n, total),
            g
        ),
    };

    let mut body = headline.clone();
    if groups.len() > 1 || groups[0].1.len() > 1 {
        body.push('\n');
        for (reason, subjects, count) in &groups {
            body.push('\n');
            if *count > 1 {
                body.push_str(&format!("{} ({}):", reason, count));
            } else {
                body.push_str(&format!("{}:", reason));
            }
            if subjects.is_empty() {
                body.pop(); // no list to introduce — drop the colon
                continue;
            }
            body.push(' ');
            body.push_str(&subjects[..subjects.len().min(MAX_SUBJECTS)].join(", "));
            if subjects.len() > MAX_SUBJECTS {
                body.push_str(&format!(", … and {} more", subjects.len() - MAX_SUBJECTS));
            }
        }
    }

    // rclone's own words, last, for anyone who wants them. Deduped across
    // failures: a batch that failed for one reason repeats one detail line
    // per invocation.
    let mut details: Vec<&str> = Vec::new();
    for f in failures {
        for d in &f.detail {
            if d != &headline && !details.contains(&d.as_str()) {
                details.push(d);
            }
        }
    }
    if !details.is_empty() {
        body.push_str("\n\nrclone said:\n");
        for d in details.iter().take(MAX_DETAIL) {
            body.push_str(d);
            body.push('\n');
        }
        if details.len() > MAX_DETAIL {
            body.push_str(&format!("… and {} more\n", details.len() - MAX_DETAIL));
        }
    }

    Some(FailureReport {
        title: format!("{} failed", noun),
        headline,
        body,
    })
}

/// "3 of 5 items", or just "3 items" when the total says nothing extra.
fn count_of(n: usize, total: u64) -> String {
    if total as usize > n {
        format!("{} of {} items", n, total)
    } else {
        format!("{} items", n)
    }
}

/// Turn the progress window's gerund into the noun a failure headline
/// wants: "Copying" → "Copy". Unknown verbs pass through — a wrong-but-
/// readable headline beats a panic or an empty title.
fn past_tense(verb: &str) -> &str {
    match verb {
        "Copying" => "Copy",
        "Moving" => "Move",
        "Deleting" => "Delete",
        "Creating" => "Create",
        "Extracting" => "Extract",
        other => other,
    }
}

/// Detail line for the progress window. Unlike [`phrase`] this is read at
/// leisure, so it carries everything: counts, bytes, rate and ETA.
pub fn window_status(m: &Meter, p: &navigator_rclone::Progress) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(pct) = m.percent() {
        parts.push(format!("{}%", pct));
    }
    if m.total() > 1 {
        parts.push(format!("{} of {}", m.units_done(), m.total()));
    }
    if p.bytes_total > 0 {
        parts.push(format!(
            "{} of {}",
            crate::listview::format_size(p.bytes_done),
            crate::listview::format_size(p.bytes_total)
        ));
    } else if p.bytes_done > 0 {
        parts.push(crate::listview::format_size(p.bytes_done));
    }
    if p.speed_bps >= 1.0 {
        parts.push(format!(
            "{}/s",
            crate::listview::format_size(p.speed_bps as u64)
        ));
    }
    if let Some(eta) = p.eta_secs {
        parts.push(format!("{} left", human_secs(eta)));
    }
    parts.join(" — ")
}

/// Headline for the progress window: what it is doing and how far along.
///
/// The window uses this twice — as the caption (with " — navigator"
/// appended), so a screen reader's read-title command answers "how far
/// along is it?" without hunting for a label, and as the status label
/// while no individual file is known to be in flight. Hence no app name
/// here: it would read as noise inside the window.
pub fn window_title(verb: &str, m: &Meter) -> String {
    match m.percent() {
        Some(p) => format!("{}% — {}", p, verb),
        None => verb.to_string(),
    }
}

/// Duration in words, rounded to the unit that matters. Never fractional:
/// whole minutes is as much precision as an rclone ETA is worth.
pub fn human_secs(secs: u64) -> String {
    match secs {
        0 => "0 seconds".to_string(),
        1 => "1 second".to_string(),
        2..=89 => format!("{} seconds", secs),
        _ => {
            // Round to whole minutes first, then split, so 59 minutes 40
            // seconds reads as "1 hour" rather than "0 hours 60 minutes".
            let mins = (secs + 30) / 60;
            match (mins / 60, mins % 60) {
                (0, 1) => "1 minute".to_string(),
                (0, m) => format!("{} minutes", m),
                (1, 0) => "1 hour".to_string(),
                (h, 0) => format!("{} hours", h),
                (1, m) => format!("1 hour {} minutes", m),
                (h, m) => format!("{} hours {} minutes", h, m),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use navigator_rclone::Progress;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// The payoff case: one `--files-from` call carrying the whole paste.
    /// Its internal fraction has to drive the job percentage directly,
    /// otherwise a 200-file batch reports 0% until the moment it is done.
    #[test]
    fn one_invocation_covering_the_whole_job_tracks_its_fraction() {
        let mut m = Meter::new(200);
        m.begin(200);
        m.set_fraction(0.45);
        assert_eq!(m.percent(), Some(45));
        assert_eq!(m.units_done(), 90);
        assert_eq!(phrase(&m).as_deref(), Some("45 percent, 90 of 200"));
    }

    /// A paste of several folders is several rclone calls. The percentage
    /// must keep climbing across the seam — the bug this replaces reported
    /// each invocation's own bytes, so every new call restarted at zero.
    #[test]
    fn percent_never_walks_backwards_across_invocations() {
        let mut m = Meter::new(4);
        let mut seen: Vec<u32> = Vec::new();
        for _ in 0..4 {
            m.begin(1);
            for f in [0.0, 0.5, 1.0] {
                m.set_fraction(f);
                seen.push(m.percent().unwrap());
            }
            m.finish();
            seen.push(m.percent().unwrap());
        }
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]),
            "percentage went backwards: {:?}",
            seen
        );
        assert_eq!(seen.last(), Some(&100));
    }

    /// rclone's byte total grows while it scans, so a fraction computed
    /// from it can dip. Within one invocation the meter only ever moves
    /// forward.
    #[test]
    fn fraction_is_monotonic_within_an_invocation() {
        let mut m = Meter::new(1);
        m.begin(1);
        m.set_fraction(0.6);
        m.set_fraction(0.2);
        assert_eq!(m.percent(), Some(60));
    }

    /// A single big file is one unit. Flooring the unit count first would
    /// pin it at 0% for the entire transfer, which is exactly the "no
    /// progress at all" complaint.
    #[test]
    fn a_single_unit_job_still_reports_a_percentage() {
        let mut m = Meter::new(1);
        m.begin(1);
        m.set_fraction(0.37);
        assert_eq!(m.percent(), Some(37));
        assert_eq!(
            phrase(&m).as_deref(),
            Some("37 percent"),
            "one item: the count clause would be noise"
        );
    }

    /// Deletes are renames — instant, no bytes, no rclone stats at all.
    /// Counting completed invocations is the only signal, and it still has
    /// to produce something to say.
    #[test]
    fn count_only_jobs_narrate_from_completed_units() {
        let mut m = Meter::new(200);
        for _ in 0..120 {
            m.begin(1);
            m.finish();
        }
        assert_eq!(m.units_done(), 120);
        assert_eq!(phrase(&m).as_deref(), Some("60 percent, 120 of 200"));
    }

    /// The caller's summary is the completion announcement. Speaking
    /// "100 percent, 200 of 200" a beat before "done — 200 items" is two
    /// utterances saying the same thing.
    #[test]
    fn a_finished_job_says_nothing_and_leaves_it_to_the_summary() {
        let mut m = Meter::new(200);
        m.begin(200);
        m.set_fraction(1.0);
        assert_eq!(m.percent(), Some(100));
        assert_eq!(phrase(&m), None);
        m.finish();
        assert_eq!(phrase(&m), None);
    }

    /// Weights let a mixed paste (one 40-file group plus two folders)
    /// stay on one scale.
    #[test]
    fn mixed_weights_sum_to_the_job_total() {
        let mut m = Meter::new(42);
        m.begin(40);
        m.set_fraction(1.0);
        m.finish();
        assert_eq!(m.units_done(), 40);
        m.begin(1);
        m.finish();
        m.begin(1);
        m.finish();
        assert_eq!(m.percent(), Some(100));
        assert_eq!(m.units_done(), 42);
    }

    /// A miscounted weight must not push the job past its own total.
    #[test]
    fn units_and_percent_are_capped() {
        let mut m = Meter::new(2);
        m.begin(5);
        m.set_fraction(1.0);
        assert_eq!(m.units_done(), 2);
        assert_eq!(m.percent(), Some(100));
        m.finish();
        assert_eq!(m.units_done(), 2);
    }

    /// The whole point of the rewrite: a copy that finishes inside one
    /// interval says nothing while it runs. The caller's completion
    /// summary is the only utterance.
    #[test]
    fn nothing_is_spoken_during_the_first_interval() {
        let t0 = Instant::now();
        let mut c = Cadence::new(secs(5), t0);
        assert!(!c.due(t0, "10 percent"));
        assert!(!c.due(t0 + secs(4), "40 percent"));
        assert!(c.due(t0 + secs(5), "50 percent"));
    }

    #[test]
    fn utterances_are_spaced_by_the_interval() {
        let t0 = Instant::now();
        let mut c = Cadence::new(secs(5), t0);
        assert!(c.due(t0 + secs(5), "10 percent"));
        assert!(!c.due(t0 + secs(9), "20 percent"));
        assert!(c.due(t0 + secs(10), "20 percent"));
    }

    /// A stalled transfer repeats the same numbers forever. Saying them
    /// again adds nothing and talks over the user; going quiet and
    /// resuming when they move is the useful behaviour.
    #[test]
    fn an_unchanged_phrase_is_not_repeated() {
        let t0 = Instant::now();
        let mut c = Cadence::new(secs(5), t0);
        assert!(c.due(t0 + secs(5), "40 percent"));
        assert!(!c.due(t0 + secs(30), "40 percent"));
        assert!(
            c.due(t0 + secs(35), "41 percent"),
            "movement must break the silence immediately"
        );
    }

    #[test]
    fn openings_read_naturally() {
        assert_eq!(
            opening("copying", 1, Some("notes.txt")),
            "copying notes.txt"
        );
        assert_eq!(opening("copying", 12, Some("a.txt")), "copying 12 items");
        assert_eq!(opening("deleting", 200, None), "deleting 200 items");
    }

    #[test]
    fn durations_round_to_the_unit_that_matters() {
        assert_eq!(human_secs(1), "1 second");
        assert_eq!(human_secs(45), "45 seconds");
        assert_eq!(human_secs(89), "89 seconds");
        assert_eq!(human_secs(90), "2 minutes");
        assert_eq!(human_secs(600), "10 minutes");
        // Rounding must not leave "0 hours 60 minutes" at the boundary.
        assert_eq!(human_secs(3580), "1 hour");
        assert_eq!(human_secs(3600), "1 hour");
        assert_eq!(human_secs(7500), "2 hours 5 minutes");
    }

    /// The window is read at leisure, so it gets the detail speech leaves
    /// out — but only the parts rclone has actually reported.
    #[test]
    fn window_status_shows_detail_and_skips_unknowns() {
        let mut m = Meter::new(20);
        m.begin(20);
        m.set_fraction(0.45);
        let p = Progress {
            bytes_done: 47_185_920,
            bytes_total: 104_857_600,
            speed_bps: 5_242_880.0,
            eta_secs: Some(11),
            ..Default::default()
        };
        let s = window_status(&m, &p);
        assert!(s.contains("45%"), "{s}");
        assert!(s.contains("9 of 20"), "{s}");
        assert!(s.contains("11 seconds left"), "{s}");

        let bare = window_status(&Meter::new(0), &Progress::default());
        assert_eq!(bare, "", "nothing known yet means nothing shown");
    }

    #[test]
    fn window_title_carries_the_percentage() {
        let mut m = Meter::new(10);
        m.begin(10);
        m.set_fraction(0.5);
        assert_eq!(window_title("Copying", &m), "50% — Copying");
        assert_eq!(window_title("Copying", &Meter::new(0)), "Copying");
    }

    fn fail(reason: &str, subject: &str) -> Failure {
        Failure::app(reason, Some(subject.into()))
    }

    /// A job that succeeded has nothing to report, and the caller uses
    /// `None` to decide whether to open a dialog at all.
    #[test]
    fn no_failures_means_no_report() {
        assert!(failure_report("Copying", 5, &[]).is_none());
    }

    /// The single-item case is the one the user hits most, and it should
    /// read as one plain sentence — cause and item, no counts.
    #[test]
    fn one_failure_names_the_cause_and_the_item() {
        let r = failure_report("Deleting", 1, &[fail("not found", "notes.txt")]).unwrap();
        assert_eq!(r.title, "Delete failed");
        assert_eq!(r.headline, "Delete failed: not found, notes.txt");
        // Nothing to enumerate, so the body is just the sentence.
        assert_eq!(r.body, r.headline);
    }

    /// Many items failing for one reason is the second-most common case
    /// (a stale listing, a disconnected drive). The cause is stated once,
    /// not once per item — this is what replaced a dialog per invocation.
    #[test]
    fn identical_reasons_collapse_to_one_line_with_a_count() {
        let r = failure_report(
            "Deleting",
            5,
            &[
                fail("not found", "a.txt"),
                fail("not found", "b.txt"),
                fail("not found", "c.txt"),
            ],
        )
        .unwrap();
        assert_eq!(r.headline, "Delete failed for 3 of 5 items: not found");
        assert!(
            r.body.contains("not found (3): a.txt, b.txt, c.txt"),
            "the body still names which items: {}",
            r.body
        );
    }

    /// Mixed causes can't be summarised in the headline, so it counts
    /// them and the body breaks them down.
    #[test]
    fn distinct_reasons_are_enumerated_in_the_body() {
        let r = failure_report(
            "Copying",
            4,
            &[
                fail("not found", "gone.txt"),
                fail("permission denied", "locked.txt"),
                fail("permission denied", "system.dll"),
            ],
        )
        .unwrap();
        assert_eq!(
            r.headline,
            "Copy failed for 3 of 4 items, 2 different errors"
        );
        assert!(r.body.contains("not found: gone.txt"), "{}", r.body);
        assert!(
            r.body
                .contains("permission denied (2): locked.txt, system.dll"),
            "{}",
            r.body
        );
    }

    /// A 200-file paste that fails wholesale must not paste 200 filenames
    /// into a message box — but it must say that it is holding some back,
    /// or the list reads as complete.
    #[test]
    fn long_subject_lists_are_truncated_audibly() {
        let items: Vec<Failure> = (0..30)
            .map(|i| fail("not found", &format!("f{i}.txt")))
            .collect();
        let r = failure_report("Copying", 30, &items).unwrap();
        assert_eq!(r.headline, "Copy failed for 30 items: not found");
        assert!(r.body.contains("… and 22 more"), "{}", r.body);
    }

    /// rclone's own sentence is kept, once, after ours — deduped, because
    /// every invocation in a batch reports the same one.
    #[test]
    fn rclone_detail_is_appended_once() {
        let mut a = fail("not found", "a.txt");
        a.detail = vec!["a.txt is a directory or doesn't exist: object not found".into()];
        let mut b = fail("not found", "b.txt");
        b.detail = a.detail.clone();
        let r = failure_report("Deleting", 2, &[a, b]).unwrap();
        assert_eq!(
            r.body.matches("is a directory or doesn't exist").count(),
            1,
            "the same rclone line must not repeat: {}",
            r.body
        );
        assert!(r.body.contains("rclone said:"), "{}", r.body);
    }

    /// The caller's name for an item beats rclone's: the job knows the
    /// filename the user selected, rclone knows a `\\?\` absolute path.
    /// The reason must stay free of the item either way, or grouping
    /// breaks — two files failing identically would read as two reasons.
    #[test]
    fn the_callers_subject_wins_over_rclones_object() {
        let e = navigator_rclone::RcloneError {
            kind: navigator_rclone::ErrorKind::NotFound,
            message: "object not found".into(),
            object: Some("C:/very/long/path/notes.txt".into()),
            exit_code: Some(4),
            detail: vec!["object not found".into()],
        };
        let with = Failure::from_rclone(&e, Some("notes.txt".into()));
        assert_eq!(with.subject.as_deref(), Some("notes.txt"));
        assert_eq!(with.reason, "not found");
        // With no caller subject, rclone's object is better than nothing.
        let without = Failure::from_rclone(&e, None);
        assert_eq!(
            without.subject.as_deref(),
            Some("C:/very/long/path/notes.txt")
        );
    }
}
