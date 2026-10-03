//! Fork data: the on-disk description of a data fork, resource fork, or one of
//! the volume's special files.
//!
//! A "fork" is an independently sized byte stream with its own extent list. The
//! same on-disk structure describes all of them, and the volume header uses it
//! for the allocation bitmap, the catalog B-tree, the extents overflow B-tree,
//! the attributes B-tree and the startup file.
//!
//! Mining reference: Apple `core/hfs_format.h`, `struct HFSPlusForkData`:
//!
//! ```c
//! struct HFSPlusForkData {
//!     u_int64_t logicalSize;    /* fork's logical size in bytes */
//!     u_int32_t clumpSize;      /* fork's clump size in bytes */
//!     u_int32_t totalBlocks;    /* total blocks used by this fork */
//!     HFSPlusExtentRecord extents;  /* initial set of extents */
//! }
//! ```
//!
//! `totalBlocks` is the count of allocation blocks the fork occupies, and is
//! deliberately redundant with the sum of the initial extents plus any overflow
//! records. Apple treats a mismatch as a corruption signal in
//! `core/hfs_extents.c`; this crate surfaces both values so a caller can
//! diagnose the disagreement instead of silently preferring one.

use super::extents::{ExtentDescriptor, ExtentRecord, INLINE_EXTENT_COUNT};
use crate::endian::Cursor;
use crate::error::{Error, Result};

/// Byte size of an on-disk `HFSPlusForkData`.
pub const FORK_DATA_SIZE: usize = 16 + INLINE_EXTENT_COUNT * 8;

/// Which fork of a catalog record a piece of data belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForkType {
    /// The file's data fork (`kHFSPlusForkData`).
    Data,
    /// The file's resource fork.
    ///
    /// Mining reference: `core/hfs_format.h` names these `kHFSPlusForkData`
    /// and `kHFSPlusResourceFork` via `kHFSPlusForkData` being reused for the
    /// resource fork in `HFSPlusCatalogFile`; the distinction is carried by
    /// which field of the record the fork appears in, and by
    /// `core/FileExtentMapping.c`, which passes an explicit
    /// `forktype == HFSP_EXTENT_RSRC` flag.
    Resource,
}

impl ForkType {
    /// Mining reference: `HFSP_EXTENT_DATA` / `HFSP_EXTENT_RSRC` naming in
    /// `core/FileExtentMapping.c` and `core/hfs_extents.h`.
    pub const fn is_resource(self) -> bool {
        matches!(self, ForkType::Resource)
    }
}

/// On-disk fork description: size, clump size, block count and inline extents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForkData {
    /// Logical size of the fork in bytes: the length a reader sees.
    ///
    /// It may be *less* than the blocks behind it -- a file of 5000 bytes occupies
    /// two 4096-byte blocks -- but never more. An HFS+ data fork cannot be sparse,
    /// so there is no such thing as a hole to account for the difference.
    pub logical_size: u64,
    /// Clump size in bytes: the granularity at which the fork grows.
    pub clump_size: u32,
    /// Total allocation blocks claimed by this fork.
    pub total_blocks: u32,
    /// The inline portion of the extent chain.
    pub extents: ExtentRecord,
}

impl Default for ForkData {
    fn default() -> Self {
        ForkData {
            logical_size: 0,
            clump_size: 0,
            total_blocks: 0,
            extents: ExtentRecord::EMPTY,
        }
    }
}

impl ForkData {
    /// Byte size of the on-disk encoding.
    pub const SIZE: usize = FORK_DATA_SIZE;

    /// An empty fork.
    pub const EMPTY: ForkData = ForkData {
        logical_size: 0,
        clump_size: 0,
        total_blocks: 0,
        extents: ExtentRecord::EMPTY,
    };

    /// Decode fork data from the head of `cursor`.
    ///
    /// Mining reference: `core/hfs_endian.c` (`hfs_swap_HFSPlusForkData`)
    /// swaps `total_size` (64-bit), `clump_size` and `total_blocks` (32-bit
    /// each) and then the eight extent descriptors, which fixes both the field
    /// order and the field widths reproduced here.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self> {
        Ok(ForkData {
            logical_size: cursor.u64()?,
            clump_size: cursor.u32()?,
            total_blocks: cursor.u32()?,
            extents: ExtentRecord::read(cursor)?,
        })
    }

    /// Encode into `out`, which must be at least [`ForkData::SIZE`] bytes.
    pub fn write_to(&self, out: &mut [u8]) -> Result<()> {
        let available = out.len();
        let dst = out.get_mut(..FORK_DATA_SIZE).ok_or(Error::Truncated {
            what: "fork data write",
            needed: FORK_DATA_SIZE,
            available,
        })?;
        dst[0..8].copy_from_slice(&self.logical_size.to_be_bytes());
        dst[8..12].copy_from_slice(&self.clump_size.to_be_bytes());
        dst[12..16].copy_from_slice(&self.total_blocks.to_be_bytes());
        dst[16..FORK_DATA_SIZE].copy_from_slice(&self.extents.to_bytes());
        Ok(())
    }

    /// Decode fork data from a 80-byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut cur = Cursor::new(bytes, "fork data");
        Self::read(&mut cur)
    }

    /// Encode to an 80-byte array.
    pub fn to_bytes(&self) -> [u8; FORK_DATA_SIZE] {
        let mut out = [0u8; FORK_DATA_SIZE];
        // Writing into a correctly sized array cannot fail.
        let _ = self.write_to(&mut out);
        out
    }

    /// Number of blocks described by the inline extents alone.
    ///
    /// This is *not* the same as [`ForkData::total_blocks`] once overflow
    /// records exist. Mining reference: `core/hfs_extents.c`
    /// (`hfs_ext_iter_init`) sets the iterator's block limit from
    /// `fork->logical_size` while the fork's `totalBlocks` accounts for the
    /// whole chain including overflow.
    pub fn inline_blocks(&self) -> u64 {
        self.extents.total_blocks()
    }

    /// Whether the inline extents account for exactly `total_blocks`.
    ///
    /// A `false` result on a non-empty fork is expected and meaningful: it
    /// means the fork has overflow extent records. A `true` result on a
    /// non-empty fork is a corruption signal, because it would imply the fork
    /// has blocks that no extent describes.
    pub fn inline_matches_total(&self) -> bool {
        self.inline_blocks() == u64::from(self.total_blocks)
    }

    /// Bytes occupied by this fork if it were fully allocated.
    ///
    /// Takes `block_size` because HFS fork geometry is in allocation blocks.
    /// A logical size larger than this is how HFS represents a sparse file:
    /// the tail blocks are simply never allocated, and
    /// `core/hfs_extents.c` (`hfs_ext_iter_next_group`) stops once it has
    /// satisfied `logical_size`.
    pub fn allocated_bytes(&self, block_size: u32) -> Option<u64> {
        u64::from(self.total_blocks).checked_mul(u64::from(block_size))
    }

    /// Whether this fork's allocated blocks extend beyond the inline extent
    /// record, requiring the extents overflow B-tree.
    ///
    /// The trigger is the *allocated* block count, not the logical size. A
    /// sparse file may have a logical size far larger than its allocated
    /// blocks, and the unallocated region is simply a hole: HFS+ does not
    /// allocate zero blocks for it and does not store extents for it. So a fork
    /// whose `total_blocks` fits in the eight inline descriptors never consults
    /// the extents B-tree, however large its `logical_size` is.
    ///
    /// Mining reference: Apple `core/hfs_extents.c` (`push_ext`,
    /// `hfs_ext_realloc`) writes inline extents for the first
    /// `kHFSPlusExtentDensity` (8) descriptors and then loops
    /// `for (; ndx < count; ndx += 8)` inserting each further group into the
    /// extents B-tree via `BTInsertRecord`, advancing the overflow key's
    /// `startBlock` by `hfs_total_blocks(&extents[ndx], ...)`.
    ///
    /// That accumulation is the subtle part: the overflow key's `startBlock`
    /// is the number of *allocation blocks already described by preceding
    /// groups*, i.e. an offset within the fork's block space. It is not a
    /// physical block number and not a byte offset.
    pub fn needs_overflow(&self) -> bool {
        self.inline_blocks() < u64::from(self.total_blocks)
    }

    /// Number of allocation blocks still described only by overflow records.
    pub fn overflow_block_count(&self) -> u64 {
        u64::from(self.total_blocks).saturating_sub(self.inline_blocks())
    }


    /// Check this fork against the blocks its extents actually describe.
    ///
    /// `described_blocks` is the total from the inline extents *plus* any overflow
    /// records, and `physical_size` is `total_blocks * block_size` as the record
    /// implies it. Two independent statements, checked against the extents:
    ///
    /// ```text
    /// logical_size <= physical_size
    /// physical_size <= described_blocks * block_size
    /// ```
    ///
    /// The first is the non-sparse invariant. An HFS+ data fork has no
    /// representation for a hole: a zero-start extent descriptor is the
    /// *attributes* file's gap marker, and `hfs_vfsops.c` has no zero-fill path
    /// for a data fork. So a logical size beyond the physical size is not a sparse
    /// file, it is a corrupt record, and a reader that zero-fills it is inventing
    /// bytes rather than recovering them.
    ///
    /// The second says the record does not claim more blocks than its extents
    /// account for -- the direction that would make a reader read past the end of
    /// the file's own data.
    ///
    /// Mining reference: `lib_fsck_hfs/dfalib/CatalogCheck.c` `CheckFileData`,
    /// which reports `E_LEOF` ("Incorrect size for file") for the first and
    /// `E_PEOF` ("Incorrect block count for file") for the second. Note the
    /// tolerance: a logical size *below* the physical size is ordinary, because the
    /// final block of a file is normally only partly used.
    pub fn validate(&self, described_blocks: u64, block_size: u32) -> Result<()> {
        if self.total_blocks == 0 {
            // A fork with no blocks must claim no bytes.
            if self.logical_size != 0 {
                return Err(Error::invalid(
                    "ForkData.logicalSize",
                    format!(
                        "E_LEOF: {} bytes claimed by a fork with no blocks",
                        self.logical_size
                    ),
                ));
            }
            return Ok(());
        }
        if described_blocks > u64::from(u32::MAX) {
            return Err(Error::overflow("fork block count"));
        }
        let physical_size = u64::from(self.total_blocks)
            .checked_mul(u64::from(block_size))
            .ok_or(Error::overflow("fork physical size"))?;

        if self.logical_size > physical_size {
            return Err(Error::invalid(
                "ForkData.logicalSize",
                format!(
                    "E_LEOF: {} bytes of data in {} blocks of {block_size} -- an \
                     HFS+ data fork cannot be sparse, so the excess is unaccounted \
                     for",
                    self.logical_size, self.total_blocks
                ),
            ));
        }
        let capacity = described_blocks
            .checked_mul(u64::from(block_size))
            .ok_or(Error::overflow("fork capacity"))?;
        if physical_size > capacity {
            return Err(Error::invalid(
                "ForkData.totalBlocks",
                format!(
                    "E_PEOF: {} blocks of {block_size} exceed the {} blocks its \
                     extents describe",
                    self.total_blocks, described_blocks
                ),
            ));
        }
        Ok(())
    }

    /// Iterate the meaningful inline extents.
    pub fn iter_inline(&self) -> impl Iterator<Item = &ExtentDescriptor> {
        self.extents.iter()
    }
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible next
// to the value under test, so the struct-update lint is relaxed here.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn fork_data_is_eighty_bytes() {
        assert_eq!(FORK_DATA_SIZE, 80);
        assert_eq!(ForkData::SIZE, 80);
    }

    #[test]
    fn round_trips_through_bytes() {
        let mut fork = ForkData::default();
        fork.logical_size = 0x1234_5678_9abc;
        fork.clump_size = 65_536;
        fork.total_blocks = 4;
        fork.extents.raw[0] = ExtentDescriptor { start_block: 10, block_count: 4 };
        let bytes = fork.to_bytes();
        assert_eq!(ForkData::from_bytes(&bytes).unwrap(), fork);
    }

    #[test]
    fn truncated_fork_data_is_rejected() {
        assert!(ForkData::from_bytes(&[0u8; 79]).is_err());
        assert!(ForkData::from_bytes(&[0u8; 16]).is_err());
    }

    #[test]
    fn inline_total_comparison_detects_overflow() {
        let mut fork = ForkData::default();
        fork.logical_size = 0;
        fork.extents.raw[0] = ExtentDescriptor { start_block: 0, block_count: 2 };
        // Inline describes 2 blocks but the fork claims 6: three overflow records.
        fork.total_blocks = 6;
        assert!(!fork.inline_matches_total());
        assert_eq!(fork.inline_blocks(), 2);
    }

    #[test]
    fn sparse_fork_needs_no_overflow_despite_large_logical_size() {
        // A sparse file: 10 blocks of logical size, 1 block allocated. The nine
        // unallocated blocks are holes, not extents, so no overflow record is
        // involved. Mining reference: core/hfs_extents.c keys overflow on
        // allocated blocks only.
        let mut fork = ForkData::default();
        fork.logical_size = 10 * 4096;
        fork.extents.raw[0] = ExtentDescriptor { start_block: 100, block_count: 1 };
        fork.total_blocks = 1;
        assert!(!fork.needs_overflow());
        assert_eq!(fork.overflow_block_count(), 0);
        assert_eq!(fork.allocated_bytes(4096), Some(4096));
        assert!(fork.inline_matches_total());
    }

    #[test]
    fn fork_with_blocks_past_the_eighth_extent_needs_overflow() {
        let mut fork = ForkData::default();
        fork.logical_size = 0;
        for i in 0..INLINE_EXTENT_COUNT {
            fork.extents.raw[i] =
                ExtentDescriptor { start_block: i as u32 * 2, block_count: 2 };
        }
        // 8 inline extents * 2 blocks = 16 allocated; the fork claims 24.
        fork.total_blocks = 24;
        assert!(fork.needs_overflow());
        assert_eq!(fork.overflow_block_count(), 8);
    }

    #[test]
    fn eight_exact_extents_do_not_need_overflow() {
        let mut fork = ForkData::default();
        for i in 0..INLINE_EXTENT_COUNT {
            fork.extents.raw[i] =
                ExtentDescriptor { start_block: i as u32 * 2, block_count: 2 };
        }
        fork.total_blocks = 16;
        assert!(!fork.needs_overflow());
        assert!(fork.inline_matches_total());
    }


    #[test]
    fn fork_type_classification() {
        assert!(ForkType::Resource.is_resource());
        assert!(!ForkType::Data.is_resource());
    }
}
