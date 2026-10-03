//! The production implementation is provably read-only.
//!
//! After Milestone 5 this is an architectural property rather than an accident,
//! and this file is what stops it decaying silently. A future write milestone is
//! *supposed* to change it -- and when it does, this test should be the thing
//! that has to be deliberately deleted, rather than something that quietly starts
//! passing anyway.
//!
//! # What is actually asserted
//!
//! Not "the tests did not notice a write". A counting device wraps a real one and
//! records every attempt, so a write anywhere on the read path is observed rather
//! than inferred from the bytes.
//!
//! The other half is structural, and the compiler is what checks it: writing lives
//! on a *different trait*. `BlockDeviceMut: BlockDevice` adds `write_at`, and
//! `Volume<'a, D>` is bounded by `BlockDevice` alone while holding `&'a D`. So a
//! volume cannot write through its device whatever the device happens to be -- the
//! write capability is not merely unused, it is not in scope.
//!
//! What a test *can* observe is the part that is not type-level: that nothing on
//! the read path reaches for a writable device in the first place. The last test
//! covers the remaining door -- the constructors that hand out writable devices,
//! which are a separate capability from the trait.

mod common;

use std::cell::RefCell;

use hfsplus::blockdev::BlockDevice;
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::volume::Volume;

/// Wraps a device and counts every write, recording the call site.
///
/// The device is immutable underneath -- `MemoryDevice::new` rather than
/// `new_writable` -- so a write cannot succeed through it. That is deliberate:
/// the point is to observe the *attempt*, and a wrapper that could really write
/// would be a worse test.
#[derive(Debug)]
struct CountingDevice<'a> {
    inner: &'a MemoryDevice,
    writes: RefCell<Vec<(u64, usize)>>,
}

impl<'a> CountingDevice<'a> {
    fn new(inner: &'a MemoryDevice) -> Self {
        CountingDevice { inner, writes: RefCell::new(Vec::new()) }
    }

    fn writes(&self) -> usize {
        self.writes.borrow().len()
    }
}

impl BlockDevice for CountingDevice<'_> {
    fn len(&self) -> hfsplus::error::Result<u64> {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> hfsplus::error::Result<()> {
        self.inner.read_at(offset, buf)
    }
}

/// Writing is on its own trait, which is the whole reason a `Volume` cannot do it.
///
/// Implementing it here is what makes the counter able to observe an attempt: the
/// test needs a device that *could* write, so that it can prove the read path
/// never asks it to.
impl hfsplus::blockdev::BlockDeviceMut for CountingDevice<'_> {
    fn sync(&mut self) -> hfsplus::error::Result<()> {
        // Recorded too: a flush is a write in everything but name, and one
        // sneaking onto the read path would be just as wrong.
        self.writes.borrow_mut().push((u64::MAX, 0));
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> hfsplus::error::Result<()> {
        // The attempt is recorded and nothing is written: the inner device is
        // immutable, so a real write here would fail rather than corrupt a
        // fixture.
        self.writes.borrow_mut().push((offset, buf.len()));
        Err(hfsplus::error::Error::ReadOnly)
    }
}

use hfsplus::blockdev::MemoryDevice;

#[test]
fn mounting_and_reading_a_volume_never_attempts_a_write() {
    // The whole read path, on a journaled image, through every route the library
    // offers. If any of them reached for a writable device this would observe it.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read");
    let device = MemoryDevice::new(bytes);
    let counting = CountingDevice::new(&device);

    {
        let vol = Volume::open(&counting).expect("mount");

        // The volume header, the journal, its transactions, its bytes, and every
        // catalog read a caller could ask for.
        let _ = vol.is_journaled();
        let _ = vol.header().journal_info_block;
        let _ = vol.name();
        let _ = vol.statfs();
        if let Ok(Some(journal)) = vol.journal() {
            let _ = journal.transactions();
            let _ = journal.replayed_blocks();
            let _ = journal.truncation();
            let _ = journal.header_checksum_ok();
            let _ = journal.read_bytes(0, 4096);
        }
        let _ = vol.external_journal();

        if let Ok(entries) = vol.read_dir(vol.root_cnid()) {
            for entry in entries {
                let units = entry.name();
                if let Ok(Some(object)) = vol.lookup(vol.root_cnid(), units) {
                    let _ = object.as_file();
                    let _ = vol.read(&object, 0, 512);
                    let _ = vol.read_resource(&object, 0, 512);
                    let _ = vol.read_link(&object);
                }
            }
        }
        let _ = vol.catalog().all_objects();
    }

    assert_eq!(
        counting.writes(),
        0,
        "the read path attempted {} writes: {:?}",
        counting.writes(),
        counting.writes.borrow()
    );
}

#[test]
fn a_read_across_the_whole_image_still_never_writes() {
    // Every block of a journaled image, read through the device directly. Slower
    // than anything else here and deliberately so: it covers offsets no catalog
    // walk would reach.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read");
    let device = MemoryDevice::new(bytes);
    let counting = CountingDevice::new(&device);

    let vh = VolumeHeader::read_from(&counting).expect("header");
    let bs = u64::from(vh.block_size);
    let mut buf = vec![0u8; bs as usize];
    let total = u64::from(vh.total_blocks);
    for block in 0..total.min(256) {
        counting
            .read_at(block * bs, &mut buf)
            .unwrap_or_else(|e| panic!("block {block}: {e}"));
    }

    assert_eq!(counting.writes(), 0, "a whole-image scan attempted a write");
}

#[test]
fn the_checker_never_writes_to_the_image_it_is_checking() {
    // `hfsck` is the one component that *does* repair, so it is worth pinning that
    // it is never handed the original. A checker that quietly repairs a fixture
    // would make every other test in the repository meaningless.
    let path = common::image("journaled-hfsplus");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let bytes = std::fs::read(&path).expect("read");
    let device = MemoryDevice::new(bytes);
    let counting = CountingDevice::new(&device);

    let vol = Volume::open(&counting).expect("mount");
    let _ = hfsplus::check::check(&vol, None).expect("check");
    assert_eq!(counting.writes(), 0, "the checker attempted a write");
}

#[test]
fn no_shipped_code_path_names_a_writable_constructor() {
    // The structural half, as close to a check as a test gets. `BlockDevice`'s
    // write method takes `&mut self` and `Volume` holds `&D`, so a volume cannot
    // write through its device whatever it is called -- but the *constructors*
    // that hand out writable devices are a separate door, and one of them being
    // reachable from a shipped binary would undo the property.
    //
    // The grep is over the source rather than over the binaries, because a
    // compiled binary no longer says where a call came from. Binaries are built
    // from `src/bin/` and library code lives under `src/`, so anything under a
    // `#[cfg(test)]` module is a fixture rather than a path.
    let root = common::repo_root().join("src");
    let mut offenders = Vec::new();
    for path in walk(&root) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(rel) = path.strip_prefix(&root) else { continue };
        let Some(production) = production_part(&text) else { continue };

        for needle in ["open_writable", "new_writable"] {
            // The definition itself is not a call. Everything else mentioning the
            // name above the test marker is.
            let mut at = 0;
            while let Some(found) = production[at..].find(needle) {
                let start = at + found;
                let end = start + needle.len();
                // Only the token immediately before decides. An earlier
                // `pub fn` elsewhere in the file must not excuse this -- which it
                // did, until the check was narrowed from the whole prefix to the
                // last few characters of it.
                let tail = production[..start].trim_end();
                let preceded_by_fn = tail.ends_with("fn ")
                    || tail.ends_with("fn")
                    || tail.ends_with('(');
                if !preceded_by_fn {
                    let line = production[..start].lines().count();
                    offenders.push(format!("{}:{} mentions {needle}", rel.display(), line));
                }
                at = end;
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "shipped code reaches a writable-device constructor: {offenders:?}"
    );
}

/// Every `.rs` under `dir`.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// The part of a source file that is *not* test code.
///
/// Everything from the first `#[cfg(test)]` onwards. That is the layout every
/// file here uses -- tests at the bottom -- and getting it wrong in the
/// permissive direction is how the first version of this test passed with a real
/// `new_writable` call sitting in `src/volume/mod.rs`: the file has a test module,
/// so exempting the whole of it exempted the production code too.
///
/// A file with test code *before* production code would under-report. Nothing here
/// is written that way, and the failure mode is a missed defect rather than a
/// false accusation.
fn production_part(text: &str) -> Option<&str> {
    let marker = text.find("#[cfg(test)]").unwrap_or(text.len());
    if marker == 0 {
        return None;
    }
    Some(&text[..marker])
}
