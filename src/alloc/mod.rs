// SPDX-License-Identifier: APSL-1.2

//! The allocator: deciding which blocks a file gets, and marking them used.
//!
//! This is the shared prerequisite for both directions. Writing a file has to
//! claim space, and checking a volume has to know what space is claimed -- a
//! checker that cannot allocate cannot tell an allocated block from an
//! orphaned one.
//!
//! # Bit order, again
//!
//! Allocation block `N` is bit `N` counting from the **most significant** bit of
//! each byte: block 0 is `0x80`, block 7 is `0x01`, block 8 is the `0x80` of the
//! second byte. 1 means allocated; there is no inversion. `VolumeAllocation.c`
//! stores the map in exactly this order, and `ReadBitmapBlock` addresses it that
//! way, so the same convention as `super::bitmap` -- which reads it.
//!
//! # Search order
//!
//! Apple does not allocate from the lowest free block. It tries a *hint* first,
//! which is either the caller's preferred start or the volume's next-allocation
//! pointer, and only then wraps. `hfs_block_alloc_int` searches
//! `[startingBlock, allocLimit)`, and on failure searches again from block 1 --
//! over the whole limit if the summary table is on, and only back to the hint if
//! it is off.
//!
//! `allocLimit` is not `totalBlocks`: the last blocks of a volume are never
//! handed out, because the backup volume header lives there. `hfsprogs`
//! demonstrates this, marking block `totalBlocks - 1` on a 32 MiB image.
//!
//! # Block 0 is never allocatable
//!
//! The allocatable range is `[FIRST_ALLOCATABLE_BLOCK, allocLimit)`. Block 0
//! holds the boot blocks and the volume header -- on the corpus images the
//! backup header sits there too, at `volumeBytes - 1024` -- and the bitmap file
//! follows it. An empty map marks block 0 free, because a *fresh* map has nothing
//! marked yet; counting it as free would overstate allocatable space by one and
//! make `NoSpace`'s `available` wrong. So the search never returns it and the
//! free count never includes it.
//!
//! # Scope
//!
//! The core operations only: reserve, release, and a first-fit search. Apple's
//! `BlockAllocate` also offers tentative and locked reservations, contiguous-only
//! allocation, and rollback, and consults a free-extent cache and a summary
//! table. None of those change *which* blocks a first-fit search finds, so they
//! are left out rather than guessed at -- and `reserve` is deliberately not
//! Apple's `HFS_ALLOC_TENTATIVE`, which marks blocks as used without committing
//! them.
//!
//! Mining reference: Apple `core/VolumeAllocation.c` `BlockFindAny`,
//! `BlockAllocateXxx`, `BlockDeallocateXxx`, `hfs_isallocated` and
//! `ReadBitmapRange`; `core/hfs_vfsutils.c` for where `allocLimit` comes from.

use crate::error::{Error, Result};

/// The volume's allocation bitmap, mutable, in memory.
///
/// Kept as whole bytes rather than a device so that reserving and releasing do
/// not each cost an I/O, and so the search can scan without touching the device
/// at all. Writing it back is a separate, explicit step -- see
/// [`AllocationMap::as_bytes`].
///
/// # Lifetime of a block
///
/// A block is only claimed once [`AllocationMap::reserve`] returns. Nothing here
/// commits anything: a caller that reserves and then fails must call
/// [`AllocationMap::release`], exactly as a caller of Apple's `BlockAllocate`
/// must free what it allocated if a later step fails.
/// The first block that may be handed out.
///
/// Block 0 carries the boot blocks and the volume header, and the allocation
/// bitmap is one of the forks immediately after it, so nothing file-sized can go
/// there. Mining reference: `core/hfs_vfsutils.c` reads the volume header from
/// block 0 and derives `allocLimit` from the end of the volume, leaving the
/// reserved range at both ends.
pub const FIRST_ALLOCATABLE_BLOCK: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationMap {
    bytes: Vec<u8>,
    total_blocks: u32,
    /// Blocks above this are never allocated.
    alloc_limit: u32,
}

impl AllocationMap {
    /// An empty map for a volume of `total_blocks` blocks.
    ///
    /// Every block reads free. A freshly formatted volume's bitmap is not this:
    /// the volume header, this file's own blocks and the catalog are already
    /// marked. Use [`AllocationMap::from_bytes`] to load one, or reserve the
    /// metadata by hand.
    pub fn empty(total_blocks: u32) -> Result<Self> {
        let bytes = crate::volume::bytes_for_blocks(total_blocks)?;
        Ok(AllocationMap {
            bytes: vec![0u8; bytes],
            total_blocks,
            alloc_limit: total_blocks,
        })
    }

    /// A map loaded from an allocation file's contents.
    ///
    /// Accepts a bitmap at least `total_blocks` bits long. Mining reference:
    /// `core/hfs_vfsutils.c` checks the allocation file's `logicalSize` against
    /// the size derived from `totalBlocks` before trusting the map, because a
    /// short one would report unallocated blocks as free.
    pub fn from_bytes(bytes: &[u8], total_blocks: u32) -> Result<Self> {
        let needed = crate::volume::bytes_for_blocks(total_blocks)?;
        if bytes.len() < needed {
            return Err(Error::Truncated {
                what: "allocation bitmap",
                needed,
                available: bytes.len(),
            });
        }
        Ok(AllocationMap {
            bytes: bytes[..needed].to_vec(),
            total_blocks,
            alloc_limit: total_blocks,
        })
    }

    /// Restrict allocation to blocks below `limit`.
    ///
    /// The last blocks of a volume hold the backup volume header and must stay
    /// free of file data. `mkfs.hfsplus` marks the final block on a freshly
    /// formatted image, which is that structure being accounted for.
    pub fn with_alloc_limit(mut self, limit: u32) -> Self {
        self.alloc_limit = limit.min(self.total_blocks);
        self
    }

    /// Highest block the allocator will ever return, exclusive.
    pub fn alloc_limit(&self) -> u32 {
        self.alloc_limit
    }

    /// Blocks the volume has.
    pub fn total_blocks(&self) -> u32 {
        self.total_blocks
    }

    /// Bytes needed to store the map.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always false: the map is sized for at least one block.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The map's bytes, for writing back to the allocation file.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable access to the underlying bytes.
    ///
    /// For loading a whole map, or for a caller that has its own byte layout to
    /// reconcile. Prefer [`AllocationMap::reserve`] and
    /// [`AllocationMap::release`] for anything that changes which blocks are
    /// in use.
    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Byte index and in-byte bit for a block.
    ///
    /// Returns `None` for a block the volume does not have. Every accessor below
    /// goes through this, so an out-of-range block is refused rather than
    /// indexing past the map.
    #[inline]
    fn bit(&self, block: u32) -> Option<(usize, u32)> {
        if block >= self.total_blocks {
            return None;
        }
        Some((((block / 8) as usize), block % 8))
    }

    /// Whether `block` is in use.
    ///
    /// # Errors
    ///
    /// Refuses a block beyond the volume. Mining reference: `hfs_isallocated`
    /// bounds its argument by the bitmap's block count and returns `bmapError`
    /// for anything above it, because a bit read from past the end would be
    /// read from a block that belongs to something else.
    pub fn is_allocated(&self, block: u32) -> Result<bool> {
        let (byte, shift) = self.bit(block).ok_or(Error::BadBlockNumber {
            block,
            total_blocks: self.total_blocks,
        })?;
        Ok(self.bytes[byte] & (0x80 >> shift) != 0)
    }

    /// Set or clear one block's bit.
    fn put(&mut self, block: u32, allocated: bool) -> Result<()> {
        let (byte, shift) = self.bit(block).ok_or(Error::BadBlockNumber {
            block,
            total_blocks: self.total_blocks,
        })?;
        let mask = 0x80u8 >> shift;
        if allocated {
            self.bytes[byte] |= mask;
        } else {
            self.bytes[byte] &= !mask;
        }
        Ok(())
    }

    /// Claim `count` contiguous blocks, searching from `hint`.
    ///
    /// Returns the first block of the allocation. `hint` of 0 means "no
    /// preference", which searches from block 1.
    ///
    /// The search runs from `hint` up to [`AllocationMap::alloc_limit`], then
    /// wraps to block 1 and continues to `hint`, so a hint pointing at a
    /// congested region does not fail an allocation that would fit earlier.
    /// Mining reference: `hfs_block_alloc_int` tries `BlockFindAny` over
    /// `[startingBlock, allocLimit)` and, on `dskFulErr`, tries again from block
    /// 1.
    ///
    /// # Errors
    ///
    /// [`Error::NoSpace`] when no run of `count` free blocks exists below the
    /// allocation limit, which is the same condition Apple reports as
    /// `dskFulErr`.
    pub fn reserve(&mut self, hint: u32, count: u32) -> Result<u32> {
        if count == 0 {
            return Err(Error::invalid("reserve", "a zero-block extent"));
        }
        let free = self.free_below_limit();
        if u64::from(count) > free {
            return Err(Error::no_space(count, free));
        }

        let start = if hint < FIRST_ALLOCATABLE_BLOCK {
            FIRST_ALLOCATABLE_BLOCK
        } else {
            hint.min(self.alloc_limit)
        };
        // Pass one: from the hint to the limit. Pass two: from block 1 to the
        // hint, so the whole allocatable range is covered exactly once.
        let first = self.find_run(start, self.alloc_limit, count);
        match first {
            Some(block) => {
                self.claim(block, count)?;
                Ok(block)
            }
            None => {
                let second = self.find_run(FIRST_ALLOCATABLE_BLOCK, start, count);
                let block = second.ok_or_else(|| Error::no_space(count, free))?;
                self.claim(block, count)?;
                Ok(block)
            }
        }
    }

    /// Claim one block, searching from `hint`.
    pub fn reserve_one(&mut self, hint: u32) -> Result<u32> {
        self.reserve(hint, 1)
    }

    /// Mark `count` blocks starting at `start` as in use.
    fn claim(&mut self, start: u32, count: u32) -> Result<()> {
        for offset in 0..count {
            self.put(start + offset, true)?;
        }
        Ok(())
    }

    /// Release `count` blocks from `start`.
    ///
    /// # Errors
    ///
    /// Refuses a range that runs past the end of the volume, or that reaches
    /// into the tail above the allocation limit: releasing a block the
    /// allocator never handed out would mark the backup volume header free.
    pub fn release(&mut self, start: u32, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let end = u64::from(start) + u64::from(count);
        if end > u64::from(self.total_blocks) {
            return Err(Error::out_of_range(
                "extent",
                end,
                u64::from(self.total_blocks),
            ));
        }
        if end > u64::from(self.alloc_limit) {
            return Err(Error::out_of_range(
                "extent end",
                end,
                u64::from(self.alloc_limit),
            ));
        }
        for offset in 0..count {
            self.put(start + offset, false)?;
        }
        Ok(())
    }

    /// First run of `count` free blocks in `[from, to)`, if there is one.
    ///
    /// The range is clamped up to [`FIRST_ALLOCATABLE_BLOCK`], so a caller
    /// passing 0 -- the "no preference" hint -- searches from block 1 and never
    /// hands out the volume header.
    fn find_run(&self, from: u32, to: u32, count: u32) -> Option<u32> {
        let from = from.max(FIRST_ALLOCATABLE_BLOCK);
        if from >= to || count == 0 {
            return None;
        }
        let mut run_start = 0u32;
        let mut run = 0u32;
        let mut block = from;
        while block < to {
            let (byte, shift) = self.bit(block)?;
            let free = self.bytes[byte] & (0x80 >> shift) == 0;
            if free {
                if run == 0 {
                    run_start = block;
                }
                run += 1;
                if run == count {
                    return Some(run_start);
                }
            } else {
                run = 0;
            }
            block += 1;
        }
        None
    }

    /// Number of allocatable free blocks in `[FIRST_ALLOCATABLE_BLOCK, to)`.
    fn count_free_below(&self, to: u32) -> u64 {
        let to = to.min(self.total_blocks);
        (FIRST_ALLOCATABLE_BLOCK..to)
            .filter(|b| self.is_allocated(*b) == Ok(false))
            .count() as u64
    }

    /// Blocks in use across the whole volume, above the limit included.
    ///
    /// The metadata above the allocation limit counts: the backup volume header
    /// is genuinely allocated, and a checker comparing this against
    /// `totalBlocks - freeBlocks` needs them both.
    pub fn count_allocated(&self) -> u64 {
        (0..self.total_blocks)
            .filter(|b| self.is_allocated(*b) == Ok(true))
            .count() as u64
    }

    /// Blocks free below the allocation limit.
    pub fn free_below_limit(&self) -> u64 {
        self.count_free_below(self.alloc_limit)
    }

    /// Blocks in use that nothing references.
    ///
    /// `referenced` is the set of blocks the catalog's extents account for. The
    /// difference is what `fsck` reports as orphaned blocks, and it is the check
    /// that catches a fork whose extents were never marked.
    pub fn orphaned(&self, referenced: &dyn Fn(u32) -> bool) -> Vec<u32> {
        (0..self.total_blocks)
            .filter(|b| self.is_allocated(*b) == Ok(true) && !referenced(*b))
            .collect()
    }

    /// Blocks referenced but not marked in use.
    ///
    /// The other half of the same disagreement, and the one that catches a fork
    /// whose extents were written without updating the bitmap.
    pub fn missing(&self, referenced: &dyn Fn(u32) -> bool) -> Vec<u32> {
        (0..self.total_blocks)
            .filter(|b| self.is_allocated(*b) == Ok(false) && referenced(*b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map of `total` blocks with everything free.
    fn free_map(total: u32) -> AllocationMap {
        AllocationMap::empty(total).expect("map")
    }

    #[test]
    fn bits_run_from_the_most_significant_end() {
        let mut map = free_map(16);
        map.put(0, true).unwrap();
        map.put(7, true).unwrap();
        map.put(8, true).unwrap();
        // Block 0 is 0x80, block 7 is 0x01, block 8 is the next byte's 0x80.
        assert_eq!(map.as_bytes()[0], 0b1000_0001);
        assert_eq!(map.as_bytes()[1], 0b1000_0000);
        for block in [0u32, 7, 8] {
            assert!(map.is_allocated(block).unwrap(), "block {block}");
        }
        assert!(!map.is_allocated(1).unwrap());
    }

    #[test]
    fn a_block_past_the_end_is_refused_not_read() {
        let map = free_map(16);
        let err = map.is_allocated(16).expect_err("block 16 does not exist");
        assert!(
            matches!(err, Error::BadBlockNumber { block: 16, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn reserve_returns_the_hint_when_it_is_free() {
        let mut map = free_map(64);
        assert_eq!(map.reserve(20, 4).unwrap(), 20);
        for block in 20..24 {
            assert!(map.is_allocated(block).unwrap(), "block {block}");
        }
        assert!(!map.is_allocated(19).unwrap());
        assert!(!map.is_allocated(24).unwrap());
    }

    #[test]
    fn reserve_walks_past_a_hole_to_the_next_run() {
        let mut map = free_map(64);
        map.claim(20, 3).unwrap();
        // The hint is inside the used run, so the search continues past it.
        assert_eq!(map.reserve(21, 2).unwrap(), 23);
    }

    #[test]
    fn reserve_wraps_when_the_tail_is_too_short() {
        let mut map = free_map(64);
        map.claim(58, 6).unwrap();
        // The tail is all used, so the search wraps to the front.
        assert_eq!(map.reserve(60, 4).unwrap(), 1);
        for block in 1..5 {
            assert!(map.is_allocated(block).unwrap(), "block {block}");
        }
        assert_eq!(
            map.free_below_limit(),
            64 - 1 - 6 - 4,
            "blocks 1..57 less the four just taken; block 0 is never allocatable"
        );
    }

    #[test]
    fn reserve_wraps_even_when_free_space_exists_ahead_of_the_hint() {
        let mut map = free_map(32);
        // Everything from 4 up is used; blocks 1..3 are the only run.
        map.claim(4, 28).unwrap();
        assert_eq!(map.reserve(20, 3).unwrap(), 1);
    }

    #[test]
    fn reserve_reports_no_space_rather_than_a_short_allocation() {
        let mut map = free_map(16);
        map.claim(1, 15).unwrap();
        // Block 0 is still marked free but is not allocatable, so `available`
        // must not count it -- otherwise a full volume reports one free block
        // and the caller retries for ever.
        assert!(
            !map.is_allocated(0).unwrap(),
            "block 0 reads free on a fresh map"
        );
        assert_eq!(map.free_below_limit(), 0);
        match map.reserve(1, 1) {
            Err(Error::NoSpace {
                requested,
                available,
            }) => {
                assert_eq!(requested, 1);
                assert_eq!(available, 0);
            }
            other => panic!("expected NoSpace, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_block_extent_is_an_error_not_a_free_allocation() {
        let mut map = free_map(16);
        assert!(
            map.reserve(1, 0).is_err(),
            "reserving nothing is a caller bug"
        );
    }

    #[test]
    fn the_allocation_limit_keeps_the_tail_unreachable() {
        let mut map = free_map(64).with_alloc_limit(60);
        assert_eq!(map.alloc_limit(), 60);
        // Blocks 1..59 are allocatable; block 0 and 60..63 are not counted, so a
        // 64-block volume with a limit of 60 offers 59 blocks and not 64.
        assert_eq!(map.free_below_limit(), 59);
        assert_eq!(map.reserve(58, 2).unwrap(), 58);
        assert!(
            !map.is_allocated(60).unwrap() && !map.is_allocated(63).unwrap(),
            "the tail must stay free however hard the allocator is asked"
        );

        // Fill everything allocatable. There are ten free blocks in the image,
        // all of them above the limit, and the allocator must still report none.
        map.claim(1, 57).unwrap();
        assert_eq!(map.free_below_limit(), 0);
        match map.reserve(1, 1) {
            Err(Error::NoSpace {
                requested,
                available,
            }) => {
                assert_eq!(requested, 1);
                assert_eq!(available, 0, "the tail is free but not allocatable");
            }
            other => panic!("expected NoSpace, got {other:?}"),
        }
        for block in 60..64 {
            assert!(
                !map.is_allocated(block).unwrap(),
                "block {block} stays free"
            );
        }
    }

    #[test]
    fn release_returns_blocks_and_refuses_to_reach_into_the_tail() {
        let mut map = free_map(64).with_alloc_limit(60);
        map.claim(10, 4).unwrap();
        map.release(10, 4).unwrap();
        for block in 10..14 {
            assert!(
                !map.is_allocated(block).unwrap(),
                "block {block} should be free"
            );
        }
        assert!(
            map.release(58, 4).is_err(),
            "releasing across the limit would free the backup volume header"
        );
        assert!(
            map.release(62, 4).is_err(),
            "and past the end is out of range"
        );
    }

    #[test]
    fn allocating_repeatedly_from_one_hint_walks_forward() {
        let mut map = free_map(1024);
        let mut starts = Vec::new();
        for _ in 0..16 {
            starts.push(map.reserve(1, 8).unwrap());
        }
        // Each allocation takes the free run left behind by the previous hint,
        // so the starts are contiguous rather than all at the hint.
        assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
        assert_eq!(starts[0], 1);
        assert_eq!(starts[1], 9);
        assert_eq!(map.count_allocated(), 16 * 8);
    }

    #[test]
    fn orphaned_and_missing_name_both_halves_of_a_disagreement() {
        let mut map = free_map(32);
        // 10..12 claimed; the catalog says 10..14.
        map.claim(10, 2).unwrap();
        let catalog: Vec<u32> = (10..14).collect();
        let refs = |b: u32| catalog.contains(&b);

        assert_eq!(map.orphaned(&refs), Vec::<u32>::new());
        assert_eq!(
            map.missing(&refs),
            vec![12, 13],
            "a fork written without updating the bitmap shows up here"
        );

        // And the other direction: a block marked used that nothing points at.
        map.claim(20, 1).unwrap();
        assert_eq!(map.orphaned(&refs), vec![20]);
    }

    #[test]
    fn a_short_bitmap_is_refused() {
        let err = AllocationMap::from_bytes(&[0u8; 3], 64).expect_err("64 blocks need 8 bytes");
        match err {
            Error::Truncated {
                what,
                needed,
                available,
            } => {
                assert_eq!(what, "allocation bitmap");
                assert_eq!(needed, 8);
                assert_eq!(available, 3);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn a_map_round_trips_through_its_bytes() {
        let mut map = free_map(128);
        map.claim(3, 5).unwrap();
        let reloaded = AllocationMap::from_bytes(map.as_bytes(), 128).expect("reload");
        assert_eq!(reloaded, map);
        for block in 3..8 {
            assert!(reloaded.is_allocated(block).unwrap());
        }
    }
}
