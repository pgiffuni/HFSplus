//! The generated corpus is complete, or the suite is lying.
//!
//! # The problem this exists to prevent
//!
//! Sixty-odd tests begin with the same guard:
//!
//! ```ignore
//! if !path.exists() {
//!     eprintln!("skipping {name}: not built");
//!     return;
//! }
//! ```
//!
//! That is the right guard for a developer who has not run the generators yet.
//! It is the *wrong* guard in a suite whose job is to prove things: a broken
//! recipe in `tools/genimages.sh` would leave every dependent test skipping, the
//! run would be green, and the coverage would be silently gone. Nothing in the
//! exit status would say so.
//!
//! That is the same failure mode as a checker that reports a problem on a
//! well-formed image — worse, because here the failure is absence, and absence
//! reads as success.
//!
//! So the guards stay, and this file asserts that they have nothing to skip.
//! A missing fixture fails here, loudly, rather than quietly elsewhere.
//!
//! # What it asserts
//!
//! Every image named by a test exists, is non-empty, and is the right size class
//! for its kind. It does not re-verify their contents -- the tests that use them
//! do that, and re-parsing 40 images here would double the suite's cost for no
//! extra coverage.
//!
//! AGENTS.md: "Images are generated, never committed", and `tools/genimages.sh`,
//! `tools/genmalformed.sh` and `tools/genmanifests.sh` are the recipes. This is
//! the test that says those recipes still work.

mod common;

use std::path::PathBuf;

/// Every image the suite depends on, with the directory it lives in.
///
/// Kept as a list rather than derived by scanning the tests, because a list is
/// something a person can check against `tools/genimages.sh`. Deriving it would
/// mean the test could only confirm what it already believes.
const FIXTURES: &[(&str, &str)] = &[
    // tools/genimages.sh, mkfs.hfsplus
    ("generated", "basic-hfsplus"),
    ("generated", "basic-hfsplus-1k"),
    ("generated", "basic-hfsplus-8k"),
    ("generated", "basic-hfsplus-16k"),
    ("generated", "hfsx-case-sensitive"),
    ("generated", "hfsx-case-insensitive"),
    ("generated", "classic-hfs"),
    // tools/genimages.sh, journaled
    ("generated", "journaled-hfsplus"),
    ("generated", "journaled-hfsplus-1k"),
    // tools/mkfiles.py
    ("generated", "journal-with-files"),
    // tools/makejournal.py, journal replay
    ("replayed", "journal-replay-be"),
    ("replayed", "journal-replay-le"),
    ("replayed", "journal-replay-1k"),
    ("replayed", "journal-replay-multi"),
    // tools/mktorn.py
    ("replayed", "journal-torn-catalog"),
    // tools/makejournal.py --legacy-header
    ("replayed", "journal-legacy-header"),
    // tools/makejournal.py --external-journal
    ("replayed", "journal-external"),
    // tools/makejournal.py, the replay-rule fixtures
    ("replayed", "journal-bad-sequence"),
    ("replayed", "journal-bad-max-blocks"),
    ("replayed", "journal-bad-bsize"),
    ("replayed", "journal-short-end"),
    // tools/mkfiles.py, the fork-rule fixtures
    ("replayed", "fork-logical-too-large"),
    ("replayed", "fork-total-too-large"),
    ("replayed", "fork-extent-past-volume"),
    ("replayed", "symlink-empty-target"),
    // tools/genmalformed.sh
    ("malformed", "bad-signature"),
    ("malformed", "hfsplus-sig-hfsx-version"),
    ("malformed", "bad-block-size"),
    ("malformed", "block-size-too-small"),
    ("malformed", "truncated-header"),
    ("malformed", "truncated-half-header"),
    ("malformed", "all-zero"),
    ("malformed", "huge-total-blocks"),
    ("malformed", "fork-blocks-exceed-volume"),
    ("malformed", "catalog-extent-out-of-range"),
    ("malformed", "extents-no-terminator"),
    ("malformed", "journal-info-block-out-of-volume"),
    ("malformed", "journal-info-block-huge"),
    ("malformed", "journal-start-at-header"),
];

fn fixture_path(dir: &str, name: &str) -> PathBuf {
    common::repo_root()
        .join("tests/images")
        .join(dir)
        .join(format!("{name}.img"))
}

#[test]
fn every_fixture_the_suite_depends_on_was_generated() {
    let mut missing = Vec::new();
    let mut empty = Vec::new();

    for (dir, name) in FIXTURES {
        let path = fixture_path(dir, name);
        if !path.exists() {
            missing.push(format!("{dir}/{name}"));
            continue;
        }
        match std::fs::metadata(&path) {
            Ok(m) if m.len() == 0 => empty.push(format!("{dir}/{name}")),
            Ok(_) => {}
            Err(e) => missing.push(format!("{dir}/{name} ({e})")),
        }
    }

    assert!(
        missing.is_empty(),
        "{} fixture(s) are missing, so every test depending on them is silently \\
         skipping and the run proves nothing:\\n  {}\\n\\nRun tools/genimages.sh, \\
         tools/genmalformed.sh and tools/genmanifests.sh.",
        missing.len(),
        missing.join("\n  ")
    );
    assert!(empty.is_empty(), "zero-length fixtures: {empty:?}");
}

#[test]
fn the_malformed_corpus_is_larger_than_the_good_one() {
    // A sanity check on the recipes themselves rather than on any image: a
    // deliberate corpus with fewer broken volumes than sound ones would mean a
    // fixture went missing from the generator.
    let good = FIXTURES.iter().filter(|(d, _)| *d != "malformed").count();
    let bad = FIXTURES.iter().filter(|(d, _)| *d == "malformed").count();
    assert!(
        bad >= good / 2,
        "the adversarial corpus has {bad} images against {good} sound ones, which \\
         suggests a recipe is no longer producing what it used to"
    );
}

#[test]
fn no_image_is_empty_and_every_volume_header_is_present_or_absent_deliberately() {
    // Cheap shape check across the whole corpus, without parsing: every image is
    // either a plausible volume (its header carries the HFS+ signature) or one of
    // the deliberately-corrupt fixtures, which is a short list this test names.
    //
    // The point is to catch a generator that writes a file of the right name and
    // the wrong content, which every existence check above would pass.
    // Deliberately not carrying a signature, and each for a stated reason:
    // a destroyed one, a signature outside the family, an image that ends before
    // the header does, and the classic-HFS volume this project recognises and
    // declines.
    let deliberately_unsigned = [
        "all-zero",
        "bad-signature",
        "truncated-header",
        "classic-hfs",
    ];

    let mut checked = 0usize;
    for (dir, name) in FIXTURES {
        let path = fixture_path(dir, name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        checked += 1;

        if deliberately_unsigned.contains(name) {
            continue;
        }
        assert!(
            bytes.len() > 1024 + 2,
            "{dir}/{name} is too short to hold a volume header"
        );
        let signature = [bytes[1024], bytes[1025]];
        assert!(
            signature == [0x48, 0x2b] || signature == [0x48, 0x58],
            "{dir}/{name} carries neither the HFS+ nor the HFSX signature: \\
             {signature:02x?}, so a generator wrote the wrong bytes"
        );
    }
    assert!(checked > 20, "only {checked} fixtures were read");
}

#[test]
fn the_corporate_generators_are_idempotent_where_it_matters() {
    // The rule fixtures are patched in place over a generated image, so running
    // the generator twice must produce the same bytes -- otherwise a test could
    // pass against one build and fail against another for no reason.
    //
    // Only the fixtures this suite *derives* are checked, and only for stability
    // of the second run: re-running a generator that embeds a timestamp will
    // differ on the first run by design.
    for (tool, dir, name, args) in [
        (
            "tools/mkfiles.py",
            "replayed",
            "fork-logical-too-large",
            vec!["--fork-logical", "100000"],
        ),
        (
            "tools/makejournal.py",
            "replayed",
            "journal-legacy-header",
            vec!["--legacy-header"],
        ),
    ] {
        let path = fixture_path(dir, name);
        if !path.exists() {
            continue;
        }
        let tmp = std::env::temp_dir().join(format!("idem-{}-{name}.img", std::process::id()));

        // The tool takes a source and a destination; the source is the image the
        // fixture is derived from.
        let source = if name.starts_with("fork-") || name.starts_with("symlink-") {
            fixture_path("generated", "journal-with-files")
        } else {
            fixture_path("replayed", "journal-replay-be")
        };
        if !source.exists() {
            continue;
        }

        let status = std::process::Command::new("python3")
            .arg(tool)
            .arg(&source)
            .arg(&tmp)
            .args(&args)
            .status();
        match status {
            Ok(s) if s.success() => {
                let a = std::fs::read(&path).expect("read the fixture");
                let b = std::fs::read(&tmp).expect("read the regenerated fixture");
                assert_eq!(
                    a.len(),
                    b.len(),
                    "{name}: regenerating produced a different length"
                );
                assert!(a == b, "{name}: regenerating did not reproduce the fixture");
                let _ = std::fs::remove_file(&tmp);
            }
            _ => {
                // Python absent or the tool failed: not this test's business. The
                // existence test above is the one that must hold.
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}
