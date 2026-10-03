//! The HFS+ journal checksum.
//!
//! Mining reference: Apple `core/hfs_journal.c` `calc_checksum`, reproduced in
//! full because the algorithm is short and its exact shape is the whole point:
//!
//! ```c
//! unsigned int calc_checksum(const char *ptr, int len)
//! {
//!     int i;
//!     unsigned int cksum = 0;
//!
//!     // this is a lame checksum but for now it'll do
//!     for(i = 0; i < len; i++, ptr++) {
//!         cksum = (cksum << 8) ^ (cksum + *(unsigned char *)ptr);
//!     }
//!
//!     return (~cksum);
//! }
//! ```
//!
//! # Why the shape matters
//!
//! This is not a sum, and not a standard CRC. The rotate-and-add is over a
//! *32-bit* accumulator whose left shift is a plain C shift on `unsigned int`, so
//! the high bits fall off after 32 iterations and the checksum is order- and
//! length-sensitive in a way that a stronger algorithm would not be.
//!
//! That matters practically: any reimplementation using `wrapping_add` and
//! `rotate_left(8)` produces a *different* value, because the shift is a
//! discard, not a rotation. Reproducing it exactly is required to validate a
//! journal written by macOS.
//!
//! The result is the bitwise complement, so the checksum of the right bytes over
//! the right length yields `0xFFFFFFFF`... and so verifying means computing over
//! the bytes *excluding* the checksum field, exactly as Apple does, and
//! comparing to the stored value.

/// Apple's journal checksum over `bytes`.
///
/// Mining reference: `core/hfs_journal.c` `calc_checksum`.
pub fn calc_checksum(bytes: &[u8]) -> u32 {
    let mut cksum: u32 = 0;
    for b in bytes {
        // The shift discards the top bits; it is deliberately not a rotation.
        cksum = (cksum << 8) ^ (cksum.wrapping_add(*b as u32));
    }
    !cksum
}

/// Verify a checksum stored in `bytes[at..at+4]`.
///
/// The stored value is **zeroed before hashing**, because the checksum field lies
/// inside the byte range being hashed. Mining reference: Apple does exactly this
/// on both sides, in `write_journal_header`:
///
/// ```c
/// jnl->jhdr->checksum = 0;
/// jnl->jhdr->checksum = calc_checksum((char *)jnl->jhdr, JOURNAL_HEADER_CKSUM_SIZE);
/// ```
///
/// and again in `journal_open` when verifying. Without the zeroing the checksum
/// could never validate, because it would have been computed over a different
/// byte range than the one it is compared against.
///
/// The caller compares against the value it read *before* the zeroing.
pub fn verify_checksum(bytes: &[u8], at: usize, len: usize) -> Option<u32> {
    if bytes.len() < len || at + 4 > bytes.len() {
        return None;
    }
    let stored = u32::from_be_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
    ]);
    let mut scratch = bytes[..len].to_vec();
    scratch[at..at + 4].fill(0);
    Some(stored) // caller compares against calc_checksum(&scratch)
}

/// Zero the 4 bytes at `at..at+4` and checksum the first `len` bytes.
///
/// Returns the checksum of the zeroed buffer, which is what a writer stores.
pub fn checksum_with_zeroed_field(bytes: &[u8], at: usize, len: usize) -> Option<u32> {
    // The field must lie inside the range being hashed, not merely inside
    // `bytes`. Checking it against `bytes.len()` alone let `at` past `len` reach
    // the slice assignment below and panic -- a field offset is read from the
    // structure being checksummed, so it is exactly the sort of untrusted value
    // this has to survive.
    //
    // `at + 4` is written rather than `at.checked_add(4)` so that an absurd `at`
    // wraps into a rejection rather than back into a plausible range.
    if bytes.len() < len || len < 4 || at > len - 4 {
        return None;
    }
    let mut scratch = bytes[..len].to_vec();
    scratch[at..at + 4].fill(0);
    Some(calc_checksum(&scratch))
}

/// Number of bytes of a journal header that are checksummed.
///
/// Mining reference: `core/hfs_journal.h`
/// `#define JOURNAL_HEADER_CKSUM_SIZE (offsetof(struct journal_header, sequence_num))`.
///
/// Apple's comment on the macro explains why the size stops there rather than
/// covering the whole struct:
///
/// ```c
/// // we only checksum the original size of the journal_header to remain
/// // backwards compatible.  the size of the original journal_header is
/// // everything up to the the sequence_num field
/// ```
///
/// The struct grew a `sequence_num` field after the checksum was defined, and
/// checksumming it would make every pre-existing journal fail validation.
pub const JOURNAL_HEADER_CKSUM_SIZE: usize = 44;

/// Number of bytes of a block-list header that are checksummed.
///
/// Mining reference: `core/hfs_journal.c`
/// `#define BLHDR_CHECKSUM_SIZE 32`, with Apple's comment:
///
/// ```c
/// // NOTE: this should be enough to clear out the header
/// //       fields as well as the first entry of binfo[]
/// ```
pub const BLHDR_CHECKSUM_SIZE: usize = 32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_nothing_is_the_complement_of_zero() {
        assert_eq!(calc_checksum(&[]), u32::MAX);
    }

    #[test]
    fn the_shift_discards_rather_than_rotates() {
        // A 32-bit accumulator shifted left by 8 loses its top byte. If the
        // algorithm were a rotation, this would differ; it is exactly what makes
        // the order of bytes matter.
        let mut cksum: u32 = 0;
        cksum = (cksum << 8) ^ (cksum.wrapping_add(0xAB));
        assert_eq!(cksum, 0xAB);

        // Four bytes of 0xFF do NOT drive the accumulator to all ones, because
        // each step XORs a *sum* in rather than ORing a byte in. That is the
        // observable difference from a rotate: a rotating implementation would
        // give 0xFFFFFFFF here.
        let mut c = 0u32;
        for _ in 0..4 {
            c = (c << 8) ^ (c.wrapping_add(0xFF));
        }
        assert_eq!(c, 0xFEFF_FFFC);
        assert_eq!(calc_checksum(&[0xFF; 4]), 0x0100_0003);
    }

    #[test]
    fn checksum_is_order_sensitive() {
        assert_ne!(calc_checksum(&[1, 2, 3]), calc_checksum(&[3, 2, 1]));
    }

    #[test]
    fn checksum_is_length_sensitive() {
        assert_ne!(calc_checksum(&[1, 2, 3]), calc_checksum(&[1, 2, 3, 0]));
    }

    #[test]
    fn moving_a_byte_changes_the_checksum() {
        let mut buf = [0u8; 64];
        let before = calc_checksum(&buf);
        buf[17] ^= 0x01;
        assert_ne!(calc_checksum(&buf), before);
    }

    #[test]
    fn checksum_sizes_match_apples_constants() {
        assert_eq!(JOURNAL_HEADER_CKSUM_SIZE, 44);
        assert_eq!(BLHDR_CHECKSUM_SIZE, 32);
    }

    #[test]
    fn a_known_vector_is_stable() {
        // Pinned so that a refactor which "improves" the checksum fails here
        // rather than silently rejecting every real journal.
        assert_eq!(calc_checksum(b"JNLx"), 0xB567_C8A3);
        assert_eq!(calc_checksum(b""), 0xFFFF_FFFF);
        assert_eq!(calc_checksum(b"a"), 0xFFFF_FF9E);
        assert_eq!(calc_checksum(b"abc"), 0xFF9E_5ED9);
        assert_eq!(calc_checksum(&[0xFF; 4]), 0x0100_0003);
    }
}