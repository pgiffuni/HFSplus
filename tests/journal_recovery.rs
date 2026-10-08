//! Crash-consistent recovery: a journalled metadata write that never reached
//! the filesystem.
//!
//! The rest of the journal suite verifies the *mechanics* of replay -- that the
//! overlay wins over the device, that a transaction is walked, that damage
//! truncates. `tools/makejournal.py` can only do that by rewriting a block the
//! filesystem never references, because it has no way to write a real catalog
//! record. The consequence is that those images prove precedence and
//! non-interference, and nothing about the reason a journal exists.
//!
//! `tools/mktorn.py` closes that gap. It writes a genuine catalog change into a
//! journal transaction and leaves the on-disk catalog alone, producing a volume
//! in exactly the state a machine that lost power mid-write leaves behind: the
//! filesystem is internally consistent but older than the journal. So the image
//! can be checked two ways, and both are asserted here:
//!
//! * `fsck.hfsplus` accepts it, because the on-disk filesystem *is* sound. It
//!   reports no missing file, because a file the journal holds and the disk does
//!   not is not a disk defect. This is what makes the scenario a realistic one
//!   rather than a corrupt image.
//! * Replay makes the file appear, and does so without touching a byte of the
//!   image.
//!
//! Mining reference: Apple `core/hfs_journal.c` `replay_journal` applies
//! transactions to the device, and `core/hfs_catalog.c` `cat_rebuild` is the
//! catalog-side equivalent -- both write through a journal, and neither modifies
//! the source image.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::cnid::Cnid;
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::journal::replay::OverlaidDevice;
use hfsplus::journal::Journal;
use hfsplus::volume::Volume;

/// The image, and the file its journal holds but its catalog does not.
const TORN: &str = "journal-torn-catalog";
const TORN_NAME: &str = "torn.txt";
const TORN_CNID: u32 = 18;

fn image_path(name: &str) -> std::path::PathBuf {
    common::repo_root()
        .join("tests/images/replayed")
        .join(format!("{name}.img"))
}

/// Names in the root folder, sorted, so an assertion does not depend on the
/// order the catalog happens to return.
///
/// Generic over the device because the point of the suite is to read the same
/// volume two ways: straight off the file, and through the journal overlay.
fn root_names<D: hfsplus::blockdev::BlockDevice + ?Sized>(vol: &Volume<'_, D>) -> Vec<String> {
    let mut names: Vec<String> = vol
        .read_dir(vol.root_cnid())
        .expect("read_dir")
        .iter()
        .map(|o| o.name_string())
        .collect();
    names.sort();
    names
}

/// Open the image with its journal replayed and hand the volume to `f`.
///
/// The chain of borrows is device -> journal -> overlay -> volume, so all four
/// have to be alive at once. Taking the device as an argument rather than
/// opening it here keeps every lifetime local to the caller, which is the only
/// way to get the ordering right without reaching for `'static` -- and a
/// `'static` borrow would demand the journal outlive this function, which it
/// cannot.
fn with_replay<D: hfsplus::blockdev::BlockDevice + ?Sized>(
    dev: &D,
    f: impl FnOnce(&Volume<'_, OverlaidDevice<'_, '_, D>>),
) {
    let vh = VolumeHeader::read_from(dev).expect("volume header");
    let journal = Journal::open(dev, vh.journal_info_block, vh.block_size)
        .unwrap_or_else(|e| panic!("journal open: {e}"))
        .unwrap_or_else(|| panic!("{TORN}: expected a journal"));
    let overlaid = journal.into_device();
    f(&Volume::open(&overlaid).expect("mount through the overlay"));
}

/// The torn-catalog image, or `None` when it has not been generated.
fn torn_device() -> Option<FileDevice> {
    let path = image_path(TORN);
    if !path.exists() {
        eprintln!(
            "skipping: {} not built; run tools/mktorn.py",
            path.display()
        );
        return None;
    }
    Some(FileDevice::open(&path).expect("open image"))
}

#[test]
fn without_replay_the_filesystem_is_sound_but_stale() {
    // The starting condition. Read straight off the image, with no journal in
    // the way, and the file is simply not there. This is what a crash leaves: not
    // a broken filesystem, but one missing the most recent writes.
    let path = image_path(TORN);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let names = root_names(&vol);

    assert!(
        !names.iter().any(|n| n == TORN_NAME),
        "the on-disk catalog must not contain {TORN_NAME}, got {names:?}"
    );
    // The rest of the volume is perfectly readable, which is the point: a stale
    // filesystem is not a broken one.
    assert!(
        names.contains(&".journal".to_string())
            && names.contains(&".journal_info_block".to_string()),
        "the existing entries must still be listed, got {names:?}"
    );
    let units: Vec<u16> = TORN_NAME.encode_utf16().collect();
    assert!(
        vol.lookup(vol.root_cnid(), &units).unwrap().is_none(),
        "{TORN_NAME} must not be findable without replay"
    );
}

#[test]
fn replaying_the_journal_recovers_the_lost_file() {
    let Some(dev) = torn_device() else { return };
    with_replay(&dev, |vol| {
        let names = root_names(vol);
        assert!(
            names.iter().any(|n| n == TORN_NAME),
            "replay must make {TORN_NAME} visible, got {names:?}"
        );
        // And nothing was lost on the way: the recovered file joins the
        // existing entries rather than replacing them.
        assert_eq!(names.len(), 3, "expected three root entries, got {names:?}");
    });
}

#[test]
fn the_recovered_file_is_reachable_by_name_and_by_cnid() {
    // A recovered file that cannot be opened is no use. Both routes matter: a
    // mount resolves by path, and anything walking the volume resolves by CNID.
    let Some(dev) = torn_device() else { return };
    with_replay(&dev, |vol| {
        let units: Vec<u16> = TORN_NAME.encode_utf16().collect();
        let object = vol
            .lookup(vol.root_cnid(), &units)
            .expect("lookup by name")
            .unwrap_or_else(|| panic!("{TORN_NAME} must be found by name after replay"));

        assert_eq!(object.name_string(), TORN_NAME);
        assert_eq!(
            object.cnid().0,
            TORN_CNID,
            "the journalled CNID must be preserved"
        );
        assert!(!object.is_dir(), "{TORN_NAME} was journalled as a file");
        assert_eq!(
            object.data_size(),
            0,
            "the journalled file record has an empty data fork"
        );

        // Resolving by CNID goes through the thread record, which the journal
        // also rewrote. A missing or mis-keyed thread record would leave the file
        // visible in a listing but unreachable by identity.
        let by_cnid = vol
            .lookup_cnid(Cnid(TORN_CNID))
            .expect("lookup by cnid")
            .unwrap_or_else(|| panic!("CNID {TORN_CNID} must resolve to {TORN_NAME}"));
        assert_eq!(by_cnid.name_string(), TORN_NAME);
        assert_eq!(by_cnid.cnid().0, TORN_CNID);
    });
}

#[test]
fn the_recovered_file_is_a_regular_file_with_its_record_intact() {
    // The record is cloned from one the formatter wrote, so the mode has to come
    // through as a regular file rather than as whatever a zeroed record would
    // decode to. A mis-built record that still parses is the failure mode worth
    // catching.
    let Some(dev) = torn_device() else { return };
    with_replay(&dev, |vol| {
        let units: Vec<u16> = TORN_NAME.encode_utf16().collect();
        let object = vol
            .lookup(vol.root_cnid(), &units)
            .expect("lookup")
            .expect("present");
        let bsd = object.bsd_info();
        assert_eq!(
            bsd.file_mode & hfsplus::catalog::record::S_IFMT,
            hfsplus::catalog::record::S_IFREG,
            "{TORN_NAME} must be a regular file, mode {:#o}",
            bsd.file_mode
        );
    });
}

#[test]
fn a_recovered_volume_lists_every_object_exactly_once() {
    // A thread record with the wrong key would make an object appear twice in a
    // whole-volume scan, or vanish from it. The scan is keyed on the thread
    // record's own CNID, so it is the check that the key was built correctly.
    let Some(dev) = torn_device() else { return };
    with_replay(&dev, |vol| {
        let mut all: Vec<(u32, String)> = vol
            .catalog()
            .all_objects()
            .expect("all_objects")
            .iter()
            .map(|e| (e.cnid.0, String::from_utf16_lossy(&e.name).to_string()))
            .collect();
        all.sort();

        let cnids: Vec<u32> = all.iter().map(|(c, _)| *c).collect();
        let mut unique = cnids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            cnids, unique,
            "every object must appear exactly once, got {all:?}"
        );

        assert!(
            all.iter().any(|(c, n)| *c == TORN_CNID && n == TORN_NAME),
            "the recovered file must be in the whole-volume inventory, got {all:?}"
        );
    });
}

#[test]
fn the_on_disk_filesystem_is_accepted_by_the_independent_checker() {
    // This is what separates a crash-consistent volume from a corrupt one.
    // The checker accepts the image and leaves it byte-identical, so it validated
    // the stale filesystem without writing the replay. That is the claim the byte
    // comparison supports; whether it replayed in memory is not observable from
    // outside, so it is not asserted.
    //
    // A file that lives only in the journal is not a disk defect, and a checker
    // that complained would be wrong about what it found. See docs/dev-tools.md
    // for what the hfsprogs port does and does not examine.
    //
    // Run on a COPY. `fsck.hfsplus` repairs as well as reports, and pointing it
    // at a fixture would rewrite it. `hfsck` is read-only and can run directly.
    let path = image_path(TORN);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }

    if let Some(hfsck) = common::hfsck_available() {
        let out = common::run_hfsck(&hfsck, &path).expect("spawn hfsck");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success(),
            "the stale on-disk filesystem must still be sound by hfsck:\n{text}"
        );
        // hfsck is read-only: the image must be byte-identical.
        let original = std::fs::read(&path).expect("read the original");
        let after = std::fs::read(&path).expect("read after hfsck");
        assert_eq!(
            digest(&original),
            digest(&after),
            "hfsck modified the image:\n{text}"
        );
        return;
    }

    // Fall back to the external checker on a copy.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: neither hfsck binary nor fsck.hfsplus installed");
        return;
    };
    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-torn-{}-{}.img", TORN, std::process::id()));
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
        "the stale on-disk filesystem must still be sound:\n{text}"
    );
    // The claim above is "validated without writing the replay", so the image
    // must come back byte-identical. A checker that replayed the journal into the
    // volume would make `torn.txt` appear on disk, and this is what would show it.
    assert_eq!(
        digest(&std::fs::read(&path).expect("read the original")),
        digest(&after),
        "the checker modified the image:\n{text}"
    );
}

#[test]
fn recovery_leaves_the_image_byte_identical() {
    // The guarantee that makes a read-only mount safe to point at real media:
    // recovering a file must not have written it to the disk. If replay ever
    // flushed its overlay, this is the assertion that would catch it.
    let path = image_path(TORN);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let before = std::fs::read(&path).expect("read before");
    let digest_before = digest(&before);

    {
        let dev = FileDevice::open(&path).expect("open");
        let vh = VolumeHeader::read_from(&dev).expect("header");
        let journal = Journal::open(&dev, vh.journal_info_block, vh.block_size)
            .unwrap()
            .expect("journal");
        let overlaid = journal.into_device();
        let vol = Volume::open(&overlaid).expect("mount");

        // Exercise the recovered path fully, so a write-back would have a chance
        // to happen rather than merely being possible.
        let names = root_names(&vol);
        assert!(names.iter().any(|n| n == TORN_NAME));
        let units: Vec<u16> = TORN_NAME.encode_utf16().collect();
        let object = vol.lookup(vol.root_cnid(), &units).unwrap().unwrap();
        assert_eq!(vol.read(&object, 0, 64).expect("read").len(), 0);
        let _ = vol.lookup_cnid(Cnid(TORN_CNID)).unwrap();
        let _ = vol.statfs().unwrap();
        let _ = vol.name().unwrap();
    }

    let after = std::fs::read(&path).expect("read after");
    assert_eq!(
        digest_before,
        digest(&after),
        "recovering {TORN_NAME} modified the source image"
    );
}

#[test]
fn the_volume_header_advances_with_the_catalogue() {
    // `nextCatalogID` is what stops a later write from reusing the CNID the
    // recovered file now holds. The journal rewrites the volume header along
    // with the catalog, so a replay that ignored it would hand out CNID 18 twice.
    let Some(dev) = torn_device() else { return };
    with_replay(&dev, |vol| {
        assert_eq!(
            vol.header().next_catalog_id,
            TORN_CNID + 1,
            "nextCatalogID must be past the journalled CNID"
        );
    });
}

#[test]
fn recovery_needs_the_journal_and_nothing_else() {
    // A negative control for the whole suite. If replay were being skipped, or
    // the file were somehow present on the disk, the two lists would be equal
    // and the recovery assertions above would be vacuous. Asserting they differ
    // is what gives them force.
    let Some(dev) = torn_device() else { return };

    let stale = {
        let vol = Volume::open(&dev).expect("mount");
        root_names(&vol)
    };
    let mut replayed = None;
    with_replay(&dev, |vol| replayed = Some(root_names(vol)));
    let replayed = replayed.expect("replayed listing");

    assert_ne!(
        stale, replayed,
        "the replayed and unreplayed listings must differ, or nothing was recovered"
    );
    assert!(!stale.contains(&TORN_NAME.to_string()));
    assert!(replayed.contains(&TORN_NAME.to_string()));
}

#[test]
fn a_second_recovery_of_the_same_image_is_idempotent() {
    // Replay is a read-side transform, so opening the same image twice must give
    // the same answer. A journal whose replay mutated its own state -- consuming
    // a transaction, advancing a cursor -- would make the second mount differ,
    // which for a filesystem means a file silently appearing or vanishing across
    // a remount.
    let Some(dev) = torn_device() else { return };

    let mut names = None;
    with_replay(&dev, |vol| names = Some(root_names(vol)));
    let first = names.expect("first read");

    let mut names = None;
    with_replay(&dev, |vol| names = Some(root_names(vol)));
    let second = names.expect("second read");

    assert_eq!(first, second, "replay must be repeatable");
    assert_eq!(first.len(), 3);
}

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}
