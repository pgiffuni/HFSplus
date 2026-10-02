//! Extent descriptors and inline extent records.
//!
//! # Terminology warning
//!
//! "Extent" here means an HFS *allocation block run*: a contiguous range of
//! allocation blocks belonging to a fork. It has nothing to do with XFS-style
//! "extent trees" or with generic filesystem extent mapping terminology, and
//! it is unrelated to Linux's `EXT4_EXTENTS_MAX` and friends. Every extent in
//! HFS+ is a single `(startBlock, blockCount)` pair.
//!
//! Mining reference: Apple `core/hfs_format.h`,
//! `struct HFSPlusExtentDescriptor`, and the `HFSPlusExtentRecord` typedef that
//! fixes the inline capacity at eight descriptors. `core/hfs_extents.c`
//! (`hfs_ext_iter_next_group`) walks a chain of these descriptors, treating a
//! `blockCount` of zero as the terminator.

use crate::endian::Cursor;
use crate::error::{Error, Result};

/// Number of extent descriptors stored inline in an HFS+ catalog record and in
/// a volume header's fork data.
///
/// Mining reference: `HFSPlusExtentRecord` is
/// `HFSPlusExtentDescriptor[8]` in Apple `core/hfs_format.h`; the constant is
/// named `kHFSPlusExtentRecordMaximumExtentCount` in `core/hfs_catalog.h`.
pub const INLINE_EXTENT_COUNT: usize = 8;

/// Byte size of one on-disk extent descriptor.
pub const EXTENT_DESCRIPTOR_SIZE: usize = 8;

/// A contiguous run of allocation blocks owned by a fork.
///
/// Both fields are in allocation blocks, not bytes or 512-byte sectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ExtentDescriptor {
    /// First allocation block of the run.
    pub start_block: u32,
    /// Number of allocation blocks in the run.
    pub block_count: u32,
}

impl ExtentDescriptor {
    /// Size of the on-disk encoding.
    pub const SIZE: usize = EXTENT_DESCRIPTOR_SIZE;

    /// The all-zero descriptor, used as the chain terminator.
    pub const EMPTY: ExtentDescriptor =
        ExtentDescriptor { start_block: 0, block_count: 0 };

    /// Decode one descriptor from the head of `cursor`.
    ///
    /// Mining reference: `core/hfs_endian.c`'s `hfs_swap_HFSPlusForkData`
    /// swaps the extent array field by field, two 32-bit values per descriptor.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self> {
        Ok(ExtentDescriptor {
            start_block: cursor.u32()?,
            block_count: cursor.u32()?,
        })
    }

    /// Encode into `out`, which must be at least [`ExtentDescriptor::SIZE`]
    /// bytes.
    pub fn write_to(&self, out: &mut [u8]) -> Result<()> {
        let available = out.len();
        let dst = out.get_mut(..EXTENT_DESCRIPTOR_SIZE).ok_or(Error::Truncated {
            what: "extent write",
            needed: EXTENT_DESCRIPTOR_SIZE,
            available,
        })?;
        dst[0..4].copy_from_slice(&self.start_block.to_be_bytes());
        dst[4..8].copy_from_slice(&self.block_count.to_be_bytes());
        Ok(())
    }

    /// Last allocation block covered by this run, or `None` if empty.
    pub fn end_block(&self) -> Option<u64> {
        if self.block_count == 0 {
            None
        } else {
            Some(u64::from(self.start_block) + u64::from(self.block_count) - 1)
        }
    }

    /// Whether this descriptor terminates an extent chain.
    ///
    /// Mining reference: `core/hfs_extents.c` (`hfs_ext_iter_next_group`)
    /// stops at the first descriptor whose `blockCount` is zero, treating the
    /// `startBlock` of a terminator as meaningless.
    pub fn is_terminator(&self) -> bool {
        self.block_count == 0
    }
}

/// The fixed-capacity array of extent descriptors stored inline in a fork.
///
/// A zero-`block_count` slot ends the chain; the remaining slots are padding
/// and are not guaranteed to be zero on disk, so they must be ignored rather
/// than validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentRecord {
    /// The eight on-disk slots, terminator included.
    pub raw: [ExtentDescriptor; INLINE_EXTENT_COUNT],
}

/// Byte size of an on-disk inline extent record.
pub const EXTENT_RECORD_SIZE: usize = INLINE_EXTENT_COUNT * EXTENT_DESCRIPTOR_SIZE;

impl Default for ExtentRecord {
    fn default() -> Self {
        ExtentRecord { raw: [ExtentDescriptor::default(); INLINE_EXTENT_COUNT] }
    }
}

impl ExtentRecord {
    /// An all-zero (empty) record: every slot is a terminator.
    pub const EMPTY: ExtentRecord =
        ExtentRecord { raw: [ExtentDescriptor::EMPTY; INLINE_EXTENT_COUNT] };

    /// Decode the eight inline slots from the head of `cursor`.
    ///
    /// Trailing slots after the terminator are preserved verbatim rather than
    /// being normalised, so that a record read from a volume can later be
    /// written back without changing bytes the filesystem never interpreted.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self> {
        let mut raw = [ExtentDescriptor::default(); INLINE_EXTENT_COUNT];
        for slot in raw.iter_mut() {
            *slot = ExtentDescriptor::read(cursor)?;
        }
        Ok(ExtentRecord { raw })
    }

    /// Encode the eight slots into a 64-byte array.
    pub fn to_bytes(&self) -> [u8; EXTENT_RECORD_SIZE] {
        let mut out = [0u8; EXTENT_RECORD_SIZE];
        for (slot, desc) in self.raw.iter().enumerate() {
            let off = slot * EXTENT_DESCRIPTOR_SIZE;
            out[off..off + 4].copy_from_slice(&desc.start_block.to_be_bytes());
            out[off + 4..off + 8].copy_from_slice(&desc.block_count.to_be_bytes());
        }
        out
    }

    /// Decode a 64-byte inline extent record.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut cur = Cursor::new(bytes, "extent record");
        Self::read(&mut cur)
    }

    /// Iterate the meaningful descriptors, stopping at the first terminator.
    ///
    /// Mining reference: `core/hfs_extents.c` (`hfs_ext_iter_next_group`) uses
    /// exactly this termination rule for inline extents.
    pub fn iter(&self) -> impl Iterator<Item = &ExtentDescriptor> {
        self.raw.iter().take_while(|d| !d.is_terminator())
    }

    /// Total blocks described by the meaningful descriptors.
    pub fn total_blocks(&self) -> u64 {
        self.iter().map(|d| u64::from(d.block_count)).sum()
    }

    /// Number of meaningful descriptors before the terminator.
    pub fn used(&self) -> usize {
        self.iter().count()
    }
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible next
// to the value under test, so the struct-update lint is relaxed here.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_bytes() {
        let mut rec = ExtentRecord::EMPTY;
        rec.raw[0] = ExtentDescriptor { start_block: 42, block_count: 3 };
        rec.raw[1] = ExtentDescriptor { start_block: 100, block_count: 1 };
        let bytes = rec.to_bytes();
        assert_eq!(bytes.len(), 64);
        assert_eq!(ExtentRecord::from_bytes(&bytes).unwrap(), rec);
    }

    #[test]
    fn iteration_stops_at_terminator_and_ignores_garbage_after() {
        let mut rec = ExtentRecord::EMPTY;
        rec.raw[0] = ExtentDescriptor { start_block: 1, block_count: 5 };
        // Slot 1 terminates; slot 2 deliberately holds junk that must be ignored.
        rec.raw[2] = ExtentDescriptor { start_block: 999, block_count: 999 };
        assert_eq!(rec.used(), 1);
        assert_eq!(rec.total_blocks(), 5);
    }

    #[test]
    fn empty_record_has_no_extents() {
        assert_eq!(ExtentRecord::EMPTY.total_blocks(), 0);
        assert_eq!(ExtentRecord::EMPTY.used(), 0);
        assert!(ExtentRecord::EMPTY.raw.iter().all(|d| d.is_terminator()));
    }

    #[test]
    fn end_block_covers_the_run() {
        let d = ExtentDescriptor { start_block: 10, block_count: 5 };
        assert_eq!(d.end_block(), Some(14));
        assert_eq!(ExtentDescriptor::default().end_block(), None);
    }

    #[test]
    fn truncated_record_is_rejected_not_panicking() {
        assert!(ExtentRecord::from_bytes(&[0u8; 63]).is_err());
        assert!(ExtentRecord::from_bytes(&[0u8; 20]).is_err());
    }
}
