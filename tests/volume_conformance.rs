// SPDX-License-Identifier: BSD-2-Clause

//! Read-only filesystem conformance against the real corpus.
//!
//! This is the layer a FUSE adapter would sit on, so these tests exercise the
//! whole path: volume header, catalog, name comparison, fork reads and the
//! allocation bitmap.
//!
//! Mining reference for the semantics under test: Apple `core/hfs_vfsutils.c`
//! (`hfs_MountHFSPlusVolume`), `core/hfs_statfs.c` (`hfs_bstatfs`), and
//! `core/VolumeAllocation.c` for the bitmap.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::cnid::{ROOT_FOLDER_ID, ROOT_PARENT_ID};
use hfsplus::format::volume_header::FileSystemKind;
use hfsplus::volume::{Object, Volume};
use std::path::PathBuf;

fn corpus() -> Vec<(&'static str, &'static str, bool)> {
    vec![
        ("basic-hfsplus", "BasicVolume", false),
        ("basic-hfsplus-1k", "SmallBlocks", false),
        ("basic-hfsplus-8k", "LargeBlocks", false),
        ("basic-hfsplus-16k", "HugeBlocks", false),
        ("hfsx-case-sensitive", "CaseSensitive", true),
        ("hfsx-case-insensitive", "CaseInsensitive", false),
        ("journaled-hfsplus", "Journaled", false),
        ("journaled-hfsplus-1k", "Journaled1K", false),
    ]
}

/// Run `f` against an opened volume, skipping images that were not generated.
fn with_volume(name: &str, f: impl FnOnce(&Volume<'_, FileDevice>)) {
    let path: PathBuf = common::image(name);
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open image");
    let vol = Volume::open(&dev).unwrap_or_else(|e| panic!("{name}: mount failed: {e}"));
    f(&vol);
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[test]
fn every_corpus_image_mounts() {
    for (name, volume_name, is_hfsx) in corpus() {
        with_volume(name, |vol| {
            assert_eq!(
                vol.kind(),
                if is_hfsx {
                    FileSystemKind::HfsX
                } else {
                    FileSystemKind::HfsPlus
                },
                "{name}: filesystem kind"
            );
            assert!(vol.is_clean(), "{name}: generated volumes are clean");
            assert_eq!(vol.root_cnid(), ROOT_FOLDER_ID, "{name}");

            // The volume name comes from the root folder's catalog record, not
            // from the header. fsck.hfsplus prints the same name.
            assert_eq!(
                vol.name().expect("volume name"),
                volume_name,
                "{name}: volume name must match what fsck.hfsplus reported"
            );
        });
    }
}

#[test]
fn classic_hfs_is_refused_rather_than_misparsed() {
    let path = common::image("classic-hfs");
    if !path.exists() {
        eprintln!("skipping: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).unwrap();
    assert!(
        Volume::open(&dev).is_err(),
        "classic HFS must not mount as an HFS+ volume"
    );
}

#[test]
fn the_root_folder_resolves_by_cnid_and_by_name() {
    for (name, volume_name, _) in corpus() {
        with_volume(name, |vol| {
            // By CNID: one descent to the thread record, one more to the record.
            let by_cnid = vol
                .lookup_cnid(ROOT_FOLDER_ID)
                .expect("lookup root by cnid")
                .unwrap_or_else(|| panic!("{name}: root must resolve by CNID"));
            assert!(by_cnid.is_dir(), "{name}: root must be a directory");

            // By name under the root parent.
            let by_name = vol
                .lookup(ROOT_PARENT_ID, &units(volume_name))
                .expect("lookup root by name")
                .unwrap_or_else(|| panic!("{name}: root must resolve by name"));
            assert_eq!(by_name.cnid(), ROOT_FOLDER_ID, "{name}");
            assert_eq!(by_name.name_string(), volume_name, "{name}");

            // And a non-existent name is absent rather than an error.
            assert!(vol
                .lookup(ROOT_PARENT_ID, &units("nope"))
                .unwrap()
                .is_none());
        });
    }
}

#[test]
fn statfs_agrees_with_the_volume_header() {
    for (name, _, _) in corpus() {
        with_volume(name, |vol| {
            let st = vol.statfs().expect("statfs");
            let vh = vol.header();

            assert_eq!(st.block_size, vh.block_size, "{name}");
            assert_eq!(st.total_blocks, vh.total_blocks, "{name}");
            assert_eq!(st.free_blocks, vh.free_blocks, "{name}");
            assert_eq!(st.total_bytes, vh.volume_bytes().unwrap(), "{name}");
            assert_eq!(
                st.free_bytes,
                u64::from(vh.free_blocks) * u64::from(vh.block_size),
                "{name}"
            );
            assert_eq!(st.file_count, vh.file_count, "{name}");
            assert_eq!(st.folder_count, vh.folder_count, "{name}");
            assert_eq!(st.journaled, vh.is_journaled(), "{name}");

            // Free blocks cannot exceed total blocks; anything else means the
            // header is inconsistent and statfs would be reporting nonsense.
            assert!(st.free_blocks <= st.total_blocks, "{name}");
            assert_eq!(st.max_name_len, 255, "{name}");
        });
    }
}

#[test]
fn the_allocation_bitmap_is_readable_and_self_consistent() {
    for (name, _, _) in corpus() {
        with_volume(name, |vol| {
            let bm = vol.allocation_bitmap().expect("allocation bitmap");
            let total = vol.header().total_blocks;
            assert_eq!(bm.total_blocks(), total, "{name}");

            // Every in-range block has an answer, and beyond the volume reads free
            // rather than erroring, which is what allocation code needs.
            for block in [0u32, 1, total / 2, total - 1] {
                let allocated = bm.is_allocated(block).expect("is_allocated");
                assert_eq!(
                    bm.is_free(block).unwrap(),
                    !allocated,
                    "{name}: block {block}"
                );
            }
            assert!(!bm.is_allocated(total).unwrap(), "{name}: past the volume");
            assert!(
                !bm.is_allocated(u32::MAX).unwrap(),
                "{name}: past the volume"
            );

            // Block 0 holds the volume header and must be allocated.
            assert!(
                bm.is_allocated(0).unwrap(),
                "{name}: block 0 is the volume header"
            );

            // The bitmap must agree with the volume header exactly, not merely
            // approximately. This is the strongest cross-check available without
            // writing anything: the header's freeBlocks and the bitmap's
            // population of bits are independent records of the same fact.
            let allocated = bm.count_all_allocated().expect("count");
            assert_eq!(
                allocated + u64::from(vol.header().free_blocks),
                u64::from(total),
                "{name}: bitmap says {allocated} allocated and the header says {} free, \
                 which must sum to {total}",
                vol.header().free_blocks
            );
        });
    }
}

#[test]
fn journaled_volumes_report_their_journal() {
    for (name, _, _) in corpus() {
        with_volume(name, |vol| {
            let expected = name.starts_with("journaled");
            assert_eq!(vol.is_journaled(), expected, "{name}: journal flag");

            // Mining reference: core/hfs_journal.c places two catalog records for
            // the journal, so a journaled volume's root is not empty.
            let entries = vol.read_dir(ROOT_FOLDER_ID).expect("read_dir root");
            let names: Vec<String> = entries.iter().map(|e| e.name_string()).collect();
            if expected {
                assert!(
                    names.iter().any(|n| n == ".journal"),
                    "{name}: a journaled volume must expose .journal, saw {names:?}"
                );
                assert!(
                    names.iter().any(|n| n == ".journal_info_block"),
                    "{name}: a journaled volume must expose .journal_info_block, saw {names:?}"
                );

                // Both must be real files with real forks.
                for wanted in [".journal", ".journal_info_block"] {
                    let e = entries
                        .iter()
                        .find(|e| e.name_string() == wanted)
                        .expect("journal entry");
                    assert!(!e.is_dir(), "{name}: {wanted} must be a file");
                    assert!(
                        e.data_size() > 0,
                        "{name}: {wanted} must have a non-empty data fork, saw {}",
                        e.data_size()
                    );
                }
            } else {
                assert!(
                    entries.is_empty(),
                    "{name}: unexpected root contents {names:?}"
                );
            }
        });
    }
}

#[test]
fn directory_listing_returns_objects_not_thread_records() {
    for (name, _, _) in corpus() {
        with_volume(name, |vol| {
            let entries = vol.read_dir(ROOT_FOLDER_ID).expect("read_dir");
            for e in &entries {
                // A thread record has an empty key name; surfacing one would put a
                // nameless entry in the listing.
                assert!(
                    !e.name().is_empty(),
                    "{name}: listing produced an empty name"
                );
                assert!(!e.is_dir() || !e.name_string().is_empty(), "{name}");
            }
        });
    }
}

#[test]
fn object_metadata_is_complete_and_consistent() {
    for (name, volume_name, _) in corpus() {
        with_volume(name, |vol| {
            let root = vol
                .lookup(ROOT_PARENT_ID, &units(volume_name))
                .unwrap()
                .expect("root");
            let Object::Directory(dir) = &root else {
                panic!("{name}: root must be a directory");
            };

            // hfsprogs writes fileMode 0 for the root folder, so the mode bits
            // carry nothing; the record type is what says "directory".
            assert_eq!(dir.bsd_info.file_mode, 0, "{name}");
            assert_eq!(dir.bsd_info.file_type(), 0, "{name}");
            assert_eq!(dir.bsd_info.owner_id, 0, "{name}");
            assert_eq!(dir.bsd_info.group_id, 0, "{name}");
            assert_eq!(dir.user_info.len(), 16, "{name}");
            assert_eq!(dir.finder_info.len(), 16, "{name}");

            // All five timestamps are present on every record.
            let t = dir.times;
            assert!(t.created.raw > 0, "{name}: the volume has a creation time");
            assert!(
                t.modified.raw > 0,
                "{name}: the volume has a modification time"
            );

            // A directory has no forks.
            assert_eq!(root.data_size(), 0, "{name}");
            assert_eq!(root.resource_size(), 0, "{name}");
            assert!(!root.has_resource_fork(), "{name}");
        });
    }
}

#[test]
fn reading_a_directory_as_a_file_is_refused() {
    with_volume("basic-hfsplus", |vol| {
        let root = vol.lookup_cnid(ROOT_FOLDER_ID).unwrap().expect("root");
        assert!(vol.read(&root, 0, 10).is_err());
        assert!(vol.read_file(&root, 10).is_err());
        assert!(vol.read_resource(&root, 0, 10).is_err());
        assert!(vol.read_link(&root).is_err());
    });
}

#[test]
fn a_case_sensitive_volume_resolves_only_the_exact_spelling() {
    // The behavioural difference that a FUSE mount must expose.
    with_volume("basic-hfsplus", |vol| {
        assert!(!vol.is_case_sensitive(), "HFS+ folds case");
        let exact = vol.lookup(ROOT_PARENT_ID, &units("BasicVolume")).unwrap();
        let folded = vol.lookup(ROOT_PARENT_ID, &units("basicvolume")).unwrap();
        assert!(exact.is_some());
        assert_eq!(
            exact.as_ref().map(|o| o.cnid()),
            folded.as_ref().map(|o| o.cnid()),
            "both spellings must resolve to the same object on a folding volume"
        );
        // The name reported is the one stored on disk, not the one that was asked
        // for. On a folding volume both spellings reach the same record, and
        // echoing the request would report a file that does not exist.
        assert_eq!(
            exact.as_ref().map(|o| o.name_string()),
            Some("BasicVolume".into())
        );
        assert_eq!(
            folded.as_ref().map(|o| o.name_string()),
            Some("BasicVolume".into()),
            "a folded lookup must still report the catalog's spelling"
        );
    });

    with_volume("hfsx-case-sensitive", |vol| {
        assert!(
            vol.is_case_sensitive(),
            "HFSX with kHFSBinaryCompare is case sensitive"
        );
        assert!(vol
            .lookup(ROOT_PARENT_ID, &units("CaseSensitive"))
            .unwrap()
            .is_some());
        assert!(vol
            .lookup(ROOT_PARENT_ID, &units("casesensitive"))
            .unwrap()
            .is_none());
    });
}

#[test]
fn absent_objects_are_absent_not_errors() {
    for (name, _, _) in corpus() {
        with_volume(name, |vol| {
            // Not-found is the normal result of a lookup, so it must not be an
            // error; only genuine structural problems are.
            assert!(vol
                .lookup(ROOT_PARENT_ID, &units("nope"))
                .unwrap()
                .is_none());
            assert!(vol.lookup_cnid(999_999.into()).unwrap().is_none());
            assert!(vol.read_dir(999_999.into()).unwrap().is_empty());
        });
    }
}

#[test]
fn the_volume_is_opened_through_the_same_path_every_time() {
    // Opening twice must give identical answers; anything cached at mount time
    // would be a correctness bug.
    for (name, volume_name, _) in corpus() {
        let path = common::image(name);
        if !path.exists() {
            continue;
        }
        let dev = FileDevice::open(&path).unwrap();
        let a = Volume::open(&dev).unwrap();
        let b = Volume::open(&dev).unwrap();
        assert_eq!(a.name().unwrap(), b.name().unwrap(), "{name}");
        assert_eq!(
            a.lookup_cnid(ROOT_FOLDER_ID).unwrap().map(|o| o.cnid()),
            b.lookup_cnid(ROOT_FOLDER_ID).unwrap().map(|o| o.cnid()),
            "{name}"
        );
        assert_eq!(a.name().unwrap(), volume_name, "{name}");
    }
}
