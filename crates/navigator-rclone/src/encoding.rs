//! Translating Windows filenames into the namespace rclone talks in.
//!
//! **rclone command lines and `--files-from` entries are not OS
//! filenames.** The local backend keeps names in an internal "standard"
//! form and applies an [encoder] on the way to and from the filesystem, so
//! a path handed to rclone is *decoded* into standard form before anything
//! is opened. Windows forbids `" * : < > ? \ |` in a filename, so the
//! encoder maps each to its full-width twin — `|` ⇄ `｜` (U+FF5C) — and
//! escapes a full-width character that was already there with a quote rune
//! `‛` (U+201B) so the two stay distinguishable.
//!
//! [encoder]: https://rclone.org/overview/#restricted-filenames
//!
//! Navigator lists directories itself, through `FindFirstFileExW`, so every
//! name it holds is in *OS* form. Handing those to rclone unconverted is
//! what this module exists to stop, and the failure is silent in the worst
//! place:
//!
//! * On the command line (`copyto`, `moveto`, `purge`, …) rclone decodes
//!   the path, fails to find it, and reports `directory not found`.
//! * In a `--files-from` list the entry is matched against rclone's *own*
//!   listing of the source, which is in standard form. A raw OS name never
//!   matches, so rclone transfers nothing, logs nothing, and **exits 0** —
//!   the paste reports success and the files never moved.
//!
//! The previous fix for the first half was a global `--local-encoding
//! None`, which makes rclone treat local names literally. It is wrong in
//! both directions: it never addressed the `--files-from` half at all, and
//! a name containing a literal `‛` becomes permanently unreachable, because
//! under `None` rclone still doubles the rune when it lists (`a‛b` reads
//! back as `a‛‛b`) but no longer halves it when it opens the file. That is
//! exactly the shape yt-dlp output acquires after one mixed-encoding copy,
//! and it is how a folder of `‛＂Title‛＂ [id].mp3` became unmovable.
//!
//! So: default encoding, and convert here. Only the *decode* direction (OS
//! → standard) is needed, and it never touches ASCII, which is why a drive
//! letter's colon and the path separators survive untouched.
//!
//! Every rule below was measured against rclone 1.73.5 by creating a file,
//! asking `rclone lsf` for the name it saw, and comparing — see
//! `tests/encoding.rs`, which re-runs that differential against whatever
//! rclone is on PATH.

/// The rune rclone escapes an already-encoded character with (U+201B,
/// SINGLE HIGH-REVERSED-9 QUOTATION MARK).
const QUOTE: char = '\u{201B}';

/// Full-width forms rclone decodes back to ASCII wherever they appear.
///
/// `／` (U+FF0F) is deliberately absent even though `Slash` is in the
/// Windows local encoding: decoding it would inject a path separator into
/// a single name component, so rclone leaves it alone.
const DECODABLE: &[(char, char)] = &[
    ('\u{FF02}', '"'),
    ('\u{FF0A}', '*'),
    ('\u{FF1A}', ':'),
    ('\u{FF1C}', '<'),
    ('\u{FF1E}', '>'),
    ('\u{FF1F}', '?'),
    ('\u{FF3C}', '\\'),
    ('\u{FF5C}', '|'),
];

/// Forms that decode only in the last position of a component, because
/// that is the only place the encoder puts them: Windows silently strips a
/// trailing space or period, so rclone substitutes `␠` / `．` there.
const TRAILING_DECODABLE: &[(char, char)] = &[('\u{2420}', ' '), ('\u{FF0E}', '.')];

fn decodable(c: char) -> Option<char> {
    DECODABLE.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

fn trailing_decodable(c: char) -> Option<char> {
    TRAILING_DECODABLE
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, v)| *v)
}

/// Encoded forms rclone recognises but refuses to decode, so a `‛` in
/// front of one is kept rather than consumed: `／` (a separator if
/// decoded) and `␀`..`␟` (U+2400..=U+241F, the control-character symbols,
/// which would decode to bytes no filename may hold).
fn escape_preserved(c: char) -> bool {
    c == '\u{FF0F}' || ('\u{2400}'..='\u{241F}').contains(&c)
}

/// `␡` (U+2421) stands for DEL, which rclone *does* decode — so a literal
/// one has to be escaped on the way out or it would read back as DEL.
fn needs_escaping(c: char) -> bool {
    c == '\u{2421}'
}

/// Cheap pre-check: `false` means the name is already its own standard
/// form and [`to_standard_name`] would return it unchanged.
///
/// Worth having because the overwhelmingly common case is a name made of
/// characters rclone's encoder never looks at, and a paste asks this once
/// per selected file.
pub fn needs_translation(name: &str) -> bool {
    name.chars().any(|c| {
        c == QUOTE
            || decodable(c).is_some()
            || trailing_decodable(c).is_some()
            || escape_preserved(c)
            || needs_escaping(c)
    })
}

/// Convert one path *component* from its on-disk form to the name rclone
/// uses internally.
///
/// This is the name to write into a `--files-from` list and the leaf to
/// put on an rclone command line. It is not a display name — never show
/// the result to the user, whose file is still called what Explorer says.
pub fn to_standard_name(name: &str) -> String {
    if !needs_translation(name) {
        return name.to_string();
    }
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let last = i + 1 == chars.len();
        if c == QUOTE {
            match chars.get(i + 1) {
                // `‛＂` is how the encoder writes a literal `＂`; unwrap it.
                Some(&n) if decodable(n).is_some() => {
                    out.push(n);
                    i += 2;
                }
                // `‛／`, `‛␀` — rclone keeps these escapes intact.
                Some(&n) if escape_preserved(n) => {
                    out.push(QUOTE);
                    out.push(n);
                    i += 2;
                }
                // `‛‛` already denotes one literal quote rune and stays as
                // it is; a lone `‛` is doubled to become one.
                Some(&n) if n == QUOTE => {
                    out.push(QUOTE);
                    out.push(QUOTE);
                    i += 2;
                }
                _ => {
                    out.push(QUOTE);
                    out.push(QUOTE);
                    i += 1;
                }
            }
            continue;
        }
        if let Some(d) = decodable(c) {
            out.push(d);
        } else if last && trailing_decodable(c).is_some() {
            out.push(trailing_decodable(c).unwrap());
        } else if needs_escaping(c) {
            out.push(QUOTE);
            out.push(c);
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Apply [`to_standard_name`] to every component of a local path.
///
/// Separators are ASCII and no rule touches ASCII, so they pass through
/// untouched along with the drive letter, the `\\?\` prefix and a UNC
/// `\\host\share` — but the split still has to happen, because the
/// trailing-position rules are defined per component, not per path.
pub fn to_standard_path(path: &str) -> String {
    if !needs_translation(path) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 8);
    let mut component = String::new();
    for c in path.chars() {
        if c == '\\' || c == '/' {
            out.push_str(&to_standard_name(&component));
            component.clear();
            out.push(c);
        } else {
            component.push(c);
        }
    }
    out.push_str(&to_standard_name(&component));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pair here was read off rclone 1.73.5: create the file, run
    /// `rclone lsf`, record what came back. `tests/encoding.rs` re-derives
    /// them from a live binary; these keep the module honest with no
    /// rclone on PATH.
    #[test]
    fn full_width_forms_decode_to_ascii() {
        assert_eq!(to_standard_name("a\u{FF5C}b"), "a|b");
        assert_eq!(to_standard_name("a\u{FF1A}b"), "a:b");
        assert_eq!(to_standard_name("a\u{FF1F}b"), "a?b");
        assert_eq!(to_standard_name("a\u{FF0A}b"), "a*b");
        assert_eq!(to_standard_name("a\u{FF1C}b"), "a<b");
        assert_eq!(to_standard_name("a\u{FF1E}b"), "a>b");
        assert_eq!(to_standard_name("a\u{FF3C}b"), "a\\b");
        assert_eq!(to_standard_name("\u{FF02}q\u{FF02}"), "\"q\"");
    }

    /// Decoding `／` would split one name into two path components, so
    /// rclone leaves it — and leaves an escape in front of it alone too.
    #[test]
    fn full_width_slash_is_never_decoded() {
        assert_eq!(to_standard_name("a\u{FF0F}b"), "a\u{FF0F}b");
        assert_eq!(to_standard_name("a\u{201B}\u{FF0F}b"), "a\u{201B}\u{FF0F}b");
    }

    /// The case the user's library is full of: yt-dlp writes a full-width
    /// twin, a mixed-encoding copy bakes rclone's escape in front of it,
    /// and the name that comes back has to lose the escape again.
    #[test]
    fn escaped_full_width_unwraps_to_the_literal() {
        assert_eq!(to_standard_name("a\u{201B}\u{FF5C}b"), "a\u{FF5C}b");
        assert_eq!(to_standard_name("a\u{201B}\u{FF02}b"), "a\u{FF02}b");
        assert_eq!(
            to_standard_name("\u{201B}\u{FF02}Get Out\u{201B}\u{FF02} [x].mp3"),
            "\u{FF02}Get Out\u{FF02} [x].mp3"
        );
    }

    /// A quote rune that isn't escaping anything is itself a literal, and
    /// the standard form doubles it. `‛‛` already means one and stays put
    /// — which makes `a‛b` and `a‛‛b` share a standard name. That is
    /// rclone's ambiguity, not ours; the point of the test is that we
    /// reproduce it rather than invent a third answer.
    #[test]
    fn a_lone_quote_rune_is_doubled_and_a_doubled_one_is_left_alone() {
        assert_eq!(to_standard_name("a\u{201B}b"), "a\u{201B}\u{201B}b");
        assert_eq!(to_standard_name("a\u{201B}\u{201B}b"), "a\u{201B}\u{201B}b");
        assert_eq!(to_standard_name("ab\u{201B}"), "ab\u{201B}\u{201B}");
        assert_eq!(to_standard_name("\u{201B}"), "\u{201B}\u{201B}");
    }

    /// Control-symbol escapes survive intact; `␡` (DEL) is decodable, so a
    /// literal one gains an escape instead.
    #[test]
    fn control_symbols_keep_their_escape_and_del_gains_one() {
        assert_eq!(to_standard_name("a\u{2400}b"), "a\u{2400}b");
        assert_eq!(to_standard_name("a\u{201B}\u{2400}b"), "a\u{201B}\u{2400}b");
        assert_eq!(to_standard_name("a\u{241F}b"), "a\u{241F}b");
        assert_eq!(to_standard_name("a\u{2421}b"), "a\u{201B}\u{2421}b");
        assert_eq!(
            to_standard_name("a\u{201B}\u{2421}b"),
            "a\u{201B}\u{201B}\u{201B}\u{2421}b"
        );
    }

    /// `␠` and `．` stand in for a trailing space / period, which Windows
    /// will not store — so they only decode in the last position.
    #[test]
    fn space_and_period_symbols_decode_only_at_the_end() {
        assert_eq!(to_standard_name("x\u{2420}"), "x ");
        assert_eq!(to_standard_name("x\u{FF0E}"), "x.");
        assert_eq!(to_standard_name("a\u{2420}z"), "a\u{2420}z");
        assert_eq!(to_standard_name("a\u{FF0E}z"), "a\u{FF0E}z");
    }

    /// The overwhelmingly common case must be free and byte-identical.
    #[test]
    fn ordinary_names_pass_through_untouched() {
        for n in [
            "song.mp3",
            "Danyèl Waro - Réunion.mp3",
            "Māori Haka [JddEXEJ8_S0].mp3",
            "Vitas \u{29F8} Nirvana.mp3",
            "10 Famous Composers (Vol. 2).mp3",
        ] {
            assert!(!needs_translation(n), "{n} should need no translation");
            assert_eq!(to_standard_name(n), n);
        }
    }

    /// Separators, the drive colon and the `\\?\` prefix are ASCII, and no
    /// rule touches ASCII — but each component still gets its own trailing
    /// context.
    #[test]
    fn paths_translate_per_component() {
        assert_eq!(
            to_standard_path("O:\\radio\\a\u{201B}\u{FF5C}b\\c\u{FF1F}d.mp3"),
            "O:\\radio\\a\u{FF5C}b\\c?d.mp3"
        );
        assert_eq!(
            to_standard_path("C:\\plain\\path.mp3"),
            "C:\\plain\\path.mp3"
        );
        // A `␠` that is trailing *within its component* decodes; the same
        // rune mid-component does not.
        assert_eq!(
            to_standard_path("C:\\dir\u{2420}\\a\u{2420}z.mp3"),
            "C:\\dir \\a\u{2420}z.mp3"
        );
        assert_eq!(
            to_standard_path("\\\\?\\C:\\a\u{FF5C}b.mp3"),
            "\\\\?\\C:\\a|b.mp3"
        );
    }

    /// A drive letter's colon is ASCII and must not be mistaken for an
    /// encoded one — turning `O:` into anything else would make every
    /// local path unreachable.
    #[test]
    fn ascii_is_never_rewritten() {
        assert_eq!(to_standard_path("O:\\radio\\songs"), "O:\\radio\\songs");
        assert_eq!(to_standard_name("already|piped.mp3"), "already|piped.mp3");
        assert_eq!(to_standard_name("a:b*c?d.mp3"), "a:b*c?d.mp3");
    }
}
