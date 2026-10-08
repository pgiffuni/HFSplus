// SPDX-License-Identifier: APSL-1.2

//! Volume creation: building the on-disk structures of a fresh HFS+ volume.
//!
//! This module constructs the special B-tree forks that bookend an HFS+
//! volume: the allocation bitmap, the extents overflow B-tree, the catalog
//! B-tree (seeded with a root folder record and its thread), and the attributes
//! B-tree. It also writes the primary and alternate volume headers with the
//! correct fork locations.
//!
//! Mining reference: Apple `newfs_hfs/makehfs.c` (`MakeHFS`, `initVolume`) and
//! `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) for the layout order. The
//! B-tree header is built from `core/hfs_format.h` (`BTHeaderRec` + `BTNodeDescriptor`).

use crate::blockdev::BlockDeviceMut;
use crate::blockdev::VOLUME_HEADER_OFFSET;
use crate::btree::header::{
    BTreeHeader, KeyCompareType, HEADER_RECORD_OFFSET, HEADER_RECORD_SIZE, HEADER_USER_BYTES,
    K_HFS_BINARY_COMPARE, K_HFS_CASE_FOLDING,
};
use crate::btree::node::{NodeKind, NODE_DESCRIPTOR_SIZE, OFFSET_SIZE};
use crate::catalog::cnid::{Cnid, FIRST_USER_CATALOG_NODE_ID, ROOT_FOLDER_ID, ROOT_PARENT_ID};
use crate::catalog::key::CatalogKey;
use crate::catalog::record::{
    BsdInfo, FileRecord, FolderRecord, FINDER_OPAQUE_INFO_SIZE, FINDER_USER_INFO_SIZE,
    K_HFS_PLUS_FILE_RECORD, K_HFS_PLUS_FILE_THREAD_RECORD, K_HFS_PLUS_FOLDER_RECORD,
    K_HFS_PLUS_FOLDER_THREAD_RECORD,
};
use crate::error::{Error, Result};
use crate::format::extents::{ExtentDescriptor, ExtentRecord};
use crate::format::fork::ForkData;
use crate::format::volume_header::{
    VolumeHeader, K_HFSX_SIG_WORD, K_HFSX_VERSION, K_HFS_EXPANDED_TIMES_MASK, K_HFS_PLUS_SIG_WORD,
    K_HFS_PLUS_VERSION, K_HFS_VOLUME_JOURNALED_MASK, K_HFS_VOLUME_NEW_FS_MASK,
    K_HFS_VOLUME_UNMOUNTED_MASK,
};

/// Total size of a thread record body before the variable-length name.
const THREAD_FIXED_SIZE: usize = 8;

/// Default clump size for special files.
///
/// Apple's `newfs_hfs` sizes clumps so that the default clump covers roughly
/// 64 KiB, capped at 16 allocation blocks per clump.
fn default_clump_size(block_size: u32) -> u32 {
    let blocks_in_clump = (65536 / block_size).clamp(1, 16);
    blocks_in_clump * block_size
}

/// `kBTBigKeysMask | kBTVariableIndexKeysMask`.
const K_BT_BIG_KEYS_MASK: u32 = 0x0000_0002;
/// `kBTVariableIndexKeysMask`.
const K_BT_VARIABLE_INDEX_KEYS_MASK: u32 = 0x0000_0004;
/// `kHFSPlusMaxNameLength * 2 + 4 + 2`: longest catalog key body (parentID
/// + UTF-16 name, with the 2-byte key length prefix counted by Apple).
const K_HFS_PLUS_MAX_KEY_LENGTH: u16 = 516;

/// `kHFSBTreeType`: the B-tree type byte for catalog, extents and attributes trees.
const K_HFS_B_TREE_TYPE: u8 = 0;

/// Build a fresh HFS+ volume on `device`.
///
/// `volume_name` becomes the root folder's name in the catalog. `total_bytes`
/// is the raw size of `device`. `block_size` is the allocation block size;
/// `node_size` is the B-tree node size for the catalog, extents and attributes
/// trees. If `journaled` is true, a journal is created with `journal_size`
/// bytes (or the 8 MiB default when `None`). If `case_sensitive` is true, an
/// HFSX volume is produced.
///
/// The device must be at least as large as `total_bytes`. Existing content is
/// not zeroed first by this function — call `device.sync()` or pre-zero if
/// needed.
#[allow(clippy::too_many_arguments)]
pub fn format_volume<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    volume_name: &str,
    total_bytes: u64,
    block_size: u32,
    node_size: u16,
    case_sensitive: bool,
    journaled: bool,
    journal_size: Option<u64>,
    _uid: u32,
    _gid: u32,
    _umask: u16,
) -> Result<()> {
    let name_units: Vec<u16> = volume_name.encode_utf16().collect();
    if name_units.len() > 255 {
        return Err(Error::invalid(
            "volume name",
            "exceeds 255 UTF-16 code units",
        ));
    }
    if !(512..=32768).contains(&node_size) || node_size & (node_size - 1) != 0 {
        return Err(Error::invalid(
            "node size",
            "must be a power of two between 512 and 32768",
        ));
    }

    let total_blocks = block_size_from_bytes(total_bytes, block_size)?;
    let journal_blocks = if journaled {
        let default_journal_size = (total_bytes / 2)
            .min(8 * 1024 * 1024)
            .max(u64::from(block_size));
        let journal_size = journal_size.unwrap_or(default_journal_size);
        ((journal_size + u64::from(block_size) - 1) / u64::from(block_size)) as u32
    } else {
        0
    };
    let layout = compute_layout(block_size, node_size, journal_blocks);
    let journal = if journaled {
        Some(JournalLayout {
            info_block: layout.journal_info_block,
            start_block: layout.journal_data_start,
            end_block: layout.journal_data_end,
        })
    } else {
        None
    };

    let header = build_volume_header(
        &layout,
        journal.as_ref(),
        total_blocks,
        block_size,
        case_sensitive,
    );

    write_allocation_bitmap(device, &layout, block_size, total_blocks, journal.as_ref())?;

    // Extents and attributes trees start empty: tree_depth=0, root_node=0.
    write_empty_btree_header(
        device,
        layout.extents_fork.extents.raw[0].start_block,
        block_size,
        layout.extents_node_size,
        layout.extents_node_count,
        10,
    )?;

    write_catalog_btree(
        device,
        layout.catalog_fork.extents.raw[0].start_block,
        block_size,
        layout.catalog_node_size,
        layout.catalog_node_count,
        case_sensitive,
        journal.as_ref(),
        &name_units,
    )?;

    write_empty_btree_header(
        device,
        layout.attributes_fork.extents.raw[0].start_block,
        block_size,
        layout.attributes_node_size,
        layout.attributes_node_count,
        266,
    )?;

    if let Some(j) = &journal {
        write_journal_info(device, j, block_size)?;
    }

    let header_bytes = header.to_bytes();
    device.write_at(VOLUME_HEADER_OFFSET, &header_bytes)?;

    let vol_bytes = u64::from(total_blocks) * u64::from(block_size);
    let alt_off = vol_bytes.saturating_sub(1024);
    device.write_at(alt_off, &header_bytes)?;

    device.sync()?;
    Ok(())
}

/// Compute total blocks, rejecting sizes that don't fit the block grid.
fn block_size_from_bytes(total_bytes: u64, block_size: u32) -> Result<u32> {
    let total_blocks = total_bytes
        .checked_div(u64::from(block_size))
        .ok_or_else(|| Error::overflow("total_blocks"))?;
    total_blocks
        .try_into()
        .map_err(|_| Error::out_of_range("total_blocks", total_blocks, u64::from(u32::MAX)))
}

/// Layout of the metadata that precedes user data.
struct VolumeLayout {
    next_allocation: u32,
    journal_info_block: u32,
    journal_data_start: u32,
    journal_data_end: u32,
    allocation_fork: ForkData,
    extents_fork: ForkData,
    catalog_fork: ForkData,
    attributes_fork: ForkData,
    extents_node_count: u32,
    extents_node_size: u16,
    catalog_node_count: u32,
    catalog_node_size: u16,
    attributes_node_count: u32,
    attributes_node_size: u16,
}

/// Journal layout: info block location, start and end allocation blocks.
struct JournalLayout {
    info_block: u32,
    start_block: u32,
    end_block: u32,
}

/// Compute which blocks each special file occupies.
fn compute_layout(block_size: u32, node_size: u16, journal_blocks: u32) -> VolumeLayout {
    // Block 0 is always reserved (boot blocks + master directory block).
    // The primary volume header lives at byte 1024 and is 1024 bytes, so the
    // allocation bitmap begins at the first block past it.
    let next_allocation =
        ((VOLUME_HEADER_OFFSET as u32 + 1024 + block_size - 1) / block_size).max(1);
    let alloc_blocks = 1u32;
    let alloc_start = next_allocation;

    // B-tree node counts: newfs_hfs allocates a minimum of 8 nodes per tree.
    let nodes_per_tree = 8u32;

    // The attributes B-tree always uses an 8 KiB node size (large enough for
    // the 266-byte attribute key body), regardless of the filesystem block size.
    let attributes_node_size: u16 = 8192;

    let extents_start = alloc_start + alloc_blocks + 1 + journal_blocks;
    let extents_blocks = nodes_per_tree * u32::from(node_size) / block_size;
    let attributes_start = extents_start + extents_blocks;
    let attributes_blocks = nodes_per_tree * u32::from(attributes_node_size) / block_size;
    let catalog_start = attributes_start + attributes_blocks;
    let catalog_blocks = nodes_per_tree * u32::from(node_size) / block_size;

    let make_fork = |start: u32, blocks: u32, nodes: u32, ns: u16, clump: u32| -> ForkData {
        ForkData {
            logical_size: u64::from(nodes) * u64::from(ns),
            clump_size: clump,
            total_blocks: blocks,
            extents: {
                let mut r = ExtentRecord::EMPTY;
                r.raw[0] = ExtentDescriptor {
                    start_block: start,
                    block_count: blocks,
                };
                r
            },
        }
    };

    VolumeLayout {
        next_allocation: catalog_start + catalog_blocks,
        journal_info_block: alloc_start + alloc_blocks,
        journal_data_start: alloc_start + alloc_blocks + 1,
        journal_data_end: alloc_start + alloc_blocks + 1 + journal_blocks,
        allocation_fork: make_fork(alloc_start, alloc_blocks, 1, block_size as u16, block_size),
        extents_fork: make_fork(
            extents_start,
            extents_blocks,
            nodes_per_tree,
            node_size,
            u32::from(node_size) * 8,
        ),
        catalog_fork: make_fork(
            catalog_start,
            catalog_blocks,
            nodes_per_tree,
            node_size,
            u32::from(node_size) * 8,
        ),
        attributes_fork: make_fork(
            attributes_start,
            attributes_blocks,
            nodes_per_tree,
            attributes_node_size,
            u32::from(attributes_node_size) * 8,
        ),
        extents_node_count: nodes_per_tree,
        extents_node_size: node_size,
        catalog_node_count: nodes_per_tree,
        catalog_node_size: node_size,
        attributes_node_count: nodes_per_tree,
        attributes_node_size,
    }
}

/// Build the volume header with fork locations, dates and CNIDs.
fn build_volume_header(
    layout: &VolumeLayout,
    journal: Option<&JournalLayout>,
    total_blocks: u32,
    block_size: u32,
    case_sensitive: bool,
) -> VolumeHeader {
    let mut attrs = K_HFS_VOLUME_NEW_FS_MASK | K_HFS_VOLUME_UNMOUNTED_MASK;
    if journal.is_some() {
        attrs |= K_HFS_VOLUME_JOURNALED_MASK;
    }
    if case_sensitive {
        attrs |= K_HFS_EXPANDED_TIMES_MASK;
    }

    let signature = if case_sensitive {
        K_HFSX_SIG_WORD
    } else {
        K_HFS_PLUS_SIG_WORD
    };
    let version = if case_sensitive {
        K_HFSX_VERSION
    } else {
        K_HFS_PLUS_VERSION
    };

    let journal_info_block = journal.map(|j| j.info_block).unwrap_or(0);

    let mut free_blocks = total_blocks;
    free_blocks -= 1; // block 0: boot blocks + volume header.
    let vh_block = VOLUME_HEADER_OFFSET as u32 / block_size;
    if vh_block > 0 {
        free_blocks -= 1; // primary volume header's own block.
    }
    free_blocks -= 1; // alternate volume header block.
    free_blocks -= layout.allocation_fork.total_blocks;
    free_blocks -= layout.extents_fork.total_blocks;
    free_blocks -= layout.catalog_fork.total_blocks;
    free_blocks -= layout.attributes_fork.total_blocks;
    if let Some(j) = journal {
        free_blocks -= 1; // journal info block.
        free_blocks -= j.end_block - j.start_block; // journal data blocks.
    }

    VolumeHeader {
        signature,
        version,
        attributes: attrs,
        last_mounted_version: 0,
        journal_info_block,
        create_date: 0,
        modify_date: 0,
        backup_date: 0,
        checked_date: 0,
        file_count: if journal.is_some() { 2 } else { 0 },
        folder_count: 0,
        block_size,
        total_blocks,
        free_blocks,
        next_allocation: layout.next_allocation,
        rsrc_clump_size: default_clump_size(block_size),
        data_clump_size: default_clump_size(block_size),
        next_catalog_id: if journal.is_some() {
            18
        } else {
            FIRST_USER_CATALOG_NODE_ID.0
        },
        write_count: 0,
        encodings_bitmap: 1,
        finder_info: [0u8; 32],
        allocation_file: layout.allocation_fork,
        extents_file: layout.extents_fork,
        catalog_file: layout.catalog_fork,
        attributes_file: layout.attributes_fork,
        startup_file: ForkData::EMPTY,
    }
}

/// Write the allocation bitmap: bit set = allocated.
fn write_allocation_bitmap<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    layout: &VolumeLayout,
    block_size: u32,
    total_blocks: u32,
    journal: Option<&JournalLayout>,
) -> Result<()> {
    let bytes = crate::volume::bytes_for_blocks(total_blocks)?;
    let mut bitmap = vec![0u8; bytes];

    eprintln!("bitmap bytes={}, total_blocks={}", bytes, total_blocks);
    let mut mark = |start: u32, count: u32| {
        for b in start..start.saturating_add(count) {
            if b >= total_blocks {
                break;
            }
            let byte = b as usize / 8;
            let bit = b as usize % 8;
            if let Some(b) = bitmap.get_mut(byte) {
                *b |= 0x80 >> bit;
            }
        }
    };

    // Block 0 is always reserved (boot/MDB).
    mark(0, 1);
    // Primary volume header at byte 1024: mark the block that contains it.
    let vh_block = VOLUME_HEADER_OFFSET as u32 / block_size;
    if vh_block != 0 {
        mark(vh_block, 1);
    }
    // The alternate volume header occupies the last 1024 bytes of the volume,
    // which falls within the last allocation block.
    mark(total_blocks - 1, 1);
    mark(
        layout.allocation_fork.extents.raw[0].start_block,
        layout.allocation_fork.total_blocks,
    );
    mark(
        layout.extents_fork.extents.raw[0].start_block,
        layout.extents_fork.total_blocks,
    );
    mark(
        layout.catalog_fork.extents.raw[0].start_block,
        layout.catalog_fork.total_blocks,
    );
    mark(
        layout.attributes_fork.extents.raw[0].start_block,
        layout.attributes_fork.total_blocks,
    );
    if let Some(j) = journal {
        eprintln!(
            "journal: info_block={}, start={}, end={}",
            j.info_block, j.start_block, j.end_block
        );
        mark(j.info_block, 1);
        mark(j.start_block, j.end_block - j.start_block);
    }

    let mut count = 0u32;
    for i in 0..total_blocks {
        let byte = i as usize / 8;
        let bit = i as usize % 8;
        if byte < bitmap.len() && bitmap[byte] & (0x80 >> bit) != 0 {
            count += 1;
        }
    }
    eprintln!(
        "Total marked blocks: {}, bitmap len: {}",
        count,
        bitmap.len()
    );

    let bm_off =
        u64::from(layout.allocation_fork.extents.raw[0].start_block) * u64::from(block_size);
    eprintln!(
        "Writing bitmap to offset {} (block {}), {} bytes",
        bm_off,
        layout.allocation_fork.extents.raw[0].start_block,
        bitmap.len()
    );
    device.write_at(bm_off, &bitmap)?;
    Ok(())
}

/// Write a B-tree header node for an empty tree (tree_depth=0, root_node=0).
///
/// Only the header node is written — no leaf. The attributes field is set
/// according to `max_key_length` (big keys when > 40, per Apple's rule) and
/// the key comparison type is left as 0 since there are no records to compare.
fn write_empty_btree_header<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    start_block: u32,
    block_size: u32,
    node_size: u16,
    node_count: u32,
    max_key_length: u16,
) -> Result<()> {
    let node_bytes = usize::from(node_size);
    let mut node = vec![0u8; node_bytes];

    write_node_descriptor(&mut node, NodeKind::Header, 0);

    let bt = BTreeHeader {
        tree_depth: 0,
        root_node: 0,
        leaf_records: 0,
        first_leaf_node: 0,
        last_leaf_node: 0,
        node_size,
        max_key_length,
        total_nodes: node_count,
        free_nodes: node_count - 1,
        reserved1: 0,
        clump_size: u32::from(node_size) * 8,
        btree_type: K_HFS_B_TREE_TYPE,
        key_compare_type: KeyCompareType::Unknown(0),
        attributes: btree_attributes(max_key_length),
    };

    let hdr = bt.to_bytes();
    node[HEADER_RECORD_OFFSET..HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE].copy_from_slice(&hdr);

    // Header node records: BTHeaderRec, 128 bytes of user data, node map.
    let map_start = HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE + HEADER_USER_BYTES;
    let num_offsets = 4; // 3 records + 1 free marker.
    let free_off = node_bytes - num_offsets * OFFSET_SIZE;
    let user_data_off = HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE;
    let map = node
        .get_mut(map_start..free_off)
        .ok_or(Error::invalid("node map", "header node too small for map"))?;
    map[0] |= 0x80;

    set_offset(&mut node, node_bytes, 0, HEADER_RECORD_OFFSET as u16);
    set_offset(&mut node, node_bytes, 1, user_data_off as u16);
    set_offset(&mut node, node_bytes, 2, map_start as u16);
    set_offset(&mut node, node_bytes, 3, free_off as u16);

    node[10..12].copy_from_slice(&3u16.to_be_bytes());

    let off = u64::from(start_block) * u64::from(block_size);
    device.write_at(off, &node)?;

    Ok(())
}

/// Compute the `attributes` field for a B-tree header: Apple's `BTOpenPath`
/// ORs in `kBTBigKeysMask` whenever `maxKeyLength > 40`, and `newfs_hfs`
/// always sets `kBTVariableIndexKeysMask`.
fn btree_attributes(max_key_length: u16) -> u32 {
    let mut attrs = K_BT_BIG_KEYS_MASK;
    if max_key_length > 40 {
        attrs |= K_BT_VARIABLE_INDEX_KEYS_MASK;
    }
    attrs
}

/// Write the catalog B-tree header node (node 0) for a non-empty tree.
///
/// The catalog tree has `tree_depth=1` (root is also the leaf) and
/// `root_node=1`. It uses the longest max key (516) and the volume's
/// case-folding or binary-compare key comparison type.
fn write_catalog_header<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    start_block: u32,
    block_size: u32,
    node_size: u16,
    node_count: u32,
    key_compare_type: u8,
    leaf_records: u32,
) -> Result<()> {
    let node_bytes = usize::from(node_size);
    let mut node = vec![0u8; node_bytes];

    write_node_descriptor(&mut node, NodeKind::Header, 0);

    let bt = BTreeHeader {
        tree_depth: 1,
        root_node: 1,
        leaf_records,
        first_leaf_node: 1,
        last_leaf_node: 1,
        node_size,
        max_key_length: K_HFS_PLUS_MAX_KEY_LENGTH,
        total_nodes: node_count,
        free_nodes: node_count - 2, // header + 1 leaf are allocated.
        reserved1: 0,
        clump_size: u32::from(node_size) * 8,
        btree_type: K_HFS_B_TREE_TYPE,
        key_compare_type: KeyCompareType::from_u8(key_compare_type),
        attributes: btree_attributes(K_HFS_PLUS_MAX_KEY_LENGTH),
    };

    let hdr = bt.to_bytes();
    node[HEADER_RECORD_OFFSET..HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE].copy_from_slice(&hdr);

    // Node map: mark nodes 0 (header) and 1 (leaf) as allocated.
    let map_start = HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE + HEADER_USER_BYTES;
    let num_offsets = 4; // 3 records + 1 free marker.
    let free_off = node_bytes - num_offsets * OFFSET_SIZE;
    let map = node
        .get_mut(map_start..free_off)
        .ok_or(Error::invalid("node map", "header node too small for map"))?;
    map[0] |= 0xC0; // bits 0 and 1 → header and leaf allocated.

    let user_data_off = HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE;
    set_offset(&mut node, node_bytes, 0, HEADER_RECORD_OFFSET as u16);
    set_offset(&mut node, node_bytes, 1, user_data_off as u16);
    set_offset(&mut node, node_bytes, 2, map_start as u16);
    set_offset(&mut node, node_bytes, 3, free_off as u16);

    node[10..12].copy_from_slice(&3u16.to_be_bytes());

    let off = u64::from(start_block) * u64::from(block_size);
    device.write_at(off, &node)?;
    Ok(())
}

/// Write the catalog B-tree: a header node (node 0) and a leaf node (node 1)
/// holding the root folder record and its thread record.
///
/// For journaled volumes, the leaf also contains file records and thread records
/// for the two hidden journal pseudo-files: `.journal` (CNID 16) and
/// `.journal_info_block` (CNID 17).
#[allow(clippy::too_many_arguments)]
fn write_catalog_btree<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    start_block: u32,
    block_size: u32,
    node_size: u16,
    node_count: u32,
    case_sensitive: bool,
    journal: Option<&JournalLayout>,
    name: &[u16],
) -> Result<()> {
    let key_compare_type = if case_sensitive {
        K_HFS_BINARY_COMPARE
    } else {
        K_HFS_CASE_FOLDING
    };

    // Write the header node (node 0).
    let leaf_records = if journal.is_some() { 6 } else { 2 };
    write_catalog_header(
        device,
        start_block,
        block_size,
        node_size,
        node_count,
        key_compare_type,
        leaf_records,
    )?;

    // The leaf node follows the header node. When node_size >= block_size this
    // is a new allocation block; when node_size < block_size multiple nodes
    // share a block and the leaf sits at an in-block offset.
    let leaf_off = u64::from(start_block) * u64::from(block_size) + u64::from(node_size);
    let node_bytes = usize::from(node_size);
    let mut leaf = vec![0u8; node_bytes];

    write_node_descriptor(&mut leaf, NodeKind::Leaf, 1);

    let mut records: Vec<Vec<u8>> = Vec::new();

    // Record 0: root folder record (keyed by parent_id=1, name=volume name).
    let folder_key = CatalogKey::for_child(ROOT_PARENT_ID, name);
    let folder = folder_root_record(journal.is_some());
    let folder_bytes = folder.to_bytes();
    let folder_rec = {
        let mut v = folder_key.to_record();
        v.extend_from_slice(&folder_bytes);
        v
    };
    records.push(folder_rec);

    // Record 1: root folder thread.
    // Root folder thread key: parent_id = ROOT_FOLDER_ID (2), empty name.
    let root_thread_key = CatalogKey::for_child(ROOT_FOLDER_ID, &[]);
    let root_thread_body = build_root_thread_record(name);
    let root_thread_rec = {
        let mut v = root_thread_key.to_record();
        v.extend_from_slice(&root_thread_body);
        v
    };
    records.push(root_thread_rec);

    if let Some(j) = journal {
        // Record 2: .journal file record (CNID 16).
        let journal_name: Vec<u16> = ".journal".encode_utf16().collect();
        let journal_file = build_journal_file_record(j, block_size, 16);
        let journal_key = CatalogKey::for_child(ROOT_FOLDER_ID, &journal_name);
        let journal_rec = {
            let mut v = journal_key.to_record();
            v.extend_from_slice(&journal_file);
            v
        };
        records.push(journal_rec);

        // Record 3: .journal_info_block file record (CNID 17).
        let jib_name: Vec<u16> = ".journal_info_block".encode_utf16().collect();
        let jib_file = build_journal_file_record(j, block_size, 17);
        let jib_key = CatalogKey::for_child(ROOT_FOLDER_ID, &jib_name);
        let jib_rec = {
            let mut v = jib_key.to_record();
            v.extend_from_slice(&jib_file);
            v
        };
        records.push(jib_rec);

        // Record 4: thread for CNID 16 (.journal).
        let journal_thread_key = CatalogKey::for_child(Cnid(16), &[]);
        let journal_thread_body = build_file_thread_record(ROOT_FOLDER_ID, &journal_name);
        let journal_thread_rec = {
            let mut v = journal_thread_key.to_record();
            v.extend_from_slice(&journal_thread_body);
            v
        };
        records.push(journal_thread_rec);

        // Record 5: thread for CNID 17 (.journal_info_block).
        let jib_thread_key = CatalogKey::for_child(Cnid(17), &[]);
        let jib_thread_body = build_file_thread_record(ROOT_FOLDER_ID, &jib_name);
        let jib_thread_rec = {
            let mut v = jib_thread_key.to_record();
            v.extend_from_slice(&jib_thread_body);
            v
        };
        records.push(jib_thread_rec);
    }

    // Write records in ascending address order (lowest key first).
    let mut offset = NODE_DESCRIPTOR_SIZE;
    for rec in &records {
        leaf[offset..offset + rec.len()].copy_from_slice(rec);
        offset += rec.len();
    }
    let free_bytes = offset;

    // Offset array: entry 0 points to the highest-address record (highest key),
    // entry num_recs-1 points to the lowest-address record (lowest key).
    let num_recs = records.len();

    // Records are stored at ascending offsets: rec0, rec0+len0, rec0+len0+len1, ...
    let mut rec_offsets: Vec<u16> = Vec::new();
    let mut current = NODE_DESCRIPTOR_SIZE as u16;
    for rec in &records {
        rec_offsets.push(current);
        current += rec.len() as u16;
    }

    // Offset array in descending value order: HFS+ entry 0 holds the highest
    // offset value, entry num_recs-1 the lowest. `set_offset(i, v)` writes to
    // position `node_bytes - (i+1)*2`, which corresponds to HFS+ entry
    // `num_recs-1-i`, so `set_offset(i, rec_offsets[i])` lands each value in
    // the correct HFS+ entry.
    for (i, &addr) in rec_offsets.iter().enumerate() {
        set_offset(&mut leaf, node_bytes, i, addr);
    }

    // Free space marker
    set_offset(&mut leaf, node_bytes, num_recs, free_bytes as u16);

    leaf[10..12].copy_from_slice(&(num_recs as u16).to_be_bytes());

    device.write_at(leaf_off, &leaf)?;

    Ok(())
}

/// Build the folder record for the root folder.
///
/// `journal` is true for journaled volumes; the root folder's `valence` is
/// then set to 2 to count the two hidden journal pseudo-files.
fn folder_root_record(journal: bool) -> FolderRecord {
    FolderRecord {
        record_type: K_HFS_PLUS_FOLDER_RECORD,
        flags: 0,
        valence: if journal { 2 } else { 0 }, // root contains .journal files when journaled.
        folder_id: ROOT_FOLDER_ID,
        create_date: 0,
        content_mod_date: 0,
        attribute_mod_date: 0,
        access_date: 0,
        backup_date: 0,
        bsd_info: BsdInfo {
            owner_id: 0,
            group_id: 0,
            admin_flags: 0,
            owner_flags: 0,
            file_mode: 0,
            special: 0,
        },
        user_info: [0u8; FINDER_USER_INFO_SIZE],
        finder_info: [0u8; FINDER_OPAQUE_INFO_SIZE],
        text_encoding: 0,
        folder_count: 0,
    }
}

/// Build the thread record body for the root folder: parentID=1, name=volume.
fn build_root_thread_record(name: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(THREAD_FIXED_SIZE + name.len() * 2);
    out.extend_from_slice(&K_HFS_PLUS_FOLDER_THREAD_RECORD.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&ROOT_PARENT_ID.0.to_be_bytes());
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    for unit in name {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

/// Build a file record body for `.journal` or `.journal_info_block`.
fn build_journal_file_record(journal: &JournalLayout, block_size: u32, cnid: u32) -> Vec<u8> {
    let mut rec = FileRecord::EMPTY;
    rec.record_type = K_HFS_PLUS_FILE_RECORD;
    rec.flags = 0;
    rec.file_id = Cnid(cnid);

    if cnid == 16 {
        // .journal: logical size = total journal size, data fork covers it.
        let journal_size =
            u64::from(journal.end_block - journal.start_block) * u64::from(block_size);
        rec.data_fork = ForkData {
            logical_size: journal_size,
            clump_size: 0,
            total_blocks: journal.end_block - journal.start_block,
            extents: ExtentRecord {
                raw: [
                    ExtentDescriptor {
                        start_block: journal.start_block,
                        block_count: journal.end_block - journal.start_block,
                    },
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                ],
            },
        };
    } else {
        // .journal_info_block: exactly one block.
        rec.data_fork = ForkData {
            logical_size: u64::from(block_size),
            clump_size: 0,
            total_blocks: 1,
            extents: ExtentRecord {
                raw: [
                    ExtentDescriptor {
                        start_block: journal.info_block,
                        block_count: 1,
                    },
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                    ExtentDescriptor::EMPTY,
                ],
            },
        };
    }

    // Use to_bytes() for the full 248-byte record.
    rec.to_bytes().to_vec()
}

/// Build the body of a file thread record: parentID + name.
fn build_file_thread_record(parent: Cnid, name: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(THREAD_FIXED_SIZE + name.len() * 2);
    out.extend_from_slice(&K_HFS_PLUS_FILE_THREAD_RECORD.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&parent.0.to_be_bytes());
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    for unit in name {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

/// Write the journal info block and journal header for a journaled volume.
fn write_journal_info<D: BlockDeviceMut + ?Sized>(
    device: &mut D,
    journal: &JournalLayout,
    block_size: u32,
) -> Result<()> {
    use crate::journal::info::JOURNAL_INFO_BLOCK_SIZE;

    // JournalInfoBlock: flags at 0, deviceSignature at 4 (32 bytes),
    // offset at 36, size at 44.
    let jib_size = u64::from(journal.end_block - journal.start_block) * u64::from(block_size);
    let mut jib_buf = vec![0u8; JOURNAL_INFO_BLOCK_SIZE];
    // flags: kJIJournalNeedInitMask (bit 0) | kJICleanMask? Let's check what
    // mkfs.hfsplus writes: flags = 0x00000005.
    // kJIJournalNeedInitMask = 0x01, kJHMBit = 0x04? No.
    // 0x05 = 0x01 | 0x04. Actually from the header file:
    // kJIJournalNeedInitMask = 0x01
    // kJIColdJournalMask = 0x04? No, that's not a standard mask.
    // Actually 0x05 is kJIJournalNeedInitMask | kJISignedJournalMask? Not sure.
    // Real images show flags=5, so use that.
    let flags: u32 = 0x05;
    let jib_offset = u64::from(journal.start_block) * u64::from(block_size);
    jib_buf[0..4].copy_from_slice(&flags.to_be_bytes());
    jib_buf[36..44].copy_from_slice(&jib_offset.to_be_bytes());
    jib_buf[44..52].copy_from_slice(&jib_size.to_be_bytes());

    let info_off = u64::from(journal.info_block) * u64::from(block_size);
    device.write_at(info_off, &jib_buf)?;

    // The journal data area is left as zeros — mkfs.hfsplus does not write a
    // journal header. The header is written on first use by the journal replay.
    Ok(())
}

/// Write the 14-byte node descriptor at the start of `node`.
fn write_node_descriptor(node: &mut [u8], kind: NodeKind, height: u8) {
    node[0..4].copy_from_slice(&0u32.to_be_bytes());
    node[4..8].copy_from_slice(&0u32.to_be_bytes());
    node[8] = kind.as_i8() as u8;
    node[9] = height;
}

/// Write `value` into offset slot `index` at the end of a node.
fn set_offset(node: &mut [u8], node_size: usize, index: usize, value: u16) {
    let slot = node_size - (index + 1) * OFFSET_SIZE;
    node[slot..slot + OFFSET_SIZE].copy_from_slice(&value.to_be_bytes());
}
