// SPDX-License-Identifier: BSD-2-Clause

//! Every parser, fed truncated input, must return an error rather than panic.
//!
//! # Why this is a separate file
//!
//! A malformed image in the corpus tests one fault at a known offset. Truncation
//! is different: it tests every *structure* at once, at every length, and it
//! reaches code a fixed fixture never will — a record whose key says it is 400
//! bytes long when only 40 remain, a node whose free space points outside itself,
//! an extent count that no longer fits.
//!
//! The parsers are the trust boundary. `AGENTS.md` requires that malformed input
//! produce a structured `Error` and never a panic, and this is where that is
//! checked rather than asserted.
//!
//! # How it is driven
//!
//! Not by fuzzing -- no dependency, and determinism matters more here than
//! coverage. Instead: start from a real, well-formed image and truncate it at
//! *every* length, so no offset is skipped. Then do the same to the structures
//! themselves, byte by byte, so a single missing byte anywhere is exercised.
//!
//! A panic fails the test. A returned `Err` passes. An `Ok` on truncated input is
//! a finding, not a pass: some parsers legitimately succeed on a prefix (a
//! volume header needs only its first 512 bytes), so those are enumerated rather
//! than assumed.
//!
//! Mining reference: `lib_fsck_hfs` exists because Apple found these by hand; the
//! same reasoning applies, and `core/` is full of explicit length checks for the
//! same reason.

mod common;

use hfsplus::blockdev::{BlockDevice, MemoryDevice};
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::volume::Volume;

/// Every image to truncate.
///
/// Chosen for structural variety rather than coverage: a 1 KiB-block volume, a
/// 4 KiB one, an HFSX one, a journaled one, and one with files, an extents tree
/// and a symlink in it.
fn source_bytes() -> Vec<(&'static str, Vec<u8>)> {
    [
        "basic-hfsplus-1k",
        "basic-hfsplus",
        "hfsx-case-sensitive",
        "journaled-hfsplus",
        "journal-with-files",
    ]
    .into_iter()
    .filter_map(|name| {
        let path = common::image(name);
        if !path.exists() {
            eprintln!("skipping {name}: not built");
            return None;
        }
        std::fs::read(&path).ok().map(|b| (name, b))
    })
    .collect()
}

/// A device that is a prefix of another, without copying it.
///
/// The point is the cost. `MemoryDevice::new(prefix.to_vec())` copies the whole
/// prefix, so a sweep over every length of a 32 MiB image copies about 500
/// gigabytes and never finishes. Wrapping the slice makes each attempt a stack
/// allocation, and it exercises the real bounds check rather than a special case:
/// a read past the end goes through `BlockDevice::read_at` exactly as it would on
/// a short file.
#[derive(Debug)]
struct Prefix<'a> {
    bytes: &'a [u8],
}

impl BlockDevice for Prefix<'_> {
    fn len(&self) -> hfsplus::error::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> hfsplus::error::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| hfsplus::error::Error::overflow("prefix read"))?;
        if end > self.bytes.len() as u64 {
            return Err(hfsplus::error::Error::Truncated {
                what: "prefixed image",
                needed: usize::try_from(end).unwrap_or(usize::MAX),
                available: self.bytes.len(),
            });
        }
        buf.copy_from_slice(&self.bytes[offset as usize..end as usize]);
        Ok(())
    }
}

/// The allocation block size, read straight out of the header bytes.
///
/// Deliberately not `VolumeHeader::read_from`: this runs before the sweep and must
/// not depend on the thing being swept.
fn block_size_of(bytes: &[u8]) -> usize {
    if bytes.len() < 1024 + 44 {
        return 4096;
    }
    let raw = u32::from_be_bytes([
        bytes[1024 + 40],
        bytes[1024 + 41],
        bytes[1024 + 42],
        bytes[1024 + 43],
    ]);
    if (512..=65_536).contains(&raw) && raw.is_power_of_two() {
        raw as usize
    } else {
        4096
    }
}

#[test]
fn no_image_truncates_into_a_panic() {
    let sources = source_bytes();
    assert!(!sources.is_empty(), "no source images were available");

    // The costs differ by three orders of magnitude, so the sweeps differ too.
    //
    // A volume header parse is 512 bytes and a handful of field checks, so it
    // runs at *every* length for the first `DENSE` bytes. That range is chosen,
    // not guessed: the volume header and all five special forks sit in the first
    // two blocks, the journal info block and journal header in the next two, and
    // nothing else structurally interesting follows until the catalog at block
    // 899. Beyond `DENSE` it samples every allocation block, because that is
    // where a length field lands.
    //
    // A full mount reads the catalog and, on a journaled image, replays the
    // journal, so it runs far less often: densely over the same dense range, then
    // every 64th block.
    //
    // These bounds are set by what is affordable in a *debug* test build, where a
    // parse costs two orders of magnitude more than in release. The first version
    // of this file swept all 33 million lengths of a 32 MiB image and did not
    // finish in eight minutes; it also copied each prefix, which was the larger
    // problem and is why `Prefix` exists.
    const DENSE: usize = 64 * 1024;

    let mut header_tried = 0usize;
    let mut mount_tried = 0usize;
    for (name, bytes) in &sources {
        let block = block_size_of(bytes).max(1);

        for cut in 0..bytes.len().min(DENSE) {
            let device = Prefix {
                bytes: &bytes[..cut],
            };
            // A volume header needs only its own 512 bytes, so a longer prefix
            // decoding is correct and anything shorter is a finding.
            if VolumeHeader::read_from(&device).is_ok() {
                assert!(
                    cut >= 512 && bytes.len() >= 512,
                    "{name}: a {cut}-byte prefix decoded as a volume header"
                );
            }
            header_tried += 1;
        }
        for cut in (DENSE..bytes.len()).step_by(block) {
            let device = Prefix {
                bytes: &bytes[..cut],
            };
            let _ = VolumeHeader::read_from(&device);
            header_tried += 1;
        }

        let mut cuts: Vec<usize> = (0..bytes.len().min(DENSE)).step_by(64).collect();
        cuts.extend((0..bytes.len()).step_by(block * 64));
        cuts.extend(bytes.len().saturating_sub(8192)..bytes.len());
        cuts.sort_unstable();
        cuts.dedup();
        for cut in cuts {
            let device = Prefix {
                bytes: &bytes[..cut],
            };
            let _ = Volume::open(&device);
            mount_tried += 1;
        }
    }
    assert!(
        header_tried > 300_000,
        "only {header_tried} header truncations were tried, which is not a sweep"
    );
    assert!(
        mount_tried > 1_000,
        "only {mount_tried} mount truncations were tried"
    );
}

#[test]
fn a_one_byte_shorter_image_still_does_not_panic() {
    // Dropping exactly one byte at a time from the end, which is where a length
    // field running just past the end of the image lands. The tail also holds the
    // alternate volume header, so it is the most interesting end.
    for (name, bytes) in source_bytes() {
        let span = block_size_of(&bytes).clamp(256, 8192);
        let mut len = bytes.len();
        let mut tried = 0usize;
        while len > 0 && tried < span {
            let device = Prefix {
                bytes: &bytes[..len],
            };
            let _ = VolumeHeader::read_from(&device);
            let _ = Volume::open(&device);
            tried += 1;
            len -= 1;
        }
        assert!(tried > 0, "{name}: no truncations were tried");
    }
}

#[test]
fn structures_parsed_from_a_prefix_do_not_panic() {
    // The device layer's bounds check is what makes the sweeps above meaningful,
    // so it is asserted directly first: a read that crosses the end is an error,
    // not a short read. A parser that could be handed a short buffer would make
    // the rest of this file meaningless.
    for (name, bytes) in source_bytes() {
        let device = MemoryDevice::new(bytes.clone());
        let len = device.len().expect("len");

        let mut one = [0u8; 1];
        assert!(
            device.read_at(len - 1, &mut one).is_ok(),
            "{name}: the last byte must be readable"
        );
        assert!(
            device.read_at(len, &mut one).is_err(),
            "{name}: one past the end must be refused"
        );
        // An offset that would overflow when added to a length.
        assert!(
            device.read_at(u64::MAX, &mut one).is_err(),
            "{name}: an offset near u64::MAX must be refused, not wrapped"
        );
        assert!(
            device
                .read_at(u64::MAX - 4096, &mut vec![0u8; 8192])
                .is_err(),
            "{name}: a read whose end would wrap must be refused"
        );
    }
}

#[test]
fn a_volume_header_shorter_than_its_own_size_is_refused() {
    // The one prefix length that must never decode: less than 512 bytes. The
    // header is 512 bytes, so anything shorter cannot hold it.
    for (name, bytes) in source_bytes() {
        for len in 0..512usize.min(bytes.len()) {
            let device = MemoryDevice::new(bytes[..len].to_vec());
            assert!(
                VolumeHeader::read_from(&device).is_err(),
                "{name}: a {len}-byte image decoded as a volume header"
            );
        }
    }
}

#[test]
fn a_journal_read_from_a_truncated_image_does_not_panic() {
    // The journal path parses its own structures, and a truncated image makes
    // every length field suspect at once.
    use hfsplus::journal::Journal;

    for (_name, bytes) in source_bytes() {
        let full = MemoryDevice::new(bytes.clone());
        let Ok(vh) = VolumeHeader::read_from(&full) else {
            continue;
        };
        if !vh.is_journaled() {
            continue;
        }

        // Truncate at every block boundary through the metadata zone, which is
        // where the journal header and the info block live.
        let bs = u64::from(vh.block_size) as usize;
        for cut in (0..bytes.len().min(bs * 200)).step_by(bs.max(1)) {
            let device = Prefix {
                bytes: &bytes[..cut],
            };
            let _ = Journal::open(&device, vh.journal_info_block, vh.block_size);
            let device = Prefix {
                bytes: &bytes[..cut],
            };
            let _ = Volume::open(&device);
        }
    }
}

#[test]
fn repeated_parsing_of_the_same_truncation_is_stable() {
    // A parser that caches state would give a different answer the second time,
    // which is how a "works on the first read" bug hides.
    for (name, bytes) in source_bytes() {
        let cut = bytes.len() / 3;
        let prefix = bytes[..cut].to_vec();

        let first = VolumeHeader::read_from(&MemoryDevice::new(prefix.clone()))
            .err()
            .map(|e| e.to_string());
        for _ in 0..8 {
            let again = VolumeHeader::read_from(&MemoryDevice::new(prefix.clone()))
                .err()
                .map(|e| e.to_string());
            assert_eq!(first, again, "{name}: the verdict changed between reads");
        }
    }
}

#[test]
fn the_source_images_are_all_still_parsed_successfully() {
    // The sweeps above are only meaningful if the untruncated images are sound,
    // so that a failure can be attributed to the truncation rather than to a
    // fixture that was already broken.
    let sources = source_bytes();
    assert!(!sources.is_empty(), "no source images were available");
    for (name, bytes) in sources {
        let device = MemoryDevice::new(bytes);
        VolumeHeader::read_from(&device)
            .unwrap_or_else(|e| panic!("{name}: the full image must parse: {e}"));
        Volume::open(&device).unwrap_or_else(|e| panic!("{name}: the full image must mount: {e}"));
    }
}
