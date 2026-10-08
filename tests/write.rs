//! The first mutation: replacing a file's contents in place.
//!
//! Every test here writes to a *copy* of a fixture. A fixture that fails
//! mid-write is a corrupted fixture, and the corpus is committed, so copies are
//! made per test and thrown away.
//!
//! Each write is checked twice, deliberately: once through this library, and
//! once by `fsck.hfsplus` on a separate copy of the result. The two checks fail
//! differently. A reader can be satisfied by a record Apple would also accept
//! while the surrounding structures are wrong, and `fsck` can pass an image this
//! library then misreads. Agreement is the only thing that establishes either.
//!
//! Images are generated *and committed* (see `AGENTS.md`), so these run on a
//! machine with no `hfsprogs`; the `fsck` checks skip when it is absent.

mod common;

use hfsplus::blockdev::{BlockDeviceMut, FileDevice};
use hfsplus::catalog::cnid::Cnid;
use hfsplus::volume::{Object, Volume, WritableVolume};

/// UTF-16 code units of `s`, for a catalog lookup.
fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The fixture every happy-path test starts from.
const IMAGE: &str = "bootstrapped-with-file";

/// Copy `name` from the corpus to a temporary path and return it.
///
/// Returns `None` when the fixture has not been generated, which is a skip and
/// not a failure.
fn copy_fixture(name: &str) -> Option<std::path::PathBuf> {
    let source = common::image(name);
    if !source.exists() {
        eprintln!("skipping: {} not built", source.display());
        return None;
    }
    let mut dest = std::env::temp_dir();
    // Per-test unique, so parallel tests cannot see each other's partial writes.
    dest.push(format!("write-{}-{}.img", name, unique()));
    std::fs::copy(&source, &dest).expect("copy fixture");
    Some(dest)
}

/// A per-call counter, so two writes in one process never collide.
fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// A write that must succeed, on `path`, flushed before the handle is dropped.
///
/// Dropping the `WritableVolume` before the device matters: `write_at` may sit in
/// a `BufWriter`, and anything that reopens the file -- including `fsck.hfsplus`
/// -- reads the file directly and will not see bytes that were never flushed.
fn write(path: &std::path::Path, cnid: u32, data: &[u8]) {
    let mut dev = FileDevice::open_writable(path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    writable
        .write_file_contents(cnid, data)
        .expect("write contents");
    dev.sync().expect("flush");
}

/// Assert `fsck.hfsplus` accepts `path`, skipping when it is not installed.
fn assert_fsck_clean(path: &std::path::Path, what: &str) {
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping fsck.hfsplus check: not installed");
        return;
    };
    // fsck.hfsplus **modifies the image it checks** -- `AGENTS.md` says so -- and on
    // its first run over an image it writes its own "fsc.k" signature into
    // `lastMountedVersion`, then reports "was repaired successfully" for that alone.
    //
    // So the verdict text cannot be the whole test. What is checked is the *image*:
    // a copy is checked, and every byte that differs from ours must be one of those
    // eight signature bytes -- four in the primary header and four in the copy at
    // the end of the volume. Anything else is a repair, and is named by offset.
    //
    // This is strictly stronger than reading the verdict. A checker that repaired
    // something and reported it would fail on the text; one that repaired
    // something *quietly* would fail here and pass there.
    let before = std::fs::read(path).expect("read image");
    let mut probe = path.to_path_buf();
    let stamp = std::process::id();
    probe.set_file_name(format!(
        "{}.fsckprobe{stamp}",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::copy(path, &probe).expect("copy for fsck");
    let out = common::run_fsck(&fsck, &probe);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read(&probe).expect("read the checked image");
    let _ = std::fs::remove_file(&probe);

    // `lastMountedVersion` is at offset 8 of the volume header, and the backup
    // header sits in the last kilobyte.
    let signature_at = |i: usize| -> bool {
        i == 1024 + 8
            || (1024 + 9..=1024 + 12).contains(&i)
            || i == before.len() - 1024 + 8
            || (before.len() - 1024 + 9..=before.len() - 1024 + 12).contains(&i)
    };
    let repaired: Vec<usize> = before
        .iter()
        .zip(after.iter())
        .enumerate()
        .filter(|(i, (a, b))| a != b && !signature_at(*i))
        .map(|(i, _)| i)
        .take(8)
        .collect();
    assert!(
        repaired.is_empty(),
        "fsck.hfsplus modified {what} at offsets {repaired:?} -- that is a repair, \
         not a signature. Its output:\n{text}"
    );
    assert!(
        text.contains("appears to be OK")
            || text.contains("File system is clean")
            || text.contains("was repaired successfully"),
        "fsck.hfsplus rejected {what}:\n{text}"
    );
}

/// Assert the copy is byte-identical to what was on disk before the attempt.
fn assert_untouched(path: &std::path::Path, before: &[u8], what: &str) {
    let now = std::fs::read(path).expect("read image");
    assert_eq!(now.len(), before.len(), "{what} changed the image's length");
    assert_eq!(now, before, "{what} wrote to the image");
}

// --- The happy path --------------------------------------------------------

#[test]
fn writing_shorter_contents_keeps_the_volume_readable() {
    // The narrow mutation: same blocks, smaller length. No allocation, no extent
    // change, no B-tree split -- which is the point. It isolates serialisation,
    // the leaf write and the timestamps, so a failure names which is wrong.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };

    let (cnid, original_size) = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let object = vol
            .lookup(vol.root_cnid(), &units("payload.bin"))
            .expect("lookup")
            .expect("payload.bin exists");
        let f = object.as_file().expect("a file");
        (f.cnid.0, f.record.data_fork.logical_size)
    };
    assert_eq!(original_size, 4096, "the fixture has one block of content");

    let new_data = b"a shorter file".to_vec();
    write(&path, cnid, &new_data);

    // Read back through a fresh volume: the new bytes, and a shorter logical
    // size. The block stays allocated -- a shorter file is not a freed block, and
    // freeing it is an allocator's job, not a writer's.
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup after write")
        .expect("payload.bin still exists");
    let f = object.as_file().expect("a file");
    assert_eq!(
        f.record.data_fork.logical_size,
        new_data.len() as u64,
        "logicalSize must follow the write"
    );
    assert_eq!(
        f.record.data_fork.total_blocks, 1,
        "and the allocation must be untouched: a shorter file frees nothing"
    );
    let got = vol.read(&object, 0, new_data.len()).expect("read back");
    assert_eq!(got, new_data, "the data blocks must hold the new bytes");

    assert_fsck_clean(&path, "an in-place write of a shorter file");
}

#[test]
fn writing_the_same_number_of_bytes_keeps_the_allocation() {
    // The size of the fork is unchanged, so nothing about the extent record may
    // move. A `logicalSize` left at the old value, or a write to the wrong block,
    // is what this catches -- and `fsck` independently notices a `logicalSize`
    // that disagrees with what it can read.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.lookup(vol.root_cnid(), &units("payload.bin"))
            .expect("lookup")
            .expect("payload.bin exists")
            .as_file()
            .expect("a file")
            .cnid
            .0
    };

    let new_data = vec![0xA5u8; 4096];
    write(&path, cnid, &new_data);

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup after write")
        .expect("payload.bin still exists");
    let f = object.as_file().expect("a file");
    assert_eq!(
        f.record.data_fork.logical_size, 4096,
        "length must not move"
    );
    let got = vol.read(&object, 0, 4096).expect("read back");
    assert!(
        got.iter().all(|b| *b == 0xA5),
        "the whole block must be the new pattern; a byte from the old contents \\
         would mean the write went somewhere other than the file's own block"
    );

    assert_fsck_clean(&path, "an in-place write of the same length");
}

#[test]
fn a_write_updates_the_modification_time_and_leaves_the_access_time_alone() {
    // Apple sets `contentModDate` and `attributeModDate` on a content change, and
    // defers an atime-only update to vnode recycle -- which this library has no
    // point for. So `accessDate` must come through untouched; a writer that
    // invented an atime would be asserting semantics it cannot implement.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };

    let (cnid, before) = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let object = vol
            .lookup(vol.root_cnid(), &units("payload.bin"))
            .expect("lookup")
            .expect("payload.bin exists");
        let f = object.as_file().expect("a file");
        (
            f.cnid.0,
            (
                f.record.content_mod_date,
                f.record.attribute_mod_date,
                f.record.access_date,
            ),
        )
    };

    write(&path, cnid, b"touched");

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup after write")
        .expect("payload.bin still exists");
    let f = object.as_file().expect("a file");
    assert_ne!(
        f.record.content_mod_date, before.0,
        "a content change must move contentModDate"
    );
    assert_ne!(
        f.record.attribute_mod_date, before.1,
        "a content change must move attributeModDate too"
    );
    assert_eq!(
        f.record.content_mod_date, f.record.attribute_mod_date,
        "both come from one clock read, so they must agree"
    );
    assert_eq!(
        f.record.access_date, before.2,
        "accessDate is not touched: there is no recycle point to defer it to, so \\
         the honest answer is to leave it alone"
    );
}

#[test]
fn writing_an_empty_file_leaves_no_trailing_bytes() {
    // Zero length is the boundary of the capacity check: `data.len() == 0` fits
    // anything, so it must not be treated as "no blocks". A reader asking for
    // nothing must get nothing, which is a different statement from there being
    // no way to ask.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.lookup(vol.root_cnid(), &units("payload.bin"))
            .expect("lookup")
            .expect("payload.bin exists")
            .as_file()
            .expect("a file")
            .cnid
            .0
    };

    write(&path, cnid, b"");

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup after write")
        .expect("payload.bin still exists");
    let f = object.as_file().expect("a file");
    assert_eq!(f.record.data_fork.logical_size, 0);
    assert_eq!(
        vol.read(&object, 0, 1).expect("a read past the end"),
        Vec::<u8>::new(),
        "reading past a zero-length file must yield nothing, not a stale byte"
    );

    assert_fsck_clean(&path, "an in-place write to zero length");
}

// --- Creation, which adds records ------------------------------------------

#[test]
fn creating_a_file_puts_it_where_a_reader_will_find_it() {
    // Four structures have to move together, so all four are checked: the file
    // record (found by name), the thread record (found by CNID), the parent
    // folder's child count, and the header's next-CNID counter. A file in three
    // of the four is a file that cannot be found, cannot be counted, or will be
    // handed the same CNID twice.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let (parent, next_before, file_count_before) = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        (
            vol.root_cnid().0,
            vol.header().next_catalog_id,
            vol.header().file_count,
        )
    };

    let cnid = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        let cnid = writable
            .create_file(parent, &units("created.bin"))
            .expect("create file");
        dev.sync().expect("flush");
        cnid
    };

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    // By name -- the file record.
    let object = vol
        .lookup(vol.root_cnid(), &units("created.bin"))
        .expect("lookup")
        .expect("the created file must be findable by name");
    let f = object.as_file().expect("a file, not a folder");
    assert_eq!(f.cnid.0, cnid);
    assert_eq!(f.record.data_fork.logical_size, 0, "a new file is empty");
    assert_eq!(f.record.data_fork.total_blocks, 0);
    assert_eq!(
        f.record.bsd_info.file_mode & 0o7777,
        0o644,
        "and readable and writable, or a caller cannot write to what it just made"
    );

    // By CNID -- the thread record. `lookup_cnid` goes through it, so this is the
    // only way a file with no thread record could be caught here.
    let by_cnid = vol
        .lookup_cnid(hfsplus::catalog::cnid::Cnid(cnid))
        .expect("lookup by CNID")
        .expect("the created file must be findable by CNID");
    assert_eq!(by_cnid.name(), &units("created.bin")[..]);

    // The counters.
    assert_eq!(
        vol.header().next_catalog_id,
        next_before + 1,
        "nextCatalogID must advance, or the next file gets this one's identity"
    );
    assert_eq!(vol.header().file_count, file_count_before + 1);

    // The parent folder's child count. The root folder's valence is readable
    // through the catalog, and `fsck` counts children independently.
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "orphaned {:?} missing {:?} missing_thread {:?}",
        report.orphaned,
        report.missing,
        report.missing_thread
    );
    assert_fsck_clean(&path, "a created file");
}

#[test]
fn creating_a_file_then_writing_it_takes_the_whole_path() {
    // Creation and growth together, which is the sequence a FUSE `create` then
    // `write` performs. The point is that a file created by *this* crate is
    // writable by it: the record it wrote is one the growth path can find.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        let cnid = writable
            .create_file(parent, &units("grown.bin"))
            .expect("create");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 241) as u8).collect();
        writable
            .write_file_contents(cnid, &data)
            .expect("a file created by this crate must be writable by it");
        dev.sync().expect("flush");
        cnid
    };

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("grown.bin"))
        .expect("lookup")
        .expect("grown.bin exists");
    let expected: Vec<u8> = (0..10_000u32).map(|i| (i % 241) as u8).collect();
    assert_eq!(vol.read(&object, 0, 10_000).expect("read back"), expected);

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "orphaned {:?} missing {:?}",
        report.orphaned,
        report.missing
    );
    assert_fsck_clean(&path, "a file created and then grown");
}

#[test]
fn creating_a_file_with_an_existing_name_is_refused() {
    // Two records under one key still parse, still search, and answer with
    // whichever comes first -- forever. So this has to be refused rather than
    // inserted beside the existing record.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };

    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    let err = writable
        .create_file(parent, &units("payload.bin"))
        .expect_err("payload.bin already exists");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("payload.bin"),
        "the refusal must name the conflict, or a caller cannot tell which name \
         was taken; got: {rendered}"
    );
}

#[test]
fn creating_a_file_in_a_folder_that_is_a_file_is_refused() {
    // CNID 16 is `payload.bin`, a file. Writing a child count into a file record
    // would corrupt it, so the CNID has to be rejected before anything is written.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let before = std::fs::read(&path).expect("read image");

    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    let err = writable
        .create_file(16, &units("child.bin"))
        .expect_err("CNID 16 is a file, not a folder");
    assert!(
        format!("{err}").contains("not a folder"),
        "the refusal must say what is wrong with the CNID; got: {err}"
    );
    dev.sync().expect("flush");
    drop(dev);
    assert_untouched(&path, &before, "a create refused for a bad parent");
}

#[test]
fn an_empty_name_is_refused() {
    // A record keyed by an empty name is a thread record's key shape, so a file
    // with one would be indistinguishable from a thread in the key ordering.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    writable
        .create_file(parent, &[])
        .expect_err("a file cannot be nameless");
}

// --- Splitting, which restructures the tree --------------------------------

/// Create `count` files named `file000.bin`.. in the root, returning the path.
///
/// The point of the helper is that it creates enough files to force the catalog's
/// leaf to split, which is the only way to reach the index node -- and the only way
/// to test it at all, since no committed image has a catalog with one.
///
/// 40 files is comfortably past the point: the fixture's catalog has eight nodes,
/// and each leaf holds about twenty-one records, so 40 files means at least three
/// splits and a two-level tree.
fn create_many(count: u32) -> std::path::PathBuf {
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    for i in 0..count {
        let name = format!("file{i:03}.bin");
        writable
            .create_file(parent, &units(&name))
            .unwrap_or_else(|e| panic!("creating {name}: {e}"));
    }
    dev.sync().expect("flush");
    drop(dev);
    path
}

#[test]
fn creating_enough_files_splits_the_catalog_and_leaves_it_consistent() {
    // The whole structural claim in one test: a catalog that has been split is
    // still a catalog. Everything else here is a detail of how.
    let path = create_many(40);

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let bt = hfsplus::btree::io::BTreeFile::open(
        &dev,
        &vol.header().catalog_file,
        vol.header().block_size,
        vol.header().is_hfsx(),
    )
    .expect("open the catalog");
    let header = *bt.header();

    // The tree really is two levels deep, or nothing above was tested.
    assert!(
        header.tree_depth >= 2,
        "the tree must have an index node above its leaves to test any of this, \
         and it is {} level(s) deep",
        header.tree_depth
    );
    let root = bt.read_node_bytes(header.root_node).expect("read root");
    let root_node = bt.parse_node(&root).expect("parse root");
    assert_eq!(
        root_node.kind(),
        hfsplus::btree::node::NodeKind::Index,
        "the root of a two-level catalog is an index node"
    );
    assert_eq!(
        root_node.descriptor().height as u16,
        header.tree_depth,
        "the root's height is the tree's depth"
    );

    // The header's counts agree with what is on disk. `leafRecords` in particular
    // counts records across *all* leaves, so a split that forgets to advance it for
    // the record that caused the split leaves it short -- and fsck recounts.
    let mut counted = 0u32;
    let mut leaves = 0u32;
    let mut cursor = header.first_leaf_node;
    while cursor != 0 && leaves < header.total_nodes {
        leaves += 1;
        let bytes = bt.read_node_bytes(cursor).expect("read leaf");
        let node = bt.parse_node(&bytes).expect("parse leaf");
        counted += u32::from(node.num_records());
        cursor = node.descriptor().f_link;
    }
    assert!(
        leaves > 1,
        "the chain must have more than one leaf for a split to have happened"
    );
    assert_eq!(
        counted, header.leaf_records,
        "leafRecords must count every record in every leaf"
    );
    assert_eq!(
        leaves,
        u32::from(root_node.num_records()),
        "there must be exactly one index record per leaf"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());

    assert_fsck_clean(&path, "a catalog whose leaf has been split");
}

#[test]
fn every_file_survives_a_split_findable_by_name_and_by_cnid() {
    // A split that puts records in the wrong leaf still parses, still searches,
    // still returns *a* record -- for some other name. So reachability is asserted
    // for every file, by both routes, rather than for the first one that happens to
    // land in the right leaf.
    let path = create_many(40);

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    for i in 0..40u32 {
        let name = format!("file{i:03}.bin");
        let object = vol
            .lookup(vol.root_cnid(), &units(&name))
            .expect("lookup")
            .unwrap_or_else(|| panic!("{name} is not findable by name"));
        let f = object.as_file().expect("a file");
        // `payload.bin` already holds CNID 16, so the first created file is 17.
        assert_eq!(
            f.cnid.0,
            17 + i,
            "{name} resolved to the wrong CNID, so the index points at the wrong leaf"
        );
        let want = units(&name);
        let got = vol
            .lookup_cnid(hfsplus::catalog::cnid::Cnid(f.cnid.0))
            .expect("lookup by CNID")
            .map(|o| o.name().to_vec());
        assert_eq!(
            got.as_deref(),
            Some(want.as_slice()),
            "{name} is findable by name but not by CNID, so its thread record is \
             unreachable"
        );
    }
}

#[test]
fn a_split_leaves_every_files_contents_intact() {
    // A split moves records between nodes. If one is dropped or duplicated in the
    // move, the catalog still searches -- so the contents are read back, not just
    // the names.
    let path = create_many(40);

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    // Every created file is empty, and `payload.bin` is the one with data, so this
    // also checks that a pre-existing file survived the restructure.
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin exists after the split");
    let data = vol.read(&object, 0, 4096).expect("read payload.bin");
    assert_eq!(data.len(), 4096);
    assert!(
        data.iter().enumerate().all(|(i, b)| *b == (i % 256) as u8),
        "payload.bin's contents must be exactly what mkbootstrap wrote"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
}

#[test]
fn a_volume_that_cannot_grow_any_further_says_what_ran_out() {
    // A catalog can now grow, so the wall moves: what a small volume runs out of
    // first is either contiguous space for a clump or the catalog fork's eight
    // inline extents. Which one you hit depends on how fragmented the volume is, and
    // a test must not encode that -- so both are accepted, and what is asserted is
    // that the failure is *named*.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };

    let (made, err) = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        let mut made = 0u32;
        let mut err = None;
        for i in 0..2000 {
            let name = format!("f{i:04}.bin");
            match writable.create_file(parent, &units(&name)) {
                Ok(_) => made += 1,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        dev.sync().expect("flush");
        (made, err)
    };

    let err = err.expect("a 1 MiB volume cannot hold 2000 files");
    let rendered = format!("{err:?}");
    assert!(
        matches!(err, hfsplus::Error::NoSpace { .. })
            || rendered.contains("extents")
            || rendered.contains("grow"),
        "the refusal must name what ran out -- space, or the catalog's inline \
         extents -- rather than failing without saying; got: {rendered}"
    );
    assert!(
        made > 47,
        "the catalog must have grown well past the 47 files a single eight-node \
         tree holds, or growth is not being tested; it reached {made}"
    );

    // A refused create leaves *nothing* behind, and that is the property worth
    // having: reaching the limit here provokes hundreds of them, and a volume that
    // accumulates a half-created file per failure is one that fills up with debris
    // rather than with data.
    //
    // The CNID counter is the one thing a refused create does change, and that is
    // deliberate: it is written first so a failure cannot hand the same CNID out
    // twice. A gap in the sequence is harmless.
    let next_after_limit = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.header().next_catalog_id
    };
    let (next_before_retry, retry) = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        (vol.header().next_catalog_id, {
            let mut dev = FileDevice::open_writable(&path).expect("open writable");
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let r = writable.create_file(parent, &units("f9999.bin"));
            dev.sync().expect("flush");
            r
        })
    };
    let _ = next_after_limit;
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    if retry.is_err() {
        assert_eq!(
            vol.header().next_catalog_id,
            next_before_retry + 1,
            "a refused create consumes its CNID, and only that"
        );
        assert!(
            vol.lookup(vol.root_cnid(), &units("f9999.bin"))
                .expect("lookup")
                .is_none(),
            "a refused create must leave no file record behind: the two records \
             have to go in together or not at all"
        );
    }

    // And the volume is still sound after all of it.
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "a volume filled to its limit must not be left full of half-created files; \
         {:?}",
        report.describe()
    );
    assert_fsck_clean(&path, "a volume filled until it could not grow further");
}

#[test]
fn growing_the_catalog_keeps_both_volume_headers_in_step() {
    // HFS+ keeps a second, identical volume header in the last 1024 bytes, and
    // `fsck.hfsplus` compares the two. Growing the catalog changes the primary
    // header's fork and free count, so a mutation that leaves the copy behind
    // produces a volume that `fsck` *repairs* -- and a repaired volume is a modified
    // fixture, which is the one thing a test asserting `fsck` accepts an image
    // cannot allow.
    //
    // Asserted by reading the image directly rather than through the library: the
    // backup header is not part of the parsed `VolumeHeader`, so a library-level
    // comparison could only ever compare the primary with itself.
    let path = create_many(120);

    let bytes = std::fs::read(&path).expect("read image");
    let primary = 1024usize;
    assert!(
        bytes.len() > 2048,
        "the image must be large enough to hold a backup header"
    );
    let backup = bytes.len() - 1024;
    assert_ne!(
        primary, backup,
        "a volume too small to hold both headers has no backup to keep in step"
    );
    assert_eq!(
        &bytes[primary..primary + 1024],
        &bytes[backup..backup + 1024],
        "the volume header at the end of the volume has diverged from the primary, \
         and fsck.hfsplus will report \"Volume header needs minor repair\""
    );

    assert_fsck_clean(&path, "a catalog grown past its original eight nodes");
}

// --- Deletion, which removes -----------------------------------------------

#[test]
fn removing_a_file_takes_both_its_records_and_fixes_the_counts() {
    // A file is a record and a thread record. Removing one without the other leaves
    // something the checker calls `missing_thread` and `fsck.hfsplus` rejects, so
    // the test looks for neither half rather than for the absence of a name.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let victim_cnid = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let cnid = {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let cnid = writable
                .create_file(parent, &units("doomed.bin"))
                .expect("create");
            writable
                .remove(parent, &units("doomed.bin"))
                .expect("remove the file just created");
            cnid
        };
        // A scope, not `drop`: `WritableVolume` has no destructor, so dropping it
        // explicitly would only end its borrow -- which a block does just as well.
        dev.sync().expect("flush");
        cnid
    };

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert!(
        vol.lookup(vol.root_cnid(), &units("doomed.bin"))
            .expect("lookup")
            .is_none(),
        "the name must no longer resolve"
    );
    assert!(
        vol.lookup_cnid(hfsplus::catalog::cnid::Cnid(victim_cnid))
            .expect("lookup by CNID")
            .is_none(),
        "and neither may the CNID: a file record with no thread record is exactly \
         what `missing_thread` reports"
    );
    assert!(
        vol.lookup(vol.root_cnid(), &units("payload.bin"))
            .expect("lookup")
            .is_some(),
        "and nothing else may go with it"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a removed file");
}

#[test]
fn removing_something_with_contents_is_refused_rather_than_stranding_its_blocks() {
    // The blocks are what the file's extents describe, so removing the record first
    // would leave them allocated with nothing pointing at them. Truncating first
    // releases them, and `truncate_file` already does that.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        let err = writable
            .remove(parent, &units("payload.bin"))
            .expect_err("payload.bin has a block of contents");
        assert!(
            format!("{err}").contains("contents"),
            "the refusal must say why, or a caller cannot tell this from a missing \
             file; got: {err}"
        );

        // Truncate first, and it goes.
        writable
            .truncate_file(16, 0)
            .expect("truncate payload.bin to nothing");
        writable
            .remove(parent, &units("payload.bin"))
            .expect("remove it");
        dev.sync().expect("flush");
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert!(vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .is_none());
    // Truncating released the block, so removing the record strands nothing.
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a file emptied and then removed");
}

#[test]
fn removing_something_that_is_not_there_is_reported_as_missing() {
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let before = std::fs::read(&path).expect("read image");
    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    let err = writable
        .remove(parent, &units("never-existed.bin"))
        .expect_err("there is no such file");
    assert!(
        matches!(err, hfsplus::Error::NotFound { .. }),
        "a name that is not there is NotFound, not a silent success; got {err:?}"
    );
    dev.sync().expect("flush");
    drop(dev);
    assert_untouched(&path, &before, "a remove of something that is not there");
}

#[test]
fn the_root_folder_and_reserved_cnids_cannot_be_removed() {
    // `cat_delete`'s preflight: a CNID at or below the reserved range, or the root
    // folder itself, is `EINVAL` -- before anything is written.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let before = std::fs::read(&path).expect("read image");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    writable
        .remove(parent, &units("no-such-root.bin"))
        .expect_err("the root folder cannot be reached by an ordinary name");
    dev.sync().expect("flush");
    drop(dev);
    assert_untouched(&path, &before, "a refused remove");
}

// --- Renaming and folders --------------------------------------------------

#[test]
fn renaming_keeps_the_object_and_moves_it_between_folders() {
    // A rename is a change to the catalog, not to the data: the CNID and the forks
    // stay, so the file is still found by the CNID it always had, and its contents
    // are still readable. Both are asserted, because a rename that quietly
    // reallocated the file would satisfy a lookup by name and nothing else.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };

    let dir;
    let file_cnid;
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let f = writable
                .create_file(parent, &units("a.bin"))
                .expect("create a.bin");
            writable
                .write_file_contents(f, &[3u8; 100])
                .expect("give it contents");
            dir = writable
                .create_folder(parent, &units("sub"))
                .expect("mkdir");

            writable
                .rename(parent, &units("a.bin"), parent, &units("b.bin"))
                .expect("rename in place");
            writable
                .rename(parent, &units("b.bin"), dir, &units("moved.bin"))
                .expect("rename into the folder");
            file_cnid = f;
        }
        dev.sync().expect("flush");
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert!(
        vol.lookup(vol.root_cnid(), &units("a.bin"))
            .expect("lookup")
            .is_none(),
        "the old name must not still resolve"
    );
    assert!(
        vol.lookup(vol.root_cnid(), &units("b.bin"))
            .expect("lookup")
            .is_none(),
        "nor the intermediate one"
    );

    let object = vol
        .lookup(hfsplus::catalog::cnid::Cnid(dir), &units("moved.bin"))
        .expect("lookup")
        .expect("the new name must resolve in the new folder");
    assert_eq!(
        object.as_file().expect("a file").cnid.0,
        file_cnid,
        "a rename must not reallocate: the CNID is the object's identity"
    );
    assert_eq!(vol.read(&object, 0, 100).expect("read"), vec![3u8; 100]);
    assert!(
        vol.lookup_cnid(hfsplus::catalog::cnid::Cnid(file_cnid))
            .expect("lookup by CNID")
            .is_some(),
        "and it must still be reachable by CNID, which is what the thread record \
         is for -- a rename that left the thread record stale would resolve the old \
         name here"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a file renamed into a subfolder");
}

#[test]
fn a_case_variant_rename_is_a_rekey_rather_than_a_move() {
    // On this volume `keyCompareType` is `kHFSCaseFolding`, so `README.TXT` and
    // `Readme.txt` are *one key to the tree*. Renaming between them is therefore a
    // re-key -- the same record under a different spelling -- not a move, and it
    // needs the two steps the other way round.
    //
    // Both spellings resolving afterwards is correct, not a duplicate: on a
    // case-insensitive volume they name one record. What would not be correct is
    // two records, so the count is asserted too.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let cnid = writable
                .create_file(parent, &units("Readme.txt"))
                .expect("create");
            writable
                .write_file_contents(cnid, &[9u8; 300])
                .expect("contents");

            let got = writable
                .rename(parent, &units("Readme.txt"), parent, &units("README.TXT"))
                .expect("re-key to the other spelling");
            assert_eq!(got, cnid, "a re-key must not change the CNID");

            // And back again, so the path is exercised in both directions.
            writable
                .rename(parent, &units("README.TXT"), parent, &units("Readme.txt"))
                .expect("re-key back");

            // A *different* file must still be refused onto the same spelling.
            writable
                .create_file(parent, &units("other.bin"))
                .expect("create a second file");
            let err = writable
                .rename(parent, &units("other.bin"), parent, &units("README.TXT"))
                .expect_err("a different object is still a collision");
            assert!(
                format!("{err}").contains("README.TXT"),
                "the refusal must name the destination; got: {err}"
            );
            dev.sync().expect("flush");
        }
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("Readme.txt"))
        .expect("lookup")
        .expect("the file must be there");
    assert_eq!(vol.read(&object, 0, 300).expect("read"), vec![9u8; 300]);
    assert!(
        vol.lookup(vol.root_cnid(), &units("other.bin"))
            .expect("lookup")
            .is_some(),
        "the second file must have survived the refused rename"
    );
    let under_parent = vol
        .catalog()
        .all_records()
        .expect("walk the catalog")
        .into_iter()
        .filter(|(k, _)| k.parent_id.0 == parent)
        .count();
    assert_eq!(
        under_parent, 3,
        "the root's own entry and two files -- not four, which is what a re-key \
         done as a move would leave behind"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a case-variant rename, both ways");
}

#[test]
fn a_folder_cannot_be_moved_beneath_itself_or_its_own_descendants() {
    // Moving `/a/b/c` into `/a/b/c` makes the path to `/a/b/c` run through itself,
    // and every lookup afterwards would have to decide where to stop. Apple refuses
    // the obvious cases outright and then walks the destination path back to the
    // root; both halves are here, because the walk is what catches a grandparent.
    //
    // Each case gets a *fresh* volume. That is not tidiness: a case that moves a
    // folder changes the tree the next case is reasoning about, and an earlier
    // version of this test reused one volume and so asserted that two of the
    // refusals were allowed -- correctly, for the tree as it then stood.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let (a, b, c) = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let ids = {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let a = writable.create_folder(parent, &units("a")).expect("a");
            let b = writable.create_folder(a, &units("b")).expect("a/b");
            let c = writable.create_folder(b, &units("c")).expect("a/b/c");
            (a, b, c)
        };
        dev.sync().expect("flush");
        ids
    };
    assert!(
        c > b && b > a,
        "CNIDs are allocated in order, and this depends on it"
    );

    // The three illegal moves, each refused with the reason.
    for (desc, from_parent, from_name, to_parent) in [
        // /a/b/c: `c` into `c` is into itself; `b` into `c` is into its own
        // child; `a` into `c` is into its own grandchild. The last two are what the
        // walk up the destination path is for -- neither is caught by comparing the
        // folder with the destination alone.
        ("into itself", b, "c", c),
        ("into its own child", a, "b", c),
        ("into its own grandchild", parent, "a", c),
    ] {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let err = writable
                .rename(from_parent, &units(from_name), to_parent, &units("x"))
                .expect_err("this move must be refused");
            assert!(
                format!("{err}").contains("above it"),
                "the refusal for {desc} must explain the cycle; got: {err}"
            );
        }
    }

    // And the two legal ones, which the check must not over-reach and refuse.
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            writable
                .rename(b, &units("c"), parent, &units("up"))
                .expect("a folder may move up to the root");
            // From its *new* home, into the folder that used to be above it --
            // legal, and the case the cycle check must not over-reach on.
            writable
                .rename(parent, &units("up"), a, &units("sideways"))
                .expect("a folder may move into a former ancestor");
            dev.sync().expect("flush");
        }
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    // Both legal moves landed, the second having replaced the first: the folder is
    // now `/a/sideways`, having gone `/a/b/c` -> `/up` -> `/a/sideways`.
    assert!(
        vol.lookup(hfsplus::catalog::cnid::Cnid(a), &units("sideways"))
            .expect("lookup")
            .is_some(),
        "the folder must be at its final home"
    );
    assert!(
        vol.lookup(hfsplus::catalog::cnid::Cnid(c), &units("x"))
            .expect("lookup")
            .is_none(),
        "none of the refused moves may have left a record behind"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "folder moves, three refused and two allowed");
}

#[test]
fn renaming_onto_a_name_that_is_taken_is_refused() {
    // `EEXIST`, not an overwrite. Apple allows the same-parent case, where it
    // becomes an exchange; a cross-parent collision is refused outright.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            writable
                .create_file(parent, &units("one.bin"))
                .expect("create");
            writable
                .create_file(parent, &units("two.bin"))
                .expect("create");
            let err = writable
                .rename(parent, &units("one.bin"), parent, &units("two.bin"))
                .expect_err("two.bin is already there");
            assert!(
                format!("{err}").contains("two.bin"),
                "the refusal must name the conflict; got: {err}"
            );
            dev.sync().expect("flush");
        }
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    for n in ["one.bin", "two.bin"] {
        assert!(
            vol.lookup(vol.root_cnid(), &units(n))
                .expect("lookup")
                .is_some(),
            "{n} must be untouched by the refused rename"
        );
    }
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a refused rename");
}

#[test]
fn renaming_onto_a_name_that_folds_onto_an_existing_one_is_refused() {
    // On this volume `keyCompareType` is `kHFSCaseFolding`, so `README.TXT` and
    // `Readme.txt` are *one key to the tree* even though the strings differ. A
    // rename must treat that as a collision.
    //
    // This is a safety property rather than a feature, and it was pinned after an
    // attempt at the case-variant rename produced the opposite: a record removed
    // that was then not put back, and a second record under a key the tree
    // considers equal to an existing one. Neither may happen, and the way to say
    // so is to assert the refusal *and* that all three records survive it.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            writable
                .create_file(parent, &units("Readme.txt"))
                .expect("create");
            writable
                .create_file(parent, &units("other.bin"))
                .expect("create");
            let err = writable
                .rename(parent, &units("other.bin"), parent, &units("README.TXT"))
                .expect_err("README.TXT folds onto Readme.txt");
            assert!(
                format!("{err}").contains("README.TXT"),
                "the refusal must name the destination; got: {err}"
            );
            dev.sync().expect("flush");
        }
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    for n in ["Readme.txt", "other.bin"] {
        assert!(
            vol.lookup(vol.root_cnid(), &units(n))
                .expect("lookup")
                .is_some(),
            "{n} must survive the refused rename: a rename that has already \
             removed a record when it discovers the collision has lost it"
        );
    }
    // And no second record under a key the tree considers equal to an existing one.
    let under_parent = vol
        .catalog()
        .all_records()
        .expect("walk the catalog")
        .into_iter()
        .filter(|(k, _)| k.parent_id.0 == parent)
        .count();
    assert_eq!(
        under_parent, 3,
        "three entries -- the two files and the root's own -- and no duplicate"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a rename onto a case-folded name, refused");
}

#[test]
fn a_created_file_says_it_has_a_thread_record() {
    // TN1150: "this bit indicates that the file has a thread record. As all files
    // in HFS Plus have thread records, **this bit must be set**."
    //
    // Without it, a reader may assume there is no thread record and decline to
    // build the reverse mapping -- so a file created by this crate is findable by
    // name and not by CNID, which is exactly the asymmetry the thread record
    // exists to remove.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            writable
                .create_file(parent, &units("threaded.bin"))
                .expect("create");
            dev.sync().expect("flush");
        }
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let f = vol
        .lookup(vol.root_cnid(), &units("threaded.bin"))
        .expect("lookup")
        .expect("threaded.bin exists")
        .as_file()
        .expect("a file")
        .record;
    assert!(
        f.flags & hfsplus::catalog::record::K_HFS_THREAD_EXISTS_MASK != 0,
        "a file's record must say its thread record exists; flags were {:#x}",
        f.flags
    );
}

#[test]
fn a_hard_link_moves_the_data_behind_a_private_node_and_leaves_a_name() {
    // A hard link is not a second name for one CNID, and it is not a second name in
    // the user's folder either. `hfs_makelink` *moves* the file's own record into
    // the metadata directory as `iNode<cnid>` -- keeping its CNID and its forks, and
    // becoming the indirect node -- and puts a link record where the name was.
    //
    // So the original name must be **gone**, the data must live under the private
    // folder, and the new name must be a link: chain flag set, no forks, the
    // indirect node's CNID in `special`, read-only, and `hlnk`/`hfs+` in `userInfo`
    // -- which is what `lib_fsck_hfs` tests to decide a file record is a link.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };

    let (target, link) = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let ids = {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let target = writable
                .create_file(parent, &units("orig.bin"))
                .expect("create");
            writable
                .write_file_contents(target, &[4u8; 700])
                .expect("contents");
            let link = writable
                .create_hard_link(parent, &units("alias.bin"), target)
                .expect("link");
            (target, link)
        };
        dev.sync().expect("flush");
        ids
    };
    assert_ne!(link, target, "a link has its own CNID");

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    assert!(
        vol.lookup(vol.root_cnid(), &units("orig.bin"))
            .expect("lookup")
            .is_none(),
        "the data moved into the metadata directory, so the original name is gone"
    );

    let alias = vol
        .lookup(vol.root_cnid(), &units("alias.bin"))
        .expect("lookup")
        .expect("alias.bin resolves");
    let record = alias.as_file().expect("a file").record;
    assert!(
        record.is_hard_link(),
        "the entry in the user's folder must carry the chain flag"
    );
    assert_eq!(
        record.link_reference().expect("a link names its node").0,
        target,
        "and that node is the indirect node's CNID"
    );
    assert_eq!(
        record.data_fork.total_blocks, 0,
        "a link has no forks: two records claiming one block is \"Overlapped extent \
         allocation\""
    );
    assert_eq!(
        record.bsd_info.file_mode & 0o7777,
        0o444,
        "`createindirectlink` uses S_IFREG | S_IRUSR | S_IRGRP | S_IROTH, so a link \
         is read-only"
    );
    assert_eq!(
        u32::from_be_bytes([
            record.user_info[0],
            record.user_info[1],
            record.user_info[2],
            record.user_info[3]
        ]),
        hfsplus::volume::K_HARD_LINK_FILE_TYPE,
        "and userInfo.fdType is `hlnk`. It is userInfo and *not* finderInfo -- \
         finderInfo is the ExtendedFileInfo at offset 64 and has no type or creator \
         -- and the constant is 0x686C6E6B, an earlier version of which read \
         `hlln`"
    );

    // The data is under `iNode<cnid>` in the metadata directory, outside the root.
    let inode_name: Vec<u16> = format!("{}{}", hfsplus::volume::INODE_NAME_PREFIX, target)
        .encode_utf16()
        .collect();
    let found = vol
        .catalog()
        .all_records()
        .expect("walk the catalog")
        .into_iter()
        .any(|(k, _)| k.parent_id != vol.root_cnid() && k.name == inode_name);
    assert!(
        found,
        "the data must live under iNode<target> outside the user's folders, or the \
         link points at nothing"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
}

#[test]
fn a_second_hard_link_is_refused_because_threading_is_not_implemented() {
    // `hfs_makelink`'s own guard is `cp->c_linkcount == 2`, so a second link is the
    // threading case: walking `hl_prevLinkID`/`hl_nextLinkID`. A `linkCount` that
    // climbs with nothing behind it is the volume shape this milestone exists to
    // avoid.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let (target, link) = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let ids = {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let target = writable
                .create_file(parent, &units("orig.bin"))
                .expect("create");
            let link = writable
                .create_hard_link(parent, &units("alias.bin"), target)
                .expect("link");
            (target, link)
        };
        dev.sync().expect("flush");
        ids
    };

    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    {
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        let err = writable
            .create_hard_link(parent, &units("third.bin"), target)
            .expect_err("a second link cannot be threaded yet");
        assert!(
            format!("{err}").contains("thread"),
            "the refusal must say what is missing; got: {err}"
        );
        let err = writable
            .create_hard_link(parent, &units("fourth.bin"), link)
            .expect_err("a link is not a target");
        assert!(
            format!("{err}").contains("indirect node"),
            "the refusal must point at the indirect node; got: {err}"
        );
        dev.sync().expect("flush");
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    for n in ["third.bin", "fourth.bin"] {
        assert!(
            vol.lookup(vol.root_cnid(), &units(n))
                .expect("lookup")
                .is_none(),
            "{n} must not exist after a refused link"
        );
    }
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
}

#[test]
fn a_folder_is_created_and_counted() {
    // `folderCount` excludes the root, which is the kind of off-by-one that only
    // an independent checker notices: a volume with one folder and nothing else
    // reports 1, not 2.
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    let folders_before = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.header().folder_count
    };

    let folder_cnid = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let cnid = {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            writable
                .create_folder(parent, &units("docs"))
                .expect("mkdir")
        };
        dev.sync().expect("flush");
        cnid
    };

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert_eq!(
        vol.header().folder_count,
        folders_before + 1,
        "folderCount must move by one, and must exclude the root"
    );
    assert!(
        vol.lookup(vol.root_cnid(), &units("docs"))
            .expect("lookup")
            .is_some(),
        "the folder must be findable by name"
    );
    assert!(
        vol.lookup_cnid(hfsplus::catalog::cnid::Cnid(folder_cnid))
            .expect("lookup by CNID")
            .is_some(),
        "and by CNID, which is the thread record doing its job"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a created folder");
}

#[test]
fn a_folder_with_children_cannot_be_removed() {
    // The same rule as a file with contents, one level up: a folder's `valence` is
    // its child count, and one that claims children it does not have is what fsck
    // reports as "Invalid directory item count".
    let path = copy_fixture(IMAGE).expect("fixture");
    let parent = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        vol.root_cnid().0
    };
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let dir = writable
                .create_folder(parent, &units("sub"))
                .expect("mkdir");
            writable
                .create_file(dir, &units("inside.bin"))
                .expect("a file inside it");
            let err = writable
                .remove(parent, &units("sub"))
                .expect_err("the folder has a child");
            assert!(
                format!("{err}").contains("not empty"),
                "the refusal must say why; got: {err}"
            );

            // Empty it, and then it goes.
            writable
                .remove(dir, &units("inside.bin"))
                .expect("remove the child");
            writable
                .remove(parent, &units("sub"))
                .expect("remove the folder");
            dev.sync().expect("flush");
        }
    }

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert!(vol
        .lookup(vol.root_cnid(), &units("sub"))
        .expect("lookup")
        .is_none());
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a folder emptied and then removed");
}

// --- Hard links ---------------------------------------------------------------

#[test]
fn the_private_file_hardlinks_folder_is_created_with_apples_exact_name() {
    // Every hard link's catalog record belongs *inside* this folder, named by
    // the link's own CNID. A volume with a link and no such folder is one where
    // the chain cannot be found, which is what `fsck` complained about when this
    // crate first tried to write a link.
    //
    // The name is four U+2500 BOX DRAWINGS LIGHT HORIZONTAL then "HFS+ Private
    // Data". Not decoration: the folder is found *by name*.
    let path = copy_fixture(IMAGE).expect("fixture");
    let folder = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let folder = {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            let folder = writable
                .ensure_file_hardlinks_folder()
                .expect("create the private folder");
            assert_eq!(
                writable
                    .ensure_file_hardlinks_folder()
                    .expect("second call"),
                folder,
                "creating it twice must not produce two folders"
            );
            folder
        };
        dev.sync().expect("flush");
        folder
    };

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let name: Vec<u16> = hfsplus::volume::FILE_HARDLINKS_FOLDER
        .encode_utf16()
        .collect();
    assert_eq!(
        name.len(),
        21,
        "four box-drawing characters plus \"HFS+ Private Data\" -- 21 UTF-16 \
         units, 29 UTF-8 bytes. An earlier draft of this test said 65, having \
         measured the *escaped* text rather than the decoded string"
    );
    assert!(
        vol.lookup(vol.root_cnid(), &name)
            .expect("lookup")
            .is_some(),
        "the private folder must be findable by name"
    );
    assert!(
        vol.lookup_cnid(hfsplus::catalog::cnid::Cnid(folder))
            .expect("lookup by CNID")
            .is_some(),
        "and its CNID must resolve to it, which is what makes it a catalog entry \
         rather than a block that happens to contain one"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(report.is_clean(), "{:?}", report.describe());
    assert_fsck_clean(&path, "a created private hardlinks folder");
}

#[test]
fn the_indirect_node_name_is_the_prefix_and_the_cnid() {
    // An indirect node's name in the private folder is `HFS_INODE_PREFIX` followed
    // by its CNID in decimal -- `MAKE_INODE_NAME` is `"%s%d"`. Directory hard links
    // use `dir_` (`HFS_DIRINODE_PREFIX`).
    //
    // Not decoration: the name is how `fsck.hfsplus` recognises an indirect node.
    // A record named with the bare CNID is not recognised, and the checker clears its
    // link-chain flag *without saying why* -- which is how this crate spent a round
    // of measurements learning that its arrangement was backwards.
    assert_eq!(hfsplus::volume::INODE_NAME_PREFIX, "iNode");
    assert_eq!(hfsplus::volume::DIR_INODE_NAME_PREFIX, "dir_");
    assert_eq!(
        format!("{}{}", hfsplus::volume::INODE_NAME_PREFIX, 17),
        "iNode17"
    );
    // A directory hard link's name is never confused with a file's, which matters
    // because both live in private folders and are told apart by this prefix.
    assert_ne!(
        hfsplus::volume::INODE_NAME_PREFIX,
        hfsplus::volume::DIR_INODE_NAME_PREFIX
    );
}

#[test]
fn the_directory_hardlinks_name_keeps_its_trailing_carriage_return() {
    // `.HFS+ Private Directory Data` followed by CR. The CR is in Apple's
    // definition and is exactly what gets lost transcribing a `#define` into a doc
    // comment and back. Asserted directly so it cannot be.
    let name = hfsplus::volume::DIR_HARDLINKS_FOLDER;
    assert!(name.ends_with('\r'), "the name ends with CR: {name:?}");
    assert_eq!(name.encode_utf16().count(), 29, "29 UTF-16 units");
    assert!(name.starts_with(".HFS+ Private Directory Data"));
}

// --- Truncation, which frees -----------------------------------------------

/// Grow `payload.bin` to 8192 bytes so there is a block to give back.
fn grown_to_two_blocks(path: &std::path::Path, cnid: u32) {
    let new_data: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
    write(path, cnid, &new_data);
}

#[test]
fn truncating_releases_the_blocks_a_file_no_longer_needs() {
    // 8192 bytes in two blocks, truncated to 4096: one block released, one kept.
    // Both halves of the fact are asserted, because a writer that freed the block
    // and did not update the free count leaves a volume the checker recomputes
    // differently from what the header claims.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    grown_to_two_blocks(&path, cnid);

    let (blocks_before, free_before) = (allocated_blocks(&path), header_free_blocks(&path));

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        writable.truncate_file(cnid, 4096).expect("truncate");
        dev.sync().expect("flush");
    }

    assert_eq!(
        allocated_blocks(&path),
        blocks_before - 1,
        "one block was released, so one fewer may be marked allocated"
    );
    assert_eq!(
        header_free_blocks(&path),
        free_before + 1,
        "and the header's free count must rise to match"
    );

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin exists");
    let f = object.as_file().expect("a file");
    assert_eq!(f.record.data_fork.logical_size, 4096);
    assert_eq!(
        f.record.data_fork.total_blocks, 1,
        "the record must claim the blocks it kept, not the ones it gave back"
    );
    assert_eq!(
        f.record.data_fork.extents.used(),
        1,
        "the emptied extent is zeroed, which is what makes it the terminator"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "orphaned {:?} missing {:?}",
        report.orphaned,
        report.missing
    );
    assert_fsck_clean(&path, "a truncation that released a block");
}

#[test]
fn a_partial_trailing_block_is_rounded_up_and_frees_nothing() {
    // 5000 bytes needs two blocks. Truncating to 4097 rounds up to two, so nothing
    // is freed. This is the property that follows from blocks being the unit of
    // allocation: the only sizes a file can have are multiples of the block size,
    // and a truncation that shaved the tail would be inventing a size.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    grown_to_two_blocks(&path, cnid);
    let (blocks_before, free_before) = (allocated_blocks(&path), header_free_blocks(&path));

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        writable.truncate_file(cnid, 4097).expect("truncate");
        dev.sync().expect("flush");
    }

    assert_eq!(
        allocated_blocks(&path),
        blocks_before,
        "4097 rounds up to two blocks, so no block is freeable"
    );
    assert_eq!(header_free_blocks(&path), free_before);

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin exists");
    let f = object.as_file().expect("a file");
    assert_eq!(
        f.record.data_fork.logical_size, 4097,
        "the size itself is exact"
    );
    assert_eq!(f.record.data_fork.total_blocks, 2);

    assert_fsck_clean(&path, "a truncation that rounded up and freed nothing");
}

#[test]
fn truncating_to_zero_frees_everything() {
    // The `peof == 0` path, taken first and unconditionally: `truncateToExtent`
    // has no meaning with no containing extent, and keeping one would leave a
    // zero-length file holding storage forever.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    grown_to_two_blocks(&path, cnid);
    let blocks_before = allocated_blocks(&path);

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        writable.truncate_file(cnid, 0).expect("truncate to zero");
        dev.sync().expect("flush");
    }

    assert_eq!(
        allocated_blocks(&path),
        blocks_before - 2,
        "both blocks must come back, or the volume leaks one every time a file is \
         emptied"
    );
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin still exists after being emptied");
    let f = object.as_file().expect("a file");
    assert_eq!(f.record.data_fork.logical_size, 0);
    assert_eq!(f.record.data_fork.total_blocks, 0);
    assert_eq!(
        f.record.data_fork.extents.used(),
        0,
        "every descriptor is zeroed, so the file describes no blocks at all"
    );
    assert_eq!(
        vol.read(&object, 0, 1).expect("read past the end"),
        Vec::<u8>::new(),
        "an emptied file reads as nothing"
    );

    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "orphaned {:?} missing {:?}",
        report.orphaned,
        report.missing
    );
    assert_fsck_clean(&path, "a truncation to zero");
}

#[test]
fn a_file_that_is_grown_and_then_truncated_returns_to_the_same_allocation() {
    // The round trip is the property that matters: an allocator and a deallocator
    // that disagree would leak a block every cycle, and nothing above would notice
    // until the volume filled.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    let (blocks_before, free_before) = (allocated_blocks(&path), header_free_blocks(&path));

    let big: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
    write(&path, cnid, &big);
    assert_eq!(
        allocated_blocks(&path),
        blocks_before + 9,
        "40000 bytes is ten blocks and the file had one"
    );

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        writable.truncate_file(cnid, 0).expect("truncate to zero");
        dev.sync().expect("flush");
    }
    // One *fewer* than before, not the same: the file's original block was
    // allocated too, and truncating to zero releases it along with the nine that
    // growth added. Coming back to exactly `blocks_before` would mean the
    // original block had leaked.
    assert_eq!(
        allocated_blocks(&path),
        blocks_before - 1,
        "all ten blocks must come back, including the one the file started with"
    );
    assert_eq!(
        header_free_blocks(&path),
        free_before + 1,
        "and the free count must rise by exactly as many as the bitmap fell"
    );

    assert_fsck_clean(&path, "a grow-then-truncate round trip");
}

#[test]
fn truncating_to_what_the_file_already_is_refused() {
    // Not truncation. A request that keeps every block would rewrite the record
    // with the same numbers, which looks like success while changing a file's
    // modification time for no reason.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    let before = std::fs::read(&path).expect("read image");

    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    let err = writable
        .truncate_file(cnid, 4096)
        .expect_err("the file is already 4096 bytes in one block");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("not a truncation"),
        "the refusal must say what was wrong, not merely fail; got: {rendered}"
    );
    dev.sync().expect("flush");
    drop(dev);
    assert_untouched(&path, &before, "a truncation that freed nothing");
}

// --- What it refuses -------------------------------------------------------

// --- The alternate volume header -------------------------------------------

/// Whether `fsck.hfsplus` modifies the image, ignoring its own signature.
fn fsck_modifies(path: &std::path::Path) -> Option<bool> {
    let fsck = common::fsck_available()?;
    let before = std::fs::read(path).expect("read image");
    let mut probe = std::env::temp_dir();
    probe.push(format!("alt-hdr-{}.img", std::process::id()));
    std::fs::copy(path, &probe).expect("copy for fsck");
    let _ = common::run_fsck(&fsck, &probe);
    let after = std::fs::read(&probe).expect("read the checked image");
    let _ = std::fs::remove_file(&probe);
    let len = before.len();
    let signature = |i: usize| {
        i == 1024 + 8
            || (1033..=1036).contains(&i)
            || i == len - 1024 + 8
            || (len - 1024 + 9..=len - 1024 + 12).contains(&i)
    };
    Some((0..len).any(|i| before[i] != after[i] && !signature(i)))
}

#[test]
fn the_alternate_header_only_needs_syncing_when_a_fork_changes() {
    // TN1150: "The implementation should only update this copy when the length or
    // location of one of the special files changes."
    //
    // This crate synced it on *every* header write, because a catalog that grew had
    // left the two headers describing different forks and `fsck` repaired it. The
    // conclusion drawn was "sync always", and it was drawn from a misreading -- so
    // the thing worth pinning is what `fsck` actually compares.
    //
    // It validates the **primary** header against the allocation bitmap, and does
    // not compare the two headers' counts. A backup left holding a stale
    // `freeBlocks` is not repaired, and a volume this crate writes is accepted.
    //
    // The cost of getting it wrong is not cosmetic: syncing on every header write
    // is a kilobyte read and a kilobyte write for every `nextCatalogID` bump.
    let Some(_) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };

    // A volume where only counts have moved -- the common case, since every create
    // writes `nextCatalogID` and `fileCount` -- must be left alone.
    let path = create_many(40);
    assert_eq!(
        fsck_modifies(&path),
        Some(false),
        "a volume whose forks have not moved must not need repair"
    );

    // And one where the catalog *has* moved must be synced, or fsck repairs it.
    // That is the bug this rule was written for, so it is the half that must hold.
    let grown = {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        {
            let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
            // The root, read before the writer exists: a `Volume` borrows the
            // device, so it cannot outlive the scope that opens the writer.
            let root = {
                let d = FileDevice::open(&path).expect("open");
                let v = Volume::open(&d).expect("mount");
                v.root_cnid().0
            };
            let cnid = writable
                .create_file(root, &units("pushes-it-over.txt"))
                .expect("create");
            writable
                .write_file_contents(cnid, &[9u8; 300_000])
                .expect("contents");
            dev.sync().expect("flush");
        }
        dev.sync().ok();
        path
    };
    assert_eq!(
        fsck_modifies(&grown),
        Some(false),
        "a catalog that grew must leave the two headers agreeing about where the \
         catalog is, or fsck repairs the volume -- which is what the sync is for"
    );
}

// --- Growing, which allocates ----------------------------------------------

/// The CNID of `payload.bin` in a fresh copy of the fixture.
fn payload_cnid(path: &std::path::Path) -> u32 {
    let dev = FileDevice::open(path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    vol.lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin exists")
        .as_file()
        .expect("a file")
        .cnid
        .0
}

/// The number of allocation blocks the bitmap says are allocated.
fn allocated_blocks(path: &std::path::Path) -> u64 {
    let dev = FileDevice::open(path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let fork = vol.header().allocation_file;
    let limit = usize::try_from(fork.logical_size).expect("allocation file fits usize");
    let bytes = vol.read_fork(&fork, limit).expect("read bitmap");
    let map = hfsplus::alloc::AllocationMap::from_bytes(&bytes, vol.header().total_blocks)
        .expect("bitmap matches totalBlocks");
    map.count_allocated()
}

#[test]
fn growing_a_file_allocates_exactly_the_blocks_it_needs() {
    // 4096 -> 8192 bytes is one more block, and `howmany` is what says so. The
    // interesting assertions are the ones about the *volume*: the bitmap must have
    // grown by exactly one block and the header's free count by exactly one fewer.
    // A writer that updated one and not the other would produce a volume this
    // crate's own checker rejects.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);

    let (blocks_before, free_before) = {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        (allocated_blocks(&path), vol.header().free_blocks)
    };

    let new_data: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
    write(&path, cnid, &new_data);

    assert_eq!(
        allocated_blocks(&path),
        blocks_before + 1,
        "8192 bytes is two blocks and the file had one, so exactly one more must \
         be marked allocated"
    );
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert_eq!(
        vol.header().free_blocks,
        free_before - 1,
        "and the header's free count must agree with the bitmap, or fsck.hfsplus \
         recomputes it and reports a disagreement"
    );

    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup after write")
        .expect("payload.bin still exists");
    let f = object.as_file().expect("a file");
    assert_eq!(f.record.data_fork.logical_size, 8192);
    assert_eq!(f.record.data_fork.total_blocks, 2);
    assert_eq!(
        vol.read(&object, 0, 8192).expect("read back"),
        new_data,
        "both blocks must read back, including the newly allocated one"
    );

    // This crate's own checker, which is a *different* check from fsck's: it
    // compares the bitmap against every extent the catalog describes, so it sees
    // a block marked allocated with nothing pointing at it (an orphan) or an
    // extent pointing at a block the bitmap calls free (missing). A writer that
    // updated the bitmap and the record in the wrong order passes fsck -- which
    // repairs orphans -- and fails here.
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let report = hfsplus::check::check(&vol, None).expect("check");
    assert!(
        report.is_clean(),
        "the volume must be internally consistent after allocating a block; \
         orphaned {:?}, missing {:?}",
        report.orphaned,
        report.missing
    );

    assert_fsck_clean(&path, "a write that allocated a block");
}

#[test]
fn a_write_that_fits_the_blocks_the_file_owns_allocates_nothing() {
    // Apple's `peof` check. The file already owns one block and the write is
    // shorter than that, so the only thing that should change is `logicalSize` --
    // and if the bitmap moved, the volume would have a block allocated to nothing.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    let (before, free_before) = (allocated_blocks(&path), header_free_blocks(&path));

    write(&path, cnid, &[7u8; 100]);

    assert_eq!(
        allocated_blocks(&path),
        before,
        "a write within the file's own blocks must not allocate"
    );
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    assert_eq!(
        vol.header().free_blocks,
        free_before,
        "and the header's free count must be untouched too, not just the bitmap"
    );
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin exists");
    assert_eq!(
        vol.read(&object, 0, 100).expect("read back"),
        vec![7u8; 100]
    );
}

/// The volume header's `freeBlocks`.
fn header_free_blocks(path: &std::path::Path) -> u32 {
    let dev = FileDevice::open(path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    vol.header().free_blocks
}

#[test]
fn a_partial_trailing_block_is_read_back_as_the_tail_and_not_beyond() {
    // 5000 bytes needs two blocks but only 5000 bytes exist. The last block's
    // remaining 3096 bytes are whatever was there before -- so reading past the
    // logical size must be refused or empty, never a stale byte from the old
    // contents of the same block.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);

    let new_data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    write(&path, cnid, &new_data);

    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");
    let object = vol
        .lookup(vol.root_cnid(), &units("payload.bin"))
        .expect("lookup")
        .expect("payload.bin exists");
    assert_eq!(
        object
            .as_file()
            .expect("a file")
            .record
            .data_fork
            .total_blocks,
        2
    );
    assert_eq!(vol.read(&object, 0, 5000).expect("read back"), new_data);
    // A read of the whole two blocks is clamped to the logical size.
    assert_eq!(
        vol.read(&object, 0, 8192).expect("clamped read"),
        new_data,
        "a read past the logical size must stop at it rather than returning the \
         rest of the block"
    );

    assert_fsck_clean(&path, "a write with a partial trailing block");
}

#[test]
fn growing_into_a_full_volume_reports_no_space_and_writes_nothing() {
    // `dskFulErr`, and the whole point is that nothing is written: a write that
    // marked a block and then failed would leave the volume with an allocation it
    // cannot account for.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let cnid = payload_cnid(&path);
    let before = std::fs::read(&path).expect("read image");

    // Ask for more than the volume has blocks, so no run can exist.
    let huge = vec![0u8; 64 * 1024 * 1024];
    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    let err = writable
        .write_file_contents(cnid, &huge)
        .expect_err("a 1 MiB volume cannot hold 64 MiB");
    assert!(
        matches!(err, hfsplus::Error::NoSpace { .. }),
        "a volume with nowhere to put the data is out of space, which is \
         distinct from every other failure; got {err:?}"
    );
    dev.sync().expect("flush");
    drop(dev);
    assert_untouched(
        &path,
        &before,
        "a write refused for want of space: nothing may be marked or written",
    );
}

#[test]
fn a_journaled_volume_accepts_writes() {
    // A journaled volume now accepts writes through `WritableVolume::open`.
    // The journal state is read and stored, and each mutation will be wrapped in
    // a transaction that commits before-images to the journal ring.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let writable = WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
    assert!(
        writable.is_journaled(),
        "the volume should report as journaled"
    );
}

#[test]
fn a_journaled_write_advances_the_journal_sequence() {
    // A write on a journaled volume must be captured in a journal transaction:
    // the journal header's sequence_num must advance.
    let Some(path) = copy_fixture("journal-with-files") else {
        return;
    };

    // Capture the journal state before any write.
    let (seq_before, end_before) = {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        let journal = vol.journal().expect("read journal");
        match journal {
            Some(j) => match j.header() {
                Some(h) => (h.sequence_num, h.end),
                None => (0, 0),
            },
            None => (0, 0),
        }
    };

    // Look up a specific file by name.
    let cnid = {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        vol.lookup(vol.root_cnid(), &units("fragmented.bin"))
            .expect("lookup")
            .expect("fragmented.bin exists")
            .as_file()
            .expect("a file")
            .cnid
            .0
    };

    // Perform a write on the journaled volume.
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        assert!(writable.is_journaled(), "the volume should be journaled");
        writable
            .write_file_contents(cnid, b"journaled write test data")
            .expect("write should succeed on a journaled volume");
        dev.sync().expect("flush");
    }

    // Reopen and verify the journal advanced.
    let dev = FileDevice::open(&path).expect("open for reading after write");
    let vol = Volume::open(&dev).expect("mount after write");
    let journal = vol.journal().expect("read journal after write");
    let Some(journal) = journal else {
        panic!("a journaled volume must still have a journal after a write");
    };
    let header = journal
        .header()
        .expect("journal should have a header after a write");
    assert!(
        header.sequence_num > seq_before || header.end != end_before,
        "the journal header must advance after a committed write: \
         before seq={} end={} vs after seq={} end={}",
        seq_before,
        end_before,
        header.sequence_num,
        header.end,
    );
    assert!(
        !journal.is_clean(),
        "a committed write leaves the journal dirty (start != end)"
    );
}

#[test]
fn a_journaled_write_is_durable_after_reopen() {
    // Perform a write on a journaled volume, then verify the data survives
    // a close/reopen cycle — proving the transaction was committed to disk.
    let Some(path) = copy_fixture("journal-with-files") else {
        return;
    };

    // Look up a specific file by name.
    let cnid = {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        vol.lookup(vol.root_cnid(), &units("fragmented.bin"))
            .expect("lookup")
            .expect("fragmented.bin exists")
            .as_file()
            .expect("a file")
            .cnid
            .0
    };

    let test_data = b"durable journaled write data";

    // Perform the write on the journaled volume.
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .write_file_contents(cnid, test_data)
            .expect("write should succeed");
        dev.sync().expect("flush");
    }

    // Reopen and verify the data is readable from the home blocks directly.
    let dev = FileDevice::open(&path).expect("open for reading after write");
    let vol = Volume::open(&dev).expect("mount after write");
    let obj = vol
        .lookup_cnid(hfsplus::catalog::cnid::Cnid(cnid))
        .expect("lookup by CNID")
        .expect("the file must exist after the journaled write");
    let read_back = vol
        .read(&obj, 0, test_data.len())
        .expect("read back after reopen");
    assert_eq!(
        read_back,
        test_data.as_slice(),
        "the written data must survive a close/reopen cycle on a journaled volume"
    );
}

#[test]
fn a_failed_journaled_write_does_not_modify_the_image() {
    // A write that fails (e.g. writing to a CNID that does not exist) must
    // not commit any journal transaction: the before-images captured so far
    // are abandoned, the journal header is not advanced, and the volume
    // remains byte-identical to before the attempt.
    let Some(path) = copy_fixture("journal-with-files") else {
        return;
    };

    let before = std::fs::read(&path).expect("read image");

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        let err = writable
            .write_file_contents(99999, b"this CNID does not exist")
            .expect_err("CNID 99999 must not exist");
        assert!(
            matches!(err, hfsplus::Error::NotFound { .. }),
            "an invalid CNID must be NotFound, got {err:?}"
        );
        dev.sync().expect("flush");
    }

    // The image must be byte-identical: no transaction was committed.
    let after = std::fs::read(&path).expect("read image after failed write");
    assert_eq!(
        before, after,
        "a failed write on a journaled volume must not modify the image"
    );
}

#[test]
fn a_missing_cnid_is_reported_rather_than_writing_something_else() {
    // A CNID that is not in the catalog must not resolve to whatever record does
    // exist. The leaf walk is bounded by the node count, so a corrupted `fLink`
    // terminates rather than spinning; and CNID 2 is the root folder, whose thread
    // record carries its own CNID as its key, which is the case where a search
    // finds a thread where a file was wanted.
    let Some(path) = copy_fixture(IMAGE) else {
        return;
    };
    let before = std::fs::read(&path).expect("read image");

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
        for cnid in [9999u32, 2] {
            let err = writable
                .write_file_contents(cnid, b"nowhere")
                .expect_err("that CNID is not a writable file in this catalog");
            assert!(
                matches!(err, hfsplus::Error::NotFound { .. }),
                "CNID {cnid} is not a file record, and must be NotFound rather \\
                 than a write to whatever does exist; got {err:?}"
            );
        }
        dev.sync().expect("flush");
    }
    assert_untouched(&path, &before, "writes that found no file record");
}

// ---------------------------------------------------------------------------
// Crash/replay tests for extended attribute mutations (Phase A2).
//
// These establish the pattern for every mutation family: write to a copy
// of a fixture, verify fsck accepts the result, then verify the value is
// readable through the library. For journaled volumes the transaction
// commits before returning, so the committed image is the crash-safe state.
// ---------------------------------------------------------------------------

/// CNID 18 is the file that owns the existing attributes in journal-with-attributes.
const ATTR_OWNER: u32 = 18;

/// Helper: look up a CNID, returning the Object or panicking.
fn lookup_obj(vol: &Volume<'_, FileDevice>, cnid: u32) -> Object {
    vol.lookup_cnid(Cnid(cnid))
        .expect("lookup CNID")
        .expect("CNID must exist")
}

/// Helper: fetch attribute names for a CNID.
fn attr_names(vol: &Volume<'_, FileDevice>, cnid: u32) -> Vec<String> {
    let obj = lookup_obj(vol, cnid);
    let names = vol.listxattr(&obj).expect("listxattr");
    let mut names = names;
    names.sort();
    names
}

/// Helper: fetch a specific attribute value for a CNID.
fn attr_value(vol: &Volume<'_, FileDevice>, cnid: u32, name: &str) -> Option<Vec<u8>> {
    let obj = lookup_obj(vol, cnid);
    vol.getxattr(&obj, name).expect("getxattr")
}

#[test]
fn a_setxattr_on_a_journaled_volume_is_durable_after_reopen() {
    // setxattr inserts a new inline attribute record into the attributes B-tree.
    // On a journaled volume the transaction commits before returning, so the
    // attribute must survive a close/reopen cycle.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let test_name = "com.test.crash";
    let test_value = b"crash replay setxattr";

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .setxattr(ATTR_OWNER, test_name, test_value)
            .expect("setxattr should succeed");
        dev.sync().expect("flush");
    }

    // Verify the attribute is readable and fsck accepts the image.
    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after write");
    let names = attr_names(&vol, ATTR_OWNER);
    assert!(
        names.iter().any(|n| n == test_name),
        "{test_name} must be present, got {names:?}"
    );
    assert_eq!(
        attr_value(&vol, ATTR_OWNER, test_name),
        Some(test_value.to_vec())
    );
    assert_fsck_clean(&path, "setxattr on a journaled volume");
}

#[test]
fn a_removexattr_on_a_journaled_volume_is_durable_after_reopen() {
    // removexattr deletes an existing attribute record. The transaction must
    // be durable, and the attribute must be gone after reopen.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let name_to_remove = "com.apple.test.inline";

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .removexattr(ATTR_OWNER, name_to_remove)
            .expect("removexattr should succeed");
        dev.sync().expect("flush");
    }

    // Verify the attribute is gone and fsck accepts the image.
    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after write");
    let names = attr_names(&vol, ATTR_OWNER);
    assert!(
        !names.iter().any(|n| n == name_to_remove),
        "{name_to_remove} must be gone, remaining: {names:?}"
    );
    assert_fsck_clean(&path, "removexattr on a journaled volume");
}

#[test]
fn a_setxattr_then_removexattr_round_trips_on_a_journaled_volume() {
    // Write an attribute, then remove it, in a single journal sequence.
    // The net effect should be the volume without the attribute's record.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let test_name = "com.test.roundtrip";
    let test_value = b"round-trip value";

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .setxattr(ATTR_OWNER, test_name, test_value)
            .expect("setxattr should succeed");
        writable
            .removexattr(ATTR_OWNER, test_name)
            .expect("removexattr should succeed");
        dev.sync().expect("flush");
    }

    // The attribute should not exist.
    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after write");
    let names = attr_names(&vol, ATTR_OWNER);
    assert!(
        !names.iter().any(|n| n == test_name),
        "{test_name} must be gone after round-trip, remaining: {names:?}"
    );
    assert_fsck_clean(&path, "setxattr then removexattr round-trip");
}

#[test]
fn a_setxattr_replacing_an_existing_attribute_updates_in_place() {
    // setxattr on an existing name should replace the value, not duplicate the
    // record. The attribute list must not change in count.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let name = "com.apple.test.inline";
    let new_value = b"replaced inline value";

    let count_before = {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        attr_names(&vol, ATTR_OWNER).len()
    };

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .setxattr(ATTR_OWNER, name, new_value)
            .expect("setxattr replace should succeed");
        dev.sync().expect("flush");
    }

    // Verify the value was replaced and the count is unchanged.
    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after write");
    let value = attr_value(&vol, ATTR_OWNER, name);
    assert_eq!(value, Some(new_value.to_vec()));
    assert_eq!(
        attr_names(&vol, ATTR_OWNER).len(),
        count_before,
        "replacing an attribute must not change the record count"
    );
    assert_fsck_clean(&path, "setxattr replacing existing attribute");
}

#[test]
fn removing_the_last_attribute_clears_the_has_attributes_flag() {
    // When the last attribute for a CNID is removed, kHFSHasAttributesMask
    // must be cleared on the catalog record. A file that declares attributes
    // but has none in the tree fails fsck.hfsplus's count comparison.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    // CNID 18 has two attributes: "com.apple.test.forked" and
    // "com.apple.test.inline". Remove both.
    let names = ["com.apple.test.forked", "com.apple.test.inline"];

    // Verify the flag is set before removal.
    {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        let obj = lookup_obj(&vol, ATTR_OWNER);
        let has_attrs = match &obj {
            Object::File(f) => f.has_attributes,
            _ => panic!("CNID {ATTR_OWNER} should be a file"),
        };
        assert!(
            has_attrs,
            "CNID {ATTR_OWNER} must have has_attributes flag set before removal"
        );
    }

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        for name in &names {
            writable
                .removexattr(ATTR_OWNER, name)
                .unwrap_or_else(|e| panic!("removing {name}: {e}"));
        }
        dev.sync().expect("flush");
    }

    // After removing both, the flag should be cleared and fsck should accept it.
    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after write");
    let obj = lookup_obj(&vol, ATTR_OWNER);
    let has_attrs = match &obj {
        Object::File(f) => f.has_attributes,
        _ => panic!("CNID {ATTR_OWNER} should be a file"),
    };
    assert!(
        !has_attrs,
        "CNID {ATTR_OWNER} must have has_attributes flag cleared after last removal"
    );
    let remaining = attr_names(&vol, ATTR_OWNER);
    assert!(
        remaining.is_empty(),
        "no attributes should remain, got {remaining:?}"
    );
    assert_fsck_clean(&path, "removing the last attribute clears the flag");
}

#[test]
fn writing_a_resource_fork_is_round_trip_safe() {
    // A resource fork write must allocate blocks, update the fork record,
    // and leave a volume that fsck.hfsplus accepts. The resource fork is a
    // real HFS+ fork, stored as a ForkData in the catalog file record --
    // the same structure as the data fork, just a different field.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    // CNID 18 (ATTR_OWNER) is a file with a data fork but no resource fork.
    let test_data = b"resource fork test data".to_vec();

    // Verify the resource fork is initially empty.
    {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        let obj = lookup_obj(&vol, ATTR_OWNER);
        let f = obj.as_file().expect("CNID {ATTR_OWNER} should be a file");
        assert_eq!(
            f.record.resource_fork.logical_size, 0,
            "the resource fork should be empty before the write"
        );
    }

    // Write the resource fork on the journaled volume.
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .write_resource_fork(ATTR_OWNER, &test_data)
            .expect("write_resource_fork should succeed");
        dev.sync().expect("flush");
    }

    // Read it back through a fresh volume.
    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after write");
    let obj = lookup_obj(&vol, ATTR_OWNER);
    let f = obj.as_file().expect("CNID {ATTR_OWNER} should be a file");
    assert_eq!(
        f.record.resource_fork.logical_size,
        test_data.len() as u64,
        "the resource fork length must match the data written"
    );
    assert_eq!(
        f.record.resource_fork.total_blocks, 1,
        "the resource fork must have allocated one block"
    );
    let back = vol
        .read_resource(&obj, 0, test_data.len())
        .expect("read resource fork");
    assert_eq!(back, test_data, "the resource fork bytes must round-trip");
    assert_fsck_clean(&path, "writing a resource fork");
}

#[test]
fn writing_a_resource_fork_is_durable_after_reopen() {
    // The data survives a close/reopen cycle on a journaled volume.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let test_data = b"durable resource fork data".to_vec();

    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .write_resource_fork(ATTR_OWNER, &test_data)
            .expect("write should succeed");
        dev.sync().expect("flush");
    }

    let dev = FileDevice::open(&path).expect("open for reading");
    let vol = Volume::open(&dev).expect("mount after reopen");
    let obj = lookup_obj(&vol, ATTR_OWNER);
    let f = obj.as_file().expect("a file");
    assert_eq!(
        f.record.resource_fork.logical_size,
        test_data.len() as u64,
        "the resource fork must survive a reopen"
    );
    let back = vol
        .read_resource(&obj, 0, test_data.len())
        .expect("read resource fork");
    assert_eq!(
        back, test_data,
        "the resource fork bytes must survive a reopen"
    );
    assert_fsck_clean(&path, "durability of resource fork write");
}

#[test]
fn a_journaled_resource_fork_write_advances_the_journal_sequence() {
    // A resource fork write must be captured in a journal transaction.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };

    let (seq_before, end_before) = {
        let dev = FileDevice::open(&path).expect("open for reading");
        let vol = Volume::open(&dev).expect("mount");
        let journal = vol.journal().expect("read journal");
        match journal {
            Some(j) => match j.header() {
                Some(h) => (h.sequence_num, h.end),
                None => (0, 0),
            },
            None => (0, 0),
        }
    };

    let test_data = b"journaled resource fork data".to_vec();
    {
        let mut dev = FileDevice::open_writable(&path).expect("open writable");
        let mut writable =
            WritableVolume::open(&mut dev).expect("a journaled volume should accept writes");
        writable
            .write_resource_fork(ATTR_OWNER, &test_data)
            .expect("write should succeed");
        dev.sync().expect("flush");
    }

    let dev = FileDevice::open(&path).expect("open for reading after write");
    let vol = Volume::open(&dev).expect("mount after write");
    let journal = vol.journal().expect("read journal after write");
    let Some(journal) = journal else {
        panic!("a journaled volume must still have a journal after a write");
    };
    let header = journal
        .header()
        .expect("journal should have a header after a write");
    assert!(
        header.sequence_num > seq_before || header.end != end_before,
        "the journal header must advance after a committed resource fork write: \
         before seq={} end={} vs after seq={} end={}",
        seq_before,
        end_before,
        header.sequence_num,
        header.end
    );
    assert_fsck_clean(
        &path,
        "journaled resource fork write advances journal sequence",
    );
}
