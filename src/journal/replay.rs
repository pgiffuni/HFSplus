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
use super::info::{JournalHeader, JournalInfoBlock, END_BLK_NUM, JOURNAL_HEADER_MAGIC};
use crate::blockdev::{BlockDevice, BlockDeviceMut};
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

/// Index of the `binfo` entry that is **not** a block.
///
/// This is the most consequential detail of the replay algorithm and it is easy
/// to miss. `binfo[0]` holds the transaction's sequence number, unioned with the
/// checksum word:
///
/// ```c
/// typedef struct _blk_info {
///     int32_t    bsize;
///     union { int32_t cksum; uint32_t sequence_num; } b;
/// } _blk_info;
/// ```
///
/// Apple's replay loop starts at index 1:
///
/// ```c
/// for (i = 1; i < blhdr->num_blocks; i++) { ... add_block(...) }
/// ```
///
/// so `num_blocks` counts the sequence slot. Replaying `binfo[0]` as a block
/// would fabricate a filesystem block out of a transaction counter, silently, on
/// a real macOS journal. A list describing one block therefore has
/// `num_blocks == 2`.
pub const FIRST_BLOCK_INDEX: usize = 1;

/// `BLHDR_FIRST_HEADER`: this block list begins a transaction.
pub const BLHDR_FIRST_HEADER: u32 = 0x0000_0002;

/// `BLHDR_CHECK_CHECKSUMS`: the recorded blocks carry their own checksums.
pub const BLHDR_CHECK_CHECKSUMS: u32 = 0x0000_0001;

/// One filesystem block as recorded in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedBlock {
    /// Block number on the filesystem device.
    ///
    /// `0xFFFF_FFFF_FFFF_FFFF` means "killed": Apple skips such a block rather
    /// than replaying it, and so does this.
    pub bnum: u64,
    /// Size of the block in bytes.
    pub bsize: u32,
    /// Checksum of the block's contents, or the transaction's sequence number
    /// when [`BlockListHeader::checks_blocks`] is clear, because the two share
    /// one word on disk.
    pub cksum: u32,
}

impl RecordedBlock {
    /// Whether this block was killed and must not be replayed.
    ///
    /// Mining reference: `core/hfs_journal.c` skips `number == (off_t)-1` with
    /// the comment "don't add \"killed\" blocks".
    pub fn is_killed(&self) -> bool {
        self.bnum == END_BLK_NUM
    }
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
    /// The transaction's sequence number, taken from `binfo[0]`.
    ///
    /// Only meaningful when [`BlockListHeader::checks_blocks`] is clear, because
    /// otherwise the same word is the sequence slot's checksum. Either way it is
    /// never a block.
    pub sequence_num: u32,
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

        // binfo[0] is the sequence-number slot, so a list needs at least that
        // plus one block.
        if usize::from(num_blocks) <= FIRST_BLOCK_INDEX {
            return Err(Error::invalid(
                "block_list_header.num_blocks",
                format!("{num_blocks} is too small to hold the sequence slot and a block"),
            ));
        }

        let sequence_num = be.u32(BLHDR_PREFIX_SIZE + 12)?;

        let mut blocks = Vec::with_capacity(usize::from(num_blocks) - FIRST_BLOCK_INDEX);
        for i in FIRST_BLOCK_INDEX..usize::from(num_blocks) {
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
            sequence_num,
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
        match crate::journal::checksum::checksum_with_zeroed_field(raw, 8, BLHDR_CHECKSUM_SIZE) {
            Some(c) => c == self.checksum,
            None => false,
        }
    }
}

/// One transaction: a run of block lists followed by their data.
///
/// # What a writer has to produce, which is the inverse of what this reads
///
/// This type is what replay *finds*. The writer's job is the other direction, and
/// mining `end_transaction` in `core/hfs_journal.c` gives the order -- which is
/// worth writing down, because it is not the obvious one:
///
/// 1. **Check for room first.** `check_free_space(jnl, tr->total_bytes,
///    &tr->delayed_header_write, jnl->saved_sequence_num)` runs before anything is
///    written, and it may decide to *defer the journal header write* rather than
///    fail. So the space decision comes first and can change what is emitted.
/// 2. **Record the sequence number**: `jnl->saved_sequence_num = jnl->sequence_num`.
/// 3. **Validate the write head**: `if (jnl->jhdr->end <= 0 || jnl->jhdr->end >
///    jnl->jhdr->size)` -- an end outside the journal is rejected rather than
///    clamped, so a corrupt header stops the transaction instead of writing past it.
/// 4. **`tr->journal_start = jnl->jhdr->end`** -- where this transaction begins.
/// 5. **Write each block list, then its blocks**:
///    ```c
///    for (blhdr = tr->blhdr; blhdr; blhdr = next) {
///            for (i = 1; i < blhdr->num_blocks; i++) {
///                    if (blhdr->binfo[i].bnum != (off_t)-1) {
///                            ... write ...
///                    }
///            }
///    }
///    ```
///    **`i` starts at 1**, because index 0 is the block-list header's *own* block --
///    the list is stored in a journal block, and that block is not one of the blocks
///    the list describes. A writer that started at 0 would list its own header as
///    recorded data.
///
/// The `do/while (err == EAGAIN)` around each write is the device being retried,
/// not a retry of the whole transaction: a transaction is written once, in order,
/// and a torn write is recovered by the journal header not having been advanced.
///
/// # Why the order of blocks inside a list is not the write order
///
/// A block list header records *which* blocks changed, and they need not be written
/// in list order for recovery to work -- replay writes them in whatever order the
/// journal holds them. But a writer should still emit them in a deterministic order,
/// because the journal's own space accounting and its `end` advance assume a stable
/// layout, and because a transaction whose bytes depend on iteration order of a hash
/// map is not reproducible.
///
/// # A journalled write never touches the home block
///
/// This is the fact that decides how a writer is shaped, and it is not visible
/// from the replay side at all.
///
/// A block that changes is **copied into the transaction's own buffer** as it is
/// dirtied. The data is written from that buffer at commit:
///
/// ```c
/// blkptr = (char*)&blhdrim->buffers[tbuffer_offset];
/// ```
///
/// so the home block is untouched until the transaction commits, and a crash before
/// that leaves the filesystem exactly as it was -- the journal holds the only copy of
/// the change. That is why `end_transaction` has nothing to undo and why a torn write
/// is recovered by the journal header not having advanced.
///
/// The port consequence is that a journalled writer here is not "write, then journal
/// what I wrote". It is "copy into a transaction buffer, then commit", which means
/// every mutation in this crate that currently writes a home block would have to be
/// re-expressed to write into the buffer instead. That is why journal writing is not
/// a feature that can be added after the mutations but a change to all of them.
///
/// # An empty transaction, and a killed block
///
/// Two things a writer must be able to express, both of which replay has to
/// recognise and neither of which the current mutations need:
///
/// - **An empty transaction.** `if (tr->total_bytes == jnl->jhdr->blhdr_size)` is
///   how a transaction with nothing in it is recognised, so "no blocks changed" is a
///   representable event and not a special case to be elided.
/// - **A killed block.** `blhdr->binfo[i].bnum = (off_t)-1` marks a block that was
///   freed, and the write loop skips it: `if (blhdr->binfo[i].bnum != (off_t)-1)`.
///   So releasing a block is something the journal records rather than the absence
///   of a record. That is the shape a future `remove`-through-a-link has to take.
///
/// Mining reference: `core/hfs_journal.c` `end_transaction`; the block-list header
/// and its checksum are parsed by [`BlockListHeader`] in this module.
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
    /// Whether the journal header's checksum matched, or `None` when it was not
    /// checked.
    checksum_ok: Option<bool>,
    /// The volume's allocation block size, which the *device* is addressed in.
    block_size: u32,
    /// Where replay stopped short, if it did.
    truncated_at: Option<u64>,
    /// Why replay stopped short.
    truncation_reason: Option<String>,
    device: &'a D,
}

impl<'a, D: BlockDevice + ?Sized> std::fmt::Debug for Journal<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("offset", &self.info.offset)
            .field("size", &self.info.size)
            .field("transactions", &self.transactions.len())
            .field("truncated_at", &self.truncated_at)
            .field("replayed_blocks", &self.overlay.len())
            .field("has_header", &self.header.is_some())
            .field("header_checksum_ok", &self.checksum_ok)
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> Journal<'a, D> {
    /// Locate and replay the journal of a volume, if it has one.
    ///
    /// Returns `Ok(None)` when the volume is not journaled, which is the normal
    /// case and not an error. Mining reference: `core/hfs_vfsutils.c` only
    /// attempts to open a journal when `kHFSVolumeJournaledBit` is set.
    /// The block size the journal addresses itself in.
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

        // Read no more than the journal itself holds. The artificial floor that
        // used to sit here could ask the device for bytes a small journal near the
        // end of an image does not contain, turning "there is no header here" into
        // a confusing truncation error. The parser decides what is enough.
        let want = usize::try_from(info.size)
            .map_err(|_| Error::overflow("journal header read length"))?
            .min(MAX_HEADER_PROBE);
        let mut header_bytes = vec![0u8; want];
        device.read_at(info.offset, &mut header_bytes)?;
        let header = JournalHeader::parse(&header_bytes)?;

        // Apple's checksum diagnostic. It computes the header checksum, compares
        // it with the stored value, and *deliberately does not fail* on a
        // mismatch: the `goto bad_journal` is commented out. So this is reported
        // rather than enforced, and only for the current magic, which is exactly
        // the condition Apple guards with.
        //
        // Mining reference: core/hfs_journal.c `journal_open`.
        let checksum_ok = match &header {
            Some(h) if h.magic == JOURNAL_HEADER_MAGIC => Some(h.checksum_matches(&header_bytes)),
            _ => None,
        };

        let mut journal = Journal {
            info,
            header,
            checksum_ok,
            transactions: Vec::new(),
            overlay: Vec::new(),
            block_size,
            truncated_at: None,
            truncation_reason: None,
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

    /// Walk the journal's block lists and build the overlay.
    ///
    /// Apple does not model transaction boundaries during replay: it reads block
    /// list headers from `start` to `end` and applies each in order, using
    /// `BLHDR_FIRST_HEADER` only to note where a transaction began. This walks
    /// the same way and then groups the block lists into transactions using that
    /// flag, so the reported grouping is the one the journal itself records
    /// rather than one invented from offsets.
    ///
    /// On damage Apple **truncates** rather than abandoning: it stops at the bad
    /// block list, keeps everything replayed before it, and adjusts the journal's
    /// end. Its own comment says why:
    ///
    /// ```c
    /// XXXdbg - if these checks fail, we should replay as much
    /// ///         as we can in the hopes that it will still leave the
    /// ///         drive in a better state than if we didn't replay
    /// ///         anything
    /// ```
    ///
    /// A read-only mount that refused the whole journal would instead show a
    /// filesystem missing every recent change, which is a worse state than one
    /// missing changes from the damage point onwards.
    fn replay(&mut self) -> Result<()> {
        let Some(header) = self.header else {
            // No header means no transactions. Treated as an empty journal rather
            // than an error, because an uninitialised journal is a normal state.
            return Ok(());
        };
        header.validate(self.info.size)?;

        let blhdr_size = u64::from(header.blhdr_size);
        let mut offset = header.start;
        let mut guard = 0u32;
        // The sequence number of the previous block list, for the ordering rule
        // below. Zero means "not yet seen", which is also what a pre-sequence
        // journal reports, and the rule skips the comparison in that case.
        let mut last_sequence_num: u32 = 0;

        // Walk to the journal's real end, not `header.end`.
        //
        // Apple's loop is `while (check_past_jnl_end || jnl->jhdr->start !=
        // jnl->jhdr->end)`, and `check_past_jnl_end` is cleared only for a
        // pre-sequence-number journal. So when sequence numbers are in use it
        // keeps going past `end`, printing "examining extra transactions" -- the
        // header's `end` can be stale, which is what a crash between writing a
        // transaction and updating the header leaves behind.
        //
        // Stopping at `end` here would silently under-recover exactly that case.
        //
        // Mining reference: `core/hfs_journal.c` `replay_journal`.
        let mut past_end = false;
        while offset < header.size {
            guard += 1;
            if guard > MAX_BLOCK_LISTS {
                return Err(Error::invalid(
                    "journal",
                    "block list walk exceeded the block list limit",
                ));
            }

            // The block list occupies one blhdr_size block; its data follows.
            let data_offset = offset
                .checked_add(blhdr_size)
                .ok_or(Error::overflow("block list cursor"))?;
            let bytes = self.read_journal(offset, header.blhdr_size as usize)?;

            // Past `end`, an empty list means the journal's real transactions
            // have run out -- it is not damage, and `parse_at` rightly refuses it
            // before the checks below can tell the difference. So the walk stops
            // here rather than reporting a truncation that never happened.
            if offset >= header.end && header.start <= header.end && bytes.len() >= 4 {
                let raw_num = u16::from_be_bytes([bytes[2], bytes[3]]);
                if raw_num == 0 {
                    break;
                }
            }

            let blhdr = BlockListHeader::parse_at(&bytes, data_offset)?;

            // Past `end`, a list that does not hold together is not damage: it is
            // where the journal's real transactions stop. Apple reaches the same
            // place by truncating there and moving `end` up, but truncating a
            // well-formed journal at the zeroed block just past its last
            // transaction would report damage that does not exist. Before `end` it
            // is damage, and is truncated.
            let at_or_past_end = offset >= header.end && header.start <= header.end;

            // A pre-sequence-number journal cannot be walked past `end`: Apple
            // clamps `start = end` and stops, because there is no sequence number
            // to tell a real transaction from a stale header.
            if at_or_past_end && last_sequence_num == 0 {
                break;
            }

            // Validate as Apple does, before any of its contents are used.
            if blhdr.num_blocks == 0 || blhdr.num_blocks > blhdr.max_blocks {
                if past_end {
                    break;
                }
                self.truncate_at(offset, "block list counts are inconsistent")?;
                break;
            }
            if blhdr.bytes_used == 0 {
                if past_end {
                    break;
                }
                self.truncate_at(offset, "block list carries no data")?;
                break;
            }
            if !blhdr.checksum_matches(&bytes) {
                // Apple truncates here rather than abandoning the replay.
                if past_end {
                    break;
                }
                self.truncate_at(offset, "block list header checksum mismatch")?;
                break;
            }
            let used = u64::from(blhdr.bytes_used);
            let runs_past_end = data_offset
                .checked_add(used)
                .map(|end| end > header.size)
                .unwrap_or(true);
            if runs_past_end {
                self.truncate_at(offset, "block list data runs past the journal end")?;
                break;
            }

            // Apple's sequence-number rule, which is what tells a journal that
            // continues from one that was reset.
            //
            // Mining reference: `core/hfs_journal.c` `replay_journal` keeps
            // `last_sequence_num` and, when a block list's sequence is neither
            // that value nor one more -- and both are non-zero -- it sets
            // `txn_start_offset = jnl->jhdr->end = blhdr_offset` and continues.
            // That is the same truncation this code already does for a bad
            // checksum, and for the same reason: replaying as much as possible
            // leaves the filesystem in a better state than replaying nothing.
            //
            // The zeros matter. A sequence of 0 means "written before
            // sequence numbers existed", and Apple's guard skips the comparison
            // rather than treating 0 as out of order -- otherwise every
            // pre-sequence journal would truncate at its first block list.
            if last_sequence_num != 0
                && blhdr.sequence_num != 0
                && blhdr.sequence_num != last_sequence_num
                && blhdr.sequence_num != last_sequence_num.wrapping_add(1)
            {
                self.truncate_at(offset, "block list sequence number is out of order")?;
                break;
            }
            last_sequence_num = blhdr.sequence_num;

            // `max_blocks` is how many blocks this list could hold, so it cannot
            // exceed the blocks the journal has. Mining reference: the same
            // function rejects `blhdr->max_blocks > (jhdr->size / jhdr->jhdr_size)`.
            // Plain integer division, as Apple writes it: `max_blocks >
            // (jhdr->size / jhdr->jhdr_size)`. Rounding up would make the bound
            // one block looser than Apple's, and a list claiming that many blocks
            // would be accepted here and refused there.
            let jhdr_size = u64::from(header.jhdr_size);
            let capacity = header.size / jhdr_size;
            if u64::from(blhdr.max_blocks) > capacity {
                self.truncate_at(
                    offset,
                    "block list claims more blocks than the journal holds",
                )?;
                break;
            }

            // A first header starts a new transaction.
            if blhdr.is_first() || self.transactions.is_empty() {
                self.transactions.push(Transaction {
                    sequence_num: blhdr.sequence_num,
                    offset,
                    end: data_offset,
                    block_lists: Vec::new(),
                });
            }

            // Mark where we are relative to the header's `end`, so the checks
            // above can tell damage from the end of the journal.
            past_end = data_offset > header.end;

            // The cursor is *not* wrapped here. A real ring is full -- the writer
            // only laps itself once every block has been used -- and `end` is the
            // point the walk stops at, so a cursor that needs wrapping means the
            // journal has no transactions left rather than one more somewhere
            // unexpected. The *data* read does wrap; that is in `read_journal`.
            let next = data_offset
                .checked_add(used)
                .ok_or(Error::overflow("block list cursor"))?;
            if next <= offset {
                self.truncate_at(offset, "block list does not advance")?;
                break;
            }

            if let Some(current) = self.transactions.last_mut() {
                current.block_lists.push(blhdr);
                current.end = next;
            }
            offset = next;
        }

        self.build_overlay()?;
        Ok(())
    }

    /// Record that replay stopped short at `offset`, keeping what came before.
    ///
    /// Mining reference: `core/hfs_journal.c` sets `jnl->jhdr->end =
    /// blhdr_offset` and continues, which is exactly this. The reason is
    /// recorded in Apple's own comment: replaying as much as possible leaves the
    /// filesystem in a better state than replaying nothing.
    /// Note that replay stopped short at `offset`, keeping what came before.
    ///
    /// Returns an error when there is nothing to keep, because that is the case
    /// Apple refuses rather than tolerates: if no transaction start was ever
    /// found, `replay_journal` prints "no known good txn start offset! aborting
    /// journal replay" and returns -1, and `journal_open` then returns NULL,
    /// which `core/hfs_vfsops.c` `hfs_mount_existing` treats as `EINVAL` -- the
    /// volume does not mount.
    ///
    /// The alternative is worse than an error. A journal that yields no
    /// transactions leaves the on-disk filesystem exactly as it was, and that
    /// filesystem is *stale*: every write in the journal is missing. Returning it
    /// as a successful mount serves a catalog that is out of date with no
    /// indication that anything is missing.
    ///
    /// Mining reference: `core/hfs_journal.c` `replay_journal`, the
    /// `txn_start_offset == 0` test at `bad_txn_handling`.
    ///
    /// # The retry rule, and why it is not implemented
    ///
    /// Apple's other response to damage is to retry: `jhdr->start = orig_jnl_start`
    /// and `jhdr->end = txn_start_offset`, then replay again, giving up after
    /// `replay_retry_count == 3`. That produces the same transactions as
    /// truncating in one pass, because the retry walks exactly the prefix that
    /// was already found. It differs only for a *transient* device error, where a
    /// second attempt might succeed.
    ///
    /// So there is nothing to gain here from looping, and a loop that re-parsed
    /// the same bytes could only obscure which check rejected what. Recorded
    /// rather than omitted silently, since Apple's rule is real.
    ///
    /// The order of the checks also matches Apple's exactly, which is what makes
    /// the abort land in the same place: `max_blocks`, `num_blocks` and the
    /// sequence rule are all tested *before* `BLHDR_FIRST_HEADER` sets
    /// `txn_start_offset`, while a block's zero `bsize` is found after it. So
    /// damage in the first block list's header aborts, and damage in its blocks
    /// truncates -- the same split Apple makes.
    fn truncate_at(&mut self, offset: u64, reason: &str) -> Result<()> {
        if self.transactions.is_empty() {
            return Err(Error::invalid(
                "journal",
                format!("no good transaction could be replayed before {offset}: {reason}"),
            ));
        }
        self.truncated_at = Some(offset);
        self.truncation_reason = Some(reason.to_string());
        if let Some(last) = self.transactions.last_mut() {
            last.end = offset.min(last.end);
        }
        Ok(())
    }

    /// Read `len` bytes at `offset` within the journal.
    /// Read `len` bytes from within the journal, wrapping at its end.
    ///
    /// The public form of what the replay walk uses, for a caller inspecting a
    /// journal by hand. Wrapping is what makes the offset meaningful: a read
    /// starting near the end continues from the beginning rather than failing.
    pub fn read_bytes(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.read_journal(offset, len)
    }

    /// Read `len` bytes from within the journal, wrapping at its end.
    ///
    /// The journal is a **ring**: when the writer reaches the end it starts again
    /// just after the header, so a block list's replacement data may straddle the
    /// wrap. Apple handles it in the walk:
    ///
    /// ```c
    /// if (offset >= jnl->jhdr->size) {
    ///     offset = jnl->jhdr->jhdr_size + (offset - jnl->jhdr->size);
    /// }
    /// ```
    ///
    /// Refusing a read that crosses the end instead would reject ordinary
    /// journals -- a transaction that begins near the end is normal, not
    /// damaged.
    ///
    /// Mining reference: `core/hfs_journal.c` `replay_journal`, the "increment
    /// offset" and "wrap to the beginning" comments.
    fn read_journal(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let size = self.info.size;
        let ring_start = self.journal_block_size();

        // A ring smaller than the header cannot wrap: the write position would
        // land on the header itself, so refuse rather than loop.
        if ring_start == 0 || ring_start >= size {
            let end = offset
                .checked_add(len as u64)
                .ok_or(Error::overflow("journal read"))?;
            if end > size {
                return Err(Error::out_of_range("journal read", end, size));
            }
            let abs = self
                .info
                .offset
                .checked_add(offset)
                .ok_or(Error::overflow("journal read"))?;
            return self.device.read_vec(abs, len);
        }

        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        let mut left = len;
        // Bounded so a ring that somehow fails to make progress cannot spin.
        let mut guard = 0usize;
        while left > 0 {
            guard += 1;
            if guard > 2 {
                return Err(Error::invalid(
                    "journal read",
                    "the wrap point made no progress",
                ));
            }
            if at >= size {
                // The wrap removes exactly one `size`, so it brings an offset
                // inside the ring only when that offset is within a lap. Anything
                // further out is refused rather than wrapped again: two laps
                // would subtract twice and land past the end, where `size - at`
                // underflows.
                //
                // Apple never reaches this because it applies the rule to an
                // offset it advanced by one step. The bound is here for a caller
                // that does not.
                if at - size >= size - ring_start {
                    return Err(Error::out_of_range(
                        "journal read offset",
                        offset,
                        size.saturating_add(size - ring_start),
                    ));
                }
                at = ring_start + (at - size);
            }
            let available = (size - at) as usize;
            let chunk = left.min(available);
            let abs = self
                .info
                .offset
                .checked_add(at)
                .ok_or(Error::overflow("journal read"))?;
            out.extend_from_slice(&self.device.read_vec(abs, chunk)?);
            at += chunk as u64;
            left -= chunk;
        }
        Ok(out)
    }

    /// Replace every block the transactions rewrote with its journal contents.
    ///
    /// Transactions are applied in journal order, so a later transaction
    /// supersedes an earlier write to the same block. A block whose recorded
    /// checksum does not match its data truncates the replay at the start of
    /// the transaction that owns it, which is what Apple does: it restarts and
    /// replays only the transactions it considers known good.
    ///
    /// Mining reference: `core/hfs_journal.c` sets `bad_blocks = 1; goto
    /// bad_txn_handling;` from inside the block loop, and `bad_txn_handling`
    /// then sets `jhdr->end = txn_start_offset` and restarts replay from the
    /// original start -- so the damaged transaction and everything after it are
    /// dropped while everything before survives. Apple bounds this to three
    /// retries before abandoning the journal entirely.
    /// The block size the journal addresses itself in.
    ///
    /// The journal header's own `jhdr_size` once it has been read, and the
    /// volume's block size before that -- the header probe cannot know otherwise.
    ///
    /// Equal to `block_size` on every volume Apple writes today, and equal on
    /// every image this project can generate, which is exactly why the
    /// distinction has to be carried rather than assumed.
    fn journal_block_size(&self) -> u64 {
        match self.header {
            Some(h) if h.jhdr_size != 0 => u64::from(h.jhdr_size),
            _ => u64::from(self.block_size),
        }
    }

    fn build_overlay(&mut self) -> Result<()> {
        for index in 0..self.transactions.len() {
            let transaction = self.transactions[index].clone();
            match self.apply_transaction(&transaction) {
                Ok(()) => {}
                Err(ApplyFailure::Truncate { at, reason }) => {
                    self.truncate_at(at, &reason)?;
                    self.transactions.truncate(index);
                    break;
                }
                Err(ApplyFailure::Fatal(e)) => return Err(e),
            }
        }
        self.overlay.sort_by_key(|b| b.device_offset);
        Ok(())
    }

    /// Apply one transaction's blocks to the overlay.
    fn apply_transaction(
        &mut self,
        transaction: &Transaction,
    ) -> std::result::Result<(), ApplyFailure> {
        // Apple's sanity pass over the whole list, before any of its contents
        // are used. A negative block number that is not the killed sentinel is
        // bogus, and letting it through would compute a device offset far
        // outside the image.
        //
        // Mining reference: core/hfs_journal.c
        //
        // ```c
        // if (blhdr->binfo[i].bnum < 0 && blhdr->binfo[i].bnum != (off_t)-1) {
        //     printf("... bogus block number 0x%llx\n", ...);
        //     bad_blocks = 1;
        //     goto bad_txn_handling;
        // }
        // ```
        for list in &transaction.block_lists {
            for block in &list.blocks {
                let negative = block.bnum > i64::MAX as u64;
                if negative && !block.is_killed() {
                    return Err(ApplyFailure::Truncate {
                        at: transaction.offset,
                        reason: format!("bogus block number {:#018x}", block.bnum),
                    });
                }
            }
        }

        for list in &transaction.block_lists {
            let mut data_cursor = 0u64;
            for block in &list.blocks {
                if block.is_killed() {
                    // Mining reference: core/hfs_journal.c skips a killed block
                    // with "don't add \"killed\" blocks", and still steps the
                    // data cursor by its size.
                    data_cursor += u64::from(block.bsize);
                    continue;
                }
                let size = block.bsize as usize;
                if size == 0 {
                    // A zero size is not an empty block to skip; it means the
                    // list is inconsistent. The data cursor advances by each
                    // block's size, so a zero here desynchronises every *later*
                    // block in the same list -- they would be read from the wrong
                    // offset and replay plausible nonsense. Apple refuses the
                    // transaction instead.
                    //
                    // Mining reference: `core/hfs_journal.c` `replay_journal`
                    // prints "invalid bsize" and goes to `bad_txn_handling`.
                    return Err(ApplyFailure::Truncate {
                        at: transaction.offset,
                        reason: "block list entry has a zero size".to_string(),
                    });
                }
                let start = list
                    .data_offset
                    .checked_add(data_cursor)
                    .ok_or_else(|| ApplyFailure::Fatal(Error::overflow("replayed block data")))?;
                let data = self
                    .read_journal(start, size)
                    .map_err(ApplyFailure::Fatal)?;
                data_cursor += size as u64;

                // A zero recorded checksum means "do not verify", which Apple
                // checks for explicitly before comparing.
                if list.checks_blocks() && block.cksum != 0 && calc_checksum(&data) != block.cksum {
                    return Err(ApplyFailure::Truncate {
                        at: transaction.offset,
                        reason: format!("block {} failed its recorded checksum", block.bnum),
                    });
                }

                // `bnum` is measured in the *journal header's* block size, not the
                // volume's. `add_block` computes `block_start = block_num *
                // jhdr_size`, so a volume whose logical block size differs from
                // the journal's -- which is what `journal_open` calls a resized
                // volume -- would place every block at the wrong offset otherwise.
                // The two agree on every image this project can generate, so the
                // distinction is invisible without saying so.
                //
                // Mining reference: `core/hfs_journal.c` `add_block`, and the
                // "the volume has probably been resized" comment in
                // `journal_open`.
                let device_offset = block
                    .bnum
                    .checked_mul(self.journal_block_size())
                    .ok_or_else(|| ApplyFailure::Fatal(Error::overflow("replayed block offset")))?;
                match self
                    .overlay
                    .iter_mut()
                    .find(|b| b.device_block == block.bnum)
                {
                    // A later transaction supersedes an earlier write to the
                    // same block, so this is a replacement rather than an append.
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

    /// Whether the journal holds no outstanding transactions.
    ///
    /// This is Apple's own predicate, and it is exactly `start == end`:
    ///
    /// > if the start and end are equal then the journal is clean. otherwise it's
    /// > not clean and therefore an error.
    ///
    /// Mining reference: `core/hfs_journal.c` `journal_is_clean`, which returns
    /// 0 for equal and `EBUSY` otherwise "so the caller can differentiate an
    /// invalid journal from a busy one".
    ///
    /// Distinct from [`Journal::is_uninitialized`], which reports the
    /// `kJIJournalNeedInitMask` flag -- a journal nobody has written to yet. A
    /// journal can be initialised and still be dirty, which is the ordinary state
    /// after a crash.
    ///
    /// # Why it is exposed
    ///
    /// Apple uses this to *refuse* a read-only mount: `hfs_vfsutils.c` checks
    /// `journal_is_clean` when the mount is read-only and not the root filesystem,
    /// and fails if the journal is busy.
    ///
    /// This crate takes the other branch. It replays the journal and serves the
    /// recovered view, which is what a read-only filesystem wants: the
    /// alternative is refusing to mount a volume whose filesystem is perfectly
    /// recoverable. The predicate is here so a caller that wants Apple's policy
    /// -- refuse rather than recover -- can implement it.
    pub fn is_clean(&self) -> bool {
        match self.header {
            // No header means nothing was ever written, which is clean.
            None => true,
            Some(h) => h.start == h.end,
        }
    }

    /// The transactions found, in journal order.
    ///
    /// Grouped by `BLHDR_FIRST_HEADER`, which is the flag the journal sets to
    /// mark where each transaction begins.
    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Every block list replayed, in journal order.
    pub fn block_lists(&self) -> usize {
        self.transactions.iter().map(|t| t.block_lists.len()).sum()
    }

    /// Whether the journal header's checksum matched.
    ///
    /// `None` when it was not checked, which is the case for a journal with no
    /// header, and for one using the legacy `JHDR` magic: Apple guards the check
    /// with `if (magic == JOURNAL_HEADER_MAGIC)`.
    ///
    /// A `Some(false)` is a diagnostic, not a failure. Apple prints a message and
    /// mounts anyway, because refusing would leave the filesystem missing every
    /// recent change.
    ///
    /// Mining reference: `core/hfs_journal.c` `journal_open`, where the
    /// `goto bad_journal` after the checksum mismatch is commented out.
    pub fn header_checksum_ok(&self) -> Option<bool> {
        self.checksum_ok
    }

    /// Where replay stopped short, and why.
    ///
    /// `Some((offset, reason))` means the journal was damaged partway and
    /// everything before that point was replayed, which is what Apple does
    /// rather than refusing the whole journal.
    pub fn truncation(&self) -> Option<(u64, &str)> {
        match (self.truncated_at, self.truncation_reason.as_deref()) {
            (Some(at), Some(why)) => Some((at, why)),
            _ => None,
        }
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
        OverlaidDevice {
            inner: self.device,
            overlay: &self.overlay,
        }
    }
}

/// Most bytes read when probing for a journal header.
///
/// A journal header is 48 bytes and occupies one filesystem block, so 4 KiB
/// covers any real volume. Reading more would be reading block-list data.
const MAX_HEADER_PROBE: usize = 4096;

/// Cap on block lists within one transaction.
const MAX_BLOCK_LISTS: u32 = 1 << 12;

/// Why applying a transaction failed.
///
/// Mining reference: Apple splits replay failures into "discard this transaction
/// and carry on" and "abandon the journal", which is exactly these two variants.
/// See `build_overlay`.
enum ApplyFailure {
    /// Drop this transaction and everything after it.
    Truncate {
        /// Journal offset at which replay stops.
        at: u64,
        /// Human-readable cause, reported rather than swallowed.
        reason: String,
    },
    /// Structural damage that stops replay entirely.
    Fatal(Error),
}

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
                b.slice_from(at, buf.len() - written)
                    .map(|slice| (b, slice))
            }) {
                Some((_, slice)) => {
                    buf[written..written + slice.len()].copy_from_slice(slice);
                    written += slice.len();
                }
                None => {
                    // Past the replayed region. Read only what the device
                    // actually has and then stop.
                    //
                    // Propagating a short read as an error would be wrong here
                    // even though it is right for `BlockDevice`: a request that
                    // begins inside a replayed block and runs past the end of
                    // the device has already been partly answered, and POSIX
                    // read reports end of file by returning fewer bytes rather
                    // than by failing.
                    let device_len = self.inner.len()?;
                    if at >= device_len {
                        break;
                    }
                    let want = ((device_len - at) as usize).min(buf.len() - written);
                    self.inner.read_at(at, &mut buf[written..written + want])?;
                    written += want;
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

    /// Build a block list header holding `blocks`, with `binfo[0]` as the
    /// sequence slot, exactly as `makejournal.py` writes it.
    fn make_blhdr(blocks: &[(u64, u32, u32)], flags: u32, blhdr_size: usize) -> Vec<u8> {
        let prefix = BLHDR_PREFIX_SIZE;
        let num_entries = blocks.len() + FIRST_BLOCK_INDEX;
        let mut raw = vec![0u8; blhdr_size];
        raw[0..2].copy_from_slice(&(num_entries as u16).to_be_bytes()); // max_blocks
        raw[2..4].copy_from_slice(&(num_entries as u16).to_be_bytes()); // num_blocks
        let bytes_used = (blocks.len() * 4096) as u32;
        raw[4..8].copy_from_slice(&bytes_used.to_be_bytes());
        raw[12..16].copy_from_slice(&flags.to_be_bytes());
        // binfo[0] is the sequence slot: bsize 0 and the sequence number.
        raw[prefix + 8..prefix + 12].copy_from_slice(&0u32.to_be_bytes());
        raw[prefix + 12..prefix + 16].copy_from_slice(&1u32.to_be_bytes());
        for (i, (bnum, bsize, cksum)) in blocks.iter().enumerate() {
            let off = prefix + (i + FIRST_BLOCK_INDEX) * BLOCK_INFO_SIZE;
            raw[off..off + 8].copy_from_slice(&bnum.to_be_bytes());
            raw[off + 8..off + 12].copy_from_slice(&bsize.to_be_bytes());
            raw[off + 12..off + 16].copy_from_slice(&cksum.to_be_bytes());
        }
        let cksum =
            super::super::checksum::checksum_with_zeroed_field(&raw, 8, BLHDR_CHECKSUM_SIZE)
                .expect("long enough");
        raw[8..12].copy_from_slice(&cksum.to_be_bytes());
        raw
    }

    #[test]
    fn block_list_header_round_trips() {
        let blocks = [(100u64, 4096u32, 0xAAAA_AAAAu32), (101, 4096, 0xBBBB_BBBB)];
        let raw = make_blhdr(&blocks, BLHDR_FIRST_HEADER, 4096);

        let h = BlockListHeader::parse(&raw).unwrap();
        // num_blocks counts the sequence slot, so two blocks means three.
        assert_eq!(h.max_blocks, 3);
        assert_eq!(h.num_blocks, 3);
        assert_eq!(h.blocks.len(), 2);
        assert!(h.is_first());
        assert!(!h.checks_blocks());
        assert_eq!(h.sequence_num, 1);
        assert_eq!(h.blocks[0].bnum, 100);
        assert_eq!(h.blocks[0].bsize, 4096);
        assert_eq!(h.blocks[0].cksum, 0xAAAA_AAAA);
        assert_eq!(h.blocks[1].bnum, 101);
        assert!(h.checksum_matches(&raw));
    }

    #[test]
    fn the_sequence_slot_is_never_replayed_as_a_block() {
        // The bug this guards: treating binfo[0] as a block would fabricate a
        // filesystem block out of a transaction counter. Apple iterates from 1.
        let blocks = [(100u64, 4096u32, 0u32)];
        let raw = make_blhdr(&blocks, BLHDR_FIRST_HEADER, 4096);
        let h = BlockListHeader::parse(&raw).unwrap();
        assert_eq!(h.blocks.len(), 1);
        assert_eq!(h.blocks[0].bnum, 100);
        assert!(
            h.blocks.iter().all(|b| b.bnum != 100 - 1),
            "the sequence slot must not appear as a block"
        );
    }

    #[test]
    fn a_list_too_small_to_hold_a_block_is_rejected() {
        // num_blocks == 1 is the sequence slot alone, which describes nothing.
        for n in [0u16, 1] {
            let mut raw = vec![0u8; BLHDR_PREFIX_SIZE + 2 * BLOCK_INFO_SIZE];
            raw[0..2].copy_from_slice(&n.max(1).to_be_bytes());
            raw[2..4].copy_from_slice(&n.to_be_bytes());
            assert!(
                matches!(
                    BlockListHeader::parse(&raw),
                    Err(Error::InvalidField {
                        field: "block_list_header.num_blocks",
                        ..
                    })
                ),
                "num_blocks {n} should be rejected"
            );
        }
    }

    #[test]
    fn a_killed_block_is_recognised() {
        let killed = RecordedBlock {
            bnum: END_BLK_NUM,
            bsize: 4096,
            cksum: 0,
        };
        assert!(killed.is_killed());
        let normal = RecordedBlock {
            bnum: 200,
            bsize: 4096,
            cksum: 0,
        };
        assert!(!normal.is_killed());
    }

    #[test]
    fn the_checks_flag_selects_how_the_union_word_is_read() {
        // The same word is a checksum with BLHDR_CHECK_CHECKSUMS and a sequence
        // number without it. Mining reference: _blk_info's union.
        let blocks = [(100u64, 4096u32, 0x1234_5678u32)];
        let with = make_blhdr(&blocks, BLHDR_CHECK_CHECKSUMS, 4096);
        assert!(BlockListHeader::parse(&with).unwrap().checks_blocks());
        let without = make_blhdr(&blocks, 0, 4096);
        assert!(!BlockListHeader::parse(&without).unwrap().checks_blocks());
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
        // Long enough for BLHDR_CHECKSUM_SIZE, since that is how much of the
        // header the checksum covers.
        let size = BLHDR_CHECKSUM_SIZE + 4 * BLOCK_INFO_SIZE;
        let raw = make_blhdr(&[(100u64, 4096u32, 0u32)], BLHDR_FIRST_HEADER, size);
        let h = BlockListHeader::parse(&raw).unwrap();
        assert!(h.checksum_matches(&raw));
        let mut corrupt = raw.clone();
        corrupt[BLHDR_PREFIX_SIZE] ^= 0xFF;
        assert!(
            !h.checksum_matches(&corrupt),
            "a corrupted field must fail the checksum"
        );
    }
}

/// One block recorded by a transaction, as a writer supplies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWrite {
    /// Block number on the filesystem device.
    pub bnum: u64,
    /// The block's new contents.
    pub data: Vec<u8>,
}

impl RecordedWrite {
    /// A record for a block that is being *released*, rather than written.
    ///
    /// A killed block is `bnum == 0xFFFF_FFFF_FFFF_FFFF`, which the write loop in
    /// `end_transaction` skips and replay skips too. So freeing a block is something
    /// the journal records -- the *absence* of a record would be indistinguishable
    /// from never having changed. That is the shape a future unlink-through-a-link
    /// has to take, and it is why the sentinel is a block number rather than a flag.
    pub fn killed() -> Self {
        RecordedWrite {
            bnum: KILLED_BNUM,
            data: Vec::new(),
        }
    }
}

/// The capacity of one block list's `binfo` array -- entries, **not** blocks.
///
/// Apple's `MAX_BLISTHDR_BLKS`, and the reason a transaction is split into several
/// block lists rather than growing one without limit: a list lives in a single
/// journal block, so the number of entries it can hold is bounded by that block's
/// size.
///
/// The distinction matters by exactly one. `binfo[0]` is the sequence-number slot
/// and not a block, so a list with this capacity describes
/// [`FIRST_BLOCK_INDEX`] fewer blocks than it has entries -- which is the same fact
/// as `end_transaction` starting its write loop at `i = 1`, seen from the other
/// side.
pub const MAX_BLOCKS_PER_LIST: usize = 127;

/// Build the bytes of one block list header, plus the blocks it describes.
///
/// The inverse of [`BlockListHeader::parse`], and written to be checked by it --
/// a writer verified only against its own encoder proves nothing.
///
/// # Layout, all offsets from the start of the header
///
/// | Offset | Size | Field |
/// | --- | --- | --- |
/// | 0 | 2 | `max_blocks` -- capacity of the `binfo` array |
/// | 2 | 2 | `num_blocks` -- how many `binfo` entries are valid |
/// | 4 | 4 | `bytes_used` -- transaction-buffer bytes this header accounts for |
/// | 8 | 4 | `checksum` over the first [`BLHDR_CHECKSUM_SIZE`] bytes, itself zeroed |
/// | 12 | 4 | `flags` |
/// | 16 | 16 | `binfo[0]` -- the sequence-number slot, **not a block** |
/// | 32 | 16 each | `binfo[1..]` -- `bnum: u64`, `bsize: u32`, `cksum: u32` |
///
/// `binfo[0]` is the detail that is easiest to get wrong, and it is the same one
/// `end_transaction`'s write loop encodes: it iterates `for (i = 1; ...)`, because
/// index 0 is the sequence slot and the header's own journal block is not one of the
/// blocks the list describes.
pub fn encode_block_list(
    sequence_num: u32,
    flags: u32,
    max_blocks: u16,
    blocks: &[RecordedWrite],
    check_blocks: bool,
) -> Result<Vec<u8>> {
    use crate::error::Error;
    use crate::journal::checksum::{calc_checksum, BLHDR_CHECKSUM_SIZE};

    if blocks.len() + FIRST_BLOCK_INDEX > usize::from(max_blocks) {
        return Err(Error::invalid(
            "block_list_header",
            format!(
                "{} block(s) plus the sequence slot exceeds max_blocks {max_blocks}",
                blocks.len()
            ),
        ));
    }

    let num_blocks = (FIRST_BLOCK_INDEX + blocks.len()) as u16;
    let bytes_used: u32 = blocks.iter().fold(0u32, |a, b| a + b.data.len() as u32);

    let need = BLHDR_PREFIX_SIZE + num_blocks as usize * BLOCK_INFO_SIZE;
    let mut out = vec![0u8; need];
    out[0..2].copy_from_slice(&max_blocks.to_be_bytes());
    out[2..4].copy_from_slice(&num_blocks.to_be_bytes());
    out[4..8].copy_from_slice(&bytes_used.to_be_bytes());
    // 8..12 is the checksum, zero for now and filled in below.
    out[12..16].copy_from_slice(&flags.to_be_bytes());
    out[BLHDR_PREFIX_SIZE + 12..BLHDR_PREFIX_SIZE + 16]
        .copy_from_slice(&sequence_num.to_be_bytes());
    for (i, b) in blocks.iter().enumerate() {
        let off = BLHDR_PREFIX_SIZE + (i + FIRST_BLOCK_INDEX) * BLOCK_INFO_SIZE;
        out[off..off + 8].copy_from_slice(&b.bnum.to_be_bytes());
        out[off + 8..off + 12].copy_from_slice(&(b.data.len() as u32).to_be_bytes());
        // With `checks_blocks` clear the word at `+12` is the sequence slot's
        // checksum rather than a block checksum, and replay says so -- so the writer
        // must not put a block checksum there.
        let cksum = if check_blocks {
            calc_checksum(&b.data)
        } else {
            sequence_num
        };
        out[off + 12..off + 16].copy_from_slice(&cksum.to_be_bytes());
    }

    // The checksum covers the header fields and the first `binfo` entry, and the
    // checksum field itself is zeroed while hashing -- the same convention the
    // reader's `checksum_matches` uses, so a mismatch here would be the reader's
    // bug rather than a tolerated difference.
    if out.len() < BLHDR_CHECKSUM_SIZE {
        return Err(Error::Truncated {
            what: "block list header",
            needed: BLHDR_CHECKSUM_SIZE,
            available: out.len(),
        });
    }
    let sum = calc_checksum(&out[..BLHDR_CHECKSUM_SIZE]);
    out[8..12].copy_from_slice(&sum.to_be_bytes());
    Ok(out)
}

/// Write a journal header to the device at the journal's own offset.
///
/// This is the non-FUA path of `write_journal_header` in `core/hfs_journal.c`:
/// encode the header (with a fresh checksum), then store it at byte zero of the
/// journal. The 1024-byte `jhdr_size` block is written, with the bytes beyond
/// the 48-byte struct left zero -- Apple's `memset(jnl->jhdr, 0, jnl->jhdr_size)`
/// before filling fields.
///
/// The caller is responsible for barrier ordering: a journal that writes the
/// header only advances once the transaction blocks it describes are durable.
/// [`commit_transaction`] enforces that ordering by syncing before the header
/// write, then writing and syncing the header.
///
/// Mining reference: `core/hfs_journal.c` `write_journal_header`.
pub fn write_journal_header<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    info: &JournalInfoBlock,
    header: &JournalHeader,
) -> Result<()> {
    let buf = header.to_bytes_alloc()?;
    if buf.len() < usize::try_from(header.jhdr_size).map_err(|_| Error::overflow("jhdr_size"))? {
        return Err(Error::invalid(
            "journal_header.jhdr_size",
            format!("buffer {} < jhdr_size {}", buf.len(), header.jhdr_size),
        ));
    }
    device.write_at(info.offset, &buf)?;
    device.sync()?;
    Ok(())
}

/// Commit one transaction to the journal.
///
/// This is the userspace port of `end_transaction` + `check_free_space` +
/// `write_journal_header` from `core/hfs_journal.c`. The ordering is what makes
/// a torn write recoverable:
///
/// 1. **Sync first.** Everything the mutation wrote through the device before
///    the transaction began is durable. This is the kernel's `HFS_SYNC_ON_UNLOCK`
///    and the `buf_bdwrite` calls in `hfs_update`; here it is the caller's
///    responsibility, and the `BlockDeviceMut` passed in is assumed synchronized.
/// 2. **Check space.** `check_free_space` with strict `>` and the pending count.
/// 3. **Write the transaction blocks.** The encoded image is stored at the
///    journal's current `end`, wrapping at `size`.
/// 4. **Sync again.** This is the pre-header barrier: on a non-FUA device it is
///    `DKIOCSYNCHRONIZE` with a barrier option. It guarantees the blocks are on
///    stable storage before `end` advances, so a crash after step 6 leaves a
///    header that does not yet describe them.
/// 5. **Advance `end` and bump `sequence_num`.** `end` wraps at `size`.
/// 6. **Write the journal header.** `write_journal_header(jnl, 0, sequence_num)`
///    with `updating_start = 0` -- we are advancing `end`, not `start`.
///
/// A crash at any point is safe:
/// - Before step 4: the old header still describes the previous transaction;
///   the new blocks are invisible.
/// - Between step 4 and step 6: the blocks are on disk but `end` hasn't moved;
///   replay ignores them.
/// - After step 6: the header names the new blocks; replay applies them.
///
/// `pending` is the count of unreplayed transactions (Apple's
/// `old_start[0] == 0` check). A non-zero count is a hard refusal: the caller
/// must replay first.
///
/// Mining reference: `core/hfs_journal.c` `end_transaction` (the write loop and
/// header advance at lines 4257-4292), and `write_journal_header` (the barrier
/// ordering at lines 467-541).
pub fn commit_transaction<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    info: &JournalInfoBlock,
    header: &mut JournalHeader,
    tx: &TransactionBuffer,
    blhdr_size: u32,
    check_blocks: bool,
    pending: usize,
) -> Result<()> {
    let desired = tx.total_bytes(blhdr_size);
    // `SequenceNum` is used for the header; the transaction gets `sequence_num + 1`.
    let sequence_num = header.sequence_num.wrapping_add(1);

    // Step 2: check space before writing anything.
    let check = header.check_free_space(desired, pending)?;
    // `deferred` is always false from the simplified check, but honor it: a
    // deferred header write means `start` was bumped inside the check, and the
    // caller should skip the header sync until the block data is written.
    // In the current port, that never happens.
    let _deferred = check.deferred;

    // Step 3: encode and write the transaction blocks.
    let (image, _end) = tx.encode(sequence_num, blhdr_size, check_blocks)?;
    // The journal is a ring: the block data may straddle the end.
    let journal_offset = info.offset;
    let start = header.end;
    let size = header.size;
    if start + image.len() as u64 <= size {
        // No wrap: a single contiguous write.
        device.write_at(journal_offset + start, &image)?;
    } else {
        // Wrap: write to the end, then from the beginning (past the header).
        let first_len = (size - start) as usize;
        device.write_at(journal_offset + start, &image[..first_len])?;
        device.write_at(
            journal_offset + header.jhdr_size as u64,
            &image[first_len..],
        )?;
    }

    // Step 4: pre-header barrier flush.
    device.sync()?;

    // Step 5: advance the header cursor in memory.
    header.end = (header.end + image.len() as u64) % header.size;
    header.sequence_num = sequence_num;

    // Step 6: write the header. `updating_start = false` because we are
    // advancing `end`, which is the case that needs the pre-write barrier
    // (not the post-write barrier that a `start` bump takes).
    write_journal_header(device, info, header)?;

    Ok(())
}

#[cfg(test)]
mod write_tests {
    use super::*;

    fn block(n: u8) -> RecordedWrite {
        RecordedWrite {
            bnum: u64::from(n) + 26,
            data: vec![n; 512],
        }
    }

    #[test]
    fn a_written_block_list_reads_back_as_itself() {
        // The point of this test is that it goes through `parse`, not through a
        // second copy of the encoder's own logic. An encoder checked against itself
        // proves nothing; this one is checked by the code that has to accept it.
        let blocks = [block(1), block(2), block(3)];
        let raw = encode_block_list(0x2a, BLHDR_FIRST_HEADER, 127, &blocks, true).expect("encode");

        let parsed = BlockListHeader::parse(&raw).expect("parse");
        assert_eq!(parsed.sequence_num, 0x2a);
        assert_eq!(parsed.num_blocks as usize, FIRST_BLOCK_INDEX + blocks.len());
        assert_eq!(parsed.flags, BLHDR_FIRST_HEADER);
        assert_eq!(
            parsed.blocks.len(),
            blocks.len(),
            "and every block survives -- binfo[0] is the sequence slot, not a block"
        );
        for (got, want) in parsed.blocks.iter().zip(blocks.iter()) {
            assert_eq!(got.bnum, want.bnum);
            assert_eq!(got.bsize as usize, want.data.len());
            assert_eq!(
                got.cksum,
                crate::journal::checksum::calc_checksum(&want.data),
                "a block recorded with checks_blocks set carries the checksum of \\
                 its contents"
            );
        }
        assert!(
            parsed.checksum_matches(&raw),
            "the header's own checksum must verify against the bytes written"
        );
    }

    #[test]
    fn without_block_checksums_the_slot_carries_the_sequence_number() {
        // The word at `binfo[i] + 12` is two things depending on a flag, which is
        // the sort of overlap that gets transcribed into the wrong field once. With
        // the flag clear it is the sequence slot's checksum, and replay reads it as
        // the sequence number -- so the writer must put the sequence number there,
        // not a block checksum.
        let blocks = [block(7)];
        let raw = encode_block_list(0x5b, 0, 127, &blocks, false).expect("encode");
        let parsed = BlockListHeader::parse(&raw).expect("parse");
        assert_eq!(parsed.blocks[0].cksum, 0x5b, "not a checksum of the data");
    }

    #[test]
    fn a_list_too_big_for_its_capacity_is_refused() {
        // The capacity is `max_blocks`, and the sequence slot counts against it.
        let blocks: Vec<RecordedWrite> = (0..4).map(block).collect();
        let err = encode_block_list(1, 0, 4, &blocks, true).expect_err("4 blocks + slot > 4");
        assert!(
            format!("{err}").contains("max_blocks"),
            "the refusal must name the capacity; got: {err}"
        );
    }

    #[test]
    fn a_killed_block_round_trips_as_the_sentinel() {
        // Freeing a block is recorded, not omitted: an absent record would be
        // indistinguishable from a block that never changed.
        let raw = encode_block_list(1, BLHDR_FIRST_HEADER, 127, &[RecordedWrite::killed()], true)
            .expect("encode");
        let parsed = BlockListHeader::parse(&raw).expect("parse");
        assert_eq!(parsed.blocks[0].bnum, 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(parsed.blocks[0].bsize, 0);
    }
}

/// One list as the assembler emits it: where it sits and how big it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedList {
    /// Byte offset of the list's header within the assembled transaction.
    pub offset: u64,
    /// Byte offset of the list's block data.
    pub data_offset: u64,
    /// The header, exactly as `BlockListHeader::parse` will read it.
    pub header: Vec<u8>,
    /// How many blocks the list describes.
    pub num_blocks: usize,
}

/// A transaction assembled for writing, and where it ends.
///
/// The inverse of the walk in [`Journal::replay`], which advances
/// `next = data_offset + bytes_used` and reads each list's header from a whole
/// `blhdr_size` block. Lists are packed **back to back with no rounding**, which is
/// the part that is not obvious and which the reader's arithmetic fixes exactly.
///
/// # Splitting
///
/// A block list describes at most [`MAX_BLOCKS_PER_LIST`] blocks, because it lives
/// in one journal block. A transaction with more blocks than that becomes several
/// lists, and only the first carries [`BLHDR_FIRST_HEADER`].
///
/// A killed block contributes a list entry but no data, so it costs an entry and
/// no bytes -- which is what makes "this block was freed" a thing the journal can
/// say.
///
/// Mining reference: `end_transaction`'s `for (blhdr = tr->blhdr; blhdr; blhdr = next)`
/// over the block lists, each followed by its own blocks.
pub fn encode_transaction(
    sequence_num: u32,
    blocks: &[RecordedWrite],
    blhdr_size: u32,
    check_blocks: bool,
) -> Result<(Vec<u8>, u32, Vec<EncodedList>)> {
    use crate::error::Error;

    if blhdr_size == 0 {
        return Err(Error::invalid(
            "blhdr_size",
            "a zero-sized block list cannot hold a header",
        ));
    }
    let blhdr_size = blhdr_size as usize;
    let mut out: Vec<u8> = Vec::new();
    let mut lists = Vec::new();

    // The capacity is set by the journal block the list lives in, not by a constant:
    // a list is one `blhdr_size` block, and its `binfo` array has to fit inside it.
    // `MAX_BLOCKS_PER_LIST` is Apple's ceiling on that array: Apple clamps the
    // capacity rather than rejecting, so a 4096-byte block (which would hold 255
    // entries) is capped at 127 the same way macOS clamps it.
    let capacity =
        (blhdr_size.saturating_sub(BLHDR_PREFIX_SIZE) / BLOCK_INFO_SIZE).min(MAX_BLOCKS_PER_LIST);
    if capacity <= FIRST_BLOCK_INDEX {
        return Err(Error::invalid(
            "blhdr_size",
            format!("{blhdr_size} bytes cannot hold a block list header and a block"),
        ));
    }
    // Entries, not blocks: the sequence slot takes one of them.
    let per_list = capacity - FIRST_BLOCK_INDEX;
    debug_assert!(per_list > 0);
    for (i, chunk) in blocks.chunks(per_list).enumerate() {
        let flags = if i == 0 { BLHDR_FIRST_HEADER } else { 0 };
        let header = encode_block_list(sequence_num, flags, capacity as u16, chunk, check_blocks)?;
        if header.len() > blhdr_size {
            return Err(Error::invalid(
                "blhdr_size",
                format!(
                    "{} bytes of block list header does not fit in a {blhdr_size}-byte \\
                     journal block",
                    header.len()
                ),
            ));
        }
        let offset = out.len();
        // The header occupies a whole journal block; the data starts after it.
        out.resize(offset + blhdr_size, 0);
        out[offset..offset + header.len()].copy_from_slice(&header);
        let data_offset = out.len();
        let mut used = 0usize;
        for b in chunk {
            if b.bnum == KILLED_BNUM {
                continue; // recorded, not written
            }
            out.extend_from_slice(&b.data);
            used += b.data.len();
        }
        lists.push(EncodedList {
            offset: offset as u64,
            data_offset: data_offset as u64,
            header,
            num_blocks: chunk.len(),
        });
        debug_assert_eq!(used, out.len() - data_offset);
    }

    if out.len() > u32::MAX as usize {
        return Err(Error::overflow("transaction length"));
    }
    let end = u32::try_from(out.len()).map_err(|_| Error::overflow("transaction length"))?;
    Ok((out, end, lists))
}

/// In-memory buffer for one journal transaction.
///
/// This is the userspace port of Apple's `block_list_header_in_memory`: a
/// block-list header paired with a buffer that holds each dirty block's
/// *before-image*. When a block is dirtied it is **copied into this buffer**
/// before the home block is modified, so the journal always carries the
/// original contents for replay. At commit time the assembled buffer is written
/// to the journal in [`encode_transaction`], and only then -- never before.
///
/// Mining reference: `core/hfs_journal.h` `block_list_header_in_memory` and
/// `core/hfs_journal.c` `journal_modify_block_end` copies the block buffer
/// into `blhdrim->buffers[tbuffer_offset]` before the caller mutates the home
/// block.
#[derive(Debug, Default)]
pub struct TransactionBuffer {
    /// The dirty blocks accumulated so far, each carrying its new contents.
    ///
    /// The order is the order they will be written to the journal, which is
    /// why the `total_bytes` advance is reproducible.
    writes: Vec<RecordedWrite>,
}

impl TransactionBuffer {
    /// Start a new, empty transaction.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many bytes this transaction's encoding will occupy in the journal.
    ///
    /// Each block list contributes its `blhdr_size` plus the sum of its blocks'
    /// data sizes, and a list starts with `num_blocks = 1` (the sequence slot).
    /// This is `tr->total_bytes` in Apple's `end_transaction`, which
    /// `check_free_space` consults before writing anything.
    ///
    /// The value is computed over the current `writes` so the caller can size
    /// the check before committing, and is exactly what
    /// [`JournalHeader::commit_transaction`] consumes.
    pub fn total_bytes(&self, blhdr_size: u32) -> u64 {
        let blhdr_size = u64::from(blhdr_size);
        let capacity = (blhdr_size as usize)
            .saturating_sub(BLHDR_PREFIX_SIZE)
            .saturating_sub(BLOCK_INFO_SIZE) // reserve the sequence slot
            / BLOCK_INFO_SIZE;
        if capacity == 0 {
            return 0;
        }
        let n = self.writes.len() as u64;
        let lists = if n == 0 {
            0
        } else {
            (n - 1) / capacity as u64 + 1
        };
        let data: u64 = self.writes.iter().map(|w| w.data.len() as u64).sum();
        lists * blhdr_size + data
    }

    /// Record a block that was copied into the transaction before mutation.
    ///
    /// `bnum` is the block number in the journal's `jhdr_size` units, which on
    /// every volume Apple writes today is the same as the volume block number.
    /// `data` is the *before-image* of the block: the contents the journal will
    /// replay if the system crashes before the home block is updated.
    pub fn record_write(&mut self, bnum: u64, data: Vec<u8>) {
        self.writes.push(RecordedWrite { bnum, data });
    }

    /// Record that a block was freed during the transaction.
    ///
    /// This is `journal_kill_block`'s path: the block's `bnum` is set to
    /// [`RecordedWrite::killed`] so the write loop skips it, but the list
    /// entry remains so replay knows the block was released.
    pub fn record_kill(&mut self, bnum: u64) {
        self.writes.push(RecordedWrite {
            bnum,
            // A killed block carries no data; the encoder writes the sentinel
            // `bnum` and empty data.
            data: Vec::new(),
        });
        // Overwrite bnum with the sentinel so replay skips it.
        self.writes.last_mut().unwrap().bnum = KILLED_BNUM;
    }

    /// The number of block entries accumulated.
    pub fn len(&self) -> usize {
        self.writes.len()
    }

    /// Whether no blocks have been recorded.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Encode the accumulated blocks as a journal transaction.
    ///
    /// Returns the full byte image (block lists back-to-back, no padding, each
    /// header occupying a whole `blhdr_size` block) and the offset past it that
    /// `jhdr->end` should advance to. `sequence_num` is the transaction's
    /// sequence number, assigned at `start_transaction` time.
    pub fn encode(
        &self,
        sequence_num: u32,
        blhdr_size: u32,
        check_blocks: bool,
    ) -> Result<(Vec<u8>, u32)> {
        let (image, end, _lists) =
            encode_transaction(sequence_num, &self.writes, blhdr_size, check_blocks)?;
        Ok((image, end))
    }

    /// The accumulated writes, as a slice.
    pub fn writes(&self) -> &[RecordedWrite] {
        &self.writes
    }
}

/// `bnum` meaning "this block was released", as `end_transaction` writes it and
/// replay skips it.
pub const KILLED_BNUM: u64 = 0xFFFF_FFFF_FFFF_FFFF;

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use crate::journal::info::{ByteOrder, ENDIAN_MAGIC, K_JI_JOURNAL_IN_FS_MASK};

    fn block(n: u8) -> RecordedWrite {
        RecordedWrite {
            bnum: u64::from(n) + 26,
            data: vec![n; 256],
        }
    }

    /// Re-walk an assembled transaction exactly as `Journal::replay` does: take a
    /// `blhdr_size` block, read the header at the cursor, put the data after it, and
    /// advance by `data_offset + bytes_used`. If the assembler's layout and the
    /// reader's arithmetic ever disagree, this is where it shows.
    fn walk(image: &[u8], blhdr_size: usize, first: u64) -> Vec<(u64, u64, usize)> {
        let mut out = Vec::new();
        let mut offset = first;
        while offset as usize + blhdr_size <= image.len() {
            let bytes = &image[offset as usize..offset as usize + blhdr_size];
            let data_offset = offset + blhdr_size as u64;
            let blhdr = BlockListHeader::parse_at(bytes, data_offset).expect("parse");
            assert!(blhdr.checksum_matches(bytes), "header checksum");
            out.push((offset, data_offset, blhdr.blocks.len()));
            let next = data_offset + u64::from(blhdr.bytes_used);
            assert!(next > offset, "a list must advance");
            offset = next;
        }
        out
    }

    #[test]
    fn an_assembled_transaction_is_walkable_by_the_readers_own_arithmetic() {
        let blocks: Vec<RecordedWrite> = (0..5).map(block).collect();
        let (image, end, lists) = encode_transaction(0x11, &blocks, 512, true).expect("encode");
        assert_eq!(lists.len(), 1, "five blocks fit one list");
        let found = walk(&image, 512, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 0);
        assert_eq!(found[0].2, blocks.len());
        assert_eq!(u64::from(end), image.len() as u64);
    }

    #[test]
    fn more_blocks_than_one_list_holds_become_several_lists() {
        // One list is bounded by the journal block it lives in, so a transaction
        // with more blocks is several lists, packed back to back, and only the
        // first is the first.
        // A 512-byte journal block holds 31 entries -- 16 of prefix and 16 per
        // entry -- so 30 blocks per list, and the sequence slot is the 31st.
        let per_list = (512 - BLHDR_PREFIX_SIZE) / BLOCK_INFO_SIZE - FIRST_BLOCK_INDEX;
        assert_eq!(per_list, 30, "the capacity comes from the journal block");
        let blocks: Vec<RecordedWrite> = (0..(per_list + 3))
            .map(|i| RecordedWrite {
                bnum: i as u64 + 26,
                data: vec![i as u8; 64],
            })
            .collect();
        let (image, _end, lists) = encode_transaction(0x22, &blocks, 512, true).expect("encode");
        assert_eq!(lists.len(), 2, "one full list and the remainder");
        assert_eq!(lists[0].num_blocks, per_list);
        assert_eq!(lists[1].num_blocks, 3);

        let found = walk(&image, 512, 0);
        assert_eq!(found.len(), 2, "and the reader's walk finds both");
        assert_eq!(
            found[1].0,
            found[0].0 + 512 + (per_list * 64) as u64,
            "the second list follows the first's data with no rounding"
        );
    }

    #[test]
    fn a_killed_block_costs_a_list_entry_and_no_bytes() {
        let mut blocks = vec![block(1), block(2)];
        blocks.push(RecordedWrite::killed());
        let (image, _end, lists) = encode_transaction(0x33, &blocks, 512, true).expect("encode");
        let found = walk(&image, 512, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(lists[0].num_blocks, 3, "three entries");

        // And the data is two blocks, not three: the sentinel contributes nothing.
        let blhdr = BlockListHeader::parse_at(&image[..512], 512).expect("parse");
        assert_eq!(blhdr.blocks[2].bnum, KILLED_BNUM);
        assert_eq!(blhdr.blocks[2].bsize, 0);
        assert_eq!(u64::from(blhdr.bytes_used), 512, "two 256-byte blocks");
    }

    #[test]
    fn a_header_too_large_for_its_journal_block_is_refused() {
        // The capacity and the journal block size have to agree; a header that will
        // not fit is a configuration error, not something to truncate.
        let blocks: Vec<RecordedWrite> = (0..3).map(block).collect();
        let err =
            encode_transaction(1, &blocks, 8, true).expect_err("8 bytes cannot hold a header");
        assert!(
            format!("{err}").contains("blhdr_size"),
            "the refusal must name the field that is wrong; got: {err}"
        );
    }

    #[test]
    fn transaction_buffer_encodes_to_the_same_image_as_encode_transaction() {
        // The buffer is a convenience wrapper around encode_transaction, and it
        // must produce identical bytes -- a divergence here would mean the
        // committer and the round-trip test disagree on layout.
        let mut buf = TransactionBuffer::new();
        for n in 0..5u8 {
            buf.record_write(u64::from(n) + 26, vec![n; 256]);
        }
        let (image, end) = buf.encode(0x11, 512, true).expect("encode");
        let (direct, end_direct, _lists) =
            encode_transaction(0x11, buf.writes(), 512, true).expect("encode");
        assert_eq!(image, direct);
        assert_eq!(end, end_direct);
    }

    #[test]
    fn transaction_buffer_total_bytes_matches_the_encoded_length() {
        // total_bytes must equal the image length, because commit_transaction
        // uses it to advance `end` and `end` must land exactly on the bytes
        // written.
        let mut buf = TransactionBuffer::new();
        for n in 0..5u8 {
            buf.record_write(u64::from(n) + 26, vec![n; 256]);
        }
        let (image, _end) = buf.encode(0x11, 512, true).expect("encode");
        let total = buf.total_bytes(512);
        assert_eq!(total, image.len() as u64);
    }

    #[test]
    fn total_bytes_accounts_for_multi_list_splitting() {
        // More blocks than one list holds must cost more than one header.
        let per_list = (512 - BLHDR_PREFIX_SIZE) / BLOCK_INFO_SIZE - FIRST_BLOCK_INDEX;
        let mut buf = TransactionBuffer::new();
        for i in 0..(per_list + 3) {
            buf.record_write(i as u64 + 26, vec![i as u8; 64]);
        }
        let (image, _end) = buf.encode(0x22, 512, true).expect("encode");
        assert_eq!(buf.total_bytes(512), image.len() as u64);
    }

    #[test]
    fn a_killed_block_in_the_buffer_is_encoded_as_sentinel() {
        let mut buf = TransactionBuffer::new();
        buf.record_write(26, vec![1u8; 256]);
        buf.record_kill(27);
        let (image, _end) = buf.encode(0x33, 512, true).expect("encode");
        let blhdr = BlockListHeader::parse(&image[..512]).expect("parse");
        assert_eq!(blhdr.blocks[0].bnum, 26);
        assert_eq!(blhdr.blocks[1].bnum, KILLED_BNUM);
        assert_eq!(blhdr.blocks[1].bsize, 0);
    }

    #[test]
    fn an_empty_buffer_encodes_to_zero_bytes() {
        let buf = TransactionBuffer::new();
        assert_eq!(
            buf.total_bytes(512),
            0,
            "no blocks means no header and no data"
        );
        // An empty transaction encodes to no bytes -- `encode_transaction`
        // iterates chunks of the block list and an empty slice yields none.
        let (image, _end) = buf.encode(0x44, 512, true).expect("encode empty");
        assert!(image.is_empty(), "nothing was accumulated");
    }

    /// A writable mock device for testing the commit path.
    struct MemDevice {
        data: Vec<u8>,
    }

    impl MemDevice {
        fn new(size: usize) -> Self {
            Self {
                data: vec![0u8; size],
            }
        }
    }

    impl BlockDevice for MemDevice {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.data.len() {
                return Err(Error::out_of_range(
                    "read",
                    end as u64,
                    self.data.len() as u64,
                ));
            }
            buf.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn len(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }
    }

    impl BlockDeviceMut for MemDevice {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.data.len() {
                return Err(Error::out_of_range(
                    "write",
                    end as u64,
                    self.data.len() as u64,
                ));
            }
            self.data[start..end].copy_from_slice(buf);
            Ok(())
        }

        fn sync(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn commit_transaction_writes_blocks_then_header_advancing_end() {
        // The full commit path: encode, write to the journal ring, advance end,
        // write the header. The reader must then find the data it wrote.
        let journal_size: u64 = 4096;
        let blhdr_size: u32 = 512;
        let mut dev = MemDevice::new(journal_size as usize);

        // A journal that is empty: start == end == jhdr_size (past the header).
        let mut header = JournalHeader {
            magic: JOURNAL_HEADER_MAGIC,
            endian: ENDIAN_MAGIC,
            byte_order: ByteOrder::Big,
            start: blhdr_size as u64,
            end: blhdr_size as u64,
            size: journal_size,
            blhdr_size,
            checksum: 0,
            jhdr_size: blhdr_size,
            sequence_num: 0,
        };
        let info = JournalInfoBlock {
            flags: K_JI_JOURNAL_IN_FS_MASK,
            device_signature: [0; 8],
            offset: 0,
            size: journal_size,
        };

        let mut tx = TransactionBuffer::new();
        tx.record_write(100, vec![0xABu8; 256]);

        commit_transaction(&mut dev, &info, &mut header, &tx, blhdr_size, true, 0).expect("commit");

        // end advanced past the transaction.
        let expected_end = blhdr_size as u64 + tx.total_bytes(blhdr_size);
        assert_eq!(header.end, expected_end);
        assert_eq!(header.sequence_num, 1);

        // The header at offset 0 must now reflect the new end.
        let mut hdr_bytes = [0u8; 1024];
        dev.read_at(0, &mut hdr_bytes).expect("read header");
        let parsed = JournalHeader::parse(&hdr_bytes)
            .expect("parse")
            .expect("a header");
        assert_eq!(parsed.end, expected_end);
        assert_eq!(parsed.sequence_num, 1);
        assert!(parsed.checksum_matches(&hdr_bytes));
    }

    #[test]
    fn commit_wraps_the_transaction_across_the_journal_end() {
        // A transaction that starts near the end wraps to the beginning, past
        // the header block. The reader must still find both halves.
        let journal_size: u64 = 8192;
        let blhdr_size: u32 = 512;
        let mut dev = MemDevice::new(journal_size as usize);

        // start is set past jhdr_size so the wrap region [jhdr_size, start)
        // has room for the wrapped data.
        let mut header = JournalHeader {
            magic: JOURNAL_HEADER_MAGIC,
            endian: ENDIAN_MAGIC,
            byte_order: ByteOrder::Big,
            start: 2048,
            // end is near the end so the transaction wraps.
            end: journal_size - 256,
            size: journal_size,
            blhdr_size,
            checksum: 0,
            jhdr_size: blhdr_size,
            sequence_num: 5,
        };
        let info = JournalInfoBlock {
            flags: K_JI_JOURNAL_IN_FS_MASK,
            device_signature: [0; 8],
            offset: 0,
            size: journal_size,
        };

        let mut tx = TransactionBuffer::new();
        // total_bytes = blhdr_size (header) + 256 (data) = 768, which wraps
        // past the end of the 8192-byte journal.
        tx.record_write(100, vec![0xCDu8; 256]);

        commit_transaction(&mut dev, &info, &mut header, &tx, blhdr_size, true, 0).expect("commit");

        // end wrapped: (7936 + 768) % 8192 = 512
        assert_eq!(header.end, 512, "wrapped past the end of the journal");
    }

    #[test]
    fn commit_refuses_when_a_transaction_is_pending() {
        let journal_size: u64 = 4096;
        let blhdr_size: u32 = 512;
        let mut dev = MemDevice::new(journal_size as usize);

        let mut header = JournalHeader {
            magic: JOURNAL_HEADER_MAGIC,
            endian: ENDIAN_MAGIC,
            byte_order: ByteOrder::Big,
            start: blhdr_size as u64,
            end: blhdr_size as u64,
            size: journal_size,
            blhdr_size,
            checksum: 0,
            jhdr_size: blhdr_size,
            sequence_num: 0,
        };
        let info = JournalInfoBlock {
            flags: K_JI_JOURNAL_IN_FS_MASK,
            device_signature: [0; 8],
            offset: 0,
            size: journal_size,
        };

        let mut tx = TransactionBuffer::new();
        tx.record_write(100, vec![0xABu8; 256]);

        // Pending = 1 must refuse, even though there's free space.
        let err = commit_transaction(&mut dev, &info, &mut header, &tx, blhdr_size, true, 1)
            .expect_err("pending blocks the write");
        assert!(
            format!("{err}").contains("replayed"),
            "the refusal must say what to do; got: {err}"
        );
    }

    #[test]
    fn commit_refuses_when_the_journal_is_too_small() {
        let journal_size: u64 = 1024;
        let blhdr_size: u32 = 512;
        let mut dev = MemDevice::new(journal_size as usize);

        let mut header = JournalHeader {
            magic: JOURNAL_HEADER_MAGIC,
            endian: ENDIAN_MAGIC,
            byte_order: ByteOrder::Big,
            start: blhdr_size as u64,
            end: blhdr_size as u64,
            size: journal_size,
            blhdr_size,
            checksum: 0,
            jhdr_size: blhdr_size,
            sequence_num: 0,
        };
        let info = JournalInfoBlock {
            flags: K_JI_JOURNAL_IN_FS_MASK,
            device_signature: [0; 8],
            offset: 0,
            size: journal_size,
        };

        let mut tx = TransactionBuffer::new();
        // Fill with too many blocks.
        for i in 0..200u8 {
            tx.record_write(u64::from(i) + 26, vec![i; 64]);
        }

        let err = commit_transaction(&mut dev, &info, &mut header, &tx, blhdr_size, true, 0)
            .expect_err("too big");
        assert!(matches!(err, Error::NoSpace { .. }), "got {err:?}");
    }
}
