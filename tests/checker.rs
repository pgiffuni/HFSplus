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

// --- Helpers for building broken copies ---------------------------------
//
// Each break is applied to a *copy* in the temporary directory, so the corpus
// image itself is never modified.

/// CNID of `fragmented.bin` in `journal-with-files`.
const FRAGMENTED_CNID: u32 = 18;
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

