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
use hfsplus::volume::{Volume, WritableVolume};

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
    let out = common::run_fsck(&fsck, path);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("appears to be OK") || text.contains("File system is clean"),
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

// --- What it refuses -------------------------------------------------------

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
fn a_journaled_volume_is_refused_for_mutation() {
    // Not because writing is impossible -- but because a write that is not
    // journalled leaves a journal that does not describe the volume, which is the
    // exact failure the journal exists to prevent. Journalled writes come later.
    let Some(path) = copy_fixture("journal-with-attributes") else {
        return;
    };
    let before = std::fs::read(&path).expect("read image");

    let mut dev = FileDevice::open(&path).expect("open");
    let err = WritableVolume::open(&mut dev).expect_err("a journaled volume must be refused");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("journal"),
        "the refusal must say why, not merely fail; got: {rendered}"
    );
    assert_untouched(&path, &before, "the refusal of a journaled volume");
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
