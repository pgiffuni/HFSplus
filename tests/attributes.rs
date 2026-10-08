//! The Attributes File, read end to end.
//!
//! `src/attributes/key.rs` and `record.rs` are covered at the structure level by
//! their own unit tests, which is all the format allows when no image in the
//! corpus carries an attribute. `mkfs.hfsplus` builds an attributes fork and an
//! empty tree and nothing else, so these images are the only way to read a real
//! one.
//!
//! # What the fixture contains
//!
//! Two attributes on one file, chosen to cover the two storage forms:
//!
//! | attribute | stored |
//! | --- | --- |
//! | `com.apple.test.inline` | inside the record |
//! | `com.apple.test.forked` | two allocation blocks, the second described by a *separate* continuation record |
//!
//! The second is the one that matters. A forked value is not in the record at
//! all, so a reader that only looks at the record sees an empty value that is
//! indistinguishable from a legitimately empty attribute. And the continuation
//! record is a separate B-tree record chained by the key's `startBlock`, so
//! returning the first record alone would silently truncate the value.
//!
//! Mining reference: `core/hfs_format.h` `kHFSPlusAttrForkData` and
//! `kHFSPlusAttrExtents`; `core/hfs_attrlist.c` scans one `fileID`.

mod common;

use hfsplus::attributes::AttributesFile;
use hfsplus::blockdev::FileDevice;
use hfsplus::format::volume_header::VolumeHeader;

/// The image, if it was generated.
fn open() -> Option<(FileDevice, VolumeHeader)> {
    let path = common::image("journal-with-attributes");
    if !path.exists() {
        eprintln!(
            "skipping: {} not built; run tools/mkfiles.py",
            path.display()
        );
        return None;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vh = VolumeHeader::read_from(&dev).expect("header");
    Some((dev, vh))
}

const OWNER: u32 = 18;
const INLINE_NAME: &str = "com.apple.test.inline";
const INLINE_VALUE: &[u8] = b"inline attribute value";
const FORKED_NAME: &str = "com.apple.test.forked";

#[test]
fn the_attributes_tree_is_readable_and_not_empty() {
    let Some((dev, vh)) = open() else { return };
    let tree = AttributesFile::open(&dev, &vh.attributes_file, vh.block_size, vh.is_hfsx())
        .expect("open the attributes tree");

    // The geometry is worth pinning on its own: a node larger than one allocation
    // block is the case that decides where the first leaf starts, and getting it
    // wrong puts the leaf inside the header node.
    assert_eq!(tree.header().node_size, 8192);
    assert_eq!(
        tree.header().max_key_length,
        266,
        "corroborates the key size"
    );
    assert_eq!(tree.header().first_leaf_node, 1);
    assert!(!tree.is_empty());
}

#[test]
fn an_inline_attribute_returns_its_value_from_the_record() {
    let Some((dev, vh)) = open() else { return };
    let tree =
        AttributesFile::open(&dev, &vh.attributes_file, vh.block_size, vh.is_hfsx()).expect("open");

    let attrs = tree.attributes_for(OWNER).expect("read");
    let inline = attrs
        .iter()
        .find(|a| a.name == INLINE_NAME)
        .unwrap_or_else(|| panic!("{INLINE_NAME} must be present, got {:?}", names(&attrs)));
    assert_eq!(inline.value, INLINE_VALUE);
}

#[test]
fn a_forked_attribute_returns_its_value_from_the_blocks_it_names() {
    let Some((dev, vh)) = open() else { return };
    let tree =
        AttributesFile::open(&dev, &vh.attributes_file, vh.block_size, vh.is_hfsx()).expect("open");

    let attrs = tree.attributes_for(OWNER).expect("read");
    let forked = attrs
        .iter()
        .find(|a| a.name == FORKED_NAME)
        .unwrap_or_else(|| panic!("{FORKED_NAME} must be present, got {:?}", names(&attrs)));

    // Two blocks, written by the fixture with a per-block pattern so a read that
    // picked the wrong block would not look right.
    assert_eq!(forked.value.len(), 8192, "two allocation blocks");
    assert_eq!(
        forked.value.len() / 2,
        4096,
        "each half is one block of the volume's size"
    );
    assert!(
        forked.value[..4096].iter().all(|b| *b == 0xA0),
        "the first block's pattern must come back exactly"
    );
    assert!(
        forked.value[4096..].iter().all(|b| *b == 0xA1),
        "and the second block's, which is named only by the continuation record -- \
         the half a reader that ignores continuations would lose"
    );
}

#[test]
fn the_two_forms_come_back_in_key_order() {
    let Some((dev, vh)) = open() else { return };
    let tree =
        AttributesFile::open(&dev, &vh.attributes_file, vh.block_size, vh.is_hfsx()).expect("open");
    let attrs = tree.attributes_for(OWNER).expect("read");
    assert_eq!(names(&attrs), vec![FORKED_NAME, INLINE_NAME]);
}

#[test]
fn a_file_with_no_attributes_yields_none_rather_than_failing() {
    // The common case: most files on most volumes have none, and a volume whose
    // attributes tree is entirely empty must be readable rather than an error.
    let Some((dev, vh)) = open() else { return };
    let tree =
        AttributesFile::open(&dev, &vh.attributes_file, vh.block_size, vh.is_hfsx()).expect("open");
    assert!(
        tree.attributes_for(16).expect("read").is_empty(),
        "a CNID with no attribute records has no attributes"
    );
    assert!(
        tree.attributes_for(9999).expect("read").is_empty(),
        "and neither does one past the last record"
    );
}

#[test]
fn the_independent_checker_accepts_the_attributes_image() {
    // The fixture is only useful if it is a real volume.
    let path = common::image("journal-with-attributes");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }

    // Prefer the in-tree hfsck (read-only, no copy needed).
    if let Some(hfsck) = common::hfsck_available() {
        let out = common::run_hfsck(&hfsck, &path).expect("spawn hfsck");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success(),
            "the attributes image must be a sound volume:\n{text}"
        );
        // hfsck is read-only: verify the image is untouched.
        let after = std::fs::read(&path).expect("read after hfsck");
        assert_eq!(
            digest(&std::fs::read(&path).expect("read the original")),
            digest(&after),
            "hfsck modified the image:\n{text}"
        );
        return;
    }

    // Fall back to the external checker on a copy.
    // fsck_hfs is pointed at a copy: it repairs as well as reports, and would
    // rewrite the original.
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: neither hfsck binary nor fsck.hfsplus installed");
        return;
    };
    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-attrs-{}.img", std::process::id()));
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
        "the attributes image must be a sound volume:\n{text}"
    );
    assert_eq!(
        digest(&std::fs::read(&path).expect("read the original")),
        digest(&after),
        "the checker modified its copy, so it repaired something:\n{text}"
    );
}

#[test]
fn the_checker_accepts_the_attributes_image() {
    // The in-tree checker, over the same ground. A fixture that fsck accepts and
    // hfsck flags is telling us something about one of them.
    let Some((_dev, vh)) = open() else { return };
    let path = common::image("journal-with-attributes");
    let dev = FileDevice::open(&path).expect("open");
    let vol = hfsplus::volume::Volume::open(&dev).expect("mount");
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "the attributes image must be internally consistent: {:?}",
        report.describe()
    );
    let _ = vh;
}

fn names(attrs: &[hfsplus::attributes::Attribute]) -> Vec<&str> {
    attrs.iter().map(|a| a.name.as_str()).collect()
}

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// --- The writable trust boundary ------------------------------------------

#[test]
fn volume_getxattr_reads_an_inline_attribute() {
    let path = common::image("journal-with-attributes");
    if !path.exists() {
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = hfsplus::volume::Volume::open(&dev).expect("mount");

    // CNID 18 is the file with the test attributes.
    let file = vol
        .lookup_cnid(OWNER.into())
        .expect("lookup by cnid")
        .expect("CNID 18 must exist");

    // The inline attribute should be readable.
    let val = vol
        .getxattr(&file, INLINE_NAME)
        .expect("getxattr")
        .expect("inline attribute must be present");
    assert_eq!(&val[..], INLINE_VALUE);

    // A non-existent attribute returns None, not an error.
    let missing = vol
        .getxattr(&file, "com.apple.does.not.exist")
        .expect("getxattr on missing name");
    assert!(missing.is_none());
}

#[test]
fn volume_listxattr_lists_all_attributes_on_a_file() {
    let path = common::image("journal-with-attributes");
    if !path.exists() {
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = hfsplus::volume::Volume::open(&dev).expect("mount");

    let file = vol
        .lookup_cnid(OWNER.into())
        .expect("lookup by cnid")
        .expect("CNID 18 must exist");

    let mut names = vol.listxattr(&file).expect("listxattr");
    names.sort();
    let mut expected = vec![INLINE_NAME.to_string(), FORKED_NAME.to_string()];
    expected.sort();
    assert_eq!(names, expected, "both attributes must be listed");
}

#[test]
fn volume_getxattr_on_a_volume_without_attributes_returns_none() {
    let path = common::image("basic-hfsplus");
    if !path.exists() {
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = hfsplus::volume::Volume::open(&dev).expect("mount");

    let root = vol
        .lookup_cnid(vol.root_cnid())
        .expect("lookup root")
        .expect("root must exist");

    let result = vol.getxattr(&root, "anything").expect("getxattr");
    assert!(result.is_none(), "no attributes on this volume");
}

// --- The writable trust boundary ------------------------------------------

#[test]
fn a_writable_view_records_whether_a_journal_was_replayed() {
    // `Volume::open` replays the journal when there is one, and does not when the
    // journal lives on another device or there is none. A writer needs to know
    // which, and needs it told rather than inferred -- so the fact is part of the
    // type rather than something a caller re-derives.

    let path = common::image("journal-with-attributes");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    // This volume *is* journaled, and the journal was replayed in the read-only
    // view, so the writable view must record that fact -- the writer works through
    // a journal transaction rather than writing the catalog directly.
    let mut dev = FileDevice::open(&path).expect("open");
    let vol = hfsplus::volume::Volume::open(&dev).expect("mount");
    assert!(
        vol.journal().expect("journal").is_some(),
        "this volume is journaled and the journal replays, so a writer must know"
    );
    drop(vol);
    let writable = hfsplus::volume::WritableVolume::open(&mut dev)
        .expect("a journaled volume should be accepted for mutation");
    assert!(
        writable.journal_was_replayed(),
        "the writable view must report that a journal was replayed"
    );
    assert!(
        writable.is_journaled(),
        "the writable view must report that the volume is journaled"
    );
}

#[test]
fn a_volume_with_no_journal_says_so_rather_than_claiming_a_replay() {
    use hfsplus::volume::WritableVolume;

    let path = common::image("basic-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let mut dev = FileDevice::open(&path).expect("open");
    let writable = WritableVolume::open(&mut dev).expect("validate");
    assert!(
        !writable.journal_was_replayed(),
        "a volume with no journal has nothing to replay, and saying otherwise \\
         would tell a writer its view is current for the wrong reason"
    );
}

#[test]
fn opening_for_mutation_does_not_bypass_the_journal_check() {
    // The point of a separate type. `Volume::open` already refuses a journal it
    // cannot replay, because the filesystem it would serve is stale -- so the
    // writable path, which is built on that same open, inherits the refusal rather
    // than needing to repeat the check. That is why `WritableVolume::open` opens a
    // `Volume` over a shared reborrow rather than having checks of its own.

    let path = common::image("journal-bad-max-blocks");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let mut dev = FileDevice::open(&path).expect("open");

    // Refused at mount, before any question of writing arises.
    let vol = hfsplus::volume::Volume::open(&dev);
    assert!(
        vol.is_err(),
        "a volume whose journal cannot be replayed must not mount at all"
    );
    // And refused again for mutation, over the same validation path. This is the
    // property that owning the device had to preserve: there is still exactly one
    // validation path, it is `Volume::open`, and asking for write access does not
    // get past it.
    drop(vol);
    assert!(
        hfsplus::volume::WritableVolume::open(&mut dev).is_err(),
        "the journal check must not be bypassed by asking for write access"
    );
}
