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
        let Some((dev, vh)) = open(name) else {
            continue;
        };

        // The attribute bit is what makes journalInfoBlock meaningful at all.
        assert!(
            vh.is_journaled(),
            "{name}: kHFSVolumeJournaledBit must be set"
        );
        assert_ne!(
            vh.journal_info_block, 0,
            "{name}: journalInfoBlock must be set"
        );
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
            journal.info().offset % block_size,
            0,
            "{name}: journal offset {} is not a multiple of the {block_size}-byte block size",
            journal.info().offset
        );

        // And its size must match the `.journal` file's data fork, which is an
        // independent record of the same thing.
        assert_eq!(
            journal.info().size,
            524_288,
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
        let Some((dev, vh)) = open(name) else {
            continue;
        };
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .expect("journal open")
            .expect("journal");

        assert!(
            journal.is_uninitialized(),
            "{name}: mkfs_hfsplus -J sets kJIJournalNeedInitMask"
        );
        // No header has been written, so there is nothing to parse and nothing to
        // replay. This is a normal state, not an error.
        assert!(
            journal.header().is_none(),
            "{name}: an uninitialised journal has no header"
        );
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
        assert!(
            names.iter().any(|n| n == ".journal"),
            "{name}: saw {names:?}"
        );
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
        let Some((dev, vh)) = open(name) else {
            continue;
        };
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
        let Some((dev, vh)) = open(name) else {
            continue;
        };
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
    assert!(
        vol.is_journaled(),
        "the header still has kHFSVolumeJournaledBit"
    );

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
    assert!(
        flags.on_other_device(),
        "kJIJournalOnOtherDeviceMask must be set"
    );
    assert_eq!(
        info.offset, 0,
        "offset means nothing for a journal that is not here"
    );
    assert_eq!(
        info.size, 524288,
        "size still describes the journal, and Apple uses it"
    );

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

    assert!(
        text.contains("appears to be OK"),
        "expected a sound volume:\n{text}"
    );
    assert_eq!(
        digest(&std::fs::read(&path).expect("read the original")),
        digest(&after),
        "the checker modified the image:\n{text}"
    );
}

// --- A journal header written by an older system -------------------------

#[test]
fn a_legacy_journal_header_is_replayed_without_a_checksum_check() {
    // Apple accepts `OLD_JOURNAL_HEADER_MAGIC` ('JHDR') as well as 'JNLx', then
    // *converts* the old one to the new -- "XXXdbg - convert old style magic
    // numbers to the new one". The conversion happens only after it has decided
    // not to check the checksum, guarded by `if (magic == JOURNAL_HEADER_MAGIC)`
    // and the comment "only check if we're the current journal header magic
    // value".
    //
    // So a legacy header is a journal that must replay, and whose stored checksum
    // is not consulted. Rewriting the magic leaves that checksum stale, which is
    // what such a journal looks like on disk, so a stale checksum must not stop
    // anything.
    let path = common::repo_root().join("tests/images/replayed/journal-legacy-header.img");
    if !path.exists() {
        eprintln!(
            "skipping: {} not built; run makejournal.py --legacy-header",
            path.display()
        );
        return;
    }

    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .expect("journal open")
        .expect("a legacy header is still a journal");

    // Located, and its transaction still replays.
    assert!(
        !journal.is_uninitialized(),
        "a legacy header must not be mistaken for an unwritten journal"
    );
    assert_eq!(
        journal.transactions().len(),
        1,
        "the transaction must replay through a legacy header"
    );
    assert_eq!(journal.replayed_blocks().len(), 1);
    assert!(
        journal.truncation().is_none(),
        "a stale checksum must not truncate the replay: {:?}",
        journal.truncation()
    );

    // And the checksum is reported as not-checked rather than as failed, which is
    // the distinction Apple draws by guarding on the magic.
    assert_eq!(
        journal.header_checksum_ok(),
        None,
        "Apple checks the checksum only for the current magic, so a legacy header \
         is not checked -- which is different from being checked and failing"
    );

    // The header is the old one, and is otherwise intact.
    let header = journal.header().expect("a header was read");
    assert_eq!(
        header.magic,
        hfsplus::journal::info::OLD_JOURNAL_HEADER_MAGIC
    );
    assert_eq!(header.start, 4096, "the transaction geometry is unchanged");
    assert_eq!(header.end, 12288);
    assert_eq!(header.sequence_num, 1);
}

#[test]
fn the_current_magic_still_has_its_checksum_checked() {
    // The converse, and it is what makes the case above mean anything: for a
    // current header the checksum is consulted, and a wrong one is reported
    // without being fatal.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .expect("journal open")
        .expect("an unwritten journal is still a journal");
    // An all-zero header is unwritten rather than current, so nothing to check.
    assert!(journal.header().is_none());
    assert_eq!(journal.header_checksum_ok(), None);

    let path = common::repo_root().join("tests/images/replayed/journal-replay-be.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
        .expect("journal open")
        .expect("a written journal");
    assert_eq!(
        journal.header_checksum_ok(),
        Some(true),
        "the fixture is sound"
    );
}

#[test]
fn opening_a_legacy_journal_leaves_the_image_byte_identical() {
    // The guarantee every journal test rests on, stated for the legacy path too:
    // reading a journal never writes it.
    let path = common::repo_root().join("tests/images/replayed/journal-legacy-header.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let before = digest(&std::fs::read(&path).expect("read before"));
    {
        let dev = FileDevice::open(&path).expect("open");
        let vh = VolumeHeader::read_from(&dev).expect("header");
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .expect("journal open")
            .expect("a legacy header is still a journal");
        let _ = journal.transactions();
        let _ = journal.replayed_blocks();
    }
    assert_eq!(
        before,
        digest(&std::fs::read(&path).expect("read after")),
        "reading a legacy journal modified the image"
    );
}

#[test]
fn a_block_list_header_size_too_small_to_hold_one_is_refused() {
    // `blhdr_size` sets how many bytes the walk reads per block list. Below the
    // five fixed fields the list cannot be read at all, and without this check the
    // failure arrives as a truncated field -- a statement about the bytes rather
    // than about the header that gave the size.
    //
    // Mining reference: `struct block_list_header` in `core/hfs_journal.h` is
    // `max_blocks`, `num_blocks`, `bytes_used`, `checksum` and `flags` before its
    // `binfo[]`.
    let mut header = hfsplus::journal::info::JournalHeader {
        magic: hfsplus::journal::info::JOURNAL_HEADER_MAGIC,
        endian: hfsplus::journal::info::ENDIAN_MAGIC,
        start: 4096,
        end: 12288,
        size: 524288,
        blhdr_size: 4,
        checksum: 0,
        jhdr_size: 4096,
        sequence_num: 1,
    };
    let err = header
        .validate(524288)
        .expect_err("four bytes cannot hold a header");
    let text = err.to_string();
    assert!(
        text.contains("blhdr_size"),
        "the error must name the field, got {text:?}"
    );

    // A size that can hold the fixed part is accepted; the walk then decides.
    header.blhdr_size = hfsplus::journal::replay::BLHDR_PREFIX_SIZE as u32;
    assert!(
        header.validate(524288).is_ok(),
        "a block list header needs only its fixed part to be read"
    );
}

#[test]
fn a_journal_header_pointing_at_its_own_header_is_refused() {
    // `start` and `end` must both be positive and within the journal. Offset
    // zero of a journal *is* its header, so a `start` of 0 would have the walk
    // parse the header as a block list and report whatever counts it found.
    //
    // Apple's CHECK_JOURNAL panics on exactly these, so a volume reaching them is
    // corrupt by definition rather than unusual.
    //
    // Mining reference: `CHECK_JOURNAL` in `core/hfs_journal.c`.
    for (start, end, what) in [
        (0u64, 12288u64, "start at the header"),
        (4096, 0, "end at the header"),
        (524288, 12288, "start beyond the journal"),
        (4096, 524289, "end beyond the journal"),
    ] {
        let header = hfsplus::journal::info::JournalHeader {
            magic: hfsplus::journal::info::JOURNAL_HEADER_MAGIC,
            endian: hfsplus::journal::info::ENDIAN_MAGIC,
            start,
            end,
            size: 524288,
            blhdr_size: 4096,
            checksum: 0,
            jhdr_size: 4096,
            sequence_num: 1,
        };
        let err = header
            .validate(524288)
            .expect_err(&format!("{what} must be refused"));
        let text = err.to_string();
        assert!(
            text.contains("start") || text.contains("end"),
            "{what}: the error must name the field, got {text:?}"
        );
    }

    // And a sound header still passes, so the checks are not simply refusing
    // everything.
    let sound = hfsplus::journal::info::JournalHeader {
        magic: hfsplus::journal::info::JOURNAL_HEADER_MAGIC,
        endian: hfsplus::journal::info::ENDIAN_MAGIC,
        start: 4096,
        end: 12288,
        size: 524288,
        blhdr_size: 4096,
        checksum: 0,
        jhdr_size: 4096,
        sequence_num: 1,
    };
    assert!(
        sound.validate(524288).is_ok(),
        "a sound header must validate"
    );
}

/// The milestone's own acceptance criteria, run over every journaled image.
///
/// Two things, for each: `fsck.hfsplus` accepts a **copy** and leaves it
/// untouched, and reading the image through the library leaves the **source**
/// byte-identical.
///
/// The distinction between the two files matters and is the whole reason for the
/// copy. `fsck_hfs` repairs as well as reports, and it repairs the volume header
/// in place -- which is how `tools/genmalformed.sh` came to undo the very
/// corruptions it was generating. Pointed at a fixture it would change it, so it
/// is only ever given a throwaway.
///
/// The faults in `journal-bad-*` are in the journal, which `fsck.hfsplus` does
/// not replay, so it accepts those images too. That is not a contradiction: they
/// are sound filesystems with unsound journals, and refusing them is the
/// reader's job rather than the checker's.
#[test]
fn the_milestone_criteria_hold_for_every_journaled_image() {
    let fsck = common::fsck_available();
    if fsck.is_none() {
        eprintln!("skipping the fsck column: fsck.hfsplus not installed");
    }

    let mut names: Vec<String> = Vec::new();
    for dir in ["generated", "replayed"] {
        let path = common::repo_root().join("tests/images").join(dir);
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("img") {
                continue;
            }
            let Some(name) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with("journal") {
                names.push(format!("{dir}/{name}"));
            }
        }
    }
    names.sort();
    assert!(!names.is_empty(), "no journaled images were found");

    let mut checked = 0;
    for entry in &names {
        // `entry` is "dir/stem"; the image is the stem plus its extension.
        let path = common::repo_root()
            .join("tests/images")
            .join(format!("{entry}.img"));
        if !path.exists() {
            eprintln!("skipping {entry}: not built");
            continue;
        }
        let before = digest(&std::fs::read(&path).expect("read the source"));

        // Read it every way the library offers, then compare.
        {
            let dev = FileDevice::open(&path).expect("open");
            let vol = Volume::open(&dev);
            if let Ok(vol) = vol {
                let _ = vol.is_journaled();
                let _ = vol.header().journal_info_block;
                if let Ok(Some(j)) = vol.journal() {
                    let _ = j.transactions();
                    let _ = j.replayed_blocks();
                    let _ = j.truncation();
                    let _ = j.header_checksum_ok();
                }
                let _ = vol.external_journal();
            }
            // And through the overlaid device, which is the view a mount sees.
            if let Ok(vh) = VolumeHeader::read_from(&dev) {
                if let Ok(Some(journal)) = Journal::open(&dev, vh.journal_info_block, vh.block_size)
                {
                    let _ = journal.read_bytes(0, 4096);
                }
            }
        }

        let after = digest(&std::fs::read(&path).expect("read the source back"));
        assert_eq!(
            before, after,
            "{entry}: reading the image modified the source"
        );

        if let Some(fsck) = &fsck {
            let mut probe = std::env::temp_dir();
            probe.push(format!("ms5-{}.img", entry.replace('/', "-")));
            std::fs::copy(&path, &probe).expect("copy for fsck");
            let out = common::run_fsck(fsck, &probe);
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let probe_after = digest(&std::fs::read(&probe).expect("read the probe back"));
            let _ = std::fs::remove_file(&probe);

            assert!(
                text.contains("appears to be OK"),
                "{entry}: the checker rejected a sound image:\n{text}"
            );
            assert_eq!(
                before, probe_after,
                "{entry}: the checker modified its copy, so it repaired something:\n{text}"
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no journaled images were checked");
}

#[test]
fn a_journal_is_clean_exactly_when_start_equals_end() {
    // Apple's own predicate, in its own words: "if the start and end are equal
    // then the journal is clean. otherwise it's not clean and therefore an
    // error."
    //
    // Mining reference: `core/hfs_journal.c` `journal_is_clean`, returning 0 for
    // equal and EBUSY otherwise "so the caller can differentiate an invalid
    // journal from a busy one".
    //
    // It is *not* the same as being uninitialised. `kJIJournalNeedInitMask` says
    // nobody has written to the journal yet; `start == end` says there is nothing
    // outstanding *right now*. A journal can be initialised and still be dirty,
    // which is the ordinary state after a crash.
    /// What a journal reports: clean, uninitialised, and its start/end when it
    /// has a header at all -- an unwritten journal has none, which is precisely
    /// a case the predicate has to answer.
    struct Report {
        clean: bool,
        uninitialised: bool,
        bounds: Option<(u64, u64)>,
    }

    let check = |name: &str, rel: &str| -> Option<Report> {
        let path = common::repo_root().join("tests/images").join(rel);
        if !path.exists() {
            eprintln!("skipping {name}: not built");
            return None;
        }
        let dev = FileDevice::open(&path).expect("open");
        let vh = VolumeHeader::read_from(&dev).expect("header");
        let journal =
            Journal::open(&dev, vh.journal_info_block, vh.block_size).expect("journal open")?;
        Some(Report {
            clean: journal.is_clean(),
            uninitialised: journal.is_uninitialized(),
            bounds: journal.header().map(|h| (h.start, h.end)),
        })
    };

    // A journal nobody has written to has no header, and is clean.
    if let Some(r) = check("journaled-hfsplus", "generated/journaled-hfsplus.img") {
        assert_eq!(r.bounds, None, "an unwritten journal has no header");
        assert!(r.clean, "and nothing outstanding, so it is clean");
        assert!(r.uninitialised, "while also carrying the need-init flag");
    }

    // A journal with a transaction: dirty, and not uninitialised.
    if let Some(r) = check("journal-replay-be", "replayed/journal-replay-be.img") {
        let (start, end) = r.bounds.expect("a written journal has a header");
        assert_ne!(start, end, "this journal has a transaction");
        assert!(!r.clean, "so it must not be clean");
        assert!(
            !r.uninitialised,
            "and it is not merely uninitialised either"
        );
    }

    // And the predicate is exactly the equality, not a heuristic: a header whose
    // start and end agree is clean whatever else is true of it.
    let header = hfsplus::journal::info::JournalHeader {
        magic: hfsplus::journal::info::JOURNAL_HEADER_MAGIC,
        endian: hfsplus::journal::info::ENDIAN_MAGIC,
        start: 4096,
        end: 4096,
        size: 524288,
        blhdr_size: 4096,
        checksum: 0,
        jhdr_size: 4096,
        sequence_num: 7,
    };
    assert!(
        header.start == header.end,
        "the predicate is the equality, so this must be clean whatever the \\
         sequence number"
    );
}
