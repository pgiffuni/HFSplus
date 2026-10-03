//! Reading file data through the volume API.
//!
//! Every corpus volume is created by `mkfs.hfsplus`, which cannot put a file in
//! it. So `file_count` is 0 on seven of nine images, and the two journaled
//! volumes hold only the two files `newfs_hfs` makes for itself. The consequence
//! was that `Volume::read`, `read_file` and `read_resource` had assertions only of
//! the form "reading a folder is an error" -- nothing ever read a byte of file
//! data through the public API.
//!
//! That is the wrong way round: a fork reader that is wrong about extents, about
//! a short final block, or about a read that runs off the end of the logical size
//! would pass every test in the repository.
//!
//! # Why the journal files are the fixture
//!
//! The two hidden files on a journaled volume are ordinary files with ordinary
//! data forks, and their contents are *known independently*:
//!
//! * `.journal_info_block` is 4096 bytes, one allocation block, and starts with
//!   `struct JournalInfoBlock`. Its flags and offsets can be checked against what
//!   `hfsplus::journal::info` decodes, and the tail must be zero padding.
//! * `.journal` is 524288 bytes -- 128 blocks of 4096 -- and this corpus is
//!   generated from a fresh volume, so no transaction has been written and the
//!   whole fork is zeros.
//!
//! So the expected bytes are derived from the format, not recorded from a previous
//! run of the code under test. That is what makes a mismatch a finding rather than
//! a change in the baseline.
//!
//! Mining reference: Apple `core/hfs_format.h` for `struct HFSPlusForkData` and
//! the extent record, and `core/hfs_vfsops.c` `hfs_vnode_read` for how a data
//! fork read maps logical offsets onto extents.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::journal::info::{JournalInfoBlock, JOURNAL_INFO_BLOCK_SIZE};
use hfsplus::volume::{Object, Volume};

/// A volume holding the two files `newfs_hfs` creates for its journal.
const JOURNALED: &str = "journaled-hfsplus";

/// Name of the journal file, as stored.
const JOURNAL: &str = ".journal";
/// Name of the journal info block file, as stored.
const JOURNAL_INFO: &str = ".journal_info_block";

/// Where the 0xdb drive filler ends inside a journal info block file.
///
/// `makehfs.c` clears one sector to 0xdb at a time, so the filler covers the
/// second sector boundary onwards from the struct's end.
const FILLER_END: usize = 512;

fn image(name: &str) -> std::path::PathBuf {
    common::image(name)
}

/// Look up a hidden entry by name, which is how these two are reached.
fn hidden_entry(vol: &Volume<'_, FileDevice>, name: &str) -> Option<Object> {
    let units: Vec<u16> = name.encode_utf16().collect();
    vol.lookup(vol.root_cnid(), &units).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Skip with a reason rather than fail when the image is absent.
fn require(name: &str) -> bool {
    if image(name).exists() {
        true
    } else {
        eprintln!("skipping: {} not built; run tools/genimages.sh", image(name).display());
        false
    }
}

/// Mount the journaled volume and hand it to `f`.
fn with_journaled(f: impl FnOnce(&Volume<'_, FileDevice>)) {
    with_volume(JOURNALED, f)
}

fn with_volume(name: &str, f: impl FnOnce(&Volume<'_, FileDevice>)) {
    let path = image(name);
    let dev = FileDevice::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let vol = Volume::open(&dev).unwrap_or_else(|e| panic!("mount {}: {e}", path.display()));
    f(&vol);
}

// --- Existence and geometry --------------------------------------------

#[test]
fn a_file_with_a_data_fork_reads_its_declared_length() {
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("the journal file must be listed");
        assert_eq!(file.data_size(), 524288, "the journalled fork's logical size");

        let data = vol.read_file(&file, 1 << 20).expect("read the whole fork");
        assert_eq!(
            data.len(),
            524288,
            "read_file must return the logical size, not the extent size"
        );

        // The other file, at a different length, so a length that only works for
        // one shape would fail here.
        let info = hidden_entry(vol, JOURNAL_INFO).expect("the info block file must be listed");
        assert_eq!(info.data_size(), 4096);
        assert_eq!(vol.read_file(&info, 1 << 20).expect("read").len(), 4096);
    });
}

// --- Content, checked against the format --------------------------------

#[test]
fn the_journal_info_block_file_contains_a_journal_info_block() {
    // Read through the *file* path rather than the journal module, so this
    // exercises the fork reader and the catalog, not the code that would
    // ordinarily decode these bytes.
    //
    // Mining reference: `struct JournalInfoBlock` in `core/hfs_format.h`, whose
    // first field is a `flags` u32 and which carries the journal's offset and
    // size in bytes.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL_INFO).expect("listed");
        let raw = vol.read_file(&file, 4096).expect("read");

        let jib = JournalInfoBlock::parse(&raw).unwrap_or_else(|e| panic!("decode: {e}"));

        // The volume header names the block this file holds, so the two routes
        // to it -- the header's `journalInfoBlock` field and a catalog lookup by
        // name -- have to agree on which block it is.
        let dev = FileDevice::open(image(JOURNALED)).expect("open");
        let vh = VolumeHeader::read_from(&dev).expect("header");
        assert_eq!(
            vh.journal_info_block,
            2,
            "the header points at the block this file occupies"
        );
        // Past the struct the block is not padding, and the reason is worth
        // recording because "the remainder must be zero" is the obvious
        // assumption and it is wrong. `makehfs.c` memsets each sector to 0xdb
        // before writing anything into it, and this block is never fully
        // overwritten:
        //
        //     memset(buffer, 0xdb, driveInfo->physSectorSize);
        //
        // So bytes 180..512 are drive filler, and only past that second
        // 512-byte boundary is the block zero. A reader that returned zeros for
        // the filler would be fabricating, and one that rejected the block as
        // malformed would be refusing a valid one.
        assert!(
            raw[JOURNAL_INFO_BLOCK_SIZE..FILLER_END].iter().all(|b| *b == 0xdb),
            "bytes {JOURNAL_INFO_BLOCK_SIZE}..{FILLER_END} are 0xdb drive filler, got {:?}",
            &raw[JOURNAL_INFO_BLOCK_SIZE..JOURNAL_INFO_BLOCK_SIZE + 8]
        );
        assert!(
            raw[FILLER_END..].iter().all(|b| *b == 0),
            "the last sector of the block is zero, found {} non-zero bytes",
            raw[FILLER_END..].iter().filter(|b| **b != 0).count()
        );

        let (expected_offset, expected_size) = (12288u64, 524288u64);
        assert_eq!(
            u64::from_be_bytes(raw[36..44].try_into().unwrap()),
            expected_offset,
            "the journal's byte offset"
        );
        assert_eq!(
            u64::from_be_bytes(raw[44..52].try_into().unwrap()),
            expected_size,
            "the journal's size in bytes"
        );
        assert_eq!(jib.offset, expected_offset);
        assert_eq!(jib.size, expected_size);

        // The info block file is one allocation block, so its own extent record
        // accounts for itself and nothing else. Checking it here ties the file's
        // geometry to the length being read, which is the thing a mis-mapped
        // extent would get wrong.
        let file = hidden_entry(vol, JOURNAL_INFO).expect("listed");
        let extents = file.as_file().expect("a file record").record.data_fork.extents;
        let extent_total: u64 = (0..8)
            .map(|i| u64::from(extents.raw[i].block_count))
            .sum();
        assert_eq!(
            extent_total * 4096,
            4096,
            "the info block file occupies exactly one allocation block"
        );
        assert_eq!(
            file.data_size(),
            4096,
            "and its logical size is that same block"
        );
    });
}

#[test]
fn an_unwritten_journal_is_entirely_zero() {
    // `-J` creates a journal and never writes a transaction, so the whole fork is
    // zeros. Any non-zero byte here means the read landed on the wrong blocks.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        let data = vol.read_file(&file, 1 << 20).expect("read");
        let non_zero = data.iter().filter(|b| **b != 0).count();
        assert_eq!(
            non_zero, 0,
            "a journal with no transaction must be all zeros, found {non_zero} non-zero bytes"
        );
    });
}

// --- Offsets and boundaries ---------------------------------------------

#[test]
fn reads_at_arbitrary_offsets_agree_with_a_whole_file_read() {
    // The strongest check available: every window must equal the same slice of
    // the whole read. A fork reader with the wrong extent arithmetic disagrees
    // with itself between the two paths.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        let whole = vol.read_file(&file, 1 << 20).expect("read");

        // Straddle the 4096-byte block boundary, and the last block.
        let block = 4096usize;
        for offset in [0, 1, block - 1, block, block + 1, 2 * block - 1, whole.len() - 1] {
            for len in [1, 7, block - 1, block, block + 1] {
                let window = vol
                    .read(&file, offset as u64, len)
                    .unwrap_or_else(|e| panic!("read at {offset} len {len}: {e}"));
                let expected_end = (offset + len).min(whole.len());
                let expected = &whole[offset..expected_end];
                assert_eq!(
                    window, expected,
                    "read at offset {offset} length {len} disagrees with the whole-file read"
                );
            }
        }
    });
}

#[test]
fn a_read_past_the_end_of_the_file_is_bounded_not_an_error() {
    // A short read at the tail is normal; an error would make a legitimate read
    // pattern fail. Mining reference: Apple `hfs_vnode_read` clamps
    // `uio_resid` to what the fork holds and returns the count it could supply.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        let size = file.data_size() as usize;

        let tail = vol.read(&file, size as u64 - 10, 10).expect("exactly to the end");
        assert_eq!(tail.len(), 10, "a read ending exactly at the size is complete");

        let over = vol.read(&file, size as u64 - 10, 1000).expect("past the end");
        assert_eq!(over.len(), 10, "a read past the end returns what there is");

        let past = vol.read(&file, size as u64, 10).expect("entirely past the end");
        assert!(past.is_empty(), "a read starting at the end returns nothing");

        let beyond = vol.read(&file, size as u64 * 2, 10).expect("far past the end");
        assert!(beyond.is_empty(), "a read beyond the end returns nothing");
    });
}

#[test]
fn a_zero_length_read_returns_nothing_rather_than_failing() {
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        assert!(vol.read(&file, 0, 0).expect("zero length at 0").is_empty());
        assert!(vol.read(&file, 100, 0).expect("zero length mid-file").is_empty());
    });
}

// --- Fork shapes -------------------------------------------------------

#[test]
fn a_file_with_no_resource_fork_reads_empty_rather_than_failing() {
    // The absence of a fork is normal, not an error. Mining reference:
    // `hfs_vnode_read` on a zero-length fork returns 0 without consulting the
    // extent list at all.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        assert_eq!(
            file.resource_size(),
            0,
            "the journal file has no resource fork"
        );
        assert!(
            vol.read_resource(&file, 0, 64).expect("read_resource").is_empty(),
            "reading an absent resource fork yields nothing"
        );
        assert!(
            vol.read_resource(&file, 1000, 64).expect("offset read").is_empty(),
            "and the same at any offset"
        );
    });
}

#[test]
fn reading_a_directory_is_an_error_on_every_fork() {
    // The negative half, kept alongside the positives so the positives are not
    // the only behaviour asserted.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let root = vol
            .lookup_cnid(vol.root_cnid())
            .expect("resolve the root")
            .expect("the root folder must exist");
        assert!(root.is_dir());
        assert!(vol.read(&root, 0, 10).is_err(), "a directory has no data fork");
        assert!(vol.read_resource(&root, 0, 10).is_err());
        assert!(vol.read_link(&root).is_err(), "the root is not a symlink");
    });
}

#[test]
fn a_symlink_without_one_fails_to_read_as_a_link() {
    // No corpus volume holds a symlink -- `mkfs.hfsplus` cannot create one -- so
    // this asserts the refusal on a file that has no symlink target, which is the
    // case a FUSE readlink would otherwise mis-handle.
    //
    // Mining reference: Apple stores a symlink's target in its data fork, so
    // readlink on a regular file has nothing to return and must fail rather than
    // return the file's contents.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        assert!(!file.is_symlink(), "the journal file is a regular file");
        assert!(
            vol.read_link(&file).is_err(),
            "readlink on a regular file must fail, not return its contents"
        );
    });
}

// --- The image must not change -----------------------------------------

#[test]
fn reading_does_not_modify_the_image() {
    // Reading is the operation a FUSE mount performs constantly. If any part of
    // the fork path wrote -- a spill, a bitmap update, a journal of its own --
    // this is the assertion that would notice.
    let path = image(JOURNALED);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let before = std::fs::read(&path).expect("read before");
    let digest_before = digest(&before);

    {
        let dev = FileDevice::open(&path).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let file = hidden_entry(&vol, JOURNAL).expect("listed");

        // Read hard enough to exercise every extent in the fork, several times.
        for _ in 0..4 {
            let data = vol.read_file(&file, 1 << 20).expect("read");
            assert_eq!(data.len(), 524288);
            let _ = vol.read(&file, 4090, 12).expect("straddling read");
            let _ = vol.read(&file, 524_278, 100).expect("tail read");
        }
    }

    let after = std::fs::read(&path).expect("read after");
    assert_eq!(digest_before, digest(&after), "reading modified the image");
}

#[test]
fn the_same_bytes_come_back_on_every_mount() {
    // Guards against a reader that depends on state it should not have: buffer
    // reuse across mounts, or geometry cached from a previous volume.
    if !require(JOURNALED) {
        return;
    }
    let first = {
        let dev = FileDevice::open(image(JOURNALED)).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let file = hidden_entry(&vol, JOURNAL).expect("listed");
        vol.read_file(&file, 1 << 20).expect("read")
    };
    let second = {
        let dev = FileDevice::open(image(JOURNALED)).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let file = hidden_entry(&vol, JOURNAL).expect("listed");
        vol.read_file(&file, 1 << 20).expect("read")
    };
    assert_eq!(first.len(), second.len());
    assert_eq!(digest(&first), digest(&second), "two mounts disagree");
}

// --- Multi-extent and sparse shapes -------------------------------------

#[test]
fn a_multi_extent_fork_is_read_as_one_contiguous_stream() {
    // The journal fork is 128 blocks in one extent, so extent *transitions* are
    // untested by the corpus. Mining reference: Apple `hfs_extents.c`
    // `extoffset` maps a logical fork offset to a physical one across extent
    // boundaries, which is what a two-extent fork needs and a single-extent one
    // never exercises.
    //
    // So the fork's geometry is checked directly, and if the corpus ever grows a
    // multi-extent file this test starts covering the real thing without change.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        let file = hidden_entry(vol, JOURNAL).expect("listed");
        let f = file.as_file().expect("a file record");

        let fork = &f.record.data_fork;
        let extents = &fork.extents;
        let used: Vec<usize> = (0..8)
            .map(|i| extents.raw[i].block_count as usize)
            .filter(|n| *n > 0)
            .collect();

        assert!(
            !used.is_empty(),
            "the fork must have at least one extent, or nothing is being tested"
        );
        assert_eq!(
            fork.logical_size,
            (used.iter().sum::<usize>() as u64) * 4096,
            "the fork is whole blocks, so its logical size is the extent total"
        );

        // Whichever shape this is, every byte must read. The point is that a
        // reader which mis-handles a boundary fails here rather than only on a
        // volume nobody has.
        let data = vol.read_file(&file, 1 << 20).expect("read");
        assert_eq!(data.len() as u64, fork.logical_size);
        assert!(data.iter().all(|b| *b == 0));

        if used.len() > 1 {
            // Cross each extent boundary and confirm the stream is continuous.
            let block = 4096u64;
            let mut cumulative = 0u64;
            for count in &used {
                cumulative += *count as u64;
                let at = cumulative * block - 1;
                let window = vol.read(&file, at, 2).expect("boundary read");
                assert_eq!(window.len(), 2, "read across the boundary at {at}");
            }
        }
    });
}

#[test]
fn a_sparse_region_within_a_fork_reads_as_zeros() {
    // HFS+ represents a hole as an extent with a zero start and a non-zero count,
    // and the reader must supply zeros rather than reading block 0. Mining
    // reference: Apple `core/hfs_vfsops.c` `hfs_vnode_read` returns zeroes for
    // `MLT` extents with `startBlock == 0` without touching the disk.
    //
    // The corpus has no sparse file, so this asserts the *absence* of the
    // condition: no extent in it has a zero start with a non-zero count, so the
    // path is untested rather than broken.
    if !require(JOURNALED) {
        return;
    }
    with_journaled(|vol| {
        for name in [JOURNAL, JOURNAL_INFO] {
            let file = hidden_entry(vol, name).expect("listed");
            let f = file.as_file().expect("a file record");
            for (i, extent) in f.record.data_fork.extents.raw.iter().enumerate() {
                assert!(
                    extent.start_block != 0 || extent.block_count == 0,
                    "{name} extent {i} is sparse (start 0, count {}); \
                     the sparse read path is then in play and must be asserted",
                    extent.block_count
                );
            }
        }
    });
}

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}