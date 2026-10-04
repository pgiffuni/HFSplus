//! Catalog conformance against the real corpus.
//!
//! These tests go past "it parses" and check what a caller actually depends on:
//! that a name can be found, that the volume's own name is recoverable, and that
//! directory contents come back in the order the formatter wrote them.
//!
//! # The key layout, verified rather than assumed
//!
//! For an object with CNID `N`, name `X` and parent `P`, a real HFS+ catalog
//! stores two records:
//!
//! ```text
//! key (P, X)  ->  the folder or file record, carrying N in its body
//! key (N, "")  ->  the thread record, carrying P and X in its body
//! ```
//!
//! The surprising half is that the thread **key**'s parentID is the object's own
//! CNID, not its parent's. Mining reference: `core/hfs_catalog.c`
//! `buildthreadkey` builds the thread key from the node's CNID, while
//! `buildthread` copies the *main* key's parent and name into the record body:
//!
//! ```c
//! rec->parentID = key->parentID;
//! bcopy(&key->nodeName, &rec->nodeName, ...);
//! ```
//!
//! Verified on `basic-hfsplus`, whose root folder has CNID 2, name `BasicVolume`
//! and parent 1: the folder record is keyed `(1, "BasicVolume")` and the thread
//! record is keyed `(2, "")` with body `(parentID = 1, name = "BasicVolume")`.
//!
//! A consequence worth stating because it invalidates an obvious optimisation:
//! thread keys are **not** one contiguous key range, since each has a different
//! parentID. Enumerating a whole volume means scanning the catalog, not one range.

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::cnid::{ROOT_FOLDER_ID, ROOT_PARENT_ID};
use hfsplus::catalog::key::{K_HFS_BINARY_COMPARE, K_HFS_CASE_FOLDING};
use hfsplus::catalog::record::{CatalogRecord, FILE_RECORD_SIZE, FOLDER_RECORD_SIZE};
use hfsplus::catalog::{Catalog, CatalogKey, Cnid};
use hfsplus::format::volume_header::VolumeHeader;
use hfsplus::unicode::{Comparator, Ordering};

/// Image, the volume name it was created with, and whether it is HFSX.
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

/// Run `f` with the header and catalog of one corpus image.
///
/// The catalog borrows the device, so the device must outlive the catalog. A
/// closure expresses that without a leaked allocation, and it keeps the "was the
/// corpus built?" check in one place.
fn with_catalog(name: &str, f: impl FnOnce(&VolumeHeader, &Catalog<'_, FileDevice>)) {
    let path = common::image(name);
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return;
    }
    let dev = FileDevice::open(&path).expect("open image");
    let vh = VolumeHeader::read_from(&dev).expect("volume header");
    let catalog = Catalog::open(&dev, &vh.catalog_file, vh.block_size, vh.is_hfsx())
        .unwrap_or_else(|e| panic!("{name}: catalog open failed: {e}"));
    f(&vh, &catalog);
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[test]
fn every_corpus_catalog_opens_with_the_right_comparator() {
    for (name, _, is_hfsx) in corpus() {
        with_catalog(name, |vh, catalog| {
            // Case sensitivity requires HFSX *and* kHFSBinaryCompare; this crate
            // enforces both. See src/unicode/compare.rs for why neither alone
            // suffices.
            let expected = if name == "hfsx-case-sensitive" {
                Comparator::Binary
            } else {
                Comparator::CaseFolding
            };
            assert_eq!(catalog.comparator(), expected, "{name}: is_hfsx={is_hfsx}");
            assert_eq!(
                catalog.comparator().is_case_sensitive(),
                name == "hfsx-case-sensitive",
                "{name}"
            );
            assert_eq!(vh.root_folder_id(), ROOT_FOLDER_ID.0, "{name}");
        });
    }
}

#[test]
fn case_sensitivity_needs_the_signature_and_the_byte_to_agree() {
    // Apple decides this in `hfs_mounthfsplus` by testing both
    // `(hfs_flags & HFS_X)` and `btinfo.keyCompareType == kHFSBinaryCompare`.
    // The corpus happens to have the two in agreement on every volume, so the
    // conjunction is never exercised -- which is exactly why it is worth stating
    // here, where it is.

    // Each corpus volume, with the byte its catalog actually carries.
    // `with_catalog` hands the volume to a closure rather than returning it, so
    // the byte is collected into a `Cell` the closure can write through.
    let observed: Vec<(bool, u8)> = corpus()
        .into_iter()
        .map(|(name, _, is_hfsx)| {
            let byte = std::cell::Cell::new(0u8);
            with_catalog(name, |_, catalog| {
                byte.set(catalog.header().key_compare_type.code());
            });
            (is_hfsx, byte.get())
        })
        .collect();

    // Sanity: the corpus really does span both bytes, so the cases below are not
    // vacuous duplicates of each other.
    assert!(
        observed.iter().any(|(_, b)| *b == K_HFS_BINARY_COMPARE),
        "expected at least one case-sensitive catalog, got {observed:?}"
    );
    assert!(
        observed.iter().any(|(_, b)| *b == K_HFS_CASE_FOLDING),
        "expected at least one case-folding catalog, got {observed:?}"
    );

    // The four combinations. Only HFSX *with* the binary byte is sensitive.
    for is_hfsx in [false, true] {
        for byte in [K_HFS_CASE_FOLDING, K_HFS_BINARY_COMPARE] {
            let got = Comparator::for_volume(is_hfsx, byte);
            let want_sensitive = is_hfsx && byte == K_HFS_BINARY_COMPARE;
            assert_eq!(
                got.is_case_sensitive(),
                want_sensitive,
                "is_hfsx={is_hfsx} keyCompareType=0x{byte:02x} chose {got:?}"
            );
        }
    }

    // The one combination a real volume can reach and a naive reader gets wrong:
    // a folding volume that carries the binary byte anyway. Treating the byte as
    // authoritative would make it case-sensitive, and every lookup of a
    // differently-cased name would miss.
    assert_eq!(
        Comparator::for_volume(false, K_HFS_BINARY_COMPARE),
        Comparator::CaseFolding,
        "an HFS+ volume folds names regardless of the byte it carries"
    );
}

#[test]
fn the_volume_name_is_the_root_folder_catalog_record() {
    // Mining reference: core/hfs_vfsutils.c hfs_MountHFSPlusVolume does
    // cat_idlookup(kHFSRootFolderID) and copies cd_nameptr into vcb->vcbVN.
    // fsck.hfsplus prints the same names, so these expectations are confirmed
    // independently rather than being whatever this code happens to produce.
    for (name, volume_name, _) in corpus() {
        with_catalog(name, |_vh, catalog| {
            // The main record is keyed by (parent, name).
            let record = catalog
                .lookup(ROOT_PARENT_ID, &units(volume_name))
                .unwrap_or_else(|e| panic!("{name}: lookup: {e}"))
                .unwrap_or_else(|| panic!("{name}: no record for {volume_name:?}"));

            let folder = match &record {
                CatalogRecord::Folder(f) => *f,
                other => panic!("{name}: expected a folder record, got {other:?}"),
            };
            assert_eq!(
                folder.folder_id, ROOT_FOLDER_ID,
                "{name}: must be the root folder"
            );
            // hfsprogs leaves `fileMode` zero on the root folder record rather
            // than writing a directory mode with 0755. Verified byte-wise: the
            // bsdInfo at body offset 32 is all zeros on every corpus image. The
            // root is a directory because its record *type* says so, not because
            // its mode says so, so nothing here may depend on the mode bits.
            assert_eq!(
                folder.bsd_info.file_mode, 0,
                "{name}: hfsprogs writes fileMode 0 for the root folder"
            );
            assert_eq!(folder.bsd_info.file_type(), 0, "{name}");
            assert_eq!(folder.bsd_info.owner_id, 0, "{name}: no uid is recorded");
            assert_eq!(folder.bsd_info.group_id, 0, "{name}: no gid is recorded");
            // Valence counts real children, and it agrees with read_dir below.
            // A journaled volume has two (`.journal` and `.journal_info_block`),
            // a plain one has none.
            let child_count = catalog.read_dir(ROOT_FOLDER_ID).unwrap().len() as u32;
            assert_eq!(
                folder.valence, child_count,
                "{name}: valence must match the number of children read_dir returns"
            );

            // The thread record, keyed by the root's own CNID, repeats parent and name.
            let thread = catalog
                .lookup(ROOT_FOLDER_ID, &[])
                .unwrap_or_else(|e| panic!("{name}: thread lookup: {e}"))
                .unwrap_or_else(|| panic!("{name}: no thread record under CNID 2"));
            let t = match thread {
                CatalogRecord::Thread(t) => t,
                other => panic!("{name}: expected a thread record, got {other:?}"),
            };
            assert_eq!(t.parent_id, ROOT_PARENT_ID, "{name}");
            assert_eq!(
                t.name_string(),
                volume_name,
                "{name}: volume name must match what fsck.hfsplus reported"
            );
            assert!(t.is_folder(), "{name}: root must be a folder thread record");

            // And the CNID is recoverable from the name alone.
            assert_eq!(
                catalog
                    .lookup_thread(&units(volume_name))
                    .expect("thread by name"),
                Some(ROOT_FOLDER_ID),
                "{name}"
            );
        });
    }
}

#[test]
fn a_fresh_volume_contains_no_user_files() {
    // `mkfs.hfsplus` creates no user files, and the catalog and extents B-trees
    // are implicit rather than directory entries.
    //
    // A *journaled* volume is the exception and it is a real, verified one: the
    // root holds `.journal` and `.journal_info_block`, which are genuine
    // directory entries with their own CNIDs. Mining reference: Apple
    // core/hfs_vfsutils.c creates the journal vnodes during mount and
    // core/hfs_journal.c places their catalog records, so they have real CNIDs
    // and real thread records.
    for (name, _, _) in corpus() {
        with_catalog(name, |vh, catalog| {
            let children = catalog.read_dir(ROOT_FOLDER_ID).expect("read_dir root");
            let names: Vec<String> = children.iter().map(|c| c.name_string()).collect();

            if vh.is_journaled() {
                assert!(
                    names.contains(&".journal".to_string()),
                    "{name}: a journaled volume must contain .journal, saw {names:?}"
                );
                assert!(
                    names.contains(&".journal_info_block".to_string()),
                    "{name}: a journaled volume must contain .journal_info_block, saw {names:?}"
                );
                // Exactly those two, and nothing a user created.
                assert_eq!(names.len(), 2, "{name}: unexpected root contents {names:?}");
            } else {
                assert!(
                    names.is_empty(),
                    "{name}: a non-journaled fresh root should be empty, saw {names:?}"
                );
            }

            // Every child must be findable by CNID as well as by listing.
            for child in &children {
                assert!(
                    catalog
                        .lookup_child(ROOT_FOLDER_ID, child.cnid)
                        .unwrap()
                        .is_some(),
                    "{name}: listed child {} must also be findable by CNID",
                    child.cnid
                );
            }

            // No main record is keyed by an empty name under parent 1.
            assert!(
                catalog.lookup(ROOT_PARENT_ID, &[]).unwrap().is_none(),
                "{name}: there is no record keyed by an empty name under parent 1"
            );
        });
    }
}

#[test]
fn lookups_report_absence_cleanly() {
    for (name, _, _) in corpus() {
        with_catalog(name, |_vh, catalog| {
            // An unknown CNID must be None, not the first child visited.
            assert!(
                catalog
                    .lookup_child(ROOT_FOLDER_ID, Cnid(999_999))
                    .unwrap()
                    .is_none(),
                "{name}: an unknown CNID must be None"
            );
            assert!(catalog
                .lookup(ROOT_FOLDER_ID, &units("nope"))
                .unwrap()
                .is_none());
            assert!(catalog.lookup_thread(&units("nope")).unwrap().is_none());
            assert!(
                catalog.read_dir(Cnid(4_294_967_000)).unwrap().is_empty(),
                "{name}"
            );
        });
    }
}

#[test]
fn case_sensitivity_manifests_in_lookup_behaviour() {
    // On a folding volume, asking for a differently-cased name **does** find the
    // record. That is the entire point of a case-insensitive filesystem, and
    // getting it backwards would make half of every HFS+ volume unreachable.
    with_catalog("basic-hfsplus", |_vh, catalog| {
        assert_eq!(
            catalog
                .comparator()
                .compare(&units("BasicVolume"), &units("basicvolume")),
            Ordering::Equal,
            "a folding volume treats these as the same name"
        );

        let exact = catalog
            .lookup(ROOT_PARENT_ID, &units("BasicVolume"))
            .unwrap();
        let folded = catalog
            .lookup(ROOT_PARENT_ID, &units("basicvolume"))
            .unwrap();
        assert!(exact.is_some(), "the stored spelling must resolve");
        assert_eq!(
            exact.map(|r| r.cnid()),
            folded.map(|r| r.cnid()),
            "both spellings must find the same record"
        );

        // The stored name is still the one reported back, not the one asked for:
        // HFS+ stores one spelling and the comparator merely makes lookups lenient.
        let key = CatalogKey::for_child(ROOT_PARENT_ID, &units("BasicVolume"));
        assert_eq!(key.name_string(), "BasicVolume");
    });

    // On a case-sensitive volume the same query must find nothing.
    with_catalog("hfsx-case-sensitive", |_vh, catalog| {
        assert!(
            catalog
                .comparator()
                .compare(&units("CaseSensitive"), &units("casesensitive"))
                != Ordering::Equal,
            "an HFSX volume with kHFSBinaryCompare must not fold case"
        );
        assert!(
            catalog
                .lookup(ROOT_PARENT_ID, &units("CaseSensitive"))
                .unwrap()
                .is_some(),
            "the stored spelling must resolve on a case-sensitive volume"
        );
        assert!(
            catalog
                .lookup(ROOT_PARENT_ID, &units("casesensitive"))
                .unwrap()
                .is_none(),
            "the other spelling must not resolve on a case-sensitive volume"
        );
    });
}

#[test]
fn the_whole_volume_enumerates_to_one_entry_per_object() {
    // Every object has exactly one thread record, and its CNID is the thread
    // record's key parentID.
    for (name, volume_name, _) in corpus() {
        with_catalog(name, |_vh, catalog| {
            let all = catalog.all_objects().expect("object scan");
            assert!(
                !all.is_empty(),
                "{name}: the scan must find the root at least"
            );

            let mut seen = std::collections::BTreeSet::new();
            for entry in &all {
                assert!(
                    seen.insert(entry.cnid),
                    "{name}: CNID {} appeared twice",
                    entry.cnid
                );
            }

            let root = all
                .iter()
                .find(|e| e.cnid == ROOT_FOLDER_ID)
                .unwrap_or_else(|| panic!("{name}: root folder missing from the scan"));
            assert_eq!(
                String::from_utf16_lossy(&root.name),
                volume_name,
                "{name}: the scan must recover the volume name"
            );
            assert!(root.is_dir, "{name}: root must be flagged as a directory");

            // Every object in the scan must also be reachable by name, which
            // cross-checks the thread scan against the main records.
            for entry in &all {
                let found = catalog.lookup_thread(&entry.name).expect("lookup by name");
                // A name may legitimately be ambiguous across directories, so the
                // check is that *some* object carries it, not that this one does.
                assert!(found.is_some(), "{name}: {} vanished by name", entry.cnid);
            }
        });
    }
}

#[test]
fn main_records_carry_the_cnid_and_thread_records_do_not() {
    // This is the layout fact the whole module is built around, so it is asserted
    // directly rather than left implicit in the other tests.
    for (name, volume_name, _) in corpus() {
        with_catalog(name, |_vh, catalog| {
            let main = catalog
                .lookup(ROOT_PARENT_ID, &units(volume_name))
                .unwrap()
                .expect("main record");
            assert_eq!(
                main.cnid(),
                Some(ROOT_FOLDER_ID),
                "{name}: a folder record carries its CNID in the body"
            );

            let thread = catalog
                .lookup(ROOT_FOLDER_ID, &[])
                .unwrap()
                .expect("thread record");
            assert!(thread.is_thread(), "{name}: expected a thread record");
            assert_eq!(
                thread.cnid(),
                None,
                "{name}: a thread record's body must not be read as a CNID"
            );
        });
    }
}

#[test]
fn record_sizes_are_the_ones_the_format_says() {
    assert_eq!(FOLDER_RECORD_SIZE, 88);
    assert_eq!(FILE_RECORD_SIZE, 248);

    // And a real record decodes as the variant the format says it is.
    for (name, volume_name, _) in corpus() {
        with_catalog(name, |_vh, catalog| {
            let record = catalog
                .lookup(ROOT_PARENT_ID, &units(volume_name))
                .unwrap()
                .expect("folder record");
            match record {
                CatalogRecord::Folder(f) => assert_eq!(f.folder_id, ROOT_FOLDER_ID, "{name}"),
                other => panic!("{name}: expected a folder, got {other:?}"),
            }
        });
    }
}

#[test]
fn catalog_keys_round_trip_through_the_on_disk_encoding() {
    // The encoder and decoder must agree on the even-byte padding, because a
    // disagreement desynchronises every record after the first in a node.
    for name in ["", "a", "ab", "abc", "abcd", "abcde", "café", "日本"] {
        let key = CatalogKey::for_child(ROOT_FOLDER_ID, &units(name));
        let encoded = key.to_record();
        let decoded = CatalogKey::from_record(&encoded, 516).expect("decode");
        assert_eq!(decoded, key, "name {name:?}");
        assert_eq!(
            encoded.len() % 2,
            0,
            "name {name:?}: encoded length must be even"
        );
    }
}

#[test]
fn lookups_and_scans_are_repeatable() {
    // A caching bug would show up here: the same query must give the same answer
    // regardless of what ran before it.
    with_catalog("basic-hfsplus", |_vh, catalog| {
        let first = catalog.lookup(ROOT_PARENT_ID, &units("BasicVolume"));
        let _ = catalog.all_objects().unwrap();
        let second = catalog.lookup(ROOT_PARENT_ID, &units("BasicVolume"));
        assert_eq!(first.unwrap().is_some(), second.unwrap().is_some());

        let dir1 = catalog.read_dir(ROOT_FOLDER_ID).unwrap();
        let _ = catalog.lookup_thread(&units("BasicVolume")).unwrap();
        let dir2 = catalog.read_dir(ROOT_FOLDER_ID).unwrap();
        assert_eq!(dir1, dir2, "read_dir must be repeatable");
    });
}
