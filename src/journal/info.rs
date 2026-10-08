// SPDX-License-Identifier: APSL-1.2

//! The journal info block and the journal header block.
//!
//! Mining reference: Apple `core/hfs_format.h` (`struct JournalInfoBlock`) and
//! `core/hfs_journal.h` (`struct journal_header`).
//!
//! # The journal header is not big-endian
//!
//! Every other structure in HFS+ is big-endian. The journal header is **not**: it
//! is written in the native byte order of whichever machine wrote it, and the
//! `endian` field records that so a reader can tell. Apple detects it by trying
//! the magic both ways and comparing against the `endian` sentinel.
//!
//! Mining reference: `core/hfs_journal.c` `journal_open` reads the magic as
//! big-endian first and falls back to swapping, and `ENDIAN_MAGIC` is defined
//! alongside `JOURNAL_HEADER_MAGIC` in `core/hfs_journal.h`.
//!
//! This matters in practice: `mkfs.hfsplus` on x86 writes a little-endian journal
//! header inside a big-endian filesystem, and a reader that assumes big-endian
//! finds a magic of zero.
//!
//! # An uninitialized journal
//!
//! `kJIJournalNeedInitMask` means the journal exists but no transaction has ever
//! been written, so the journal header area is all zeros. Every image
//! `mkfs.hfsplus -J` produces is in exactly this state. Reading a header there
//! must yield "nothing to replay", not a corrupt-journal error.

use super::checksum::{calc_checksum, JOURNAL_HEADER_CKSUM_SIZE};
use crate::endian::Be;
use crate::error::{Error, Result};

/// `JOURNAL_HEADER_MAGIC` = `'JNLx'`.
pub const JOURNAL_HEADER_MAGIC: u32 = 0x4a4e_4c78;

/// `OLD_JOURNAL_HEADER_MAGIC` = `'JHDR'`.
///
/// Mining reference: `core/hfs_journal.h`. Apple still accepts it and converts it
/// on write, so a read-only reader must recognise it too.
pub const OLD_JOURNAL_HEADER_MAGIC: u32 = 0x4a48_4452;

/// `ENDIAN_MAGIC`, the sentinel in the journal header's `endian` field.
pub const ENDIAN_MAGIC: u32 = 0x1234_5678;

/// `kJIJournalInFSMask`: the journal lives inside the filesystem.
pub const K_JI_JOURNAL_IN_FS_MASK: u32 = 0x0000_0001;

/// `kJIJournalOnOtherDeviceMask`: the journal lives on a separate device.
pub const K_JI_JOURNAL_ON_OTHER_DEVICE_MASK: u32 = 0x0000_0002;

/// `kJIJournalNeedInitMask`: the journal exists but has no transactions.
pub const K_JI_JOURNAL_NEED_INIT_MASK: u32 = 0x0000_0004;

/// `END_BLK_NUM`: the block number marking the end of a block list.
///
/// Mining reference: `core/hfs_journal.c` uses it as a sentinel in the block
/// list, matching `0xFFFFFFFF` in `block_info.bnum`.
pub const END_BLK_NUM: u64 = 0xFFFF_FFFF;

/// Byte size of `struct JournalInfoBlock`.
///
/// Mining reference: `core/hfs_format.h` computes `JIB_RESERVED_SIZE` as
/// `(32 * sizeof(u_int32_t)) - sizeof(uuid_string_t) - 48`, so that adding
/// `ext_jnl_uuid` and `machine_serial_num` did not change the struct's size. With
/// a 16-byte `uuid_string_t` that is 128 - 16 - 48 = 64, and the fields are:
///
/// ```text
/// u_int32_t  flags;                    //   0
/// u_int32_t  device_signature[8];      //   4
/// u_int64_t  offset;                   //  36
/// u_int64_t  size;                     //  44
/// uuid_string_t ext_jnl_uuid;          //  52, 16 bytes
/// char       machine_serial_num[48];   //  68
/// char       reserved[64];             // 116
/// ```
///
/// The whole struct occupies one allocation block; this is the length of the
/// declared fields, not of the block.
///
/// What follows the struct inside that block is **not** zero, and reading it as
/// if it were would be wrong in either direction. `newfs_hfs`'s `makehfs.c`
/// memsets each sector to `0xdb` before writing anything into it:
///
/// ```c
/// memset(buffer, 0xdb, driveInfo->physSectorSize);
/// ```
///
/// So on a volume from `mkfs_hfsplus -J` the block is 180 bytes of struct, then
/// `0xdb` drive filler to the next 512-byte boundary, then zeros. Verified on
/// `journaled-hfsplus`: bytes 180..512 are `0xdb`, and everything from 512 to the
/// end of the 4096-byte block is zero. A reader that zero-fills the filler is
/// fabricating bytes, and one that treats the filler as corruption is refusing a
/// valid block.
pub const JOURNAL_INFO_BLOCK_SIZE: usize = 4 + 32 + 8 + 8 + 16 + 48 + 64;

/// Byte offset of the journal's byte offset within the info block.
pub const JIB_OFFSET_OFFSET: usize = 4 + 32;

/// Byte offset of the journal's size within the info block.
pub const JIB_SIZE_OFFSET: usize = JIB_OFFSET_OFFSET + 8;

/// Decoded journal flags from the info block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalFlags(pub u32);

impl JournalFlags {
    /// The journal lives inside the filesystem rather than on another device.
    pub fn in_filesystem(self) -> bool {
        self.0 & K_JI_JOURNAL_IN_FS_MASK != 0
    }

    /// The journal lives on a separate device, named by `ext_jnl_uuid`.
    ///
    /// Mining reference: `EXTJNL_CONTENT_TYPE_UUID` in `core/hfs_format.h` is the
    /// GPT partition type for such a device.
    pub fn on_other_device(self) -> bool {
        self.0 & K_JI_JOURNAL_ON_OTHER_DEVICE_MASK != 0
    }

    /// The journal exists but has never been written to.
    ///
    /// Every volume created by `mkfs_hfsplus -J` has this set, and its journal
    /// header area is zeroed. A read-only mount must treat that as "nothing to
    /// replay" rather than as corruption.
    pub fn needs_init(self) -> bool {
        self.0 & K_JI_JOURNAL_NEED_INIT_MASK != 0
    }
}

/// The `JournalInfoBlock`, which locates the journal on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfoBlock {
    /// Location flags; see [`JournalFlags`].
    pub flags: u32,
    /// Device signature used to locate the device. Opaque to a userspace reader.
    pub device_signature: [u32; 8],
    /// Byte offset of the journal on the device.
    pub offset: u64,
    /// Size of the journal in bytes.
    pub size: u64,
}

impl JournalInfoBlock {
    /// Parse the leading fields of a `JournalInfoBlock`.
    ///
    /// Only the fields a reader needs are decoded; the UUID, serial number and
    /// reserved tail are skipped, because a userspace reader has no use for them
    /// and refusing to parse would make the block useless on a volume written by
    /// a newer format version.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < JOURNAL_INFO_BLOCK_SIZE {
            return Err(Error::Truncated {
                what: "journal info block",
                needed: JOURNAL_INFO_BLOCK_SIZE,
                available: bytes.len(),
            });
        }
        let be = Be::new(bytes);
        let mut device_signature = [0u32; 8];
        for (i, slot) in device_signature.iter_mut().enumerate() {
            *slot = be.u32(4 + i * 4)?;
        }
        Ok(JournalInfoBlock {
            flags: be.u32(0)?,
            device_signature,
            offset: be.u64(JIB_OFFSET_OFFSET)?,
            size: be.u64(JIB_SIZE_OFFSET)?,
        })
    }

    /// Decoded flags.
    pub fn flag_set(&self) -> JournalFlags {
        JournalFlags(self.flags)
    }

    /// Validate that the journal lies inside `device_bytes`.
    ///
    /// Mining reference: `core/hfs_journal.c` `journal_open` checks the journal's
    /// offset and size against the device before using them. An unvalidated
    /// offset would let a corrupt info block point a reader anywhere, including
    /// outside the image.
    /// Check this block against the size of the device it describes.
    ///
    /// The two journal locations need different things, and conflating them is a
    /// mistake that refuses a valid volume:
    ///
    /// - **Inside the filesystem**: `offset` is the journal's byte offset *in this
    ///   volume* and `size` its length, so both must be non-zero and the extent
    ///   must lie within the device.
    /// - **On another device**: `offset` is not read at all. The journal is named
    ///   by `ext_jnl_uuid`, and `size` is the length used to match it to the
    ///   partition. A volume in this state legitimately has `offset == 0`, so
    ///   requiring it to be non-zero refuses a correct volume with a confusing
    ///   error about a field that means nothing here.
    ///
    /// Mining reference: `core/hfs_vfsutils.c` reads `jibp->offset` only inside
    /// `if (jib_flags & kJIJournalInFSMask)`, and passes `jib_size` to
    /// `open_journal_dev` on the other path. When that device cannot be opened
    /// Apple fails the mount with `EROFS` -- the volume becomes read-only rather
    /// than becoming unopenable.
    pub fn validate(&self, device_bytes: u64) -> Result<()> {
        if self.size == 0 {
            return Err(Error::invalid(
                "JournalInfoBlock",
                format!("size {} must be non-zero", self.size),
            ));
        }

        let flags = self.flag_set();
        if !flags.in_filesystem() {
            // The journal is elsewhere; `offset` says nothing about this device.
            return Ok(());
        }

        if self.offset == 0 {
            return Err(Error::invalid(
                "JournalInfoBlock",
                format!(
                    "offset 0 is not valid for a journal in the filesystem at {}",
                    self.offset
                ),
            ));
        }
        let end = self
            .offset
            .checked_add(self.size)
            .ok_or(Error::overflow("journal extent"))?;
        if end > device_bytes {
            return Err(Error::out_of_range("journal extent", end, device_bytes));
        }
        Ok(())
    }
}

/// Which byte order a journal header was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// Big-endian.
    Big,
    /// Little-endian.
    Little,
}

/// The journal header, stored at byte zero of the journal.
///
/// Mining reference: `core/hfs_journal.h` `struct journal_header`:
///
/// ```c
/// typedef struct journal_header {
///     int32_t  magic;
///     int32_t  endian;
///     off_t    start;      // zero-based byte offset of the start of the first transaction
///     off_t    end;        // zero-based byte offset of where free space begins
///     off_t    size;       // size in bytes of the entire journal
///     int32_t  blhdr_size; // size in bytes of each block_list_header in the journal
///     uint32_t checksum;
///     int32_t  jhdr_size;  // block size (in bytes) of the journal header
///     uint32_t sequence_num;
/// } journal_header;
/// ```
///
/// Field offsets are computed for a 64-bit `off_t`, which is the on-disk layout
/// Apple writes on 64-bit builds. `off_t` is 4 bytes on a 32-bit build, so a
/// 32-bit-written header would misparse; Apple handles that with the endian
/// field, but a userspace reader that supports only one layout must say so
/// rather than silently misreading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalHeader {
    /// `JOURNAL_HEADER_MAGIC` or `OLD_JOURNAL_HEADER_MAGIC`.
    ///
    /// This is the *canonical* value, not the bytes as stored. Use
    /// [`Self::byte_order`] to know how to re-encode it.
    pub magic: u32,
    /// `ENDIAN_MAGIC`, in the header's own byte order.
    ///
    /// Normalised to the sentinel value regardless of which order the
    /// header was written in. Use [`Self::byte_order`] to recover the order.
    pub endian: u32,
    /// Byte offset of the first transaction, within the journal.
    pub start: u64,
    /// Byte offset where free space begins, within the journal.
    pub end: u64,
    /// Total journal size in bytes.
    pub size: u64,
    /// Size of each block-list header in the journal.
    pub blhdr_size: u32,
    /// Checksum over the first [`JOURNAL_HEADER_CKSUM_SIZE`] bytes.
    pub checksum: u32,
    /// Block size of the journal header itself.
    pub jhdr_size: u32,
    /// Monotonically increasing value assigned to each transaction.
    pub sequence_num: u32,
    /// The byte order the header was read in, used to re-encode for writing.
    ///
    /// This is stored explicitly because [`Self::parse`] normalises `magic` and
    /// `endian` to their canonical values, which is right for *comparisons*
    /// but loses the information `to_bytes` needs to pick the field encoding.
    pub byte_order: ByteOrder,
}

impl JournalHeader {
    /// Byte offsets within the header, for a 64-bit `off_t`.
    pub const MAGIC_OFFSET: usize = 0;
    pub const ENDIAN_OFFSET: usize = 4;
    pub const START_OFFSET: usize = 8;
    pub const END_OFFSET: usize = 16;
    pub const SIZE_OFFSET: usize = 24;
    pub const BLHDR_SIZE_OFFSET: usize = 32;
    pub const CHECKSUM_OFFSET: usize = 36;
    pub const JHDR_SIZE_OFFSET: usize = 40;
    pub const SEQUENCE_OFFSET: usize = 44;
    /// Byte size of the on-disk header.
    pub const SIZE: usize = 48;

    /// Parse a journal header, detecting its byte order.
    ///
    /// Returns `Ok(None)` when the magic is neither current nor old, which is
    /// what an uninitialized journal looks like. Distinguishing that from an
    /// error matters: every volume created by `mkfs_hfsplus -J` has an all-zero
    /// journal header and must mount cleanly.
    ///
    /// Mining reference: `core/hfs_journal.c` `journal_open` tries
    /// `SWAP32(JOURNAL_HEADER_MAGIC)` first and then the unswapped value, and
    /// reports a bad magic only after both fail.
    pub fn parse(bytes: &[u8]) -> Result<Option<Self>> {
        if bytes.len() < Self::SIZE {
            return Err(Error::Truncated {
                what: "journal header",
                needed: Self::SIZE,
                available: bytes.len(),
            });
        }
        let be = Be::new(bytes);
        // The magic is a byte pattern, so it is recognised in both orders: a
        // little-endian header holds the reversed bytes. Mining reference:
        // Apple tests `jhdr->magic == SWAP32(JOURNAL_HEADER_MAGIC)` before the
        // unswapped comparison, which is exactly this.
        let magic_be = be.u32(Self::MAGIC_OFFSET)?;
        let magic_pattern = |v: u32| {
            v == JOURNAL_HEADER_MAGIC
                || v == OLD_JOURNAL_HEADER_MAGIC
                || v.swap_bytes() == JOURNAL_HEADER_MAGIC
                || v.swap_bytes() == OLD_JOURNAL_HEADER_MAGIC
        };
        if !magic_pattern(magic_be) {
            return Ok(None);
        }

        // The `endian` sentinel is stored in the same order as the magic, so it
        // both confirms and selects the order.
        // The magic matched as a byte pattern, which is order-independent. The
        // endian sentinel says how the *numeric* fields must be read, and it is
        // itself stored in that order, so a little-endian header holds the bytes
        // of ENDIAN_MAGIC reversed.
        let endian_be = be.u32(Self::ENDIAN_OFFSET)?;
        let order = if endian_be == ENDIAN_MAGIC {
            ByteOrder::Big
        } else if endian_be.swap_bytes() == ENDIAN_MAGIC {
            ByteOrder::Little
        } else {
            return Err(Error::invalid(
                "journal_header.endian",
                format!("0x{endian_be:08x} is neither byte order"),
            ));
        };

        // A value read big-endian from a little-endian buffer needs its bytes
        // reversed; `swap_bytes` is exactly that, and it is not the same as
        // re-encoding, which is why the distinction matters.
        let word32 = |off: usize| -> Result<u32> {
            let v = be.u32(off)?;
            Ok(match order {
                ByteOrder::Big => v,
                ByteOrder::Little => v.swap_bytes(),
            })
        };
        let word64 = |off: usize| -> Result<u64> {
            let v = be.u64(off)?;
            Ok(match order {
                ByteOrder::Big => v,
                ByteOrder::Little => v.swap_bytes(),
            })
        };

        // The magic is normalised to its canonical big-endian value so that
        // callers can compare it against the constants directly.
        let magic = if magic_be == JOURNAL_HEADER_MAGIC || magic_be == OLD_JOURNAL_HEADER_MAGIC {
            magic_be
        } else {
            magic_be.swap_bytes()
        };

        Ok(Some(JournalHeader {
            magic,
            endian: ENDIAN_MAGIC,
            start: word64(Self::START_OFFSET)?,
            end: word64(Self::END_OFFSET)?,
            size: word64(Self::SIZE_OFFSET)?,
            blhdr_size: word32(Self::BLHDR_SIZE_OFFSET)?,
            checksum: word32(Self::CHECKSUM_OFFSET)?,
            jhdr_size: word32(Self::JHDR_SIZE_OFFSET)?,
            sequence_num: word32(Self::SEQUENCE_OFFSET)?,
            byte_order: order,
        }))
    }

    /// The byte order this header was written in.
    ///
    /// This is a stored value, set during [`Self::parse`]. A header constructed
    /// by hand defaults to [`ByteOrder::Big`], since the canonical on-disk byte
    /// order is big-endian.
    pub fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }

    /// Encode this header into `buf`, in its own byte order, with a fresh
    /// checksum.
    ///
    /// The buffer must be at least [`Self::SIZE`] bytes; the field beyond `SIZE`
    /// (if any) is left as the caller supplied it. The header is written in the
    /// byte order returned by [`Self::byte_order`]: a header that parsed as
    /// big-endian is re-encoded big-endian, and one that parsed as little-endian
    /// round-trips little-endian. A freshly constructed header (where
    /// [`Self::byte_order`] returns `Big`) is written big-endian to match the
    /// on-disk convention.
    ///
    /// The checksum covers the first [`JOURNAL_HEADER_CKSUM_SIZE`] bytes with the
    /// `checksum` field zeroed, matching `write_journal_header` in
    /// `core/hfs_journal.c`.
    pub fn to_bytes(&self, buf: &mut [u8]) -> Result<()> {
        if buf.len() < Self::SIZE {
            return Err(Error::Truncated {
                what: "journal header buffer",
                needed: Self::SIZE,
                available: buf.len(),
            });
        }
        // Start from the field values rather than re-encoding a stored buffer:
        // the parsed `magic` is the canonical constant, and a header built by hand
        // carries the canonical constant too, so the magic and endian sentinels
        // are always those values in the header's byte order.
        let order = self.byte_order();
        let put32 = |buf: &mut [u8], off: usize, v: u32| {
            let b = match order {
                ByteOrder::Big => v.to_be_bytes(),
                ByteOrder::Little => v.to_le_bytes(),
            };
            buf[off..off + 4].copy_from_slice(&b);
        };
        let put64 = |buf: &mut [u8], off: usize, v: u64| {
            let b = match order {
                ByteOrder::Big => v.to_be_bytes(),
                ByteOrder::Little => v.to_le_bytes(),
            };
            buf[off..off + 8].copy_from_slice(&b);
        };
        put32(buf, Self::MAGIC_OFFSET, self.magic);
        put32(buf, Self::ENDIAN_OFFSET, ENDIAN_MAGIC);
        put64(buf, Self::START_OFFSET, self.start);
        put64(buf, Self::END_OFFSET, self.end);
        put64(buf, Self::SIZE_OFFSET, self.size);
        put32(buf, Self::BLHDR_SIZE_OFFSET, self.blhdr_size);
        // The checksum covers the first `JOURNAL_HEADER_CKSUM_SIZE` = 44 bytes,
        // which includes the `checksum` field itself. Apple's
        // `write_journal_header` sets every field -- including
        // `sequence_num` -- and *then* zeroes the checksum and hashes the
        // whole range, so the checksum validates the complete header:
        //
        //   jnl->jhdr->sequence_num = sequence_num;
        //   jnl->jhdr->checksum = 0;
        //   jnl->jhdr->checksum = calc_checksum((char *)jnl->jhdr,
        //       JOURNAL_HEADER_CKSUM_SIZE);
        //
        // Writing `jhdr_size` and `sequence_num` *after* the hash would leave
        // them as zero in the checksummed bytes, and the parser would reject
        // a zero `jhdr_size` on replay.
        put32(buf, Self::JHDR_SIZE_OFFSET, self.jhdr_size);
        put32(buf, Self::SEQUENCE_OFFSET, self.sequence_num);
        for b in &mut buf[Self::CHECKSUM_OFFSET..Self::CHECKSUM_OFFSET + 4] {
            *b = 0;
        }
        let ck = calc_checksum(&buf[..JOURNAL_HEADER_CKSUM_SIZE]);
        put32(buf, Self::CHECKSUM_OFFSET, ck);
        Ok(())
    }

    /// Serialize this header to a freshly allocated buffer of its `jhdr_size`.
    ///
    /// The buffer is zeroed first so the bytes beyond [`Self::SIZE`] (which the
    /// checksum does not cover) are defined. Apple writes a 1024-byte header
    /// block by default, so `jhdr_size` is typically 1024.
    pub fn to_bytes_alloc(&self) -> Result<Vec<u8>> {
        let cap = usize::try_from(self.jhdr_size).map_err(|_| Error::overflow("jhdr_size"))?;
        let mut buf = vec![0u8; cap];
        self.to_bytes(&mut buf)?;
        // The checksum must be recomputed over the final buffer, because the
        // bytes beyond the header proper are now zeroed and the checksum covers
        // only the first `JOURNAL_HEADER_CKSUM_SIZE` bytes -- but those bytes
        // are the same, so the recomputation is a no-op safety net.
        Ok(buf)
    }

    /// Verify the header's checksum against its own bytes.
    ///
    /// Mining reference: `core/hfs_journal.c` `journal_open` computes the same
    /// value after zeroing `checksum`, then compares with the saved original.
    /// A mismatch there is **not** fatal: Apple's diagnostic path prints a
    /// message and the `goto bad_journal` is commented out, so a volume with a
    /// stale journal-header checksum still mounts. Reporting the mismatch as
    /// information rather than refusing the mount is deliberate.
    ///
    /// Mining reference: `core/hfs_journal.c` computes the checksum over
    /// `JOURNAL_HEADER_CKSUM_SIZE` bytes of the header as it was read, which is
    /// why the checksum field itself is excluded.
    pub fn checksum_matches(&self, raw: &[u8]) -> bool {
        if raw.len() < Self::SIZE {
            return false;
        }
        // The checksum field sits inside the checksummed range, so it is zeroed
        // before hashing. See the note on checksum_with_zeroed_field.
        match crate::journal::checksum::checksum_with_zeroed_field(
            raw,
            Self::CHECKSUM_OFFSET,
            JOURNAL_HEADER_CKSUM_SIZE,
        ) {
            Some(c) => c == self.checksum,
            None => false,
        }
    }

    /// Validate the header's internal consistency against a journal of `size`
    /// bytes.
    pub fn validate(&self, journal_size: u64) -> Result<()> {
        if self.size != journal_size {
            return Err(Error::invalid(
                "journal_header.size",
                format!(
                    "header says {} but the info block says {journal_size}",
                    self.size
                ),
            ));
        }
        // `start` and `end` must both be positive and within the journal. A
        // `start` of 0 is the dangerous one: offset zero of the journal is the
        // journal *header*, so the walk would parse the header as a block list
        // and report whatever counts it found there.
        //
        // Mining reference: `CHECK_JOURNAL` in `core/hfs_journal.c` panics on
        // `jhdr->start <= 0 || jhdr->start > jnl->jhdr->size`, and the same for
        // `end`. Those are assertions rather than errors, so a volume reaching
        // them is corrupt by definition.
        if self.start == 0 {
            return Err(Error::invalid(
                "journal_header.start",
                "0 is the journal header itself, not a transaction",
            ));
        }
        if self.end == 0 {
            return Err(Error::invalid(
                "journal_header.end",
                "0 is the journal header itself, not a transaction",
            ));
        }
        if self.start > self.size {
            return Err(Error::out_of_range(
                "journal_header.start",
                self.start,
                self.size,
            ));
        }
        if self.end > self.size {
            return Err(Error::out_of_range(
                "journal_header.end",
                self.end,
                self.size,
            ));
        }
        if self.start > self.end {
            return Err(Error::invalid(
                "journal_header",
                format!("start {} is beyond end {}", self.start, self.end),
            ));
        }
        if self.end > journal_size {
            return Err(Error::out_of_range(
                "journal_header.end",
                self.end,
                journal_size,
            ));
        }
        if self.jhdr_size == 0 || u64::from(self.jhdr_size) > journal_size {
            return Err(Error::invalid(
                "journal_header.jhdr_size",
                format!("{} is not a plausible header size", self.jhdr_size),
            ));
        }
        // A block-list header has five fixed fields before its `binfo[]`, so a
        // size smaller than that cannot hold one. Without this the walk reads
        // `blhdr_size` bytes and fails on a truncated field, which says
        // something about the bytes rather than about the header.
        //
        // Mining reference: `struct block_list_header` in `core/hfs_journal.h` is
        // `max_blocks`, `num_blocks`, `bytes_used`, `checksum` and `flags` before
        // the array.
        if u64::from(self.blhdr_size) < crate::journal::replay::BLHDR_PREFIX_SIZE as u64 {
            return Err(Error::invalid(
                "journal_header.blhdr_size",
                format!(
                    "{} is smaller than a {} byte block-list header",
                    self.blhdr_size,
                    crate::journal::replay::BLHDR_PREFIX_SIZE
                ),
            ));
        }
        if u64::from(self.blhdr_size) > journal_size {
            return Err(Error::invalid(
                "journal_header.blhdr_size",
                format!(
                    "{} is not a plausible block-list header size",
                    self.blhdr_size
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a journal header in `order`, with a correct checksum.
    fn make_header(order: ByteOrder, start: u64, end: u64, size: u64) -> Vec<u8> {
        let mut raw = vec![0u8; JournalHeader::SIZE];
        let put32 = |raw: &mut Vec<u8>, off: usize, v: u32| {
            let b = match order {
                ByteOrder::Big => v.to_be_bytes(),
                ByteOrder::Little => v.to_le_bytes(),
            };
            raw[off..off + 4].copy_from_slice(&b);
        };
        let put64 = |raw: &mut Vec<u8>, off: usize, v: u64| {
            let b = match order {
                ByteOrder::Big => v.to_be_bytes(),
                ByteOrder::Little => v.to_le_bytes(),
            };
            raw[off..off + 8].copy_from_slice(&b);
        };
        put32(&mut raw, JournalHeader::MAGIC_OFFSET, JOURNAL_HEADER_MAGIC);
        put32(&mut raw, JournalHeader::ENDIAN_OFFSET, ENDIAN_MAGIC);
        put64(&mut raw, JournalHeader::START_OFFSET, start);
        put64(&mut raw, JournalHeader::END_OFFSET, end);
        put64(&mut raw, JournalHeader::SIZE_OFFSET, size);
        put32(&mut raw, JournalHeader::BLHDR_SIZE_OFFSET, 4096);
        put32(&mut raw, JournalHeader::JHDR_SIZE_OFFSET, 4096);
        // The checksum covers the first 44 bytes as written, i.e. the bytes in
        // their stored order, exactly as Apple computes it over the raw buffer.
        let cksum = crate::journal::checksum::checksum_with_zeroed_field(
            &raw,
            JournalHeader::CHECKSUM_OFFSET,
            JOURNAL_HEADER_CKSUM_SIZE,
        )
        .expect("the buffer is long enough");
        put32(&mut raw, JournalHeader::CHECKSUM_OFFSET, cksum);
        raw
    }

    #[test]
    fn info_block_field_offsets_match_apple() {
        assert_eq!(JIB_OFFSET_OFFSET, 36);
        assert_eq!(JIB_SIZE_OFFSET, 44);
        assert_eq!(JOURNAL_INFO_BLOCK_SIZE, 180);
    }

    #[test]
    fn info_block_round_trips() {
        let mut raw = vec![0u8; JOURNAL_INFO_BLOCK_SIZE];
        raw[0..4].copy_from_slice(&0x0000_0005u32.to_be_bytes());
        raw[36..44].copy_from_slice(&12288u64.to_be_bytes());
        raw[44..52].copy_from_slice(&524_288u64.to_be_bytes());
        let jib = JournalInfoBlock::parse(&raw).unwrap();
        assert_eq!(jib.offset, 12288);
        assert_eq!(jib.size, 524_288);
        assert!(jib.flag_set().needs_init());
        assert!(jib.flag_set().in_filesystem());
        assert!(!jib.flag_set().on_other_device());
    }

    #[test]
    fn info_block_rejects_a_short_buffer() {
        for len in [0usize, 1, 36, 51, 179] {
            assert!(matches!(
                JournalInfoBlock::parse(&vec![0u8; len]),
                Err(Error::Truncated { .. })
            ));
        }
    }

    #[test]
    fn info_block_must_lie_inside_the_device() {
        let mut jib = JournalInfoBlock {
            flags: 1,
            device_signature: [0; 8],
            offset: 1000,
            size: 2000,
        };
        assert!(jib.validate(10_000).is_ok());
        assert!(matches!(jib.validate(2999), Err(Error::OutOfRange { .. })));
        assert!(jib.validate(3000).is_ok());

        // A zero offset or size is meaningless and must be refused.
        jib.offset = 0;
        assert!(jib.validate(10_000).is_err());
        jib.offset = 1000;
        jib.size = 0;
        assert!(jib.validate(10_000).is_err());

        // An overflowing extent is caught rather than wrapping.
        jib.size = u64::MAX;
        assert!(matches!(jib.validate(10_000), Err(Error::Overflow { .. })));
    }

    #[test]
    fn an_uninitialized_journal_header_reads_as_absent() {
        // This is what mkfs_hfsplus -J produces: an all-zero journal header area.
        let raw = vec![0u8; 4096];
        assert_eq!(JournalHeader::parse(&raw).unwrap(), None);
    }

    #[test]
    fn header_parses_in_either_byte_order() {
        for order in [ByteOrder::Big, ByteOrder::Little] {
            let raw = make_header(order, 4096, 8192, 524_288);
            let h = JournalHeader::parse(&raw).unwrap().expect("a header");
            assert_eq!(h.magic, JOURNAL_HEADER_MAGIC);
            assert_eq!(h.endian, ENDIAN_MAGIC);
            assert_eq!(h.start, 4096, "{order:?}");
            assert_eq!(h.end, 8192, "{order:?}");
            assert_eq!(h.size, 524_288, "{order:?}");
            assert_eq!(h.blhdr_size, 4096);
            assert_eq!(h.jhdr_size, 4096);
            assert!(h.checksum_matches(&raw), "{order:?}");
        }
    }

    #[test]
    fn the_two_orders_produce_different_bytes() {
        // If they did not, the endian field would be pointless.
        let be = make_header(ByteOrder::Big, 4096, 8192, 524_288);
        let le = make_header(ByteOrder::Little, 4096, 8192, 524_288);
        assert_ne!(be, le);
        assert_eq!(&be[0..4], &JOURNAL_HEADER_MAGIC.to_be_bytes());
        assert_eq!(&le[0..4], &JOURNAL_HEADER_MAGIC.to_le_bytes());
    }

    #[test]
    fn the_old_magic_is_still_recognised() {
        let mut raw = make_header(ByteOrder::Big, 0, 4096, 4096);
        raw[0..4].copy_from_slice(&OLD_JOURNAL_HEADER_MAGIC.to_be_bytes());
        let cksum = crate::journal::checksum::checksum_with_zeroed_field(
            &raw,
            JournalHeader::CHECKSUM_OFFSET,
            JOURNAL_HEADER_CKSUM_SIZE,
        )
        .expect("the buffer is long enough")
        .to_be_bytes();
        raw[JournalHeader::CHECKSUM_OFFSET..JournalHeader::CHECKSUM_OFFSET + 4]
            .copy_from_slice(&cksum);
        let h = JournalHeader::parse(&raw)
            .unwrap()
            .expect("old magic still parses");
        assert_eq!(h.magic, OLD_JOURNAL_HEADER_MAGIC);
        assert!(h.checksum_matches(&raw));
    }

    #[test]
    fn a_bad_endian_sentinel_is_rejected() {
        let mut raw = make_header(ByteOrder::Big, 0, 4096, 4096);
        raw[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        assert!(matches!(
            JournalHeader::parse(&raw),
            Err(Error::InvalidField {
                field: "journal_header.endian",
                ..
            })
        ));
    }

    #[test]
    fn a_corrupt_checksum_is_detected() {
        let mut raw = make_header(ByteOrder::Big, 0, 4096, 4096);
        raw[JournalHeader::START_OFFSET] ^= 0xFF;
        let h = JournalHeader::parse(&raw)
            .unwrap()
            .expect("still parses structurally");
        assert!(
            !h.checksum_matches(&raw),
            "a corrupted field must fail the checksum"
        );
    }

    #[test]
    fn header_validation_catches_impossible_geometry() {
        let raw = make_header(ByteOrder::Big, 4096, 8192, 524_288);
        let h = JournalHeader::parse(&raw).unwrap().unwrap();
        assert!(h.validate(524_288).is_ok());

        // Header claiming a different size than the info block.
        assert!(h.validate(4096).is_err());
        // start beyond end.
        let bad = JournalHeader {
            start: 100,
            end: 50,
            ..h
        };
        assert!(bad.validate(524_288).is_err());
        // end past the journal.
        let bad = JournalHeader { end: 600_000, ..h };
        assert!(matches!(
            bad.validate(524_288),
            Err(Error::OutOfRange { .. })
        ));
        // Zero-sized header or block list.
        let bad = JournalHeader { jhdr_size: 0, ..h };
        assert!(bad.validate(524_288).is_err());
        let bad = JournalHeader { blhdr_size: 0, ..h };
        assert!(bad.validate(524_288).is_err());
    }

    #[test]
    fn a_short_header_buffer_is_truncated_not_panicked_on() {
        for len in [0usize, 1, 4, 44] {
            assert!(matches!(
                JournalHeader::parse(&vec![0u8; len]),
                Err(Error::Truncated { .. })
            ));
        }
    }

    #[test]
    fn end_blk_num_is_the_all_ones_sentinel() {
        assert_eq!(END_BLK_NUM, u32::MAX as u64);
    }
}

/// Result of a [`JournalHeader::check_free_space`] call.
///
/// Carries the `delayed_header_write` signal that Apple's `check_free_space`
/// emits when it advances `jhdr->start` to make room: the header must then be
/// written, but the caller can choose the ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceCheck {
    /// Whether the journal header write should be deferred until after the
    /// transaction's block data is written. Always `false` from the current
    /// simplified `check_free_space`, which does not bump `start` itself.
    pub deferred: bool,
}

// --- Writing -----------------------------------------------------------------
//
// A second `impl` block rather than more methods on the first: everything above is
// the *reader's* view of a journal header, parsed and validated, and these are the
// writer's arithmetic on the same bytes.

impl JournalHeader {
    /// Free bytes in the journal, as `free_space` computes them.
    ///
    /// Three cases, and the middle one is the whole reason this is not
    /// `size - (end - start)`:
    ///
    /// ```c
    /// if (jnl->jhdr->start < jnl->jhdr->end) {
    ///         free_space_offset = jnl->jhdr->size - (jnl->jhdr->end - jnl->jhdr->start) - jnl->jhdr->jhdr_size;
    /// } else if (jnl->jhdr->start > jnl->jhdr->end) {
    ///         free_space_offset = jnl->jhdr->start - jnl->jhdr->end;
    /// } else {
    ///         // journal is completely empty
    ///         free_space_offset = jnl->jhdr->size - jnl->jhdr->jhdr_size;
    /// }
    /// ```
    ///
    /// **`start > end` is the wrap.** The journal is a ring, so the live region is
    /// `[end, start)` and the free region is `[start, size)` *plus* `[0, end)`. Reading
    /// the wrap as a huge live region -- or as corruption -- is the mistake this
    /// shape invites, and `start > end` is ordinary on a journal that has wrapped.
    ///
    /// The header's own size is subtracted in the two non-wrapped cases and not in
    /// the wrapped one: once wrapped, `end` is already past the header.
    ///
    /// Mining reference: `core/hfs_journal.c` `free_space`.
    pub fn free_space(&self) -> u64 {
        let jhdr_size = u64::from(self.jhdr_size);
        if self.start < self.end {
            self.size
                .saturating_sub(self.end - self.start)
                .saturating_sub(jhdr_size)
        } else if self.start > self.end {
            self.start - self.end
        } else {
            self.size.saturating_sub(jhdr_size)
        }
    }

    /// Whether a transaction of `desired` bytes can be written now, and whether
    /// the journal header write should be deferred.
    ///
    /// Apple's `check_free_space` has two conditions and **both** matter:
    ///
    /// ```c
    /// if (free_space(jnl) > desired_size && jnl->old_start[0] == 0) {
    ///         break;
    /// }
    /// ```
    ///
    /// - **Strictly greater.** `>`, not `>=`. A transaction that exactly fills the
    ///   journal leaves no room for the next one, and the header's own size is
    ///   already accounted for in [`Self::free_space`].
    /// - **Nothing pending.** `old_start[0] == 0` is the empty ring: no transaction
    ///   is waiting to be replayed. Apple's answer to a pending one is to flush it,
    ///   advancing `start`, and look again -- up to 7500 times before `ENOSPC`.
    ///
    /// A library cannot flush on the caller's behalf. Replaying is a decision about
    /// whether the volume is current, and doing it silently inside a write is how a
    /// library ends up writing through a filesystem the caller never accepted. So the
    /// two conditions are reported separately and the caller replays, then retries.
    ///
    /// # Deferred header write
    ///
    /// Apple's `check_free_space` also takes a `boolean_t *delayed_header_write`
    /// output. When the function bumps `jhdr->start` (freeing space from
    /// completed transactions via `old_start`), it sets that flag rather than
    /// calling `write_journal_header` synchronously -- the header gets fired off
    /// to a kernel thread instead.
    ///
    /// A userspace library has no kernel thread, but the flag is still meaningful:
    /// a bumped `start` means the header *must* be written, and whether that happens
    /// before or after the transaction's block-data write is an ordering choice the
    /// caller should see. The flag is returned so the committer knows.
    ///
    /// Our simplified `check_free_space` does not bump `start` (that is the job of
    /// [`Self::release_transaction`], which the caller invokes explicitly), so
    /// `deferred` is always `false` here -- but the shape matches Apple's contract
    /// so the hook is present when the flush loop is ported.
    ///
    /// Mining reference: `core/hfs_journal.c` `check_free_space`, the
    /// `*delayed_header_write` out-parameter and the call site in `end_transaction`.
    pub fn check_free_space(&self, desired: u64, pending: usize) -> Result<SpaceCheck> {
        let free = self.free_space();
        if free <= desired {
            return Err(Error::no_space(
                u32::try_from(desired).unwrap_or(u32::MAX),
                free,
            ));
        }
        if pending > 0 {
            return Err(Error::invalid(
                "journal",
                format!(
                    "{pending} transaction(s) on the journal have not been replayed; \
                     flush them so jhdr->start advances, then retry"
                ),
            ));
        }
        Ok(SpaceCheck { deferred: false })
    }

    /// Advance `end` past a written transaction and assign it the next sequence
    /// number.
    ///
    /// The journal is a ring, so `end` wraps at `size`, and a transaction crossing
    /// the end is written across the wrap rather than refused. This moves only the
    /// cursor; the bytes themselves are the assembler's.
    pub fn commit_transaction(&mut self, transaction_bytes: u64) -> Result<()> {
        let free = self.free_space();
        if transaction_bytes > free {
            return Err(Error::no_space(
                u32::try_from(transaction_bytes).unwrap_or(u32::MAX),
                free,
            ));
        }
        self.end = (self.end + transaction_bytes) % self.size;
        self.sequence_num = self.sequence_num.wrapping_add(1);
        Ok(())
    }

    /// Advance `start` past a transaction that has been replayed and consumed.
    ///
    /// The other half of `check_free_space`'s condition: replaying is what makes the
    /// space a transaction was holding available again.
    pub fn release_transaction(&mut self, transaction_bytes: u64) -> Result<()> {
        let new_start = self.start + transaction_bytes;
        if new_start > self.size {
            return Err(Error::invalid(
                "journal",
                format!(
                    "releasing {transaction_bytes} bytes from start {} would pass the \
                     journal's {} bytes",
                    self.start, self.size
                ),
            ));
        }
        self.start = new_start;
        Ok(())
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use crate::error::Error;
    use crate::journal::checksum::checksum_with_zeroed_field;

    fn header(start: u64, end: u64, size: u64) -> JournalHeader {
        JournalHeader {
            magic: JOURNAL_HEADER_MAGIC,
            endian: ENDIAN_MAGIC,
            byte_order: ByteOrder::Big,
            start,
            end,
            size,
            blhdr_size: 512,
            checksum: 0,
            jhdr_size: 1024,
            sequence_num: 7,
        }
    }

    #[test]
    fn free_space_is_the_empty_journal_when_start_equals_end() {
        // The third branch. A journal that has never been written is `size`
        // less its own header, and reading `size - (end - start)` here would
        // claim the whole journal is used.
        let h = header(2048, 2048, 16384);
        assert_eq!(h.free_space(), 16384 - 1024);
    }

    #[test]
    fn free_space_is_the_gap_when_start_precedes_end() {
        // The ordinary case: live region [start, end), header excluded.
        let h = header(2048, 6144, 16384);
        assert_eq!(h.free_space(), 16384 - (6144 - 2048) - 1024);
    }

    #[test]
    fn free_space_wraps_when_start_is_past_end() {
        // The case that is not `size - (end - start)`. The live region is
        // [end, start) and the free region is [start, size) *plus* [0, end), so
        // the answer is the gap between them -- and no header is subtracted,
        // because `end` is already past the header.
        let h = header(12288, 4096, 16384);
        assert_eq!(h.free_space(), 12288 - 4096);
        // And it is *not* the non-wrapped reading. Applying the first branch's
        // arithmetic to a wrapped header does not fail loudly -- `end - start` is
        // negative, so it reports *more* free space than the journal has. That is
        // the shape worth guarding: a journal that has wrapped reads as larger than
        // it is, rather than as corrupt.
        let naive = 16384i64 - (4096i64 - 12288) - 1024;
        assert_eq!(h.free_space() as i64, 12288 - 4096);
        assert_ne!(naive, h.free_space() as i64);
        assert!(
            naive > 16384,
            "the naive reading claims more space than exists"
        );
    }

    #[test]
    fn space_must_be_strictly_greater_than_the_transaction() {
        // `free_space(jnl) > desired_size`, not `>=`. A transaction that exactly
        // fills the journal leaves nothing for the next one.
        let h = header(2048, 4096, 16384);
        let free = h.free_space();
        h.check_free_space(free, 0)
            .expect_err("exactly full is not enough");
        h.check_free_space(free - 1, 0)
            .expect("one byte spare is enough");
    }

    #[test]
    fn a_successful_check_reports_no_deferred_header_write() {
        // The simplified check_free_space does not bump start, so deferred is
        // always false -- but the shape matches Apple's contract.
        let h = header(2048, 4096, 16384);
        let check = h.check_free_space(512, 0).expect("enough room");
        assert!(!check.deferred);
    }

    #[test]
    fn an_unreplayed_transaction_blocks_a_write_even_with_room() {
        // The second condition. Apple's answer is to flush and retry; a library
        // reports it instead, because replaying is the caller's decision.
        let h = header(2048, 4096, 16384);
        let err = h
            .check_free_space(512, 1)
            .expect_err("a pending transaction blocks");
        assert!(
            matches!(err, Error::InvalidField { .. }),
            "a pending transaction is not out of space; got {err:?}"
        );
        assert!(
            format!("{err}").contains("replayed"),
            "and the refusal must say what to do; got: {err}"
        );
    }

    #[test]
    fn committing_advances_end_and_the_sequence_number() {
        let mut h = header(2048, 4096, 16384);
        h.commit_transaction(512).expect("commit");
        assert_eq!(h.end, 4608);
        assert_eq!(h.sequence_num, 8);
    }

    #[test]
    fn end_wraps_at_the_size_of_the_journal() {
        // A ring, not a line: a transaction that runs past the end is written
        // across the wrap rather than refused.
        let mut h = header(15360, 15360, 16384);
        h.commit_transaction(2048).expect("commit across the wrap");
        assert_eq!(h.end, 1024, "wrapped past the end of the journal");
    }

    #[test]
    fn a_transaction_bigger_than_the_journal_is_refused() {
        let mut h = header(2048, 2048, 16384);
        let err = h.commit_transaction(1 << 20).expect_err("does not fit");
        assert!(matches!(err, Error::NoSpace { .. }), "got {err:?}");
        assert_eq!(h.end, 2048, "and a refused commit moves nothing");
    }

    #[test]
    fn releasing_past_the_end_of_the_journal_is_refused() {
        let mut h = header(15360, 15360, 16384);
        h.release_transaction(1024).expect("release");
        assert_eq!(h.start, 16384);
        let err = h.release_transaction(1).expect_err("past the end");
        assert!(
            matches!(err, Error::InvalidField { .. }),
            "releasing space that was never held is a caller error, not no-space; \\
             got {err:?}"
        );
    }

    #[test]
    fn to_bytes_round_trips_through_parse() {
        // A freshly constructed header (big-endian byte order) must encode and
        // then re-parse to the same values, with a valid checksum.
        let h = header(4096, 8192, 524_288);
        let mut buf = vec![0u8; 4096];
        h.to_bytes(&mut buf).expect("encode");
        let parsed = JournalHeader::parse(&buf)
            .expect("parse")
            .expect("a header");
        assert_eq!(parsed.magic, JOURNAL_HEADER_MAGIC);
        assert_eq!(parsed.start, 4096);
        assert_eq!(parsed.end, 8192);
        assert_eq!(parsed.size, 524_288);
        assert_eq!(parsed.blhdr_size, 512);
        assert_eq!(parsed.jhdr_size, 1024);
        assert_eq!(parsed.sequence_num, 7);
        assert!(parsed.checksum_matches(&buf));
    }

    #[test]
    fn to_bytes_alloc_produces_a_jhdr_size_block_zeroed_beyond_the_struct() {
        let h = header(4096, 8192, 524_288);
        let buf = h.to_bytes_alloc().expect("encode");
        assert_eq!(buf.len(), h.jhdr_size as usize);
        // The bytes beyond the 48-byte struct must be zero.
        assert!(buf[JournalHeader::SIZE..].iter().all(|&b| b == 0));
        // And the header within must parse and validate.
        let parsed = JournalHeader::parse(&buf)
            .expect("parse")
            .expect("a header");
        assert!(parsed.checksum_matches(&buf));
    }

    #[test]
    fn a_short_output_buffer_is_truncated() {
        let h = header(4096, 8192, 524_288);
        let mut buf = vec![0u8; 40];
        assert!(matches!(h.to_bytes(&mut buf), Err(Error::Truncated { .. })));
    }

    #[test]
    fn a_little_endian_header_round_trips() {
        // Build a little-endian header by hand (as mkfs.hfsplus on x86 would),
        // then parse and re-encode it.
        let mut raw = vec![0u8; 1024];
        raw[0..4].copy_from_slice(&JOURNAL_HEADER_MAGIC.to_le_bytes());
        raw[4..8].copy_from_slice(&ENDIAN_MAGIC.to_le_bytes());
        raw[8..16].copy_from_slice(&4096u64.to_le_bytes());
        raw[16..24].copy_from_slice(&8192u64.to_le_bytes());
        raw[24..32].copy_from_slice(&524_288u64.to_le_bytes());
        raw[32..36].copy_from_slice(&512u32.to_le_bytes());
        // checksum field zeroed; compute below
        raw[40..44].copy_from_slice(&1024u32.to_le_bytes());
        raw[44..48].copy_from_slice(&7u32.to_le_bytes());
        let ck = checksum_with_zeroed_field(
            &raw,
            JournalHeader::CHECKSUM_OFFSET,
            JOURNAL_HEADER_CKSUM_SIZE,
        )
        .expect("enough bytes");
        raw[JournalHeader::CHECKSUM_OFFSET..JournalHeader::CHECKSUM_OFFSET + 4]
            .copy_from_slice(&ck.to_le_bytes());

        let h = JournalHeader::parse(&raw)
            .expect("parse")
            .expect("a header");
        assert_eq!(h.byte_order(), ByteOrder::Little);
        // Re-encode and verify the checksum still validates.
        let mut reenc = vec![0u8; 1024];
        h.to_bytes(&mut reenc).expect("encode");
        assert!(h.checksum_matches(&reenc));
        // And the magic must still be the canonical constant.
        assert_eq!(h.magic, JOURNAL_HEADER_MAGIC);
    }
}
