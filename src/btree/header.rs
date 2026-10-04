//! The B-tree header record and Apple's header validation rules.
//!
//! Mining reference: Apple `core/hfs_format.h` (`struct BTHeaderRec`), the
//! endian swap in `core/hfs_endian.c`, and — most importantly — the validation in
//! `core/BTreeMiscOps.c` (`VerifyHeader`), which decides whether a fork even
//! looks like a B-tree before anything else is trusted.

use crate::endian::Be;
use crate::error::{Error, Result};
use crate::format::fork::ForkData;

/// Byte size of an on-disk `BTHeaderRec`.
pub const HEADER_RECORD_SIZE: usize = 106;

/// Byte offset of the header record within a header node: immediately after the
/// 14-byte node descriptor.
///
/// Mining reference: Apple reads it as
/// `(BTHeaderRec *)((uintptr_t)nodeRec.buffer + sizeof(BTNodeDescriptor))` in
/// `core/BTree.c` `BTOpenPath`, and in `core/hfs_btreeio.c`
/// `GetBTreeBlock`, which writes 14 rather than `sizeof(...)` deliberately.
pub const HEADER_RECORD_OFFSET: usize = 14;

/// Byte offset of `leafRecords` within the header record.
///
/// Named because a writer needs it and 6 is not guessable: the fields are packed
/// with no alignment padding -- `treeDepth` is a `u16`, so `rootNode` starts at 2
/// and `leafRecords` at 6 -- and writing at 8 instead lands in `firstLeafNode`,
/// which then reads as a node number no volume has.
pub const LEAF_RECORDS_OFFSET: u64 = 6;

/// Byte offset of `firstLeafNode` within the header record.
pub const FIRST_LEAF_OFFSET: u64 = 10;

/// `kBTreeHeaderUserBytes`: the user data area that follows the header record.
///
/// Mining reference: Apple `core/BTreesInternal.h` `#define kBTreeHeaderUserBytes 128`.
pub const HEADER_USER_BYTES: usize = 128;

/// Whether a node size is one of the seven values Apple's `VerifyHeader`
/// accepts: 512 through 32768 in powers of two.
const fn is_legal_node_size(size: u16) -> bool {
    matches!(size, 512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768)
}

/// Catalog key comparison: case folding, i.e. case-insensitive.
pub const K_HFS_CASE_FOLDING: u8 = 0xCF;

/// Binary comparison, i.e. case-sensitive.
pub const K_HFS_BINARY_COMPARE: u8 = 0xBC;

/// `kUserBTreeType`: user B-trees start at 128.
const K_USER_BTREE_TYPE: u8 = 128;

/// `kReservedBTreeType`.
const K_RESERVED_BTREE_TYPE: u8 = 255;

/// `kMaxTreeDepth` from `core/BTreesPrivate.h`.
const K_MAX_TREE_DEPTH: u16 = 16;

/// Which key comparison a tree uses.
///
/// Mining reference: `core/hfs_format.h` names the two values
/// `kHFSCaseFolding = 0xCF` and `kHFSBinaryCompare = 0xBC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCompareType {
    /// `kHFSCaseFolding`: names compare case-insensitively, using the HFS
    /// decomposition rules rather than Unicode case folding alone.
    ///
    /// Mining reference: `core/UnicodeWrappers.c` `FastUnicodeCompare` takes a
    /// case-folding flag sourced from this value; `core/hfs_catalog.c`
    /// (`cat_binarykeycompare`) dispatches on it.
    CaseFolding,
    /// `kHFSBinaryCompare`: names compare as a plain binary string.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_binarykeycompare` calls
    /// `FastRelString(..., false)` for this case, i.e. no case folding.
    BinaryCompare,
    /// A value this crate does not recognise, preserved verbatim.
    Unknown(u8),
}

impl KeyCompareType {
    /// Decode from the on-disk byte.
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            K_HFS_CASE_FOLDING => KeyCompareType::CaseFolding,
            K_HFS_BINARY_COMPARE => KeyCompareType::BinaryCompare,
            other => KeyCompareType::Unknown(other),
        }
    }

    /// Whether names on this tree compare case-sensitively.
    ///
    /// This is authoritative for HFSX, where the volume signature alone does not
    /// say: the catalog B-tree's own `keyCompareType` decides.
    pub const fn is_case_sensitive(self) -> bool {
        matches!(self, KeyCompareType::BinaryCompare)
    }

    /// The raw on-disk byte.
    pub const fn code(self) -> u8 {
        match self {
            KeyCompareType::CaseFolding => K_HFS_CASE_FOLDING,
            KeyCompareType::BinaryCompare => K_HFS_BINARY_COMPARE,
            KeyCompareType::Unknown(raw) => raw,
        }
    }
}

/// The parsed B-tree header record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BTreeHeader {
    /// Height of the tree: 1 for a tree that is only a root leaf.
    pub tree_depth: u16,
    /// Node number of the root.
    pub root_node: u32,
    /// Total records across all leaf nodes.
    pub leaf_records: u32,
    /// First leaf node, for sequential iteration.
    pub first_leaf_node: u32,
    /// Last leaf node, for reverse sequential iteration.
    pub last_leaf_node: u32,
    /// Size of each node in bytes.
    pub node_size: u16,
    /// Longest key body the tree accepts.
    pub max_key_length: u16,
    /// Node numbers in the tree, including free ones.
    pub total_nodes: u32,
    /// How many of those are free.
    pub free_nodes: u32,
    /// Reserved, initialised to zero.
    pub reserved1: u16,
    /// Reserved, initialised to zero.
    pub clump_size: u32,
    /// Tree type; 0 for HFS, 128.. for user trees.
    pub btree_type: u8,
    /// Key comparison rule.
    pub key_compare_type: KeyCompareType,
    /// Persistent tree attributes.
    pub attributes: u32,
}

impl BTreeHeader {
    /// Parse a `BTHeaderRec` from the 106 bytes at a node's offset 14.
    ///
    /// Field order and widths follow `core/hfs_endian.c`'s swap routine for
    /// `BTHeaderRec`.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_RECORD_SIZE {
            return Err(Error::Truncated {
                what: "btree header record",
                needed: HEADER_RECORD_SIZE,
                available: bytes.len(),
            });
        }
        let be = Be::new(bytes);
        Ok(BTreeHeader {
            tree_depth: be.u16(0)?,
            root_node: be.u32(2)?,
            leaf_records: be.u32(6)?,
            first_leaf_node: be.u32(10)?,
            last_leaf_node: be.u32(14)?,
            node_size: be.u16(18)?,
            max_key_length: be.u16(20)?,
            total_nodes: be.u32(22)?,
            free_nodes: be.u32(26)?,
            reserved1: be.u16(30)?,
            clump_size: be.u32(32)?,
            btree_type: be.u8(36)?,
            key_compare_type: KeyCompareType::from_u8(be.u8(37)?),
            attributes: be.u32(38)?,
        })
    }

    /// The parsed bytes begin at `HEADER_RECORD_OFFSET` in `node`.
    pub fn from_node(node: &[u8]) -> Result<Self> {
        let start = HEADER_RECORD_OFFSET;
        let end = start + HEADER_RECORD_SIZE;
        Self::parse(node.get(start..end).ok_or(Error::Truncated {
            what: "btree header record",
            needed: end,
            available: node.len(),
        })?)
    }

    /// Bytes occupied by the whole tree, `totalNodes * nodeSize`.
    pub fn tree_bytes(&self) -> Option<u64> {
        u64::from(self.total_nodes).checked_mul(u64::from(self.node_size))
    }

    /// Whether this tree's keys carry a 16-bit length prefix.
    ///
    /// Mining reference: Apple `core/BTree.c` `BTOpenPath` ORs the big-keys
    /// flags in whenever `maxKeyLength > 40`, so that a wrong stored attribute
    /// cannot desynchronise key parsing.
    pub fn has_big_keys(&self) -> bool {
        crate::btree::key::has_big_keys(self.max_key_length, self.attributes)
    }

    /// Whether names compare case-sensitively.
    pub fn is_case_sensitive(&self) -> bool {
        self.key_compare_type.is_case_sensitive()
    }

    /// Apple's `VerifyHeader`, translated.
    ///
    /// Mining reference: Apple `core/BTreeMiscOps.c` `VerifyHeader`, which
    /// rejects a header unless the node size is one of seven fixed values, the
    /// tree fits inside its fork, every node number is in range, the tree is not
    /// entirely free, the depth is plausible, and the tree type is known.
    ///
    /// `fork` supplies the fork's logical size, which Apple reads as `fcbEOF`.
    /// `hfs_plus` selects the extra HFS+ rule that a 512-byte node size belongs
    /// to classic HFS only.
    pub fn validate(&self, fork: &ForkData, hfs_plus: bool) -> Result<()> {
        if !is_legal_node_size(self.node_size) {
            return Err(Error::invalid(
                "BTHeaderRec.nodeSize",
                format!(
                    "{} is not one of 512..32768 by powers of two",
                    self.node_size
                ),
            ));
        }
        if hfs_plus && self.node_size == 512 {
            return Err(Error::invalid(
                "BTHeaderRec.nodeSize",
                "512-byte nodes are classic HFS only, not HFS+",
            ));
        }

        let tree_bytes = self
            .tree_bytes()
            .ok_or(Error::overflow("btree tree_bytes"))?;
        if tree_bytes > fork.logical_size {
            return Err(Error::out_of_range(
                "btree tree size",
                tree_bytes,
                fork.logical_size,
            ));
        }

        let total = u64::from(self.total_nodes);
        for (field, value) in [
            ("freeNodes", self.free_nodes),
            ("rootNode", self.root_node),
            ("firstLeafNode", self.first_leaf_node),
            ("lastLeafNode", self.last_leaf_node),
        ] {
            if u64::from(value) >= total {
                return Err(Error::invalid(
                    "BTHeaderRec",
                    format!("{field} {value} is not below totalNodes {total}"),
                ));
            }
        }

        if self.tree_depth > K_MAX_TREE_DEPTH {
            return Err(Error::invalid(
                "BTHeaderRec.treeDepth",
                format!(
                    "{} exceeds kMaxTreeDepth {K_MAX_TREE_DEPTH}",
                    self.tree_depth
                ),
            ));
        }

        match self.btree_type {
            0 | K_USER_BTREE_TYPE | K_RESERVED_BTREE_TYPE => Ok(()),
            other => Err(Error::invalid(
                "BTHeaderRec.btreeType",
                format!("unknown tree type {other}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{K_BT_BIG_KEYS_MASK, K_BT_VARIABLE_INDEX_KEYS_MASK};

    fn header(node_size: u16, total_nodes: u32) -> BTreeHeader {
        BTreeHeader {
            tree_depth: 1,
            root_node: 0,
            leaf_records: 0,
            first_leaf_node: 0,
            last_leaf_node: 0,
            node_size,
            max_key_length: 516,
            total_nodes,
            free_nodes: 0,
            reserved1: 0,
            clump_size: 0,
            btree_type: 0,
            key_compare_type: KeyCompareType::CaseFolding,
            attributes: K_BT_VARIABLE_INDEX_KEYS_MASK | K_BT_BIG_KEYS_MASK,
        }
    }

    fn fork(size: u64) -> ForkData {
        ForkData {
            logical_size: size,
            ..ForkData::EMPTY
        }
    }

    #[test]
    fn header_record_is_106_bytes_at_offset_14() {
        assert_eq!(HEADER_RECORD_SIZE, 106);
        assert_eq!(HEADER_RECORD_OFFSET, 14);
        assert_eq!(HEADER_RECORD_OFFSET + HEADER_RECORD_SIZE, 120);
    }

    #[test]
    fn accepts_every_legal_node_size_on_hfsplus() {
        // 512 is excluded here and covered by rejects_512_byte_nodes_on_hfsplus.
        for size in [1024u16, 2048, 4096, 8192, 16384, 32768] {
            let h = header(size, 16);
            assert!(h.validate(&fork(1 << 20), true).is_ok(), "node size {size}");
        }
    }

    #[test]
    fn accepts_512_byte_nodes_on_classic_hfs() {
        let h = header(512, 16);
        assert!(h.validate(&fork(1 << 20), false).is_ok());
    }

    #[test]
    fn rejects_illegal_node_sizes() {
        for size in [0u16, 1, 256, 511, 513, 3000, 65535] {
            let h = header(size, 16);
            assert!(
                matches!(
                    h.validate(&fork(1 << 20), true),
                    Err(Error::InvalidField { .. })
                ),
                "node size {size} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_512_byte_nodes_on_hfsplus() {
        // Classic HFS allows it; HFS+ does not. Apple asserts this explicitly.
        let h = header(512, 16);
        assert!(h.validate(&fork(1 << 20), true).is_err());
        assert!(h.validate(&fork(1 << 20), false).is_ok());
    }

    #[test]
    fn rejects_a_tree_larger_than_its_fork() {
        let h = header(4096, 1_000_000);
        let err = h.validate(&fork(1024 * 1024), true).unwrap_err();
        assert!(matches!(err, Error::OutOfRange { .. }));
    }

    #[test]
    fn rejects_node_numbers_at_or_above_total_nodes() {
        let mut h = header(4096, 16);
        assert!(h.validate(&fork(1 << 20), true).is_ok());

        h.root_node = 16;
        assert!(h.validate(&fork(1 << 20), true).is_err());
        h.root_node = 0;
        h.free_nodes = 16;
        assert!(h.validate(&fork(1 << 20), true).is_err());
        h.free_nodes = 15;
        assert!(h.validate(&fork(1 << 20), true).is_ok());
        h.free_nodes = 0;
        h.last_leaf_node = 99;
        assert!(h.validate(&fork(1 << 20), true).is_err());
    }

    #[test]
    fn rejects_implausible_depth() {
        let mut h = header(4096, 16);
        h.tree_depth = 17;
        assert!(h.validate(&fork(1 << 20), true).is_err());
        h.tree_depth = 16;
        assert!(h.validate(&fork(1 << 20), true).is_ok());
    }

    #[test]
    fn rejects_unknown_tree_type() {
        let mut h = header(4096, 16);
        h.btree_type = 5;
        assert!(h.validate(&fork(1 << 20), true).is_err());
        h.btree_type = 0;
        assert!(h.validate(&fork(1 << 20), true).is_ok());
        h.btree_type = 255;
        assert!(h.validate(&fork(1 << 20), true).is_ok());
    }

    #[test]
    fn key_compare_type_drives_case_sensitivity() {
        assert_eq!(KeyCompareType::from_u8(0xCF), KeyCompareType::CaseFolding);
        assert_eq!(KeyCompareType::from_u8(0xBC), KeyCompareType::BinaryCompare);
        assert!(!KeyCompareType::from_u8(0xCF).is_case_sensitive());
        assert!(KeyCompareType::from_u8(0xBC).is_case_sensitive());
        assert_eq!(KeyCompareType::from_u8(7), KeyCompareType::Unknown(7));
    }

    #[test]
    fn big_keys_are_derived_from_max_key_length() {
        let mut h = header(4096, 16);
        h.attributes = 0;
        h.max_key_length = 516;
        assert!(h.has_big_keys(), "catalog keys are far above 40 bytes");
        h.max_key_length = 8;
        assert!(!h.has_big_keys(), "8-byte keys do not need a 16-bit prefix");
        // The stored bit also counts, independently of maxKeyLength.
        h.attributes = K_BT_BIG_KEYS_MASK;
        assert!(h.has_big_keys());
    }

    #[test]
    fn parses_a_synthetic_header_record() {
        let mut raw = vec![0u8; HEADER_RECORD_SIZE];
        raw[0..2].copy_from_slice(&1u16.to_be_bytes()); // treeDepth
        raw[2..6].copy_from_slice(&3u32.to_be_bytes()); // rootNode
        raw[6..10].copy_from_slice(&7u32.to_be_bytes()); // leafRecords
        raw[18..20].copy_from_slice(&4096u16.to_be_bytes()); // nodeSize
        raw[20..22].copy_from_slice(&516u16.to_be_bytes()); // maxKeyLength
        raw[22..26].copy_from_slice(&16u32.to_be_bytes()); // totalNodes
        raw[26..30].copy_from_slice(&4u32.to_be_bytes()); // freeNodes
        raw[37] = 0xBC; // keyCompareType

        let h = BTreeHeader::parse(&raw).unwrap();
        assert_eq!(h.tree_depth, 1);
        assert_eq!(h.root_node, 3);
        assert_eq!(h.leaf_records, 7);
        assert_eq!(h.node_size, 4096);
        assert_eq!(h.max_key_length, 516);
        assert_eq!(h.total_nodes, 16);
        assert_eq!(h.free_nodes, 4);
        assert!(h.is_case_sensitive());
    }

    #[test]
    fn short_header_records_are_refused() {
        for len in [0usize, 1, 50, 105] {
            assert!(BTreeHeader::parse(&vec![0u8; len]).is_err(), "len {len}");
        }
    }
}
