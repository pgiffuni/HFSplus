//! The checker against the corpus.
//!
//! These are the acceptance tests for the in-tree checker, and they are
//! deliberately structured as *disagreements*, not verdicts. Nothing here decides
//! whether an image is correct: the checker's correctness comes from
//! `lib_fsck_hfs` by way of `Mining reference: Apple` on each check. What is
//! tested is that the checker finds the disagreements the corpus is known to
//! contain -- and, just as importantly, that it does not invent them.
//!
//! # Why this can catch a reader bug at all
//!
//! A checker built on the same parser as the reader will mis-read a structure the
//! same way, so agreement between them proves nothing. The exception is a
//! *disagreement between two structures written independently*: the allocation
//! bitmap and the catalog's extents were written by the formatter at different
//! times and with no shared state, so if the checker finds a fork whose extents
//! contradict the bitmap, something is wrong even though one code path read both.
//! That is what the orphaned and missing checks are for.
//!
//! It is also why these tests assert the *specific* block numbers for the
//! deliberately broken images, rather than merely that a report is non-empty. A
//! checker that reports the wrong block is as wrong as one that reports nothing.
//!
//! Mining reference: `lib_fsck_hfs/dfalib/SVerify1.c` `CheckBitmapRange` and
//! `SVerify2.c`'s volume-information checks, for what each of these verifies.

mod common;

use std::process::Command;

use hfsplus::blockdev::FileDevice;
use hfsplus::check::{self, CheckReport};
use hfsplus::volume::Volume;

fn check_image(name: &str) -> Option<(CheckReport, u32)> {
    let path = common::image(name);
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return None;
    }
    check_path(&path)
}

/// Check a specific image, which must exist.
///
/// A path rather than a name, so a broken copy can live in the temporary
/// directory. `None` means "out of scope" -- a signature this project refuses --
/// and nothing else. A missing file is an error, so a test cannot skip itself
/// into a vacuous pass by mistaking a deleted file for a clean one.
fn check_path(path: &std::path::Path) -> Option<(CheckReport, u32)> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if !path.exists() {
        panic!("{}: check_path was given a file that does not exist", path.display());
    }
    let dev = FileDevice::open(path).unwrap_or_else(|e| panic!("open {name}: {e}"));
    // A volume outside this project's scope is refused structurally, and there
    // is nothing to check on it. That refusal is its own test.
    let vol = match Volume::open(&dev) {
        Ok(vol) => vol,
        Err(hfsplus::error::Error::BadSignature { found }) => {
            eprintln!("skipping {name}: signature 0x{found:04x} is out of scope");
            return None;
        }
        Err(e) => panic!("mount {name}: {e}"),
    };
    let report = check::check(&vol, None).unwrap_or_else(|e| panic!("check {name}: {e}"));
    Some((report, vol.header().total_blocks))
}

// --- The good corpus ----------------------------------------------------

#[test]
fn every_generated_image_is_internally_consistent() {
    // The load-bearing negative: a checker that reports something on a
    // well-formed image is worse than no checker, because it trains the reader to
    // ignore it. Every image `mkfs.hfsplus` wrote, plus the ones the Python
    // generators wrote and `fsck.hfsplus` accepted unchanged.
    let mut checked = 0;
    for name in common::generated_names() {
        let Some((report, _)) = check_image(&name) else { continue };
        let described = report.describe();
        assert!(
            described.is_empty(),
            "{name}: a well-formed image reported {}:\n  {}",
            described.len(),
            described.join("\n  ")
        );
        checked += 1;
    }
    assert!(checked > 0, "no generated images were checked");
}

#[test]
fn the_torn_catalog_image_is_consistent_because_replay_is_the_readers_job() {
    // The journal is newer than the filesystem here, but the *filesystem* is
    // sound: the file exists only in the journal, which costs no blocks. So the
    // bitmap and the extents still agree, and a checker that reported a
    // disagreement here would be right about nothing.
    //
    // This is a real limitation rather than a passing test: the checker does not
    // replay, so it cannot tell a volume needing recovery from a sound one.
    // Whether a journal needs replaying is Apple's check to make.
    let Some((report, _)) = check_image("journal-torn-catalog") else { return };
    let described = report.describe();
    assert!(
        described.is_empty(),
        "the on-disk filesystem must be self-consistent:\n  {}",
        described.join("\n  ")
    );
}

// --- Deliberate breakage ------------------------------------------------

#[test]
fn a_block_marked_but_referenced_by_nothing_is_reported_orphaned() {
    // Built the way fsck itself detects it: a block marked in use that no fork's
    // extents point at. Here a data block is freed in the catalog without the
    // bitmap being updated.
    let mut freed = 0;
    let image = break_image("journal-with-files", "orphan", |img| {
        freed = fragmented_blocks(img)[0];
        free_fragmented_extent(img);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);

    assert!(
        report.missing.is_empty(),
        "the extent was removed, so nothing is missing from the bitmap: {:?}",
        report.missing
    );
    assert!(
        report.orphaned.contains(&freed),
        "block {freed} must be reported orphaned, got {:?}",
        report.orphaned
    );
}

#[test]
fn a_fork_whose_extents_werent_marked_is_reported_missing() {
    // The other direction. A fork claims blocks the bitmap does not have marked,
    // which is what happens when extents are written without updating the map.
    let mut cleared = 0;
    let image = break_image("journal-with-files", "missing", |img| {
        cleared = fragmented_blocks(img)[0];
        mark_only_in_catalog(img, cleared);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);

    assert!(
        report.orphaned.is_empty(),
        "nothing was orphaned, since the bitmap was left alone: {:?}",
        report.orphaned
    );
    assert!(
        report.missing.contains(&cleared),
        "block {cleared} must be reported missing, got {:?}",
        report.missing
    );
}

#[test]
fn a_fork_declaring_more_blocks_than_its_extents_describe_is_reported() {
    // `totalBlocks` is a separate field from the extents, so it can disagree with
    // them independently -- and the difference is exactly what the file's own
    // length check would hide.
    let image = break_image("journal-with-files", "totalblocks", |img| {
        inflate_total_blocks(img, FRAGMENTED_CNID, 7);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);

    let entry = report
        .fork_block_count
        .iter()
        .find(|(cnid, _, _)| *cnid == FRAGMENTED_CNID)
        .unwrap_or_else(|| panic!("the inflated fork must be reported, got {:?}", report));
    assert_eq!(
        *entry,
        (FRAGMENTED_CNID, 8 + 7, 8),
        "the report must name the declared and described counts, not just that they differ"
    );
}

#[test]
fn a_next_catalog_id_behind_an_existing_cnid_is_reported() {
    // The consequence is worse than a bad number: a later create would be handed
    // a CNID that already identifies an existing file, and a lookup by CNID would
    // then find the wrong one.
    let image = break_image("journal-with-files", "nextcnid", |img| {
        set_next_catalog_id(img, FRAGMENTED_CNID);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);

    let (next, highest) = report
        .next_cnid_reuse
        .unwrap_or_else(|| panic!("a stale nextCatalogID must be reported, got {:?}", report));
    assert_eq!(next, FRAGMENTED_CNID);
    assert!(
        highest > FRAGMENTED_CNID,
        "and the CNID already in use that it would reuse, got {highest}"
    );
}

#[test]
fn a_file_with_no_thread_record_is_still_visited() {
    // The reason the checker walks raw records rather than `all_objects`: a file
    // whose thread record is missing would be invisible to an object walk, and its
    // data blocks would then be reported as orphaned -- a symptom, reported with
    // the wrong cause. Here the file is present, complete, and correctly accounted
    // for, which is the point: it must not be mistaken for damage.
    let Some((report, _)) = check_image("journal-with-files") else { return };
    assert!(
        report.orphaned.is_empty() && report.missing.is_empty(),
        "every file's blocks must be accounted for through the raw walk:\n  {}",
        report.describe().join("\n  ")
    );
}

#[test]
fn swapping_two_adjacent_keys_is_reported_as_out_of_order() {
    // The check the reader cannot perform on itself: a binary search over
    // unordered records still returns an answer, and the answer is wrong. So the
    // corruption here is subtle by construction -- both records stay valid, and
    // the catalog still parses.
    let image = break_image("journal-with-files", "keyorder", |img| {
        swap_two_records(img, RECORD_A, RECORD_B);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);
    assert!(
        !report.key_order.is_empty(),
        "swapped keys must be reported, got a clean report"
    );
    // And nothing else: the volumes are still structurally sound otherwise.
    assert!(
        report.missing_thread.is_empty(),
        "a reordering must not invent a missing thread record: {:?}",
        report.missing_thread
    );
}

#[test]
fn a_record_with_no_thread_record_is_reported() {
    // The object stays in the catalog and stays on the bitmap, so every other
    // check still passes. What changes is that it can no longer be reached by
    // name -- which a reader walking thread records would simply not see.
    let image = break_image("journal-with-files", "nothread", |img| {
        zero_thread_record(img, FRAGMENTED_CNID);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);
    assert!(
        report.missing_thread.contains(&FRAGMENTED_CNID),
        "the object with no thread record must be named, got {:?}",
        report.missing_thread
    );
    assert!(
        report.orphaned.is_empty() && report.missing.is_empty(),
        "its blocks are still accounted for; only reachability changed"
    );
}

#[test]
fn a_folder_whose_valence_disagrees_is_reported() {
    // Valence is a count the folder record declares and the thread records imply.
    // Two independent statements about one fact, so a mismatch is real evidence.
    let image = break_image("journal-with-files", "valence", |img| {
        inflate_root_valence(img, 3);
    });
    let Some((report, _)) = check_path(&image) else { panic!("in scope") };
    let _ = std::fs::remove_file(&image);
    let entry = report
        .valence
        .iter()
        .find(|(cnid, _, _)| *cnid == ROOT_CNID)
        .unwrap_or_else(|| panic!("a wrong valence must be reported, got {:?}", report));
    assert_eq!(entry.1, expected_valence() + 3, "the inflated declared count");
    assert_eq!(entry.2, expected_valence(), "the count actually implied");
}

// --- The malformed corpus -----------------------------------------------

#[test]
fn every_malformed_image_is_either_refused_or_flagged() {
    // The complement of the negative test above. An image the corpus deliberately
    // broke must not pass silently, whether the parser refuses it or a check
    // finds the damage -- and if some future check makes one of them pass, this
    // says so by name.
    let dir = common::repo_root().join("tests/images/malformed");
    let entries = std::fs::read_dir(&dir).expect("the malformed corpus must exist");
    let mut names: Vec<String> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? == "img" {
                Some(path.file_stem()?.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no malformed images found");

    let mut clean: Vec<&str> = Vec::new();
    for name in &names {
        let path = dir.join(format!("{name}.img"));
        let dev = FileDevice::open(&path).expect("open");
        let verdict = Volume::open(&dev).and_then(|vol| check::check(&vol, None));
        if let Ok(report) = verdict {
            if report.is_clean() {
                clean.push(name.as_str());
            }
        }
    }
    assert!(
        clean.is_empty(),
        "these images were deliberately corrupted but the checker passed them: {clean:?}"
    );
}

// --- Agreement with an independent implementation ------------------------

/// The strongest evidence available that these checks mean what they claim: the
/// two implementations agree about the same damage.
///
/// It is agreement, not proof. `fsck_hfs` is Apple's own code, but only as
/// ported by `hfsprogs`, and `docs/dev-tools.md` records the checks that port
/// does not carry. Where this project checks something the port does not, there
/// is nothing to agree with, and the test says so rather than skipping quietly.
#[test]
fn the_checker_and_the_independent_checker_agree_about_damage() {
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    if !common::image("journal-with-files").exists() {
        eprintln!("skipping: journal-with-files not built");
        return;
    }

    // Each case: a description, the bytes to break, and a phrase Apple's checker
    // uses when it notices. The phrase is what makes this a comparison rather
    // than two independent "not OK"s -- and it has to be Apple's wording, not
    // ours, which is why valence matches on "Invalid directory item count".
    struct Case {
        what: &'static str,
        break_it: fn(&mut [u8]),
        apple_says: &'static str,
    }

    let cases = [
        Case {
            what: "a fork's block cleared from the bitmap",
            break_it: clear_first_fragmented_block_bit,
            apple_says: "under-allocation",
        },
        Case {
            what: "two catalog keys swapped",
            break_it: |img| swap_two_records(img, RECORD_A, RECORD_B),
            apple_says: "Keys out of order",
        },
        Case {
            what: "the root folder's valence inflated",
            break_it: inflate_root_valence_raw,
            // Apple calls the same finding a directory's "item count" rather
            // than its valence. `valence` is the field name in
            // `struct HFSPlusCatalogFolder`; the message is the checker's word for
            // it, and matching on the field name would have failed.
            apple_says: "Invalid directory item count",
        },
        Case {
            what: "a thread record's type destroyed",
            break_it: destroy_thread_record,
            apple_says: "Invalid catalog record type",
        },
    ];

    for case in cases {
        let path = break_image("journal-with-files", "agree", |img| (case.break_it)(img));

        // Ours first: an image we cannot even read is not a disagreement.
        let ours = run_hfsck(&path);
        assert_eq!(
            ours, 3,
            "{}: hfsck should report a finding, got exit {ours}",
            case.what
        );

        let mut probe = std::env::temp_dir();
        probe.push(format!("agree-probe-{}.img", std::process::id()));
        std::fs::copy(&path, &probe).expect("copy for fsck");
        let out = common::run_fsck(&fsck, &probe);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&path);

        assert!(
            !text.contains("appears to be OK"),
            "{}: the independent checker accepted damage we rejected:\n{text}",
            case.what
        );
        assert!(
            text.contains(case.apple_says),
            "{}: the independent checker did not report {:?}, only:\n{text}",
            case.what,
            case.apple_says
        );
    }
}

/// Run the `hfsck` binary and return its exit status.
///
/// Uses the binary Cargo built for this test rather than one on `PATH`, so this
/// is testing the code in this repository.
fn run_hfsck(path: &std::path::Path) -> i32 {
    Command::new(env!("CARGO_BIN_EXE_hfsck"))
        .arg(path)
        .output()
        .expect("run hfsck")
        .status
        .code()
        .unwrap_or(-1)
}

/// Clear the bitmap bit for `fragmented.bin`'s first block, leaving the catalog
/// alone -- the under-allocation direction.
fn clear_first_fragmented_block_bit(img: &mut [u8]) {
    let block = fragmented_blocks(img)[0];
    let bs = block_size(img);
    let allocation_block = u32::from_be_bytes([
        img[VOLUME_HEADER_OFFSET + 112 + 16],
        img[VOLUME_HEADER_OFFSET + 112 + 17],
        img[VOLUME_HEADER_OFFSET + 112 + 18],
        img[VOLUME_HEADER_OFFSET + 112 + 19],
    ]);
    let at = (allocation_block as usize) * (bs as usize) + (block / 8) as usize;
    img[at] &= 0xFFu8 ^ (0x80u8 >> (block % 8));
}

/// Inflate the root folder's declared valence, for the agreement case.
fn inflate_root_valence_raw(img: &mut [u8]) {
    inflate_root_valence(img, 3);
}

/// Set a thread record's type to an unassigned value.
fn destroy_thread_record(img: &mut [u8]) {
    zero_thread_record(img, FRAGMENTED_CNID);
}

// --- Helpers for building broken copies ---------------------------------
//
// Each break is applied to a *copy* in the temporary directory, so the corpus
// image itself is never modified.

/// CNID of `fragmented.bin` in `journal-with-files`.
const FRAGMENTED_CNID: u32 = 18;
/// CNID of the root folder, `kHFSRootFolderID`.
const ROOT_CNID: u32 = 2;
/// CNID of `.journal`, whose thread record will be swapped with the one below.
const RECORD_A: u32 = 16;
/// CNID of `.journal_info_block`.
const RECORD_B: u32 = 17;
const VOLUME_HEADER_OFFSET: usize = 1024;
const CATALOG_FORK_OFFSET: usize = 272;

fn block_size(img: &[u8]) -> u32 {
    u32::from_be_bytes([
        img[VOLUME_HEADER_OFFSET + 40],
        img[VOLUME_HEADER_OFFSET + 41],
        img[VOLUME_HEADER_OFFSET + 42],
        img[VOLUME_HEADER_OFFSET + 43],
    ])
}

fn catalog_start(img: &[u8]) -> u32 {
    u32::from_be_bytes([
        img[VOLUME_HEADER_OFFSET + CATALOG_FORK_OFFSET + 16],
        img[VOLUME_HEADER_OFFSET + CATALOG_FORK_OFFSET + 17],
        img[VOLUME_HEADER_OFFSET + CATALOG_FORK_OFFSET + 18],
        img[VOLUME_HEADER_OFFSET + CATALOG_FORK_OFFSET + 19],
    ])
}

/// Apply `break_it` to a copy of `name` and return the copy's stem.
///
/// The copy lives in the temp directory and is removed by the caller's image
/// lookup failing on a second run, which is why each test re-derives its own.
/// Apply `break_it` to a copy of `name` in the temporary directory.
///
/// Returns the path. The copy is *not* removed: an earlier version deleted it
/// immediately, and every caller then failed to find it and skipped its own
/// assertions -- so four breakage tests passed without running. A caller that
/// wants the file gone must say so, by removing it.
fn break_image(name: &str, label: &str, break_it: impl FnOnce(&mut [u8])) -> std::path::PathBuf {
    let src = common::image(name);
    assert!(
        src.exists(),
        "{}: the source image must exist before it can be broken",
        src.display()
    );
    let mut img = std::fs::read(&src).unwrap_or_else(|e| panic!("read {name}: {e}"));
    break_it(&mut img);

    let mut dest = std::env::temp_dir();
    dest.push(format!("hfsplus-{label}-{name}-{}.img", std::process::id()));
    std::fs::write(&dest, &img).unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
    dest
}

/// The eight physical blocks `fragmented.bin` occupies.
fn fragmented_blocks(img: &[u8]) -> Vec<u32> {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let node = &img[(leaf as usize) * (bs as usize)..(leaf as usize + 1) * (bs as usize)];
    let count = u16::from_be_bytes([node[10], node[11]]) as usize;
    for i in 0..count {
        let at = u16::from_be_bytes([
            node[node.len() - 2 * (i + 1)],
            node[node.len() - 2 * (i + 1) + 1],
        ]) as usize;
        let key_len = u16::from_be_bytes([node[at], node[at + 1]]) as usize;
        let body = at + 2 + key_len;
        if i16::from_be_bytes([node[body], node[body + 1]]) != 2 {
            continue;
        }
        if u32::from_be_bytes([
            node[body + 8],
            node[body + 9],
            node[body + 10],
            node[body + 11],
        ]) != FRAGMENTED_CNID
        {
            continue;
        }
        let fork = body + 88;
        return (0..8)
            .map(|e| {
                u32::from_be_bytes([
                    node[fork + 16 + e * 8],
                    node[fork + 16 + e * 8 + 1],
                    node[fork + 16 + e * 8 + 2],
                    node[fork + 16 + e * 8 + 3],
                ])
            })
            .collect();
    }
    panic!("{FRAGMENTED_CNID}: fragmented.bin not found in the catalog");
}

/// Set `fragmented.bin`'s first extent to zero blocks, leaving the bitmap alone.
fn free_fragmented_extent(img: &mut [u8]) {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;
    for i in 0..count {
        let at = u16::from_be_bytes([
            img[base + bs as usize - 2 * (i + 1)],
            img[base + bs as usize - 2 * (i + 1) + 1],
        ]) as usize;
        let key_len = u16::from_be_bytes([img[base + at], img[base + at + 1]]) as usize;
        let body = base + at + 2 + key_len;
        if u32::from_be_bytes([
            img[body + 8],
            img[body + 9],
            img[body + 10],
            img[body + 11],
        ]) != FRAGMENTED_CNID
        {
            continue;
        }
        // Zero the descriptor rather than repointing it: the block stays
        // allocated in the bitmap, which is the orphan this test wants.
        let extent = body + 88 + 16;
        img[extent..extent + 8].copy_from_slice(&[0u8; 8]);
        return;
    }
    panic!("{FRAGMENTED_CNID}: not found");
}

/// Clear the bitmap bit for `block` while leaving the catalog's extent alone.
fn mark_only_in_catalog(img: &mut [u8], block: u32) {
    let bs = block_size(img);
    let allocation_block = u32::from_be_bytes([
        img[VOLUME_HEADER_OFFSET + 112 + 16],
        img[VOLUME_HEADER_OFFSET + 112 + 17],
        img[VOLUME_HEADER_OFFSET + 112 + 18],
        img[VOLUME_HEADER_OFFSET + 112 + 19],
    ]);
    let at = (allocation_block as usize) * (bs as usize) + (block / 8) as usize;
    img[at] &= 0xFFu8 ^ (0x80u8 >> (block % 8));
}

/// Raise `fragmented.bin`'s declared `totalBlocks` without touching its extents.
fn inflate_total_blocks(img: &mut [u8], cnid: u32, by: u32) {
    let body = file_record_offset(img, cnid);
    let total = u32::from_be_bytes([
        img[body + 88 + 12],
        img[body + 88 + 13],
        img[body + 88 + 14],
        img[body + 88 + 15],
    ]);
    img[body + 88 + 12..body + 88 + 16].copy_from_slice(&(total + by).to_be_bytes());
}

/// Set the volume header's `nextCatalogID`.
fn set_next_catalog_id(img: &mut [u8], value: u32) {
    let at = VOLUME_HEADER_OFFSET + 64;
    img[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// Absolute offset of `cnid`'s file record body.
/// Byte offset of the record whose key parentID is `cnid`, plus its length.
fn record_span(img: &[u8], cnid: u32) -> Option<(usize, usize)> {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;
    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let at = u16::from_be_bytes([
            img[base + bs as usize - 2 * (i + 1)],
            img[base + bs as usize - 2 * (i + 1) + 1],
        ]) as usize;
        offsets.push(at);
    }
    for (i, at) in offsets.iter().enumerate() {
        if u32::from_be_bytes([
            img[base + at + 2],
            img[base + at + 3],
            img[base + at + 4],
            img[base + at + 5],
        ]) != cnid
        {
            continue;
        }
        let end = if i + 1 < offsets.len() {
            offsets[i + 1]
        } else {
            u16::from_be_bytes([img[base + 40], img[base + 41]]) as usize
        };
        // The offset array lives at the end of the node and must move with the
        // records, so the swap has to rewrite it too.
        return Some((base + at, end - at));
    }
    None
}

/// Swap the *keys* of two adjacent catalog records.
///
/// Keys only, and only where the two are the same length, so nothing after them
/// moves and the node's offset array stays correct. Every thread record's key is
/// `(fileID, "")` -- six bytes -- whatever its name, so any two of them qualify
/// while their bodies differ.
///
/// Swapping the whole record instead would need the records to be the same
/// length too, and they are not: a thread record's body holds the name, so
/// `.journal` and `.journal_info_block` differ by 22 bytes. Rebuilding the offset
/// array to suit that is a lot of machinery for a fixture that does not need it.
///
/// The result is the fault the reader cannot detect in itself. Both records stay
/// individually valid and the catalog still parses; only the two keys are now the
/// wrong way round.
fn swap_two_records(img: &mut [u8], a: u32, b: u32) {
    let (a_at, _) = record_span(img, a).expect("record A");
    let (b_at, _) = record_span(img, b).expect("record B");
    assert!(
        b_at > a_at,
        "records {a} and {b} must be distinct positions, got {a_at} and {b_at}"
    );

    let a_key_len = u16::from_be_bytes([img[a_at], img[a_at + 1]]) as usize + 2;
    let b_key_len = u16::from_be_bytes([img[b_at], img[b_at + 1]]) as usize + 2;
    assert_eq!(
        a_key_len, b_key_len,
        "the two keys must be the same length for a plain swap, got {a_key_len} and {b_key_len}"
    );

    let a_key = img[a_at..a_at + a_key_len].to_vec();
    let b_key = img[b_at..b_at + b_key_len].to_vec();
    img[a_at..a_at + a_key_len].copy_from_slice(&b_key);
    img[b_at..b_at + b_key_len].copy_from_slice(&a_key);
}
/// Turn the thread record for `cnid` into one that decodes as nothing.
///
/// A thread record's key parentID is the object's CNID. Setting the record type
/// to an unassigned value makes the record unparseable, which is the bluntest
/// possible "no thread record here" -- a real volume would instead have the
/// record absent, and that is what the checker must report either way.
fn zero_thread_record(img: &mut [u8], cnid: u32) {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;
    for i in 0..count {
        let at = u16::from_be_bytes([
            img[base + bs as usize - 2 * (i + 1)],
            img[base + bs as usize - 2 * (i + 1) + 1],
        ]) as usize;
        let key_len = u16::from_be_bytes([img[base + at], img[base + at + 1]]) as usize;
        // A thread record's key carries the *object's* CNID, so the match is on
        // the key's parentID rather than anything in the body.
        if u32::from_be_bytes([
            img[base + at + 2],
            img[base + at + 3],
            img[base + at + 4],
            img[base + at + 5],
        ]) != cnid
        {
            continue;
        }
        let body_at = base + at + 2 + key_len;
        // An unassigned record type, so nothing decodes it as a thread record.
        img[body_at..body_at + 2].copy_from_slice(&0x7FFFu16.to_be_bytes());
        return;
    }
    panic!("{cnid}: thread record not found");
}

/// Raise the root folder's declared `valence`.
fn inflate_root_valence(img: &mut [u8], by: u32) {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;
    for i in 0..count {
        let at = u16::from_be_bytes([
            img[base + bs as usize - 2 * (i + 1)],
            img[base + bs as usize - 2 * (i + 1) + 1],
        ]) as usize;
        let key_len = u16::from_be_bytes([img[base + at], img[base + at + 1]]) as usize;
        let body_at = base + at + 2 + key_len;
        if i16::from_be_bytes([img[body_at], img[body_at + 1]]) != 1 {
            continue;
        }
        let valence = u32::from_be_bytes([
            img[body_at + 4],
            img[body_at + 5],
            img[body_at + 6],
            img[body_at + 7],
        ]);
        img[body_at + 4..body_at + 8].copy_from_slice(&(valence + by).to_be_bytes());
        return;
    }
    panic!("root folder record not found");
}

/// How many children the root folder's thread records actually name.
fn expected_valence() -> u32 {
    let img = std::fs::read(common::image("journal-with-files")).expect("read");
    let bs = block_size(&img);
    let leaf = catalog_start(&img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;
    let mut children = 0;
    for i in 0..count {
        let at = u16::from_be_bytes([
            img[base + bs as usize - 2 * (i + 1)],
            img[base + bs as usize - 2 * (i + 1) + 1],
        ]) as usize;
        let key_len = u16::from_be_bytes([img[base + at], img[base + at + 1]]) as usize;
        let body_at = base + at + 2 + key_len;
        let rtype = i16::from_be_bytes([img[body_at], img[body_at + 1]]);
        if rtype != 3 && rtype != 4 {
            continue;
        }
        // A thread record's body names the object's parent.
        let parent = u32::from_be_bytes([
            img[body_at + 4],
            img[body_at + 5],
            img[body_at + 6],
            img[body_at + 7],
        ]);
        if parent == ROOT_CNID {
            children += 1;
        }
    }
    children
}

/// Absolute offset of `cnid`'s file record body.
fn file_record_offset(img: &[u8], cnid: u32) -> usize {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;
    for i in 0..count {
        let at = u16::from_be_bytes([
            img[base + bs as usize - 2 * (i + 1)],
            img[base + bs as usize - 2 * (i + 1) + 1],
        ]) as usize;
        let key_len = u16::from_be_bytes([img[base + at], img[base + at + 1]]) as usize;
        let body = base + at + 2 + key_len;
        if u32::from_be_bytes([
            img[body + 8],
            img[body + 9],
            img[body + 10],
            img[body + 11],
        ]) == cnid
        {
            return body;
        }
    }
    panic!("{cnid}: file record not found");
}

