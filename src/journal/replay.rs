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
    /// Whether the journal header's checksum matched, or `None` when it was not
    /// checked.
    checksum_ok: Option<bool>,
    /// The volume's allocation block size, which `bnum` is measured in.
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
                self.truncate_at(offset, "block list counts are inconsistent");
                break;
            }
            if blhdr.bytes_used == 0 {
                if past_end {
                    break;
                }
                self.truncate_at(offset, "block list carries no data");
                break;
            }
            if !blhdr.checksum_matches(&bytes) {
                // Apple truncates here rather than abandoning the replay.
                if past_end {
                    break;
                }
                self.truncate_at(offset, "block list header checksum mismatch");
                break;
            }
            let used = u64::from(blhdr.bytes_used);
            let runs_past_end = data_offset
                .checked_add(used)
                .map(|end| end > header.size)
                .unwrap_or(true);
            if runs_past_end {
                self.truncate_at(offset, "block list data runs past the journal end");
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
                self.truncate_at(
                    offset,
                    "block list sequence number is out of order",
                );
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
                self.truncate_at(offset, "block list claims more blocks than the journal holds");
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

            let next = data_offset.checked_add(used).ok_or(Error::overflow("block list cursor"))?;
            if next <= offset {
                self.truncate_at(offset, "block list does not advance");
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
    fn truncate_at(&mut self, offset: u64, reason: &str) {
        self.truncated_at = Some(offset);
        self.truncation_reason = Some(reason.to_string());
        if let Some(last) = self.transactions.last_mut() {
            last.end = offset.min(last.end);
        }
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
    fn build_overlay(&mut self) -> Result<()> {
        for index in 0..self.transactions.len() {
            let transaction = self.transactions[index].clone();
            match self.apply_transaction(&transaction) {
                Ok(()) => {}
                Err(ApplyFailure::Truncate { at, reason }) => {
                    self.truncate_at(at, &reason);
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
                let data = self.read_journal(start, size).map_err(ApplyFailure::Fatal)?;
                data_cursor += size as u64;

                // A zero recorded checksum means "do not verify", which Apple
                // checks for explicitly before comparing.
                if list.checks_blocks()
                    && block.cksum != 0
                    && calc_checksum(&data) != block.cksum
                {
                    return Err(ApplyFailure::Truncate {
                        at: transaction.offset,
                        reason: format!(
                            "block {} failed its recorded checksum",
                            block.bnum
                        ),
                    });
                }

                let device_offset = block
                    .bnum
                    .checked_mul(u64::from(self.block_size))
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
        OverlaidDevice { inner: self.device, overlay: &self.overlay }
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
                b.slice_from(at, buf.len() - written).map(|slice| (b, slice))
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
        let cksum = super::super::checksum::checksum_with_zeroed_field(&raw, 8, BLHDR_CHECKSUM_SIZE)
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
                    Err(Error::InvalidField { field: "block_list_header.num_blocks", .. })
                ),
                "num_blocks {n} should be rejected"
            );
        }
    }

    #[test]
    fn a_killed_block_is_recognised() {
        let killed = RecordedBlock { bnum: END_BLK_NUM, bsize: 4096, cksum: 0 };
        assert!(killed.is_killed());
        let normal = RecordedBlock { bnum: 200, bsize: 4096, cksum: 0 };
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
        assert!(!h.checksum_matches(&corrupt), "a corrupted field must fail the checksum");
    }
}