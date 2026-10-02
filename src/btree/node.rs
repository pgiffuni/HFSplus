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
        Node { raw, node_size, desc }.validate()?;
        Ok(Node { raw, node_size, desc })
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
        let at = self
            .node_size
            .checked_sub(back)
            .ok_or(Error::out_of_range("offset slot address", back as u64, self.node_size as u64))?;
        Be::new(self.raw)
            .u16(at)
            .map_err(|_| Error::invalid("btree offset array", "offset array runs past the node"))
    }
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
            raw[slot..slot + 2]
                .copy_from_slice(&((NODE_DESCRIPTOR_SIZE + i) as u16).to_be_bytes());
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
        assert!(matches!(err, Error::InvalidField { field: "BTNodeDescriptor.numRecords", .. }));
    }

    #[test]
    fn an_unknown_node_kind_is_rejected() {
        let mut raw = node(4096, 1);
        raw[8] = 7;
        assert!(matches!(
            Node::parse(&raw, 4096),
            Err(Error::InvalidField { field: "BTNodeDescriptor.kind", .. })
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
        let s0 = 4096 - 2;   // offset[0]
        let s1 = 4096 - 4;   // offset[1]
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