//! Pure tree comparison behind File → Compare trees….
//!
//! The user copies a tree listing (Alt+L, or any plain list of paths) from
//! somewhere else — another machine, another drive, an rclone remote — and
//! pastes it here. We walk the folder they are standing in and report what
//! the pasted tree has that this one doesn't, and vice versa.
//!
//! Kept free of HWND / speech for the same reason [`crate::props`] is: the
//! parse, the diff and the report are all unit-testable without a live
//! window. The walk itself is [`crate::props::walk_tree`], shared with the
//! Alt+L dump so the two can't drift.
//!
//! Four rules shape the output, and each has a test:
//!
//! * **A missing folder is reported once, and its contents are not.** If
//!   `a/b` is absent then `a/b/c`, `a/b/c/hi.mp3` and every other
//!   descendant are absent too, and listing them tells the user nothing
//!   they can act on — restoring `a/b` brings all of it back. The subtree
//!   is *counted* (`12 files inside`) and then skipped; the scan carries
//!   straight on with `a/b`'s siblings.
//! * **Sorting is component-wise, not string-wise.** That is what makes a
//!   folder's descendants contiguous, which is what lets the prune above
//!   be a single running prefix. Plain string order interleaves them:
//!   `a` < `a.txt` < `a/b`, because `.` (0x2E) sorts below `/` (0x2F).
//! * **Matching is case-insensitive.** Windows is, and the common case is
//!   comparing a local folder against a copy of itself. A case-only
//!   difference is not what the user opened this screen to find.
//! * **Both sides are completed with their implied parent folders.** A
//!   bare list of file paths carries no `dirs` array; without synthesising
//!   `a` and `a/b` out of `a/b/c.txt`, every intermediate folder of the
//!   walked tree would come back as "only here".

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry as MapEntry;

use navigator_rclone::RemoteTreeItem;

/// How many rows one section of the report lists before it truncates.
/// Silent truncation reads as "that was everything", so the cut always
/// says how many rows it dropped — same rule as the rclone error pane.
const MAX_LISTED: usize = 5000;

/// One node of a flattened tree: a root-relative, forward-slashed path
/// plus what it is and (for files) how big it is.
///
/// `key` is `path` lower-cased and is the identity used for both matching
/// and ordering. It is derived in [`TreeEntry::new`] rather than supplied
/// by callers so the two can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub key: String,
    pub is_dir: bool,
    pub size: u64,
}

impl TreeEntry {
    pub fn new(path: &str, is_dir: bool, size: u64) -> Self {
        let path = normalize_path(path);
        let key = path.to_lowercase();
        Self {
            path,
            key,
            is_dir,
            size,
        }
    }
}

/// A whole tree: deduplicated, completed with implied parents, and sorted
/// so that every folder is immediately followed by its own descendants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// What to call this tree in the report — a path, an rclone
    /// `remote:sub` argument, or whatever `root =` said in a pasted dump.
    pub label: String,
    /// Sub-trees that failed to enumerate during a local walk. Non-zero
    /// means the "missing here" list may be overstated, so the report says
    /// so rather than presenting a partial walk as fact.
    pub errors: u64,
    /// `total_size` as *declared* by a pasted dump. A dump lists paths
    /// without per-file sizes, so summing `entries` would report zero for
    /// a tree whose header knows the real number.
    pub declared_size: Option<u64>,
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Normalise, complete, dedupe and sort `entries` into a tree.
    pub fn new(label: impl Into<String>, entries: Vec<TreeEntry>) -> Self {
        let mut by_key: HashMap<String, TreeEntry> = HashMap::with_capacity(entries.len() * 2);
        for e in entries {
            if e.path.is_empty() {
                continue;
            }
            // Every ancestor of an entry is a folder, whether or not the
            // source bothered to list it.
            let owned = e.path.clone();
            let mut p = owned.as_str();
            while let Some(i) = p.rfind('/') {
                p = &p[..i];
                let parent = TreeEntry::new(p, true, 0);
                by_key
                    .entry(parent.key.clone())
                    .and_modify(|v| v.is_dir = true)
                    .or_insert(parent);
            }
            match by_key.entry(e.key.clone()) {
                MapEntry::Occupied(mut o) => {
                    let v = o.get_mut();
                    v.is_dir |= e.is_dir;
                    v.size = v.size.max(e.size);
                }
                MapEntry::Vacant(v) => {
                    v.insert(e);
                }
            }
        }
        let mut entries: Vec<TreeEntry> = by_key.into_values().collect();
        entries.sort_by(|a, b| key_cmp(&a.key, &b.key));
        Self {
            label: label.into(),
            errors: 0,
            declared_size: None,
            entries,
        }
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn file_count(&self) -> u64 {
        self.entries.iter().filter(|e| !e.is_dir).count() as u64
    }

    pub fn dir_count(&self) -> u64 {
        self.entries.iter().filter(|e| e.is_dir).count() as u64
    }

    /// Total bytes: the declared figure when the source gave one, else the
    /// sum of the file sizes we actually have.
    pub fn total_size(&self) -> u64 {
        self.declared_size.unwrap_or_else(|| {
            self.entries
                .iter()
                .filter(|e| !e.is_dir)
                .fold(0u64, |acc, e| acc.saturating_add(e.size))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build a tree from one `rclone lsjson --recursive` walk. The items
/// already carry root-relative forward-slashed paths, so there is no path
/// arithmetic to redo — which is what keeps this half testable with no
/// rclone binary in sight.
pub fn tree_from_remote_items(label: impl Into<String>, items: &[RemoteTreeItem]) -> Tree {
    let entries = items
        .iter()
        .map(|i| TreeEntry::new(&i.path, i.is_dir, if i.is_dir { 0 } else { i.size }))
        .collect();
    Tree::new(label, entries)
}

/// Root-relative path in the one shape everything downstream expects:
/// forward slashes, no `./` lead, no leading or trailing separator, no
/// doubled separator.
pub fn normalize_path(raw: &str) -> String {
    let mut s = raw.trim().replace('\\', "/");
    loop {
        if let Some(r) = s.strip_prefix("./") {
            s = r.trim_start().to_string();
            continue;
        }
        if let Some(r) = s.strip_prefix('/') {
            s = r.to_string();
            continue;
        }
        break;
    }
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// Order two keys component by component.
///
/// This is the whole reason the prune in [`missing_from`] can be a single
/// running prefix: plain string order puts `a.txt` between `a` and `a/b`,
/// which breaks a folder's descendants into non-contiguous runs.
/// Comparing `["a"]` against `["a", "b"]` against `["a.txt"]` keeps every
/// subtree in one block.
fn key_cmp(a: &str, b: &str) -> Ordering {
    a.split('/').cmp(b.split('/'))
}

// ---------------------------------------------------------------- parsing

/// A tree the user pasted in. Two shapes are accepted:
///
/// * the Alt+L dump — `root = "…"`, `total_size = N`, and the `dirs` /
///   `files` arrays, which is where folder-vs-file comes from;
/// * a bare list of paths, one per line, where a trailing separator marks
///   a folder and everything else is inferred from who has children.
///
/// The error case is input with nothing in it at all; a partially
/// unrecognisable paste yields whatever parsed, because a diff against
/// most of a tree still beats a dialog saying no.
pub fn parse_tree(text: &str) -> Result<Tree, String> {
    #[derive(PartialEq, Clone, Copy)]
    enum Sect {
        None,
        Dirs,
        Files,
    }

    let structured = text.lines().any(|l| {
        let t = l.trim_start();
        array_opens(t, "dirs").is_some() || array_opens(t, "files").is_some()
    });

    let mut label = String::new();
    let mut declared_size: Option<u64> = None;
    let mut entries: Vec<TreeEntry> = Vec::new();

    if structured {
        let mut sect = Sect::None;
        for line in text.lines() {
            let t = line.trim();
            match sect {
                Sect::None => {
                    if let Some(rest) = array_opens(t, "dirs") {
                        sect = Sect::Dirs;
                        if take_array_items(rest, true, &mut entries) {
                            sect = Sect::None;
                        }
                    } else if let Some(rest) = array_opens(t, "files") {
                        sect = Sect::Files;
                        if take_array_items(rest, false, &mut entries) {
                            sect = Sect::None;
                        }
                    } else if let Some(v) = scalar(t, "root") {
                        label = unescape(&v);
                    } else if let Some(v) = scalar(t, "total_size") {
                        declared_size = v.trim().parse::<u64>().ok();
                    }
                }
                Sect::Dirs | Sect::Files => {
                    if take_array_items(t, sect == Sect::Dirs, &mut entries) {
                        sect = Sect::None;
                    }
                }
            }
        }
    } else {
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            // A `key = value` header line from some other listing format
            // is not a path; skip it rather than diff against it.
            if t.contains(" = ") && !t.starts_with('"') {
                continue;
            }
            let mut raw = t.trim_end_matches(',').trim();
            let quoted = raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"');
            if quoted {
                raw = &raw[1..raw.len() - 1];
            }
            let owned = if quoted {
                unescape(raw)
            } else {
                raw.to_string()
            };
            let is_dir = owned.ends_with('/') || owned.ends_with('\\');
            let e = TreeEntry::new(&owned, is_dir, 0);
            if !e.path.is_empty() {
                entries.push(e);
            }
        }
    }

    if entries.is_empty() {
        return Err(
            "nothing recognisable in the pasted text — paste an Alt+L tree dump, \
                    or one path per line"
                .to_string(),
        );
    }

    let mut tree = Tree::new(
        if label.is_empty() {
            "(pasted)".to_string()
        } else {
            label
        },
        entries,
    );
    tree.declared_size = declared_size;
    Ok(tree)
}

/// `Some(rest_of_line)` when `line` opens the TOML array named `name`.
fn array_opens<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.trim_start().strip_prefix(name)?;
    let rest = rest.trim_start().strip_prefix('=')?;
    rest.trim_start().strip_prefix('[')
}

/// `Some(value)` when `line` is the TOML scalar assignment `name = value`.
fn scalar(line: &str, name: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(name)?;
    let rest = rest.trim_start().strip_prefix('=')?.trim();
    let rest = rest.strip_suffix(',').unwrap_or(rest).trim();
    if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
        Some(rest[1..rest.len() - 1].to_string())
    } else {
        Some(rest.to_string())
    }
}

/// Pull every quoted string out of one line of array body. Returns true
/// when the line closes the array.
fn take_array_items(line: &str, is_dir: bool, out: &mut Vec<TreeEntry>) -> bool {
    let body = match line.find(']') {
        Some(i) => &line[..i],
        None => line,
    };
    for s in quoted_strings(body) {
        let e = TreeEntry::new(&s, is_dir, 0);
        if !e.path.is_empty() {
            out.push(e);
        }
    }
    line.contains(']')
}

/// Every `"…"` on the line, with TOML backslash escapes resolved.
fn quoted_strings(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inside = false;
    let mut escaped = false;
    for c in line.chars() {
        if !inside {
            if c == '"' {
                inside = true;
                cur.clear();
            }
            continue;
        }
        if escaped {
            cur.push(unescape_char(c));
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            inside = false;
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            out.push(unescape_char(c));
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_char(c: char) -> char {
    match c {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        other => other,
    }
}

// ---------------------------------------------------------------- diffing

/// One thing the other tree has that this one doesn't. For a folder,
/// `hidden_*` count what was suppressed below it — the point of the prune
/// is that the user never sees those rows, not that they never happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingEntry {
    pub path: String,
    pub is_dir: bool,
    pub hidden_files: u64,
    pub hidden_dirs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// In the pasted tree, absent from the walked one.
    pub missing_here: Vec<MissingEntry>,
    /// In the walked tree, absent from the pasted one.
    pub only_here: Vec<MissingEntry>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.missing_here.is_empty() && self.only_here.is_empty()
    }
}

pub fn compare(here: &Tree, there: &Tree) -> Diff {
    Diff {
        missing_here: missing_from(here, there),
        only_here: missing_from(there, here),
    }
}

/// Everything in `want` that `have` doesn't have, pruned at the first
/// missing folder on each branch.
///
/// The prune is one running prefix rather than a set because `want` is
/// sorted component-wise, so a folder's descendants are the block of rows
/// directly after it. Anything landing inside that block is tallied onto
/// the folder and dropped.
fn missing_from(have: &Tree, want: &Tree) -> Vec<MissingEntry> {
    let present: std::collections::HashSet<&str> =
        have.entries().iter().map(|e| e.key.as_str()).collect();
    let mut out: Vec<MissingEntry> = Vec::new();
    let mut prune: Option<String> = None;

    for e in want.entries() {
        if let Some(p) = &prune {
            if e.key.starts_with(p.as_str()) {
                if let Some(last) = out.last_mut() {
                    if e.is_dir {
                        last.hidden_dirs += 1;
                    } else {
                        last.hidden_files += 1;
                    }
                }
                continue;
            }
            prune = None;
        }
        if present.contains(e.key.as_str()) {
            continue;
        }
        out.push(MissingEntry {
            path: e.path.clone(),
            is_dir: e.is_dir,
            hidden_files: 0,
            hidden_dirs: 0,
        });
        if e.is_dir {
            prune = Some(format!("{}/", e.key));
        }
    }
    out
}

// -------------------------------------------------------------- reporting

/// Human-readable comparison, ready for the viewer window.
pub fn format_report(here: &Tree, there: &Tree, diff: &Diff) -> String {
    let mut s = String::new();

    s.push_str("Tree comparison\n");
    s.push_str("===============\n\n");
    s.push_str(&format!("Here (current):  {}\n", here.label));
    s.push_str(&format!("                 {}\n", census(here)));
    s.push_str(&format!("There (pasted):  {}\n", there.label));
    s.push_str(&format!("                 {}\n", census(there)));
    s.push('\n');

    let (m_files, m_dirs, m_hidden_f, m_hidden_d) = tally(&diff.missing_here);
    let (o_files, o_dirs, o_hidden_f, o_hidden_d) = tally(&diff.only_here);

    s.push_str("Summary\n");
    s.push_str("-------\n");
    s.push_str(&format!("Total files missing:    {}\n", commas(m_files)));
    s.push_str(&format!("Total folders missing:  {}\n", commas(m_dirs)));
    if let Some(line) = holds(m_hidden_f, m_hidden_d) {
        s.push_str(&line);
    }
    s.push_str(&format!("Only here — files:      {}\n", commas(o_files)));
    s.push_str(&format!("Only here — folders:    {}\n", commas(o_dirs)));
    if let Some(line) = holds(o_hidden_f, o_hidden_d) {
        s.push_str(&line);
    }
    if here.errors > 0 {
        s.push_str(&format!(
            "\nWarning: {} could not be read while walking this tree, so items\n\
             below them are reported as missing whether they are or not.\n",
            plural(here.errors, "sub-folder", "sub-folders"),
        ));
    }
    s.push_str(
        "\nMatching is case-insensitive. A folder missing in whole is listed once,\n\
         without its contents.\n",
    );

    if diff.is_empty() {
        s.push_str("\n--- No differences ---\n\n");
        s.push_str(&format!("Both trees hold the same {}.\n", census(here)));
        return s;
    }

    section(
        &mut s,
        "Missing here",
        "in the pasted tree, not in this one",
        &diff.missing_here,
    );
    section(
        &mut s,
        "Only here",
        "in this tree, not in the pasted one",
        &diff.only_here,
    );
    s
}

/// Parse `other_text`, diff it against the tree we walked, and render the
/// report — the whole job in one pure call, so the worker thread that runs
/// it has no logic of its own to get wrong.
pub fn compare_against_text(here: Tree, other_text: &str) -> String {
    match parse_tree(other_text) {
        Ok(there) => {
            let diff = compare(&here, &there);
            format_report(&here, &there, &diff)
        }
        Err(e) => format_error(&here.label, &e),
    }
}

/// Render a failed comparison. A parse or walk error must never come back
/// as an empty diff — "no differences" is the one answer the user would
/// act on and the one we cannot stand behind.
pub fn format_error(here_label: &str, err: &str) -> String {
    format!(
        "Tree comparison\n\
         ===============\n\n\
         Here (current):  {here_label}\n\n\
         Comparison failed — nothing was compared.\n\n\
         {err}\n"
    )
}

fn section(out: &mut String, title: &str, subtitle: &str, items: &[MissingEntry]) {
    let dirs: Vec<&MissingEntry> = items.iter().filter(|e| e.is_dir).collect();
    let files: Vec<&MissingEntry> = items.iter().filter(|e| !e.is_dir).collect();

    out.push_str(&format!("\n--- {title} ({subtitle}) ---\n"));
    if items.is_empty() {
        out.push_str("\n  nothing\n");
        return;
    }
    if !dirs.is_empty() {
        out.push_str(&format!(
            "\nFolders ({}) — contents not listed:\n",
            commas(dirs.len() as u64)
        ));
        let width = dirs
            .iter()
            .take(MAX_LISTED)
            .map(|e| e.path.chars().count() + 1)
            .max()
            .unwrap_or(0)
            .min(60);
        for e in dirs.iter().take(MAX_LISTED) {
            let label = format!("{}/", e.path);
            out.push_str(&format!(
                "  {label:<width$}  {inside}\n",
                label = label,
                width = width,
                inside = inside(e)
            ));
        }
        if dirs.len() > MAX_LISTED {
            out.push_str(&format!(
                "  … and {} more folders, not listed\n",
                commas((dirs.len() - MAX_LISTED) as u64)
            ));
        }
    }
    if !files.is_empty() {
        out.push_str(&format!("\nFiles ({}):\n", commas(files.len() as u64)));
        for e in files.iter().take(MAX_LISTED) {
            out.push_str(&format!("  {}\n", e.path));
        }
        if files.len() > MAX_LISTED {
            out.push_str(&format!(
                "  … and {} more files, not listed\n",
                commas((files.len() - MAX_LISTED) as u64)
            ));
        }
    }
}

/// The "and this much sits underneath them" note in the summary. `None`
/// when nothing was suppressed, and a zero half is dropped rather than
/// printed — "and 0 sub-folders" is a fact nobody needed.
fn holds(hidden_files: u64, hidden_dirs: u64) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if hidden_files > 0 {
        parts.push(plural(hidden_files, "file", "files"));
    }
    if hidden_dirs > 0 {
        parts.push(plural(hidden_dirs, "sub-folder", "sub-folders"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "  (those folders hold a further {}, not listed)\n",
        parts.join(" and ")
    ))
}

fn inside(e: &MissingEntry) -> String {
    if e.hidden_files == 0 && e.hidden_dirs == 0 {
        return "[ empty ]".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if e.hidden_files > 0 {
        parts.push(plural(e.hidden_files, "file", "files"));
    }
    if e.hidden_dirs > 0 {
        parts.push(plural(e.hidden_dirs, "folder", "folders"));
    }
    format!("[ {} inside ]", parts.join(", "))
}

fn tally(items: &[MissingEntry]) -> (u64, u64, u64, u64) {
    let mut files = 0;
    let mut dirs = 0;
    let mut hf = 0;
    let mut hd = 0;
    for e in items {
        if e.is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
        hf += e.hidden_files;
        hd += e.hidden_dirs;
    }
    (files, dirs, hf, hd)
}

fn census(t: &Tree) -> String {
    let size = t.total_size();
    let head = format!(
        "{}, {}",
        plural(t.file_count(), "file", "files"),
        plural(t.dir_count(), "folder", "folders")
    );
    if size > 0 {
        format!("{head}, {}", crate::listview::format_size(size))
    } else {
        head
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", commas(n), if n == 1 { one } else { many })
}

/// Thousands separators. Long listings get read aloud as often as they get
/// read on screen, and "1234" comes out as one number nobody can hold.
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(label: &str, paths: &[(&str, bool)]) -> Tree {
        Tree::new(
            label,
            paths
                .iter()
                .map(|(p, d)| TreeEntry::new(p, *d, 0))
                .collect(),
        )
    }

    #[test]
    fn a_missing_folder_is_reported_once_and_its_contents_are_counted_not_listed() {
        let here = tree("here", &[("a", true)]);
        let there = tree(
            "there",
            &[
                ("a", true),
                ("a/b", true),
                ("a/b/hi.mp3", false),
                ("a/b/deep", true),
                ("a/b/deep/x.mp3", false),
            ],
        );
        let d = compare(&here, &there);
        assert_eq!(d.missing_here.len(), 1, "{:?}", d.missing_here);
        let m = &d.missing_here[0];
        assert_eq!(m.path, "a/b");
        assert!(m.is_dir);
        assert_eq!(m.hidden_files, 2);
        assert_eq!(m.hidden_dirs, 1);
    }

    #[test]
    fn siblings_after_a_pruned_folder_are_still_examined() {
        // The prune must end when the subtree does. `a/c` sits right after
        // `a/b`'s descendants and is missing in its own right.
        let here = tree("here", &[("a", true)]);
        let there = tree(
            "there",
            &[
                ("a", true),
                ("a/b", true),
                ("a/b/hi.mp3", false),
                ("a/c", true),
                ("a/d.txt", false),
            ],
        );
        let d = compare(&here, &there);
        let paths: Vec<&str> = d.missing_here.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["a/b", "a/c", "a/d.txt"]);
    }

    #[test]
    fn a_sibling_that_sorts_between_a_folder_and_its_children_does_not_break_the_prune() {
        // Plain string order is `a` < `a.txt` < `a/b`, which would clear a
        // running prefix before the children arrived. Component-wise order
        // keeps the subtree in one block.
        let here = tree("here", &[("keep", true)]);
        let there = tree(
            "there",
            &[
                ("a", true),
                ("a.txt", false),
                ("a/b", true),
                ("a/b/c.mp3", false),
            ],
        );
        let d = compare(&here, &there);
        let paths: Vec<&str> = d.missing_here.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "a.txt"]);
        assert_eq!(d.missing_here[0].hidden_files, 1);
        assert_eq!(d.missing_here[0].hidden_dirs, 1);
    }

    #[test]
    fn matching_ignores_case() {
        let here = tree("here", &[("Music/Song.MP3", false)]);
        let there = tree("there", &[("music/song.mp3", false)]);
        assert!(compare(&here, &there).is_empty());
    }

    #[test]
    fn implied_parents_are_synthesised_so_a_bare_file_list_compares_clean() {
        let here = tree("here", &[("a", true), ("a/b", true), ("a/b/c.txt", false)]);
        let there = tree("there", &[("a/b/c.txt", false)]);
        assert!(
            compare(&here, &there).is_empty(),
            "{:?}",
            compare(&here, &there)
        );
    }

    #[test]
    fn both_directions_are_reported() {
        let here = tree("here", &[("shared.txt", false), ("mine.txt", false)]);
        let there = tree("there", &[("shared.txt", false), ("theirs.txt", false)]);
        let d = compare(&here, &there);
        assert_eq!(d.missing_here.len(), 1);
        assert_eq!(d.missing_here[0].path, "theirs.txt");
        assert_eq!(d.only_here.len(), 1);
        assert_eq!(d.only_here[0].path, "mine.txt");
    }

    #[test]
    fn a_dump_round_trips_through_the_parser() {
        // Verbatim shape of `props::render_tree_toml` output.
        let dump = "root = \"D:\\\\music\"\n\
                    dir_count = 2\n\
                    file_count = 2\n\
                    total_size = 4096\n\
                    \n\
                    dirs = [\n\
                    \x20 \"a\",\n\
                    \x20 \"a/b\",\n\
                    ]\n\
                    \n\
                    files = [\n\
                    \x20 \"a/b/hi.mp3\",\n\
                    \x20 \"top.txt\",\n\
                    ]\n";
        let t = parse_tree(dump).expect("parses");
        assert_eq!(t.label, "D:\\music");
        assert_eq!(t.declared_size, Some(4096));
        assert_eq!(t.dir_count(), 2);
        assert_eq!(t.file_count(), 2);
        let paths: Vec<&str> = t.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "a/b", "a/b/hi.mp3", "top.txt"]);
    }

    #[test]
    fn a_bare_path_list_parses_and_a_trailing_slash_means_folder() {
        // Backslashes, a `./` lead and CRLF all normalise away; `empty/`
        // is a folder because of the trailing separator, and `a` / `a/b`
        // are folders because something lives under them.
        let t = parse_tree("empty/\n./a/b/hi.mp3\r\nsub\\deep\\y.txt\n\n").expect("parses");
        let dirs: Vec<&str> = t
            .entries()
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(dirs, vec!["a", "a/b", "empty", "sub", "sub/deep"]);
        let files: Vec<&str> = t
            .entries()
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(files, vec!["a/b/hi.mp3", "sub/deep/y.txt"]);
    }

    #[test]
    fn empty_input_is_an_error_not_an_empty_tree() {
        // An empty tree would diff as "everything here is extra", which
        // reads like a real answer. It isn't one.
        assert!(parse_tree("").is_err());
        assert!(parse_tree("   \n\n  \n").is_err());
    }

    #[test]
    fn an_inline_array_closes_on_its_own_line() {
        let t = parse_tree("dirs = []\nfiles = [\"a.txt\"]\nroot = \"x\"\n").expect("parses");
        assert_eq!(t.file_count(), 1);
        assert_eq!(t.dir_count(), 0);
    }

    #[test]
    fn quoted_paths_keep_their_escaped_backslashes() {
        let got = quoted_strings("  \"a\\\\b\", \"c\\\"d\",");
        assert_eq!(got, vec!["a\\b".to_string(), "c\"d".to_string()]);
    }

    #[test]
    fn normalize_strips_the_leads_and_the_trailing_separator() {
        assert_eq!(normalize_path("./a/b/"), "a/b");
        assert_eq!(normalize_path("\\a\\\\b\\"), "a/b");
        assert_eq!(normalize_path("  /a/b  "), "a/b");
        assert_eq!(normalize_path("/"), "");
    }

    #[test]
    fn the_report_leads_with_the_totals_and_never_lists_a_missing_folders_contents() {
        let here = tree("D:/music", &[("keep.txt", false)]);
        let there = tree(
            "mac:Music",
            &[
                ("keep.txt", false),
                ("gone", true),
                ("gone/a.mp3", false),
                ("gone/b.mp3", false),
                ("lost.txt", false),
            ],
        );
        let d = compare(&here, &there);
        let out = format_report(&here, &there, &d);
        assert!(out.contains("Total files missing:    1"), "{out}");
        assert!(out.contains("Total folders missing:  1"), "{out}");
        assert!(out.contains("gone/"), "{out}");
        assert!(out.contains("2 files inside"), "{out}");
        assert!(!out.contains("a.mp3"), "{out}");
        assert!(out.contains("lost.txt"), "{out}");
    }

    #[test]
    fn identical_trees_say_so_instead_of_printing_empty_sections() {
        let here = tree("a", &[("x.txt", false)]);
        let there = tree("b", &[("x.txt", false)]);
        let out = format_report(&here, &there, &compare(&here, &there));
        assert!(out.contains("No differences"), "{out}");
        assert!(!out.contains("Missing here ("), "{out}");
    }

    #[test]
    fn an_unreadable_subfolder_is_called_out_so_the_missing_list_is_not_taken_as_fact() {
        let mut here = tree("a", &[("x.txt", false)]);
        here.errors = 2;
        let there = tree("b", &[("x.txt", false), ("y.txt", false)]);
        let out = format_report(&here, &there, &compare(&here, &there));
        assert!(out.contains("2 sub-folders could not be read"), "{out}");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1000), "1,000");
        assert_eq!(commas(1234567), "1,234,567");
    }
}
