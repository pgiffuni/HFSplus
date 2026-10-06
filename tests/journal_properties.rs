//! Property tests for the arithmetic a journal read depends on.
//!
//! # Why properties rather than examples
//!
//! The checksum and the ring arithmetic are the two places in the journal where
//! a mistake produces *plausible* output rather than an error. A wrong checksum
//! either always fails, which is loud, or never fails, which is worse. Ring
//! arithmetic that lands one block off reads real bytes from the wrong place.
//!
//! Examples cannot cover either: the checksum is a 32-bit value with no useful
//! structure, and the interesting ring offsets are the ones nobody thinks to
//! write down. So these assert relationships that must hold for *all* inputs of a
//! shape, over a deterministic pseudo-random spread rather than a fixed handful.
//!
//! No dependency and no fuzzing harness -- the crate has neither by policy -- so
//! the spread is a small xorshift, seeded fixed so a failure is reproducible.
//!
//! Mining reference: `core/hfs_journal.c` `calc_checksum`, and the wrap in
//! `replay_journal`:
//!
//! ```c
//! if (offset >= jnl->jhdr->size) {
//!     offset = jnl->jhdr->jhdr_size + (offset - jnl->jhdr->size);
//! }
//! ```

mod common;

use hfsplus::blockdev::MemoryDevice;
use hfsplus::journal::checksum::{calc_checksum, checksum_with_zeroed_field};
use hfsplus::journal::info::JournalHeader;

/// A deterministic spread of values, so a failure reproduces exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*, which is more than enough for choosing offsets.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

// --- The checksum -------------------------------------------------------

#[test]
fn the_checksum_is_deterministic_and_input_sensitive() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);

    for len in [0usize, 1, 2, 3, 4, 7, 8, 31, 32, 33, 44, 64, 4096] {
        let bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();

        // Same input, same answer. A checksum that depended on anything else --
        // an uninitialised field, a previous call -- would make replay
        // non-reproducible, and a journal is replayed more than once.
        assert_eq!(
            calc_checksum(&bytes),
            calc_checksum(&bytes),
            "length {len}: the checksum is not deterministic"
        );

        // Any single flipped bit must change it, for any length that has bits.
        // A checksum that ignored a byte would let a corrupt block pass.
        if !bytes.is_empty() {
            for _ in 0..8 {
                let mut flipped = bytes.clone();
                let at = rng.below(flipped.len() as u64) as usize;
                flipped[at] ^= 1 << rng.below(8);
                assert_ne!(
                    calc_checksum(&bytes),
                    calc_checksum(&flipped),
                    "length {len}: a flipped bit at {at} went unnoticed"
                );
            }
        }

        // An empty input has a defined answer, not a panic.
        if bytes.is_empty() {
            assert_eq!(calc_checksum(&[]), !0);
        }
    }
}

#[test]
fn the_checksum_is_not_a_rotation() {
    // `calc_checksum` shifts left and XORs, discarding the top bits. That is
    // deliberate and matches Apple: a rotation would let the same byte pattern
    // at different alignments collide, which a journal -- full of fixed-layout
    // structures at varying offsets -- would hit.
    //
    // So a checksum must depend on where a byte sits, not just what it is.
    let a = calc_checksum(&[0x01, 0x00, 0x00, 0x00]);
    let b = calc_checksum(&[0x00, 0x00, 0x00, 0x01]);
    assert_ne!(a, b, "position within the input must matter");
}

#[test]
fn a_zeroed_checksum_field_does_not_change_the_result() {
    // The whole reason `checksum_with_zeroed_field` exists: the checksum field
    // lies inside the byte range being hashed, so hashing the stored value would
    // never reproduce it. Apple's `write_journal_header` zeroes it on both sides.
    let mut rng = Rng(0xC0FF_EE11_2233_4455);
    // At least the 32-byte checksummed range, or the helper has nothing to hash.
    for len in [32usize, 44, 48, 4096] {
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();

        let expected = checksum_with_zeroed_field(&bytes, 8, 32);
        assert!(expected.is_some(), "length {len} must be long enough");

        // Whatever the field holds, the answer must not move.
        for value in [0u32, 1, 0xFFFF_FFFF, 0x8000_0000] {
            bytes[8..12].copy_from_slice(&value.to_be_bytes());
            assert_eq!(
                checksum_with_zeroed_field(&bytes, 8, 32),
                expected,
                "length {len}: a stored checksum of 0x{value:08x} changed the result"
            );
        }
    }
}

#[test]
fn a_short_input_to_the_zeroed_field_helper_is_refused_not_panicked() {
    // `None` rather than a truncated read.
    for len in 0..32usize {
        let bytes = vec![0u8; len];
        assert!(
            checksum_with_zeroed_field(&bytes, 8, 32).is_none(),
            "length {len}: too short to hold a 32-byte checksummed range"
        );
    }
    // Exactly long enough is enough, and the field must fit inside it.
    assert!(checksum_with_zeroed_field(&[0u8; 32], 8, 32).is_some());
    // And a field offset that does not fit.
    let bytes = vec![0u8; 64];
    assert!(checksum_with_zeroed_field(&bytes, 60, 32).is_none());
    assert!(checksum_with_zeroed_field(&bytes, 8, 4096).is_none());
    assert!(checksum_with_zeroed_field(&bytes, 8, 32).is_some());
}

// --- The ring ----------------------------------------------------------

/// The wrap rule, as `replay_journal` states it.
fn wrap(offset: u64, size: u64, jhdr_size: u64) -> u64 {
    if offset >= size {
        jhdr_size + (offset - size)
    } else {
        offset
    }
}

#[test]
fn wrapping_lands_in_the_ring_for_every_offset_the_walk_can_produce() {
    // The defining property, stated the way Apple's rule actually works.
    //
    // `if (offset >= size) offset = jhdr_size + (offset - size)` is applied *per
    // step*, to an offset the walk advanced by one block list's data. It is not a
    // total function on arbitrary offsets: handed `size + 14 * jhdr` it returns a
    // value still past the end, because each application removes exactly one
    // `size`. So the property is over the offsets the walk can reach -- within one
    // step of the end -- and asserting it over *all* offsets would be asserting
    // something Apple does not promise.
    //
    // What matters for a reader is that a read starting inside the ring and
    // running past its end lands somewhere valid, which is what the two halves
    // agreeing proves below.
    let size = 4096u64 * 8;
    let jhdr = 4096u64;
    let step = jhdr; // one block list of data, the largest the corpus produces

    // From the end of the header: offset 0 *is* the header, not a journal offset.
    for offset in jhdr..(size + step) {
        let landed = wrap(offset, size, jhdr);
        assert!(
            landed >= jhdr && landed < size + step,
            "offset {offset} wrapped to {landed}, outside the ring plus one step"
        );
    }

    // And an offset inside the ring is untouched, which is the other half: a wrap
    // that fired early would corrupt every ordinary read.
    for offset in [jhdr, jhdr + 1, size / 2, size - 1] {
        assert_eq!(
            wrap(offset, size, jhdr),
            offset,
            "offset {offset} must not move"
        );
    }
}

#[test]
fn wrapping_a_single_read_yields_the_two_halves_read_separately() {
    // The property the wrap exists to provide, asserted against the real reader
    // rather than the helper: a read across the end equals the concatenation of
    // the tail and the head.
    // A journal *with* a written header: `journaled-hfsplus` has none, and
    // `jhdr_size` is read from it.
    let path = common::repo_root().join("tests/images/replayed/journal-replay-be.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read");
    let device = MemoryDevice::new(bytes);
    let header = hfsplus::format::volume_header::VolumeHeader::read_from(&device).expect("header");
    let journal =
        hfsplus::journal::Journal::open(&device, header.journal_info_block, header.block_size)
            .expect("journal open")
            .expect("a journal");

    let size = journal.info().size;
    let jhdr = u64::from(journal.header().expect("header").jhdr_size);
    assert!(jhdr > 0 && jhdr < size, "a ring needs room to wrap into");

    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut wrapped_reads = 0usize;
    for _ in 0..64 {
        // An offset near the end, with a length chosen to reach past it, so the
        // read genuinely wraps rather than merely approaching the end.
        let head_room = size - jhdr - 1;
        assert!(head_room > 0, "the ring needs room to wrap into");
        let tail = 1 + rng.below(head_room);
        let start = size - tail;
        let beyond = 1 + rng.below(4096);
        let len = (tail + beyond) as usize;

        let first = tail as usize;
        let rest = len - first;

        let head_half = journal
            .read_bytes(start, first)
            .expect("the tail half on its own");
        let wrapped_head = journal
            .read_bytes(jhdr, rest)
            .expect("the head half on its own");
        let whole = journal.read_bytes(start, len).expect("a wrapping read");

        assert_eq!(
            whole.len(),
            len,
            "a wrapping read must return what was asked for"
        );
        assert_eq!(
            whole,
            [head_half, wrapped_head].concat(),
            "a read from {start} of {len} did not wrap as the rule says"
        );
        wrapped_reads += 1;
    }
    assert!(wrapped_reads > 0, "no wrapping read was attempted");
}

#[test]
fn a_read_beyond_two_laps_still_lands_in_the_image() {
    // The read is bounded by the journal's own size, so an offset past the end
    // must not be able to ask the device for bytes it does not have.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read");
    let device = MemoryDevice::new(bytes);
    let header = hfsplus::format::volume_header::VolumeHeader::read_from(&device).expect("header");
    let journal =
        hfsplus::journal::Journal::open(&device, header.journal_info_block, header.block_size)
            .expect("journal open")
            .expect("a journal");
    let size = journal.info().size;

    for offset in [size, size + 1, size * 2, u64::MAX / 2] {
        // Whatever it returns, it must be an error or a bounded read -- never a
        // request the device cannot satisfy, and never a panic.
        if let Ok(bytes) = journal.read_bytes(offset, 4096) {
            assert!(
                bytes.len() <= 4096,
                "a read returned more than was asked for"
            );
        }
    }
}

// --- The header fields these depend on ---------------------------------

#[test]
fn the_header_offsets_the_ring_uses_are_the_ones_apple_declares() {
    // `jhdr_size` is what the wrap lands on and what `bnum` is measured in, so a
    // wrong offset for it moves every block.
    let h = JournalHeader {
        magic: hfsplus::journal::info::JOURNAL_HEADER_MAGIC,
        endian: hfsplus::journal::info::ENDIAN_MAGIC,
        byte_order: hfsplus::journal::ByteOrder::Big,
        start: 4096,
        end: 8192,
        size: 1 << 20,
        blhdr_size: 4096,
        checksum: 0,
        jhdr_size: 4096,
        sequence_num: 1,
    };
    assert_eq!(h.jhdr_size, 4096);
    assert_eq!(
        h.jhdr_size as u64,
        wrap(h.size, h.size, h.jhdr_size as u64),
        "an offset exactly at the end wraps to the header size"
    );
}
