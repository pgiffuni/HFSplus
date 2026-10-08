// SPDX-License-Identifier: APSL-1.2

//! Reading bytes out of a fork.
//!
//! Mining reference: Apple `core/FileExtentMapping.c` (`MapFileBlockC`,
//! `MapFileBlockC_noPerm`) is the kernel path this mirrors, and
//! `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) is where the volume block size
//! that drives the mapping comes from.
//!
//! # Offsets past the allocated blocks
//!
//! A read that lands beyond a fork's extents returns zeros rather than failing.
//!
//! Worth being precise about why, because the obvious explanation is wrong. An
//! HFS+ data fork **cannot be sparse**: `fsck.hfsplus` rejects an extent with
//! `startBlock == 0` and a non-zero count ("Invalid extent entry"), and rejects
//! a `logicalSize` larger than the allocated blocks ("Incorrect size for file").
//! `core/FileExtentMapping.c` `MapFileBlockC` has no zero-fill path for either --
//! it returns whatever `SearchExtentFile` finds, and an error if it finds
//! nothing. A zero-start descriptor is the *attributes* file's gap marker, not a
//! data fork's.
//!
//! So zero-filling is a tolerance for images that do not satisfy those rules --
//! a hand-written image, a partially recovered one, or a corrupt `totalBlocks` --
//! and not a format feature. Mining reference: `core/hfs_extents.c`
//! (`hfs_ext_iter_init`) sets the iterator's limit from `logicalSize`, while
//! `hfs_ext_iter_next_group` stops once no group covers the requested block.
//!
//! Getting this wrong in either direction is a bug with no obvious symptom: too
//! eager and the reader fabricates data for a fork that should have failed, too
//! reluctant and a partially readable file reports end-of-file early.

use crate::blockdev::BlockDevice;
use crate::btree::io::BTreeFile;
use crate::error::{Error, Result};
use crate::extent::{ExtentMapper, OverflowResolver};
use crate::format::fork::ForkData;

/// Reads bytes from a fork, treating holes as zeros.
pub struct ForkReader<'a, D: ?Sized> {
    mapper: ExtentMapper<'a>,
    fork: ForkData,
    device: &'a D,
}

impl<'a, D: ?Sized> std::fmt::Debug for ForkReader<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkReader")
            .field("logical_size", &self.fork.logical_size)
            .field("total_blocks", &self.fork.total_blocks)
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> ForkReader<'a, D> {
    /// Build a reader for `fork` on `device`.
    ///
    /// `block_size` is the volume's allocation block size, which the fork's byte
    /// offsets are expressed in.
    pub fn new(device: &'a D, fork: &ForkData, block_size: u32) -> Self {
        ForkReader {
            mapper: ExtentMapper::new(fork, block_size),
            fork: *fork,
            device,
        }
    }

    /// Build a reader whose overflow extents are resolved through `resolver`.
    pub fn with_overflow(
        device: &'a D,
        fork: &ForkData,
        block_size: u32,
        resolver: Box<dyn OverflowResolver + Send + Sync + 'a>,
    ) -> Self {
        ForkReader {
            mapper: ExtentMapper::new(fork, block_size).with_overflow(resolver),
            fork: *fork,
            device,
        }
    }

    /// The fork's logical size in bytes.
    pub fn logical_size(&self) -> u64 {
        self.fork.logical_size
    }

    /// Whether the fork has no contents.
    pub fn is_empty(&self) -> bool {
        self.fork.logical_size == 0
    }

    /// Whether any allocated block lies beyond the inline extents.
    pub fn needs_overflow(&self) -> bool {
        self.fork.needs_overflow()
    }

    /// Read up to `len` bytes starting at `offset`, clamped to the fork's size.
    ///
    /// Returns fewer than `len` bytes at end of file, as POSIX `read` does. A
    /// range entirely within a hole returns zeros; the zero fill is real data as
    /// far as a reader can tell, and returning it keeps a fork whose `logicalSize`
    /// outruns its extents readable up to the size it claims.
    pub fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(Error::overflow("fork read range"))?;
        // A read starting past the end is empty rather than an error, as POSIX
        // read is. A range whose *end* exceeds the size is simply clamped below.
        if offset >= self.fork.logical_size {
            return Ok(Vec::new());
        }
        let _ = end;
        let avail = self.fork.logical_size.saturating_sub(offset) as usize;
        let want = len.min(avail);
        let mut out = vec![0u8; want];
        if want == 0 {
            return Ok(out);
        }

        let block_size = u64::from(self.mapper.block_size());
        let mut written = 0usize;

        while written < want {
            let at = offset + written as u64;
            let within = at % block_size;
            let chunk = (block_size - within) as usize;
            let chunk = chunk.min(want - written);

            // A block past the fork's allocated blocks is a hole.
            let physical = match self.mapper.map_byte_offset(at) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    // Leave the zeros already written.
                    written += chunk;
                    continue;
                }
                Err(e) => return Err(e),
            };

            let device_offset = u64::from(physical)
                .checked_mul(block_size)
                .and_then(|b| b.checked_add(within))
                .ok_or(Error::overflow("fork device offset"))?;
            self.device
                .read_at(device_offset, &mut out[written..written + chunk])?;
            written += chunk;
        }

        Ok(out)
    }

    /// Read the whole fork, for the sizes a metadata record can safely hold.
    ///
    /// Bounded by `limit` so that a corrupt logical size cannot make a caller
    /// try to allocate an unbounded buffer. Returns an error past the limit
    /// rather than silently truncating, because a silent truncation would look
    /// like a short file.
    pub fn read_all(&self, limit: usize) -> Result<Vec<u8>> {
        let size = self.fork.logical_size;
        if size > limit as u64 {
            return Err(Error::out_of_range("fork logical size", size, limit as u64));
        }
        self.read(0, size as usize)
    }

    /// SEEK_DATA: the byte offset of the next allocated region at or after `offset`.
    ///
    /// If `offset` already falls inside an extent, it is returned unchanged.
    /// If `offset` is in a hole, the start of the next extent is returned.
    /// Returns `None` when no extent covers or follows the offset.
    pub fn seek_data(&self, offset: u64) -> Result<Option<u64>> {
        if offset >= self.fork.logical_size {
            return Ok(None);
        }
        let block_size = u64::from(self.mapper.block_size());
        let fork_block = offset / block_size;

        for range in self.mapper.ranges() {
            let range = range?;
            let range_end = u64::from(range.fork_block + range.block_count);
            if fork_block < range_end {
                if fork_block >= u64::from(range.fork_block) {
                    // Offset is within this extent.
                    return Ok(Some(offset));
                }
                // Offset is in a hole before this extent.
                return Ok(Some(u64::from(range.fork_block) * block_size));
            }
        }
        Ok(None)
    }

    /// SEEK_HOLE: the byte offset of the next hole at or after `offset`.
    ///
    /// If `offset` already falls in a hole, it is returned unchanged.
    /// If `offset` is inside an extent, the end of that extent is returned
    /// (the start of the following hole, if any).
    /// Returns `None` when the entire fork is allocated with no hole past `offset`.
    pub fn seek_hole(&self, offset: u64) -> Result<Option<u64>> {
        if offset >= self.fork.logical_size {
            return Ok(None);
        }
        let block_size = u64::from(self.mapper.block_size());

        for range in self.mapper.ranges() {
            let range = range?;
            let range_start = u64::from(range.fork_block) * block_size;
            let range_end = u64::from(range.fork_block + range.block_count) * block_size;

            if offset < range_start {
                // In a hole before this extent.
                return Ok(Some(offset));
            }
            if offset < range_end {
                // Inside this extent; hole starts at the end of it.
                return Ok(Some(range_end));
            }
        }

        // Past all extents but within logical size: a tail hole.
        Ok(Some(offset))
    }

    /// BMAP: translate a fork-relative byte offset to a device byte offset.
    ///
    /// The offset must be block-aligned. A hole yields
    /// [`Error::OutOfRange`](crate::error::Error::OutOfRange), matching the
    /// semantics of `ExtentMapper::bmap`.
    pub fn bmap(&self, offset: u64) -> Result<u64> {
        self.mapper.bmap(offset)
    }
}

/// Resolves a fork's overflow extents through a volume's Extents B-tree.
///
/// Mining reference: `core/hfs_extents.c` keys those records on the file's CNID
/// and a cumulative block offset, which is why the resolver is given both.
pub struct TreeOverflow<'a, D: ?Sized> {
    extents_tree: BTreeFile<'a, D>,
}

impl<'a, D: BlockDevice + ?Sized> std::fmt::Debug for TreeOverflow<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeOverflow")
            .field("node_size", &self.extents_tree.node_size())
            .field("total_nodes", &self.extents_tree.header().total_nodes)
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> TreeOverflow<'a, D> {
    /// Build a resolver over the volume's Extents B-tree.
    pub fn new(extents_tree: BTreeFile<'a, D>) -> Self {
        TreeOverflow { extents_tree }
    }

    /// Find the group of up to eight extents that begins at `start_block`.
    ///
    /// The key is `(forkType, fileID = fork CNID, startBlock = cumulative
    /// allocated blocks already described)`.
    ///
    /// # Errors
    ///
    /// A `None` means "no further groups", which ends the walk. Any structural
    /// problem inside the Extents B-tree is propagated, because silently
    /// stopping would turn a corrupt volume into files that read as short.
    pub fn find_group(
        &self,
        fork_type: u8,
        file_id: u32,
        start_block: u32,
    ) -> Result<Option<crate::format::extents::ExtentRecord>> {
        use crate::btree::key::split_extent_record;

        let header = self.extents_tree.header();
        if header.leaf_records == 0 {
            return Ok(None);
        }
        let mut node_num = header.first_leaf_node;
        let mut budget = header.total_nodes;
        while budget > 0 {
            budget -= 1;
            let bytes = self.extents_tree.read_node_bytes(node_num)?;
            let node = self.extents_tree.parse_node(&bytes)?;

            for i in 0..node.num_records() {
                let Ok(record) = node.record(i) else { continue };
                let Some((key, body)) = split_extent_record(record) else {
                    continue;
                };
                if key.file_id != file_id {
                    // Keys sort by CNID first, so once past it we are done.
                    if key.file_id > file_id {
                        return Ok(None);
                    }
                    continue;
                }
                // Both forks of a file share a CNID and sort adjacently, so
                // matching on the CNID alone would let a data fork adopt its
                // resource fork's extents.
                if key.fork_type != fork_type {
                    continue;
                }
                if key.start_block == start_block {
                    if body.len() < crate::format::extents::EXTENT_RECORD_SIZE {
                        return Err(Error::Truncated {
                            what: "extent record",
                            needed: crate::format::extents::EXTENT_RECORD_SIZE,
                            available: body.len(),
                        });
                    }
                    return Ok(Some(crate::format::extents::ExtentRecord::from_bytes(
                        &body[..crate::format::extents::EXTENT_RECORD_SIZE],
                    )?));
                }
            }

            if node_num == header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
        }
        Ok(None)
    }
}

/// A `TreeOverflow` paired with the CNID of the fork being read.
pub struct ForkOverflow<'a, 'r, D: ?Sized> {
    tree: &'r TreeOverflow<'a, D>,
    fork_type: u8,
    file_id: u32,
}

impl<'a, 'r, D: BlockDevice + ?Sized> std::fmt::Debug for ForkOverflow<'a, 'r, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkOverflow")
            .field("fork_type", &self.fork_type)
            .field("file_id", &self.file_id)
            .finish()
    }
}

impl<'a, 'r, D: BlockDevice + ?Sized> ForkOverflow<'a, 'r, D> {
    /// Bind a resolver to one fork.
    pub fn for_fork(tree: &'r TreeOverflow<'a, D>, fork_type: u8, file_id: u32) -> Self {
        ForkOverflow {
            tree,
            fork_type,
            file_id,
        }
    }
}

impl<'a, 'r, D: BlockDevice + ?Sized> OverflowResolver for ForkOverflow<'a, 'r, D> {
    fn resolve_group(
        &self,
        start_block: u32,
    ) -> Result<Option<crate::format::extents::ExtentRecord>> {
        self.tree
            .find_group(self.fork_type, self.file_id, start_block)
    }
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::blockdev::MemoryDevice;
    use crate::format::extents::ExtentDescriptor;

    /// A device whose blocks are filled with their block number, so a hole and a
    /// real block are trivially distinguishable.
    fn patterned_device(blocks: u64, block_size: u32) -> MemoryDevice {
        let mut dev = MemoryDevice::zeroed((blocks * u64::from(block_size)) as usize);
        for b in 0..blocks {
            let off = (b * u64::from(block_size)) as usize;
            dev.as_mut_slice()[off..off + 4].copy_from_slice(&(b as u32).to_be_bytes());
        }
        dev
    }

    fn fork(extents: &[(u32, u32)], total_blocks: u32, logical_size: u64) -> ForkData {
        let mut f = ForkData::default();
        f.clump_size = 4096;
        f.logical_size = logical_size;
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
    fn reads_a_contiguous_fork() {
        let dev = patterned_device(32, 4096);
        // Blocks 4 and 5 hold the data.
        let f = fork(&[(4, 2)], 2, 8192);
        let r = ForkReader::new(&dev, &f, 4096);
        assert_eq!(r.logical_size(), 8192);

        let first = r.read(0, 8).unwrap();
        assert_eq!(first.len(), 8);
        assert_eq!(&first[..4], &4u32.to_be_bytes());

        // Reading across the block boundary must be seamless. The device marks
        // each block by writing its own number in the *first four bytes*, so a
        // straddling read has to land the marker from the second block at the
        // right offset within the buffer.
        let across = r.read(4094, 8).unwrap();
        assert_eq!(across.len(), 8);
        assert_eq!(&across[..2], &[0, 0], "tail of block 4");
        assert_eq!(&across[2..6], &5u32.to_be_bytes(), "head of block 5");
    }

    #[test]
    fn reading_past_the_end_returns_nothing() {
        let dev = patterned_device(32, 4096);
        let f = fork(&[(4, 2)], 2, 8192);
        let r = ForkReader::new(&dev, &f, 4096);
        assert!(r.read(8192, 10).unwrap().is_empty());
        assert!(r.read(100_000, 10).unwrap().is_empty());
        // A read straddling the end is clamped, as POSIX read does.
        assert_eq!(r.read(8190, 100).unwrap().len(), 2);
    }

    #[test]
    fn a_sparse_tail_reads_as_zeros() {
        // One block allocated, but a logical size of four blocks.
        let dev = patterned_device(32, 4096);
        let f = fork(&[(4, 1)], 1, 4 * 4096);
        let r = ForkReader::new(&dev, &f, 4096);
        assert!(
            !r.needs_overflow(),
            "a sparse file needs no overflow records"
        );

        // The allocated part is real.
        assert_eq!(&r.read(0, 4).unwrap(), &4u32.to_be_bytes());
        // The hole is zero.
        let hole = r.read(4096, 16).unwrap();
        assert_eq!(hole, vec![0u8; 16]);
        // And so is the far end.
        let tail = r.read(3 * 4096, 4096).unwrap();
        assert_eq!(tail, vec![0u8; 4096]);
    }

    #[test]
    fn a_file_with_no_extents_at_all_is_all_zeros() {
        let dev = patterned_device(32, 4096);
        let f = ForkData::default();
        let r = ForkReader::new(&dev, &f, 4096);
        assert!(r.is_empty());
        assert!(r.read(0, 10).unwrap().is_empty());

        // A logical size with no blocks behind it is still a readable hole.
        let mut sparse = ForkData::default();
        sparse.logical_size = 4096;
        let r = ForkReader::new(&dev, &sparse, 4096);
        assert_eq!(r.read(0, 4096).unwrap(), vec![0u8; 4096]);
    }

    #[test]
    fn reads_across_several_extent_groups() {
        let dev = patterned_device(64, 4096);
        // Two groups: blocks 4-5 and 20-21.
        let f = fork(&[(4, 2), (20, 2)], 4, 4 * 4096);
        let r = ForkReader::new(&dev, &f, 4096);
        let all = r.read_all(1 << 20).unwrap();
        assert_eq!(all.len(), 4 * 4096);
        assert_eq!(&all[0..4], &4u32.to_be_bytes());
        assert_eq!(&all[4096..4100], &5u32.to_be_bytes());
        assert_eq!(&all[8192..8196], &20u32.to_be_bytes());
        assert_eq!(&all[12288..12292], &21u32.to_be_bytes());
    }

    #[test]
    fn read_all_refuses_a_logical_size_past_the_limit() {
        let dev = patterned_device(64, 4096);
        let mut f = fork(&[(4, 2)], 2, 0);
        f.logical_size = 1 << 30;
        let r = ForkReader::new(&dev, &f, 4096);
        assert!(matches!(r.read_all(1 << 20), Err(Error::OutOfRange { .. })));
    }

    #[test]
    fn a_zero_length_read_is_a_no_op() {
        let dev = patterned_device(32, 4096);
        let f = fork(&[(4, 2)], 2, 8192);
        let r = ForkReader::new(&dev, &f, 4096);
        assert!(r.read(0, 0).unwrap().is_empty());
        assert!(r.read(8192, 0).unwrap().is_empty());
    }

    #[test]
    fn a_read_range_that_would_wrap_is_an_error() {
        // offset + len must not wrap: silently wrapping would turn a huge range
        // into a small one and return data from the wrong place.
        let dev = patterned_device(32, 4096);
        let f = fork(&[(4, 2)], 2, 8192);
        let r = ForkReader::new(&dev, &f, 4096);
        assert!(matches!(r.read(u64::MAX, 16), Err(Error::Overflow { .. })));
        assert!(matches!(
            r.read(u64::MAX - 1, 16),
            Err(Error::Overflow { .. })
        ));
        // One below the wrap point is merely past the end.
        assert!(r.read(u64::MAX - 8191, 4096).unwrap().is_empty());
    }

    #[test]
    fn seek_data_returns_offset_when_already_in_data() {
        let dev = patterned_device(32, 4096);
        let f = fork(&[(4, 2)], 2, 8192);
        let r = ForkReader::new(&dev, &f, 4096);

        // Offset 0 is at the start of the first extent.
        assert_eq!(r.seek_data(0).unwrap(), Some(0));
        // Offset 100 is within the first extent.
        assert_eq!(r.seek_data(100).unwrap(), Some(100));
        // Offset 4096 is at the start of the second extent.
        assert_eq!(r.seek_data(4096).unwrap(), Some(4096));
    }

    #[test]
    fn seek_data_skips_to_next_extent_from_tail_hole() {
        let dev = patterned_device(32, 4096);
        // Only block 0 is allocated, but logical size implies 4 blocks.
        // Fork blocks 1-3 are holes.
        let f = fork(&[(4, 1)], 1, 4 * 4096);
        let r = ForkReader::new(&dev, &f, 4096);

        // Offset 0 is in the extent.
        assert_eq!(r.seek_data(0).unwrap(), Some(0));
        // Offset in the hole: no more extents, so None.
        assert_eq!(r.seek_data(4096).unwrap(), None);
    }

    #[test]
    fn seek_hole_returns_offset_when_already_in_hole() {
        let dev = patterned_device(32, 4096);
        // Block 0: allocated. Logical size is 4 blocks, so blocks 1-3 are holes.
        let f = fork(&[(4, 1)], 1, 4 * 4096);
        let r = ForkReader::new(&dev, &f, 4096);

        // Offset in the hole returns itself.
        assert_eq!(r.seek_hole(4096).unwrap(), Some(4096));
        // Offset past the end returns None.
        assert_eq!(r.seek_hole(4 * 4096).unwrap(), None);
    }

    #[test]
    fn seek_hole_finds_end_of_extent_from_within() {
        let dev = patterned_device(32, 4096);
        // Block 0: allocated at phys 4 (2 blocks). Logical size 4 blocks.
        let f = fork(&[(4, 2)], 2, 4 * 4096);
        let r = ForkReader::new(&dev, &f, 4096);

        // Inside the extent: hole starts after it (block 2).
        assert_eq!(r.seek_hole(0).unwrap(), Some(2 * 4096));
        assert_eq!(r.seek_hole(4096).unwrap(), Some(2 * 4096));
        // Block 2 is the start of the hole.
        assert_eq!(r.seek_hole(2 * 4096).unwrap(), Some(2 * 4096));
    }

    #[test]
    fn seek_data_and_seek_hole_work_with_overflow() {
        let dev = patterned_device(64, 4096);

        // A resolver that serves one fixed overflow group.
        struct Fixed(crate::format::extents::ExtentRecord);
        impl crate::extent::OverflowResolver for Fixed {
            fn resolve_group(
                &self,
                start_block: u32,
            ) -> Result<Option<crate::format::extents::ExtentRecord>> {
                if start_block == 16 {
                    Ok(Some(self.0))
                } else {
                    Ok(None)
                }
            }
        }

        let mut group = crate::format::extents::ExtentRecord::EMPTY;
        group.raw[0] = ExtentDescriptor {
            start_block: 300,
            block_count: 8,
        };
        let f = fork(&[(100, 8), (200, 8)], 24, 24 * 4096);
        let r = ForkReader::with_overflow(&dev, &f, 4096, Box::new(Fixed(group)));

        // Block 16 starts the overflow extent at physical 300.
        assert_eq!(r.seek_data(16 * 4096).unwrap(), Some(16 * 4096));
        // Block 20 is inside the overflow extent.
        assert_eq!(r.seek_data(20 * 4096).unwrap(), Some(20 * 4096));
        // Block 24 is past all data.
        assert_eq!(r.seek_data(24 * 4096).unwrap(), None);

        // Seeking for a hole: inside the overflow extent, hole starts after it.
        assert_eq!(r.seek_hole(16 * 4096).unwrap(), Some(24 * 4096));
    }
}
