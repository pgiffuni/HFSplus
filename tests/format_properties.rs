//! Properties for extent accounting and B-tree record decoding.
//!
//! Both are places where an on-disk length is trusted far enough to index with,
//! which `AGENTS.md` names as the thing this crate must never do: "Never trust
//! an on-disk length. Bounds-check every field, every offset, every count."
//!
//! The adversarial input here is an **arbitrary offset array** -- every one of
//! its entries a random `u16`, which is what a corrupt or hostile B-tree node
//! looks like. No fixture in the corpus contains one, because the corpus is
//! written by a formatter that always lays nodes out consistently.
//!
//! No fuzzing harness and no dependency, by policy. The spread is a fixed-seed
//! xorshift so any failure reproduces exactly.
//!
//! Mining reference: `core/BTreeNodeOps.c` `GetRecordOffset` and
//! `GetNodeFreeSize`, and `lib_fsck_hfs/dfalib/SVerify2.c` for what Apple's own
//! checker validates about a node's geometry before reading it.

// Building fork fixtures field by field keeps each on-disk field visible next to
// the behaviour under test, so the struct-update lint is relaxed here -- as it is
// in the crate's own fixtures.
#![allow(clippy::field_reassign_with_default)]

mod common;

use hfsplus::btree::node::{Node, NODE_DESCRIPTOR_SIZE};
use hfsplus::btree::ExtentKey;
use hfsplus::format::fork::ForkData;

/// Fixed-seed spread, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

// --- B-tree records -----------------------------------------------------

/// A leaf node whose offset array is filled with arbitrary values.
///
/// `node_size` bytes of node, the descriptor claiming `num_records` records, and
/// an offset array that says nothing in particular. Any of it can be made to
/// disagree with anything else.
fn adversarial_node(node_size: usize, num_records: u16, rng: &mut Rng) -> Node<'static> {
    assert!(
        node_size >= 64,
        "a node smaller than this cannot hold an offset array"
    );
    // Leaked so the node can borrow it for 'static, which keeps the test free of
    // lifetime plumbing. One node per case and the cases are bounded, so this is
    // a few megabytes rather than a habit.
    let raw: &'static mut [u8] = vec![0u8; node_size].leak();
    // Records start after the descriptor, so an offset below that is nonsense
    // rather than merely arbitrary.
    let floor = NODE_DESCRIPTOR_SIZE as u64;
    let span = node_size as u64 - floor;
    // Deliberately *unsorted*. Sorting would make every offset non-decreasing,
    // so every record would be valid and the refusal paths would never run --
    // which is precisely what a consistent formatter produces and precisely what
    // this is here to contradict.
    let offsets: Vec<u16> = (0..num_records + 1)
        .map(|_| (floor + rng.below(span)).min(u64::from(u16::MAX)) as u16)
        .collect();
    for (i, v) in offsets.iter().enumerate() {
        let at = node_size - 2 * (i + 1);
        raw[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }
    raw[8] = 0xFF; // kBTLeafNode
    raw[9] = 1; // height
    raw[10..12].copy_from_slice(&num_records.to_be_bytes());
    Node::parse(raw, node_size).expect("a node whose descriptor is well formed")
}

#[test]
fn an_adversarial_offset_array_never_panics_and_never_exceeds_the_node() {
    // The core of "never trust an on-disk length". Every record the node claims
    // must either come back inside the node or be refused.
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut refused = 0usize;
    let mut returned = 0usize;

    for node_size in [64usize, 512, 4096, 8192] {
        let max_records = ((node_size - NODE_DESCRIPTOR_SIZE) / 2) as u16;
        for _ in 0..200 {
            let num_records = rng.below(u64::from(max_records) + 1) as u16;
            let node = adversarial_node(node_size, num_records, &mut rng);

            for index in 0..node.num_records() {
                match node.record(index) {
                    Ok(record) => {
                        returned += 1;
                        assert!(
                            record.as_ptr() as usize + record.len()
                                <= (node.raw().as_ptr() as usize + node_size),
                            "a record escaped the node: {record:?}"
                        );
                        // `record` bounds the slice by the node's own offset
                        // array and says nothing about what the bytes *inside*
                        // claim. That is the right split: a key length is the key
                        // parser's to validate, and `CatalogKey::from_record`
                        // takes the node's `maxKeyLength` for exactly that. So
                        // here the property is only that the bytes came from
                        // inside the node.
                        assert!(
                            record.as_ptr() as usize >= node.raw().as_ptr() as usize,
                            "a record pointed before the node"
                        );
                    }
                    Err(_) => refused += 1,
                }
            }

            // `free_offset` is the terminal entry of the same array, so it gets
            // the same treatment.
            if let Ok(free) = node.free_offset() {
                assert!(free <= node_size, "free offset {free} is past the node");
            }
        }
    }
    assert!(
        returned > 0 && refused > 0,
        "an adversarial array should produce both records and refusals, got \
         {returned} and {refused}"
    );
}

#[test]
fn a_record_index_past_the_count_is_refused_at_every_index() {
    // Including the values either side of `u16::MAX`, which a corrupt
    // `numRecords` could reach.
    let node_size = 4096usize;
    let mut rng = Rng(0x0BAD_C0DE_1234_5678);
    let num_records = rng.below(8) as u16;

    let mut raw = vec![0u8; node_size];
    raw[0..4].copy_from_slice(&0u32.to_be_bytes()); // fLink
    raw[4..8].copy_from_slice(&0u32.to_be_bytes()); // bLink
    raw[8] = 0xFF; // kBTLeafNode
    raw[9] = 1; // height
    raw[10..12].copy_from_slice(&num_records.to_be_bytes());
    let node = Node::parse(&raw, node_size).expect("a well-formed descriptor");

    for index in [num_records, num_records + 1, 1000, u16::MAX - 1, u16::MAX] {
        assert!(
            node.record(index).is_err(),
            "record {index} must be refused with {num_records} records"
        );
        assert!(
            node.record_range(index).is_err(),
            "the range for {index} must be refused too"
        );
    }
}

#[test]
fn free_space_never_goes_negative() {
    // `GetNodeFreeSize` subtracts the offset array from the node size; a node
    // whose array claims more space than exists must saturate rather than wrap.
    let node_size = 512usize;
    let mut rng = Rng(0xFEED_FACE_0000_0001);
    for num_records in [0u16, 1, 2, 10, 100] {
        let node = adversarial_node(node_size, num_records, &mut rng);
        if let Ok(free) = node.free_space() {
            assert!(
                free <= node_size,
                "node_size {node_size} with {num_records} records claimed {free} free"
            );
        }
    }
}

// --- Fork extent accounting --------------------------------------------

#[test]
fn overflow_accounting_is_consistent_for_every_extent_layout() {
    // `needs_overflow`, `overflow_block_count` and `validate` are three views of
    // the same arithmetic, and a fork that disagrees with itself would make the
    // reader walk an extent list it does not have.
    let mut rng = Rng(0xAAAA_BBBB_CCCC_DDDD);

    for _ in 0..500 {
        // Dense: descriptors up to a count, then terminators. A *hole* is not
        // representable -- `ExtentRecord::iter` stops at the first zero
        // descriptor, because a zero block count is the terminator -- so a layout
        // with a gap describes a shorter fork than it appears to.
        let mut fork = ForkData::default();
        let mut described = 0u64;
        let live = rng.below(9) as usize;
        for extent in fork.extents.raw.iter_mut().take(live) {
            // At least one block: a zero block count is the terminator, so a
            // live descriptor with zero would end the list before the rest.
            let count = 1 + rng.below(64) as u32;
            extent.start_block = rng.below(1 << 20) as u32;
            extent.block_count = count;
            described += u64::from(count);
        }

        let total = rng.below(1024) as u32;
        fork.total_blocks = total;
        fork.logical_size = u64::from(total) * 4096;

        assert_eq!(
            fork.inline_blocks(),
            described,
            "inline_blocks must be the sum of the descriptors before the terminator"
        );
        assert_eq!(
            fork.extents.used(),
            live.min(8),
            "used() counts the descriptors before the terminator"
        );
        assert_eq!(
            fork.overflow_block_count(),
            u64::from(total).saturating_sub(described),
            "the overflow count is what the inline record does not cover"
        );
        assert_eq!(
            fork.needs_overflow(),
            described < u64::from(total),
            "a fork needs overflow exactly when its inline record falls short"
        );
        // Whatever the layout, validation agrees with the arithmetic rather than
        // panicking on it.
        let _ = fork.validate(described, 4096);
    }
}

#[test]
fn a_fork_with_no_blocks_must_claim_no_bytes() {
    // The degenerate case, which `validate` handles separately and which a
    // multiplicative check would get wrong: zero blocks means zero capacity, so
    // any byte at all is unaccounted for.
    let mut fork = ForkData::default();
    assert!(fork.validate(0, 4096).is_ok());

    fork.logical_size = 1;
    let err = fork
        .validate(0, 4096)
        .expect_err("one byte in an empty fork");
    assert!(
        err.to_string().contains("logicalSize"),
        "the error must name the field, got {err}"
    );
}

#[test]
fn validate_agrees_with_itself_at_both_boundaries() {
    // `physical == described * block_size` must pass, and one block more must
    // not. Similarly `logical == physical` passes and one byte more does not.
    // The tolerance in between is ordinary and must not be rejected.
    let block: u32 = 4096;
    let mut fork = ForkData::default();
    fork.extents.raw[0] = hfsplus::format::extents::ExtentDescriptor {
        start_block: 10,
        block_count: 4,
    };
    fork.total_blocks = 4;
    let described = 4u64;

    // Exactly at both limits.
    fork.logical_size = 4u64 * u64::from(block);
    assert!(fork.validate(described, block).is_ok());

    // One byte of slack below is ordinary: a file's last block is rarely full.
    fork.logical_size = 4u64 * u64::from(block) - 1;
    assert!(fork.validate(described, block).is_ok());

    // One byte over the physical size is not.
    fork.logical_size = 4u64 * u64::from(block) + 1;
    assert!(fork.validate(described, block).is_err());

    // One block more claimed than the extents describe is not.
    fork.logical_size = 4u64 * u64::from(block);
    fork.total_blocks = 5;
    assert!(fork.validate(described, block).is_err());

    // And the extremes of the block size must not overflow the arithmetic.
    for block_size in [512u32, 1024, 4096, 65536] {
        let mut f = ForkData::default();
        f.total_blocks = 1;
        f.logical_size = u64::from(block_size);
        // One block described by a zero-start extent: capacity exactly one block.
        assert!(f.validate(1, block_size).is_ok());
        assert!(f.validate(0, block_size).is_err());
    }
}

#[test]
fn the_extent_key_round_trips_for_every_fork_type_and_cnid() {
    // Both forks of every CNID, including the reserved ones, because that is the
    // dimension `fork_type` exists to separate.
    for fork_type in [ExtentKey::DATA_FORK, ExtentKey::RESOURCE_FORK] {
        for cnid in [0u32, 1, 2, 3, 16, 17, 0xFFFF_FFFE, u32::MAX] {
            for start in [0u32, 1, 4096, 0x7FFF_FFFF, u32::MAX] {
                let key = ExtentKey {
                    fork_type,
                    file_id: cnid,
                    start_block: start,
                };
                let bytes = key.to_record();
                assert_eq!(bytes.len(), ExtentKey::ON_DISK_SIZE);
                assert_eq!(
                    ExtentKey::from_record(&bytes).expect("round trip"),
                    key,
                    "fork_type {fork_type:#x}, cnid {cnid}, start {start}"
                );
            }
        }
    }
}
