//! Read-only journal replay.
//!
//! # What replay is for
//!
//! On a journaled volume, metadata updates are written to a journal before they
//! are written to the filesystem. If the machine crashes, the filesystem may hold
//! older metadata than the journal does, and the journal is what a recovering
//! mount replays to bring the filesystem up to date.
//!
//! Mining reference: Apple `core/hfs_journal.c` `journal_replay` walks the
//! transaction list and calls `update_fs_block` for each recorded block, and
//! `core/hfs_vfsutils.c` decides at mount time whether replay is needed.
//!
//! # Replay must not write
//!
//! A read-only mount can only produce a *view*: the replayed blocks live in
//! memory and reads consult them in preference to the underlying device. The
//! image is never opened for writing, so replay cannot damage it even if it is
//! wrong. That is the property this module is built around, and
//! [`Journal::into_device`] is the only way to get at the overlaid device.
//!
//! # What the corpus can and cannot prove
//!
//! Every image `mkfs_hfsplus -J` produces has `kJIJournalNeedInitMask` set and a
//! zeroed journal header, because no transaction has ever been written. So the
//! corpus proves detection, validation and the *empty* replay path, and cannot
//! prove transaction replay. The transaction walk is therefore covered by
//! synthetic journals built in these tests, and the gap is recorded rather than
//! papered over.

use super::checksum::{calc_checksum, BLHDR_CHECKSUM_SIZE};
use super::info::{JournalHeader, JournalInfoBlock, END_BLK_NUM};
use crate::blockdev::BlockDevice;
use crate::error::{Error, Result};

/// On-disk size of one `block_list_header` prefix, before its `binfo` array.
///
/// Mining reference: `core/hfs_journal.h`:
///
/// ```c
/// typedef struct block_list_header {
///     u_int16_t max_blocks;
///     u_int16_t num_blocks;
///     int32_t   bytes_used;
///     uint32_t  checksum;
///     int32_t   flags;
///     block_info binfo[];
/// } block_list_header;
/// ```
pub const BLHDR_PREFIX_SIZE: usize = 16;

/// On-disk size of one `block_info` entry.
///
/// Mining reference: `core/hfs_journal.h` declares `block_info` as
/// `off_t bnum` followed by a union, and only `bsize` and `b.cksum` from that
/// union are stored; the `struct buf *` arm is in-memory only. With a 64-bit
/// `off_t` that is 8 + 4 + 4.
pub const BLOCK_INFO_SIZE: usize = 16;

/// `BLHDR_FIRST_HEADER`: this block list begins a transaction.
pub const BLHDR_FIRST_HEADER: u32 = 0x0000_0002;

/// `BLHDR_CHECK_CHECKSUMS`: the recorded blocks carry their own checksums.
pub const BLHDR_CHECK_CHECKSUMS: u32 = 0x0000_0001;

/// One filesystem block as recorded in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedBlock {
    /// Block number on the filesystem device.
    pub bnum: u64,
    /// Size of the block in bytes.
    pub bsize: u32,
    /// Checksum of the block's contents.
    pub cksum: u32,
}

/// A block list header and the blocks it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockListHeader {
    /// Capacity of the `binfo` array in this header.
    pub max_blocks: u16,
    /// How many `binfo` entries are valid.
    pub num_blocks: u16,
    /// Bytes of the transaction buffer this header accounts for.
    pub bytes_used: u32,
    /// Checksum over the first [`BLHDR_CHECKSUM_SIZE`] bytes.
    pub checksum: u32,
    /// Flags; see [`BLHDR_FIRST_HEADER`] and [`BLHDR_CHECK_CHECKSUMS`].
    pub flags: u32,
    /// Byte offset within the journal where this list's block data begins.
    ///
    /// Recorded while walking rather than recomputed later: the data follows the
    /// block list, and deriving it again from the header sizes is where an
    /// off-by-a-header-size bug hides.
    pub data_offset: u64,
    /// The blocks described.
    pub blocks: Vec<RecordedBlock>,
}

impl BlockListHeader {
    /// Whether this header begins a transaction.
    pub fn is_first(&self) -> bool {
        self.flags & BLHDR_FIRST_HEADER != 0
    }

    /// Whether the recorded blocks carry their own checksums.
    pub fn checks_blocks(&self) -> bool {
        self.flags & BLHDR_CHECK_CHECKSUMS != 0
    }

    /// Parse a block list header from `bytes`.
    ///
    /// `num_blocks` and `max_blocks` are attacker-controlled, so the array size
    /// is bounds-checked against the buffer before any of it is read.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_at(bytes, 0)
    }

    /// Parse a block list header, taking its data offset from `data_offset`.
    pub fn parse_at(bytes: &[u8], data_offset: u64) -> Result<Self> {
        if bytes.len() < BLHDR_PREFIX_SIZE {
            return Err(Error::Truncated {
                what: "block list header",
                needed: BLHDR_PREFIX_SIZE,
                available: bytes.len(),
            });
        }
        let be = crate::endian::Be::new(bytes);
        let max_blocks = be.u16(0)?;
        let num_blocks = be.u16(2)?;
        let bytes_used = be.u32(4)?;
        let checksum = be.u32(8)?;
        let flags = be.u32(12)?;

        // The array must fit, and it cannot be smaller than its own count.
        let need = usize::from(num_blocks) * BLOCK_INFO_SIZE;
        if need > bytes.len() - BLHDR_PREFIX_SIZE {
            return Err(Error::Truncated {
                what: "block list entries",
                needed: need,
                available: bytes.len() - BLHDR_PREFIX_SIZE,
            });
        }
        if usize::from(num_blocks) > usize::from(max_blocks) {
            return Err(Error::invalid(
                "block_list_header",
                format!("num_blocks {num_blocks} exceeds max_blocks {max_blocks}"),
            ));
        }

        let mut blocks = Vec::with_capacity(usize::from(num_blocks));
        for i in 0..usize::from(num_blocks) {
            let off = BLHDR_PREFIX_SIZE + i * BLOCK_INFO_SIZE;
            let bnum = be.u64(off)?;
            let bsize = be.u32(off + 8)?;
            let cksum = be.u32(off + 12)?;
            blocks.push(RecordedBlock { bnum, bsize, cksum });
        }

        Ok(BlockListHeader {
            max_blocks,
            num_blocks,
            bytes_used,
            checksum,
            flags,
            data_offset,
            blocks,
        })
    }

    /// Verify the header's own checksum.
    ///
    /// Mining reference: `core/hfs_journal.c` computes it over
    /// `BLHDR_CHECKSUM_SIZE` bytes, which covers the header fields and the first
    /// `binfo` entry.
    pub fn checksum_matches(&self, raw: &[u8]) -> bool {
        // Same zeroing convention as the journal header: the checksum field lies
        // inside the 32-byte range, so it is zeroed before hashing.
        match crate::journal::checksum::checksum_with_zeroed_field(
            raw,
            8,
            BLHDR_CHECKSUM_SIZE,
        ) {
            Some(c) => c == self.checksum,
            None => false,
        }
    }
}

/// One transaction: a run of block lists followed by their data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// Sequence number assigned when the transaction began.
    pub sequence_num: u32,
    /// Byte offset within the journal where the transaction starts.
    pub offset: u64,
    /// Byte offset within the journal where it ends.
    pub end: u64,
    /// The block lists it contains.
    pub block_lists: Vec<BlockListHeader>,
}

impl Transaction {
    /// Every block the transaction rewrites, in journal order.
    pub fn blocks(&self) -> impl Iterator<Item = &RecordedBlock> {
        self.block_lists.iter().flat_map(|b| b.blocks.iter())
    }
}

/// A filesystem block replaced by the journal.
///
/// The byte offset is stored alongside the block number because a read has to be
/// resolved by byte range, and recomputing it would mean carrying the volume's
/// block size into every read path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayedBlock {
    /// Block number on the filesystem device, as the journal recorded it.
    pub device_block: u64,
    /// Byte offset of this block on the device.
    pub device_offset: u64,
    /// The replacement contents.
    pub data: Vec<u8>,
}

impl ReplayedBlock {
    /// Whether this block covers the byte offset `at`.
    #[inline]
    pub fn covers(&self, at: u64) -> bool {
        match at.checked_sub(self.device_offset) {
            Some(within) => (within as usize) < self.data.len(),
            None => false,
        }
    }

    /// The bytes of this block from `at`, as far as `want` extends.
    #[inline]
    pub fn slice_from(&self, at: u64, want: usize) -> Option<&[u8]> {
        let within = at.checked_sub(self.device_offset)? as usize;
        if within >= self.data.len() {
            return None;
        }
        let end = (within + want).min(self.data.len());
        Some(&self.data[within..end])
    }
}

/// A journal located on a device, with replayed blocks held in memory.
///
/// Construction is pure: it reads and never writes. The overlaid device returned
/// by [`Journal::into_device`] reads through to the original for anything the
/// journal does not replace.
pub struct Journal<'a, D: ?Sized> {
    info: JournalInfoBlock,
    header: Option<JournalHeader>,
    transactions: Vec<Transaction>,
    overlay: Vec<ReplayedBlock>,
    /// The volume's allocation block size, which `bnum` is measured in.
    block_size: u32,
    device: &'a D,
}

impl<'a, D: BlockDevice + ?Sized> std::fmt::Debug for Journal<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("offset", &self.info.offset)
            .field("size", &self.info.size)
            .field("transactions", &self.transactions.len())
            .field("replayed_blocks", &self.overlay.len())
            .field("has_header", &self.header.is_some())
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> Journal<'a, D> {
    /// Locate and replay the journal of a volume, if it has one.
    ///
    /// Returns `Ok(None)` when the volume is not journaled, which is the normal
    /// case and not an error. Mining reference: `core/hfs_vfsutils.c` only
    /// attempts to open a journal when `kHFSVolumeJournaledBit` is set.
    pub fn open(device: &'a D, journal_info_block: u32, block_size: u32) -> Result<Option<Self>> {
        let device_len = device.len()?;

        // The info block occupies one allocation block, so the read is bounded by
        // the device rather than by anything the volume claims.
        let mut buf = vec![0u8; block_size as usize];
        let at = u64::from(journal_info_block)
            .checked_mul(u64::from(block_size))
            .ok_or(Error::overflow("journal info block offset"))?;
        device.read_at(at, &mut buf)?;

        let info = JournalInfoBlock::parse(&buf)?;
        info.validate(device_len)?;

        let flags = info.flag_set();
        if !flags.in_filesystem() {
            // The journal lives on another device, named by a GPT UUID. Locating
            // that device is a mount-time policy decision, not a parser's, and
            // this crate reads only the image it was handed.
            return Ok(None);
        }

        let mut header_bytes = vec![0u8; 4096.min(info.size as usize).max(512)];
        device.read_at(info.offset, &mut header_bytes)?;
        let header = JournalHeader::parse(&header_bytes)?;

        let mut journal = Journal {
            info,
            header,
            transactions: Vec::new(),
            overlay: Vec::new(),
            block_size,
            device,
        };

        if flags.needs_init() {
            // The journal exists but no transaction was ever written. Its header
            // area is zeroed, and there is nothing to replay.
            return Ok(Some(journal));
        }

        journal.replay()?;
        Ok(Some(journal))
    }

    /// Walk the journal's transactions and build the overlay.
    fn replay(&mut self) -> Result<()> {
        let Some(header) = self.header else {
            // No header means no transactions. Treated as an empty journal rather
            // than an error, because an uninitialized journal is a normal state.
            return Ok(());
        };
        header.validate(self.info.size)?;

        let mut offset = header.start;
        let mut guard = 0u32;
        while offset < header.end {
            guard += 1;
            if guard > MAX_TRANSACTIONS {
                return Err(Error::invalid(
                    "journal",
                    "transaction walk exceeded the transaction limit",
                ));
            }
            let transaction = self.read_transaction(offset, &header)?;
            let end = transaction.end;
            if end <= offset {
                return Err(Error::invalid(
                    "journal",
                    format!("transaction at {offset} does not advance"),
                ));
            }
            offset = end;
            self.transactions.push(transaction);
        }
        self.build_overlay()?;
        Ok(())
    }

    /// Read one transaction starting at `offset` within the journal.
    fn read_transaction(&mut self, offset: u64, header: &JournalHeader) -> Result<Transaction> {
        let mut block_lists = Vec::new();
        let mut data_cursor = offset;
        let sequence_num = header.sequence_num;

        for _ in 0..MAX_BLOCK_LISTS {
            // The block list occupies one blhdr_size block; its data follows.
            let data_offset = data_cursor
                .checked_add(u64::from(header.blhdr_size))
                .ok_or(Error::overflow("transaction cursor"))?;
            let bytes = self.read_journal(data_cursor, header.blhdr_size as usize)?;
            let blhdr = BlockListHeader::parse_at(&bytes, data_offset)?;
            if blhdr.num_blocks == 0 {
                return Err(Error::invalid(
                    "block_list_header",
                    format!("empty block list at journal offset {data_cursor}"),
                ));
            }

            // `bytes_used` says how much data this list carries.
            let used = u64::from(blhdr.bytes_used);
            if used == 0 {
                return Err(Error::invalid(
                    "block_list_header.bytes_used",
                    "a block list with no data cannot be replayed",
                ));
            }
            block_lists.push(blhdr);

            data_cursor = data_offset
                .checked_add(used)
                .ok_or(Error::overflow("transaction cursor"))?;
            if data_cursor > header.end {
                return Err(Error::out_of_range("transaction", data_cursor, header.end));
            }
            // Stop when no further block list fits before the end of the journal.
            if data_cursor + u64::from(header.blhdr_size) >= header.end {
                break;
            }
        }

        Ok(Transaction { sequence_num, offset, end: data_cursor, block_lists })
    }

    /// Read `len` bytes at `offset` within the journal.
    fn read_journal(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(Error::overflow("journal read"))?;
        if end > self.info.size {
            return Err(Error::out_of_range("journal read", end, self.info.size));
        }
        let abs = self
            .info
            .offset
            .checked_add(offset)
            .ok_or(Error::overflow("journal read"))?;
        self.device.read_vec(abs, len)
    }

    /// Replace every block the transactions rewrote with its journal contents.
    fn build_overlay(&mut self) -> Result<()> {
        // Later transactions win, so blocks are applied in order and an earlier
        // write to the same block is overwritten.
        for transaction in &self.transactions {
            for list in &transaction.block_lists {
                let mut data_cursor = 0u64;
                for block in &list.blocks {
                    if block.bnum == END_BLK_NUM {
                        break;
                    }
                    let size = block.bsize as usize;
                    if size == 0 {
                        continue;
                    }
                    let start = list
                        .data_offset
                        .checked_add(data_cursor)
                        .ok_or(Error::overflow("replayed block data"))?;
                    let data = self.read_journal(start, size)?;
                    data_cursor += size as u64;

                    if list.checks_blocks() && calc_checksum(&data) != block.cksum {
                        return Err(Error::invalid(
                            "journal block checksum",
                            format!("block {} failed its recorded checksum", block.bnum),
                        ));
                    }

                    let device_offset = block
                        .bnum
                        .checked_mul(u64::from(self.block_size))
                        .ok_or(Error::overflow("replayed block offset"))?;
                    match self
                        .overlay
                        .iter_mut()
                        .find(|b| b.device_block == block.bnum)
                    {
                        // A later transaction supersedes an earlier write to the
                        // same block, so this is a replacement rather than an
                        // append.
                        Some(existing) => {
                            existing.device_offset = device_offset;
                            existing.data = data;
                        }
                        None => self.overlay.push(ReplayedBlock {
                            device_block: block.bnum,
                            device_offset,
                            data,
                        }),
                    }
                }
            }
        }
        self.overlay.sort_by_key(|b| b.device_offset);
        Ok(())
    }

    /// The volume's `JournalInfoBlock`.
    pub fn info(&self) -> &JournalInfoBlock {
        &self.info
    }

    /// The journal header, if the journal has been initialised.
    pub fn header(&self) -> Option<&JournalHeader> {
        self.header.as_ref()
    }

    /// Whether the journal has never been written to.
    pub fn is_uninitialized(&self) -> bool {
        self.info.flag_set().needs_init()
    }

    /// The transactions found, in journal order.
    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// The blocks replayed on top of the filesystem.
    pub fn replayed_blocks(&self) -> &[ReplayedBlock] {
        &self.overlay
    }

    /// Wrap the device so reads consult the overlay.
    ///
    /// The returned value borrows both, so neither can be dropped while the
    /// replayed view is in use.
    pub fn into_device(&'a self) -> OverlaidDevice<'a, 'a, D> {
        OverlaidDevice { inner: self.device, overlay: &self.overlay }
    }
}

/// Cap on transactions walked per journal.
const MAX_TRANSACTIONS: u32 = 1 << 16;

/// Cap on block lists within one transaction.
const MAX_BLOCK_LISTS: u32 = 1 << 12;

/// A read-only device that consults a journal overlay first.
pub struct OverlaidDevice<'o, 'j, D: ?Sized> {
    inner: &'o D,
    overlay: &'j [ReplayedBlock],
}

impl<D: ?Sized> std::fmt::Debug for OverlaidDevice<'_, '_, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlaidDevice")
            .field("inner", &std::any::type_name::<D>())
            .field("replayed_blocks", &self.overlay.len())
            .finish()
    }
}

impl<D: BlockDevice + ?Sized> OverlaidDevice<'_, '_, D> {
    /// Look up a replayed block by device byte offset.
    ///
    /// `at` is a byte offset on the device, so a caller that knows a block number
    /// multiplies by the volume's block size first.
    pub fn block_at(&self, at: u64) -> Option<&ReplayedBlock> {
        self.overlay.iter().find(|b| b.covers(at))
    }
}

impl<D: BlockDevice + ?Sized> BlockDevice for OverlaidDevice<'_, '_, D> {
    /// Read through the overlay, falling back to the device for anything the
    /// journal does not replace.
    ///
    /// Resolution is per byte offset rather than per block because a replayed
    /// block's size comes from the journal and may differ from the volume's
    /// allocation block size.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.overlay.is_empty() || buf.is_empty() {
            return self.inner.read_at(offset, buf);
        }

        let mut written = 0usize;
        while written < buf.len() {
            let at = offset + written as u64;
            match self.overlay.iter().find_map(|b| {
                b.slice_from(at, buf.len() - written).map(|slice| (b, slice))
            }) {
                Some((_, slice)) => {
                    buf[written..written + slice.len()].copy_from_slice(slice);
                    written += slice.len();
                }
                None => {
                    // The rest of the request is not replayed; one read covers it.
                    self.inner.read_at(at, &mut buf[written..])?;
                    written = buf.len();
                }
            }
        }
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_list_header_prefix_sizes_match_apple() {
        assert_eq!(BLHDR_PREFIX_SIZE, 16);
        assert_eq!(BLOCK_INFO_SIZE, 16);
        assert_eq!(super::super::checksum::BLHDR_CHECKSUM_SIZE, 32);
    }

    #[test]
    fn block_list_header_round_trips() {
        let mut raw = vec![0u8; BLHDR_PREFIX_SIZE + 2 * BLOCK_INFO_SIZE];
        raw[0..2].copy_from_slice(&16u16.to_be_bytes()); // max_blocks
        raw[2..4].copy_from_slice(&2u16.to_be_bytes()); // num_blocks
        raw[4..8].copy_from_slice(&8192u32.to_be_bytes()); // bytes_used
        raw[12..16].copy_from_slice(&BLHDR_FIRST_HEADER.to_be_bytes());
        for (i, (bnum, bsize)) in [(100u64, 4096u32), (101, 4096)].iter().enumerate() {
            let off = BLHDR_PREFIX_SIZE + i * BLOCK_INFO_SIZE;
            raw[off..off + 8].copy_from_slice(&bnum.to_be_bytes());
            raw[off + 8..off + 12].copy_from_slice(&bsize.to_be_bytes());
        }
        let cksum = crate::journal::checksum::checksum_with_zeroed_field(&raw, 8, BLHDR_CHECKSUM_SIZE)
            .expect("the buffer is long enough")
            .to_be_bytes();
        raw[8..12].copy_from_slice(&cksum);

        let h = BlockListHeader::parse(&raw).unwrap();
        assert_eq!(h.max_blocks, 16);
        assert_eq!(h.num_blocks, 2);
        assert_eq!(h.bytes_used, 8192);
        assert!(h.is_first());
        assert!(!h.checks_blocks());
        assert_eq!(h.blocks.len(), 2);
        assert_eq!(h.blocks[0].bnum, 100);
        assert_eq!(h.blocks[1].bsize, 4096);
        assert!(h.checksum_matches(&raw));
    }

    #[test]
    fn block_list_header_rejects_impossible_counts() {
        let mut raw = vec![0u8; BLHDR_PREFIX_SIZE + BLOCK_INFO_SIZE];
        raw[0..2].copy_from_slice(&4u16.to_be_bytes()); // max_blocks
        raw[2..4].copy_from_slice(&100u16.to_be_bytes()); // num_blocks > capacity
        assert!(matches!(
            BlockListHeader::parse(&raw),
            Err(Error::Truncated { .. })
        ));

        // num_blocks above max_blocks is corrupt even when the bytes are there.
        let mut big = vec![0u8; BLHDR_PREFIX_SIZE + 8 * BLOCK_INFO_SIZE];
        big[0..2].copy_from_slice(&2u16.to_be_bytes());
        big[2..4].copy_from_slice(&8u16.to_be_bytes());
        assert!(matches!(
            BlockListHeader::parse(&big),
            Err(Error::InvalidField { .. })
        ));
    }

    #[test]
    fn a_short_block_list_buffer_is_truncated_not_panicked_on() {
        for len in [0usize, 1, 8, 15] {
            assert!(matches!(
                BlockListHeader::parse(&vec![0u8; len]),
                Err(Error::Truncated { .. })
            ));
        }
    }

    #[test]
    fn a_corrupt_block_list_checksum_is_detected() {
        let mut raw = vec![0u8; BLHDR_PREFIX_SIZE + BLOCK_INFO_SIZE];
        raw[2..4].copy_from_slice(&1u16.to_be_bytes());
        raw[0..2].copy_from_slice(&1u16.to_be_bytes());
        let cksum = crate::journal::checksum::checksum_with_zeroed_field(&raw, 8, BLHDR_CHECKSUM_SIZE)
            .expect("the buffer is long enough")
            .to_be_bytes();
        raw[8..12].copy_from_slice(&cksum);
        let h = BlockListHeader::parse(&raw).unwrap();
        assert!(h.checksum_matches(&raw));
        raw[BLHDR_PREFIX_SIZE] ^= 0xFF;
        assert!(!h.checksum_matches(&raw));
    }
}