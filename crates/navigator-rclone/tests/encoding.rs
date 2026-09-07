//! Differential test: `navigator_rclone::encoding` versus a live rclone.
//!
//! The module reimplements one direction of rclone's filename encoder, so
//! the only defence against drift is asking rclone. Each case creates a
//! file with a known on-disk name, runs `rclone lsf` on the directory, and
//! compares the name rclone reports against [`to_standard_name`].
//!
//! Skipped when rclone is not on PATH — the unit tests in the module pin
//! the same table without it.

#![cfg(windows)]

use navigator_rclone::encoding::to_standard_name;

fn rclone_available() -> bool {
    std::process::Command::new("rclone")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create every `name` in one directory, then ask rclone what it calls
/// them. Returns `(os_name, rclone_name)` pairs.
///
/// Names are tagged with an ASCII prefix so the two listings can be
/// correlated: the interesting runes are exactly the ones that change, so
/// matching on the name itself would be circular.
fn ask_rclone(names: &[String]) -> Vec<(String, String)> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut tagged = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let tag = format!("T{i:04}");
        let full = format!("{tag}{n}");
        std::fs::write(dir.path().join(&full), b"x").expect("write fixture");
        tagged.push((tag, n.clone()));
    }

    let out = std::process::Command::new("rclone")
        .arg("lsf")
        .arg(dir.path())
        .output()
        .expect("rclone lsf");
    assert!(out.status.success(), "rclone lsf failed");
    let listing = String::from_utf8(out.stdout).expect("rclone lsf emits UTF-8");

    tagged
        .into_iter()
        .map(|(tag, os)| {
            let seen = listing
                .lines()
                .find(|l| l.starts_with(&tag))
                .unwrap_or_else(|| panic!("rclone never listed {tag} (os name {os:?})"));
            (os, seen[tag.len()..].to_string())
        })
        .collect()
}

fn check(cases: &[String]) {
    if !rclone_available() {
        eprintln!("rclone not on PATH — skipping");
        return;
    }
    let mut wrong = Vec::new();
    for (os, rclone_says) in ask_rclone(cases) {
        let ours = to_standard_name(&os);
        if ours != rclone_says {
            wrong.push(format!(
                "  {:?}\n      rclone: {:?}\n      ours:   {:?}",
                os, rclone_says, ours
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} names disagree with rclone:\n{}",
        wrong.len(),
        cases.len(),
        wrong.join("\n")
    );
}

/// Every rune rclone's Windows local encoder can touch, plus fillers that
/// put the interesting ones in a mid-name and a trailing position.
const ALPHABET: &[char] = &[
    '\u{201B}', // the quote rune
    '\u{FF02}', '\u{FF0A}', '\u{FF0F}', '\u{FF1A}', '\u{FF1C}', '\u{FF1E}', '\u{FF1F}', '\u{FF3C}',
    '\u{FF5C}', // full-width " * / : < > ? \ |
    '\u{FF0E}', // full-width period (trailing rule)
    '\u{2400}', '\u{241F}', // control-character symbols
    '\u{2420}', // space symbol (trailing rule)
    '\u{2421}', // DEL symbol
    '\u{2422}', // not an encoded form — the control case
    'a',
];

/// Exhaustive over every ordered pair from [`ALPHABET`], in a mid-name
/// position. This is where the quote-rune interactions live: `‛` in front
/// of a decodable form unwraps, in front of a preserved form stays, and in
/// front of anything else doubles.
#[test]
fn every_rune_pair_matches_rclone_mid_name() {
    let mut cases = Vec::new();
    for &p in ALPHABET {
        for &q in ALPHABET {
            cases.push(format!("{p}{q}z"));
        }
    }
    check(&cases);
}

/// The same alphabet in the last position, where `␠` and `．` decode to a
/// trailing space / period and nothing else changes behaviour.
#[test]
fn every_rune_matches_rclone_in_the_trailing_position() {
    // A real trailing space or period is not storable on Windows — the
    // filesystem strips it — so those two are left out rather than
    // producing a fixture whose name is not what we asked for.
    let cases: Vec<String> = ALPHABET
        .iter()
        .filter(|c| **c != ' ' && **c != '.')
        .map(|c| format!("z{c}"))
        .collect();
    check(&cases);
}

/// The names this whole module was written for: a folder of yt-dlp
/// downloads that a mixed-encoding copy has stamped rclone's escape onto.
/// Verbatim from the failing directory.
#[test]
fn real_world_yt_dlp_names_match_rclone() {
    let cases: Vec<String> = [
        "\u{201B}\u{FF02}Get Out Of My Car\u{201B}\u{FF02} [Musical Remake] [xnmhuq1NdJA].mp3",
        "\u{201B}\u{FF02}Weird Arby's Guy\u{201B}\u{FF02} \u{201B}\u{FF5C} The Musical [nJ_WdPEccmE].mp3",
        "'Have you ever had a dream\u{201B}\u{FF1F}' goes METAL! [PJ7vHDKOWXI].mp3",
        "10 Famous Classic Composers Rock Medley \u{201B}\u{FF5C} Andre Antunes [FxNAwnTDrWc].mp3",
        "Remixing the World\u{201B}\u{FF1A} Dany\u{E8}l Waro from R\u{E9}union Island [lGxIMKs-u_M].mp3",
        // Already-clean neighbours that must not be disturbed.
        "Vitas Meets Nirvana - Smells Like Teen Spirit \u{29F8} 7th Element Mashup [-x_WVQllXoA].mp3",
        "M\u{101}ori Haka in NZ Parliament goes METAL! [JddEXEJ8_S0].mp3",
        "Andre Antunes - Ace Of Spades (Mot\u{F6}rhead) [A33pFNZArzM].mp3",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    check(&cases);
}

/// End-to-end proof that the translation is what makes a `--files-from`
/// batch work: the same list that silently transfers nothing when written
/// with on-disk names moves every file when written with translated ones.
///
/// This is the actual reported bug — exit 0, no error, no files.
#[test]
fn a_files_from_batch_moves_escaped_names_only_after_translation() {
    if !rclone_available() {
        eprintln!("rclone not on PATH — skipping");
        return;
    }
    let names = [
        "\u{201B}\u{FF02}Get Out\u{201B}\u{FF02} [x].mp3",
        "10 Famous \u{201B}\u{FF5C} Andre [y].mp3",
        "ordinary.mp3",
    ];

    let root = tempfile::tempdir().expect("tempdir");
    let src = root.path().join("src");
    std::fs::create_dir(&src).unwrap();
    for n in names {
        std::fs::write(src.join(n), b"x").unwrap();
    }

    let run = |list_body: String, dest: &std::path::Path| {
        let list = root.path().join("list.txt");
        std::fs::write(&list, list_body).unwrap();
        std::fs::create_dir_all(dest).unwrap();
        let out = std::process::Command::new("rclone")
            .arg("--files-from")
            .arg(&list)
            .arg("copy")
            .arg(&src)
            .arg(dest)
            .output()
            .expect("rclone copy");
        let landed = std::fs::read_dir(dest).unwrap().count();
        (out.status.success(), landed)
    };

    // Raw on-disk names: rclone reports success and moves only the name
    // its encoder leaves alone.
    let raw: String = names.iter().map(|n| format!("{n}\n")).collect();
    let (ok, landed) = run(raw, &root.path().join("dest_raw"));
    assert!(
        ok,
        "rclone exits 0 even though it transferred almost nothing"
    );
    assert_eq!(
        landed, 1,
        "untranslated names should silently lose everything but the plain one \
         — if this now moves 3, rclone changed and the translation may be moot"
    );

    // Translated names: everything arrives, under its original on-disk name.
    let translated: String = names
        .iter()
        .map(|n| format!("{}\n", to_standard_name(n)))
        .collect();
    let dest = root.path().join("dest_translated");
    let (ok, landed) = run(translated, &dest);
    assert!(ok, "translated batch must succeed");
    assert_eq!(landed, 3, "every file must arrive");
    for n in names {
        assert!(
            dest.join(n).exists(),
            "{n:?} must land under its original on-disk name"
        );
    }
}
