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

#[test]
fn growing_a_file_is_refused_by_name_rather_than_truncated() {
    // The alternative -- writing what fits and quietly reporting success --
    // would hand back a file whose contents are not what was asked for, with no
    // indication that anything was dropped. The error names the shortfall so the
    // caller can tell an allocator from an i/o problem.
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
    let before = std::fs::read(&path).expect("read image");

    let mut dev = FileDevice::open_writable(&path).expect("open writable");
    let mut writable = WritableVolume::open(&mut dev).expect("open for mutation");
    let err = writable
        .write_file_contents(cnid, &vec![0u8; 8192])
        .expect_err("4096 bytes cannot hold 8192");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("8192") && rendered.contains("4096"),
        "the error must name both the request and the capacity, or a caller \\
         cannot tell what to do about it; got: {rendered}"
    );
    assert!(
        rendered.contains("allocator"),
        "and it must say that growth needs an allocator, which is the actual \\
         reason; got: {rendered}"
    );
    dev.sync().expect("flush");
    drop(dev);

    assert_untouched(
        &path,
        &before,
        "a write refused for capacity: the check happens before the first block",
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
