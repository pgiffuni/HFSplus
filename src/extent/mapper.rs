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
    overflow: Option<Box<dyn OverflowResolver + 'a>>,
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
    pub fn with_overflow(mut self, resolver: Box<dyn OverflowResolver + 'a>) -> Self {
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
}
