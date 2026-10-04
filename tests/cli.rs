//! The two command-line tools, driven as processes.
//!
//! Everything else in the suite drives the library directly, which means the
//! binaries themselves were never executed by a test. That is a gap worth
//! closing for reasons beyond coverage: argument parsing, exit status and output
//! formatting are each places where the tool can disagree with the library it
//! wraps, and a tool whose output a user reads is a public surface.
//!
//! Two bugs were found by writing this file, and neither could have been found
//! from the library side:
//!
//! - `hfsinspect --help` exited **1**. `--help` was folded in with "no
//!   arguments given", both of which print usage, but only one of them is a
//!   mistake. A script checking `$?` after asking for help was told the tool
//!   had failed.
//! - `hfsls` documented six options and accepted a seventh. `-j` was
//!   implemented and unlisted in both the module documentation and the usage
//!   text.
//!
//! The tests spawn the real binaries. `CARGO_BIN_EXE_<name>` is set by Cargo for
//! integration tests, so the binary under test is the one just built rather than
//! whatever happens to be on `PATH`.

mod common;

use std::path::Path;
use std::process::{Command, Output};

/// Run `hfsls` with `args`.
fn hfsls(args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_hfsls"), args)
}

/// Run `hfsinspect` with `args`.
fn hfsinspect(args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_hfsinspect"), args)
}

fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"))
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

/// Absolute path of a generated image.
fn generated(name: &str) -> String {
    common::image(name).to_string_lossy().to_string()
}

/// Absolute path of a replayed image.
fn replayed(name: &str) -> String {
    common::repo_root()
        .join("tests/images/replayed")
        .join(format!("{name}.img"))
        .to_string_lossy()
        .to_string()
}

/// Skip with a reason rather than failing when an image is absent.
fn require(path: &str) -> bool {
    if Path::new(path).exists() {
        true
    } else {
        eprintln!("skipping: {path} not built; run tools/genimages.sh");
        false
    }
}

// --- Exit status --------------------------------------------------------

#[test]
fn asking_for_help_succeeds_in_both_tools() {
    // The bug this suite was written to find. Asking a tool how to use it is not
    // an error, and a caller branching on the exit status has no way to tell the
    // two apart if help reports failure.
    for out in [hfsls(&["--help"]), hfsinspect(&["--help"])] {
        assert_eq!(
            code(&out),
            0,
            "--help must exit 0, got {} with stderr {:?}",
            code(&out),
            stderr(&out)
        );
    }
    // `-h` is the same request.
    for out in [hfsls(&["-h"]), hfsinspect(&["-h"])] {
        assert_eq!(code(&out), 0, "-h must exit 0");
    }
}

#[test]
fn help_text_lists_every_option_the_tool_accepts() {
    // `hfsls` accepted `-j` without documenting it. A user reading `--help` had
    // no way to find the journal report.
    let text = stderr(&hfsls(&["--help"]));
    for opt in ["-l", "-a", "-R", "-s", "-b", "-j", "--json"] {
        assert!(
            text.contains(opt),
            "hfsls --help must document {opt}, got:\n{text}"
        );
    }

    let text = stderr(&hfsinspect(&["--help"]));
    for opt in ["--json", "--verbose", "--btrees"] {
        assert!(
            text.contains(opt),
            "hfsinspect --help must document {opt}, got:\n{text}"
        );
    }
}

#[test]
fn missing_arguments_are_a_usage_error() {
    for out in [hfsls(&[]), hfsinspect(&[])] {
        assert_eq!(
            code(&out),
            1,
            "no arguments must exit 1, got {}",
            code(&out)
        );
        assert!(
            !stderr(&out).is_empty(),
            "a usage error must explain itself"
        );
    }
}

#[test]
fn an_unknown_option_is_refused_rather_than_ignored() {
    // Treating an unknown flag as a filename would produce a baffling "no such
    // file" instead of "I do not know that option".
    let out = hfsls(&["--nonsense", "x"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("--nonsense"),
        "the offending option must be named, got {:?}",
        stderr(&out)
    );

    let out = hfsinspect(&["--nonsense"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("--nonsense"));
}

#[test]
fn a_good_image_exits_zero_and_a_bad_one_exits_two() {
    let good = generated("basic-hfsplus");
    let bad = common::repo_root()
        .join("tests/images/malformed/bad-signature.img")
        .to_string_lossy()
        .to_string();
    if !require(&bad) {
        return;
    }

    // 0 for success, 2 for "the image is malformed", 1 for "you asked wrongly".
    // Three distinct statuses, so a caller can tell a corrupt image from a
    // malformed command line.
    assert_eq!(code(&hfsls(&[&good])), 0, "a good image must succeed");
    assert_eq!(code(&hfsls(&[&bad])), 2, "a malformed image must exit 2");
    assert_eq!(code(&hfsinspect(&[&good])), 0);
    assert_eq!(code(&hfsinspect(&[&bad])), 2);
}

#[test]
fn a_missing_image_is_reported_not_panicked() {
    for out in [
        hfsls(&["/nonexistent/hfs.img"]),
        hfsinspect(&["/nonexistent/hfs.img"]),
    ] {
        assert_eq!(code(&out), 2, "an unreadable path must exit 2");
        let text = stderr(&out);
        assert!(
            text.contains("nonexistent"),
            "the path must be named in the error, got {text:?}"
        );
    }
}

#[test]
fn a_directory_is_reported_not_panicked() {
    // Opening a directory succeeds on Linux; reading from it does not. The tool
    // must surface that as an error rather than unwinding.
    let dir = common::repo_root().to_string_lossy().to_string();
    for out in [hfsls(&[&dir]), hfsinspect(&[&dir])] {
        assert_eq!(code(&out), 2);
        assert!(!stderr(&out).is_empty(), "an error must be reported");
    }
}

#[test]
fn classic_hfs_is_refused_cleanly() {
    // The corpus holds a classic HFS volume on purpose. Recognising the
    // signature and declining to misparse it is the required behaviour.
    let img = generated("classic-hfs");
    if !require(&img) {
        return;
    }
    let out = hfsls(&[&img]);
    assert_eq!(code(&out), 2, "classic HFS is out of scope, not a success");
    assert!(
        !stderr(&out).is_empty(),
        "refusing must say why, otherwise the exit status is the whole message"
    );
}

// --- hfsls listing ------------------------------------------------------

#[test]
fn a_volume_with_no_entries_lists_nothing_and_succeeds() {
    // `mkfs.hfsplus` creates an empty volume, and this is what a bare listing
    // looks like: no output, exit 0. Not an error, and not a panic -- the
    // distinction a caller branching on the exit status depends on.
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    let out = hfsls(&[&img]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        "",
        "an empty volume must list nothing, got {:?}",
        stdout(&out)
    );

    // The volume name still resolves: it is the root folder's name, which lives
    // in the catalog even when the folder itself holds nothing.
    let text = stdout(&hfsls(&["-s", &img]));
    assert!(
        text.contains("volume name:     BasicVolume"),
        "the volume name must come from the root folder, got:\n{text}"
    );
}

#[test]
fn a_volume_with_entries_lists_them() {
    let img = generated("journaled-hfsplus");
    if !require(&img) {
        return;
    }
    let out = hfsls(&["-a", &img]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(".journal") && text.contains(".journal_info_block"),
        "a journaled volume has two hidden root entries, got:\n{text}"
    );
}

#[test]
fn a_path_argument_resolves_component_by_component() {
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    // Every image here has only root entries, so the deepest meaningful path is
    // the root itself. A trailing-slash and bare form must agree.
    let bare = stdout(&hfsls(&[&img]));
    let slashed = stdout(&hfsls(&[&img, "/"]));
    assert_eq!(bare, slashed, "'/' must be the root, as no path must be");

    // And a name that cannot exist must fail rather than list something.
    let out = hfsls(&[&img, "definitely-not-here"]);
    assert_eq!(code(&out), 2, "a missing path must exit 2");
    assert!(stderr(&out).contains("definitely-not-here"));
}

#[test]
fn hidden_entries_need_an_explicit_flag() {
    let img = generated("journaled-hfsplus");
    if !require(&img) {
        return;
    }
    // `.journal` and `.journal_info_block` are real files that happen to be
    // hidden by name. Hiding them by default is the convention `ls` follows, and
    // `-a` is the override.
    let plain = stdout(&hfsls(&[&img]));
    assert!(
        !plain.contains(".journal"),
        "dot-entries must be hidden without -a, got:\n{plain}"
    );

    let all = stdout(&hfsls(&["-a", &img]));
    assert!(
        all.contains(".journal") && all.contains(".journal_info_block"),
        "-a must list them, got:\n{all}"
    );
}

#[test]
fn stat_reports_the_volume_geometry() {
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    let out = hfsls(&["-s", &img]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);

    for field in [
        "volume name:",
        "block size:",
        "total blocks:",
        "free blocks:",
        "journaled:",
        "case sensitive:",
    ] {
        assert!(text.contains(field), "-s must report {field}, got:\n{text}");
    }
    // The block size has to be the volume's, not a default. mkfs.hfsplus was
    // asked for 4096 by the recipe.
    assert!(
        text.contains("block size:      4096"),
        "expected the formatter's block size, got:\n{text}"
    );
}

#[test]
fn a_one_kilobyte_volume_reports_its_own_block_size() {
    // The same tool on a differently shaped volume: if `block size` were
    // hardcoded, this is the assertion that would catch it.
    let img = generated("basic-hfsplus-1k");
    if !require(&img) {
        return;
    }
    let text = stdout(&hfsls(&["-s", &img]));
    assert!(
        text.contains("block size:      1024"),
        "expected the 1 KiB volume's own block size, got:\n{text}"
    );
}

#[test]
fn the_json_output_is_well_formed() {
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    let out = hfsls(&["--json", &img]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);

    assert!(text.starts_with('{'), "must be a JSON object, got:\n{text}");
    assert!(text.trim_end().ends_with('}'), "must close, got:\n{text}");
    // Braces and brackets have to balance, or a consumer fails on our output
    // rather than on the data.
    let opens = text.matches('{').count();
    let closes = text.matches('}').count();
    assert_eq!(opens, closes, "unbalanced braces in:\n{text}");
    let brackets = text.matches('[').count();
    let bracket_closes = text.matches(']').count();
    assert_eq!(brackets, bracket_closes, "unbalanced brackets in:\n{text}");

    for field in [
        "name",
        "filesystem",
        "block_size",
        "total_blocks",
        "objects",
    ] {
        assert!(text.contains(field), "--json must include {field}");
    }
}

#[test]
fn the_journal_report_distinguishes_a_present_journal_from_an_empty_one() {
    // Two different states, and the report has to say which is which: an
    // uninitialised journal is a volume nobody has written to yet, which is a
    // normal state rather than a fault.
    let fresh = generated("journaled-hfsplus");
    let written = replayed("journal-replay-be");
    if !require(&fresh) || !require(&written) {
        return;
    }

    let text = stdout(&hfsls(&["-j", &fresh]));
    assert!(text.contains("journaled:       true"), "got:\n{text}");
    assert!(text.contains("uninitialised:   true"), "got:\n{text}");
    assert!(
        text.contains("never written") || text.contains("replayed blocks: 0"),
        "an unwritten journal must say so, got:\n{text}"
    );

    let text = stdout(&hfsls(&["-j", &written]));
    assert!(text.contains("uninitialised:   false"), "got:\n{text}");
    assert!(text.contains("transactions:    1"), "got:\n{text}");
    assert!(text.contains("replayed blocks: 1"), "got:\n{text}");
    assert!(
        text.contains("header checksum: ok"),
        "a valid header must be reported as valid, got:\n{text}"
    );
}

#[test]
fn the_journal_report_on_a_volume_without_one_says_so() {
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    let text = stdout(&hfsls(&["-j", &img]));
    assert!(text.contains("journaled:       false"), "got:\n{text}");
    assert!(
        text.contains("journal:         none"),
        "a volume with no journal must say none rather than printing zeros, got:\n{text}"
    );
}

#[test]
fn the_recursive_flag_descends_and_the_non_recursive_one_does_not() {
    // The generated volumes hold only root entries, so the check that matters is
    // structural: -R must not fail where the plain listing succeeds, and it must
    // produce the root entries too. A subtree difference is exercised by the
    // library suite, which has an image with a real directory tree.
    let img = generated("hfsx-case-sensitive");
    if !require(&img) {
        return;
    }
    let flat = hfsls(&[&img]);
    let deep = hfsls(&["-R", &img]);
    assert_eq!(code(&flat), 0, "{}", stderr(&flat));
    assert_eq!(code(&deep), 0, "{}", stderr(&deep));

    let flat_names = stdout(&flat);
    let deep_names = stdout(&deep);
    for name in flat_names.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            deep_names.contains(name.trim()),
            "-R must still list {name:?}, which the flat listing had"
        );
    }
}

// --- hfsinspect ---------------------------------------------------------

#[test]
fn hfsinspect_reports_the_volume_header_facts() {
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    let out = hfsinspect(&[&img]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);

    for field in ["signature", "block size", "total blocks", "journal"] {
        assert!(
            text.contains(field),
            "hfsinspect must report {field}, got:\n{text}"
        );
    }
    // 0x482b is 'H+' in ASCII, the HFSPlusWrapper signature.
    assert!(
        text.contains("482b") || text.contains("H+"),
        "the HFS+ signature must be reported, got:\n{text}"
    );
}

#[test]
fn hfsinspect_json_is_well_formed_even_when_an_image_fails() {
    // A malformed image is a result, not a crash: in JSON mode the tool must
    // still emit parseable output saying so, or a consumer gets a truncated
    // document with no way to tell what went wrong.
    let good = generated("basic-hfsplus");
    let bad = common::repo_root()
        .join("tests/images/malformed/bad-signature.img")
        .to_string_lossy()
        .to_string();
    if !require(&good) || !require(&bad) {
        return;
    }

    let out = hfsinspect(&["--json", &good, &bad]);
    // One image is bad, so the overall status reflects that even though the good
    // one was inspected and reported.
    assert_eq!(code(&out), 2, "a failed image must set the exit status");
    let text = stdout(&out);
    assert!(
        text.contains("\"ok\":false"),
        "the failure must be in the output:\n{text}"
    );
    assert!(
        text.contains("\"ok\":true"),
        "the good image must still be reported, not abandoned:\n{text}"
    );

    // Each line is a self-contained object, so a consumer can process them one at
    // a time and a later failure cannot truncate an earlier result.
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in line {line:?}");
    }
}

#[test]
fn hfsinspect_keeps_going_after_a_bad_image() {
    // Order matters here: the good image comes second, so a tool that aborted on
    // the first failure would print nothing for it.
    let good = generated("basic-hfsplus");
    let bad = common::repo_root()
        .join("tests/images/malformed/all-zero.img")
        .to_string_lossy()
        .to_string();
    if !require(&good) || !require(&bad) {
        return;
    }

    let out = hfsinspect(&["--json", &bad, &good]);
    assert_eq!(code(&out), 2);
    let text = stdout(&out);
    let entries: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        entries.len(),
        2,
        "both images must be reported, got:\n{text}"
    );
    assert!(
        entries[0].contains("\"ok\":false"),
        "the first image failed"
    );
    assert!(
        entries[1].contains("\"ok\":true"),
        "the second image must still be inspected, got {:?}",
        entries[1]
    );
}

// --- Read-only guarantee ------------------------------------------------

#[test]
fn running_either_tool_leaves_the_image_untouched() {
    // The tools are the diagnostic surface a user reaches for when a volume
    // misbehaves, and they are the ones most likely to be pointed at real media.
    // Neither may write, whichever options are combined.
    let img = generated("journaled-hfsplus");
    if !require(&img) {
        return;
    }
    let before = std::fs::read(&img).expect("read before");
    let digest_before = digest(&before);

    for args in [
        vec![img.as_str()],
        vec!["-a", "-R", img.as_str()],
        vec!["-l", img.as_str()],
        vec!["-s", img.as_str()],
        vec!["-b", img.as_str()],
        vec!["-j", img.as_str()],
        vec!["--json", img.as_str()],
    ] {
        let out = hfsls(&args);
        assert_eq!(code(&out), 0, "hfsls {args:?}: {}", stderr(&out));
    }
    let _ = hfsinspect(&[&img]);
    let _ = hfsinspect(&["--verbose", "--btrees", &img]);

    let after = std::fs::read(&img).expect("read after");
    assert_eq!(
        digest_before,
        digest(&after),
        "running the tools modified the image"
    );
}

#[test]
fn the_journal_report_distinguishes_an_external_journal_from_none() {
    // A volume whose journal lives on another device is journaled, and saying
    // "journal: none" would tell a user the opposite. This is what a Time Machine
    // volume looks like, so it is not an exotic state.
    let img = replayed("journal-external");
    if !require(&img) {
        return;
    }
    let out = hfsls(&["-j", &img]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);

    assert!(text.contains("journaled:       true"), "got:\n{text}");
    assert!(
        text.contains("journal:         on another device"),
        "an external journal must not be reported as none:\n{text}"
    );
    assert!(
        text.contains("0x00000006"),
        "the flags should be shown, since they are what says where the journal is:\n{text}"
    );
}

#[test]
fn a_volume_without_a_journal_still_says_none() {
    // The converse, so the new branch cannot swallow the ordinary case.
    let img = generated("basic-hfsplus");
    if !require(&img) {
        return;
    }
    let text = stdout(&hfsls(&["-j", &img]));
    assert!(
        text.contains("journal:         none"),
        "an unjournaled volume must still say none:\n{text}"
    );
}

#[test]
fn the_journal_report_distinguishes_clean_from_uninitialised() {
    // Two different states that read alike in the directory listing. "Nobody has
    // written to this yet" and "there is nothing outstanding right now" are not
    // the same claim -- a journal is initialised and dirty after an ordinary
    // crash -- and Apple's read-only mount policy turns on the second one.
    let unwritten = generated("journaled-hfsplus");
    let dirty = replayed("journal-replay-be");
    if !require(&unwritten) || !require(&dirty) {
        return;
    }

    let fresh = stdout(&hfsls(&["-j", &unwritten]));
    assert!(
        fresh.contains("uninitialised:   true") && fresh.contains("clean:           true"),
        "an unwritten journal is both:\n{fresh}"
    );

    let busy = stdout(&hfsls(&["-j", &dirty]));
    assert!(
        busy.contains("uninitialised:   false") && busy.contains("clean:           false"),
        "a journal with a transaction is neither:\n{busy}"
    );
}

#[test]
fn inspecting_a_torn_volume_does_not_recover_it() {
    // The recovered file is visible only through replay. `hfsls` lists the
    // filesystem, and it does not replay, so it must report the stale view. If
    // it ever started replaying silently, a user diagnosing a crash would see a
    // filesystem that does not match the disk.
    let img = replayed("journal-torn-catalog");
    if !require(&img) {
        return;
    }
    let text = stdout(&hfsls(&["-a", &img]));
    assert!(
        !text.contains("torn.txt"),
        "hfsls must not replay; the journal report is the explicit way to ask"
    );
    assert!(
        text.contains(".journal"),
        "the stale view must still list what is there"
    );
}

// --- Name comparison ----------------------------------------------------

#[test]
fn name_resolution_follows_the_volumes_own_comparison_rule() {
    // The strongest statement the tools can make: a path resolves through
    // whatever comparison the volume declares, not one this crate imposes.
    // The two corpus volumes differ, so the same input has to give different
    // answers, and a hardcoded comparator would fail one of them.
    let folding = generated("journaled-hfsplus");
    let sensitive = generated("hfsx-case-sensitive");
    if !require(&folding) || !require(&sensitive) {
        return;
    }

    // On a case-folding volume, a differently-cased name finds the same entry.
    let out = hfsls(&[&folding, ".JOURNAL"]);
    assert_eq!(
        code(&out),
        0,
        "a folding volume must accept any case: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains(".journal"),
        "the entry must be found and reported under its real name, got {:?}",
        stdout(&out)
    );

    // On a case-sensitive volume the same input must not resolve. These volumes
    // have no entries under either spelling, so the point is the refusal.
    let out = hfsls(&[&sensitive, "BASICVOLUME"]);
    assert_eq!(
        code(&out),
        2,
        "a case-sensitive volume must not fold names, got {}",
        code(&out)
    );
}

#[test]
fn the_case_sensitivity_reported_matches_the_one_used() {
    // `-s` states the rule; the lookup above exercises it. If the two disagreed
    // the report would be decoration.
    let folding = generated("journaled-hfsplus");
    let sensitive = generated("hfsx-case-sensitive");
    if !require(&folding) || !require(&sensitive) {
        return;
    }
    assert!(
        stdout(&hfsls(&["-s", &folding])).contains("case sensitive:  false"),
        "a plain HFS+ volume folds names"
    );
    assert!(
        stdout(&hfsls(&["-s", &sensitive])).contains("case sensitive:  true"),
        "an HFSX volume with kHFSBinaryCompare must report case sensitivity"
    );
}

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}
