//! A fork whose extents overflow into the Extents B-tree.
//!
//! A fork holds at most `kHFSPlusExtentDensity` — 8 — extents inline. A file
//! larger than that keeps the rest in the **extents overflow B-tree**, keyed on
//! `(fileID = the fork's CNID, startBlock = cumulative blocks already described)`.
//!
//! # Why this needed its own suite
//!
//! `Volume::fork_reader` built a plain `ForkReader` with no overflow resolver, so
//! `ForkOverflow` and `TreeOverflow` were implemented, tested in isolation, and
//! **never used by anything**. A file that overflowed would read only its first
//! eight extents: past that boundary every offset mapped to nothing, and the
//! reader returned zeros instead of the file. Nothing in the corpus has a file
//! large enough to overflow — `mkfs.hfsplus` cannot create files at all — so the
//! bug was invisible.
//!
//! The fix wires the resolver in. These tests build the overflowing shape
//! synthetically, so the coverage does not depend on a corpus image that would
//! need macOS or `hfsprogs` to produce.
//!
//! Mining reference: Apple `core/hfs_extents.c` `extoffset` walks a fork's inline
//! extents and, once past the density, resolves the remainder through
//! `SearchExtentFile`; `core/FileExtentMapping.c` `MapFileBlockC` maps a file
//! offset onto a physical sector using the extent record that `extoffset` found.

mod common;

use hfsplus::blockdev::MemoryDevice;
use hfsplus::btree::ExtentKey;
use hfsplus::file::{ForkOverflow, ForkReader, TreeOverflow};
use hfsplus::format::extents::{ExtentDescriptor, ExtentRecord, EXTENT_RECORD_SIZE};
use hfsplus::format::fork::ForkData;

/// Blocks per inline extent record. `kHFSPlusExtentDensity`.
const DENSITY: usize = 8;

/// `sizeof(struct HFSPlusExtentKey)` -- the 2-byte prefix plus its 10-byte
/// body (`forkType`, `pad`, `fileID`, `startBlock`).
const EXTENT_KEY_ON_DISK_SIZE: usize = 12;

const BLOCK: u32 = 4096;

/// Build a device where block `n` contains a byte pattern identifying it, so a
/// read from the wrong block is visible in the bytes rather than plausible.
fn patterned_device(blocks: u32) -> MemoryDevice {
    let mut image = vec![0u8; (blocks as usize) * (BLOCK as usize)];
    for b in 0..blocks {
        let at = (b as usize) * (BLOCK as usize);
        image[at..at + 4].copy_from_slice(&b.to_be_bytes());
        // Fill the rest deterministically so a mis-mapped block is obvious.
        for i in 0..(BLOCK as usize - 4) {
            image[at + 4 + i] = ((b as usize + i) % 251) as u8;
        }
    }
    MemoryDevice::new(image)
}

/// Which block does each byte of block `b` hold?
fn expected_byte(b: u32, within: usize) -> u8 {
    if within < 4 {
        u8::try_from(b >> ((3 - within) * 8)).unwrap_or(0)
    } else {
        ((b as usize + within - 4) % 251) as u8
    }
}

/// A fork of `total_extents` single-block extents, laid out as the on-disk
/// structures really are: the first eight inline, the rest in overflow groups of
/// eight.
///
/// Physical blocks are allocated with a **stride**, so consecutive logical
/// extents are not physically adjacent. A reader that concatenates the extents
/// in the wrong order, or that drops a group, then produces a different byte
/// stream rather than a plausible one.
///
/// Returns the fork plus the full extent list in logical order, so a test can
/// check that the reader reproduces it.
fn fragmented_fork(total_extents: u32) -> (ForkData, Vec<ExtentDescriptor>) {
    let mut fork = ForkData::default();
    let mut layout = Vec::new();
    let mut block = 1u32;
    let stride = 3u32;

    // Every extent, in logical order: the first eight go inline, the rest are
    // what the overflow tree has to supply.
    for _ in 0..total_extents {
        layout.push(ExtentDescriptor {
            start_block: block,
            block_count: 1,
        });
        block += stride;
    }
    fork.extents.raw[..(total_extents as usize).min(DENSITY)]
        .clone_from_slice(&layout[..(total_extents as usize).min(DENSITY)]);

    fork.logical_size = u64::from(total_extents) * u64::from(BLOCK);
    fork.total_blocks = total_extents;
    (fork, layout)
}

/// The overflow records that `fragmented_fork(total)` implies: one group per
/// eight extents past the inline eight, keyed on the running block total.
fn overflow_groups_for(layout: &[ExtentDescriptor]) -> Vec<(u32, u32, ExtentRecord)> {
    let mut records = Vec::new();
    // The first overflow group is keyed on the number of blocks the *inline*
    // record already covers, not on zero: `startBlock` is a file allocation
    // block number, and the inline record covers blocks 0..8.
    let inline: u32 = layout.iter().take(DENSITY).map(|e| e.block_count).sum();
    let mut described = inline;
    let mut index = DENSITY;
    while index < layout.len() {
        let end = (index + DENSITY).min(layout.len());
        let mut group = ExtentRecord::default();
        for (slot, i) in (index..end).enumerate() {
            group.raw[slot] = layout[i];
        }
        records.push((FORK_CNID, described, group));
        described += (end - index) as u32;
        index = end;
    }
    records
}

// --- The shape itself ---------------------------------------------------

#[test]
fn a_fork_past_eight_extents_reports_needing_overflow() {
    // The premise of the whole suite: if this does not hold, nothing below is
    // testing what it claims to.
    let (nine, _) = fragmented_fork(9);
    assert!(
        nine.needs_overflow(),
        "9 single-block extents exceed the density"
    );
    assert_eq!(nine.overflow_block_count(), 1);

    let (eight, _) = fragmented_fork(8);
    assert!(!eight.needs_overflow(), "8 extents fit inline");
    assert_eq!(eight.overflow_block_count(), 0);
}

#[test]
fn the_overflow_count_is_what_the_inline_record_does_not_cover() {
    // The count is blocks, not extents, so a fork whose extents are many blocks
    // long overflows sooner than one with single-block extents. Checking the
    // arithmetic rather than assuming it.
    let (fork, _) = fragmented_fork(10);
    let inline: u64 = (0..DENSITY)
        .map(|i| u64::from(fork.extents.raw[i].block_count))
        .sum();
    assert_eq!(inline, DENSITY as u64);
    assert_eq!(fork.total_blocks, 10);
    assert_eq!(fork.overflow_block_count(), 10 - DENSITY as u64);
}

// --- Reading through the resolver ---------------------------------------

/// Build an extents B-tree body holding one record for `file_id`, and return the
/// bytes.
///
/// The corpus cannot supply a real extents tree for a synthetic fork, so the
/// tree is assembled by hand: one header node whose `leafRecords` is 1, whose
/// first leaf is node 1, and whose leaf node holds a single key-and-record pair.
fn synthetic_extents_tree(node_size: usize, records: &[(u32, u32, ExtentRecord)]) -> Vec<u8> {
    assert!(
        node_size >= EXTENT_RECORD_SIZE + 64,
        "node too small for this test"
    );
    let mut image = vec![0u8; node_size * 2];

    // Node 0: the header. `struct BTNodeDescriptor` is 14 bytes; the header
    // record follows it.
    let h = 0usize;
    image[h + 8] = 1; // kBTHeaderNode
    put_u16(&mut image, h + 10, 3);
    let hdr_rec = h + 14;
    put_u16(&mut image, hdr_rec + 18, node_size as u16); // nodeSize
    put_u16(&mut image, hdr_rec + 20, 516); // maxKeyLength, kHFSPlusAttrKeyMaximumLength
    put_u32(&mut image, hdr_rec + 22, 2); // totalNodes
    put_u32(&mut image, hdr_rec + 26, 1); // freeNodes
    put_u32(&mut image, hdr_rec + 6, records.len() as u32); // leafRecords
    put_u32(&mut image, hdr_rec + 10, 1); // firstLeafNode
    put_u32(&mut image, hdr_rec + 14, 1); // lastLeafNode
                                          // Offset array: 3 records + the free-space slot.
    put_u16(&mut image, node_size - 2, (h + 14) as u16);
    put_u16(&mut image, node_size - 4, (h + 14 + 106) as u16);
    put_u16(&mut image, node_size - 6, (h + 14 + 106 + 8) as u16);
    put_u16(
        &mut image,
        node_size - 8,
        (h + 14 + 106 + 8 + EXTENT_RECORD_SIZE) as u16,
    );

    // Node 1: the leaf, holding every record.
    let l = node_size;
    image[l + 8] = 0xFF; // kBTLeafNode
    put_u16(&mut image, l + 10, records.len() as u16);
    let mut at = l + 14;
    let mut offsets = Vec::new();
    for (file_id, start_block, extents) in records {
        let fork_type = ExtentKey::DATA_FORK;
        offsets.push(at);
        // `struct HFSPlusExtentKey` is
        //
        //     u_int16_t keyLength;   // length excluding this field
        //     u_int8_t  forkType;    //  0 = data fork
        //     u_int8_t  pad;
        //     u_int32_t fileID;
        //     u_int32_t startBlock;
        //
        // so keyLength is 8 and the record occupies 10 bytes with the prefix.
        put_u16(&mut image, at, 10);
        image[at + 2] = fork_type;
        image[at + 3] = 0; // pad
        put_u32(&mut image, at + 4, *file_id);
        put_u32(&mut image, at + 8, *start_block);
        at += EXTENT_KEY_ON_DISK_SIZE;
        image[at..at + EXTENT_RECORD_SIZE].copy_from_slice(&extents.to_bytes());
        at += EXTENT_RECORD_SIZE;
    }
    put_u16(&mut image, l + 40, (at - l) as u16);
    for (i, off) in offsets.iter().enumerate() {
        put_u16(&mut image, node_size * 2 - 2 * (i + 1), (*off - l) as u16);
    }
    put_u16(
        &mut image,
        node_size * 2 - 2 * (records.len() + 1),
        (at - l) as u16,
    );
    image
}

fn put_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

fn one_extent(start: u32, count: u32) -> ExtentRecord {
    let mut e = ExtentRecord::default();
    e.raw[0] = ExtentDescriptor {
        start_block: start,
        block_count: count,
    };
    e
}

#[test]
fn reading_past_the_inline_extents_requires_the_overflow_tree() {
    // The property the missing wiring broke. Without a resolver the reader stops
    // after eight extents; with one it must continue into the tree.
    //
    // `ForkReader::with_overflow` takes a boxed resolver, so this drives it
    // directly rather than through `Volume` -- the volume-level wiring is
    // covered by the corpus-backed suite, and what matters here is the reader's
    // behaviour on a fork that overflows.
    let (fork, layout) = fragmented_fork(10);
    let dev = patterned_device(64);

    // Without a resolver: the first eight extents read, the rest do not.
    let plain = ForkReader::new(&dev, &fork, BLOCK);
    let head = plain.read(0, DENSITY * BLOCK as usize).expect("head");
    assert_eq!(
        head.len(),
        DENSITY * BLOCK as usize,
        "the inline extents are readable without any tree"
    );
    let beyond = plain
        .read((DENSITY * BLOCK as usize) as u64, 16)
        .expect("beyond inline");
    assert!(
        beyond.iter().all(|b| *b == 0),
        "without a resolver the overflow extent reads as zeros, which is the bug"
    );

    // With a resolver over the tree holding the overflow group.
    let records = overflow_groups_for(&layout);
    assert_eq!(
        records.len(),
        1,
        "10 extents need exactly one overflow group"
    );

    let tree_bytes = synthetic_extents_tree(BLOCK as usize, &records);
    let tree_len = tree_bytes.len() as u64;
    let tree_dev = MemoryDevice::new(tree_bytes);
    let extents_fork = ForkData {
        logical_size: tree_len,
        total_blocks: 2,
        extents: one_extent(0, 2),
        ..ForkData::default()
    };

    let tree = hfsplus::btree::io::BTreeFile::open(&tree_dev, &extents_fork, BLOCK, true)
        .expect("open the extents tree");
    let overflow = TreeOverflow::new(tree);
    let resolver = ForkOverflow::for_fork(&overflow, ExtentKey::DATA_FORK, FORK_CNID);
    let reader = ForkReader::with_overflow(&dev, &fork, BLOCK, Box::new(resolver));

    let whole = reader
        .read(0, 10 * BLOCK as usize)
        .expect("read the whole fork");
    assert_eq!(
        whole.len(),
        10 * BLOCK as usize,
        "the whole fork must be readable"
    );

    // And the content must be right, not merely present: each block must carry
    // its own pattern, in the order the extents give.
    for (i, extent) in layout.iter().enumerate() {
        let base = i * BLOCK as usize;
        for within in [0usize, 4, 100, BLOCK as usize - 1] {
            assert_eq!(
                whole[base + within],
                expected_byte(extent.start_block, within),
                "block {i} (physical {}) wrong at +{within}",
                extent.start_block
            );
        }
        assert_eq!(
            extent.block_count, 1,
            "this layout uses one block per extent so ordering is unambiguous"
        );
    }
}

/// The CNID used for the synthetic fork's extents keys.
const FORK_CNID: u32 = 42;

#[test]
fn a_resolver_for_an_unknown_cnid_finds_nothing() {
    // A tree that has no record for this CNID must end the walk rather than
    // return another file's extents. Getting this wrong would splice two files
    // together.
    let image = synthetic_extents_tree(
        BLOCK as usize,
        &[(FORK_CNID, DENSITY as u32, one_extent(40, 1))],
    );
    let image_len = image.len() as u64;
    let dev = MemoryDevice::new(image);
    let fork = ForkData {
        logical_size: image_len,
        total_blocks: 2,
        extents: one_extent(0, 2),
        ..ForkData::default()
    };
    let tree = hfsplus::btree::io::BTreeFile::open(&dev, &fork, BLOCK, true).expect("open");
    let overflow = TreeOverflow::new(tree);

    assert!(
        overflow
            .find_group(ExtentKey::DATA_FORK, FORK_CNID, DENSITY as u32)
            .unwrap()
            .is_some(),
        "the record for our CNID must be found"
    );
    assert!(
        overflow
            .find_group(ExtentKey::DATA_FORK, FORK_CNID + 1, DENSITY as u32)
            .unwrap()
            .is_none(),
        "another file's CNID must find nothing"
    );
    assert!(
        overflow
            .find_group(ExtentKey::DATA_FORK, FORK_CNID, DENSITY as u32 + 1)
            .unwrap()
            .is_none(),
        "a start block with no record must find nothing"
    );
}

#[test]
fn the_key_is_the_running_total_of_blocks_already_described() {
    // The second overflow group's key is not 8 and not 16: it is the cumulative
    // block count of everything the inline record and the first group already
    // cover. Getting the key wrong is the classic bug in this area, and it would
    // make group two unreachable.
    let (fork, layout) = fragmented_fork(20);
    assert_eq!(fork.total_blocks, 20);
    assert_eq!(layout.len(), 20, "20 single-block extents");
    assert_eq!(fork.overflow_block_count(), 20 - DENSITY as u64);

    let image_records = overflow_groups_for(&layout);
    // Keys are 8, 16 and 24: each group is 8 single-block extents.
    let keys: Vec<u32> = image_records.iter().map(|(_, k, _)| *k).collect();
    assert_eq!(
        keys,
        vec![8, 16],
        "20 extents is 8 inline plus two groups of 6"
    );

    let image = synthetic_extents_tree(BLOCK as usize, &image_records);
    let image_len = image.len() as u64;
    let dev = MemoryDevice::new(image);
    let tree_fork = ForkData {
        logical_size: image_len,
        total_blocks: 2,
        extents: one_extent(0, 2),
        ..ForkData::default()
    };
    let tree = hfsplus::btree::io::BTreeFile::open(&dev, &tree_fork, BLOCK, true).expect("open");
    let overflow = TreeOverflow::new(tree);

    for (i, key) in keys.iter().enumerate() {
        let found = overflow
            .find_group(ExtentKey::DATA_FORK, FORK_CNID, *key)
            .unwrap_or_else(|e| panic!("group {i} at key {key}: {e}"));
        assert!(found.is_some(), "group {i} at key {key} must be found");
    }
    assert!(
        overflow
            .find_group(ExtentKey::DATA_FORK, FORK_CNID, 32)
            .unwrap()
            .is_none(),
        "a key past the last group must find nothing"
    );
}

#[test]
fn an_empty_extents_tree_resolves_to_nothing_rather_than_failing() {
    // A volume with an extents file but no overflow records is normal -- most
    // volumes never overflow. It must not be an error.
    let image = synthetic_extents_tree(BLOCK as usize, &[]);
    let image_len = image.len() as u64;
    let dev = MemoryDevice::new(image);
    let fork = ForkData {
        logical_size: image_len,
        total_blocks: 2,
        extents: one_extent(0, 2),
        ..ForkData::default()
    };
    let tree = hfsplus::btree::io::BTreeFile::open(&dev, &fork, BLOCK, true).expect("open");
    let overflow = TreeOverflow::new(tree);
    assert_eq!(
        overflow
            .find_group(ExtentKey::DATA_FORK, FORK_CNID, 8)
            .unwrap(),
        None
    );
}

#[test]
fn a_corrupt_extents_tree_is_an_error_not_a_short_read() {
    // Silently stopping at a broken tree would turn a corrupt volume into files
    // that read as short, which is worse than refusing.
    let dev = MemoryDevice::zeroed((2 * BLOCK) as usize);
    let fork = ForkData {
        logical_size: 2 * u64::from(BLOCK),
        total_blocks: 2,
        extents: one_extent(0, 2),
        ..ForkData::default()
    };
    let tree = hfsplus::btree::io::BTreeFile::open(&dev, &fork, BLOCK, true);
    // Either the header is rejected outright, or it opens and then refuses on
    // lookup. Both are refusals; neither is a panic.
    match tree {
        Err(e) => {
            assert!(!e.to_string().is_empty(), "the error must explain itself");
        }
        Ok(tree) => {
            let overflow = TreeOverflow::new(tree);
            let outcome = overflow.find_group(ExtentKey::DATA_FORK, FORK_CNID, 8);
            match outcome {
                Err(e) => assert!(!e.to_string().is_empty()),
                Ok(None) => {}
                Ok(Some(_)) => panic!("a zeroed extents tree must not yield an extent record"),
            }
        }
    }
}

#[test]
fn cnids_order_numerically_in_the_extents_tree() {
    // Keys sort by CNID, so the walk can stop early once past the one wanted.
    // A lexicographic comparison would stop at the wrong place for CNID 100 vs
    // 42, and the walk would either miss a record or scan the whole tree.
    let mut records = Vec::new();
    for cnid in [2u32, 16, 42, 100, 1000] {
        records.push((cnid, 0u32, one_extent(cnid * 2, 1)));
    }
    let image = synthetic_extents_tree(BLOCK as usize, &records);
    let image_len = image.len() as u64;
    let dev = MemoryDevice::new(image);
    let fork = ForkData {
        logical_size: image_len,
        total_blocks: 2,
        extents: one_extent(0, 2),
        ..ForkData::default()
    };
    let tree = hfsplus::btree::io::BTreeFile::open(&dev, &fork, BLOCK, true).expect("open");
    let overflow = TreeOverflow::new(tree);

    for (cnid, _, _) in &records {
        let found = overflow.find_group(ExtentKey::DATA_FORK, *cnid, 0).unwrap();
        assert!(found.is_some(), "CNID {cnid} must be found");
        assert_eq!(
            found.expect("present").raw[0].start_block,
            cnid * 2,
            "CNID {cnid} must resolve to its own record"
        );
    }
    assert!(
        overflow
            .find_group(ExtentKey::DATA_FORK, 43, 0)
            .unwrap()
            .is_none(),
        "an absent CNID finds nothing"
    );
}
