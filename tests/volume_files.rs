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
//! generator wrote something coherent rather than merely self-consistent.
//!
//! Mining reference: Apple `core/hfs_format.h` for `struct HFSPlusCatalogFile`
//! and `struct HFSPlusBSDInfo`; `core/hfs_xattr.c` for a link target living in
//! the data fork.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::record::{S_IFLNK, S_IFMT, S_IFREG};
use hfsplus::volume::Object;

/// The image `tools/mkfiles.py` produces from `journaled-hfsplus`.
const WITH_FILES: &str = "journal-with-files";

/// A file whose eight extents are physically scattered.
const FRAGMENTED: &str = "fragmented.bin";
/// A symbolic link.
const LINK: &str = "link";
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
            vec![".journal", ".journal_info_block", "fragmented.bin", "link"],
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

        assert_eq!(vol.read(&frag, (size - 4) as u64, 4).expect("to the end").len(), 4);
        assert_eq!(vol.read(&frag, (size - 4) as u64, 999).expect("past the end").len(), 4);
        assert!(vol.read(&frag, size as u64, 10).expect("at the end").is_empty());
    });
}

// --- Symlinks ---------------------------------------------------------

#[test]
fn read_link_returns_the_stored_target() {
    // Apple keeps a symlink's target in the file's data fork, so this reads the
    // same bytes a data read would -- but it must interpret them as a path, not
    // return them as file contents.
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
        assert_eq!(vol.statfs().expect("statfs").total_blocks, vol.header().total_blocks);
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
        vol.read_file(&entry(&vol, FRAGMENTED), 1 << 20).expect("read")
    };
    let second = {
        let dev = FileDevice::open(image_path(WITH_FILES)).expect("open");
        let vol = hfsplus::volume::Volume::open(&dev).expect("mount");
        vol.read_file(&entry(&vol, FRAGMENTED), 1 << 20).expect("read")
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
    let Some(fsck) = common::fsck_available() else {
        eprintln!("skipping: fsck.hfsplus not installed");
        return;
    };
    let path = image_path(WITH_FILES);
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let mut probe = std::env::temp_dir();
    probe.push(format!("hfsplus-with-files-{}-{}.img", WITH_FILES, std::process::id()));
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
    assert_eq!(count, 2, "the manifest must specify both added files");

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
            assert_eq!(
                u32::from(mode_bits),
                want,
                "{name}: mode from the manifest"
            );

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

fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}