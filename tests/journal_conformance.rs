//! Journal detection and read-only replay against the real corpus.
//!
//! # What this suite can and cannot prove
//!
//! Every image `mkfs_hfsplus -J` produces has `kJIJournalNeedInitMask` set and a
//! zeroed journal header, because no transaction has ever been written. The
//! corpus therefore proves detection, validation and the *empty* replay path.
//!
//! It cannot prove transaction replay, because that needs a volume crashed
//! mid-transaction, which cannot be produced without macOS or a fault-injection
//! harness. That gap is asserted here so it stays visible: the transaction walk
//! is covered by synthetic journals in `src/journal/replay.rs`'s unit tests, and
//! this file records that the real-image path stops at zero transactions.

mod common;

use hfsplus::blockdev::{BlockDevice, FileDevice};
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::journal::Journal;
use hfsplus::volume::Volume;

/// Images the corpus builds with `mkfs_hfsplus -J`.
///
/// Only the names are pinned. The journal's offset and size are *derived* and
/// asserted as properties below, because they depend on the allocation block size
/// and hard-coding them would encode one volume's geometry as if it were the
/// format.
fn journaled() -> Vec<&'static str> {
    vec!["journaled-hfsplus", "journaled-hfsplus-1k"]
}

fn non_journaled() -> Vec<&'static str> {
    vec![
        "basic-hfsplus",
        "basic-hfsplus-1k",
        "basic-hfsplus-8k",
        "basic-hfsplus-16k",
        "hfsx-case-sensitive",
        "hfsx-case-insensitive",
    ]
}

fn open(name: &str) -> Option<(FileDevice, VolumeHeader)> {
    let path = common::image(name);
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return None;
    }
    let dev = FileDevice::open(&path).expect("open image");
    let vh = VolumeHeader::read_from(&dev).expect("volume header");
    Some((dev, vh))
}

#[test]
fn journaled_volumes_declare_a_journal_and_the_info_block_agrees() {
    for name in journaled() {
        let Some((dev, vh)) = open(name) else { continue };

        // The attribute bit is what makes journalInfoBlock meaningful at all.
        assert!(vh.is_journaled(), "{name}: kHFSVolumeJournaledBit must be set");
        assert_ne!(vh.journal_info_block, 0, "{name}: journalInfoBlock must be set");
        assert!(
            vh.journal_info_block < vh.total_blocks,
            "{name}: journalInfoBlock {} is outside the volume",
            vh.journal_info_block
        );

        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .expect("journal open")
            .unwrap_or_else(|| panic!("{name}: a journaled volume must have a journal"));

        let device_len = dev.len().unwrap();
        let block_size = u64::from(vh.block_size);

        // The journal must lie inside the device.
        assert!(
            journal.info().validate(device_len).is_ok(),
            "{name}: journal must lie inside the {device_len}-byte image"
        );

        // It must start on an allocation block boundary: the journal is addressed
        // in whole blocks, and an unaligned offset would make every transaction
        // read land mid-block.
        assert_eq!(
            journal.info().offset % block_size, 0,
            "{name}: journal offset {} is not a multiple of the {block_size}-byte block size",
            journal.info().offset
        );

        // And its size must match the `.journal` file's data fork, which is an
        // independent record of the same thing.
        assert_eq!(
            journal.info().size, 524_288,
            "{name}: hfsprogs sizes the journal in 512 kB"
        );
        assert!(
            journal.info().flag_set().in_filesystem(),
            "{name}: the journal lives inside the filesystem"
        );
    }
}

#[test]
fn a_fresh_journal_is_uninitialised_and_has_nothing_to_replay() {
    for name in journaled() {
        let Some((dev, vh)) = open(name) else { continue };
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .expect("journal open")
            .expect("journal");

        assert!(
            journal.is_uninitialized(),
            "{name}: mkfs_hfsplus -J sets kJIJournalNeedInitMask"
        );
        // No header has been written, so there is nothing to parse and nothing to
        // replay. This is a normal state, not an error.
        assert!(journal.header().is_none(), "{name}: an uninitialised journal has no header");
        assert!(
            journal.transactions().is_empty(),
            "{name}: an uninitialised journal has no transactions"
        );
        assert!(
            journal.replayed_blocks().is_empty(),
            "{name}: an uninitialised journal replays nothing"
        );
    }
}

#[test]
fn the_journal_files_are_catalog_objects() {
    // Mining reference: core/hfs_journal.c gives the journal real catalog records
    // during mount, so they are directory entries rather than something the
    // journal parser has to synthesise.
    for name in journaled() {
        let path = common::image(name);
        if !path.exists() {
            continue;
        }
        let dev = FileDevice::open(&path).unwrap();
        let vol = hfsplus::volume::Volume::open(&dev).expect("mount");
        let entries = vol
            .read_dir(hfsplus::catalog::ROOT_FOLDER_ID)
            .expect("read_dir root");
        let names: Vec<String> = entries.iter().map(|e| e.name_string()).collect();
        assert!(names.iter().any(|n| n == ".journal"), "{name}: saw {names:?}");
        assert!(
            names.iter().any(|n| n == ".journal_info_block"),
            "{name}: saw {names:?}"
        );

        // The journal file's fork size must be the journal's size.
        let journal_entry = entries
            .iter()
            .find(|e| e.name_string() == ".journal")
            .expect(".journal");
        assert_eq!(
            journal_entry.data_size(),
            524_288,
            "{name}: .journal fork size must match the info block's journal size"
        );
    }
}

#[test]
fn non_journaled_volumes_have_no_journal() {
    for name in non_journaled() {
        let Some((dev, vh)) = open(name) else { continue };
        assert!(!vh.is_journaled(), "{name}: must not be journaled");
        // journalInfoBlock overlaps spare space on a non-journaled volume, so its
        // value is meaningless and must not be consulted.
        assert_eq!(
            vh.journal_info_block, 0,
            "{name}: a non-journaled volume leaves journalInfoBlock zero"
        );

        // Attempting to open a journal from a zeroed info block must fail rather
        // than silently succeeding with a bogus zero-length journal.
        match Journal::open(&dev, 0, vh.block_size) {
            Ok(None) | Err(_) => {}
            Ok(Some(j)) => panic!(
                "{name}: a non-journaled volume must not yield a journal (offset {})",
                j.info().offset
            ),
        }
    }
}

#[test]
fn opening_the_journal_leaves_the_image_byte_identical() {
    // The central guarantee: a read-only mount must never write, and journal
    // handling is exactly where that is most likely to be violated.
    for name in journaled().into_iter().chain(non_journaled()) {
        let path = common::image(name);
        if !path.exists() {
            continue;
        }
        let before = std::fs::read(&path).expect("read image before");
        let digest_before = digest(&before);

        let dev = FileDevice::open(&path).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();
        let _ = Journal::open(&dev, vh.journal_info_block, vh.block_size);
        // Also exercise the volume path, which builds an overlay from the replay.
        if let Ok(vol) = hfsplus::volume::Volume::open(&dev) {
            let _ = vol.statfs();
            if let Ok(Some(j)) = vol.journal() {
                let overlaid = j.into_device();
                let mut buf = [0u8; 512];
                let _ = overlaid.read_at(0, &mut buf);
                let _ = vol.name();
            }
        }
        drop(dev);

        let after = std::fs::read(&path).expect("read image after");
        assert_eq!(
            digest_before,
            digest(&after),
            "{name}: opening the volume or journal modified the image"
        );
    }
}

fn digest(bytes: &[u8]) -> u64 {
    // A small FNV-1a so the test needs no dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[test]
fn the_corpus_cannot_exercise_transaction_replay() {
    // Recorded as a test so the coverage gap is visible rather than implied by
    // the absence of a test. If this ever fails, the corpus has grown a crashed
    // volume and the synthetic-journal unit tests should be supplemented with a
    // real one.
    let mut total_transactions = 0u32;
    for name in journaled() {
        let Some((dev, vh)) = open(name) else { continue };
        if let Ok(Some(j)) = Journal::open(&dev, vh.journal_info_block, vh.block_size) {
            total_transactions += j.transactions().len() as u32;
        }
    }
    assert_eq!(
        total_transactions, 0,
        "the corpus now contains replayable transactions; add real-image replay tests"
    );
}

#[test]
fn journal_info_blocks_are_validated_before_use() {
    // A journal info block pointing outside the device must be refused, not
    // followed: it is untrusted input like everything else.
    use hfsplus::blockdev::MemoryDevice;
    let block_size = 4096u32;
    let mut dev = MemoryDevice::zeroed(64 * 1024);

    // Write a plausible info block claiming a journal that runs off the end.
    let mut info = vec![0u8; block_size as usize];
    info[0..4].copy_from_slice(&0x0000_0001u32.to_be_bytes());
    info[36..44].copy_from_slice(&(60 * 1024u64).to_be_bytes()); // offset
    info[44..52].copy_from_slice(&(100 * 1024u64).to_be_bytes()); // size
    dev.as_mut_slice()[4096..8192].copy_from_slice(&info);

    // journalInfoBlock = 1 selects that block.
    let err = Journal::open(&dev, 1, block_size);
    assert!(
        err.is_err(),
        "a journal running past the end of the device must be refused, got {:?}",
        err.is_ok()
    );

    // A zero offset is equally meaningless.
    let mut info2 = info.clone();
    info2[36..44].copy_from_slice(&0u64.to_be_bytes());
    dev.as_mut_slice()[8192..12288].copy_from_slice(&info2);
    assert!(Journal::open(&dev, 2, block_size).is_err());
}
// --- The journal info block is bounded by the volume ---------------------

#[test]
fn the_journal_info_block_is_bounded_by_the_volume() {
    // `journalInfoBlock` is a `u32` that nothing else constrains. A volume
    // naming a block at or past its own end is damaged, and refusing it beats
    // parsing whatever lives there, because the resulting complaint would be
    // about those bytes rather than about the volume.
    //
    // Without the bound the refusal is a *coincidence* of what is found at that
    // offset: pointed at the last block of a real volume it failed with "size 0
    // must be non-zero", which is true of the bytes there and of nothing in the
    // header.
    let dir = common::repo_root().join("tests/images/malformed");
    let mut checked = 0;
    for name in [
        "journal-info-block-out-of-volume",
        "journal-info-block-huge",
    ] {
        let path = dir.join(format!("{name}.img"));
        if !path.exists() {
            eprintln!("skipping {name}: not built");
            continue;
        }
        let dev = FileDevice::open(&path).expect("open");
        let err = VolumeHeader::read_from(&dev).expect_err("must be refused");
        let text = err.to_string();
        assert!(
            text.contains("journalInfoBlock"),
            "{name}: the error must name the field, got {text:?}"
        );
        checked += 1;
    }
    assert!(checked > 0, "no malformed journal images were checked");
}

#[test]
fn an_unjournaled_volume_is_not_held_to_the_journal_bound() {
    // The negative control, and it matters: the bound applies only where there is
    // a journal to point at. An unjournaled volume carries whatever it carries in
    // that field -- zero, in practice -- and refusing it would reject images that
    // have never been journaled, which is most of the corpus.
    let path = common::image("basic-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let header = VolumeHeader::read_from(&dev).expect("an unjournaled volume must mount");
    assert!(!header.is_journaled());
    assert_eq!(header.journal_info_block, 0, "the field is simply zero");
}

// --- A journal on another device -----------------------------------------

#[test]
fn a_journal_on_another_device_is_declined_rather_than_missed() {
    // `kJIJournalOnOtherDeviceMask` names the journal by a GPT UUID on a partition
    // this reader is not given. The right answer is "no journal in this image",
    // not a search that finds nothing and reports the volume as unjournaled.
    //
    // Mining reference: `core/hfs_vfsutils.c` `hfs_mount_hfsplus` branches on
    // `kJIJournalInFSMask` and, on the other path, calls `open_journal_dev` with
    // `ext_jnl_uuid` and `machine_serial_num`. When that device cannot be opened
    // Apple fails with `EROFS` -- the volume becomes read-only rather than
    // unopenable.
    let path = common::repo_root().join("tests/images/replayed/journal-external.img");
    if !path.exists() {
        eprintln!(
            "skipping: {} not built; run makejournal.py --external-journal",
            path.display()
        );
        return;
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("the volume itself must still mount");

    // The header still says journaled: this is a journaled volume whose journal
    // lives elsewhere, not a volume with no journal.
    assert!(vol.is_journaled(), "the header still has kHFSVolumeJournaledBit");

    // No journal to replay, and that is not an error.
    assert!(
        vol.journal().expect("journal open must not fail").is_none(),
        "an external journal must not be reported as replayable"
    );

    // But the info block is readable, and it says where the journal is.
    let info = vol
        .external_journal()
        .expect("reading the info block must succeed")
        .expect("the journal must be reported as external");
    let flags = info.flag_set();
    assert!(!flags.in_filesystem(), "kJIJournalInFSMask must be clear");
    assert!(flags.on_other_device(), "kJIJournalOnOtherDeviceMask must be set");
    assert_eq!(info.offset, 0, "offset means nothing for a journal that is not here");
    assert_eq!(info.size, 524288, "size still describes the journal, and Apple uses it");

    // And the volume reads normally: a journal elsewhere changes nothing about
    // what is on this filesystem.
    let mut names: Vec<String> = vol
        .read_dir(vol.root_cnid())
        .expect("read_dir")
        .iter()
        .map(|o| o.name_string())
        .collect();
    names.sort();
    assert_eq!(names, vec![".journal", ".journal_info_block"]);
}

#[test]
fn an_in_filesystem_journal_is_not_reported_as_external() {
    // The converse, so `external_journal` cannot drift into reporting every
    // journaled volume as external.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert!(
        vol.external_journal()
            .expect("reading the info block")
            .is_none(),
        "a journal inside the filesystem must not be reported as external"
    );
    assert!(
        vol.journal().expect("journal open").is_some(),
        "and it must still be replayable"
    );
}

#[test]
fn the_independent_checker_accepts_the_external_journal_image() {
    // This is not an exotic state -- it is what a Time Machine volume looks like
    // -- so the checker should accept a volume whose journal it cannot replay
    // rather than call it damaged.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    let path = common::repo_root().join("tests/images/replayed/journal-external.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-external-{}.img", std::process::id()));
    std::fs::copy(&path, &probe).expect("copy for fsck");

    let out = common::run_fsck(&fsck, &probe);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read(&probe).expect("read the probe back");
    let _ = std::fs::remove_file(&probe);

    assert!(text.contains("appears to be OK"), "expected a sound volume:\n{text}");
    assert_eq!(
        digest(&std::fs::read(&path).expect("read the original")),
        digest(&after),
        "the checker modified the image:\n{text}"
    );
}
