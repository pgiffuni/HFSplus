// SPDX-License-Identifier: BSD-2-Clause

//! Reading files that actually have data: fragmented extents and a symlink.
//!
//! # Why this suite exists
//!
//! `mkfs.hfsplus` creates an empty volume and cannot put a file in it, so the
//! corpus has `file_count = 0` on seven of nine images and the two journaled ones
//! hold nothing but the two files `newfs_hfs` makes for its journal. Every
//! positive test of `Volume::read` therefore had to invent its own bytes, and
//! `read_link` had no positive test at all.
//!
//! `tools/mkfiles.py` writes the two shapes a read-only filesystem has to get
//! right and a formatter cannot produce:
//!
//! - **fragmented extents.** `fragmented.bin` is eight single-block extents at
//!   physically scattered blocks, each block filled with its own block number.
//!   Reading it must reproduce those numbers in order, so a reader that mis-maps
//!   an offset, concatenates the extents in the wrong order, or stops after the
//!   first extent gets a *different* byte rather than a plausible one.
//! - **a symbolic link.** `link` stores its target in the data fork, as Apple
//!   does. `read_link` must return the target, and on a regular file it must fail
//!   rather than return the file's contents.
//!
//! The image passes `fsck.hfsplus` unchanged, which is the check that the
//! generator wrote something coherent rather than merely self-consistent -- with
//! one caveat worth stating, because it is the one place this suite leans on
//! something the checker does not do. `hfsprogs` is an unofficial port of Apple's
//! `fsck_hfs` and it **does not validate symlinks at all**: zeroing the data fork
//! of `link`, which Apple rejects as bad information for a symbolic link, passes
//! without comment. So the symlink fixture's validity rests on the format --
//! Apple's `S_IFLNK` mode and its target in the data fork -- and on the crate
//! reading it back, not on the checker agreeing. See `docs/dev-tools.md`.
//!
//! Mining reference: Apple `core/hfs_format.h` for `struct HFSPlusCatalogFile`
//! and `struct HFSPlusBSDInfo`; `core/hfs_xattr.c` for a link target living in
//! the data fork.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::record::{S_IFLNK, S_IFMT, S_IFREG};
use hfsplus::catalog::DirCursor;
use hfsplus::volume::{Object, Volume};

/// The image `tools/mkfiles.py` produces from `journaled-hfsplus`.
const WITH_FILES: &str = "journal-with-files";

/// A file whose eight extents are physically scattered.
const FRAGMENTED: &str = "fragmented.bin";
/// A symbolic link.
const LINK: &str = "link";
/// A file whose extents spill past the inline eight into the extents B-tree.
const OVERFLOW: &str = "overflow.bin";
/// The link's target, as stored.
const LINK_TARGET: &str = "../elsewhere/target";

fn image_path(name: &str) -> std::path::PathBuf {
    common::repo_root()
        .join("tests/images/generated")
        .join(format!("{name}.img"))
}

/// Skip with a reason rather than fail when the image is absent.
fn require(name: &str) -> bool {
    if image_path(name).exists() {
        true
    } else {
        eprintln!(
            "skipping: {} not built; run tools/mkfiles.py",
            image_path(name).display()
        );
        false
    }
}

/// Mount the volume and hand it to `f`.
fn with_volume(name: &str, f: impl FnOnce(&hfsplus::volume::Volume<'_, FileDevice>)) {
    let path = image_path(name);
    let dev = FileDevice::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let vol = hfsplus::volume::Volume::open(&dev)
        .unwrap_or_else(|e| panic!("mount {}: {e}", path.display()));
    f(&vol);
}

/// Look up a root-folder entry by name.
fn entry(vol: &hfsplus::volume::Volume<'_, FileDevice>, name: &str) -> Object {
    let units: Vec<u16> = name.encode_utf16().collect();
    vol.lookup(vol.root_cnid(), &units)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .unwrap_or_else(|| panic!("{name} must be present"))
}

// --- Discovery ---------------------------------------------------------

#[test]
fn the_root_folder_lists_the_files_alongside_the_journal_ones() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let mut names: Vec<String> = vol
            .read_dir(vol.root_cnid())
            .expect("read_dir")
            .iter()
            .map(|o| o.name_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ".journal",
                ".journal_info_block",
                "fragmented.bin",
                "link",
                "overflow.bin",
            ],
            "the added files must appear alongside the two newfs_hfs created"
        );
    });
}

#[test]
fn each_file_has_the_size_and_type_its_record_declares() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let frag = entry(vol, FRAGMENTED);
        assert_eq!(frag.data_size(), 8 * 4096, "eight single-block extents");
        assert!(!frag.is_dir());
        assert!(!frag.is_symlink());
        assert_eq!(
            frag.bsd_info().file_mode & S_IFMT,
            S_IFREG,
            "fragmented.bin must be a regular file, got {:#o}",
            frag.bsd_info().file_mode
        );
        assert_eq!(
            frag.bsd_info().permissions(),
            0o644,
            "the permission bits must survive the record"
        );

        let link = entry(vol, LINK);
        assert!(link.is_symlink(), "the mode must mark it as a link");
        assert_eq!(
            link.bsd_info().file_mode & S_IFMT,
            S_IFLNK,
            "a symlink's type bits are S_IFLNK, not S_IFREG"
        );
        assert_eq!(link.data_size(), LINK_TARGET.len() as u64);
    });
}

// --- Fragmented extents -----------------------------------------------

#[test]
fn a_fragmented_file_reads_as_one_contiguous_stream() {
    // The central assertion. Each block of the file carries its own physical
    // block number in its first four bytes, so the read must produce that
    // sequence, ascending, with a stride -- not the file's blocks reassembled in
    // any other order.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let frag = entry(vol, FRAGMENTED);
        let data = vol.read_file(&frag, 1 << 20).expect("read");
        assert_eq!(data.len(), 8 * 4096, "the whole data fork");

        let mut numbers = Vec::new();
        for block in data.chunks(4096) {
            let n = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
            numbers.push(n);
        }
        assert_eq!(numbers.len(), 8);
        assert!(
            numbers.windows(2).all(|w| w[1] > w[0]),
            "the eight physical blocks must appear in ascending order, got {numbers:?}"
        );

        // And the block numbers must be the ones in the extent record, so this
        // is not merely self-consistent.
        let extents = frag
            .as_file()
            .expect("a file record")
            .record
            .data_fork
            .extents
            .raw
            .iter()
            .map(|e| e.start_block)
            .take(8)
            .collect::<Vec<_>>();
        assert_eq!(numbers, extents, "the bytes must match the extents on disk");
    });
}

#[test]
fn every_offset_into_a_fragmented_file_agrees_with_the_whole_read() {
    // A reader that is wrong about extent boundaries can still be self-
    // consistent, so windows are compared against the whole-file read. Offsets
    // straddle each of the eight extent boundaries.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let frag = entry(vol, FRAGMENTED);
        let whole = vol.read_file(&frag, 1 << 20).expect("read");

        for index in 0..8usize {
            let at = (index as u64) * 4096;
            for offset in [at.saturating_sub(1), at, at + 1] {
                if offset as usize + 8 > whole.len() {
                    continue;
                }
                let window = vol.read(&frag, offset, 8).expect("window read");
                assert_eq!(
                    window,
                    &whole[offset as usize..offset as usize + 8],
                    "a read at {offset} disagrees with the whole-file read"
                );
            }
        }
    });
}

#[test]
fn the_tail_of_a_fragmented_file_is_bounded() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let frag = entry(vol, FRAGMENTED);
        let size = frag.data_size() as usize;
        assert_eq!(size, 32768);

        assert_eq!(
            vol.read(&frag, (size - 4) as u64, 4)
                .expect("to the end")
                .len(),
            4
        );
        assert_eq!(
            vol.read(&frag, (size - 4) as u64, 999)
                .expect("past the end")
                .len(),
            4
        );
        assert!(vol
            .read(&frag, size as u64, 10)
            .expect("at the end")
            .is_empty());
    });
}

// --- Symlinks ---------------------------------------------------------

#[test]
fn read_link_returns_the_stored_target() {
    // Apple keeps a symlink's target in the file's data fork, so this reads the
    // same bytes a data read would -- but it must interpret them as a path, not
    // return them as file contents.
    //
    // The checker will not confirm any of this; see the module note.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let link = entry(vol, LINK);
        assert!(link.is_symlink());

        let target = vol.read_link(&link).expect("read_link");
        assert_eq!(
            target, LINK_TARGET,
            "the target must come back exactly, with no terminator or decoding"
        );

        // The target is also readable as data, which is what makes it a target
        // rather than a separate structure.
        let raw = vol.read_file(&link, 256).expect("read");
        assert_eq!(raw, LINK_TARGET.as_bytes());
        assert_eq!(raw.len(), LINK_TARGET.len());
    });
}

#[test]
fn read_link_fails_on_a_regular_file_rather_than_returning_its_contents() {
    // The failure mode worth excluding: readlink on a regular file handing back
    // the file's data, which a caller would then try to open as a path.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let frag = entry(vol, FRAGMENTED);
        assert!(!frag.is_symlink());
        assert!(
            vol.read_link(&frag).is_err(),
            "readlink on a regular file must fail"
        );
    });
}

#[test]
fn read_link_fails_on_a_directory() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let root = vol
            .lookup_cnid(vol.root_cnid())
            .expect("resolve the root")
            .expect("the root exists");
        assert!(root.is_dir());
        assert!(vol.read_link(&root).is_err());
    });
}

// --- The image must not change ----------------------------------------

#[test]
fn reading_these_files_does_not_modify_the_image() {
    let path = image_path(WITH_FILES);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let before = std::fs::read(&path).expect("read before");
    let digest_before = digest(&before);

    {
        let dev = FileDevice::open(&path).expect("open");
        let vol = hfsplus::volume::Volume::open(&dev).expect("mount");

        let frag = entry(&vol, FRAGMENTED);
        for _ in 0..3 {
            let data = vol.read_file(&frag, 1 << 20).expect("read");
            assert_eq!(data.len(), 32768);
        }
        for index in 0..8u64 {
            let _ = vol.read(&frag, index * 4096, 4096).expect("per extent");
        }
        let link = entry(&vol, LINK);
        assert_eq!(vol.read_link(&link).expect("read_link"), LINK_TARGET);
        assert_eq!(
            vol.statfs().expect("statfs").total_blocks,
            vol.header().total_blocks
        );
    }

    let after = std::fs::read(&path).expect("read after");
    assert_eq!(digest_before, digest(&after), "reading modified the image");
}

#[test]
fn the_two_appearances_of_the_volume_agree() {
    // Mounting twice must give the same bytes, so nothing depends on state left
    // behind by a previous read.
    if !require(WITH_FILES) {
        return;
    }
    let first = {
        let dev = FileDevice::open(image_path(WITH_FILES)).expect("open");
        let vol = hfsplus::volume::Volume::open(&dev).expect("mount");
        vol.read_file(&entry(&vol, FRAGMENTED), 1 << 20)
            .expect("read")
    };
    let second = {
        let dev = FileDevice::open(image_path(WITH_FILES)).expect("open");
        let vol = hfsplus::volume::Volume::open(&dev).expect("mount");
        vol.read_file(&entry(&vol, FRAGMENTED), 1 << 20)
            .expect("read")
    };
    assert_eq!(digest(&first), digest(&second));
}

// --- The independent checker ------------------------------------------

#[test]
fn fsck_accepts_the_image_and_leaves_it_alone() {
    // The check that the generator wrote something coherent rather than merely
    // self-consistent: a wrong extent count, a stale file count, an unallocated
    // block or a bad directory valence all produce a repair here, and a repair
    // shows up as the probe differing from the original.
    let path = image_path(WITH_FILES);
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
            "the generated image must be sound:\n{text}"
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
    probe.push(format!(
        "hfsplus-with-files-{}-{}.img",
        WITH_FILES,
        std::process::id()
    ));
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
        "the generated image must be sound:\n{text}"
    );
    assert_eq!(
        digest(&std::fs::read(&path).expect("read the original")),
        digest(&after),
        "fsck repaired the image, so something in it was wrong:\n{text}"
    );
}

#[test]
fn the_manifests_file_entries_describe_what_the_volume_actually_has() {
    // The `[[files]]` section is hand-edited, so it is the specification rather
    // than something captured from the image. This is what stops it drifting into
    // a description of whatever the generator happened to produce: each field is
    // checked against what a caller actually sees.
    if !image_path(WITH_FILES).exists() {
        eprintln!("skipping: {} not built", image_path(WITH_FILES).display());
        return;
    }
    let text = std::fs::read_to_string(common::manifest(WITH_FILES))
        .unwrap_or_else(|e| panic!("read manifest: {e}"));
    let manifest = common::manifest::Manifest::parse(&text);
    let count = manifest.array_len("files");
    assert_eq!(count, 3, "the manifest must specify all three added files");

    with_volume(WITH_FILES, |vol| {
        for index in 1..=count {
            let name = manifest
                .array_item("files", index, "name")
                .unwrap_or_else(|| panic!("files.{index} has no name"));
            let object = entry(vol, name);
            let mode_bits = object.bsd_info().file_mode;

            let size: u64 = manifest
                .array_item("files", index, "size")
                .unwrap_or_else(|| panic!("{name}: no size"))
                .parse()
                .expect("size parses");
            assert_eq!(object.data_size(), size, "{name}: size from the manifest");

            let mode = manifest
                .array_item("files", index, "mode")
                .unwrap_or_else(|| panic!("{name}: no mode"));
            let want = u32::from_str_radix(mode.trim_start_matches("0o"), 8).expect("mode parses");
            assert_eq!(u32::from(mode_bits), want, "{name}: mode from the manifest");

            let cnid: u32 = manifest
                .array_item("files", index, "cnid")
                .unwrap_or_else(|| panic!("{name}: no cnid"))
                .parse()
                .expect("cnid parses");
            assert_eq!(object.cnid().0, cnid, "{name}: CNID from the manifest");

            // The kind is what decides whether `target` may be read at all, so a
            // manifest calling a regular file a symlink has to fail here rather
            // than at some later caller's readlink.
            match manifest.array_item("files", index, "kind") {
                Some("symlink") => {
                    assert!(object.is_symlink(), "{name}: manifest says symlink");
                    let target = manifest
                        .array_item("files", index, "target")
                        .unwrap_or_else(|| panic!("{name}: a symlink must state its target"));
                    assert_eq!(
                        &vol.read_link(&object).expect("read_link"),
                        target,
                        "{name}: target from the manifest"
                    );
                }
                Some("file") => assert!(!object.is_symlink(), "{name}: manifest says file"),
                other => panic!("{name}: unexpected kind {other:?}"),
            }
        }
    });
}

// --- Extents that overflow into the B-tree ----------------------------

#[test]
fn a_file_whose_extents_overflow_is_read_through_the_extents_tree() {
    // The end-to-end version of what `tests/extents_overflow.rs` covers at the
    // reader level. `overflow.bin` keeps eight extents inline and two in the
    // volume's extents overflow B-tree, keyed on `(forkType, fileID,
    // startBlock)`. Before the resolver was wired into `Volume::fork_reader`, a
    // read past the eighth extent returned zeros, so this file would have been
    // silently half empty.
    //
    // Every block carries its own physical block number, so the whole file must
    // read as ten ascending numbers -- and the last two can only come from the
    // tree.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let file = entry(vol, OVERFLOW);
        assert_eq!(
            file.data_size(),
            10 * 4096,
            "ten blocks, eight of them inline"
        );

        let data = vol.read_file(&file, 1 << 20).expect("read");
        assert_eq!(data.len(), 10 * 4096);

        let numbers: Vec<u32> = data
            .chunks(4096)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert!(
            numbers.windows(2).all(|w| w[1] > w[0]),
            "all ten block numbers must ascend, got {numbers:?}"
        );
        assert!(
            numbers[0] > 0,
            "the last two blocks are only findable through the tree, so their \
             numbers cannot be zero; got {numbers:?}"
        );

        // The inline eight and the overflow two must both be real content, not
        // a zero-fill fallback: a distinct pattern per block is what the
        // generator writes, and zero would mean the resolver was never consulted.
        assert!(
            numbers.iter().all(|n| *n != 0),
            "every block must be real data, got {numbers:?}"
        );
    });
}

#[test]
fn a_read_past_the_inline_extents_agrees_with_the_whole_file() {
    // The boundary that matters is between extent eight and extent nine, since
    // that is where the data stops being inline and starts coming from the tree.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let file = entry(vol, OVERFLOW);
        let whole = vol.read_file(&file, 1 << 20).expect("read");

        for block in [7u64, 8, 9] {
            let at = block * 4096;
            for offset in [at.saturating_sub(2), at, at + 2] {
                if offset as usize + 8 > whole.len() {
                    continue;
                }
                assert_eq!(
                    vol.read(&file, offset, 8).expect("window"),
                    &whole[offset as usize..offset as usize + 8],
                    "a read at {offset} disagrees with the whole-file read"
                );
            }
        }
    });
}

#[test]
fn the_overflowing_file_declares_every_block_it_uses() {
    // `totalBlocks` counts inline *and* overflow blocks, so a fork that spills
    // must declare all ten while showing only eight extents. fsck checks this
    // ("Incorrect block count"), and the crate has to read it consistently too.
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let file = entry(vol, OVERFLOW);
        let fork = file.as_file().expect("a file record").record.data_fork;
        let inline: u64 = (0..8)
            .map(|i| u64::from(fork.extents.raw[i].block_count))
            .sum();
        assert_eq!(inline, 8, "eight blocks are described inline");
        assert_eq!(fork.total_blocks, 10, "but the fork declares all ten");
        assert!(fork.needs_overflow(), "so the fork must consult the tree");
        assert_eq!(fork.overflow_block_count(), 2);
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

#[test]
fn a_fork_extent_past_the_volume_is_refused_when_a_read_reaches_it() {
    // A descriptor's `startBlock` is a device block number and nothing in the
    // record bounds it -- the block number is multiplied by the block size before
    // any read, so a descriptor past the end gives an offset past the end.
    //
    // Mining reference: `core/FileExtentMapping.c` `MapFileBlockC` computes
    // `block_num * jhdr_size` with no bound on `block_num`; the bound comes from
    // the caller.
    //
    // Worth pinning because of *when* it is caught. A read confined to the earlier,
    // valid extents succeeds -- the same tolerance that lets a fork short of its
    // logical size read as zeros -- so the damage is silent until a read reaches
    // it. The checker refuses the whole volume instead, which is where the
    // complaint belongs.
    let path = common::repo_root().join("tests/images/replayed/fork-extent-past-volume.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let units: Vec<u16> = FRAGMENTED_NAME.encode_utf16().collect();
    let object = vol
        .lookup(vol.root_cnid(), &units)
        .expect("lookup")
        .expect("fragmented.bin must be listed");

    // Extent 2 is the broken one, so offset 2 blocks in reaches it. The reader
    // refuses, but as a *device* truncation rather than by naming the bound --
    // it multiplies the block number and asks for bytes that are not there. That
    // is safe and it is the reader's only line of defence here; the message
    // naming the extent bound is the checker's.
    let err = vol
        .read(&object, 2 * 4096, 64)
        .expect_err("a read reaching the bad extent must be refused");
    assert!(
        err.to_string().contains("truncated"),
        "the read must fail as a short read, got {err}"
    );

    // And the earlier extents still read, which is the tolerance being described
    // rather than a second bug.
    assert_eq!(
        vol.read(&object, 0, 64)
            .expect("a read inside the valid extents")
            .len(),
        64
    );

    // The checker refuses the volume outright, and is the place the bound is
    // named.
    match hfsplus::check::check(&vol, None) {
        Err(e) => assert!(
            e.to_string().contains("outside volume"),
            "the checker must name the extent bound, got {e}"
        ),
        Ok(report) => assert!(
            !report.is_clean(),
            "the checker must not call this volume consistent: {:?}",
            report.describe()
        ),
    }
}

#[test]
fn a_symlink_whose_target_is_empty_is_refused() {
    // A symlink's target *is* its data fork, so an empty one names nothing.
    // Returning "" would hand back a path, and a caller would try to resolve it
    // -- against the process's working directory in the worst case.
    //
    // Mining reference: `core/hfs_xattr.c` reads a link target out of the file's
    // data fork for HFSPlus, so the fork and the target cannot disagree.
    let path = common::repo_root().join("tests/images/replayed/symlink-empty-target.img");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open");
    let vol = Volume::open(&dev).expect("mount");

    let units: Vec<u16> = "link".encode_utf16().collect();
    let object = vol
        .lookup(vol.root_cnid(), &units)
        .expect("lookup")
        .expect("the symlink must still be listed");
    assert!(object.is_symlink(), "it is still a symlink");
    assert_eq!(object.data_size(), 0, "its data fork is empty");

    let err = vol
        .read_link(&object)
        .expect_err("an empty target must be refused, not returned");
    assert!(
        err.to_string().contains("no target"),
        "the error must say why, got {err}"
    );

    // And the sound symlink still reads, so the check is not simply refusing all
    // of them.
    let good = common::image("journal-with-files");
    if good.exists() {
        let dev = FileDevice::open(&good).expect("open");
        let vol = Volume::open(&dev).expect("mount");
        let object = vol
            .lookup(vol.root_cnid(), &units)
            .expect("lookup")
            .expect("link");
        assert_eq!(
            vol.read_link(&object).expect("a sound symlink"),
            "../elsewhere/target"
        );
    }
}

// --- READDIRPLUS ----------------------------------------------------------

#[test]
fn read_dir_plus_yields_entries_with_metadata_inlined() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        // A large limit means everything comes back in one shot.
        let (entries, cursor) = vol
            .read_dir_plus(vol.root_cnid(), DirCursor::start(), usize::MAX)
            .expect("read_dir_plus");

        // Should match what read_dir returns, but with full metadata.
        let mut names: Vec<String> = entries.iter().map(|o| o.name_string()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ".journal",
                ".journal_info_block",
                "fragmented.bin",
                "link",
                "overflow.bin",
            ],
            "the plus call must list the same entries as read_dir"
        );

        // Each entry carries real stat metadata.
        let frag = entries
            .iter()
            .find(|o| o.name_string() == FRAGMENTED)
            .expect("fragmented.bin must be present");
        assert_eq!(frag.as_file().expect("must be a file").data_size, 8 * 4096);

        // The cursor should not advance when the directory is exhausted.
        let (empty_batch, same_cursor) = vol
            .read_dir_plus(vol.root_cnid(), cursor, 10)
            .expect("read_dir_plus after completion");
        assert!(empty_batch.is_empty(), "no entries after exhaustion");
        assert_eq!(same_cursor, cursor, "cursor stays put past the end");
    });
}

#[test]
fn read_dir_plus_resumes_from_cursor_in_batches() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        // Walk the root folder two entries at a time.
        let mut cursor = DirCursor::start();
        let mut all_names = Vec::new();

        loop {
            let (batch, next) = vol
                .read_dir_plus(vol.root_cnid(), cursor, 2)
                .expect("read_dir_plus");
            for obj in &batch {
                all_names.push(obj.name_string());
            }
            if batch.is_empty() {
                break;
            }
            cursor = next;
        }

        all_names.sort();
        assert_eq!(
            all_names,
            vec![
                ".journal",
                ".journal_info_block",
                "fragmented.bin",
                "link",
                "overflow.bin",
            ],
            "batched reads must collect the same entries as a single call"
        );
    });
}

#[test]
fn read_dir_plus_metadata_matches_lookup() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let (plus_entries, _) = vol
            .read_dir_plus(vol.root_cnid(), DirCursor::start(), usize::MAX)
            .expect("read_dir_plus");

        // Every entry from read_dir_plus must match a lookup by the same name.
        for obj in &plus_entries {
            let units: Vec<u16> = obj.name().to_vec();
            let looked_up = vol
                .lookup(vol.root_cnid(), &units)
                .expect("lookup")
                .expect("entry must still be present");

            assert_eq!(obj.cnid(), looked_up.cnid());
            assert_eq!(obj.data_size(), looked_up.data_size());
            assert_eq!(obj.bsd_info(), looked_up.bsd_info());
        }
    });
}

/// Name of the fragmented file in the generated fixtures.
const FRAGMENTED_NAME: &str = "fragmented.bin";

#[test]
fn volume_bmap_translates_a_fragmented_file_offset() {
    if !require(WITH_FILES) {
        return;
    }
    with_volume(WITH_FILES, |vol| {
        let frag = entry(vol, FRAGMENTED);
        let bs = 4096u64;

        // First block of the file maps to some physical device offset.
        let dev_offset = vol.bmap(&frag, 0).expect("bmap");
        assert_eq!(dev_offset % bs, 0, "device offset must be block-aligned");

        // Second block maps to a different physical offset (fragmented file).
        let second = vol.bmap(&frag, bs).expect("bmap second block");
        assert_ne!(dev_offset, second);

        // An unaligned offset is rejected.
        assert!(vol.bmap(&frag, 1).is_err());

        // Past the file's data fork is out of range.
        assert!(vol.bmap(&frag, frag.data_size() + 1).is_err());
    });
}
