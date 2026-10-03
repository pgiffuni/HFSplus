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

use super::checksum::JOURNAL_HEADER_CKSUM_SIZE;
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
/// Mining reference: `core/hfs_format.h` computes `JIB_RESERVED_SIZE` so that the
/// struct totals `32 * sizeof(u_int32_t)` for the leading fields, plus the UUID
/// and serial number. The whole struct occupies one allocation block; this is the
/// length of the declared fields, not of the block.
pub const JOURNAL_INFO_BLOCK_SIZE: usize = 4 + 32 + 8 + 8 + 37 + 48 + 43;

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
    pub fn validate(&self, device_bytes: u64) -> Result<()> {
        if self.offset == 0 || self.size == 0 {
            return Err(Error::invalid(
                "JournalInfoBlock",
                format!("offset {} and size {} must both be non-zero", self.offset, self.size),
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
    pub magic: u32,
    /// `ENDIAN_MAGIC`, in the header's own byte order.
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
        }))
    }

    /// The byte order this header was written in.
    pub fn byte_order(&self) -> ByteOrder {
        // Both orders produce a normalised `endian` of ENDIAN_MAGIC, so the order
        // is recovered from the stored magic's natural reading instead. A header
        // that parsed at all is already known-good, so this is only for display.
        if self.magic == JOURNAL_HEADER_MAGIC || self.magic == OLD_JOURNAL_HEADER_MAGIC {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        }
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
                format!("header says {} but the info block says {journal_size}", self.size),
            ));
        }
        if self.start > self.end {
            return Err(Error::invalid(
                "journal_header",
                format!("start {} is beyond end {}", self.start, self.end),
            ));
        }
        if self.end > journal_size {
            return Err(Error::out_of_range("journal_header.end", self.end, journal_size));
        }
        if self.jhdr_size == 0 || u64::from(self.jhdr_size) > journal_size {
            return Err(Error::invalid(
                "journal_header.jhdr_size",
                format!("{} is not a plausible header size", self.jhdr_size),
            ));
        }
        if self.blhdr_size == 0 || u64::from(self.blhdr_size) > journal_size {
            return Err(Error::invalid(
                "journal_header.blhdr_size",
                format!("{} is not a plausible block-list header size", self.blhdr_size),
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
        let h = JournalHeader::parse(&raw).unwrap().expect("old magic still parses");
        assert_eq!(h.magic, OLD_JOURNAL_HEADER_MAGIC);
        assert!(h.checksum_matches(&raw));
    }

    #[test]
    fn a_bad_endian_sentinel_is_rejected() {
        let mut raw = make_header(ByteOrder::Big, 0, 4096, 4096);
        raw[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        assert!(matches!(
            JournalHeader::parse(&raw),
            Err(Error::InvalidField { field: "journal_header.endian", .. })
        ));
    }

    #[test]
    fn a_corrupt_checksum_is_detected() {
        let mut raw = make_header(ByteOrder::Big, 0, 4096, 4096);
        raw[JournalHeader::START_OFFSET] ^= 0xFF;
        let h = JournalHeader::parse(&raw).unwrap().expect("still parses structurally");
        assert!(!h.checksum_matches(&raw), "a corrupted field must fail the checksum");
    }

    #[test]
    fn header_validation_catches_impossible_geometry() {
        let raw = make_header(ByteOrder::Big, 4096, 8192, 524_288);
        let h = JournalHeader::parse(&raw).unwrap().unwrap();
        assert!(h.validate(524_288).is_ok());

        // Header claiming a different size than the info block.
        assert!(h.validate(4096).is_err());
        // start beyond end.
        let bad = JournalHeader { start: 100, end: 50, ..h };
        assert!(bad.validate(524_288).is_err());
        // end past the journal.
        let bad = JournalHeader { end: 600_000, ..h };
        assert!(matches!(bad.validate(524_288), Err(Error::OutOfRange { .. })));
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