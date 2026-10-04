//! Volume consistency checks, read-only.
//!
//! # What this is and is not
//!
//! These checks exist because two structures on disk have to agree with each
//! other, and either can be wrong. The allocation bitmap says which blocks are in
//! use; the catalog's fork extents say which blocks the files occupy. If those two
//! disagree, the volume is inconsistent whichever one is right — and this crate
//! has no way to tell which, only that something is wrong.
//!
//! That makes these checks worth more than they look. A mis-parsed extent record
//! is mis-parsed identically here and in the reader, so it does **not** get caught
//! — a checker built on the same parser cannot catch that class of bug, and the
//! format notes say so. What it *does* catch is disagreement between two
//! structures that were written independently by the formatter, which is a real
//! signal even though both sides are read by the same code.
//!
//! Correctness of what each check *is* comes from Apple's source, not from here.
//! The provenance of each check is named below.
//!
//! # Nothing is repaired
//!
//! Every check is a read. `fsck_hfs` repairs as well as reports, and pointing it
//! at a fixture once undid a corruption it was meant to be diagnosing. A checker
//! that only reads cannot make that mistake, so there is no repair mode to
//! mis-invoke.
//!
//! Mining reference: `lib_fsck_hfs/dfalib/SVerify1.c` `CheckBitmapRange` and
//! `ExtBTChk`, `SVerify2.c` `BTMapChk` and the volume-information checks, and
//! `core/VolumeAllocation.c` `hfs_count_allocated`.

use crate::alloc::AllocationMap;
use crate::btree::io::BTreeFile;
use crate::btree::key::ExtentKey;
use crate::catalog::record::CatalogRecord;
use crate::error::{Error, Result};
use crate::volume::Volume;

/// Sentinel in [`CheckReport::fork_block_count`] for the volume's own forks.
///
/// No file can hold CNID 0: `kHFSRootFolderID` is 2, and 0 and 1 are reserved,
/// so the value cannot collide with a file's.
pub const SPECIAL_FORK_SENTINEL: u32 = 0;

/// What a set of checks found.
///
/// Every field is a disagreement between two structures, not a verdict on which
/// side is wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Keys inside a leaf node that are not strictly increasing.
    ///
    /// Node number and the index of the offending record. This is the check the
    /// reader cannot perform on itself: a binary search over unordered records
    /// returns an answer, and the answer is wrong. Mining reference:
    /// `lib_fsck_hfs/dfalib/SVerify2.c` compares each key with its predecessor
    /// and reports `E_KeyOrd` when `CompareKeys(prev, key) >= 0` -- so equal keys
    /// are an error too, not merely unsorted ones.
    pub key_order: Vec<(u32, usize)>,
    /// A key longer than the tree's `maxKeyLength`.
    ///
    /// Node number and index. Mining reference: the same function rejects
    /// `keyLength > btcb->maxKeyLength` before comparing, so an over-long key is
    /// a structural fault rather than a comparison that happens to work.
    pub key_length: Vec<(u32, usize)>,
    /// A file or folder record with no matching thread record.
    ///
    /// Such an object exists in the catalog but cannot be reached by name, so a
    /// reader that walked thread records -- which is what `all_objects` does --
    /// would never see it. That makes it invisible damage rather than visible
    /// damage. Mining reference: `lib_fsck_hfs/dfalib/SVerify1.c`'s catalog
    /// hierarchy pass requires a thread record for every object, and
    /// `core/hfs_catalog.c` resolves names through them.
    pub missing_thread: Vec<u32>,
    /// A folder whose `valence` disagrees with the thread records naming it.
    ///
    /// `(folder CNID, declared, counted)`. The count comes from thread records,
    /// which are the only place a parent is recorded for its children, so the two
    /// are independent statements about the same fact.
    pub valence: Vec<(u32, u32, u32)>,
    /// A B-tree node whose height contradicts the tree's depth.
    ///
    /// `(tree, node number)`. A leaf's height is one more than its parent's, so
    /// at the bottom of a depth-*n* tree every leaf must read *n*. A reader
    /// descends by height, so a wrong one sends it into nodes that are not
    /// leaves. Mining reference: `struct BTNodeDescriptor` says "zero for header,
    /// map; child is one more than parent", and
    /// `lib_fsck_hfs/dfalib/SVerify2.c` tests
    /// `height == treeDepth - BTLevel + 1` for every node it visits.
    pub node_height: Vec<(u8, u32)>,
    /// An index record pointing at a node that cannot exist.
    ///
    /// `(tree, node number, child)`. The header node is 0, so a child of 0 is as
    /// impossible as a child past the end of the file. Mining reference: the same
    /// function rejects `nodeNum == kHeaderNodeNum ||
    /// nodeNum >= totalNodes` with `E_IndxLk`.
    pub child_node: Vec<(u8, u32, u32)>,
    /// A node whose sibling link disagrees with the node it was reached from.
    ///
    /// `(tree, node number)`. Leaves at one level form a doubly linked list, and
    /// the links are the only way to enumerate them; a wrong `fLink` silently
    /// truncates the walk. Mining reference: the same function compares each
    /// node's `fLink` against the node the traversal expected to follow.
    pub sibling_link: Vec<(u8, u32)>,
    /// A B-tree whose stored node map disagrees with what it contains.
    ///
    /// `(tree, node the comparison stopped at)`.
    ///
    /// The map is the tree's own claim about which nodes are in use, kept in the
    /// header node's record index 2 and continued through map nodes chained by
    /// `fLink`. One bit per node, MSB first, exactly like the volume allocation
    /// bitmap. `fsck.hfsplus` compares it against a map computed by walking the
    /// tree and reports `E_BadMapN`.
    ///
    /// Mining reference: `lib_fsck_hfs/dfalib/SVerify2.c` `CmpBTreeMap`,
    /// `BTMapChk`, and `SUtils.c` `AllocBTN` for the bit order.
    pub node_map_mismatch: Vec<(u8, u32)>,
    /// A node in the map chain that is not a well-formed map node.
    ///
    /// `(tree, node)`. After the header, every node must be `kBTMapNode` with
    /// `numRecords == Num_MRecs` (1) and height 0.
    pub bad_map_node: Vec<(u8, u32)>,
    /// A node nothing points at that is not erased.
    ///
    /// `(tree, node number)`. Every node the file contains is either reachable
    /// from the root or unused, and an unused node must be entirely zero --
    /// otherwise it holds stale data from whatever the file used to contain, which
    /// is the signal that a tree was edited rather than rebuilt.
    ///
    /// This is the check that catches a *partly* rebuilt tree, and it caught one
    /// while building this crate's own images: a new leaf written into the extents
    /// tree without the header's root node pointing at it left the old nodes
    /// unerased, and `fsck.hfsplus` reported "Unused node is not erased" while
    /// this check did not exist to say anything.
    ///
    /// Mining reference: `BTCheckUnusedNodes` requires all `nodeSize` bytes of an
    /// unvisited node to be zero.
    pub unerased_node: Vec<(u8, u32)>,
    /// A tree whose keys would need the 8-bit length form, which is not decoded.
    ///
    /// `tree`. `kBTBigKeysMask` selects a 16-bit key length; without it Apple
    /// reads a 1-byte one, and this implementation always assumes the 16-bit form.
    /// Every Apple-written tree sets the bit, so a volume needing the other form
    /// is not something the formatters here produce -- but it is expressible, and
    /// misreading it would decode a valid-looking key from the wrong bytes.
    ///
    /// So it is reported rather than misparsed. That is the difference between a
    /// limitation that is visible and one that corrupts silently.
    ///
    /// Mining reference: `lib_fsck_hfs/dfalib/BTreeNodeOps.c` `CalcKeySize`
    /// branches on `btreePtr->attributes & kBTBigKeysMask`, adding
    /// `sizeof(UInt16)` or `sizeof(UInt8)` accordingly.
    pub key_width: Vec<&'static str>,
    /// Blocks the bitmap marks used that nothing references.
    ///
    /// `fsck` reports this as the bitmap needing repair for orphaned blocks.
    pub orphaned: Vec<u32>,
    /// Blocks the catalog references that the bitmap does not mark used.
    ///
    /// The opposite disagreement: a fork whose extents were written without
    /// updating the bitmap. `fsck` reports it as under-allocation.
    pub missing: Vec<u32>,
    /// `nextCatalogID` is not past the highest CNID in use.
    ///
    /// If it is not, the next file created would reuse a CNID that already
    /// identifies an existing one, and a lookup by CNID would then find the wrong
    /// file. Mining reference: `core/hfs_catalog.c` allocates `nextCatalogID`
    /// monotonically and `hfs_vfsutils.c` checks it against the catalog.
    pub next_cnid_reuse: Option<(u32, u32)>,
    /// A fork that breaks one of Apple's two size inequalities.
    ///
    /// `(CNID, or 0 for a special fork, the reason)`. Both come from
    /// `ForkData::validate`, which checks `logical <= physical` and
    /// `physical <= described_blocks * block_size`.
    ///
    /// Mining reference: `lib_fsck_hfs/dfalib/CatalogCheck.c` `CheckFileData`,
    /// reporting `E_LEOF` and `E_PEOF` respectively.
    pub fork_rule: Vec<(u32, String)>,
    /// A fork's `totalBlocks` disagrees with the blocks its extents describe.
    ///
    /// The other direction of the same arithmetic: the extents must account for
    /// every block the fork claims, inline and overflow together.
    pub fork_block_count: Vec<(u32, u32, u32)>,
}

impl CheckReport {
    /// Whether every check passed.
    pub fn is_clean(&self) -> bool {
        self.orphaned.is_empty()
            && self.missing.is_empty()
            && self.next_cnid_reuse.is_none()
            && self.fork_block_count.is_empty()
            && self.fork_rule.is_empty()
            && self.key_order.is_empty()
            && self.key_length.is_empty()
            && self.missing_thread.is_empty()
            && self.valence.is_empty()
            && self.node_height.is_empty()
            && self.child_node.is_empty()
            && self.sibling_link.is_empty()
            && self.unerased_node.is_empty()
            && self.node_map_mismatch.is_empty()
            && self.bad_map_node.is_empty()
            && self.key_width.is_empty()
    }

    /// A one-line summary per disagreement, for a tool to print.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        for block in &self.orphaned {
            out.push(format!(
                "block {block} is marked allocated but no file references it"
            ));
        }
        for block in &self.missing {
            out.push(format!(
                "block {block} is referenced by a fork but not marked allocated"
            ));
        }
        if let Some((next, highest)) = self.next_cnid_reuse {
            out.push(format!(
                "nextCatalogID is {next} but CNID {highest} is already in use"
            ));
        }
        for (node, index) in &self.key_order {
            out.push(format!(
                "node {node} record {index}: its key does not follow the previous one"
            ));
        }
        for (node, index) in &self.key_length {
            out.push(format!(
                "node {node} record {index}: its key is longer than maxKeyLength"
            ));
        }
        for cnid in &self.missing_thread {
            out.push(format!(
                "CNID {cnid} has a record but no thread record, so it cannot be reached by name"
            ));
        }
        for (tree, node) in &self.node_height {
            out.push(format!(
                "{tree} node {node}: its height contradicts the tree depth"
            ));
        }
        for (tree, node, child) in &self.child_node {
            out.push(format!(
                "{tree} node {node}: its index record points at node {child}, which cannot exist"
            ));
        }
        for (tree, node) in &self.sibling_link {
            out.push(format!(
                "{tree} node {node}: its forward link disagrees with the node before it"
            ));
        }
        for tree in &self.key_width {
            out.push(format!(
                "{tree}: its keys would use the 8-bit length form, which this \
                 implementation does not decode; every Apple-written tree sets \
                 kBTBigKeysMask, so this volume was not written by Apple or hfsprogs"
            ));
        }
        for (tree, node) in &self.node_map_mismatch {
            out.push(format!(
                "{tree}: its node map at node {node} disagrees with the nodes it \
                 contains -- a stale map, which is what editing a tree in place \
                 leaves behind"
            ));
        }
        for (tree, node) in &self.bad_map_node {
            out.push(format!(
                "{tree}: node {node} is in the map chain but is not a map node"
            ));
        }
        for (tree, node) in &self.unerased_node {
            out.push(format!(
                "{tree} node {node}: nothing points at it and it is not erased"
            ));
        }
        for (cnid, declared, counted) in &self.valence {
            out.push(format!(
                "folder {cnid} declares valence {declared} but has {counted} children"
            ));
        }
        for (cnid, reason) in &self.fork_rule {
            let which = if *cnid == SPECIAL_FORK_SENTINEL {
                "a special fork".to_string()
            } else {
                format!("file {cnid}")
            };
            out.push(format!("{which}: {reason}"));
        }
        for (cnid, declared, described) in &self.fork_block_count {
            let which = if *cnid == SPECIAL_FORK_SENTINEL {
                "a special fork".to_string()
            } else {
                format!("file {cnid}")
            };
            out.push(format!(
                "{which} declares {declared} blocks but its extents describe {described}"
            ));
        }
        out
    }
}

/// Every node number a tree's root can reach, including the root.
///
/// Walks each level as a sibling chain: the root, then every child of every node
/// on the level above. A node nothing points at is not reached, which is the
/// point -- the map is a claim about what is *in use*.
///
/// Mining reference: `lib_fsck_hfs/dfalib/SVerify2.c` `BTCheck` walks the tree
/// this way, calling `AllocBTN` for each node it lands on.
fn reachable_nodes<D: crate::blockdev::BlockDevice + ?Sized>(
    bt: &BTreeFile<'_, D>,
) -> Result<Vec<u32>> {
    let header = *bt.header();
    let mut out = vec![0u32];
    if header.tree_depth == 0 {
        return Ok(out);
    }

    // One level at a time. Each level is a sibling chain, and a level of index
    // nodes produces the next one; a level of leaves produces nothing.
    //
    // The loop runs `depth + 1` times rather than `depth`, because the last level
    // holds the leaves and they have to be *counted* even though they have
    // nothing below them. Missing that is not subtle in the map -- a depth-1 tree
    // has its root as its only leaf, so the expected map comes out with one bit
    // clear and every freshly formatted volume reports a mismatch.
    let mut level = vec![header.root_node];
    for _ in 0..=header.tree_depth {
        let mut next = Vec::new();
        let mut any_index = false;

        for node_num in &level {
            if *node_num == 0 || *node_num >= header.total_nodes {
                continue;
            }
            let bytes = bt.read_node_bytes(*node_num)?;
            let node = bt.parse_node(&bytes)?;

            // This node, and every right sibling sharing its level, are in use.
            let mut chain = vec![*node_num];
            let mut sibling = node.descriptor().f_link;
            let mut guard = header.total_nodes;
            while sibling != 0 && sibling < header.total_nodes && guard > 0 {
                guard -= 1;
                chain.push(sibling);
                let bytes = bt.read_node_bytes(sibling)?;
                sibling = bt.parse_node(&bytes)?.descriptor().f_link;
            }
            for member in chain {
                if !out.contains(&member) {
                    out.push(member);
                }
            }

            if node.kind() == crate::btree::node::NodeKind::Index {
                any_index = true;
                for index in 0..node.num_records() {
                    let child = node.child(index)?;
                    if child != 0 && child < header.total_nodes && !next.contains(&child) {
                        next.push(child);
                    }
                }
            }
        }

        if !any_index {
            break;
        }
        level = next;
    }
    Ok(out)
}

/// The node map a tree's contents imply: one bit per node, MSB first.
///
/// Byte `n / 8`, mask `0x80 >> (n % 8)` -- the same order as the volume
/// allocation bitmap, which is not a coincidence: both are "which block is in
/// use" bitmaps.
///
/// Mining reference: `lib_fsck_hfs/dfalib/SUtils.c` `AllocBTN`:
///
/// ```c
/// byteP = BTCBMPtr + (nodeNumber / 8);
/// bitPos = nodeNumber % 8;
/// mask = (0x80 >> bitPos);
/// ```
fn expected_node_map(reachable: &[u32], total_nodes: u32) -> Vec<u8> {
    let mut map = vec![0u8; ((total_nodes as usize) + 7) / 8];
    for node in reachable {
        let Some(slot) = map.get_mut((*node / 8) as usize) else {
            continue;
        };
        *slot |= 0x80 >> (*node % 8);
    }
    map
}

/// A node's record offsets, read from the end of the node.
///
/// `nrec + 1` entries, so entry `nrec` is where the free space begins.
fn record_offsets(bytes: &[u8], node_size: usize, num_records: u16) -> Result<Vec<usize>> {
    let mut out = Vec::with_capacity(num_records as usize + 1);
    for i in 0..=num_records {
        let at = node_size
            .checked_sub(2 * (i as usize + 1))
            .ok_or_else(|| Error::invalid("btree node", "the offset array runs past the node"))?;
        if at + 2 > bytes.len() {
            return Err(Error::Truncated {
                what: "btree offset array",
                needed: at + 2,
                available: bytes.len(),
            });
        }
        out.push(usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]])));
    }
    Ok(out)
}

/// Compare a tree's stored node map against the one its contents imply.
///
/// The map is the tree's own claim about which nodes are in use, kept in the
/// header node's record index 2 and continued through map nodes chained by
/// `fLink`. `fsck.hfsplus` compares it against a map computed by walking the
/// tree, and reports `E_BadMapN` on a difference.
///
/// This matters most to a writer: adding a leaf to a tree in place changes what
/// is reachable without touching the map, leaving a tree that reads correctly and
/// disagrees with itself about which nodes it owns.
///
/// Mining reference: `lib_fsck_hfs/dfalib/SVerify2.c` `CmpBTreeMap` and
/// `BTMapChk`. The walk starts at node 0 with record index 2 and follows `fLink`;
/// every node after the first must be `kBTMapNode` with `numRecords == 1` and
/// height 0 (`Num_MRecs`).
fn check_node_map<D: crate::blockdev::BlockDevice + ?Sized>(
    device: &D,
    fork: &crate::format::fork::ForkData,
    block_size: u32,
    hfs_plus: bool,
    tree: &'static str,
    report: &mut CheckReport,
) -> Result<()> {
    use crate::btree::node::NodeKind;

    let bt = BTreeFile::open(device, fork, block_size, hfs_plus)?;
    let header = *bt.header();
    if header.total_nodes == 0 || header.node_size == 0 {
        // An empty tree: `mkfs.hfsplus` leaves the attributes fork's B-tree
        // entirely zeroed, and there is nothing to compare.
        return Ok(());
    }

    let expected = expected_node_map(&reachable_nodes(&bt)?, header.total_nodes);
    let node_size = bt.node_size();
    let mut map_size = expected.len();
    let mut compared = 0usize;

    // The walk starts in the header node at record index 2, then follows fLink
    // through the map nodes.
    let mut node_num = 0u32;
    let mut rec_index = 2u16;
    let mut guard = 16usize;

    while map_size > 0 && guard > 0 {
        guard -= 1;
        let bytes = bt.read_node_bytes(node_num)?;
        let node = bt.parse_node(&bytes)?;

        if node_num != 0 {
            // After the header, every node in the chain must be a map node
            // holding exactly one record at height zero.
            if node.kind() != NodeKind::Map || node.num_records() != 1 || node.height() != 0 {
                report.bad_map_node.push((name_u8(tree), node_num));
                return Ok(());
            }
            rec_index = 0;
        }

        let offsets = record_offsets(&bytes, node_size, node.num_records())?;
        let Some(&at) = offsets.get(rec_index as usize) else {
            report.bad_map_node.push((name_u8(tree), node_num));
            return Ok(());
        };
        let end = offsets
            .get(rec_index as usize + 1)
            .copied()
            .unwrap_or(node_size);
        let size = end.saturating_sub(at).min(map_size);
        let Some(window) = bytes.get(at..(at + size).min(bytes.len())) else {
            report.bad_map_node.push((name_u8(tree), node_num));
            return Ok(());
        };
        if window != &expected[compared..compared + size] {
            report.node_map_mismatch.push((name_u8(tree), node_num));
            return Ok(());
        }
        compared += size;
        map_size -= size;

        // On to the next map node, if the map continues past this record.
        node_num = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if node_num == 0 {
            break;
        }
        rec_index = 0;
    }
    Ok(())
}

/// Every allocation block the volume's own metadata occupies.
///
/// These are the blocks a catalog walk will never yield, because they belong to
/// the structures doing the walking. Omitting them would report the whole
/// metadata zone as orphaned.
///
/// Two regions qualify, and neither is hardcoded:
///
/// - **The reserved prefix.** Everything below the allocation file's first
///   extent. `struct HFSPlusVolumeHeader` sits in block 0 with the boot blocks,
///   and how much else the formatter reserves there depends on the block size:
///   at 4 KiB the allocation file starts at block 1, and at 1 KiB it starts at
///   block 2 with block 1 reserved as well. Measured across the corpus, every
///   block below the allocation file's start is marked used and nothing above it
///   is gratuitously reserved — so deriving the prefix from the volume is both
///   correct and not a guess at a fixed count.
///
/// - **The backup volume header**, 1024 bytes before the end of the volume. That
///   is in the *last* block, not one block from the end. Mining reference:
///   `core/hfs_vfsutils.c` reads the backup from `mdb` at that offset.
///
/// # Errors
///
/// Refuses a volume with no blocks, and one smaller than the backup header's
/// 1024 bytes.
pub fn metadata_blocks(
    block_size: u32,
    total_blocks: u32,
    allocation_start: u32,
) -> Result<Vec<u32>> {
    let mut blocks: Vec<u32> = (0..allocation_start.min(total_blocks)).collect();

    // The backup volume header: 1024 bytes before the end of the volume.
    let volume_bytes = u64::from(total_blocks) * u64::from(block_size);
    if volume_bytes < 1024 {
        return Err(Error::invalid(
            "volume_bytes",
            "a volume smaller than the backup header cannot hold one",
        ));
    }
    let backup_block = ((volume_bytes - 1024) / u64::from(block_size)) as u32;
    if backup_block < total_blocks {
        blocks.push(backup_block);
    }
    Ok(blocks)
}

/// Every allocation block a fork's extents describe, inline and overflow.
///
/// A fork keeps at most `kHFSPlusExtentDensity` (8) extents inline and the rest
/// in the extents overflow B-tree, so a fork that spills has to be walked to its
/// end. Mining reference: `core/hfs_extents.c` `extoffset` walks inline extents
/// and then overflow groups, advancing the key by the blocks already described.
///
/// Returns the blocks and the total the extents account for, so a caller can
/// compare it against the fork's own `totalBlocks`.
pub fn fork_blocks<D: crate::blockdev::BlockDevice + ?Sized>(
    vol: &Volume<'_, D>,
    fork: &crate::format::fork::ForkData,
    cnid: u32,
) -> Result<(Vec<u32>, u32)> {
    let mut blocks = Vec::new();
    let mut total = 0u32;

    for extent in fork.extents.raw.iter() {
        blocks.reserve(extent.block_count as usize);
        for offset in 0..extent.block_count {
            blocks.push(extent.start_block + offset);
        }
        total = total.saturating_add(extent.block_count);
    }

    // Walk the overflow groups, if this fork spills past the inline density.
    if fork.needs_overflow() {
        let header = vol.header();
        let tree = crate::file::TreeOverflow::new(BTreeFile::open(
            vol.device(),
            &header.extents_file,
            header.block_size,
            header.is_hfsx(),
        )?);
        let mut seen = total;
        // Bounded by the fork's own block count, so a corrupt tree cannot make
        // this loop forever.
        let mut guard = 0;
        while u64::from(seen) < u64::from(fork.total_blocks) {
            guard += 1;
            if guard > 64 {
                return Err(Error::overflow("overflow extent groups"));
            }
            let Some(group) = tree
                .find_group(ExtentKey::DATA_FORK, cnid, seen)
                .map_err(|e| Error::invalid("extents overflow tree", e.to_string()))?
            else {
                break;
            };
            let group_total = group.total_blocks();
            if group_total == 0 {
                break;
            }
            for extent in group.raw.iter() {
                for offset in 0..extent.block_count {
                    blocks.push(extent.start_block + offset);
                }
            }
            seen = seen.saturating_add(group_total as u32);
            total = total.saturating_add(group_total as u32);
        }
    }

    Ok((blocks, total))
}

/// Run every check against a volume.
///
/// `alloc_limit` is the point above which blocks are metadata rather than file
/// data; pass `total_blocks` when the caller has no separate limit, which makes
/// the orphaned check report the tail as unallocated-but-marked.
pub fn check<D: crate::blockdev::BlockDevice + ?Sized>(
    vol: &Volume<'_, D>,
    alloc_limit: Option<u32>,
) -> Result<CheckReport> {
    let header = vol.header();
    let mut report = CheckReport::default();

    // --- The bitmap as a mutable map -----------------------------------
    let fork = &header.allocation_file;
    // The allocation file is read whole, using its own declared length: it is a
    // full allocation block on every volume, not merely the bytes the bitmap
    // needs.
    let limit = usize::try_from(fork.logical_size).unwrap_or(1 << 20);
    let bytes = vol.read_fork(fork, limit)?;
    let mut map = AllocationMap::from_bytes(&bytes, header.total_blocks)?
        .with_alloc_limit(alloc_limit.unwrap_or(header.total_blocks));

    // --- Every block the volume claims ---------------------------------
    let allocation_start = header.allocation_file.extents.raw[0].start_block;
    let mut referenced: Vec<u32> =
        metadata_blocks(header.block_size, header.total_blocks, allocation_start)?;
    // The volume's own five forks, each checked as carefully as a file's. A
    // special fork is validated at mount time -- `core/hfs_vfsutils.c` derives
    // each one's expected size from the header and refuses the volume otherwise
    // -- so a catalog fork claiming two billion blocks is a mount failure, not a
    // curiosity.
    for (name, special) in [
        ("allocationFile", &header.allocation_file),
        ("extentsFile", &header.extents_file),
        ("catalogFile", &header.catalog_file),
        ("attributesFile", &header.attributes_file),
        ("startupFile", &header.startup_file),
    ] {
        if special.logical_size == 0 && special.total_blocks == 0 {
            continue;
        }
        let (blocks, described) = fork_blocks(vol, special, 0)?;
        referenced.extend(blocks);
        let _ = name;
        match special.validate(u64::from(described), header.block_size) {
            Ok(()) => {
                if described != special.total_blocks {
                    report.fork_block_count.push((
                        SPECIAL_FORK_SENTINEL,
                        special.total_blocks,
                        described,
                    ));
                }
            }
            Err(e) => report
                .fork_rule
                .push((SPECIAL_FORK_SENTINEL, e.to_string())),
        }
    }

    // --- Every block the catalog says the files occupy ------------------
    //
    // The raw record walk, not `all_objects`: that resolves each object through
    // its thread record, so a file whose thread record is missing would not
    // appear here at all -- and its data blocks would then be reported as
    // orphaned, which is a symptom rather than the diagnosis.
    let mut highest_cnid = 0u32;
    for (_key, record) in vol.catalog().all_records()? {
        match record {
            CatalogRecord::File(f) => {
                let cnid = f.file_id.0;
                highest_cnid = highest_cnid.max(cnid);
                for fork in [&f.data_fork, &f.resource_fork] {
                    if fork.logical_size == 0 && fork.total_blocks == 0 {
                        continue;
                    }
                    let (blocks, described) = fork_blocks(vol, fork, cnid)?;
                    referenced.extend(blocks);
                    // Apple's two inequalities, checked against the blocks the
                    // extents really describe rather than against `total_blocks`
                    // alone. `reported` accumulates so a fork is not reported once
                    // per check: the first failure is the informative one.
                    match fork.validate(u64::from(described), header.block_size) {
                        Ok(()) => {
                            if described != fork.total_blocks {
                                report
                                    .fork_block_count
                                    .push((cnid, fork.total_blocks, described));
                            }
                        }
                        Err(e) => report.fork_rule.push((cnid, e.to_string())),
                    }
                }
            }
            CatalogRecord::Folder(f) => {
                highest_cnid = highest_cnid.max(f.folder_id.0);
            }
            CatalogRecord::Thread(_) => {}
        }
    }

    // --- Blocks held by attribute values --------------------------------
    //
    // An attribute whose value is too big for its record lives in allocation
    // blocks that the catalog never mentions: they are reachable only by walking
    // the attributes tree. Without this, every such block looks orphaned -- which
    // means this checker would report a false positive on any real macOS volume
    // with a large FinderInfo, rather than only on the fixture that has one.
    //
    // Mining reference: `core/hfs_format.h` `kHFSPlusAttrForkData` holds a full
    // `HFSPlusForkData`, and `kHFSPlusAttrExtents` records continue it. The value's
    // blocks are as real as any file's.
    if header.attributes_file.logical_size > 0 {
        if let Ok(tree) = crate::attributes::AttributesFile::open(
            vol.device(),
            &header.attributes_file,
            header.block_size,
            header.is_hfsx(),
        ) {
            if let Ok(blocks) = tree.allocated_blocks() {
                for block in blocks {
                    if block < u64::from(header.total_blocks) {
                        referenced.push(block as u32);
                    }
                }
            }
        }
    }

    // --- Compare --------------------------------------------------------
    let mut referenced_bitmap = vec![false; header.total_blocks as usize];
    for block in &referenced {
        if *block >= header.total_blocks {
            return Err(Error::BadBlockNumber {
                block: *block,
                total_blocks: header.total_blocks,
            });
        }
        referenced_bitmap[*block as usize] = true;
    }

    let is_referenced =
        |b: u32| -> bool { referenced_bitmap.get(b as usize).copied().unwrap_or(false) };
    report.orphaned = map.orphaned(&is_referenced);
    report.missing = map.missing(&is_referenced);

    if header.next_catalog_id <= highest_cnid {
        report.next_cnid_reuse = Some((header.next_catalog_id, highest_cnid));
    }

    // --- B-tree structure ----------------------------------------------
    //
    // Checked for every tree the volume has, not just the catalog: an extents or
    // attributes tree with a wrong height or a dangling child pointer would make
    // a fork resolve to nothing, which looks like a missing file rather than a
    // damaged tree.
    for (name, fork) in [
        ("catalog", &header.catalog_file),
        ("extents", &header.extents_file),
        ("attributes", &header.attributes_file),
    ] {
        if fork.logical_size == 0 {
            continue;
        }
        check_btree(
            vol.device(),
            fork,
            header.block_size,
            header.is_hfsx(),
            name,
            &mut report,
        )?;
        check_node_map(
            vol.device(),
            fork,
            header.block_size,
            header.is_hfsx(),
            name,
            &mut report,
        )?;
    }

    // --- Catalog structure ---------------------------------------------
    //
    // Separate from the bitmap work because these catch a different class: the
    // bitmap checks find a disagreement between two structures, while these find
    // a fault *within* the catalog that the reader cannot notice on its own. A
    // binary search over unordered keys returns an answer, and the answer is
    // wrong.
    check_catalog_structure(vol, &mut report)?;

    // Keep `map` borrowed so the borrow checker proves nothing above wrote to it.
    let _ = &mut map;
    Ok(report)
}

/// Check the catalog's own structure: key order, key lengths, thread records
/// and folder valence.
///
/// Nothing here needs the bitmap, and nothing repairs anything -- a catalog that
/// fails these is reported with the node and record index so a repair can be
/// attempted deliberately.
///
/// Mining reference: `lib_fsck_hfs/dfalib/SVerify2.c` for key order and key
/// length, and `SVerify1.c`'s catalog hierarchy pass for thread records and
/// valence.
fn check_catalog_structure<D: crate::blockdev::BlockDevice + ?Sized>(
    vol: &Volume<'_, D>,
    report: &mut CheckReport,
) -> Result<()> {
    let cat = vol.catalog();
    let tree = cat.tree();
    let max_key_length = tree.header().max_key_length as usize;
    let last_leaf = tree.header().last_leaf_node;
    let mut node_num = tree.header().first_leaf_node;
    let mut budget = tree.header().total_nodes;

    // CNIDs seen with their own record, and thread records seen.
    let mut objects: Vec<u32> = Vec::new();
    let mut threads: Vec<(u32, u32)> = Vec::new();
    // Declared child counts, from folder records.
    let mut declared_valence: Vec<(u32, u32)> = Vec::new();

    while budget > 0 {
        budget -= 1;
        let bytes = tree.read_node_bytes(node_num)?;
        let node = tree.parse_node(&bytes)?;
        if node.kind() != crate::btree::node::NodeKind::Leaf {
            break;
        }

        let mut previous: Option<crate::catalog::key::CatalogKey> = None;
        for index in 0..node.num_records() {
            let record = node
                .record(index)
                .map_err(|_| Error::invalid("catalog node", "a record offset ran past the node"))?;

            // Key length first, as Apple does: an over-long key is a structural
            // fault, and comparing it anyway would be comparing garbage.
            match crate::catalog::key::CatalogKey::from_record(record, max_key_length) {
                Ok(key) => {
                    let declared = key_length(record)?;
                    if declared > max_key_length {
                        report.key_length.push((node_num, index as usize));
                        continue;
                    }
                    if let Some(prev) = &previous {
                        if cat.compare_keys(prev, &key) != crate::unicode::Ordering::Less {
                            // E_KeyOrd: "Keys out of order".
                            report.key_order.push((node_num, index as usize));
                        }
                    }
                    previous = Some(key);
                }
                Err(_) => {
                    report.key_length.push((node_num, index as usize));
                    continue;
                }
            }

            let Some((key, body)) = crate::catalog::lookup::split_record(record) else {
                continue;
            };
            let Ok(parsed) = crate::catalog::record::parse_record(body) else {
                continue;
            };
            match parsed {
                CatalogRecord::File(f) => objects.push(f.file_id.0),
                CatalogRecord::Folder(f) => {
                    objects.push(f.folder_id.0);
                    declared_valence.push((f.folder_id.0, f.valence));
                }
                // The thread record's own CNID lives in its *key*, not its body:
                // `struct HFSPlusCatalogThread` names the object's parent, and
                // the key's parentID is the object itself. So the pairing is
                // (parent from the body, object from the key).
                CatalogRecord::Thread(t) => {
                    threads.push((t.parent_id.0, key.parent_id.0));
                }
            }
        }

        if node_num == last_leaf {
            break;
        }
        // `fLink` is at 0 and `bLink` at 4. Walking `bLink` goes *backwards*, which
        // on a catalog with one leaf is indistinguishable from walking forwards --
        // there is no second leaf to be missed -- and on any larger catalog silently
        // examines one leaf and reports every object in it as having no thread
        // record. Which is exactly what it did.
        let next = u32::from_be_bytes([node.raw()[0], node.raw()[1], node.raw()[2], node.raw()[3]]);
        if next == 0 {
            break;
        }
        node_num = next;
    }

    // Every object needs a thread record keyed on its own CNID with an empty
    // name. That pairing is the only route from a name to an object, so an object
    // without one is unreachable however sound the rest of the volume is.
    let has_thread = |cnid: u32| -> bool { threads.iter().any(|(_, t)| *t == cnid) };
    for cnid in &objects {
        if !has_thread(*cnid) {
            // No catalog fsck code for this one: Apple reports it through the
            // hierarchy pass rather than a single check, so the message is the
            // description.
            report.missing_thread.push(*cnid);
        }
    }

    // Valence: the declared count against the number of thread records naming
    // this folder as parent.
    for (cnid, declared) in &declared_valence {
        let counted = threads.iter().filter(|(parent, _)| parent == cnid).count() as u32;
        if counted != *declared {
            // E_DirVal: "Invalid directory item count".
            report.valence.push((*cnid, *declared, counted));
        }
    }

    Ok(())
}

/// Check one B-tree's node structure.
///
/// Walks from the root, which is the only way to see every node: a tree can have
/// nodes that no index record and no sibling link reaches, and those are exactly
/// the ones worth finding.
///
/// `tree` names the tree in any finding, so a report says which tree it is
/// complaining about rather than leaving a bare node number.
///
/// Mining reference: `lib_fsck_hfs/dfalib/SVerify2.c` `BTCheck`, which walks
/// from `rootNode` checking kind, height, child pointers and sibling links.
fn check_btree<D: crate::blockdev::BlockDevice + ?Sized>(
    device: &D,
    fork: &crate::format::fork::ForkData,
    block_size: u32,
    hfs_plus: bool,
    tree: &'static str,
    report: &mut CheckReport,
) -> Result<()> {
    let bt = BTreeFile::open(device, fork, block_size, hfs_plus)?;
    let header = *bt.header();
    let tree_depth = header.tree_depth;
    let total_nodes = header.total_nodes;

    // A tree that needs the 8-bit key-length form cannot be walked correctly by
    // this implementation, so it is reported instead of walked. Note that
    // `has_big_keys` ORs the bit with `maxKeyLength > 40`, so this only fires for
    // a tree that is *consistently* short-key: a corrupt attribute word on a
    // long-key tree already reads as big-key and is caught by the key-length
    // check instead.
    if !header.has_big_keys() {
        report.key_width.push(tree);
        return Ok(());
    }

    // An empty tree still has a header node and nothing else, and `rootNode` is 0
    // -- which is the header. There is nothing to walk, but the unused-node check
    // still applies: every other node must be erased.
    if tree_depth == 0 {
        check_unused_nodes(&bt, &[0], tree, report);
        return Ok(());
    }

    // Depth 1 means the root is a leaf; otherwise it is an index node with a
    // child per record.
    let mut level = tree_depth;
    let node_num = header.root_node;
    // The loop below inspects exactly one node -- the root -- and then each of
    // its children, so the bound is the node count: a tree cannot have more nodes
    // than it has, and `budget` makes that a hard stop rather than a promise.
    let budget = total_nodes.max(1);

    if budget == 0 || node_num == 0 || node_num >= total_nodes {
        report.child_node.push((name_u8(tree), node_num, node_num));
        return Ok(());
    }
    {
        let bytes = bt.read_node_bytes(node_num)?;
        let node = bt.parse_node(&bytes)?;

        // Height is `tree_depth` minus the number of index levels above the node:
        // the root sits at `tree_depth` and a leaf directly under a single index
        // node at 1. `level` *is* that count -- it starts at `tree_depth` and drops
        // by one per index level -- so the expectation is `level` itself.
        //
        // Reading it as `level - 1` inverts the whole tree, and nothing notices on a
        // catalog with one leaf, where the two agree. `fsck.hfsplus` does notice: it
        // rejects a depth-2 catalog whose index node is at 1 and whose leaves are at
        // 2, which is exactly what the inverted rule asks for.
        let expected_height = level as u8;
        if node.height() != expected_height {
            // E_NHeight: "Invalid node height".
            report.node_height.push((name_u8(tree), node_num));
        }

        match node.kind() {
            crate::btree::node::NodeKind::Leaf => {
                // Reached the bottom. Walk the sibling chain, which is how leaves
                // at this level are enumerated, and check each link against the
                // node we expect to come next.
                let mut reached = vec![0u32, node_num];
                let mut expected = node_num;
                let mut cursor = node_num;
                let mut budget = total_nodes;
                while budget > 0 {
                    budget -= 1;
                    let next = f_link(&bt, cursor)?;
                    if next == 0 {
                        break;
                    }
                    if next == expected {
                        report.sibling_link.push((name_u8(tree), next));
                        break;
                    }
                    if next >= total_nodes {
                        report.child_node.push((name_u8(tree), expected, next));
                        break;
                    }
                    let bytes = bt.read_node_bytes(next)?;
                    let next_node = bt.parse_node(&bytes)?;
                    if next_node.kind() != crate::btree::node::NodeKind::Leaf {
                        report.sibling_link.push((name_u8(tree), next));
                        break;
                    }
                    let expected_height = level as u8;
                    if next_node.height() != expected_height {
                        report.node_height.push((name_u8(tree), next));
                    }
                    reached.push(next);
                    expected = next;
                    cursor = next;
                }
                reached.extend(reached_from_index_children(&bt, node_num, total_nodes)?);
                check_unused_nodes(&bt, &reached, tree, report);
            }
            crate::btree::node::NodeKind::Index => {
                level -= 1;
                for index in 0..node.num_records() {
                    let child = node.child(index)?;
                    if child == 0 || child >= total_nodes {
                        // E_IndxLk: "Invalid index link".
                        report.child_node.push((name_u8(tree), node_num, child));
                    } else {
                        // The child must itself be structurally sound, so walk
                        // into it rather than only recording it.
                        check_child(&bt, child, level, tree_depth, tree, total_nodes, report)?;
                    }
                }
            }
            other => {
                return Err(Error::invalid(
                    "B-tree node kind",
                    format!("node {node_num} is {other:?}, which is neither index nor leaf"),
                ))
            }
        }
    }
    Ok(())
}

/// Every node reachable from an index node's children, one level down.
///
/// The unused-node check needs the whole reachable set, and on a depth-*n* tree
/// that is every level. Recursing here rather than threading a set through the
/// caller keeps the walk in one place.
fn reached_from_index_children<D: crate::blockdev::BlockDevice + ?Sized>(
    bt: &BTreeFile<'_, D>,
    node_num: u32,
    total_nodes: u32,
) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    if node_num == 0 || node_num >= total_nodes {
        return Ok(out);
    }
    let bytes = bt.read_node_bytes(node_num)?;
    let node = bt.parse_node(&bytes)?;
    if node.kind() != crate::btree::node::NodeKind::Index {
        return Ok(out);
    }
    for index in 0..node.num_records() {
        let child = node.child(index)?;
        if child == 0 || child >= total_nodes {
            continue;
        }
        out.push(child);
        out.extend(reached_from_index_children(bt, child, total_nodes)?);
        // Children at every level also enumerate through sibling links.
        out.extend(sibling_chain(bt, child, total_nodes)?);
    }
    Ok(out)
}

/// A node's right siblings, following `fLink` until it ends.
fn sibling_chain<D: crate::blockdev::BlockDevice + ?Sized>(
    bt: &BTreeFile<'_, D>,
    from: u32,
    total_nodes: u32,
) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut cursor = from;
    let mut budget = total_nodes;
    while budget > 0 {
        budget -= 1;
        let next = f_link(bt, cursor)?;
        if next == 0 || next >= total_nodes || out.contains(&next) {
            break;
        }
        out.push(next);
        cursor = next;
    }
    Ok(out)
}

/// A node's `fLink`: the next node to its right at the same level.
fn f_link<D: crate::blockdev::BlockDevice + ?Sized>(
    bt: &BTreeFile<'_, D>,
    node_num: u32,
) -> Result<u32> {
    let bytes = bt.read_node_bytes(node_num)?;
    if bytes.len() < 8 {
        return Err(Error::Truncated {
            what: "BTNodeDescriptor",
            needed: 8,
            available: bytes.len(),
        });
    }
    Ok(u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}

/// Report every node of the file that nothing reaches and that is not erased.
///
/// Mining reference: `lib_fsck_hfs/dfalib/SVerify2.c` `BTCheckUnusedNodes`
/// requires all `nodeSize` bytes of an unvisited node to be zero, and stops at
/// the first that are not.
fn check_unused_nodes<D: crate::blockdev::BlockDevice + ?Sized>(
    bt: &BTreeFile<'_, D>,
    reached: &[u32],
    tree: &'static str,
    report: &mut CheckReport,
) {
    let total_nodes = bt.header().total_nodes;
    for node_num in 0..total_nodes {
        if reached.contains(&node_num) {
            continue;
        }
        let Ok(bytes) = bt.read_node_bytes(node_num) else {
            continue;
        };
        if bytes.iter().any(|b| *b != 0) {
            // E_UnusedNodeNotZeroed: "Unused node is not erased".
            report.unerased_node.push((name_u8(tree), node_num));
        }
    }
}

/// Walk one level down, checking the child's height.
///
/// Kept separate so the index case reads as a loop over children rather than a
/// recursion, and so the depth accounting is stated in one place.
fn check_child<D: crate::blockdev::BlockDevice + ?Sized>(
    bt: &BTreeFile<'_, D>,
    child: u32,
    level: u16,
    tree_depth: u16,
    tree: &'static str,
    total_nodes: u32,
    report: &mut CheckReport,
) -> Result<()> {
    if child == 0 || child >= total_nodes {
        report.child_node.push((name_u8(tree), child, child));
        return Ok(());
    }
    let bytes = bt.read_node_bytes(child)?;
    let node = bt.parse_node(&bytes)?;
    // `level` is the depth-minus-index-levels count for this node, which *is* its
    // height. The `- level + 1` form of this formula appears three times in this
    // file and all three were wrong in the same way; see the root check above.
    let _ = tree_depth;
    let expected = level as u8;
    if node.height() != expected {
        report.node_height.push((name_u8(tree), child));
    }
    Ok(())
}

/// A short, stable identifier for a tree, for report messages.
fn name_u8(tree: &str) -> u8 {
    match tree {
        "catalog" => 1,
        "extents" => 2,
        _ => 3,
    }
}

/// The declared byte length of a key, prefix included.
fn key_length(record: &[u8]) -> Result<usize> {
    if record.len() < 2 {
        return Err(Error::Truncated {
            what: "catalog key",
            needed: 2,
            available: record.len(),
        });
    }
    let declared = u16::from_be_bytes([record[0], record[1]]) as usize;
    Ok(declared + 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backup_header_is_in_the_last_block() {
        // 32 MiB at 4 KiB: the backup header is 1024 bytes before the end, which
        // is inside block 8191 -- not one block from the end.
        let blocks = metadata_blocks(4096, 8192, 1).expect("metadata blocks");
        assert_eq!(blocks, vec![0, 8191]);
        assert!(
            !blocks.contains(&(8192 - 2)),
            "8190 is ordinary file space; the backup header is not there"
        );
    }

    #[test]
    fn the_backup_header_follows_the_block_size_not_a_fixed_offset() {
        // At 1 KiB blocks the backup header moves with the block size, so a
        // fixed "second to last block" would be wrong for every volume but a 4 KiB
        // one.
        let blocks = metadata_blocks(1024, 32768, 2).expect("metadata blocks");
        assert_eq!(blocks, vec![0, 1, 32767]);
    }

    #[test]
    fn a_volume_too_small_for_a_backup_header_is_refused() {
        // The backup header sits 1024 bytes before the end, so a volume smaller
        // than that has nowhere to put it and the subtraction would wrap.
        assert!(
            metadata_blocks(512, 1, 1).is_err(),
            "512 bytes is less than 1024"
        );
        assert!(metadata_blocks(4096, 0, 1).is_err(), "an empty volume");
        assert!(
            metadata_blocks(1024, 1, 1).is_ok(),
            "one 1 KiB block is exactly enough for the header to have a home"
        );
    }

    #[test]
    fn a_clean_report_says_so_and_prints_nothing() {
        let report = CheckReport::default();
        assert!(report.is_clean());
        assert!(report.describe().is_empty());
    }

    #[test]
    fn a_report_names_each_disagreement() {
        let report = CheckReport {
            orphaned: vec![7],
            missing: vec![9],
            next_cnid_reuse: Some((18, 20)),
            fork_block_count: vec![(19, 10, 8)],
            fork_rule: vec![(20, "logicalSize claims a hole".to_string())],
            key_order: vec![(1, 4)],
            key_length: vec![(1, 2)],
            missing_thread: vec![22],
            valence: vec![(2, 9, 8)],
            node_height: vec![(1, 7)],
            child_node: vec![(1, 3, 900)],
            sibling_link: vec![(2, 5)],
            unerased_node: vec![(1, 9)],
            node_map_mismatch: vec![(1, 0)],
            bad_map_node: vec![(2, 3)],
            key_width: vec!["attributes"],
        };
        assert!(!report.is_clean());
        let lines = report.describe();
        // Thirteen original findings, plus the fork rule and the two map ones.
        assert_eq!(
            lines.len(),
            16,
            "one line per disagreement:\n{}",
            lines.join("\n")
        );
        assert!(lines[0].contains('7') && lines[0].contains("no file references"));
        assert!(lines[1].contains('9') && lines[1].contains("not marked"));
        assert!(lines[2].contains("nextCatalogID"));
        assert!(lines
            .iter()
            .any(|l| l.contains("19") && l.contains("declares 10")));
        assert!(lines.iter().any(|l| l.contains("does not follow")));
        assert!(lines.iter().any(|l| l.contains("maxKeyLength")));
        assert!(lines.iter().any(|l| l.contains("no thread record")));
        assert!(lines.iter().any(|l| l.contains("valence")));
        assert!(lines.iter().any(|l| l.contains("claims a hole")));
        assert!(lines.iter().any(|l| l.contains("height contradicts")));
        assert!(lines.iter().any(|l| l.contains("points at node 900")));
        assert!(lines.iter().any(|l| l.contains("forward link disagrees")));
        assert!(lines.iter().any(|l| l.contains("not erased")));
        assert!(lines.iter().any(|l| l.contains("node map")));
        assert!(lines.iter().any(|l| l.contains("map chain")));
        assert!(lines.iter().any(|l| l.contains("8-bit length form")));
    }
}
