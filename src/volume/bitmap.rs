//! The volume allocation bitmap.
//!
//! Mining reference: Apple `core/VolumeAllocation.c` implements
//! `ReadBitmapBlock`, `ReadBitmapRange`, `hfs_isallocated` and
//! `hfs_count_allocated` over exactly this structure, and `core/hfs_vfsutils.c`
//! validates its size against the volume header before trusting it.
//!
//! # Bit order is the opposite of the obvious one
//!
//! Allocation block `N` is bit `N` counting **from the most significant bit of
//! each byte**. So block 0 is `0x80`, block 7 is `0x01`, block 8 is the `0x80` of
//! the second byte. This is a constant source of off-by-`n`*8 errors and it is
//! easy to check: a freshly formatted volume must mark the volume header block
//! and the bitmap's own blocks used, and everything after them free.
//!
//! # A bit set means "allocated"
//!
//! There is no sense inversion: 1 means the block is in use.

use crate::blockdev::BlockDevice;
use crate::error::{Error, Result};
use crate::file::ForkReader;
use crate::format::fork::ForkData;

/// A read-only view of the volume's allocation bitmap.
pub struct AllocationBitmap<'a, D: ?Sized> {
    reader: ForkReader<'a, D>,
    /// Total allocation blocks the volume claims.
    total_blocks: u32,
    /// Bytes needed to represent `total_blocks` bits.
    bitmap_bytes: usize,
}

impl<'a, D: ?Sized> std::fmt::Debug for AllocationBitmap<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllocationBitmap")
            .field("total_blocks", &self.total_blocks)
            .field("bitmap_bytes", &self.bitmap_bytes)
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> AllocationBitmap<'a, D> {
    /// Open the bitmap for a volume of `total_blocks` blocks.
    ///
    /// # Errors
    ///
    /// Rejects a bitmap too small to describe the volume. Mining reference:
    /// `core/hfs_vfsutils.c` derives the expected size from `totalBlocks` and
    /// checks the allocation file's `logicalSize` against it before proceeding,
    /// because a short bitmap would silently report unallocated blocks as free.
    pub fn open(
        device: &'a D,
        fork: &ForkData,
        block_size: u32,
        total_blocks: u32,
    ) -> Result<Self> {
        let needed = bytes_for_blocks(total_blocks)?;
        if fork.logical_size < needed as u64 {
            return Err(Error::invalid(
                "allocationFile.logicalSize",
                format!(
                    "{} is too small to describe {total_blocks} blocks; need {needed}",
                    fork.logical_size
                ),
            ));
        }
        Ok(AllocationBitmap {
            reader: ForkReader::new(device, fork, block_size),
            total_blocks,
            bitmap_bytes: needed,
        })
    }

    /// Total allocation blocks the volume claims.
    pub fn total_blocks(&self) -> u32 {
        self.total_blocks
    }

    /// Bytes the bitmap occupies.
    pub fn bitmap_bytes(&self) -> usize {
        self.bitmap_bytes
    }

    /// Whether allocation block `block` is in use.
    ///
    /// Mining reference: `core/VolumeAllocation.c` `hfs_isallocated` returns
    /// `false` for a block number outside the volume rather than erroring, because
    /// the kernel calls it on allocation results it must then reject itself.
    pub fn is_allocated(&self, block: u32) -> Result<bool> {
        if block >= self.total_blocks {
            return Ok(false);
        }
        let (byte_index, bit_index) = bit_position(block);
        // One byte at a time rather than the whole bitmap: a caller asking about
        // scattered single bits should not pay for the entire file.
        let byte = self.reader.read(byte_index as u64, 1)?;
        let byte = byte.first().copied().ok_or(Error::Truncated {
            what: "allocation bitmap",
            needed: byte_index + 1,
            available: 0,
        })?;
        Ok(byte & (0x80 >> bit_index) != 0)
    }

    /// Whether allocation block `block` is free.
    pub fn is_free(&self, block: u32) -> Result<bool> {
        Ok(!self.is_allocated(block)?)
    }

    /// Count allocated blocks in `range`, which must be within the volume.
    ///
    /// Mining reference: `core/VolumeAllocation.c` `hfs_count_allocated` counts
    /// across the whole bitmap; this variant is bounded so that a caller checking
    /// one region cannot be surprised by a whole-volume scan.
    pub fn count_allocated(&self, start: u32, end: u32) -> Result<u64> {
        let end = end.min(self.total_blocks);
        if start >= end {
            return Ok(0);
        }
        let mut count = 0u64;
        let mut block = start;
        while block < end {
            let byte_index = block / 8;
            let bit_in_byte = block % 8;
            let byte = self.reader.read(byte_index as u64, 1)?;
            let b = byte.first().copied().unwrap_or(0);
            // Count the bits of this byte from `bit_in_byte` upward.
            for i in bit_in_byte..8 {
                let block = block + i - bit_in_byte;
                if block >= end {
                    break;
                }
                if b & (0x80 >> i) != 0 {
                    count += 1;
                }
            }
            block = (block / 8 + 1) * 8;
        }
        Ok(count)
    }

    /// Count every allocated block on the volume.
    pub fn count_all_allocated(&self) -> Result<u64> {
        self.count_allocated(0, self.total_blocks)
    }
}

/// Bytes needed to hold one bit per allocation block.
pub fn bytes_for_blocks(total_blocks: u32) -> Result<usize> {
    usize::try_from(total_blocks)
        .ok()
        .and_then(|n| n.checked_add(7))
        .map(|n| n / 8)
        .ok_or(Error::overflow("allocation bitmap size"))
}

/// Byte index and in-byte bit position for a block number.
///
/// Bits run from the **most significant** bit of each byte, so block 0 is bit 7.
#[inline]
fn bit_position(block: u32) -> (usize, u32) {
    ((block / 8) as usize, block % 8)
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::blockdev::MemoryDevice;
    use crate::format::extents::ExtentDescriptor;

    /// A device whose first bytes are a supplied bitmap.
    fn device_with_bitmap(bytes: &[u8]) -> MemoryDevice {
        let mut dev = MemoryDevice::zeroed(8192);
        dev.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        dev
    }

    fn fork_for(bytes: &[u8]) -> ForkData {
        let mut f = ForkData::default();
        f.logical_size = 8192;
        f.total_blocks = 1;
        f.clump_size = 4096;
        f.extents.raw[0] = ExtentDescriptor {
            start_block: 0,
            block_count: 1,
        };
        let _ = bytes;
        f
    }

    #[test]
    fn bit_order_runs_from_the_most_significant_bit() {
        // Within one byte, bit 7 of the mask is block 0 and bit 0 is block 7; the
        // next byte starts again at block 8. Reading this backwards shifts every
        // block by an unknown multiple of eight, so each boundary is asserted
        // separately rather than swept.
        let single = |bytes: &[u8], block: u32| {
            let dev = device_with_bitmap(bytes);
            let bm = AllocationBitmap::open(&dev, &fork_for(&[]), 4096, 32).unwrap();
            bm.is_allocated(block).unwrap()
        };

        assert!(single(&[0x80], 0), "0x80 in byte 0 is block 0");
        assert!(!single(&[0x80], 7), "0x01 in byte 0 would be block 7");
        assert!(single(&[0x01], 7), "0x01 in byte 0 is block 7");
        assert!(!single(&[0x01], 0));

        assert!(single(&[0x00, 0x80], 8), "0x80 in byte 1 is block 8");
        assert!(single(&[0x00, 0x01], 15), "0x01 in byte 1 is block 15");
        assert!(!single(&[0x00, 0x80], 0), "byte 1 must not set block 0");
        assert!(!single(&[0x80, 0x00], 8), "byte 0 must not set block 8");
    }

    #[test]
    fn every_bit_of_a_byte_maps_to_its_block() {
        for bit in 0..8u32 {
            let byte = 0x80u8 >> bit;
            let dev = device_with_bitmap(&[byte]);
            let bm = AllocationBitmap::open(&dev, &fork_for(&[]), 4096, 16).unwrap();
            for block in 0..8u32 {
                assert_eq!(
                    bm.is_allocated(block).unwrap(),
                    block == bit,
                    "block {block} with byte {byte:#04x}"
                );
            }
        }
    }

    #[test]
    fn blocks_beyond_the_volume_are_reported_free_not_allocated() {
        let dev = device_with_bitmap(&[0xFF; 8]);
        let bm = AllocationBitmap::open(&dev, &fork_for(&[]), 4096, 16).unwrap();
        assert!(bm.is_allocated(15).unwrap());
        assert!(!bm.is_allocated(16).unwrap(), "beyond the volume");
        assert!(!bm.is_allocated(u32::MAX).unwrap());
    }

    #[test]
    fn counting_matches_single_bit_queries() {
        // A repeating pattern so the count is not trivially zero or full.
        let bytes = [0b1010_1010u8, 0b0101_0101, 0b1111_0000, 0b0000_1111];
        let dev = device_with_bitmap(&bytes);
        let bm = AllocationBitmap::open(&dev, &fork_for(&[]), 4096, 32).unwrap();

        let counted = bm.count_all_allocated().unwrap();
        let mut queried = 0u64;
        for block in 0..32u32 {
            if bm.is_allocated(block).unwrap() {
                queried += 1;
            }
        }
        assert_eq!(counted, queried);
        assert_eq!(counted, 16);
    }

    #[test]
    fn counting_a_sub_range_is_exact() {
        let bytes = [0xFFu8; 4];
        let dev = device_with_bitmap(&bytes);
        let bm = AllocationBitmap::open(&dev, &fork_for(&[]), 4096, 32).unwrap();
        assert_eq!(bm.count_allocated(4, 12).unwrap(), 8);
        assert_eq!(bm.count_allocated(0, 8).unwrap(), 8);
        assert_eq!(bm.count_allocated(10, 5).unwrap(), 0, "empty range");
        // A range past the end is clamped.
        assert_eq!(bm.count_allocated(24, 1000).unwrap(), 8);
    }

    #[test]
    fn a_bitmap_too_small_for_the_volume_is_refused() {
        // A short bitmap would report unallocated blocks as free, which is worse
        // than refusing: it looks like free space.
        let dev = device_with_bitmap(&[0xFF; 8]);
        let mut fork = fork_for(&[]);
        fork.logical_size = 8;
        assert!(AllocationBitmap::open(&dev, &fork, 4096, 8192).is_err());
        // One byte per 8 blocks is exactly enough.
        let mut ok = fork;
        ok.logical_size = 1024;
        assert!(AllocationBitmap::open(&dev, &ok, 4096, 8192).is_ok());
    }

    #[test]
    fn size_arithmetic_cannot_overflow() {
        assert_eq!(bytes_for_blocks(1).unwrap(), 1);
        assert_eq!(bytes_for_blocks(8).unwrap(), 1);
        assert_eq!(bytes_for_blocks(9).unwrap(), 2);
        assert_eq!(bytes_for_blocks(u32::MAX).unwrap(), 512 * 1024 * 1024);
    }
}
