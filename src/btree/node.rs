//! B-tree node parsing and record addressing.
//!
//! # Node layout
//!
//! An HFS+ B-tree node is a fixed-size block with three regions, and the
//! direction of growth is what makes it confusing on first contact:
//!
//! ```text
//! 0                                                        node_size
//! +--------+--------------------------------------------+
//! | 14-byte|  records, growing UPWARD from offset 14     |
//! |  node  |                                             |
//! | descr. |                                             |
//! +--------+----------------------------------------------+
//! |                     free space                        |
//! +------------------------------------------------------+
//! |  offset array: (numRecords + 1) u16, growing DOWNWARD|
//! +-------------------------------------------+---------+
//! ```
//!
//! The offset array is a list of `u16` values at the *end* of the node, in
//! descending address order. Record `i` begins at the address stored in slot
//! `i`, and ends at the address in slot `i + 1`.
//!
//! Mining reference: Apple `core/BTreeNodeOps.c` defines exactly this in
//! `GetRecordOffset` and `GetRecordSize`:
//!
//! ```c
//! #define GetRecordOffset(btreePtr,node,index) \
//!     (*(short *) ((u_int8_t *)(node) + (btreePtr)->nodeSize - ((index) << 1) - kOffsetSize))
//!
//! pos = (u_int16_t *) ((Ptr)node + btreePtr->nodeSize - (index << 1) - kOffsetSize);
//! return  *(pos-1) - *pos;      /* GetRecordSize: offset[i] - offset[i+1] */
//! ```
//!
//! `kOffsetSize` is 2. Note that `GetRecordOffset` does no bounds checking of
//! its own: it will happily return an address outside the node if `index` is
//! absurd. Every accessor here bounds-checks first, because the node's
//! `numRecords` comes off the disk.

use crate::endian::Be;
use crate::error::{Error, Result};

/// Byte size of an on-disk `BTNodeDescriptor`.
pub const NODE_DESCRIPTOR_SIZE: usize = 14;

/// Byte size of each entry in a node's record offset array.
pub const OFFSET_SIZE: usize = 2;

/// Upper bound on how many levels a search will descend.
///
/// Apple's `VerifyHeader` rejects a header whose `treeDepth` exceeds
/// `kMaxTreeDepth`, which is 16, so a valid tree never needs more than that.
/// Descent is bounded anyway: the bound is what stops a corrupt tree whose
/// header lies about its depth from looping forever.
pub const NODE_MAX_DEPTH: usize = 16;

/// The kind of a B-tree node.
///
/// Mining reference: Apple `core/hfs_format.h`:
///
/// ```c
/// enum { kBTLeafNode = -1, kBTIndexNode = 0, kBTHeaderNode = 1, kBTMapNode = 2 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// Leaf node: holds the actual key-record pairs.
    Leaf,
    /// Index node: holds keys plus child node numbers.
    Index,
    /// Header node: always node 0; holds the `BTHeaderRec` and the node map.
    Header,
    /// Map node: holds the allocation bitmap for node numbers.
    Map,
}

impl NodeKind {
    /// Decode the on-disk `kind` byte.
    ///
    /// # Errors
    ///
    /// Rejects anything outside the four defined values. A node of unknown kind
    /// is a corrupt tree, and guessing at its layout would be worse than
    /// refusing.
    pub fn from_i8(raw: i8) -> Result<Self> {
        match raw {
            -1 => Ok(NodeKind::Leaf),
            0 => Ok(NodeKind::Index),
            1 => Ok(NodeKind::Header),
            2 => Ok(NodeKind::Map),
            other => Err(Error::invalid(
                "BTNodeDescriptor.kind",
                format!("unknown node kind {other}"),
            )),
        }
    }

    /// The on-disk byte value.
    pub const fn as_i8(self) -> i8 {
        match self {
            NodeKind::Leaf => -1,
            NodeKind::Index => 0,
            NodeKind::Header => 1,
            NodeKind::Map => 2,
        }
    }

    /// Whether this kind participates in search descent.
    pub const fn is_internal(self) -> bool {
        matches!(self, NodeKind::Index | NodeKind::Leaf)
    }
}

/// The 14-byte node descriptor at the start of every node.
///
/// Mining reference: Apple `core/hfs_format.h`, `struct BTNodeDescriptor`, and
/// its swap routine in `core/hfs_endian.c`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeDescriptor {
    /// Next node at this level, or 0 if none.
    pub f_link: u32,
    /// Previous node at this level, or 0 if none.
    pub b_link: u32,
    /// Node kind.
    pub kind: NodeKind,
    /// Height above the leaves; zero for header and map nodes, and a leaf's
    /// parent indexes with 1.
    pub height: u8,
    /// Number of records in this node.
    pub num_records: u16,
    /// Reserved, initialised to zero.
    pub reserved: u16,
}

impl NodeDescriptor {
    /// Parse a node descriptor from the head of `bytes`.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let be = Be::new(bytes);
        Ok(NodeDescriptor {
            f_link: be.u32(0)?,
            b_link: be.u32(4)?,
            kind: NodeKind::from_i8(be.u8(8)? as i8)?,
            height: be.u8(9)?,
            num_records: be.u16(10)?,
            reserved: be.u16(12)?,
        })
    }
}

/// A borrowed B-tree node.
#[derive(Clone, Copy)]
pub struct Node<'a> {
    raw: &'a [u8],
    node_size: usize,
    desc: NodeDescriptor,
}

impl std::fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("kind", &self.desc.kind)
            .field("height", &self.desc.height)
            .field("num_records", &self.desc.num_records)
            .field("f_link", &self.desc.f_link)
            .field("b_link", &self.desc.b_link)
            .field("node_size", &self.node_size)
            .finish()
    }
}

impl<'a> Node<'a> {
    /// Parse a node from `raw`, which must be exactly `node_size` bytes.
    ///
    /// `node_size` is a parameter rather than a field because it comes from the
    /// tree's header record, which lives inside the node being opened. Apple
    /// handles the same chicken-and-egg by reading node 0 at the device block
    /// size, learning `nodeSize` from the header record, and re-reading if the
    /// two disagree; see `core/BTree.c` `BTOpenPath`.
    pub fn parse(raw: &'a [u8], node_size: usize) -> Result<Self> {
        if raw.len() < node_size {
            return Err(Error::Truncated {
                what: "btree node",
                needed: node_size,
                available: raw.len(),
            });
        }
        if node_size < NODE_DESCRIPTOR_SIZE + OFFSET_SIZE {
            return Err(Error::invalid(
                "BTHeaderRec.nodeSize",
                format!("{node_size} is too small to hold a node descriptor"),
            ));
        }
        let desc = NodeDescriptor::parse(raw)?;
        Node {
            raw,
            node_size,
            desc,
        }
        .validate()?;
        Ok(Node {
            raw,
            node_size,
            desc,
        })
    }

    /// Structural checks that every node access depends on.
    ///
    /// The important one is `num_records`: it is a 16-bit value read from the
    /// disk, and the offset array holds only `node_size / 2` slots, so an
    /// oversized count would otherwise let a caller read past the node.
    fn validate(&self) -> Result<()> {
        let capacity = (self.node_size - NODE_DESCRIPTOR_SIZE) / OFFSET_SIZE;
        if usize::from(self.desc.num_records) > capacity {
            return Err(Error::invalid(
                "BTNodeDescriptor.numRecords",
                format!(
                    "{} records exceeds the {} the node can hold",
                    self.desc.num_records, capacity
                ),
            ));
        }
        Ok(())
    }

    /// The node descriptor.
    pub fn descriptor(&self) -> NodeDescriptor {
        self.desc
    }

    /// Node kind.
    pub fn kind(&self) -> NodeKind {
        self.desc.kind
    }

    /// Node height: zero for the header and map nodes, one more than the parent
    /// for the rest.
    pub fn height(&self) -> u8 {
        self.desc.height
    }

    /// The child node number stored at `index` in an index node.
    ///
    /// An index record is a key followed by a `u32` node number, so the child
    /// sits immediately after the key. Refuses a leaf node, which has no
    /// children, and an index beyond the node's records.
    ///
    /// Mining reference: `lib_fsck_hfs/dfalib/BTreeNodeOps.c` `GetChildNodeNum`
    /// seeks to the record, steps past `CalcKeySize`, and reads a `UInt32`.
    pub fn child(&self, index: u16) -> Result<u32> {
        if self.kind() != NodeKind::Index {
            return Err(Error::invalid(
                "node kind",
                "only an index node has child node numbers",
            ));
        }
        let record = self.record(index)?;
        let key_size = key_size_on_disk(record)?;
        let at = key_size;
        if at + 4 > record.len() {
            return Err(Error::Truncated {
                what: "index record",
                needed: at + 4,
                available: record.len(),
            });
        }
        Ok(u32::from_be_bytes([
            record[at],
            record[at + 1],
            record[at + 2],
            record[at + 3],
        ]))
    }

    /// Number of records.
    pub fn num_records(&self) -> u16 {
        self.desc.num_records
    }

    /// Configured node size.
    pub fn node_size(&self) -> usize {
        self.node_size
    }

    /// The whole node buffer.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// Address of record `index` within the node.
    ///
    /// Mining reference: `GetRecordOffset` in `core/BTreeNodeOps.c`, which
    /// reads the `u16` at `node + nodeSize - (index << 1) - kOffsetSize`.
    pub fn record_offset(&self, index: u16) -> Result<usize> {
        if index > self.desc.num_records {
            return Err(Error::out_of_range(
                "record index",
                u64::from(index),
                u64::from(self.desc.num_records),
            ));
        }
        let raw = self.read_offset_slot(index)?;
        Ok(usize::from(raw))
    }

    /// Byte range of record `index` within the node.
    ///
    /// Mining reference: `GetRecordSize` returns `offset[index] - offset[index+1]`.
    pub fn record_range(&self, index: u16) -> Result<std::ops::Range<usize>> {
        if index >= self.desc.num_records {
            return Err(Error::out_of_range(
                "record index",
                u64::from(index),
                u64::from(self.desc.num_records),
            ));
        }
        let start = self.record_offset(index)?;
        let end = self.record_offset(index + 1)?;
        if end < start {
            return Err(Error::invalid(
                "btree record offset array",
                format!("record {index} ends at {end} before it starts at {start}"),
            ));
        }
        // A record must not extend past the node.
        if end > self.node_size {
            let node_size = self.node_size;
            return Err(Error::invalid(
                "btree record offset array",
                format!("record {index} ends at {end}, past the {node_size}-byte node"),
            ));
        }
        Ok(start..end)
    }

    /// Borrow record `index`.
    pub fn record(&self, index: u16) -> Result<&'a [u8]> {
        let range = self.record_range(index)?;
        Ok(&self.raw[range])
    }

    /// Address just past the last record, where a new record would be written.
    pub fn free_offset(&self) -> Result<usize> {
        self.record_offset(self.desc.num_records)
    }

    /// Bytes of unused space in the node.
    ///
    /// Mining reference: Apple `core/BTreeNodeOps.c` `GetNodeFreeSize`, which
    /// is verbatim
    ///
    /// ```c
    /// freeOffset = GetRecordOffset (btreePtr, node, node->numRecords);
    /// return btreePtr->nodeSize - freeOffset - (node->numRecords << 1) - kOffsetSize;
    /// ```
    ///
    /// i.e. the gap between the lowest record and the start of the offset array.
    /// The node descriptor is not subtracted: it sits below the records, outside
    /// that gap.
    pub fn free_space(&self) -> Result<usize> {
        let free = self.free_offset()?;
        let tail = (usize::from(self.desc.num_records) + 1) * OFFSET_SIZE;
        let tail = tail.max(OFFSET_SIZE);
        self.node_size
            .checked_sub(free)
            .and_then(|v| v.checked_sub(tail))
            .ok_or(Error::overflow("node free space"))
    }

    /// Read the raw `u16` in offset slot `index`.
    fn read_offset_slot(&self, index: u16) -> Result<u16> {
        let back = (index as usize)
            .checked_mul(OFFSET_SIZE)
            .and_then(|m| m.checked_add(OFFSET_SIZE))
            .ok_or(Error::overflow("offset slot address"))?;
        let at = self.node_size.checked_sub(back).ok_or(Error::out_of_range(
            "offset slot address",
            back as u64,
            self.node_size as u64,
        ))?;
        Be::new(self.raw)
            .u16(at)
            .map_err(|_| Error::invalid("btree offset array", "offset array runs past the node"))
    }
}

/// Byte size of the key at the head of a node record, prefix included.
///
/// **Assumes a 16-bit key length.** A HFS+ key's `keyLength` excludes the
/// length field itself, so with a 16-bit prefix the key occupies
/// `keyLength + 2` bytes.
///
/// The format also allows an 8-bit prefix, and Apple branches on it: `CalcKeySize`
/// adds `sizeof(UInt16)` when `kBTBigKeysMask` is set and `sizeof(UInt8)`
/// otherwise. This does not, because every Apple-written tree sets the bit --
/// `newfs_hfs/makehfs.c` ORs it in for the catalog, the extents tree and the
/// attributes tree alike, as do `core/hfs_btreeio.c` and `core/hfs_hotfiles.c` --
/// and a volume without it would be parsed wrongly rather than refused.
///
/// That is a deliberate limitation, not an oversight, and it is reported rather
/// than misparsed: `hfsck` flags a tree that would need the 8-bit form, so a
/// volume in that state is refused with an explanation instead of decoded into
/// nonsense. See [`crate::check`].
fn key_size_on_disk(record: &[u8]) -> Result<usize> {
    if record.len() < 2 {
        return Err(Error::Truncated {
            what: "node record key",
            needed: 2,
            available: record.len(),
        });
    }
    let declared = u16::from_be_bytes([record[0], record[1]]) as usize;
    declared
        .checked_add(2)
        .ok_or_else(|| Error::overflow("node record key length"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic node with `num_records` one-byte records.
    ///
    /// Records start immediately after the node descriptor and grow upward, so
    /// record `i` occupies `[14 + i, 14 + i + 1)` and its offset slot holds
    /// `14 + i`. Slot `i` is stored at `node_size - 2*i - 2`, per
    /// `GetRecordOffset`, so slot values *ascend* with the index even though the
    /// slot *addresses* descend.
    fn node(node_size: usize, num_records: u16) -> Vec<u8> {
        let mut raw = vec![0u8; node_size];
        raw[0..4].copy_from_slice(&0u32.to_be_bytes()); // fLink
        raw[4..8].copy_from_slice(&0u32.to_be_bytes()); // bLink
        raw[8] = NodeKind::Leaf.as_i8() as u8;
        raw[9] = 1; // height
        raw[10..12].copy_from_slice(&num_records.to_be_bytes());
        raw[12..14].copy_from_slice(&0u16.to_be_bytes());

        for i in 0..usize::from(num_records) {
            raw[NODE_DESCRIPTOR_SIZE + i] = i as u8;
        }
        for i in 0..=usize::from(num_records) {
            let slot = node_size - 2 * i - 2;
            raw[slot..slot + 2].copy_from_slice(&((NODE_DESCRIPTOR_SIZE + i) as u16).to_be_bytes());
        }
        raw
    }

    #[test]
    fn parses_a_descriptor() {
        let raw = node(4096, 3);
        let n = Node::parse(&raw, 4096).unwrap();
        assert_eq!(n.kind(), NodeKind::Leaf);
        assert_eq!(n.num_records(), 3);
        assert_eq!(n.descriptor().height, 1);
    }

    #[test]
    fn records_are_addressable_in_order() {
        let raw = node(4096, 3);
        let n = Node::parse(&raw, 4096).unwrap();
        for i in 0..3u16 {
            assert_eq!(n.record(i).unwrap(), &[i as u8], "record {i}");
        }
    }

    #[test]
    fn record_offsets_ascend_as_the_index_grows() {
        // Slot *addresses* descend with the index, but the offsets they hold
        // ascend, because records grow upward. Getting this backwards yields
        // record ranges that end before they start.
        let raw = node(4096, 4);
        let n = Node::parse(&raw, 4096).unwrap();
        let o0 = n.record_offset(0).unwrap();
        let o1 = n.record_offset(1).unwrap();
        let o2 = n.record_offset(2).unwrap();
        assert!(o0 < o1 && o1 < o2, "offsets must ascend with the index");
        assert_eq!(n.record_range(0).unwrap(), o0..o1);
        assert_eq!(o0 - NODE_DESCRIPTOR_SIZE, 0);
    }

    #[test]
    fn out_of_range_indices_are_refused() {
        let raw = node(4096, 2);
        let n = Node::parse(&raw, 4096).unwrap();
        assert!(matches!(n.record(2), Err(Error::OutOfRange { .. })));
        assert!(matches!(n.record(9999), Err(Error::OutOfRange { .. })));
        // record_offset is permissive up to numRecords, because that is exactly
        // what the free-offset computation needs.
        assert!(n.record_offset(2).is_ok());
        assert!(n.record_offset(3).is_err());
    }

    #[test]
    fn an_oversized_record_count_is_rejected() {
        // numRecords far beyond what the offset array can hold.
        let mut raw = node(4096, 1);
        raw[10..12].copy_from_slice(&30000u16.to_be_bytes());
        let err = Node::parse(&raw, 4096).unwrap_err();
        assert!(matches!(
            err,
            Error::InvalidField {
                field: "BTNodeDescriptor.numRecords",
                ..
            }
        ));
    }

    #[test]
    fn an_unknown_node_kind_is_rejected() {
        let mut raw = node(4096, 1);
        raw[8] = 7;
        assert!(matches!(
            Node::parse(&raw, 4096),
            Err(Error::InvalidField {
                field: "BTNodeDescriptor.kind",
                ..
            })
        ));
    }

    #[test]
    fn a_record_pointing_past_the_node_is_rejected() {
        // Give both slots of record 0 an address beyond the node.
        let mut raw = node(4096, 2);
        for i in 0..2 {
            let slot = 4096 - 2 * i - 2;
            raw[slot..slot + 2].copy_from_slice(&60000u16.to_be_bytes());
        }
        let n = Node::parse(&raw, 4096).unwrap();
        assert!(matches!(n.record(0), Err(Error::InvalidField { .. })));
    }

    #[test]
    fn an_inverted_offset_pair_is_rejected() {
        // Record 0 starts above where it ends.
        let mut raw = node(4096, 2);
        let s0 = 4096 - 2; // offset[0]
        let s1 = 4096 - 4; // offset[1]
        let o0 = u16::from_be_bytes([raw[s0], raw[s0 + 1]]);
        let o1 = u16::from_be_bytes([raw[s1], raw[s1 + 1]]);
        raw[s0..s0 + 2].copy_from_slice(&o1.to_be_bytes());
        raw[s1..s1 + 2].copy_from_slice(&o0.to_be_bytes());
        let n = Node::parse(&raw, 4096).unwrap();
        assert!(matches!(n.record(0), Err(Error::InvalidField { .. })));
    }

    #[test]
    fn short_buffers_are_refused_not_panicked_on() {
        for len in [0usize, 1, 13, 15, 100, 4095] {
            let raw = vec![0u8; len];
            assert!(Node::parse(&raw, 4096).is_err(), "length {len}");
        }
    }

    #[test]
    fn a_node_too_small_for_a_descriptor_is_refused() {
        // 15 bytes cannot hold a 14-byte descriptor plus an offset slot.
        let small = vec![0u8; 15];
        assert!(Node::parse(&small, 15).is_err());
        let ok = vec![0u8; 16];
        assert!(Node::parse(&ok, 16).is_ok());
    }

    #[test]
    fn free_space_is_the_gap_before_the_offset_array() {
        // Apple: nodeSize - freeOffset - (numRecords << 1) - kOffsetSize.
        let raw = node(4096, 2);
        let n = Node::parse(&raw, 4096).unwrap();
        let free_offset = n.free_offset().unwrap();
        let expected = 4096usize - free_offset - 2 * 2 - 2;
        assert_eq!(n.free_space().unwrap(), expected);

        // The gap must be exactly what lies between the last record and the
        // first offset slot.
        let first_slot = 4096 - 2 * 3;
        assert_eq!(n.free_space().unwrap(), first_slot - free_offset);
    }

    #[test]
    fn a_node_sized_exactly_to_its_content_has_no_free_space() {
        // 512-byte node, 3 records of 1 byte, anchor just below the offset array.
        let node_size = 512usize;
        let num_records = 3u16;
        let mut raw = vec![0u8; node_size];
        raw[8] = NodeKind::Leaf.as_i8() as u8;
        raw[10..12].copy_from_slice(&num_records.to_be_bytes());
        // Push the records up against the offset array so nothing is free.
        let top = node_size - (usize::from(num_records) + 1) * OFFSET_SIZE;
        let start = top - usize::from(num_records);
        for i in 0..usize::from(num_records) {
            raw[start + i] = i as u8;
        }
        for i in 0..=usize::from(num_records) {
            let slot = node_size - 2 * i - 2;
            raw[slot..slot + 2].copy_from_slice(&((start + i) as u16).to_be_bytes());
        }
        let n = Node::parse(&raw, node_size).unwrap();
        assert_eq!(n.free_space().unwrap(), 0);
    }
}

// --- Mutation -------------------------------------------------------------
//
// The layout here is a fixed fact of the format, verified against real images
// rather than inferred: a node's record offsets live at the *end* of the node,
// with record 0's offset in the last two bytes, record 1's two bytes below that,
// and so on. So slot `i` is at `node_size - 2 * (i + 1)`, and the offset values
// increase with `i` because records are stored in ascending address order.
//
// Mining reference: `GetRecordOffset` and `GetOffsetAddress` in
// `core/BTreeNodeOps.c` are `node + nodeSize - (index << 1) - kOffsetSize`, and
// `kOffsetSize` is 2 (`core/BTreesPrivate.h`).

/// Byte offset of the offset slot for record `index`.
///
/// `index` may equal the record count, which is the free offset — the address one
/// past the last record, and how a node's used length is found.
pub fn offset_slot(node_size: usize, index: usize) -> Option<usize> {
    let at = node_size.checked_sub(index.checked_mul(2)?.checked_add(OFFSET_SIZE)?)?;
    Some(at)
}

/// Read the offset slot for record `index`.
pub fn read_offset(node: &[u8], index: usize) -> Result<usize> {
    let node_size = node.len();
    let at = offset_slot(node_size, index).ok_or(Error::Truncated {
        what: "btree node offset slot",
        needed: index.saturating_mul(2).saturating_add(OFFSET_SIZE),
        available: node_size,
    })?;
    Ok(usize::from(u16::from_be_bytes([
        *node.get(at).ok_or(Error::Truncated {
            what: "btree node offset slot",
            needed: 2,
            available: node_size.saturating_sub(at),
        })?,
        *node.get(at + 1).ok_or(Error::Truncated {
            what: "btree node offset slot",
            needed: 2,
            available: node_size.saturating_sub(at),
        })?,
    ])))
}

/// Set the record count in a node's descriptor.
///
/// The counterpart of [`num_records`], for a mutation. Public because a caller
/// rebuilding a node from a list of records has to set it.
pub fn set_record_count(node: &mut [u8], count: u16) -> Result<()> {
    set_num_records(node, count)
}

/// Write the offset slot for record `index`.
pub fn write_offset(node: &mut [u8], index: usize, value: usize) -> Result<()> {
    let node_size = node.len();
    let at = offset_slot(node_size, index).ok_or(Error::Truncated {
        what: "btree node offset slot",
        needed: index.saturating_mul(2).saturating_add(OFFSET_SIZE),
        available: node_size,
    })?;
    let bytes = u16::try_from(value)
        .map_err(|_| Error::out_of_range("node record offset", value as u64, u16::MAX as u64))?;
    node.get_mut(at..at + OFFSET_SIZE)
        .ok_or(Error::Truncated {
            what: "btree node offset slot",
            needed: OFFSET_SIZE,
            available: node_size.saturating_sub(at),
        })?
        .copy_from_slice(&bytes.to_be_bytes());
    Ok(())
}

/// The record count in a node's descriptor.
///
/// At offset 10: `fLink` 0..4, `bLink` 4..8, `kind` 8, `height` 9,
/// `numRecords` 10..12, `reserved` 12..14.
pub fn num_records(node: &[u8]) -> Result<u16> {
    let at = 10;
    Ok(u16::from_be_bytes([
        *node.get(at).ok_or(Error::Truncated {
            what: "btree node descriptor",
            needed: at + 1,
            available: node.len(),
        })?,
        *node.get(at + 1).ok_or(Error::Truncated {
            what: "btree node descriptor",
            needed: at + 2,
            available: node.len(),
        })?,
    ]))
}

fn set_num_records(node: &mut [u8], count: u16) -> Result<()> {
    let len = node.len();
    node.get_mut(10..12)
        .ok_or(Error::Truncated {
            what: "btree node descriptor",
            needed: 12,
            available: len,
        })?
        .copy_from_slice(&count.to_be_bytes());
    Ok(())
}

/// Unused bytes in a node, as `GetNodeFreeSize` computes it.
///
/// `nodeSize - freeOffset - numRecords * 2 - kOffsetSize`: everything between the
/// end of the last record and the offset array, minus the array itself. The `-2`
/// matters — an insertion needs a slot for the new record's offset as well as
/// room for its bytes, and a node that has room for the bytes but not the slot
/// cannot take the record.
///
/// Mining reference: `GetNodeFreeSize`, `core/BTreeNodeOps.c`.
pub fn free_space(node: &[u8]) -> Result<usize> {
    let node_size = node.len();
    let count = usize::from(num_records(node)?);
    let free_offset = read_offset(node, count)?;
    let used_slots = (count + 1) * OFFSET_SIZE;
    node_size
        .checked_sub(free_offset)
        .and_then(|v| v.checked_sub(used_slots))
        .ok_or(Error::invalid(
            "btree node",
            format!(
                "freeOffset {free_offset} and {count} record(s) need more than the \
                 {node_size}-byte node"
            ),
        ))
}

/// Insert `record` at `index`, shifting later records and their offsets right.
///
/// # The layout, and why the new record goes where the old one was
///
/// Records occupy a contiguous run starting just after the descriptor, in
/// ascending address order, and the offset array at the node's end maps slot `i`
/// to record `i`'s address. Inserting therefore has to open a hole: everything from
/// `index` onwards slides right by `record.len()`, the offsets of those records
/// move with them, and a new offset slot appears for the free offset. The new
/// record lands at the *old* address of record `index`, which is why slot `index`
/// itself does not change — the slot is still correct, it just describes different
/// bytes.
///
/// Afterwards, with `n` the original record count:
///
/// | slot | value |
/// | --- | --- |
/// | `0..index` | unchanged |
/// | `index` | unchanged (the new record took record `index`'s address) |
/// | `index+1 ..= n` | the old slot below it, plus `record.len()` |
/// | `n+1` | the old free offset, plus `record.len()` |
///
/// # Errors
///
/// [`Error::NoSpace`] when the record and its offset slot do not both fit. The
/// node is left untouched in that case, so a caller that splits the node instead
/// has not half-applied anything.
///
/// Mining reference: `InsertRecord` and `InsertKeyRecord` in
/// `core/BTreeNodeOps.c` — `GetNodeFreeSize`, the `MoveRecordsRight`, the
/// `InsertOffset`, then the copy. `InsertKeyRecord` splits key and record; this
/// takes one already-encoded blob, because the caller has the key bytes.
///
/// # A note on the source, unresolved
///
/// `InsertOffset` there writes `numRecords - index` slots, starting at the free
/// slot and walking down. By the layout above, that leaves slot `index + 1`
/// holding the old value rather than the old `index` slot plus `delta`. The layout
/// itself is not in doubt — it was measured against real images, where slot 0 is
/// the last two bytes of the node. So either that loop count is short by one, or
/// `index` reaches it adjusted, and reading it did not settle which. This
/// implementation follows the layout rather than the loop, and
/// `every_record_is_still_findable_after_an_insertion` is the test that decides
/// whether the layout is self-consistent.
pub fn insert_record(node: &mut [u8], index: usize, record: &[u8]) -> Result<()> {
    let node_size = node.len();
    let count = usize::from(num_records(node)?);
    if index > count {
        return Err(Error::out_of_range(
            "btree record index",
            index as u64,
            count as u64,
        ));
    }
    let need = record.len() + OFFSET_SIZE;
    let available = free_space(node)?;
    if available < need {
        return Err(Error::no_space(need as u32, available as u64));
    }

    let at = read_offset(node, index)?;
    let free_at = read_offset(node, count)?;
    let moved = free_at - at;

    // Slide the tail right, from the back so the overlap is never read as stale.
    //
    // `at + record.len() + moved` must not exceed the free offset's new position:
    // the node's used region grows by exactly `record.len()`, and `free_space`
    // already proved the offset array has room for one more slot.
    let end = at + record.len() + moved;
    if end > node_size - (count + 2) * OFFSET_SIZE {
        return Err(Error::no_space(need as u32, available as u64));
    }
    node.copy_within(at..free_at, at + record.len());

    // Offsets. Walk from the last one down so each read is still the old value.
    for i in (index + 1..=count + 1).rev() {
        let below = read_offset(node, i - 1)?;
        write_offset(node, i, below + record.len())?;
    }
    set_num_records(node, count as u16 + 1)?;
    node.get_mut(at..at + record.len())
        .ok_or(Error::Truncated {
            what: "btree node record area",
            needed: record.len(),
            available: node_size.saturating_sub(at),
        })?
        .copy_from_slice(record);
    Ok(())
}

/// Remove record `index`, sliding later records and their offsets left.
///
/// The inverse of [`insert_record`], and it leaves the freed bytes in place rather
/// than clearing them: the next insertion overwrites them, and a node's unused
/// tail is not interpreted by anything.
///
/// Mining reference: `DeleteRecord` and `DeleteOffset` in `core/BTreeNodeOps.c`.
pub fn remove_record(node: &mut [u8], index: usize) -> Result<()> {
    let count = usize::from(num_records(node)?);
    if index >= count {
        return Err(Error::out_of_range(
            "btree record index",
            index as u64,
            count.saturating_sub(1) as u64,
        ));
    }
    let at = read_offset(node, index)?;
    let next = read_offset(node, index + 1)?;
    let free_at = read_offset(node, count)?;
    let size = next - at;
    node.copy_within(next..free_at, at);

    for i in index..count {
        let below = read_offset(node, i + 1)?;
        write_offset(node, i, below - size)?;
    }
    // The new free offset is the last real record's offset, or 14 when the node is
    // left empty -- the first free byte after the descriptor.
    let new_free = if count == 1 {
        NODE_DESCRIPTOR_SIZE
    } else {
        read_offset(node, count - 1)?
    };
    write_offset(node, count - 1, new_free)?;
    set_num_records(node, count as u16 - 1)?;
    Ok(())
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    /// A node of `node_size` bytes with `count` records of `rec_size` each,
    /// written the way a real one is: descriptor, then records in ascending
    /// address order, then the offset array from the end.
    fn build(node_size: usize, rec_size: usize, count: usize) -> Vec<u8> {
        let mut node = vec![0u8; node_size];
        let base = NODE_DESCRIPTOR_SIZE;
        for i in 0..count {
            let at = base + i * rec_size;
            node[at..at + rec_size].fill(0xA0u8.wrapping_add(i as u8));
            write_offset(&mut node, i, at).expect("offset");
        }
        let free = if count == 0 {
            base
        } else {
            base + count * rec_size
        };
        write_offset(&mut node, count, free).expect("free offset");
        // Leaf, height 1. Written after the offsets so nothing clobbers them:
        // `numRecords` lives at 10, which is inside the region a careless helper
        // would have used for the record area.
        node[8] = 0xFF; // kBTLeafNode
        node[9] = 1;
        set_num_records(&mut node, count as u16).expect("count");
        node
    }

    fn record_at(node: &[u8], i: usize) -> Vec<u8> {
        let at = read_offset(node, i).expect("offset");
        let end = read_offset(node, i + 1).expect("next offset");
        node[at..end].to_vec()
    }

    /// The property the whole module rests on: after an insertion, every record
    /// that was there is still findable, at its new offset, with its bytes
    /// intact -- and a search over the node finds them all.
    ///
    /// This is the test that decides whether the layout in the docs is
    /// self-consistent, rather than whether it matches a particular loop in
    /// Apple's source. Every other mutation test here is a special case of it.
    #[test]
    fn every_record_is_still_findable_after_an_insertion() {
        let node_size = 512;
        let rec_size = 20;
        let before: Vec<Vec<u8>> = (0..6)
            .map(|i| vec![0xA0u8.wrapping_add(i as u8); rec_size])
            .collect();

        // Insert at every position, including the end and the beginning.
        for index in 0..=6usize {
            let mut node = build(node_size, rec_size, 6);
            let inserted = vec![0xEEu8; rec_size];
            insert_record(&mut node, index, &inserted).expect("insert");

            assert_eq!(
                num_records(&node).expect("count"),
                7,
                "inserting at {index} must leave seven records"
            );

            // Every original record survives, in order, with the new one in the
            // right place.
            for (i, want) in before.iter().enumerate() {
                let got_index = if i < index { i } else { i + 1 };
                assert_eq!(
                    &record_at(&node, got_index),
                    want,
                    "inserting at {index}: original record {i} moved or changed"
                );
            }
            assert_eq!(
                &record_at(&node, index),
                &inserted,
                "inserting at {index}: the new record is not where it was asked for"
            );

            // The free offset accounts for every byte, and free space agrees.
            let used_end = read_offset(&node, 7).expect("free offset");
            assert_eq!(
                used_end,
                NODE_DESCRIPTOR_SIZE + 7 * rec_size,
                "inserting at {index}: the used region must grow by exactly one record"
            );
            assert_eq!(
                free_space(&node).expect("free space"),
                node_size - used_end - 8 * OFFSET_SIZE,
                "inserting at {index}: free space must account for the new offset slot"
            );
        }
    }

    #[test]
    fn a_node_with_no_records_starts_its_first_record_after_the_descriptor() {
        // The boundary: with no records there is no slot 0 to copy an offset from,
        // so the used region starts at the descriptor's end. Getting this wrong
        // writes a record over the descriptor, which still parses as a node.
        let mut node = build(512, 20, 0);
        insert_record(&mut node, 0, &[7u8; 20]).expect("insert");
        assert_eq!(read_offset(&node, 0).expect("offset"), NODE_DESCRIPTOR_SIZE);
        assert_eq!(&record_at(&node, 0), &vec![7u8; 20]);
    }

    #[test]
    fn a_record_that_does_not_fit_is_refused_and_changes_nothing() {
        let mut node = build(512, 20, 4);
        let snapshot = node.clone();
        // Free space here is 512 - 94 (four records and a descriptor) - 10 (five
        // offset slots) = 408, so 500 bytes cannot fit and 400 easily could.
        let err = insert_record(&mut node, 2, &[0u8; 500]).expect_err("500 bytes will not fit");
        assert!(
            matches!(err, Error::NoSpace { .. }),
            "a record that does not fit is out of space, not a corrupt node; got {err:?}"
        );
        assert_eq!(
            node, snapshot,
            "a refused insert must not have moved anything"
        );
    }

    /// The `+2` is the new offset slot. A node with room for the bytes but not the
    /// slot cannot take the record, and accepting it would write over the offset
    /// array -- losing every record after the insertion point.
    #[test]
    fn the_new_offset_slot_is_charged_against_free_space() {
        let node_size = 512;
        let mut node = build(node_size, 40, 5);
        let free = free_space(&node).expect("free space");
        // Exactly free bytes fit a record of that length *plus* no slot.
        insert_record(&mut node, 5, &vec![1u8; free - OFFSET_SIZE]).expect("exact fit");
        assert_eq!(free_space(&node).expect("after"), 0);
        assert!(insert_record(&mut node, 6, &[2u8; 2]).is_err(), "now full");
    }

    #[test]
    fn removal_is_the_inverse_of_insertion() {
        let node_size = 512;
        let rec_size = 20;
        let original: Vec<Vec<u8>> = (0..5)
            .map(|i| vec![0xA0u8.wrapping_add(i as u8); rec_size])
            .collect();

        for index in 0..5usize {
            let mut node = build(node_size, rec_size, 5);
            // Push it to the back first so removal has both a tail and a middle.
            insert_record(&mut node, index, &vec![0xEEu8; rec_size]).expect("insert");
            remove_record(&mut node, index).expect("remove");

            assert_eq!(num_records(&node).expect("count"), 5);
            for (i, want) in original.iter().enumerate() {
                assert_eq!(
                    &record_at(&node, i),
                    want,
                    "removing index {index}: record {i} did not come back intact"
                );
            }
            assert_eq!(
                read_offset(&node, 5).expect("free offset"),
                NODE_DESCRIPTOR_SIZE + 5 * rec_size,
                "removing index {index}: the used region must shrink by exactly one record"
            );
        }
    }

    #[test]
    fn removing_the_only_record_leaves_the_region_after_the_descriptor() {
        let mut node = build(512, 20, 1);
        remove_record(&mut node, 0).expect("remove");
        assert_eq!(num_records(&node).expect("count"), 0);
        assert_eq!(read_offset(&node, 0).expect("offset"), NODE_DESCRIPTOR_SIZE);
        // And a node that has been emptied can take a record again.
        insert_record(&mut node, 0, &[3u8; 20]).expect("insert");
        assert_eq!(&record_at(&node, 0), &vec![3u8; 20]);
    }

    /// A node whose descriptor and offsets are truncated must produce an error
    /// rather than a panic or a read past the end. `Node` parses defensively;
    /// these functions have to be as careful.
    #[test]
    fn a_truncated_node_is_an_error_rather_than_a_panic() {
        assert!(read_offset(&[], 0).is_err());
        assert!(
            read_offset(&[0u8; 1], 0).is_err(),
            "one byte cannot hold a slot"
        );
        assert!(read_offset(&[0u8; 3], 1).is_err(), "index 1 needs four");
        assert!(num_records(&[0u8; 11]).is_err());
        assert!(set_num_records(&mut [0u8; 5], 1).is_err());
        assert!(insert_record(&mut [0u8; 8], 0, &[1, 2]).is_err());
        assert!(remove_record(&mut [0u8; 8], 0).is_err());
    }

    #[test]
    fn an_out_of_range_index_is_refused() {
        let mut node = build(512, 20, 3);
        assert!(
            insert_record(&mut node, 4, &[1u8; 20]).is_err(),
            "past the count"
        );
        assert!(remove_record(&mut node, 3).is_err(), "past the last record");
    }
}
