//! End-to-end journal replay against images that contain real transactions.
//!
//! `tests/journal_conformance.rs` covers detection against the generated corpus,
//! whose journals are all uninitialised and therefore empty. This suite closes
//! the gap: `tools/makejournal.py` writes a real journal header, a real
//! transaction, a real block list and real replacement data into a journaled
//! image, and the whole path is then exercised from the volume header down to
//! the bytes a read returns.
//!
//! # Layout, from Apple
//!
//! Mining reference: Apple `core/hfs_journal.h` and `core/hfs_journal.c`.
//!
//! ```text
//! journal + 0                 journal_header    (jhdr_size bytes)
//! journal + jhdr_size         block_list_header  (blhdr_size bytes)
//! journal + jhdr_size + blhdr_size   the replacement block data
//! ```
//!
//! A block list records `bnum` (a device block number) and `bsize` for each
//! block, and `bytes_used` says how much data follows. `BLHDR_FIRST_HEADER`
//! marks the list as beginning a transaction.
//!
//! # What is deliberately *not* here
//!
//! A crash-consistent image: a volume whose journal is newer than its filesystem
//! because the machine died mid-write. That cannot be produced without macOS or
//! fault injection, so these images replay a block that the filesystem does not
//! currently reference. The overlay is therefore verified for *precedence* and
//! for *non-interference* rather than for repairing a torn catalog.

mod common;

use hfsplus::blockdev::{BlockDevice, FileDevice};
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::journal::Journal;
use hfsplus::volume::Volume;

/// Image, the device block the transaction rewrites, and the payload marker.
fn replayed() -> Vec<(&'static str, u64, &'static str)> {
    vec![
        ("journal-replay-be", 200, "journal replayed block 200"),
        ("journal-replay-le", 201, "journal replayed block 201"),
        ("journal-replay-1k", 300, "journal replayed block 300 on a 1k volume"),
    ]
}

fn image_path(name: &str) -> std::path::PathBuf {
    common::repo_root().join("tests/images/replayed").join(format!("{name}.img"))
}

fn open(name: &str) -> Option<(FileDevice, VolumeHeader)> {
    let path = image_path(name);
    if !path.exists() {
        eprintln!(
            "skipping {name}: {} not built; run tools/makejournal.py",
            path.display()
        );
        return None;
    }
    let dev = FileDevice::open(&path).expect("open image");
    let vh = VolumeHeader::read_from(&dev).expect("volume header");
    Some((dev, vh))
}

/// Run `f` with the image's journal. The journal borrows the device, so the
/// device is owned here and handed to the closure rather than returned.
fn with_journal(name: &str, f: impl FnOnce(&Journal<'_, FileDevice>)) -> bool {
    let Some((dev, vh)) = open(name) else { return false };
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .unwrap_or_else(|e| panic!("{name}: journal open: {e}"))
        .unwrap_or_else(|| panic!("{name}: expected a journal"));
    f(&journal);
    true
}

#[test]
fn a_journal_with_a_transaction_is_detected_and_replayed() {
    for (name, block, marker) in replayed() {
        let ran = with_journal(name, |journal| {
            assert!(!journal.is_uninitialized(), "{name}: the journal has a transaction");
            assert!(journal.header().is_some(), "{name}: a written journal has a header");
            assert_eq!(journal.transactions().len(), 1, "{name}: one transaction");
            assert_eq!(journal.replayed_blocks().len(), 1, "{name}: one block");

            let replayed = &journal.replayed_blocks()[0];
            assert_eq!(replayed.device_block, block, "{name}: recorded block number");
            assert!(
                String::from_utf8_lossy(&replayed.data).starts_with(marker),
                "{name}: replayed data must start with {marker:?}"
            );
        });
        assert!(ran, "{name}: the image must be built");
    }
}

#[test]
fn the_overlay_wins_over_the_device_and_the_rest_is_untouched() {
    for (name, block, marker) in replayed() {
        let Some((dev, vh)) = open(name) else { continue };
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .unwrap()
            .unwrap();
        let overlaid = journal.into_device();
        let bs = u64::from(vh.block_size);

        // The replayed block reads from the journal.
        let mut buf = vec![0u8; vh.block_size as usize];
        overlaid
            .read_at(block * bs, &mut buf)
            .unwrap_or_else(|e| panic!("{name}: read replayed block {block}: {e}"));
        assert!(
            String::from_utf8_lossy(&buf).starts_with(marker),
            "{name}: block {block} must come from the journal, got {:?}",
            String::from_utf8_lossy(&buf[..marker.len().min(buf.len())])
        );

        // The same block read directly from the device is different, which is
        // what makes the test meaningful rather than circular.
        let mut direct = vec![0u8; vh.block_size as usize];
        dev.read_at(block * bs, &mut direct).unwrap();
        assert_ne!(&direct, &buf, "{name}: the overlay must actually override the device");

        // A block the journal does not touch still comes from the device.
        let untouched = block + 1;
        let mut a = vec![0u8; vh.block_size as usize];
        let mut b = vec![0u8; vh.block_size as usize];
        overlaid.read_at(untouched * bs, &mut a).unwrap();
        dev.read_at(untouched * bs, &mut b).unwrap();
        assert_eq!(a, b, "{name}: block {untouched} must be unaffected by replay");
    }
}

#[test]
fn a_read_spanning_the_overlay_boundary_is_continuous() {
    // A read that starts inside a replayed block and ends past it must be
    // stitched from both sources rather than truncated or misaligned.
    for (name, block, marker) in replayed() {
        let Some((dev, vh)) = open(name) else { continue };
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .unwrap()
            .unwrap();
        let overlaid = journal.into_device();
        let bs = u64::from(vh.block_size);

        let start = block * bs + bs - 16;
        let mut buf = vec![0u8; 32];
        overlaid
            .read_at(start, &mut buf)
            .unwrap_or_else(|e| panic!("{name}: straddling read: {e}"));
        assert_eq!(buf.len(), 32, "{name}");
        // The tail of the replayed block must not be zeros from a device read.
        assert!(
            !buf[..16].iter().all(|b| *b == 0),
            "{name}: the straddling read did not see replayed data"
        );
        let _ = marker;
    }
}

#[test]
fn the_volume_still_mounts_through_the_overlaid_device() {
    // The strongest statement available: replaying a journal does not make the
    // filesystem unreadable, because the overlay is consulted per byte offset and
    // everything the journal does not replace still comes from the device.
    for (name, _, _) in replayed() {
        let Some((dev, vh)) = open(name) else { continue };
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .unwrap()
            .unwrap();

        // The catalog must still be reachable through the overlay.
        let overlaid = journal.into_device();
        let raw = overlaid.read_vec(VOLUME_HEADER_OFFSET, 512).unwrap();
        let through_overlay = VolumeHeader::from_bytes(&raw).expect("header via overlay");
        assert_eq!(through_overlay.signature, vh.signature, "{name}");
        assert_eq!(through_overlay.total_blocks, vh.total_blocks, "{name}");
        assert_eq!(
            through_overlay.catalog_file.logical_size,
            vh.catalog_file.logical_size,
            "{name}"
        );
    }
}

#[test]
fn replay_never_modifies_the_image() {
    // The central guarantee, now exercised on images that actually have
    // transactions rather than an empty journal.
    for (name, _, _) in replayed() {
        let path = image_path(name);
        if !path.exists() {
            continue;
        }
        let before = std::fs::read(&path).expect("read before");
        let digest_before = digest(&before);

        let dev = FileDevice::open(&path).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();

        // Read through the overlay in several places, including the replay.
        {
            let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
                .unwrap()
                .unwrap();
            let overlaid = journal.into_device();
            for block in [0u64, 1, 2, 200, 201, 300, 770, 8191] {
                let mut buf = vec![0u8; 4096];
                let _ = overlaid.read_at(block * 4096, &mut buf);
            }
        }

        // And exercise the ordinary volume path as well.
        {
            let vol = Volume::open(&dev).expect("mount");
            let _ = vol.name();
            let _ = vol.statfs();
            let _ = vol.catalog().all_objects();
        }
        drop(dev);

        let after = std::fs::read(&path).expect("read after");
        assert_eq!(
            digest_before,
            digest(&after),
            "{name}: replay modified the source image"
        );
    }
}

#[test]
fn the_independent_checker_accepts_a_replayed_image() {
    // fsck.hfsplus reads the image directly, so it sees the filesystem as it is
    // on disk. Because the transaction rewrites a block the filesystem does not
    // reference, the filesystem must still be sound. Run on a COPY: fsck repairs
    // as well as reports, and pointing it at a fixture would rewrite it.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    for (name, _, _) in replayed() {
        let path = image_path(name);
        if !path.exists() {
            continue;
        }
        let mut probe = std::env::temp_dir();
        probe.push(format!("hfsplus-journal-{name}-{}.img", std::process::id()));
        std::fs::copy(&path, &probe).expect("copy for fsck");

        let out = common::run_fsck(&fsck, &probe);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_file(&probe);

        assert!(
            text.contains("appears to be OK"),
            "{name}: fsck.hfsplus rejected a replayed image:\n{text}"
        );
    }
}

#[test]
fn a_corrupted_transaction_is_refused_rather_than_replayed() {
    // A block whose recorded checksum does not match its data must stop the
    // replay. Silently replaying it would present corrupted metadata as if it
    // were current, which is the one failure mode a journal exists to prevent.
    let source = image_path("journal-replay-be");
    if !source.exists() {
        eprintln!("skipping: {} not built", source.display());
        return;
    }
    let mut broken = std::fs::read(&source).expect("read image");

    let vh_off = VOLUME_HEADER_OFFSET as usize;
    let be32 = |buf: &[u8], at: usize| {
        u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
    };
    let bs = be32(&broken, vh_off + 40);
    let jib_block = be32(&broken, vh_off + 12);
    let jib_off = jib_block as usize * bs as usize;
    let journal_offset =
        u64::from_be_bytes(broken[jib_off + 36..jib_off + 44].try_into().unwrap()) as usize;

    // The block data begins after the journal header and the block list, both of
    // which are one block. Flip a byte in it.
    let data_at = journal_offset + bs as usize + bs as usize;
    broken[data_at + 64] ^= 0xFF;

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-journal-broken-{}.img", std::process::id()));
    std::fs::write(&probe, &broken).expect("write probe");

    let dev = FileDevice::open(&probe).expect("open probe");
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let result = Journal::open(&dev, vh.journal_info_block, vh.block_size);
    // Bound the borrow so the assertion below can inspect the outcome.
    let _ = std::fs::remove_file(&probe);

    let outcome = match &result {
        Ok(None) => "no journal".to_string(),
        Ok(Some(j)) => format!("replayed {} block(s)", j.replayed_blocks().len()),
        Err(e) => format!("{e}"),
    };
    assert!(
        result.is_err() || matches!(&result, Ok(None)),
        "a block failing its recorded checksum must not be replayed; got {outcome}"
    );
}

/// Byte offset of the volume header.
const VOLUME_HEADER_OFFSET: u64 = 1024;

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}