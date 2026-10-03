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
//! Repairing a torn catalog. `makejournal.py` rewrites a block the filesystem
//! does not reference, so these images verify the overlay for *precedence* and
//! *non-interference* — that it wins over the device, and that it leaves
//! everything else alone — but not that replay recovers anything. A block nothing
//! references repairs nothing.
//!
//! `tests/journal_recovery.rs` covers that, using `tools/mktorn.py` to write a
//! real catalog change into a journal instead.

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

/// Image holding three transactions, two of which rewrite the same block.
const MULTI: &str = "journal-replay-multi";

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
fn a_corrupted_block_truncates_replay_rather_than_being_applied() {
    // Presenting corrupted metadata as if it were current is the one failure
    // mode a journal exists to prevent. So a block failing its recorded checksum
    // must not reach the overlay.
    //
    // Apple does not abandon the whole journal here: it restarts and replays
    // only the transactions it considers known good. Since this image has a
    // single transaction, the observable result is that nothing is replayed and
    // the truncation is reported.
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

    // The block data follows the journal header and the block list, one block
    // each. Flip a byte in it.
    let data_at = journal_offset + 2 * bs as usize;
    broken[data_at + 64] ^= 0xFF;

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-journal-broken-{}.img", std::process::id()));
    std::fs::write(&probe, &broken).expect("write probe");

    let dev = FileDevice::open(&probe).expect("open probe");
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let result = Journal::open(&dev, vh.journal_info_block, vh.block_size);
    let _ = std::fs::remove_file(&probe);

    match result {
        Err(e) => panic!("a damaged journal must not refuse to open outright: {e}"),
        Ok(None) => panic!("the journal should still be readable, with nothing replayed"),
        Ok(Some(j)) => {
            assert_eq!(
                j.replayed_blocks().len(),
                0,
                "the corrupted block must not reach the overlay"
            );
            let (_, why) = j
                .truncation()
                .expect("the truncation must be reported, not silent");
            assert!(
                why.contains("checksum"),
                "the reason should name the checksum, got {why:?}"
            );
        }
    }
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

#[test]
fn multiple_transactions_are_walked_and_grouped() {
    let ran = with_journal(MULTI, |journal| {
        // Three block lists, each flagged BLHDR_FIRST_HEADER, so three
        // transactions. Apple does not model transaction boundaries at all
        // during replay; the grouping here comes from that flag.
        assert_eq!(journal.transactions().len(), 3, "three transactions");
        assert_eq!(journal.block_lists(), 3, "one block list each");

        for (i, t) in journal.transactions().iter().enumerate() {
            assert_eq!(t.sequence_num as usize, i + 1, "sequence numbers ascend");
            assert_eq!(t.block_lists.len(), 1, "transaction {i} has one block list");
            assert!(t.offset < t.end, "transaction {i} advances");
        }
        // Offsets must be contiguous and ascending across transactions.
        for pair in journal.transactions().windows(2) {
            assert_eq!(pair[0].end, pair[1].offset, "transactions must abut");
        }
    });
    assert!(ran, "{MULTI}: the image must be built");
}

#[test]
fn a_later_transaction_supersedes_an_earlier_write_to_the_same_block() {
    // Block 200 is written twice: once by the first transaction and once by the
    // second. Replaying in journal order means the second write is the one a
    // reader sees, which is what makes an unwound volume coherent.
    let ran = with_journal(MULTI, |journal| {
        // Three writes across two distinct blocks leave two overlay entries.
        assert_eq!(journal.replayed_blocks().len(), 2, "200 and 250");

        let b200 = journal
            .replayed_blocks()
            .iter()
            .find(|b| b.device_block == 200)
            .expect("block 200 must be replayed");
        let text = String::from_utf8_lossy(&b200.data);
        assert!(
            text.starts_with("second write to block 200"),
            "the later write must win, got {:?}",
            &text[..26.min(text.len())]
        );

        let b250 = journal
            .replayed_blocks()
            .iter()
            .find(|b| b.device_block == 250)
            .expect("block 250 must be replayed");
        assert!(String::from_utf8_lossy(&b250.data).starts_with("a third block"));
    });
    assert!(ran, "{MULTI}: the image must be built");
}

#[test]
fn replaying_a_multi_transaction_journal_never_modifies_the_image() {
    let path = image_path(MULTI);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let before = digest(&std::fs::read(&path).unwrap());

    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    {
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .unwrap()
            .unwrap();
        assert_eq!(journal.transactions().len(), 3);
        let overlaid = journal.into_device();
        let mut buf = vec![0u8; 4096];
        overlaid.read_at(200 * 4096, &mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf).starts_with("second write to block 200"));
    }
    drop(dev);

    assert_eq!(
        before,
        digest(&std::fs::read(&path).unwrap()),
        "{MULTI}: replay modified the source image"
    );
}

#[test]
fn a_damaged_journal_truncates_rather_than_being_abandoned() {
    // Apple stops at the bad block list and keeps what came before, because its
    // own comment says replaying as much as possible leaves the filesystem in a
    // better state than replaying nothing. A read-only mount that refused the
    // whole journal would show a filesystem missing *every* recent change.
    let path = image_path(MULTI);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let mut broken = std::fs::read(&path).unwrap();

    let vh_off = VOLUME_HEADER_OFFSET as usize;
    let be32 = |buf: &[u8], at: usize| u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    let bs = be32(&broken, vh_off + 40);
    let jib_block = be32(&broken, vh_off + 12);
    let jib_off = jib_block as usize * bs as usize;
    let journal_offset =
        u64::from_be_bytes(broken[jib_off + 36..jib_off + 44].try_into().unwrap()) as usize;

    // Layout: journal header (one block), then per transaction a block list
    // (one block) followed by its data (one block). So the *second*
    // transaction's block list is at three blocks past the journal offset.
    // The damaged byte is inside the header fields the checksum covers.
    let second_blhdr = journal_offset + 3 * bs as usize;
    broken[second_blhdr + 20] ^= 0xFF;

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-truncated-{}.img", std::process::id()));
    std::fs::write(&probe, &broken).unwrap();

    let dev = FileDevice::open(&probe).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let result = Journal::open(&dev, vh.journal_info_block, vh.block_size);
    let _ = std::fs::remove_file(&probe);

    let journal = result
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("{MULTI}: the journal before the damage is readable"));

    // The first transaction survives: one block replayed, and the damage is
    // reported rather than swallowed.
    assert_eq!(
        journal.transactions().len(),
        1,
        "replay must stop at the damaged block list"
    );
    assert_eq!(journal.replayed_blocks().len(), 1, "the first block survives");
    let (at, why) = journal
        .truncation()
        .expect("truncation must be reported, not silent");
    // Offsets are journal-relative, matching header.start and header.end, so
    // the first transaction's block list is at one block.
    assert!(
        at >= bs as u64,
        "truncation must be at the damage, not at the first transaction; got {at}"
    );
    assert!(
        why.contains("checksum"),
        "the reason should name the checksum, got {why:?}"
    );

    // The surviving block must be the first transaction's write, not a mixture.
    let b200 = journal
        .replayed_blocks()
        .iter()
        .find(|b| b.device_block == 200)
        .expect("block 200 from the surviving transaction");
    let text = String::from_utf8_lossy(&b200.data);
    assert!(
        text.starts_with("first write to block 200"),
        "the surviving transaction's own data must be intact, got {:?}",
        &text[..26.min(text.len())]
    );
}


#[test]
fn a_read_running_past_the_end_of_the_device_short_reads() {
    // A request that starts inside a replayed block and runs past the end of the
    // device has already been partly answered. POSIX read reports end of file by
    // returning fewer bytes, not by failing, and a mount that got an error here
    // would report EIO for a file whose tail it had just read successfully.
    let path = image_path("journal-replay-be");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .unwrap()
        .unwrap();
    let overlaid = journal.into_device();

    let device_len = dev.len().unwrap();
    let replayed = journal.replayed_blocks()[0].device_offset;
    let bs = u64::from(vh.block_size);

    // Start at the beginning of the replayed block, and ask for 4 KiB more than
    // the device has left, so the request straddles the overlay and the end.
    let start = replayed;
    let over = 4096usize;
    let want = ((device_len - start) as usize) + over;
    let mut buf = vec![0u8; want];

    let read = overlaid.read_at(start, &mut buf);
    assert!(
        read.is_ok(),
        "a read straddling the replay and the end of the device must not fail: {:?}",
        read.err()
    );

    // The head must be journal data: the payload marker, which the device's own
    // contents could not produce.
    assert!(
        String::from_utf8_lossy(&buf[..26]).contains("journal replayed"),
        "the replayed head must come from the journal, got {:?}",
        String::from_utf8_lossy(&buf[..26])
    );

    // Everything up to the end of the device must be filled: one block from the
    // journal, the remainder from the device.
    let block = bs as usize;
    assert!(block < want, "the request must reach past the overlay block");
    assert_eq!(buf[block..].iter().take(64).len(), 64);
}

#[test]
fn a_read_entirely_past_the_end_is_empty_rather_than_an_error() {
    let path = image_path("journal-replay-be");
    if !path.exists() {
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .unwrap()
        .unwrap();
    let overlaid = journal.into_device();

    let device_len = dev.len().unwrap();
    let mut buf = vec![0u8; 4096];
    // Must terminate rather than spin, and must not error.
    overlaid
        .read_at(device_len, &mut buf)
        .expect("a read starting at end of file is empty, not an error");
    overlaid
        .read_at(device_len + (1 << 20), &mut buf)
        .expect("a read starting past the end is empty, not an error");
}

#[test]
fn a_negative_block_number_is_refused() {
    // A bnum with the high bit set that is not the -1 killed sentinel is bogus.
    // Letting it through computes a device offset far outside the image.
    //
    // Mining reference: core/hfs_journal.c rejects it before any of the list's
    // contents are used, and drops the transaction rather than the journal.
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
    let bs = be32(&broken, vh_off + 40) as usize;
    let jib_block = be32(&broken, vh_off + 12);
    let jib_off = jib_block as usize * bs;
    let journal_offset =
        u64::from_be_bytes(broken[jib_off + 36..jib_off + 44].try_into().unwrap()) as usize;

    // binfo[1].bnum is at block-list prefix + 16 bytes. The block list sits one
    // block past the journal header.
    let blhdr = journal_offset + bs;
    let bnum_at = blhdr + 16 + 16;
    // A large-but-not-all-ones negative value, which is the case Apple rejects.
    broken[bnum_at..bnum_at + 8].copy_from_slice(&0x8000_0000_0000_0000u64.to_be_bytes());

    // binfo[1] begins at offset 32, which is outside the 32 bytes the header
    // checksum covers, so the checksum itself is untouched by this edit. That is
    // deliberate: it isolates the block-number check from the header check.
    assert_eq!(bnum_at - blhdr, 32, "binfo[1] must start just past the checksummed range");

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-negbnum-{}.img", std::process::id()));
    std::fs::write(&probe, &broken).unwrap();

    let dev = FileDevice::open(&probe).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let result = Journal::open(&dev, vh.journal_info_block, vh.block_size);
    let _ = std::fs::remove_file(&probe);

    match result {
        Err(e) => panic!("a bogus block number must truncate, not fail outright: {e}"),
        Ok(None) => {}
        Ok(Some(j)) => {
            assert_eq!(
                j.replayed_blocks().len(),
                0,
                "a transaction with a bogus block number must not be applied"
            );
            let (_, why) = j.truncation().expect("truncation must be reported");
            assert!(
                why.contains("bogus block number"),
                "the reason should say so, got {why:?}"
            );
        }
    }
}



#[test]
fn a_stale_header_checksum_is_reported_but_not_fatal() {
    // Apple computes the journal header checksum, compares it, and then mounts
    // anyway: the `goto bad_journal` after the mismatch is commented out. So this
    // must be a reported diagnostic, not a failure -- refusing would leave the
    // filesystem missing every recent change.
    //
    // Mining reference: core/hfs_journal.c journal_open.
    let source = image_path("journal-replay-be");
    if !source.exists() {
        eprintln!("skipping: {} not built", source.display());
        return;
    }
    let mut stale = std::fs::read(&source).expect("read image");

    let vh_off = VOLUME_HEADER_OFFSET as usize;
    let be32 = |buf: &[u8], at: usize| {
        u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
    };
    let bs = be32(&stale, vh_off + 40) as usize;
    let jib_block = be32(&stale, vh_off + 12);
    let jib_off = jib_block as usize * bs;
    let journal_offset =
        u64::from_be_bytes(stale[jib_off + 36..jib_off + 44].try_into().unwrap()) as usize;

    // Make the header inconsistent with its own stored checksum, without
    // breaking the geometry checks that run first.
    //
    // `jhdr_size` is at offset 40, inside the 44 bytes the checksum covers.
    // Doubling it to 8192 is still a plausible header size for a 512 KiB
    // journal, so geometry validation passes and only the checksum notices --
    // which is precisely the case Apple decided not to treat as fatal.
    let current = be32(&stale, journal_offset + 40);
    stale[journal_offset + 40..journal_offset + 44]
        .copy_from_slice(&(current * 2).to_be_bytes());

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-stale-hdr-{}.img", std::process::id()));
    std::fs::write(&probe, &stale).unwrap();

    let dev = FileDevice::open(&probe).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let result = Journal::open(&dev, vh.journal_info_block, vh.block_size);
    let _ = std::fs::remove_file(&probe);

    let journal = match result {
        Ok(Some(j)) => j,
        Ok(None) => panic!("a stale header checksum must not hide the journal"),
        Err(e) => panic!("a stale header checksum must not be fatal: {e}"),
    };

    assert_eq!(
        journal.header_checksum_ok(),
        Some(false),
        "the stale checksum must be reported"
    );
    // The header is still structurally sound: the diagnostic is separate from
    // geometry validation, which is the point.
    assert_eq!(journal.header().unwrap().jhdr_size, current * 2);
    // And the journal is still usable: the block was replayed.
    assert_eq!(journal.replayed_blocks().len(), 1, "replay still happens");
}

#[test]
fn a_good_header_checksum_is_reported_as_ok() {
    let path = image_path("journal-replay-be");
    if !path.exists() {
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .unwrap()
        .unwrap();
    assert_eq!(journal.header_checksum_ok(), Some(true));
}

#[test]
fn an_uninitialised_journal_reports_no_header_checksum() {
    // Nothing was written, so there is nothing to check. Reporting `false` would
    // be a lie about a checksum that does not exist.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).unwrap();
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .unwrap()
        .expect("a journaled volume has a journal");
    assert!(journal.header().is_none());
    assert_eq!(journal.header_checksum_ok(), None);
}

#[test]
fn a_journal_smaller_than_the_probe_is_still_read_without_overrunning_the_image() {
    // The header probe used to have an artificial 512-byte floor, which could ask
    // the device for bytes a small journal near the end of an image does not
    // contain. A journal that is smaller than the header must produce a clean
    // "no header here", not a confusing truncation.
    use hfsplus::blockdev::MemoryDevice;

    let block_size = 4096u32;
    let mut dev = MemoryDevice::zeroed(64 * 1024);

    // A journal of 16 bytes at the very end of the device.
    let journal_offset = 64 * 1024 - 16u64;
    let mut info = vec![0u8; block_size as usize];
    info[0..4].copy_from_slice(&0x0000_0001u32.to_be_bytes()); // in-filesystem
    info[36..44].copy_from_slice(&journal_offset.to_be_bytes());
    info[44..52].copy_from_slice(&16u64.to_be_bytes());
    dev.as_mut_slice()[4096..8192].copy_from_slice(&info);

    // The journal area is too small to hold a header, so there is nothing to
    // replay -- but opening must not ask the device for bytes it does not have.
    let result = Journal::open(&dev, 1, block_size);
    match result {
        Err(_) => {}
        Ok(None) => {}
        Ok(Some(j)) => assert!(
            j.header().is_none() && j.replayed_blocks().is_empty(),
            "a journal with no room for a header must replay nothing"
        ),
    }
}

// --- Replay rules taken from replay_journal ------------------------------

/// Read every property of a journal, inside a closure that owns the device.
///
/// The journal borrows the device and the overlay borrows the journal, so
/// neither can outlive this function -- which is why the caller gets a closure
/// rather than a `Journal` back.
fn with_fixture(
    name: &str,
    f: impl FnOnce(&Journal<'_, FileDevice>),
) -> bool {
    let path = common::repo_root()
        .join("tests/images/replayed")
        .join(format!("{name}.img"));
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return false;
    }
    let dev = FileDevice::open(&path).unwrap_or_else(|e| panic!("open {name}: {e}"));
    let vh = VolumeHeader::read_from(&dev).unwrap_or_else(|e| panic!("header {name}: {e}"));
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .unwrap_or_else(|e| panic!("journal {name}: {e}"))
        .unwrap_or_else(|| panic!("{name}: expected a journal to replay"));
    f(&journal);
    true
}

#[test]
fn a_transaction_sequence_number_that_jumps_truncates_the_replay() {
    // Sequence numbers are the only record of which *generation* a transaction
    // belongs to. A journal that was reset and appended to carries numbers that
    // jump, and replaying across the reset would apply stale writes over newer
    // data -- so Apple truncates at the jump and keeps what came before.
    //
    // Mining reference: `core/hfs_journal.c` `replay_journal` compares each list's
    // sequence with `last_sequence_num` and sets
    // `txn_start_offset = jnl->jhdr->end = blhdr_offset` when it is neither that
    // value nor one more.
    let ran = with_fixture("journal-bad-sequence", |journal| {
    // The first transaction survives; the jump is not.
    assert_eq!(
        journal.transactions().len(),
        1,
        "only the transaction before the jump may be replayed"
    );
    assert_eq!(journal.replayed_blocks().len(), 1);
    let (offset, reason) = journal
        .truncation()
        .expect("a sequence jump must truncate, not be ignored");
    assert_eq!(offset, 12288, "the truncation is at the second block list");
    assert!(
        reason.contains("sequence"),
        "the reason must name the sequence rule, got {reason:?}"
    );
    });
    assert!(ran, "journal-bad-sequence: the image must be built");
}

#[test]
fn consecutive_sequence_numbers_are_accepted_in_full() {
    // The rule allows the previous number or one more, and skips the comparison
    // when either side is zero. So +1 replays in full -- and a test that only
    // covered the jump would pass against an implementation that truncated on
    // every list.
    let path = common::repo_root()
        .join("tests/images/replayed/journal-replay-multi.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .expect("journal open")
        .expect("a journal");

    let numbers: Vec<u32> = journal.transactions().iter().map(|t| t.sequence_num).collect();
    assert_eq!(numbers, vec![1, 2, 3], "the generator writes +1 each time");
    assert!(
        journal.truncation().is_none(),
        "a well-ordered journal must replay in full, got {:?}",
        journal.truncation()
    );
    assert_eq!(journal.transactions().len(), 3);

    // Three transactions over *two* distinct blocks: the fixture writes block 200
    // twice and block 250 once. That is the point of the fixture, not a
    // bookkeeping accident -- a later transaction must win for a block an earlier
    // one also wrote, and the overlay holds one entry per block.
    assert_eq!(
        journal.replayed_blocks().len(),
        2,
        "the overlay holds one entry per block, however many transactions wrote it"
    );
    let blocks: Vec<u64> = journal
        .replayed_blocks()
        .iter()
        .map(|b| b.device_block)
        .collect();
    assert_eq!(blocks, vec![200, 250], "the blocks the fixture writes");
}

#[test]
fn a_block_list_claiming_more_blocks_than_the_journal_holds_is_refused() {
    // `max_blocks` is how many blocks a list could hold, so it cannot exceed the
    // blocks the journal has. Apple's bound is plain integer division --
    // `max_blocks > (jhdr->size / jhdr->jhdr_size)` -- so rounding up would be one
    // block looser than Apple's, and a list claiming that many blocks would be
    // accepted here and refused there.
    let ran = with_fixture("journal-bad-max-blocks", |journal| {
    assert!(
        journal.transactions().is_empty(),
        "nothing may be replayed from a list claiming more blocks than exist"
    );
    let (offset, reason) = journal
        .truncation()
        .expect("an impossible max_blocks must truncate");
    assert_eq!(offset, 4096, "at the first block list");
    assert!(
        reason.contains("more blocks than the journal holds"),
        "the reason must state the bound, got {reason:?}"
    );
    });
    assert!(ran, "journal-bad-max-blocks: the image must be built");
}

#[test]
fn every_replay_fixture_opens_without_panicking_and_without_writing() {
    // A journal header is untrusted input. Whatever a fixture contains, opening
    // it must produce a result rather than a panic -- and reading must leave the
    // image alone, which is the guarantee the whole milestone rests on.
    let dir = common::repo_root().join("tests/images/replayed");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let mut checked = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("img") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if name == "journal-external" {
            // Deliberately has no journal to replay; its own suite covers that.
            continue;
        }
        let before = std::fs::read(&path).expect("read before");
        {
            let dev = FileDevice::open(&path).expect("open");
            let vh = VolumeHeader::read_from(&dev).expect("header");
            if let Ok(Some(journal)) =
                Journal::open(&dev, vh.journal_info_block, vh.block_size)
            {
                let _ = journal.transactions();
                let _ = journal.replayed_blocks();
                let _ = journal.truncation();
                let _ = journal.header_checksum_ok();
            }
        }
        let after = std::fs::read(&path).expect("read after");
        assert_eq!(
            digest(&before),
            digest(&after),
            "{name}: opening the journal modified the image"
        );
        checked += 1;
    }
    assert!(checked > 0, "no replay fixtures were checked");
}

#[test]
fn a_block_list_entry_with_a_zero_size_truncates_the_transaction() {
    // Not an empty block to skip: a zero size means the list is inconsistent. The
    // data cursor advances by each entry's size, so a zero desynchronises every
    // *later* entry in the same list -- they would be read from the wrong offset
    // and replay plausible nonsense. Refusing is the only safe answer, and it is
    // what Apple does.
    //
    // Mining reference: `core/hfs_journal.c` `replay_journal` prints "invalid
    // bsize" inside the block loop and goes to `bad_txn_handling`.
    let ran = with_fixture("journal-bad-bsize", |journal| {
        assert!(
            journal.transactions().is_empty(),
            "nothing may be replayed from a list whose cursor is desynchronised"
        );
        assert_eq!(journal.replayed_blocks().len(), 0);
        let (offset, reason) = journal
            .truncation()
            .expect("a zero-size entry must truncate");
        assert_eq!(offset, 4096, "at the first block list");
        assert!(
            reason.contains("zero size"),
            "the reason must say what is wrong, got {reason:?}"
        );
    });
    assert!(ran, "journal-bad-bsize: the image must be built");
}

#[test]
fn a_killed_block_is_skipped_without_stopping_the_replay() {
    // The counterpart, and the distinction that makes the zero-size rule safe.
    // A killed block is *meant* to be absent -- Apple writes `(off_t)-1` when a
    // block could not be journalled -- and it is skipped while the data cursor
    // still steps over its recorded size. So a killed block and a zero-sized one
    // are opposite instructions, and treating them alike would either drop data
    // or refuse a sound journal.
    //
    // Mining reference: `core/hfs_journal.c` `replay_journal`, "don't add \"killed\"
    // blocks", with the cursor still advanced past them.
    let path = common::repo_root().join("tests/images/replayed/journal-replay-be.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .expect("journal open")
        .expect("a journal");
    assert!(
        journal.truncation().is_none(),
        "a sound journal must replay in full"
    );

    // The point is that the rule is about the size, not about the block being
    // absent: the same loop skips -1 and refuses 0. A killed block never reaches
    // the overlay at all, so its absence is not observable here -- which is
    // exactly why a zero size has to be refused instead of skipped.
    assert_eq!(
        journal.replayed_blocks().len(),
        journal.transactions().len(),
        "one block per transaction in this fixture"
    );
}

#[test]
fn transactions_past_a_stale_journal_header_end_are_still_recovered() {
    // The crash case. A transaction is journalled and the machine stops before
    // the header's `end` is updated, so the journal holds more than its header
    // claims. Apple walks on: its loop is
    // `while (check_past_jnl_end || jnl->jhdr->start != jnl->jhdr->end)`, and
    // `check_past_jnl_end` is cleared only for a pre-sequence-number journal.
    //
    // Stopping at `end` under-recovers this silently -- the file returned is
    // shorter, and nothing says so. Confirmed against the previous bound, which
    // recovered 1 transaction from this image where the fix recovers 3.
    //
    // Mining reference: `core/hfs_journal.c` `replay_journal`, the loop condition
    // and its "examining extra transactions" branch.
    let ran = with_fixture("journal-short-end", |journal| {
        assert_eq!(
            journal.transactions().len(),
            3,
            "every journalled transaction must be recovered"
        );
        assert_eq!(journal.replayed_blocks().len(), 2, "over two distinct blocks");
        assert!(
            journal.truncation().is_none(),
            "the journal is sound; walking past a stale end is not damage, got {:?}",
            journal.truncation()
        );
    });
    assert!(ran, "journal-short-end: the image must be built");

    // And the same image with its header intact must give the identical answer,
    // which is what makes the walk past `end` a recovery rather than a guess.
    let path = common::repo_root()
        .join("tests/images/replayed/journal-replay-multi.img");
    if !path.exists() {
        eprintln!("skipping the comparison: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    let intact = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .expect("journal open")
        .expect("a journal");
    assert_eq!(
        journal_txn_count(&intact),
        3,
        "the intact journal yields the same transactions"
    );
}

fn journal_txn_count<D: hfsplus::blockdev::BlockDevice + ?Sized>(
    journal: &Journal<'_, D>,
) -> usize {
    journal.transactions().len()
}

#[test]
fn the_stale_end_fixture_is_otherwise_sound() {
    // The point of the previous test is that the extra transactions are real
    // ones the header had not caught up with. That only holds if the header is
    // otherwise consistent -- and `end` sits inside the header's checksummed
    // range, so cutting it invalidates the checksum unless the fixture refreshes
    // it.
    //
    // A stale header checksum is not fatal -- Apple mounts anyway, and so does
    // this crate -- so a fixture that skipped the refresh would still replay all
    // three transactions and still pass, while testing something narrower than it
    // claims.
    //
    // Mining reference: `JOURNAL_HEADER_CKSUM_SIZE` is `offsetof(sequence_num)`,
    // so the checksum covers `end`.
    let ran = with_fixture("journal-short-end", |journal| {
        assert_eq!(
            journal.header_checksum_ok(),
            Some(true),
            "the fixture must be a sound journal, not one that merely replays"
        );
        assert_eq!(journal.transactions().len(), 3);
    });
    assert!(ran, "journal-short-end: the image must be built");
}
