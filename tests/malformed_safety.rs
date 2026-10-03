//! Malformed images must fail safely, never fatally.
//!
//! Every image in `tests/images/malformed/` is a good volume with a small,
//! surgical corruption. None of them is expected to mount. The requirement is
//! narrower and stricter than "returns an error": it must return a *specific,
//! diagnosable* error, must not panic, must not read out of bounds, and must not
//! loop.
//!
//! # A note on `fsck.hfsplus` and these images
//!
//! The Apple checker accepts several of these images, and that is expected
//! rather than alarming. `fsck_hfs` is a repair tool: it derives the volume's
//! identity from the catalog root directory record (which is why it can still
//! print the volume name when both volume header signatures are destroyed) and
//! its job is to *fix* a volume, not to refuse one. A mount path is stricter:
//! Apple `core/hfs_vfsutils.c` (`hfs_ValidateHFSPlusVolumeHeader`) rejects a
//! header whose signature, version or block size is wrong, and
//! `hfs_MountHFSPlusVolume` returns `EINVAL`.
//!
//! So for this corpus our parser is the oracle: the recorded expectation in
//! `tests/images/malformed/EXPECTATIONS.md` is what Apple would do at mount
//! time, not what fsck would do.

mod common;

use hfsplus::blockdev::{BlockDevice, FileDevice};
use hfsplus::error::Error;
use hfsplus::format::volume_header::VolumeHeader;
use std::path::{Path, PathBuf};

fn malformed_dir() -> PathBuf {
    common::repo_root().join("tests/images/malformed")
}

/// Every malformed image, paired with a substring its error must contain.
///
/// Kept as a table rather than as per-image manifests because these cases are
/// about *error identity*, and a TOML round-trip would hide which assertion is
/// actually being made.
fn cases() -> Vec<(&'static str, &'static str)> {
    vec![
        // Signature is not a member of the HFS family at all.
        ("bad-signature", "unrecognised volume signature"),
        // HFS+ signature carrying the HFSX version number. Apple validates the
        // signature/version pair, so this is a corrupt HFS+ volume, not HFSX.
        ("hfsplus-sig-hfsx-version", "volume version"),
        // blockSize must be a power of two and at least 512.
        ("bad-block-size", "power of two"),
        ("block-size-too-small", "power of two"),
        // Images that end before the header can be read.
        ("truncated-header", "truncated"),
        ("truncated-half-header", "truncated"),
        ("all-zero", "unrecognised volume signature"),
        // journalInfoBlock is a block number and nothing else constrains it, so a
        // volume naming one at or past its own end is damaged. The error must name
        // the field: the bytes found there would otherwise be parsed as a journal
        // info block and produce a complaint about *those* instead.
        ("journal-info-block-out-of-volume", "journalInfoBlock"),
        // A value large enough that the block offset would overflow, told apart
        // from the range check by the same message -- both are refused, and both
        // for the same reason.
        ("journal-info-block-huge", "journalInfoBlock"),
    ]
}


fn image(name: &str) -> PathBuf {
    malformed_dir().join(format!("{name}.img"))
}

/// Run the parser and return the error, asserting that no panic occurred.
fn parse_error(path: &Path) -> Error {
    let dev = FileDevice::open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    match VolumeHeader::read_from(&dev) {
        Ok(_) => panic!("{} was accepted but must be rejected", path.display()),
        Err(e) => e,
    }
}

#[test]
fn malformed_images_are_present() {
    for (name, _) in cases() {
        let p = image(name);
        assert!(
            p.exists(),
            "missing malformed image {}; run tools/genmalformed.sh",
            p.display()
        );
    }
}

#[test]
fn each_malformed_image_produces_its_expected_structural_error() {
    for (name, expected) in cases() {
        let path = image(name);
        if !path.exists() {
            eprintln!("skipping {name}: not built");
            continue;
        }
        let err = parse_error(&path);
        let text = err.to_string();
        assert!(
            text.contains(expected),
            "{name}: expected an error containing {expected:?}, got {text:?}"
        );
    }
}

#[test]
fn no_malformed_image_panics_under_repeated_parsing() {
    // Cheap insurance against a loop or a cached-state bug: parse each image
    // many times. A hang shows up as a test timeout, a panic as a failure.
    for (name, _) in cases() {
        let path = image(name);
        if !path.exists() {
            continue;
        }
        let dev = FileDevice::open(&path).unwrap();
        for i in 0..64 {
            let out = format!("{name} iteration {i}");
            if VolumeHeader::read_from(&dev).is_ok() {
                panic!("{out}: accepted a malformed image");
            }
        }
    }
}

#[test]
fn reading_offsets_outside_the_image_report_truncation() {
    // The block device layer must refuse to read past the end rather than
    // zero-filling, otherwise a short image would decode as a plausible volume.
    let path = image("truncated-header");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let len = dev.len().unwrap();

    // The last byte is readable.
    let mut one = [0u8; 1];
    assert!(dev.read_at(len - 1, &mut one).is_ok());

    // One past the end is not.
    assert!(matches!(dev.read_at(len, &mut one), Err(Error::Truncated { .. })));

    // A huge read is rejected by the bounds check rather than overflowing.
    let mut big = vec![0u8; 4096];
    assert!(matches!(dev.read_at(0, &mut big), Err(Error::Truncated { .. })));

    // An offset near u64::MAX must produce an error, not a wrap-around.
    assert!(dev.read_at(u64::MAX, &mut one).is_err());
    assert!(dev.read_at(u64::MAX - 1024, &mut big).is_err());
}

#[test]
fn empty_and_tiny_devices_are_rejected_structurally() {
    // Mining reference: Apple core/hfs_vfsutils.c hfs_ValidateHFSPlusVolumeHeader
    // returns EINVAL before any field is trusted, so a 0-byte device is an error
    // and not a zeroed volume.
    use hfsplus::blockdev::MemoryDevice;
    for len in [0usize, 1, 512, 1023, 1024, 1500] {
        let dev = MemoryDevice::zeroed(len);
        assert!(
            VolumeHeader::read_from(&dev).is_err(),
            "a {len}-byte device must not parse as a volume"
        );
    }
}

#[test]
fn corrupted_fork_geometry_is_preserved_for_diagnosis_not_hidden() {
    // The catalog-fork images parse at the header level, because their headers
    // are well-formed. The corruption is in the fork's extent records, and the
    // library must make it visible rather than silently clamping it.
    //
    // This is the difference between "returns an error" and "returns an error
    // you can act on": a caller needs the raw values to diagnose a bad image.
    let path = image("catalog-extent-out-of-range");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).expect("header itself is well-formed");

    let first = vh.catalog_file.iter_inline().next().copied().expect("one extent");
    assert_eq!(
        first.start_block, 0xFFFF_0000,
        "the corrupt extent must be reported verbatim, not clamped"
    );
    assert_eq!(first.block_count, 1);

    // The library must not itself decide the extent is valid, but it must not
    // claim the fork is self-consistent either.
    assert!(
        !vh.catalog_file.inline_matches_total()
            || u64::from(first.start_block) + 1 > u64::from(vh.total_blocks),
        "an out-of-range catalog extent must not look self-consistent"
    );
}

#[test]
fn extents_without_a_terminator_do_not_run_past_the_record() {
    // A record whose eight slots all carry a block count has no terminator.
    // Iteration must still stop at slot eight rather than reading past the
    // 64-byte record into the next fork.
    let path = image("extents-no-terminator");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).expect("header itself is well-formed");

    let used = vh.catalog_file.extents.used();
    assert_eq!(
        used, 8,
        "iteration must stop at the end of the fixed-capacity record"
    );
    // Reading past the record must fail rather than return adjacent bytes.
    let bytes = vh.catalog_file.extents.to_bytes();
    assert!(hfsplus::format::extents::ExtentRecord::from_bytes(&bytes[..63]).is_err());
}

#[test]
fn a_volume_larger_than_its_device_is_caught_at_the_alternate_header() {
    // A header can be internally well-formed and still describe a volume that
    // does not fit on the device. `totalBlocks * blockSize` cannot overflow a
    // u64, because both factors are u32, so the arithmetic is safe by
    // construction and the defect has to be caught as a range error instead.
    //
    // Mining reference: Apple core/hfs_vfsutils.c (hfs_MountHFSPlusVolume)
    // computes the alternate header position from `totalBlocks * blockSize` and
    // compares it against the device's real sector count, distinguishing
    // "innocuous spare sectors" from a degenerate filesystem-smaller-than-
    // partition case.
    let path = image("huge-total-blocks");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    let vh = VolumeHeader::read_from(&dev).expect("the header itself is well-formed");

    assert_eq!(vh.total_blocks, u32::MAX);
    let claimed = vh.volume_bytes().expect("u32 * u32 cannot overflow u64");
    let actual = dev.len().unwrap();
    assert!(
        claimed > actual,
        "fixture is mis-built: claimed {claimed} should exceed device {actual}"
    );

    // The alternate header lies beyond the device, so reading it must fail
    // rather than wrap around or return zeros.
    let alt = vh.alternate_header_offset().unwrap();
    assert!(alt > actual, "alternate header offset {alt} should exceed device {actual}");
    assert!(matches!(
        dev.read_vec(alt, 512),
        Err(Error::Truncated { .. })
    ));
}

#[test]
fn the_checker_repairs_rather_than_refuses() {
    // Documents, as an executable assertion, the finding described at the top of
    // this file.
    //
    // `fsck_hfs` is a repair tool. Given a volume whose primary volume header it
    // dislikes, it restores that header from the backup copy 1024 bytes before
    // the end of the volume and rewrites checkedDate. Observed here: an image
    // whose primary signature was destroyed came back byte-identical to the
    // pristine original, and fsck reported "appears to be OK".
    //
    // A mount path must not be so forgiving. Apple core/hfs_vfsutils.c
    // (hfs_ValidateHFSPlusVolumeHeader) returns EINVAL for a header with a bad
    // signature, version or block size.
    //
    // fsck therefore runs against a *copy*, so the canonical fixture survives.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    let path = image("bad-signature");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }

    // The untouched fixture must be rejected by us.
    assert!(
        parse_error(&path)
            .to_string()
            .contains("unrecognised volume signature"),
        "our parser must reject a destroyed primary signature"
    );

    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-repair-probe-{}.img", std::process::id()));
    std::fs::copy(&path, &probe).unwrap();
    let before = std::fs::read(&probe).unwrap();

    let out = common::run_fsck(&fsck, &probe);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read(&probe).unwrap();
    let _ = std::fs::remove_file(&probe);

    let reported_ok = text.contains("appears to be OK");
    let modified = before != after;
    println!(
        "fsck.hfsplus on a volume with a destroyed primary signature: \
         reported_ok={reported_ok} modified_image={modified}"
    );

    // If fsck repaired the copy, the repaired bytes are a valid volume again.
    // That is exactly why fsck is a repair tool and not a conformance oracle.
    if modified && reported_ok {
        use hfsplus::blockdev::MemoryDevice;
        let repaired = MemoryDevice::new(after);
        assert!(
            VolumeHeader::read_from(&repaired).is_ok(),
            "the fsck-repaired image should be a parseable volume again"
        );
    }

    // And the canonical fixture is still corrupt, because we never gave it to fsck.
    assert!(
        parse_error(&path)
            .to_string()
            .contains("unrecognised volume signature"),
        "running fsck must not have repaired the canonical fixture"
    );
}


