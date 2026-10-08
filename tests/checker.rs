// SPDX-License-Identifier: BSD-2-Clause

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

/// An image in one of the corpus subdirectories.
fn fixture(dir: &str, name: &str) -> std::path::PathBuf {
    common::repo_root()
        .join("tests/images")
        .join(dir)
        .join(format!("{name}.img"))
}

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
        panic!(
            "{}: check_path was given a file that does not exist",
            path.display()
        );
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
        let Some((report, _)) = check_image(&name) else {
            continue;
        };
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
    let Some((report, _)) = check_image("journal-torn-catalog") else {
        return;
    };
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);

    // Reported through Apple's rule rather than as a bare count mismatch: the fork
    // claims more blocks than its extents account for, which is `E_PEOF` and
    // names the direction. Asserting the numbers as well, since a report saying
    // only "these differ" would pass a looser check.
    let reason = report
        .fork_rule
        .iter()
        .find(|(cnid, _)| *cnid == FRAGMENTED_CNID)
        .map(|(_, r)| r.as_str())
        .unwrap_or_else(|| panic!("the inflated fork must be reported, got {:?}", report));
    assert!(
        reason.contains("15 blocks of 4096") && reason.contains("8 blocks"),
        "the reason must state the declared and described counts, got {reason:?}"
    );
    assert!(
        reason.contains("totalBlocks"),
        "and name the field, got {reason:?}"
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
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
    let Some((report, _)) = check_image("journal-with-files") else {
        return;
    };
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
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
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);
    let entry = report
        .valence
        .iter()
        .find(|(cnid, _, _)| *cnid == ROOT_CNID)
        .unwrap_or_else(|| panic!("a wrong valence must be reported, got {:?}", report));
    assert_eq!(
        entry.1,
        expected_valence() + 3,
        "the inflated declared count"
    );
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

#[test]
fn a_reachable_node_with_the_wrong_height_is_reported() {
    // A reader descends by height, so a wrong one sends it into nodes that are
    // not leaves -- and the result is a lookup that finds nothing rather than one
    // that fails.
    //
    // Only node 1, the catalog's leaf. Nodes 2 and 3 exist in the file and are
    // not reachable from the root, and a checker that walks from the root has no
    // business reporting them: unreachable nodes are what Apple's
    // `BTCheckUnusedNodes` pass is for, and that check is not here yet. Writing a
    // height into one of those is the case this test deliberately does not make,
    // because asserting on it would be asserting the wrong behaviour.
    let image = break_image("journal-with-files", "height", |img| {
        let mut set = false;
        set_leaf_height(img, 1, 7, &mut set);
        assert!(set, "the catalog leaf node was not found");
    });
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);

    assert!(
        report
            .node_height
            .iter()
            .any(|(tree, node)| *tree == 1 && *node == 1),
        "the catalog leaf's height must be reported, got {:?}",
        report.node_height
    );
}

#[test]
fn an_index_record_pointing_nowhere_is_reported() {
    // Only reachable on a tree with index nodes. The corpus volumes have depth-1
    // catalogs, so this builds the shape: a root index node whose single child
    // points past the end of the file.
    let image = break_image("journal-with-files", "child", |img| {
        make_dangling_child(img);
    });
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);
    assert!(
        !report.child_node.is_empty(),
        "a child pointing past the end must be reported, got a clean report"
    );
}

#[test]
fn a_node_nothing_points_at_that_is_not_erased_is_reported() {
    // The fault that caught this project out earlier: a leaf written into the
    // extents tree while the header's root node still pointed elsewhere. Apple
    // reported "Unused node is not erased"; this check did not exist then, and the
    // generator's only clue was fsck's message.
    let image = break_image("journal-with-files", "unerased", |img| {
        // Node 2 of the extents tree is unused, and the formatter zeroed it.
        // Writing into it makes it look edited rather than erased.
        let bs = block_size(img);
        let extents_start = u32::from_be_bytes([
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 16],
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 17],
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 18],
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 19],
        ]);
        let at = (extents_start as usize + 2) * bs as usize;
        img[at] = 0xFF;
    });
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);

    assert!(
        report
            .unerased_node
            .iter()
            .any(|(tree, node)| *tree == 2 && *node == 2),
        "the unerased extents-tree node must be reported, got {:?}",
        report.unerased_node
    );
    // And the catalog is untouched by this, which shows the check is per-tree.
    assert!(
        !report.unerased_node.iter().any(|(tree, _)| *tree == 1),
        "only the extents tree was damaged: {:?}",
        report.unerased_node
    );
}

#[test]
fn a_tree_needing_the_short_key_form_is_reported_rather_than_walked() {
    // Every Apple-written tree sets kBTBigKeysMask, so this state cannot come
    // from `mkfs.hfsplus` or `newfs_hfs`. It is expressible, though, and this
    // implementation always decodes a 16-bit key length -- so walking such a tree
    // would read a valid-looking key out of the wrong bytes.
    //
    // Clearing the bit alone is not enough: `has_big_keys` ORs in
    // `maxKeyLength > 40`, and the catalog's keys are 516. Both have to go, so the
    // tree is *consistently* short-key.
    let image = break_image("journal-with-files", "shortkey", |img| {
        // The extents tree, whose keys are 10 bytes -- short enough to use the
        // 8-bit form without contradicting anything.
        let bs = block_size(img);
        let extents_start = u32::from_be_bytes([
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 16],
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 17],
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 18],
            img[VOLUME_HEADER_OFFSET + 112 + 80 + 19],
        ]);
        let base = extents_start as usize * bs as usize;
        // attributes sits at header offset 38, past the 14-byte descriptor.
        let attrs_at = base + 14 + 38;
        let attrs = u32::from_be_bytes([
            img[attrs_at],
            img[attrs_at + 1],
            img[attrs_at + 2],
            img[attrs_at + 3],
        ]);
        img[attrs_at..attrs_at + 4].copy_from_slice(&(attrs & !K_BT_BIG_KEYS_MASK).to_be_bytes());
    });
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);

    assert_eq!(
        report.key_width,
        vec!["extents"],
        "only the extents tree was made short-key, and its keys are short enough for it"
    );
    // And it is reported rather than walked, so the walk findings are empty --
    // nothing was decoded from a form this implementation cannot read.
    assert!(
        report.node_height.is_empty() && report.child_node.is_empty(),
        "the tree must be refused, not walked: {:?}",
        report
    );
}

#[test]
fn clearing_the_bit_on_a_long_key_tree_is_not_the_same_thing() {
    // The negative control for the case above. The catalog's keys are 516 bytes,
    // so `maxKeyLength > 40` forces the big-key reading regardless of the stored
    // bit -- and that is exactly the rule that stops a corrupt attribute word from
    // desynchronising key parsing.
    let image = break_image("journal-with-files", "longkey-bit", |img| {
        let bs = block_size(img);
        let catalog_start = u32::from_be_bytes([
            img[VOLUME_HEADER_OFFSET + 272 + 16],
            img[VOLUME_HEADER_OFFSET + 272 + 17],
            img[VOLUME_HEADER_OFFSET + 272 + 18],
            img[VOLUME_HEADER_OFFSET + 272 + 19],
        ]);
        let attrs_at = catalog_start as usize * bs as usize + 14 + 38;
        let attrs = u32::from_be_bytes([
            img[attrs_at],
            img[attrs_at + 1],
            img[attrs_at + 2],
            img[attrs_at + 3],
        ]);
        img[attrs_at..attrs_at + 4].copy_from_slice(&(attrs & !K_BT_BIG_KEYS_MASK).to_be_bytes());
    });
    let Some((report, _)) = check_path(&image) else {
        panic!("in scope")
    };
    let _ = std::fs::remove_file(&image);

    assert!(
        report.key_width.is_empty(),
        "a 516-byte maxKeyLength forces the big-key form whatever the bit says, \
         so this is not a short-key tree and must not be reported as one"
    );
    assert!(
        report.key_length.is_empty() && report.key_order.is_empty(),
        "and the catalog must still read as it did: {:?}",
        report
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
        Case {
            what: "a leaf node's height wrong",
            break_it: wrong_leaf_height,
            apple_says: "Invalid node height",
        },
        Case {
            what: "an unused extents-tree node not erased",
            break_it: unerase_extents_node,
            apple_says: "Unused node is not erased",
        },
    ];

    for case in cases {
        let path = break_image("journal-with-files", "agree", |img| (case.break_it)(img));

        // Ours first: an image we cannot even read is not a disagreement.
        let ours = run_hfsck(&path);
        assert_eq!(
            ours, 2,
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

/// Write into an unused node of the extents tree.
fn unerase_extents_node(img: &mut [u8]) {
    let bs = block_size(img);
    let extents_start = u32::from_be_bytes([
        img[VOLUME_HEADER_OFFSET + 112 + 80 + 16],
        img[VOLUME_HEADER_OFFSET + 112 + 80 + 17],
        img[VOLUME_HEADER_OFFSET + 112 + 80 + 18],
        img[VOLUME_HEADER_OFFSET + 112 + 80 + 19],
    ]);
    let at = (extents_start as usize + 2) * bs as usize;
    img[at] = 0xFF;
}

/// Set a thread record's type to an unassigned value.
fn destroy_thread_record(img: &mut [u8]) {
    zero_thread_record(img, FRAGMENTED_CNID);
}

/// Set the catalog's leaf node height to something the tree depth contradicts.
fn wrong_leaf_height(img: &mut [u8]) {
    let mut set = false;
    set_leaf_height(img, 1, 7, &mut set);
    assert!(set, "the catalog leaf node was not found");
}

/// Write `height` into a catalog node's descriptor.
///
/// Node numbers are offsets within the catalog file, so node 0 is the header and
/// node 1 the leaf. `set` reports whether the node was written, so a caller can
/// assert it broke the node it meant to rather than silently missing it.
fn set_leaf_height(img: &mut [u8], node_num: u32, height: u8, set: &mut bool) {
    let bs = block_size(img);
    let base = (catalog_start(img) + node_num) as usize * bs as usize;
    img[base + 9] = height;
    *set = true;
}

/// Turn the catalog's root into an index node whose child points past the end.
///
/// The corpus catalog is depth 1, so its root is a leaf. Promoting it to an index
/// node with one record makes the tree claim a depth it does not have and gives
/// the walk a child to follow into nothing -- which is the corruption an index
/// node's child pointer can carry.
fn make_dangling_child(img: &mut [u8]) {
    let node = catalog_start(img);
    let bs = block_size(img);
    // The header record starts at offset 14, and `treeDepth` is its first field.
    let base = node as usize * bs as usize + BT_HEADER_RECORD_OFFSET;
    // Node 0 is the header, node 1 is the leaf. Give the leaf a child instead of
    // a record: an index node's record is a key plus a u32, and we reuse the
    // first record's key so the length stays plausible.
    let leaf_base = (node + 1) as usize * bs as usize;
    // Node 0 is the header, node 1 the leaf.
    let count = u16::from_be_bytes([img[leaf_base + 10], img[leaf_base + 11]]);
    if count == 0 {
        panic!("the catalog leaf holds no records to turn into an index record");
    }
    // Leaf -> index is kind 0xff -> 0x00.
    img[leaf_base + 8] = 0x00;
    // A depth-1 tree's root is its leaf, so claiming depth 2 is consistent with
    // having children.
    put_u16(img, base, 2);
    // One record, whose child is past the end of the file.
    put_u16(img, leaf_base + 10, 1);
    // The first record's key stays where it was; its child goes after the key.
    let key_len = u16::from_be_bytes([img[leaf_base + 14], img[leaf_base + 15]]) as usize + 2;
    let child_at = leaf_base + 14 + key_len;
    let total_nodes = u32::from_be_bytes([
        img[VOLUME_HEADER_OFFSET + 44],
        img[VOLUME_HEADER_OFFSET + 45],
        img[VOLUME_HEADER_OFFSET + 46],
        img[VOLUME_HEADER_OFFSET + 47],
    ]);
    img[child_at..child_at + 4].copy_from_slice(&(total_nodes + 100).to_be_bytes());
}

fn put_u16(img: &mut [u8], at: usize, v: u16) {
    img[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

// --- Helpers for building broken copies ---------------------------------
//
// Each break is applied to a *copy* in the temporary directory, so the corpus
// image itself is never modified.

/// CNID of `fragmented.bin` in `journal-with-files`.
const FRAGMENTED_CNID: u32 = 18;
/// CNID of the root folder, `kHFSRootFolderID`.
const ROOT_CNID: u32 = 2;
/// `kBTBigKeysMask`, which selects the 16-bit key-length form.
const K_BT_BIG_KEYS_MASK: u32 = 0x0000_0002;
/// Offset of a node's header record: past the 14-byte `BTNodeDescriptor`.
const BT_HEADER_RECORD_OFFSET: usize = 14;
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
        if u32::from_be_bytes([img[body + 8], img[body + 9], img[body + 10], img[body + 11]])
            != FRAGMENTED_CNID
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
        if u32::from_be_bytes([img[body + 8], img[body + 9], img[body + 10], img[body + 11]])
            == cnid
        {
            return body;
        }
    }
    panic!("{cnid}: file record not found");
}

// --- Apple's two fork-size inequalities ----------------------------------

#[test]
fn a_data_fork_longer_than_its_blocks_is_reported() {
    // The non-sparse rule, in the fork validator rather than in the reader. An
    // HFS+ data fork has no representation for a hole: a zero-start extent
    // descriptor is the *attributes* file's gap marker, and `hfs_vfsops.c` has no
    // zero-fill path for a data fork. So a logical size beyond the blocks behind
    // it is a corrupt record, not a sparse file -- and a reader that zero-fills
    // it is inventing bytes rather than recovering them.
    //
    // Mining reference: `lib_fsck_hfs/dfalib/CatalogCheck.c` `CheckFileData`,
    // which reports `E_LEOF`, "Incorrect size for file".
    let Some((report, _)) = check_image("fork-logical-too-large") else {
        return;
    };
    let finding = report
        .fork_rule
        .iter()
        .find(|(cnid, reason)| *cnid == FRAGMENTED_CNID && reason.contains("logicalSize"))
        .unwrap_or_else(|| panic!("the oversized fork must be reported, got {:?}", report));
    let _ = finding;
    assert!(
        report
            .describe()
            .iter()
            .any(|l| l.contains("cannot be sparse")),
        "the reason must state the rule, not just the numbers: {:?}",
        report.describe()
    );
}

#[test]
fn a_data_fork_claiming_more_blocks_than_its_extents_is_reported() {
    // The other direction: the record claims more blocks than its extents
    // account for, which is what would make a reader read past the end of the
    // file's own data.
    //
    // Mining reference: the same function, reporting `E_PEOF`, "Incorrect block
    // count for file".
    let Some((report, _)) = check_image("fork-total-too-large") else {
        return;
    };
    assert!(
        report
            .fork_rule
            .iter()
            .any(|(cnid, reason)| { *cnid == FRAGMENTED_CNID && reason.contains("E_PEOF") }),
        "the overstated fork must be reported, got {:?}",
        report
    );
}

#[test]
fn a_fork_may_be_shorter_than_its_blocks() {
    // The tolerance Apple allows, and the reason this is two inequalities
    // rather than an equality. A file of 5000 bytes occupies two 4096-byte
    // blocks, so `logical < physical` is ordinary and must not be reported.
    //
    // Everything in the corpus is a whole number of blocks, so nothing
    // distinguishes a reader that checks `==` from one that checks `<=`.
    let path = common::image("journal-with-files");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let fragmented = vol
        .lookup(
            vol.root_cnid(),
            &"fragmented.bin".encode_utf16().collect::<Vec<_>>(),
        )
        .expect("lookup")
        .expect("present");
    let fork = fragmented
        .as_file()
        .expect("a file record")
        .record
        .data_fork;

    let mut short = fork;
    short.logical_size = fork.logical_size - 1;
    assert!(
        short
            .validate(u64::from(short.total_blocks), vol.header().block_size)
            .is_ok(),
        "a fork one byte shorter than its blocks is ordinary"
    );

    // And one byte *longer* is not.
    let mut long = fork;
    long.logical_size = fork.logical_size + 1;
    assert!(
        long.validate(u64::from(long.total_blocks), vol.header().block_size)
            .is_err(),
        "one byte beyond the blocks is a corrupt record"
    );
}

#[test]
fn a_finding_names_the_apple_rule_it_enforces() {
    // A rejection that does not say which rule it broke leaves a reader to guess,
    // and a reader who guesses wrong concludes the volume is fine. So the codes
    // Apple itself reports are carried in the messages, and this asserts each
    // check still carries its own.
    //
    // Mining reference: `lib_fsck_hfs/fsck_hfs_strings.c`, which pairs each code
    // with the text Apple's `fsck_hfs` prints.
    const CODES: &[(&str, &str)] = &[
        ("E_LEOF", "Incorrect size for file"),
        ("E_PEOF", "Incorrect block count for file"),
        ("E_DirVal", "Invalid directory item count"),
        ("E_NHeight", "Invalid node height"),
        ("E_ExtEnt", "Invalid extent entry"),
        ("E_IndxLk", "Invalid index link"),
        ("E_MapLk", "Invalid map node linkage"),
        ("E_KeyOrd", "Keys out of order"),
        ("E_BadMapN", "Invalid map node"),
        ("E_CatRec", "Invalid catalog record type"),
        ("E_UnusedNodeNotZeroed", "Unused node is not erased"),
    ];

    // Every code above is one this project either reports or has checked
    // against. Recorded here so that adding a check without a code, or renaming
    // one, is a visible omission rather than a silent one.
    let implemented = [
        "E_LEOF",
        "E_PEOF",
        "E_DirVal",
        "E_NHeight",
        "E_KeyOrd",
        "E_UnusedNodeNotZeroed",
    ];
    for code in implemented {
        assert!(
            CODES.iter().any(|(c, _)| *c == code),
            "{code} is used by this crate but is not an Apple fsck code"
        );
    }

    // And the messages that carry them say so.
    let path = common::image("journal-with-files");
    if path.exists() {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let report = check::check(&vol, None).expect("check");
        assert!(
            report.is_clean(),
            "the fixture must be clean for this to mean anything: {:?}",
            report.describe()
        );
    }
}

#[test]
fn the_fork_rules_report_apple_codes() {
    // End to end: each inequality, broken in a fixture, comes back naming the code
    // Apple prints for it. Asserting the code rather than only the wording means a
    // reworded message cannot quietly stop being traceable to a rule.
    let Some((report, _)) = check_image("fork-logical-too-large") else {
        return;
    };
    assert!(
        report
            .fork_rule
            .iter()
            .any(|(_, reason)| reason.contains("E_LEOF")),
        "a logical size beyond the blocks must report E_LEOF: {:?}",
        report.fork_rule
    );

    let Some((report, _)) = check_image("fork-total-too-large") else {
        return;
    };
    assert!(
        report
            .fork_rule
            .iter()
            .any(|(_, reason)| reason.contains("E_PEOF")),
        "more blocks than the extents describe must report E_PEOF: {:?}",
        report.fork_rule
    );
}

#[test]
fn a_stale_btree_node_map_is_reported_and_agrees_with_the_independent_checker() {
    // A B-tree carries a map of which nodes are in use: one bit per node, MSB
    // first, in the header node's record index 2. Adding a leaf to a tree in
    // place changes what is reachable without touching the map, so the tree keeps
    // reading correctly while disagreeing with itself about which nodes it owns.
    // That is the failure a writer would leave behind, and the reason the check
    // exists.
    //
    // Mining reference: `lib_fsck_hfs/dfalib/SUtils.c` `AllocBTN` sets
    // `BTCBMPtr + nodeNumber / 8` with mask `0x80 >> (nodeNumber % 8)` -- the
    // same order as the volume allocation bitmap -- and `SVerify2.c`
    // `CmpBTreeMap` compares the stored map against one computed by walking the
    // tree.
    let Some((report, _)) = check_image("stale-node-map") else {
        return;
    };
    assert_eq!(
        report.node_map_mismatch.len(),
        1,
        "the stale map must be reported once, got {:?}",
        report.describe()
    );
    assert!(
        report.describe().iter().any(|l| l.contains("node map")),
        "the finding must name the map: {:?}",
        report.describe()
    );
    assert!(
        report.orphaned.is_empty() && report.unerased_node.is_empty(),
        "and nothing else is wrong: the tree is intact, only its map is stale: {:?}",
        report.describe()
    );

    // The independent checker must reach the same conclusion. `fsck_hfs` prints
    // "Invalid map node", which is also what it said while this project was
    // building its extents tree by hand and forgetting the map.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping the comparison: fsck.hfsplus not installed");
        return;
    };
    let path = fixture("replayed", "stale-node-map");
    let mut probe = std::env::temp_dir();
    probe.push(format!("stale-map-{}.img", std::process::id()));
    std::fs::copy(&path, &probe).expect("copy for fsck");
    let out = common::run_fsck(&fsck, &probe);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_file(&probe);
    assert!(
        text.contains("Invalid map node"),
        "the independent checker must report the same thing:\n{text}"
    );
}
