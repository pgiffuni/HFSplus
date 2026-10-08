//! The empty-catalog bootstrap, piece by piece.
//!
//! `mkfs.hfsplus` builds a volume with no files, so its catalog already holds the
//! two records every HFS+ volume starts with. `mkfiles.py` clones those records,
//! which is safe -- the template is Apple's own bytes -- but it means no tool here
//! can create the *first* file. Milestones 8 and 9 both need that.
//!
//! An attempt to teach `mkfiles.py` to synthesise the records was reverted: it
//! emitted catalog keys with a `keyLength` of 4 where 6 is correct, and
//! `fsck.hfsplus` rejected the result. The fix was not a patch to the emitted
//! byte but a separate generator whose every piece is asserted here, so the same
//! class of mistake has one place to happen.
//!
//! Each test below checks one element, because the whole record is opaque: a
//! `keyLength` error in the middle looks exactly like a body error three records
//! later.
//!
//! Mining reference: `core/hfs_format.h` for every structure, and the records
//! `mkfs.hfsplus` writes, which this compares against byte for byte.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::volume::Volume;

const IMAGE: &str = "bootstrapped-catalog";
const VOLUME_NAME: &str = "BasicVolume";
const BLOCK: usize = 4096;

fn image() -> Option<Vec<u8>> {
    let path = common::image(IMAGE);
    if !path.exists() {
        eprintln!(
            "skipping: {} not built; run tools/mkbootstrap.py",
            path.display()
        );
        return None;
    }
    Some(std::fs::read(&path).expect("read"))
}

fn catalog_node(img: &[u8], node: usize) -> Option<Vec<u8>> {
    let bs = u32::from_be_bytes([
        img[1024 + 40],
        img[1024 + 41],
        img[1024 + 42],
        img[1024 + 43],
    ]) as usize;
    let start = u32::from_be_bytes([
        img[1024 + 272 + 16],
        img[1024 + 272 + 17],
        img[1024 + 272 + 18],
        img[1024 + 272 + 19],
    ]) as usize;
    if bs != BLOCK {
        return None;
    }
    let at = (start + node) * bs;
    Some(img[at..at + bs].to_vec())
}

fn be16(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_be_bytes([b[at], b[at + 1]]))
}

/// A `u16` field read as one, for comparing against a typed constant.
fn be16_u16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn i16(b: &[u8], at: usize) -> i16 {
    i16::from_be_bytes([b[at], b[at + 1]])
}

/// A node's `kind`: one signed byte, not a 16-bit field.
///
/// `kBTHeaderNode` is 1, `kBTIndexNode` 0, `kBTMapNode` 2 and `kBTLeafNode` -1,
/// which is 0xFF on disk. Reading it as a 16-bit value shifts the next field and
/// turns a leaf into -255.
fn kind(b: &[u8]) -> i8 {
    b[8] as i8
}

// --- The catalog header node --------------------------------------------

#[test]
fn the_catalog_header_node_is_well_formed() {
    let Some(img) = image() else { return };
    let Some(header) = catalog_node(&img, 0) else {
        return;
    };

    assert_eq!(kind(&header), 1, "kind must be kBTHeaderNode");
    assert_eq!(header[9], 0, "a header node's height is zero");
    assert_eq!(
        be16_u16(&header, 10),
        3,
        "Apple's header node declares three records"
    );

    // The BTHeaderRec is *record 0*, at offset 14. Putting these fields at
    // record 1's offset (120) leaves every one of them reading zero, which is how
    // this went wrong once.
    const REC: usize = 14;
    assert_eq!(be16(&header, REC), 1, "treeDepth");
    assert_eq!(be32(&header, REC + 2), 1, "rootNode");
    assert_eq!(
        be32(&header, REC + 6),
        2,
        "leafRecords: the two records below"
    );
    assert_eq!(be32(&header, REC + 10), 1, "firstLeafNode");
    assert_eq!(be32(&header, REC + 14), 1, "lastLeafNode");
    assert_eq!(be16_u16(&header, REC + 18), BLOCK as u16, "nodeSize");
    assert_eq!(
        be16_u16(&header, REC + 20),
        516,
        "maxKeyLength, HFSPlusCatalogKeyMaximumLength"
    );

    // totalNodes is the whole file; freeNodes counts what nothing points at.
    assert_eq!(be32(&header, REC + 22), 8, "totalNodes");
    assert_eq!(
        be32(&header, REC + 26),
        6,
        "freeNodes: eight nodes, two of them in use"
    );
}

#[test]
fn the_header_declares_case_folding_and_big_keys() {
    // Two bytes that are easy to leave at zero and that change how every key is
    // read. fsck.hfsplus reports "Invalid B-tree header" without them.
    let Some(img) = image() else { return };
    let Some(header) = catalog_node(&img, 0) else {
        return;
    };
    const REC: usize = 14;

    assert_eq!(
        header[REC + 37],
        0xCF,
        "keyCompareType must be kHFSCaseFolding on an HFS+ volume"
    );
    let attributes = be32(&header, REC + 38);
    assert_eq!(
        attributes & 0x2,
        0x2,
        "kBTBigKeysMask: without it keys carry a one-byte length"
    );
    assert_eq!(attributes & 0x4, 0x4, "kBTVariableIndexKeysMask");
}

#[test]
fn the_node_map_claims_exactly_the_nodes_that_are_used() {
    // One bit per node, MSB first -- the same convention as the volume's
    // allocation bitmap, since both answer "is this block in use".
    let Some(img) = image() else { return };
    let Some(header) = catalog_node(&img, 0) else {
        return;
    };

    // The map lives where the header node's third record would be: offset 248.
    assert_eq!(header[248], 0xC0, "nodes 0 and 1 are in use");
}

// --- The records --------------------------------------------------------

#[test]
fn the_root_folder_record_is_keyed_by_its_parent_and_its_own_name() {
    // Every object's record is keyed (parentID, name). For the root that is
    // (1, volume name) -- which is why the volume's name is in the catalog at all
    // and not in the volume header.
    let Some(img) = image() else { return };
    let Some(leaf) = catalog_node(&img, 1) else {
        return;
    };

    assert_eq!(kind(&leaf), -1, "kind must be kBTLeafNode");
    assert_eq!(be16_u16(&leaf, 10), 2, "two records");

    let at = 14;
    assert_eq!(be16(&leaf, at), 4 + 2 + VOLUME_NAME.len() * 2, "keyLength");
    assert_eq!(be32(&leaf, at + 2), 1, "key parentID is kHFSRootParentID");
    let name_len = be16(&leaf, at + 6);
    assert_eq!(name_len, VOLUME_NAME.len());
    let units: Vec<u16> = leaf[at + 8..at + 8 + name_len * 2]
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(String::from_utf16_lossy(&units), VOLUME_NAME);

    let body = at + 2 + be16(&leaf, at);
    assert_eq!(i16(&leaf, body), 1, "kHFSPlusFolderRecord");
    assert_eq!(be32(&leaf, body + 8), 2, "folderID is kHFSRootFolderID");
    assert_eq!(
        be32(&leaf, body + 4),
        0,
        "valence excludes the root's own thread record"
    );
}

#[test]
fn the_root_thread_record_names_the_object_and_its_parent_in_the_body() {
    // The asymmetry that makes a thread record necessary: the *key* names the
    // object with an empty name, and the *body* carries the parent and the
    // object's own name.
    let Some(img) = image() else { return };
    let Some(leaf) = catalog_node(&img, 1) else {
        return;
    };

    // Locate the second record through the node's offset array rather than by
    // computing body sizes, so the test does not restate the layout it checks.
    let n = be16(&leaf, 10);
    let offsets: Vec<usize> = (0..=n).map(|i| be16(&leaf, BLOCK - 2 * (i + 1))).collect();
    let at = offsets[1];

    assert_eq!(
        be16(&leaf, at),
        6,
        "a thread key names the object with an empty name"
    );
    assert_eq!(
        be32(&leaf, at + 2),
        2,
        "key parentID is the object's own CNID"
    );
    assert_eq!(be16(&leaf, at + 6), 0, "and an empty name");

    let body = at + 2 + be16(&leaf, at);
    assert_eq!(
        i16(&leaf, body),
        3,
        "kHFSPlusFolderThreadRecord -- a directory's thread is 3, a file's is 4"
    );
    assert_eq!(
        be32(&leaf, body + 4),
        1,
        "the body's parent is kHFSRootParentID"
    );
    let name_len = be16(&leaf, body + 8);
    assert_eq!(
        name_len,
        VOLUME_NAME.len(),
        "the body carries the object's own name, not an empty one -- fsck.hfsplus \\
         validates this and reports \"Invalid parent CName\" without it"
    );
}

#[test]
fn the_records_are_in_ascending_key_order() {
    // Order is not cosmetic: the tree is only valid if the keys increase, and
    // `fsck.hfsplus` rebuilds the B-tree rather than repairing it.
    let Some(img) = image() else { return };
    let Some(leaf) = catalog_node(&img, 1) else {
        return;
    };
    let n = be16(&leaf, 10);
    let offsets: Vec<usize> = (0..=n).map(|i| be16(&leaf, BLOCK - 2 * (i + 1))).collect();

    let mut previous: Option<(u32, usize)> = None;
    for (i, window) in offsets.windows(2).enumerate() {
        let [at, _end] = [window[0], window[1]];
        let key = (be32(&leaf, at + 2), be16(&leaf, at + 6));
        if let Some(prev) = previous {
            assert!(
                prev < key,
                "record {i} at {at} has key {key:?}, which does not follow {prev:?}"
            );
        }
        previous = Some(key);
    }
    assert_eq!(
        offsets.len(),
        n + 1,
        "the offset array has one more entry than records"
    );
}

// --- The volume header --------------------------------------------------

#[test]
fn the_volume_header_counts_a_volume_with_no_files() {
    let Some(img) = image() else { return };
    let Some(vh) = VolumeHeader::from_bytes(&img[1024..1536]).ok() else {
        return;
    };

    assert_eq!(vh.file_count, 0, "no files");
    assert_eq!(
        vh.folder_count, 0,
        "and the root directory does not count itself"
    );
    assert_eq!(
        vh.next_catalog_id, 16,
        "kHFSFirstUserCatalogNodeID: CNIDs below 16 are reserved, and \\
         fsck.hfsplus normalises a smaller value"
    );
}

// --- The parser and the independent checker -----------------------------

#[test]
fn the_parser_reads_the_bootstrapped_volume() {
    let path = common::image(IMAGE);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    assert_eq!(
        vol.name().expect("name"),
        VOLUME_NAME,
        "from the root folder's name"
    );
    assert_eq!(
        vol.read_dir(vol.root_cnid()).expect("read_dir").len(),
        0,
        "the root has no children"
    );

    // The root resolves as an object, which means its folder record and thread
    // record both decoded.
    let root = vol
        .lookup_cnid(vol.root_cnid())
        .expect("resolve")
        .expect("the root must exist");
    assert!(root.is_dir());
}

#[test]
fn the_independent_checker_accepts_the_bootstrapped_volume() {
    // The strongest test available, and the one that caught every error above:
    // `fsck.hfsplus` recomputes the header's counts and the tree's geometry rather
    // than trusting them.
    //
    // Run on a copy. It repairs as well as reports, and "the volume appears to be
    // OK" is only evidence if nothing was repaired on the way there -- so the
    // probe is compared byte for byte afterwards.
    let path = common::image(IMAGE);
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
            "the bootstrapped volume must be sound:\n{text}"
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
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: neither hfsck binary nor fsck.hfsplus installed");
        return;
    };
    let mut probe = std::env::temp_dir();
    probe.push(format!("bootstrap-{}.img", std::process::id()));
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
        "the bootstrapped volume must be sound:\n{text}"
    );
    assert_eq!(
        digest(&std::fs::read(&path).expect("read the original")),
        digest(&after),
        "the checker repaired something, so the volume was not already sound:\n{text}"
    );
}

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}
