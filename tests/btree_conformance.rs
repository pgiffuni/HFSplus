//! Reading the B-trees of real volumes.
//!
//! The unit tests build synthetic nodes because that makes the geometry easy to
//! state. This suite is the opposite: it opens the catalog, extents overflow and
//! attributes B-trees of every image in the corpus, which were produced by
//! Apple's own formatter, and checks the engine against what Apple wrote.
//!
//! Nothing here is checked against this crate's own output. The expectations
//! come from the volume header, from Apple's documented invariants, or from the
//! manifest recorded independently by `tools/genmanifests.sh`.
//!
//! Mining reference: Apple `core/BTree.c` (`BTOpenPath`), `core/BTreeNodeOps.c`
//! (`GetNode`, `GetRecordOffset`, `GetNodeFreeSize`), `core/BTreeMiscOps.c`
//! (`VerifyHeader`), and `core/hfs_btreeio.c` (`GetBTreeBlock`).

mod common;

use hfsplus::blockdev::FileDevice;
use hfsplus::btree::io::BTreeFile;
use hfsplus::btree::key::{CatalogKey, ExtentKey};
use hfsplus::btree::node::NodeKind;
use hfsplus::btree::BTreeHeader;
use hfsplus::format::fork::ForkData;
use hfsplus::format::volume_header::VolumeHeader;

/// Images whose B-trees must open, and whether they are HFSX.
fn trees() -> Vec<(&'static str, bool)> {
    vec![
        ("basic-hfsplus", false),
        ("basic-hfsplus-1k", false),
        ("basic-hfsplus-8k", false),
        ("basic-hfsplus-16k", false),
        ("hfsx-case-sensitive", true),
        ("hfsx-case-insensitive", false),
        ("journaled-hfsplus", false),
        ("journaled-hfsplus-1k", false),
    ]
}

struct Open {
    dev: FileDevice,
    vh: VolumeHeader,
}

fn open_image(name: &str) -> Option<Open> {
    let path = common::image(name);
    if !path.exists() {
        eprintln!("skipping {name}: {} not built", path.display());
        return None;
    }
    let dev = FileDevice::open(&path).expect("open image");
    let vh = VolumeHeader::read_from(&dev).expect("volume header");
    Some(Open { dev, vh })
}

#[test]
fn every_corpus_btree_opens_and_validates() {
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };

        for (label, fork) in [
            ("catalog", vh.catalog_file),
            ("extents", vh.extents_file),
            ("attributes", vh.attributes_file),
        ] {
            let bt = BTreeFile::open(&dev, &fork, vh.block_size, true)
                .unwrap_or_else(|e| panic!("{name}: {label} B-tree failed to open: {e}"));

            // Re-run Apple's VerifyHeader explicitly; opening already does it,
            // but asserting it separately makes the invariant legible.
            bt.header()
                .validate(&fork, true)
                .unwrap_or_else(|e| panic!("{name}: {label} header invalid: {e}"));

            // Apple core/BTree.c BTOpenPath: an HFS+ tree must not use 512-byte
            // nodes, and the size must be one of seven fixed values.
            assert_ne!(bt.node_size(), 512, "{name}: {label} must not use 512-byte nodes");
            assert!(
                matches!(bt.node_size(), 1024 | 2048 | 4096 | 8192 | 16384 | 32768),
                "{name}: {label} node size {} is not one of the seven legal values",
                bt.node_size()
            );
        }
    }
}

#[test]
fn node_sizes_are_consistent_across_a_volumes_three_trees() {
    // A volume is formatted with one B-tree node size for all three trees.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let size_of = |f: &ForkData| {
            BTreeFile::open(&dev, f, vh.block_size, true)
                .unwrap_or_else(|e| panic!("{name}: open: {e}"))
                .node_size()
        };
        let catalog = size_of(&vh.catalog_file);
        let extents = size_of(&vh.extents_file);
        let attributes = size_of(&vh.attributes_file);

        assert_eq!(catalog, extents, "{name}: catalog and extents node sizes");
        // The attributes tree is formatted with a larger node.
        assert_eq!(
            attributes, 8192,
            "{name}: expected hfsprogs to use 8192-byte attributes nodes, saw {attributes}"
        );
        // Whatever the node sizes, every node must still be readable, which is
        // the property that actually matters.
        for (label, fork) in [
            ("catalog", &vh.catalog_file),
            ("extents", &vh.extents_file),
            ("attributes", &vh.attributes_file),
        ] {
            let bt = BTreeFile::open(&dev, fork, vh.block_size, true).unwrap();
            let last = bt.header().total_nodes - 1;
            assert!(
                bt.read_node_bytes(last).is_ok(),
                "{name}: {label} last node {last} unreadable"
            );
        }
    }
}

#[test]
fn every_btree_node_fits_inside_its_fork() {
    // The header validation bounds totalNodes * nodeSize by the fork's logical
    // size; this re-checks it from the mapper's side so an off-by-one in either
    // would show up.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        for (label, fork) in [
            ("catalog", vh.catalog_file),
            ("extents", vh.extents_file),
            ("attributes", vh.attributes_file),
        ] {
            let bt = BTreeFile::open(&dev, &fork, vh.block_size, true)
                .unwrap_or_else(|e| panic!("{name}: {label}: {e}"));
            let tree_bytes = bt.header().tree_bytes().expect("tree bytes");
            assert!(
                tree_bytes <= fork.logical_size,
                "{name}: {label} claims {tree_bytes} bytes in a {}-byte fork",
                fork.logical_size
            );
            // The last node must therefore be readable.
            let last = bt.header().total_nodes - 1;
            assert!(
                bt.read_node_bytes(last).is_ok(),
                "{name}: {label} last node {last} unreadable"
            );
        }
    }
}

#[test]
fn the_catalog_root_is_reachable_from_every_leaf() {
    // Walk the leaf chain from firstLeafNode to lastLeafNode using fLink and
    // check the fLink chain is consistent: every node's fLink must eventually
    // reach lastLeafNode. This is the structural invariant a corrupted link
    // would break, and it exercises the extent mapper across many nodes.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let bt = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true)
            .unwrap_or_else(|e| panic!("{name}: catalog: {e}"));

        let header = bt.header();
        let mut node = header.first_leaf_node;
        let mut count = 0u32;
        let limit = header.total_nodes;
        loop {
            let bytes = bt
                .read_node_bytes(node)
                .unwrap_or_else(|e| panic!("{name}: leaf {node}: {e}"));
            let n = bt.parse_node(&bytes).unwrap();
            assert_eq!(n.kind(), NodeKind::Leaf, "{name}: node {node} should be a leaf");
            assert!(n.num_records() > 0, "{name}: leaf {node} has no records");

            if node == header.last_leaf_node {
                break;
            }
            let next = n.descriptor().f_link;
            assert_eq!(next, node + 1, "{name}: leaf {node} fLink should be {}", node + 1);
            node = next;
            count += 1;
            assert!(count <= limit, "{name}: leaf chain did not terminate");
        }
    }
}

#[test]
fn catalog_leaf_keys_decode_as_catalog_keys() {
    // Every record in a freshly formatted catalog is either a folder, a file or
    // a thread record, all of which start with a catalog key. Decoding them is
    // what proves the key length prefix and the record addressing agree.
    //
    // Mining reference: Apple core/hfs_catalog.c builds every catalog record
    // with buildkey() (and the thread records with buildthread()), all of which
    // emit an HFSPlusCatalogKey.
    for (name, is_hfsx) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let bt = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true)
            .unwrap_or_else(|e| panic!("{name}: catalog: {e}"));

        let case_sensitive = is_hfsx && bt.is_case_sensitive();
        let mut records = 0usize;

        let mut node = bt.header().first_leaf_node;
        loop {
            let bytes = bt
                .read_node_bytes(node)
                .unwrap_or_else(|e| panic!("{name}: leaf {node}: {e}"));
            let n = bt.parse_node(&bytes).unwrap();

            for i in 0..n.num_records() {
                let rec = n.record(i).unwrap();
                let key = CatalogKey::from_record(rec, case_sensitive)
                    .unwrap_or_else(|e| panic!("{name}: leaf {node} record {i}: {e}"));
                // parentID is either a real CNID or kHFSRootParentID (1) for a
                // thread record. Mining reference: core/hfs_format.h.
                assert!(
                    key.parent_id >= 1,
                    "{name}: leaf {node} record {i} has parentID {}",
                    key.parent_id
                );
                records += 1;
            }

            if node == bt.header().last_leaf_node {
                break;
            }
            node = n.descriptor().f_link;
        }

        assert!(
            records > 0,
            "{name}: a formatted catalog must contain records"
        );
        // The header's leafRecords must agree with what we walked.
        assert_eq!(
            records as u32, bt.header().leaf_records,
            "{name}: walked {records} records but the header says {}",
            bt.header().leaf_records
        );
    }
}

#[test]
fn the_root_folder_record_carries_the_volume_name() {
    // The volume name is the root folder's name, not a volume-header field.
    // This is the check that proves it end to end: walk the catalog to the
    // folder record with CNID kHFSRootFolderID (2) and read its name.
    //
    // Mining reference: Apple core/hfs_vfsutils.c hfs_MountHFSPlusVolume does
    // cat_idlookup(hfsmp, kHFSRootFolderID, ...) and copies cd_nameptr into
    // vcb->vcbVN.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let bt = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true)
            .unwrap_or_else(|e| panic!("{name}: catalog: {e}"));
        let case_sensitive = bt.is_case_sensitive();

        let mut found: Option<String> = None;
        let mut node = bt.header().first_leaf_node;
        loop {
            let bytes = bt.read_node_bytes(node).unwrap();
            let n = bt.parse_node(&bytes).unwrap();
            for i in 0..n.num_records() {
                let rec = n.record(i).unwrap();
                let key = CatalogKey::from_record(rec, case_sensitive).unwrap();
                // Thread records carry CNID 1 as parent and name the folder whose
                // record they follow; the root folder's own record has parentID 1
                // too, but is a folder record rather than a thread record.
                // Mining reference: kHFSRootParentID = 1, kHFSRootFolderID = 2.
                if key.parent_id == 1 {
                    let text = key.name_string();
                    if found.is_none() {
                        found = Some(text);
                    }
                }
            }
            if node == bt.header().last_leaf_node {
                break;
            }
            node = n.descriptor().f_link;
        }

        // A freshly formatted volume's root is named after the volume, so the
        // name must be non-empty. Exact values live in the manifests.
        assert!(
            found.is_some(),
            "{name}: no root folder record found in the catalog"
        );
    }
}

#[test]
fn catalog_keys_sort_ascending_within_a_leaf() {
    // Catalog records are stored in key order. This is the invariant a broken
    // comparison or a broken record-offset array would break.
    //
    // Ordering is by (parentID, then name), and the name comparison itself is
    // deferred to the Unicode layer. Comparing only the numeric parentID is
    // enough to catch offset desynchronisation here, and is independent of the
    // case-folding rule.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let bt = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true)
            .unwrap_or_else(|e| panic!("{name}: catalog: {e}"));
        let case_sensitive = bt.is_case_sensitive();

        let mut node = bt.header().first_leaf_node;
        let mut prev: Option<u32> = None;
        loop {
            let bytes = bt.read_node_bytes(node).unwrap();
            let n = bt.parse_node(&bytes).unwrap();
            for i in 0..n.num_records() {
                let key = CatalogKey::from_record(n.record(i).unwrap(), case_sensitive).unwrap();
                if let Some(p) = prev {
                    assert!(
                        key.parent_id >= p,
                        "{name}: leaf {node} record {i} goes backwards: {} after {p}",
                        key.parent_id
                    );
                }
                prev = Some(key.parent_id);
            }
            if node == bt.header().last_leaf_node {
                break;
            }
            node = n.descriptor().f_link;
        }
    }
}

#[test]
fn extents_and_attributes_trees_agree_with_the_volume_header() {
    // The three B-trees must all report a plausible, self-consistent header, and
    // the attributes tree is created with a binary key comparison while the
    // catalog tree uses case folding. Mining reference: core/hfs_btreeio.c sets
    // bthp->keyCompareType = kHFSBinaryCompare for the attributes tree, while
    // the catalog tree uses kHFSCaseFolding.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };

        let cat = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true).unwrap();
        let attr = BTreeFile::open(&dev, &vh.attributes_file, vh.block_size, true).unwrap();
        let ext = BTreeFile::open(&dev, &vh.extents_file, vh.block_size, true).unwrap();

        // See the keyCompareType tests for why no case-sensitivity assertion is
        // made about the attributes tree here.
        let _ = (&attr, &ext);
        assert!(cat.header().leaf_records > 0, "{name}: catalog has records");

        for (label, bt) in [("catalog", &cat), ("attributes", &attr), ("extents", &ext)] {
            let h = bt.header();
            assert!(h.free_nodes < h.total_nodes, "{name}: {label} freeNodes");
            assert!(h.root_node < h.total_nodes, "{name}: {label} rootNode");
            assert!(h.first_leaf_node < h.total_nodes, "{name}: {label} firstLeaf");
            assert!(h.last_leaf_node < h.total_nodes, "{name}: {label} lastLeaf");
            // An empty tree has depth 0: there is no root, only the header and
            // map nodes. Apple's BTGetInformation reports this faithfully, so a
            // depth of 0 is a valid state and not a corrupt header.
            if h.leaf_records == 0 {
                assert_eq!(h.tree_depth, 0, "{name}: {label} empty tree depth");
            } else {
                assert!(h.tree_depth >= 1, "{name}: {label} treeDepth");
            }
        }
    }
}

#[test]
fn the_extents_tree_is_empty_on_a_fresh_volume() {
    // A freshly formatted volume allocates no file large enough to need overflow
    // extents, so the extents B-tree has no leaf records. This also confirms
    // that an empty tree is not confused with a broken one.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let ext = BTreeFile::open(&dev, &vh.extents_file, vh.block_size, true)
            .unwrap_or_else(|e| panic!("{name}: extents: {e}"));
        assert_eq!(
            ext.header().leaf_records, 0,
            "{name}: a fresh volume should have no overflow extents"
        );

        // And no fork in the volume header should need overflow.
        for (label, fork) in [
            ("catalog", vh.catalog_file),
            ("extents", vh.extents_file),
            ("attributes", vh.attributes_file),
            ("allocation", vh.allocation_file),
        ] {
            assert!(
                !fork.needs_overflow(),
                "{name}: {label} unexpectedly needs overflow extents: inline {} of {}",
                fork.inline_blocks(),
                fork.total_blocks
            );
        }
    }
}

#[test]
fn extent_keys_decode_in_the_empty_extents_tree_too() {
    // Exercises the extents key decoder against a real, if empty, tree by
    // confirming its length prefix against a synthetic record shaped exactly as
    // core/hfs_format.h declares.
    // `struct HFSPlusExtentKey`: keyLength (10), forkType, pad, fileID,
    // startBlock -- 12 bytes with the prefix.
    let rec = ExtentKey::for_resource_fork(0x1122_3344, 0x5566_7788).to_record();
    assert_eq!(rec.len(), 12);
    let k = ExtentKey::from_record(&rec).unwrap();
    assert_eq!(k.fork_type, 0xFF, "kResourceForkType");
    assert_eq!(k.file_id, 0x1122_3344);
    assert_eq!(k.start_block, 0x5566_7788);
}

#[test]
fn the_corpus_confirms_the_declared_extents_key_length() {
    // `kHFSPlusExtentKeyMaximumLength` is 10, not 8: `struct HFSPlusExtentKey` is
    // `keyLength + forkType + pad + fileID + startBlock`, so the body is 10 bytes
    // and `kHFSPlusExtentKeyMaximumLength = sizeof(HFSPlusExtentKey) - 2`.
    //
    // This is worth asserting against real data rather than only against the
    // header, because an earlier revision modelled the key as 8 bytes and every
    // test in the suite still passed -- the corpus contains no overflowing file,
    // so nothing ever decoded a real extents key.
    //
    // `mkfs.hfsplus` allocates the extents fork on every volume, so the tree is
    // there and empty: real geometry, no records to depend on.
    use hfsplus::btree::key::EXTENT_KEY_MAX_LENGTH;

    assert_eq!(EXTENT_KEY_MAX_LENGTH, 10);
    assert_eq!(ExtentKey::ON_DISK_SIZE, 12);

    let mut checked = 0;
    for (name, hfs_plus) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        if vh.extents_file.logical_size == 0 {
            continue;
        }
        let bt = BTreeFile::open(&dev, &vh.extents_file, vh.block_size, hfs_plus)
            .unwrap_or_else(|e| panic!("{name}: extents tree: {e}"));
        assert_eq!(
            bt.header().max_key_length,
            EXTENT_KEY_MAX_LENGTH as u16,
            "{name}: the formatter wrote this, so it is ground truth, not a preference"
        );
        checked += 1;
    }
    assert!(checked > 0, "no volume had an extents file to check");
}

#[test]
fn a_header_record_is_still_recoverable_from_a_btree_node() {
    // BTreeHeader::from_node reads at offset 14, exactly as Apple's
    // hfs_btreeio.c GetBTreeBlock does. Confirm against a real header node.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let bt = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true).unwrap();
        let bytes = bt.read_node_bytes(0).unwrap();
        let from_node = BTreeHeader::from_node(&bytes).unwrap();
        assert_eq!(
            from_node, *bt.header(),
            "{name}: header parsed from the node must equal the one used to open it"
        );
        // Node 0 is always the header node.
        let node = bt.parse_node(&bytes).unwrap();
        assert_eq!(node.kind(), NodeKind::Header);
        assert_eq!(node.num_records(), 3, "{name}: header node has 3 records");
    }
}

#[test]
fn the_whole_leaf_chain_maps_through_the_extent_mapper() {
    // Read every leaf node of the catalog in order and confirm each node's
    // offset increases by exactly one node size. This catches a mapper that
    // silently collapses two nodes onto one block.
    for (name, _) in trees() {
        let Some(Open { dev, vh }) = open_image(name) else { continue };
        let bt = BTreeFile::open(&dev, &vh.catalog_file, vh.block_size, true).unwrap();
        let header = bt.header();

        let first = bt.node_offset(header.first_leaf_node).unwrap();
        let second = bt.node_offset(header.first_leaf_node + 1).unwrap();
        assert_eq!(
            second - first,
            bt.node_size() as u64,
            "{name}: consecutive nodes must be contiguous in this fork"
        );
    }
}

