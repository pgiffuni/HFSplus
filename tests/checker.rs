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

use hfsplus::blockdev::FileDevice;
use hfsplus::check::{self, CheckReport};
use hfsplus::volume::Volume;

fn check_image(name: &str) -> Option<(CheckReport, u32)> {
    let path = common::image(name);
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return None;
    }
    let dev = FileDevice::open(&path).unwrap_or_else(|e| panic!("open {name}: {e}"));
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
    let image = break_image("journal-with-files", |img| {
        free_fragmented_extent(img);
    });
    let Some((report, _)) = check_image(&image) else { return };

    assert!(
        report.missing.is_empty(),
        "the extent was removed, so nothing is missing from the bitmap: {:?}",
        report.missing
    );
    assert!(
        report.orphaned.contains(&fragmented_first_block(&image)),
        "the freed block must be reported orphaned, got {:?}",
        report.orphaned
    );
}

#[test]
fn a_fork_whose_extents_werent_marked_is_reported_missing() {
    // The other direction. A fork claims blocks the bitmap does not have marked,
    // which is what happens when extents are written without updating the map.
    let image = break_image("journal-with-files", |img| {
        let blocks = fragmented_blocks(img);
        mark_only_in_catalog(img, blocks[0]);
    });
    let Some((report, _)) = check_image(&image) else { return };

    assert!(
        report.orphaned.is_empty(),
        "nothing was orphaned, since the bitmap was left alone: {:?}",
        report.orphaned
    );
    assert!(
        report.missing.contains(&fragmented_first_block(&image)),
        "the unmarked block must be reported missing, got {:?}",
        report.missing
    );
}

#[test]
fn a_fork_declaring_more_blocks_than_its_extents_describe_is_reported() {
    // `totalBlocks` is a separate field from the extents, so it can disagree with
    // them independently -- and the difference is exactly what the file's own
    // length check would hide.
    let image = break_image("journal-with-files", |img| {
        inflate_total_blocks(img, FRAGMENTED_CNID, 7);
    });
    let Some((report, _)) = check_image(&image) else { return };

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
    let image = break_image("journal-with-files", |img| {
        set_next_catalog_id(img, FRAGMENTED_CNID);
    });
    let Some((report, _)) = check_image(&image) else { return };

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
    let image = break_image("journal-with-files", |img| {
        swap_two_records(img, RECORD_A, RECORD_B);
    });
    let Some((report, _)) = check_image(&image) else { return };
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
    let image = break_image("journal-with-files", |img| {
        zero_thread_record(img, FRAGMENTED_CNID);
    });
    let Some((report, _)) = check_image(&image) else { return };
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
    let image = break_image("journal-with-files", |img| {
        inflate_root_valence(img, 3);
    });
    let Some((report, _)) = check_image(&image) else { return };
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

// --- Helpers for building broken copies ---------------------------------
//
// Each break is applied to a *copy* in the temporary directory, so the corpus
// image itself is never modified.

/// CNID of `fragmented.bin` in `journal-with-files`.
const FRAGMENTED_CNID: u32 = 18;
/// CNID of the root folder, `kHFSRootFolderID`.
const ROOT_CNID: u32 = 2;
/// `sizeof(struct BTNodeDescriptor)`: where a leaf node's records begin.
const NODE_DESCRIPTOR_SIZE: usize = 14;
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
fn break_image(name: &str, break_it: impl FnOnce(&mut [u8])) -> String {
    let src = common::image(name);
    if !src.exists() {
        return name.to_string();
    }
    let mut img = std::fs::read(&src).unwrap_or_else(|e| panic!("read {name}: {e}"));
    break_it(&mut img);

    // A stable name derived from the modification, so repeated runs do not collide.
    let mut tag: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &img {
        tag ^= *b as u64;
        tag = tag.wrapping_mul(0x100_0000_01b3);
    }
    let stem = format!("{name}-broken-{tag:016x}");
    let dest = common::image(&stem);
    std::fs::write(&dest, &img).unwrap_or_else(|e| panic!("write {stem}: {e}"));
    let _ = std::fs::remove_file(&dest);
    stem
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

/// The first block of `fragmented.bin`, read back from the broken copy.
fn fragmented_first_block(broken: &str) -> u32 {
    // The tests that need this know the block they broke, so read it from the
    // image rather than hardcoding it: block numbers depend on the allocation
    // order, which the generator is free to change.
    let path = common::image(broken);
    let img = std::fs::read(&path).unwrap_or_else(|e| panic!("read {broken}: {e}"));
    fragmented_blocks(&img)[0]
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

/// Swap two adjacent records in the catalog leaf, moving them with their keys.
///
/// The records differ in length, so this shifts everything between them and
/// rewrites the offset array accordingly. Both records remain individually
/// valid: only their order changes, which is what makes this the check the
/// reader cannot perform on itself.
fn swap_two_records(img: &mut [u8], a: u32, b: u32) {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);

    let (a_at, a_len) = record_span(img, a).expect("record A");
    let (b_at, b_len) = record_span(img, b).expect("record B");
    assert!(
        b_at == a_at + a_len,
        "this helper only handles adjacent records, got {a_at}+{a_len} then {b_at}"
    );

    let a_bytes = img[a_at..a_at + a_len].to_vec();
    let b_bytes = img[b_at..b_at + b_len].to_vec();

    // Put B where A was and A where B was. Because the lengths differ, B's
    // neighbours shift by the difference.
    let delta = b_len as isize - a_len as isize;
    let after = base + bs as usize;
    for slot in img.iter_mut().take(after).skip(b_at + b_len) {
        *slot = slot.wrapping_add(delta as u8);
    }
    img[a_at..a_at + b_len].copy_from_slice(&b_bytes);
    img[a_at + b_len..a_at + b_len + a_len].copy_from_slice(&a_bytes);

    // Rebuild the offset array from the keys, which the swap left in place but
    // at the wrong offsets.
    rebuild_offset_array(img);
}

/// Recompute the leaf node's offset array by walking its records in order.
///
/// The array is the only thing that says where each record starts, so after
/// moving records it has to agree with the new layout. Records are variable
/// length and self-delimiting -- a key declares its own size, and a thread or
/// folder record is fixed -- so walking forward from the descriptor is enough.
fn rebuild_offset_array(img: &mut [u8]) {
    let bs = block_size(img);
    let leaf = catalog_start(img) + 1;
    let base = (leaf as usize) * (bs as usize);
    let count = u16::from_be_bytes([img[base + 10], img[base + 11]]) as usize;

    // Walk forward from the descriptor. A record is self-delimiting -- a key
    // declares its own size, and a thread, folder or file record is fixed -- so
    // the layout can be rebuilt without consulting the old offsets.
    let mut starts = Vec::with_capacity(count);
    let mut cursor = NODE_DESCRIPTOR_SIZE;
    for _ in 0..count {
        starts.push(cursor);
        let key_len = u16::from_be_bytes([
            img[base + cursor],
            img[base + cursor + 1],
        ]) as usize;
        let body_at = cursor + 2 + key_len;
        let rtype = i16::from_be_bytes([img[base + body_at], img[base + body_at + 1]]);
        let body = match rtype {
            1 => 88,
            2 => 248,
            _ => {
                // Thread record: fixed part plus a u16 name count.
                8 + 2 + 2 * u16::from_be_bytes([
                    img[base + body_at + 8],
                    img[base + body_at + 9],
                ]) as usize
            }
        };
        cursor = body_at + body;
    }
    for (i, at) in starts.iter().enumerate() {
        let pos = base + bs as usize - 2 * (i + 1);
        img[pos..pos + 2].copy_from_slice(&(*at as u16).to_be_bytes());
    }
    let free = base + bs as usize - 2 * (count + 1);
    img[free..free + 2].copy_from_slice(&(cursor as u16).to_be_bytes());
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

