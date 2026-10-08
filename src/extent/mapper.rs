// SPDX-License-Identifier: APSL-1.2

//! Mapping a fork-relative block to a physical allocation block.
//!
//! # The mapping chain
//!
//! A B-tree node number, a file byte offset and a device byte offset are three
//! different things. The chain between them is:
//!
//! ```text
//! B-tree node number
//!     -> byte offset within the fork      (node_number * node_size)
//!     -> fork-relative allocation block   (byte_offset / block_size)
//!     -> physical allocation block        (extent mapping)
//!     -> device byte offset               (block * block_size)
//! ```
//!
//! Only the last two steps live here. The first is trivial arithmetic that
//! belongs to the B-tree layer, and the fourth is the block device's job.
//!
//! Mining reference: Apple `core/FileExtentMapping.c` (`MapFileBlockC`,
//! `MapFileBlockC_noPerm`) performs exactly this fork-block to physical-block
//! translation, walking the inline extents and falling through to the Extents
//! B-tree when they run out. `core/hfs_extents.c` (`hfs_ext_iter_init`,
//! `hfs_ext_iter_next_group`) is the iterator being driven.
//!
//! # Overflow is keyed on allocated blocks
//!
//! A fork's `totalBlocks` counts every allocation block it occupies, across all
//! extents including overflow records. The eight inline descriptors cover only
//! the first `kHFSPlusExtentDensity` groups; anything past them lives in the
//! Extents B-tree under a key whose `startBlock` is the running count of blocks
//! already described. So a sparse file with a large `logicalSize` and few
//! allocated blocks never reaches the overflow B-tree at all: the unallocated
//! region is a hole, not an extent.

use crate::error::{Error, Result};
use crate::format::extents::ExtentRecord;

/// Supplies extent records from the volume's Extents B-tree.
///
/// Kept as a trait so the mapper does not depend on the B-tree layer, and the
/// B-tree layer does not depend on the mapper. The volume constructs both once
/// the Extents B-tree is open.
pub trait OverflowResolver {
    /// Return the eight extent descriptors for the group beginning at
    /// `start_block`, which is a count of allocation blocks already covered by
    /// earlier groups, not a physical block number.
    ///
    /// Returns `Ok(None)` when no such group exists.
    fn resolve_group(&self, start_block: u32) -> Result<Option<ExtentRecord>>;
}

/// Maps fork-relative allocation blocks to physical allocation blocks.
pub struct ExtentMapper<'a> {
    inline: ExtentRecord,
    /// Total allocation blocks claimed by the fork, inline and overflow together.
    total_blocks: u32,
    block_size: u32,
    overflow: Option<Box<dyn OverflowResolver + Send + Sync + 'a>>,
}

impl std::fmt::Debug for ExtentMapper<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtentMapper")
            .field("inline_extents", &self.inline.used())
            .field("total_blocks", &self.total_blocks)
            .field("block_size", &self.block_size)
            .field("overflow", &self.overflow.is_some())
            .finish()
    }
}

impl<'a> ExtentMapper<'a> {
    /// Build a mapper for a fork whose extents may all be inline.
    pub fn new(fork: &crate::format::fork::ForkData, block_size: u32) -> Self {
        ExtentMapper {
            inline: fork.extents,
            total_blocks: fork.total_blocks,
            block_size,
            overflow: None,
        }
    }

    /// Attach an Extents B-tree for forks that overflow.
    ///
    /// Mining reference: Apple `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`)
    /// builds the Extents B-tree vnode first, precisely because the overflow
    /// lookup for other forks depends on it. Note that the Extents B-tree's own
    /// fork is opened *without* a resolver: it is not allowed to overflow into
    /// itself.
    pub fn with_overflow(mut self, resolver: Box<dyn OverflowResolver + Send + Sync + 'a>) -> Self {
        self.overflow = Some(resolver);
        self
    }

    /// Total allocation blocks this fork claims.
    pub fn total_blocks(&self) -> u32 {
        self.total_blocks
    }

    /// Allocation block size of the volume.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Whether any allocated block lies beyond the inline extents.
    pub fn needs_overflow(&self) -> bool {
        self.inline.total_blocks() < u64::from(self.total_blocks)
    }

    /// Translate a fork-relative allocation block to a physical one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutOfRange`] when the block is past the fork's
    /// allocated blocks, which includes every offset inside a sparse hole.
    pub fn map_block(&self, fork_block: u32) -> Result<u32> {
        let target = u64::from(fork_block);
        if target >= u64::from(self.total_blocks) {
            return Err(Error::out_of_range(
                "fork block",
                u64::from(fork_block),
                u64::from(self.total_blocks),
            ));
        }
        if let Some(phys) = self.walk(&self.inline, target) {
            return Ok(phys);
        }
        self.map_via_overflow(fork_block, target)
    }

    /// Translate a byte offset within the fork to a physical allocation block.
    ///
    /// A byte offset that lands past the fork's *allocated* blocks is not an
    /// error: it is a sparse hole. Callers that need to know should compare
    /// against the fork's logical size themselves.
    pub fn map_byte_offset(&self, offset: u64) -> Result<Option<u32>> {
        let block = offset / u64::from(self.block_size);
        if block >= u64::from(self.total_blocks) {
            return Ok(None);
        }
        match self.map_block(block as u32) {
            Ok(p) => Ok(Some(p)),
            // A hole below the logical size still resolves to "not on disk".
            Err(Error::OutOfRange { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Translate a fork-relative block to a device byte offset.
    pub fn map_to_device_offset(&self, fork_block: u32, byte_in_block: u64) -> Result<u64> {
        let phys = u64::from(self.map_block(fork_block)?);
        phys.checked_mul(u64::from(self.block_size))
            .and_then(|base| base.checked_add(byte_in_block))
            .ok_or(Error::overflow("fork to device offset"))
    }

    /// Find `target` within one extent group.
    ///
    /// Returns the physical block if the target lies in this group.
    fn walk(&self, group: &ExtentRecord, target: u64) -> Option<u32> {
        let mut seen = 0u64;
        for desc in group.iter() {
            let count = u64::from(desc.block_count);
            if target < seen + count {
                let within = target - seen;
                return desc.start_block.checked_add(within as u32);
            }
            seen += count;
        }
        None
    }

    /// Walk the overflow B-tree for groups beyond the inline extents.
    ///
    /// Mining reference: `core/hfs_extents.c` advances the overflow key by
    /// `hfs_total_blocks(&extents[ndx], kHFSPlusExtentDensity)` for each group,
    /// so the key handed to the resolver is the cumulative block count.
    fn map_via_overflow(&self, fork_block: u32, target: u64) -> Result<u32> {
        let Some(resolver) = self.overflow.as_ref() else {
            return Err(Error::out_of_range(
                "fork block",
                u64::from(fork_block),
                u64::from(self.total_blocks),
            ));
        };

        // Start looking past the inline extents.
        let mut seen = self.inline.total_blocks();
        // Bounded by the fork's block count, so a corrupt or looping resolver
        // cannot make this spin.
        let mut guard = 0u32;
        while seen < u64::from(self.total_blocks) {
            guard += 1;
            if guard > MAX_OVERFLOW_GROUPS {
                return Err(Error::overflow("overflow extent groups"));
            }
            let key = u32::try_from(seen).map_err(|_| Error::overflow("overflow extent key"))?;
            let Some(group) = resolver.resolve_group(key)? else {
                break;
            };
            if group.total_blocks() == 0 {
                break;
            }
            if let Some(phys) = self.walk(&group, target.wrapping_sub(seen)) {
                return Ok(phys);
            }
            seen += group.total_blocks();
        }

        Err(Error::out_of_range(
            "fork block",
            u64::from(fork_block),
            u64::from(self.total_blocks),
        ))
    }
}

/// One contiguous range of allocation blocks in a fork's extent chain.
///
/// A single allocation-descriptor pair from the on-disk extent chain:
/// the first fork-relative block it covers, how many blocks long the run is,
/// and where those blocks live on the device. Holes between extents (sparse
/// regions) are not represented here -- they are the absence of a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentRange {
    /// First fork-relative allocation block covered by this range.
    pub fork_block: u32,
    /// Number of allocation blocks in this run.
    pub block_count: u32,
    /// First physical allocation block on the device.
    pub physical_block: u32,
}

impl ExtentRange {
    /// Last fork-relative block covered, inclusive.
    ///
    /// `None` when the range is empty, which should not occur for ranges
    /// yielded by [`ExtentRanges`].
    pub fn end_fork_block(&self) -> Option<u32> {
        if self.block_count == 0 {
            None
        } else {
            Some(self.fork_block + self.block_count - 1)
        }
    }

    /// Last physical block covered, inclusive.
    pub fn end_physical_block(&self) -> Option<u32> {
        if self.block_count == 0 {
            None
        } else {
            Some(self.physical_block + self.block_count - 1)
        }
    }
}

/// Iterate over every extent range in a fork, inline extents first then
/// overflow groups from the Extents B-tree.
///
/// Each item is an [`ExtentRange`] describing one contiguous run of
/// fork-allocated blocks. Sparse holes between extents are skipped -- this
/// iterator walks only what is actually allocated, which is exactly what an
/// LSEEK SEEK_DATA/SEEK_HOLE scan needs.
///
/// Mining reference: Apple `core/hfs_extents.c` (`hfs_ext_iter_init`,
/// `hfs_ext_iter_next_group`) walks the inline extents then the overflow groups
/// in the same shape.
pub struct ExtentRanges<'a> {
    mapper: &'a ExtentMapper<'a>,
    inline_idx: usize,
    consumed: u64,
    overflow_group: Option<ExtentRecord>,
    overflow_idx: usize,
    overflow_fetch_count: u32,
}

impl<'a> std::fmt::Debug for ExtentRanges<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtentRanges")
            .field("inline_idx", &self.inline_idx)
            .field("consumed", &self.consumed)
            .field("overflow_idx", &self.overflow_idx)
            .field("overflow_fetch_count", &self.overflow_fetch_count)
            .finish()
    }
}

impl<'a> ExtentRanges<'a> {
    pub fn new(mapper: &'a ExtentMapper<'a>) -> Self {
        ExtentRanges {
            mapper,
            inline_idx: 0,
            consumed: 0,
            overflow_group: None,
            overflow_idx: 0,
            overflow_fetch_count: 0,
        }
    }
}

impl<'a> Iterator for ExtentRanges<'a> {
    type Item = Result<ExtentRange>;

    fn next(&mut self) -> Option<Self::Item> {
        let used_inline = self.mapper.inline.used();

        if self.inline_idx < used_inline {
            let desc = self.mapper.inline.raw[self.inline_idx];
            self.inline_idx += 1;
            let range = ExtentRange {
                fork_block: self.consumed as u32,
                block_count: desc.block_count,
                physical_block: desc.start_block,
            };
            self.consumed += u64::from(desc.block_count);
            return Some(Ok(range));
        }

        loop {
            if let Some(group) = &self.overflow_group {
                if self.overflow_idx < group.used() {
                    let desc = group.raw[self.overflow_idx];
                    self.overflow_idx += 1;
                    let range = ExtentRange {
                        fork_block: self.consumed as u32,
                        block_count: desc.block_count,
                        physical_block: desc.start_block,
                    };
                    self.consumed += u64::from(desc.block_count);
                    return Some(Ok(range));
                }
            }

            if self.consumed >= u64::from(self.mapper.total_blocks) {
                return None;
            }

            self.overflow_fetch_count += 1;
            if self.overflow_fetch_count > MAX_OVERFLOW_GROUPS {
                return Some(Err(Error::overflow("overflow extent groups")));
            }

            let resolver = self.mapper.overflow.as_ref()?;
            let key = match u32::try_from(self.consumed) {
                Ok(k) => k,
                Err(_) => return Some(Err(Error::overflow("overflow extent key"))),
            };
            match resolver.resolve_group(key) {
                Ok(Some(group)) => {
                    self.overflow_group = Some(group);
                    self.overflow_idx = 0;
                }
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

impl<'a> ExtentMapper<'a> {
    /// Iterate over every extent range in this fork.
    ///
    /// Yields [`ExtentRange`] items in fork-block order, inline extents first
    /// then overflow groups. Holes (unallocated blocks between extents) are
    /// not emitted; callers that need to know about holes must compare the
    /// fork-block boundaries themselves.
    pub fn ranges(&self) -> ExtentRanges<'_> {
        ExtentRanges::new(self)
    }

    /// Find the extent range covering `fork_block`, if any.
    ///
    /// Returns the descriptor whose `[fork_block, fork_block + count)` range
    /// contains `fork_block`, or `Err(OutOfRange)` when the block is past all
    /// allocated blocks. A sparse hole below the logical size yields `Ok(None)`:
    /// the block index is within the fork's allocation but no descriptor covers
    /// it.
    pub fn range_at(&self, fork_block: u32) -> Result<Option<ExtentRange>> {
        let target = u64::from(fork_block);
        if target >= u64::from(self.total_blocks) {
            return Err(Error::out_of_range(
                "fork block",
                u64::from(fork_block),
                u64::from(self.total_blocks),
            ));
        }
        for range in self.ranges() {
            let range = range?;
            let start = u64::from(range.fork_block);
            let end = start + u64::from(range.block_count);
            if target >= start && target < end {
                return Ok(Some(range));
            }
        }
        Ok(None)
    }

    /// BMAP: translate a fork-relative byte offset to a device byte offset.
    ///
    /// The offset must be aligned to the volume's allocation block size, as
    /// FUSE's BMAP opcode requires. A hole (allocated block count less than the
    /// logical size) yields `Error::OutOfRange`.
    pub fn bmap(&self, offset: u64) -> Result<u64> {
        let block_size = u64::from(self.block_size);
        let unaligned = offset % block_size;
        if unaligned != 0 {
            return Err(Error::invalid(
                "offset",
                "not aligned to the allocation block size",
            ));
        }
        let fork_block = u32::try_from(offset / block_size)
            .map_err(|_| Error::overflow("fork block from bmap offset"))?;
        self.map_to_device_offset(fork_block, 0)
    }
}
/// Hard cap on overflow groups examined for one lookup.
///
/// The Extents B-tree cannot contain more groups than a fork has blocks, and a
/// fork's block count is bounded by `u32::MAX` divided by the volume block size.
/// This bound is far below any real value and exists purely so that a malicious
/// or corrupt Extents B-tree cannot make a single block lookup run unbounded.
const MAX_OVERFLOW_GROUPS: u32 = 1 << 20;

/// A resolver that always reports "no such group", for trees that do not
/// overflow.
#[derive(Debug, Clone, Copy)]
pub struct NoOverflow;

impl OverflowResolver for NoOverflow {
    fn resolve_group(&self, _start_block: u32) -> Result<Option<ExtentRecord>> {
        Ok(None)
    }
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::format::extents::ExtentDescriptor;
    use crate::format::fork::ForkData;

    fn fork_with(extents: &[(u32, u32)], total_blocks: u32) -> ForkData {
        let mut f = ForkData::default();
        f.clump_size = 4096;
        f.logical_size = u64::from(total_blocks) * 4096;
        f.total_blocks = total_blocks;
        for (i, (start, count)) in extents.iter().enumerate() {
            f.extents.raw[i] = ExtentDescriptor {
                start_block: *start,
                block_count: *count,
            };
        }
        f
    }

    #[test]
    fn maps_blocks_within_a_single_extent() {
        let f = fork_with(&[(100, 10)], 10);
        let m = ExtentMapper::new(&f, 4096);
        assert_eq!(m.map_block(0).unwrap(), 100);
        assert_eq!(m.map_block(9).unwrap(), 109);
    }

    #[test]
    fn maps_across_contiguous_extent_groups() {
        let f = fork_with(&[(100, 4), (200, 4)], 8);
        let m = ExtentMapper::new(&f, 4096);
        assert_eq!(m.map_block(3).unwrap(), 103);
        assert_eq!(m.map_block(4).unwrap(), 200);
        assert_eq!(m.map_block(7).unwrap(), 203);
    }

    #[test]
    fn blocks_past_the_fork_are_out_of_range() {
        let f = fork_with(&[(100, 4)], 4);
        let m = ExtentMapper::new(&f, 4096);
        assert!(matches!(m.map_block(4), Err(Error::OutOfRange { .. })));
        assert!(matches!(
            m.map_block(u32::MAX),
            Err(Error::OutOfRange { .. })
        ));
    }

    #[test]
    fn sparse_hole_resolves_to_none_not_an_error() {
        // One allocated block, but a large logical size: the rest is a hole.
        let mut f = fork_with(&[(500, 1)], 1);
        f.logical_size = 10 * 4096;
        let m = ExtentMapper::new(&f, 4096);

        assert_eq!(m.map_byte_offset(0).unwrap(), Some(500));
        // Block 1 is inside the logical size but unallocated.
        assert_eq!(m.map_byte_offset(4096).unwrap(), None);
        assert_eq!(m.map_byte_offset(9 * 4096).unwrap(), None);
        // Past the logical size entirely.
        assert_eq!(m.map_byte_offset(100 * 4096).unwrap(), None);
    }

    #[test]
    fn device_offset_computation_is_checked() {
        let f = fork_with(&[(7, 4)], 4);
        let m = ExtentMapper::new(&f, 4096);
        assert_eq!(m.map_to_device_offset(0, 0).unwrap(), 7 * 4096);
        assert_eq!(m.map_to_device_offset(1, 12).unwrap(), 8 * 4096 + 12);
        assert!(m.map_to_device_offset(9, 0).is_err());
    }

    /// A resolver that serves one fixed overflow group.
    struct Fixed(ExtentRecord);

    impl OverflowResolver for Fixed {
        fn resolve_group(&self, start_block: u32) -> Result<Option<ExtentRecord>> {
            if start_block == 16 {
                Ok(Some(self.0))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn overflow_groups_are_consulted_after_the_inline_extents() {
        // Two inline extents cover 16 blocks; the fork claims 24.
        let f = fork_with(&[(100, 8), (200, 8)], 24);
        assert!(ExtentMapper::new(&f, 4096).needs_overflow());

        let mut group = ExtentRecord::EMPTY;
        group.raw[0] = ExtentDescriptor {
            start_block: 300,
            block_count: 8,
        };

        let m = ExtentMapper::new(&f, 4096).with_overflow(Box::new(Fixed(group)));
        assert_eq!(m.map_block(0).unwrap(), 100);
        assert_eq!(m.map_block(15).unwrap(), 207);
        // Block 16 is the first block of the overflow group.
        assert_eq!(m.map_block(16).unwrap(), 300);
        assert_eq!(m.map_block(23).unwrap(), 307);
        // Past the fork's allocated blocks.
        assert!(m.map_block(24).is_err());
    }

    #[test]
    fn overflow_lookup_without_a_resolver_is_out_of_range() {
        let f = fork_with(&[(100, 8), (200, 8)], 24);
        let m = ExtentMapper::new(&f, 4096);
        assert_eq!(m.map_block(15).unwrap(), 207);
        assert!(matches!(m.map_block(16), Err(Error::OutOfRange { .. })));
    }

    #[test]
    fn a_resolver_returning_nothing_terminates_the_walk() {
        let f = fork_with(&[(100, 8)], 100);
        let m = ExtentMapper::new(&f, 4096).with_overflow(Box::new(NoOverflow));
        assert_eq!(m.map_block(0).unwrap(), 100);
        assert!(matches!(m.map_block(20), Err(Error::OutOfRange { .. })));
    }

    fn collect_ranges(m: &ExtentMapper) -> Vec<ExtentRange> {
        m.ranges().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn ranges_yields_inline_extents_in_order() {
        let f = fork_with(&[(100, 4), (200, 4)], 8);
        let m = ExtentMapper::new(&f, 4096);
        let ranges = collect_ranges(&m);

        assert_eq!(ranges.len(), 2);
        assert_eq!(
            ranges[0],
            ExtentRange {
                fork_block: 0,
                block_count: 4,
                physical_block: 100,
            }
        );
        assert_eq!(
            ranges[1],
            ExtentRange {
                fork_block: 4,
                block_count: 4,
                physical_block: 200,
            }
        );
    }

    #[test]
    fn ranges_yields_fragmented_extents_in_order() {
        // Two inline extents: the file is fragmented on disk but the fork
        // blocks are still covered sequentially with no gap.
        let f = fork_with(&[(500, 1), (300, 2)], 3);
        let m = ExtentMapper::new(&f, 4096);

        let ranges = collect_ranges(&m);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].fork_block, 0);
        assert_eq!(ranges[0].block_count, 1);
        assert_eq!(ranges[0].physical_block, 500);
        assert_eq!(ranges[1].fork_block, 1);
        assert_eq!(ranges[1].block_count, 2);
        assert_eq!(ranges[1].physical_block, 300);
    }

    #[test]
    fn ranges_continues_into_overflow() {
        let f = fork_with(&[(100, 8), (200, 8)], 24);
        let mut group = ExtentRecord::EMPTY;
        group.raw[0] = ExtentDescriptor {
            start_block: 300,
            block_count: 8,
        };
        let m = ExtentMapper::new(&f, 4096).with_overflow(Box::new(Fixed(group)));

        let ranges = collect_ranges(&m);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[2].fork_block, 16);
        assert_eq!(ranges[2].block_count, 8);
        assert_eq!(ranges[2].physical_block, 300);
    }

    #[test]
    fn ranges_with_no_overflow_stops_at_inline() {
        let f = fork_with(&[(100, 4)], 4);
        let m = ExtentMapper::new(&f, 4096);
        let ranges = collect_ranges(&m);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].fork_block, 0);
        assert_eq!(ranges[0].block_count, 4);
    }

    #[test]
    fn range_at_finds_covering_extent() {
        let f = fork_with(&[(50, 4), (200, 4)], 8);
        let m = ExtentMapper::new(&f, 4096);

        let r = m.range_at(0).unwrap().unwrap();
        assert_eq!(r.physical_block, 50);

        let r = m.range_at(3).unwrap().unwrap();
        assert_eq!(r.physical_block, 50);

        // Block 4..7 covers the second extent starting at fork block 4.
        let r = m.range_at(5).unwrap().unwrap();
        assert_eq!(r.physical_block, 200);
    }

    #[test]
    fn range_at_none_past_allocated_blocks() {
        let mut f = fork_with(&[(100, 1)], 1);
        // One allocated block but logical size implies 10: blocks 1..9 are
        // holes beyond the extent chain.
        f.logical_size = 10 * 4096;
        let m = ExtentMapper::new(&f, 4096);

        // Block 0 is covered.
        assert!(m.range_at(0).unwrap().is_some());
        // Block 1 is past total_blocks (1), so it is out of range, not a hole.
        assert!(m.range_at(1).is_err());
    }

    #[test]
    fn extent_range_end_block_helpers() {
        let r = ExtentRange {
            fork_block: 10,
            block_count: 5,
            physical_block: 100,
        };
        assert_eq!(r.end_fork_block(), Some(14));
        assert_eq!(r.end_physical_block(), Some(104));
    }

    #[test]
    fn bmap_translates_aligned_offset() {
        let f = fork_with(&[(100, 4)], 4);
        let m = ExtentMapper::new(&f, 4096);

        assert_eq!(m.bmap(0).unwrap(), 100 * 4096);
        assert_eq!(m.bmap(4096).unwrap(), 101 * 4096);
        // 4KB offset aligned.
        assert_eq!(m.bmap(3 * 4096).unwrap(), 103 * 4096);
    }

    #[test]
    fn bmap_rejects_unaligned_offset() {
        let f = fork_with(&[(100, 4)], 4);
        let m = ExtentMapper::new(&f, 4096);
        assert!(m.bmap(1).is_err());
    }

    #[test]
    fn bmap_past_allocation_is_out_of_range() {
        let f = fork_with(&[(100, 4)], 4);
        let m = ExtentMapper::new(&f, 4096);
        assert!(m.bmap(4 * 4096).is_err());
    }
}
