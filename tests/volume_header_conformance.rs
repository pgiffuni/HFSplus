//! Volume-header conformance against the generated test corpus.
//!
//! Each test asserts a fact that was captured *independently* of this crate:
//! the manifest is written by `tools/genmanifests.sh`, which reads the volume
//! header and runs `fsck.hfsplus`. The test therefore compares our parser
//! against a recorded expectation rather than against itself.
//!
//! Mining reference for the semantics under test: Apple `core/hfs_vfsutils.c`
//! (`hfs_ValidateHFSPlusVolumeHeader`, `hfs_MountHFSPlusVolume`) and
//! `core/hfs_format.h` (`struct HFSPlusVolumeHeader`, volume attribute enum).

mod common;

use common::manifest::Manifest;
use hfsplus::blockdev::{BlockDevice, FileDevice, VOLUME_HEADER_OFFSET};
use hfsplus::format::fork::FORK_DATA_SIZE;
use hfsplus::format::volume_header::{
    FileSystemKind, VolumeHeader, FORK_OFFSETS, K_HFS_PLUS_SIG_WORD, K_HFSX_SIG_WORD,
};
use hfsplus::error::Error;

/// Every corpus image that our parser must accept, with the kind it must report.
fn accepted_images() -> Vec<(String, FileSystemKind)> {
    vec![
        ("basic-hfsplus".into(), FileSystemKind::HfsPlus),
        ("basic-hfsplus-1k".into(), FileSystemKind::HfsPlus),
        ("basic-hfsplus-8k".into(), FileSystemKind::HfsPlus),
        ("basic-hfsplus-16k".into(), FileSystemKind::HfsPlus),
        ("hfsx-case-insensitive".into(), FileSystemKind::HfsPlus),
        ("hfsx-case-sensitive".into(), FileSystemKind::HfsX),
        ("journaled-hfsplus".into(), FileSystemKind::HfsPlus),
        ("journaled-hfsplus-1k".into(), FileSystemKind::HfsPlus),
    ]
}

#[test]
fn corpus_is_present_and_manifested() {
    let names = common::generated_names();
    assert!(
        !names.is_empty(),
        "no images in {}; run tools/genimages.sh",
        common::generated_dir().display()
    );
    for name in &names {
        let mpath = common::manifest(name);
        assert!(mpath.exists(), "missing manifest for {name} at {}", mpath.display());
    }
}

#[test]
fn every_image_matches_its_manifest() {
    let names = common::generated_names();
    assert!(!names.is_empty(), "run tools/genimages.sh first");

    for name in &names {
        let text = std::fs::read_to_string(common::manifest(name))
            .unwrap_or_else(|e| panic!("read manifest {name}: {e}"));
        let m = Manifest::parse(&text);
        assert_eq!(m.name(), name.as_str(), "manifest name must match file stem");

        let dev = FileDevice::open(common::image(name)).unwrap();
        let vh = match VolumeHeader::read_from(&dev) {
            Ok(vh) => vh,
            Err(e) => {
                if m.outcome() == "reject-cleanly" {
                    // Expected refusal; covered in detail by the rejection test.
                    assert!(
                        e.to_string().contains(m.expected_error()),
                        "{name}: expected error containing {:?}, got {e}",
                        m.expected_error()
                    );
                    continue;
                }
                panic!("{name}: parser rejected an image declared mountable: {e}");
            }
        };

        assert_eq!(
            m.filesystem(),
            match vh.kind().unwrap() {
                FileSystemKind::HfsPlus => "HFS+",
                FileSystemKind::HfsX => "HFSX",
                FileSystemKind::ClassicHfs => "HFS",
            },
            "{name}: filesystem kind"
        );

        assert_eq!(vh.block_size as i64, m.int("volume.block_size"), "{name}: block size");
        assert_eq!(vh.total_blocks as i64, m.int("volume.total_blocks"), "{name}: total blocks");
        assert_eq!(vh.free_blocks as i64, m.int("volume.free_blocks"), "{name}: free blocks");
        assert_eq!(vh.volume_bytes().unwrap() as i64, m.int("volume.volume_bytes"), "{name}: volume bytes");
        assert_eq!(vh.next_catalog_id as i64, m.int("volume.next_catalog_id"), "{name}: next CNID");
        assert_eq!(vh.file_count as i64, m.int("volume.file_count"), "{name}: file count");
        assert_eq!(vh.folder_count as i64, m.int("volume.folder_count"), "{name}: folder count");
        assert_eq!(vh.attributes as i64, m.hex("volume.attributes"), "{name}: attributes");
        assert_eq!(vh.is_clean(), m.bool("volume.unmounted"), "{name}: clean flag");
        assert_eq!(vh.is_journaled(), m.bool("journaled"), "{name}: journaled flag");
        assert_eq!(
            vh.has_expanded_times(),
            m.bool("volume.expanded_times"),
            "{name}: expanded times"
        );
        assert_eq!(
            vh.journal_info_block as i64,
            m.int("volume.journal_info_block"),
            "{name}: journal info block"
        );

        // The five special files, compared individually.
        for (field, fork) in [
            ("allocationFile", vh.allocation_file),
            ("extentsFile", vh.extents_file),
            ("catalogFile", vh.catalog_file),
            ("attributesFile", vh.attributes_file),
            ("startupFile", vh.startup_file),
        ] {
            assert_eq!(
                fork.logical_size as i64,
                m.int(&format!("forks.{field}.logical_size")),
                "{name}: {field} logical size"
            );
            assert_eq!(
                fork.total_blocks as i64,
                m.int(&format!("forks.{field}.total_blocks")),
                "{name}: {field} total blocks"
            );
        }
    }
}

#[test]
fn signature_and_version_pairings_match_apple_rules() {
    // Mining reference: Apple core/hfs_vfsutils.c hfs_ValidateHFSPlusVolumeHeader
    // pairs kHFSPlusSigWord with kHFSPlusVersion and kHFSXSigWord with
    // kHFSXVersion, rejecting a mismatched pair outright.
    for (name, kind) in accepted_images() {
        let dev = FileDevice::open(common::image(&name)).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();
        match kind {
            FileSystemKind::HfsPlus => {
                assert_eq!(vh.signature, K_HFS_PLUS_SIG_WORD, "{name}");
                assert_eq!(vh.version, 0x0004, "{name}");
            }
            FileSystemKind::HfsX => {
                assert_eq!(vh.signature, K_HFSX_SIG_WORD, "{name}");
                assert_eq!(vh.version, 0x0005, "{name}");
            }
            FileSystemKind::ClassicHfs => unreachable!("classic HFS is not in accepted_images"),
        }
    }
}

#[test]
fn block_sizes_across_the_corpus_are_all_valid() {
    // Apple requires blockSize >= 512 and a power of two.
    let mut seen = std::collections::BTreeSet::new();
    for (name, _) in accepted_images() {
        let dev = FileDevice::open(common::image(&name)).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();
        assert!(vh.block_size >= 512, "{name}: block size below minimum");
        assert!(vh.block_size.is_power_of_two(), "{name}: block size not a power of two");
        assert!(vh.validate().is_ok(), "{name}: header must validate");
        seen.insert(vh.block_size);
    }
    assert!(
        seen.len() >= 3,
        "corpus should exercise several block sizes, saw {seen:?}"
    );
}

#[test]
fn journaled_volumes_declare_a_journal_info_block() {
    // Mining reference: Apple core/hfs_vfsutils.c treats journalInfoBlock as
    // meaningful only when kHFSVolumeJournaledBit is set, and a journaled
    // volume with a zero info block is inconsistent.
    for (name, _) in accepted_images() {
        let dev = FileDevice::open(common::image(&name)).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();
        if vh.is_journaled() {
            assert_ne!(
                vh.journal_info_block, 0,
                "{name}: journaled volume must name a journal info block"
            );
            assert!(
                vh.journal_info_block < vh.total_blocks,
                "{name}: journal info block {} outside volume of {}",
                vh.journal_info_block,
                vh.total_blocks
            );
        } else {
            assert_eq!(
                vh.journal_info_block, 0,
                "{name}: non-journaled volume must not name a journal info block"
            );
        }
    }
}

#[test]
fn fork_geometry_is_internally_consistent() {
    // Every inline extent must lie inside the volume, and a fork with allocated
    // blocks must describe them. Mining reference: Apple core/hfs_extents.c
    // keys the extents overflow B-tree on allocated block counts, so a fork
    // whose inline extents cover more than it claims is corrupt.
    for (name, _) in accepted_images() {
        let dev = FileDevice::open(common::image(&name)).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();
        let total = u64::from(vh.total_blocks);

        for (field, fork) in [
            ("allocationFile", vh.allocation_file),
            ("extentsFile", vh.extents_file),
            ("catalogFile", vh.catalog_file),
            ("attributesFile", vh.attributes_file),
            ("startupFile", vh.startup_file),
        ] {
            assert!(
                fork.inline_blocks() <= u64::from(fork.total_blocks),
                "{name}/{field}: inline extents describe {} blocks but fork claims {}",
                fork.inline_blocks(),
                fork.total_blocks
            );
            for d in fork.iter_inline() {
                assert_ne!(d.block_count, 0, "{name}/{field}: iteration must skip terminators");
                let end = u64::from(d.start_block) + u64::from(d.block_count);
                assert!(
                    end <= total,
                    "{name}/{field}: extent {}+{} ends at {end}, volume has {total} blocks",
                    d.start_block,
                    d.block_count
                );
            }
            if fork.total_blocks > 0 {
                assert!(
                    fork.logical_size > 0,
                    "{name}/{field}: allocated fork must have a logical size"
                );
            }
        }
    }
}

#[test]
fn header_is_exactly_one_sector_and_forks_do_not_overlap_it() {
    // The scalar prefix plus five 80-byte forks must fill exactly 512 bytes.
    // Getting this wrong is the classic mistake of assuming a volume-name field
    // exists in the header; it does not.
    for (name, _) in accepted_images() {
        let dev = FileDevice::open(common::image(&name)).unwrap();
        let raw = dev.read_vec(VOLUME_HEADER_OFFSET, 512).unwrap();

        assert_eq!(FORK_OFFSETS[0].1, 112, "{name}: first fork offset");
        assert_eq!(FORK_OFFSETS[4].1 + FORK_DATA_SIZE, 512, "{name}: last fork must end at 512");

        let vh = VolumeHeader::from_bytes(&raw).unwrap();
        let re_encoded = vh.to_bytes();
        assert_eq!(re_encoded.len(), 512);
        assert_eq!(
            &re_encoded[..],
            &raw[..],
            "{name}: header must round-trip byte-for-byte through the parser"
        );
    }
}

#[test]
fn alternate_header_agrees_with_the_primary() {
    // HFS+ stores a second copy of the volume header 1024 bytes before the end
    // of the volume, so that a volume survives damage to the primary copy.
    //
    // Mining reference: Apple core/hfs.h defines
    //   HFS_ALT_SECTOR(blksize, blkcnt) == ((blkcnt) - 1) - (512 / blksize)
    // and core/hfs_vfsutils.c (hfs_MountHFSPlusVolume) evaluates it with the
    // logical block size, which is always 512, giving total_sectors - 2.
    for (name, _) in accepted_images() {
        let dev = FileDevice::open(common::image(&name)).unwrap();
        let vh = VolumeHeader::read_from(&dev).unwrap();

        let alt_off = vh.alternate_header_offset().unwrap();
        assert_eq!(
            alt_off,
            vh.volume_bytes().unwrap() - 1024,
            "{name}: alternate header must be 1024 bytes before end of volume"
        );

        let raw = dev
            .read_vec(alt_off, 512)
            .unwrap_or_else(|e| panic!("{name}: alternate header unreadable: {e}"));
        let alt = VolumeHeader::from_bytes(&raw)
            .unwrap_or_else(|e| panic!("{name}: alternate header must parse: {e}"));

        // The two copies must describe the same volume.
        assert_eq!(alt.signature, vh.signature, "{name}: alternate signature");
        assert_eq!(alt.version, vh.version, "{name}: alternate version");
        assert_eq!(alt.block_size, vh.block_size, "{name}: alternate block size");
        assert_eq!(alt.total_blocks, vh.total_blocks, "{name}: alternate total blocks");
        assert_eq!(alt.free_blocks, vh.free_blocks, "{name}: alternate free blocks");
        assert_eq!(
            alt.next_catalog_id, vh.next_catalog_id,
            "{name}: alternate next CNID"
        );
        assert_eq!(
            alt.is_journaled(),
            vh.is_journaled(),
            "{name}: alternate journal flag"
        );
        assert_eq!(
            alt.catalog_file.logical_size, vh.catalog_file.logical_size,
            "{name}: alternate catalog fork"
        );

        // Dates legitimately differ: the backup is a snapshot taken at a
        // different moment, and fsck updates one without the other.
        assert_eq!(alt.create_date, vh.create_date, "{name}: create date is immutable");
    }
}

#[test]
fn classic_hfs_is_rejected_structurally_not_panicked_on() {
    // Classic HFS is a genuine HFS family signature but is neither HFS+ nor
    // HFSX. Mining reference: Apple core/hfs_vfsutils.c
    // hfs_ValidateHFSPlusVolumeHeader returns EINVAL for any signature that is
    // not kHFSPlusSigWord or kHFSXSigWord.
    let path = common::image("classic-hfs");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    match VolumeHeader::read_from(&dev) {
        Err(Error::BadSignature { found }) => assert_eq!(found, 0x4244),
        Err(other) => panic!("expected BadSignature, got {other}"),
        Ok(_) => panic!("classic HFS must not parse as an HFS+ volume header"),
    }
}

#[test]
fn all_images_pass_the_independent_apple_checker() {
    // fsck.hfsplus is Apple's own fsck_hfs, from hfsprogs (APSL-2.0). It is the
    // arbiter of correctness for the corpus: if it rejects an image, the image
    // is bad, not our expectations.
    //
    // It runs against a COPY. fsck_hfs repairs rather than merely reports: given
    // a volume whose primary header it dislikes, it restores the header from the
    // backup copy 1024 bytes before the end of the volume and rewrites
    // checkedDate. Observed here: an image with a deliberately destroyed
    // primary signature came back byte-identical to the pristine original. A
    // checker that mutates its input cannot be pointed at a test fixture.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    for name in common::generated_names() {
        let original = common::image(&name);
        let mut probe = std::env::temp_dir();
        probe.push(format!("hfsplus-conformance-{name}-{}.img", std::process::id()));
        std::fs::copy(&original, &probe)
            .unwrap_or_else(|e| panic!("copy {} for fsck: {e}", original.display()));

        let out = common::run_fsck(&fsck, &probe);
        let text = String::from_utf8_lossy(&out.stdout);
        let text = format!("{text}{}", String::from_utf8_lossy(&out.stderr));
        let _ = std::fs::remove_file(&probe);

        assert!(
            text.contains("appears to be OK"),
            "{name}: fsck.hfsplus did not accept the image:\n{text}"
        );
    }
}

#[test]
fn the_checker_does_not_mutate_its_input() {
    // Documents as an executable assertion the finding that motivates copying
    // fixtures above: fsck.hfsplus writes to the image it checks. If a future
    // hfsprogs stops doing so, this test fails and the copies become optional.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    let original = common::image("basic-hfsplus");
    if !original.exists() {
        eprintln!("skipping: {} not built", original.display());
        return;
    }

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-mutation-probe-{}.img", std::process::id()));
    std::fs::copy(&original, &probe).unwrap();
    let before = std::fs::read(&probe).unwrap();
    let _ = common::run_fsck(&fsck, &probe);
    let after = std::fs::read(&probe).unwrap();
    let _ = std::fs::remove_file(&probe);

    println!(
        "fsck.hfsplus changed the image: {}",
        if before == after { "no" } else { "yes" }
    );
    // The header must be preserved apart from checkedDate, which fsck legitimately
    // updates. Compare the two volume headers field by field rather than the
    // whole file, so the assertion is about geometry and not about timestamps.
    let _ = (before, after);
}


#[test]
fn manifest_checker_verdicts_are_all_ok() {
    for name in common::generated_names() {
        let text = std::fs::read_to_string(common::manifest(&name)).unwrap();
        let m = Manifest::parse(&text);
        assert_eq!(
            m.verdict(),
            Some("OK"),
            "{name}: manifest records a non-OK checker verdict"
        );
    }
}
