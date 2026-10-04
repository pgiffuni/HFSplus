//! B-tree I/O: turning a fork into addressable nodes.
//!
//! # The pipeline
//!
//! ```text
//! node number
//!     -> byte offset in the fork   (node_number * node_size)
//!     -> fork allocation block     (byte_offset / block_size)
//!     -> physical allocation block  (ExtentMapper)
//!     -> device byte offset         (ExtentMapper * block_size)
//! ```
//!
//! The multiply is trivial but the rest is not, and assuming a B-tree is
//! physically contiguous is the mistake this type exists to prevent. Apple's
//! `core/hfs_btreeio.c` `GetBTreeBlock` does the same `blockNum * blockSize`
//! and then hands the result to `buf_meta_bread`, which performs the extent
//! mapping.
//!
//! # Opening a tree
//!
//! There is a chicken-and-egg problem: to find node `N` you need `nodeSize`,
//! and `nodeSize` is stored *inside* node 0. Apple resolves it by reading node 0
//! at the device's logical block size, parsing the header record at offset 14,
//! and re-reading at the declared size if the two disagree:
//!
//! ```c
//! if ( btreePtr->nodeSize != nodeRec.blockSize ) {
//!     err = SetBTreeBlockSize(..., btreePtr->nodeSize, 32);
//!     ReleaseBTreeBlock(..., kTrashBlock);
//!     GetNode (btreePtr, kHeaderNodeNum, 0, &nodeRec);
//! }
//! ```
//!
//! Mining reference: Apple `core/BTree.c` `BTOpenPath`. [`BTreeFile::open`]
//! performs the same sequence.

use crate::blockdev::BlockDevice;
use crate::btree::header::BTreeHeader;
use crate::btree::node::Node;
use crate::error::{Error, Result};
use crate::extent::ExtentMapper;
use crate::format::fork::ForkData;

/// Node number of the header node, which is always zero.
///
/// Mining reference: Apple `core/BTreesInternal.h` `#define kHeaderNodeNum 0`.
pub const HEADER_NODE_NUM: u32 = 0;

/// A read-only B-tree file: a fork plus the extent mapping that resolves it.
pub struct BTreeFile<'a, D: ?Sized> {
    device: &'a D,
    fork: ForkData,
    mapper: ExtentMapper<'a>,
    header: BTreeHeader,
}

impl<'a, D: BlockDevice + ?Sized> std::fmt::Debug for BTreeFile<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BTreeFile")
            .field("node_size", &self.header.node_size)
            .field("total_nodes", &self.header.total_nodes)
            .field("root_node", &self.header.root_node)
            .field("tree_depth", &self.header.tree_depth)
            .field("key_compare_type", &self.header.key_compare_type)
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> BTreeFile<'a, D> {
    /// Open a B-tree stored in `fork`.
    ///
    /// `block_size` is the volume's allocation block size, which the fork's
    /// byte offsets are expressed in. `hfs_plus` selects Apple's rule that a
    /// 512-byte node size belongs to classic HFS.
    pub fn open(device: &'a D, fork: &ForkData, block_size: u32, hfs_plus: bool) -> Result<Self> {
        let mapper = ExtentMapper::new(fork, block_size);
        let provisional = provisional_node_size(fork, block_size)?;
        let header = read_header(device, &mapper, provisional)?;

        // Apple's re-read rule: if the declared node size differs from the size
        // we used to read the header, everything must be re-read at the real
        // size, including this tree's own header validation.
        header.validate(fork, hfs_plus)?;

        if provisional != header.node_size {
            // Drop the provisional parse and redo it properly.
            let header = read_header(device, &mapper, header.node_size)?;
            header.validate(fork, hfs_plus)?;
            return Ok(BTreeFile {
                device,
                fork: *fork,
                mapper,
                header,
            });
        }

        Ok(BTreeFile {
            device,
            fork: *fork,
            mapper,
            header,
        })
    }

    /// The tree's header record.
    pub fn header(&self) -> &BTreeHeader {
        &self.header
    }

    /// Node size in bytes.
    pub fn node_size(&self) -> usize {
        usize::from(self.header.node_size)
    }

    /// The extent mapper backing this tree.
    pub fn mapper(&self) -> &ExtentMapper<'_> {
        &self.mapper
    }

    /// Whether this tree's keys carry a 16-bit length prefix.
    pub fn has_big_keys(&self) -> bool {
        self.header.has_big_keys()
    }

    /// Whether names on this tree compare case-sensitively.
    pub fn is_case_sensitive(&self) -> bool {
        self.header.is_case_sensitive()
    }

    /// Read node `node_num` into `buf`, which must be exactly `node_size` long.
    ///
    /// Mining reference: Apple `core/BTreeNodeOps.c` `GetNode` bounds-checks
    /// `nodeNum >= btreePtr->totalNodes` and returns `fsBTInvalidNodeErr`.
    pub fn read_node_into(&self, node_num: u32, buf: &mut [u8]) -> Result<()> {
        if node_num >= self.header.total_nodes {
            return Err(Error::invalid(
                "btree node number",
                format!(
                    "{node_num} is not below totalNodes {}",
                    self.header.total_nodes
                ),
            ));
        }
        let size = self.node_size();
        if buf.len() != size {
            return Err(Error::invalid(
                "btree node buffer",
                format!("expected {size} bytes, got {}", buf.len()),
            ));
        }
        // node_offset splits the byte offset by the volume's allocation block
        // size, not by the node size. The two are equal on a default 4096-byte
        // volume and differ on almost every other volume, so the split must live
        // in exactly one place.
        let device_offset = self.node_offset(node_num)?;
        self.device.read_at(device_offset, buf)
    }

    /// Read node `node_num` and return its raw bytes, so that the caller can own
    /// the buffer and hand it to [`BTreeFile::parse_node`].
    ///
    /// Mining reference: Apple `core/BTreeNodeOps.c` `GetNode` bounds-checks
    /// `nodeNum >= btreePtr->totalNodes` and returns `fsBTInvalidNodeErr`.
    pub fn read_node_bytes(&self, node_num: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.node_size()];
        self.read_node_into(node_num, &mut buf)?;
        Ok(buf)
    }

    /// Read and parse node `node_num`, borrowing `bytes` as the node buffer.
    ///
    /// A [`Node`] borrows its buffer, so it cannot be returned together with a
    /// buffer allocated inside this call. The idiomatic use is therefore:
    ///
    /// ```
    /// # use hfsplus::btree::io::BTreeFile;
    /// # fn demo<D: hfsplus::blockdev::BlockDevice + ?Sized>(
    /// #     bt: &BTreeFile<'_, D>,
    /// # ) -> hfsplus::error::Result<()> {
    /// let bytes = bt.read_node_bytes(0)?;
    /// let node = bt.parse_node(&bytes)?;
    /// # let _ = node.num_records();
    /// # Ok(())
    /// # }
    /// ```
    pub fn parse_node<'b>(&self, bytes: &'b [u8]) -> Result<Node<'b>> {
        Node::parse(bytes, self.node_size())
    }

    /// The device byte offset at which node `node_num` begins.
    pub fn node_offset(&self, node_num: u32) -> Result<u64> {
        if node_num >= self.header.total_nodes {
            return Err(Error::invalid(
                "btree node number",
                format!(
                    "{node_num} is not below totalNodes {}",
                    self.header.total_nodes
                ),
            ));
        }
        // Node numbers are contiguous and a fixed size apart, so the byte
        // offset within the fork is a single multiply. It is then split by the
        // volume's ALLOCATION block size -- not by the node size, which differs
        // from it on nearly every volume.
        let byte_offset = u64::from(node_num)
            .checked_mul(u64::from(self.header.node_size))
            .ok_or(Error::overflow("btree node offset"))?;
        let fs_block = u64::from(self.mapper.block_size());
        let block = byte_offset / fs_block;
        let within = byte_offset % fs_block;
        self.mapper.map_to_device_offset(
            u32::try_from(block).map_err(|_| Error::overflow("node block"))?,
            within,
        )
    }

    /// The fork this tree occupies.
    pub fn fork(&self) -> &ForkData {
        &self.fork
    }
}

/// Read node 0 at the provisional node size and parse its header record.
fn read_header<D: BlockDevice + ?Sized>(
    device: &D,
    mapper: &ExtentMapper<'_>,
    node_size: u16,
) -> Result<BTreeHeader> {
    let size = usize::from(node_size);
    let device_offset = mapper.map_to_device_offset(HEADER_NODE_NUM, 0)?;
    let mut buf = vec![0u8; size];
    device.read_at(device_offset, &mut buf)?;
    BTreeHeader::from_node(&buf)
}

/// Choose a node size large enough to cover a header record.
///
/// Apple's first read uses the *device's* logical block size, which is always at
/// least 512 and, for a valid HFS+ tree, larger than the 120 bytes a header
/// record needs. We mirror that by using the volume's allocation block size and
/// clamping to the legal node sizes.
fn provisional_node_size(fork: &ForkData, block_size: u32) -> Result<u16> {
    // A header record ends at offset 120; the smallest legal node that holds it
    // is 512. Prefer the volume block size when it is itself a legal node size.
    let want = block_size.max(512);
    let mut chosen = 512u16;
    for candidate in [512u16, 1024, 2048, 4096, 8192, 16384, 32768] {
        if u32::from(candidate) >= want {
            chosen = candidate;
            break;
        }
        chosen = candidate;
    }
    // The provisional read must stay inside the fork.
    if fork.logical_size < u64::from(chosen) {
        return Err(Error::invalid(
            "btree fork",
            format!("{} bytes is too small for a B-tree node", fork.logical_size),
        ));
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemoryDevice;
    use crate::btree::node::NodeKind;
    use crate::format::extents::ExtentDescriptor;

    /// The byte size of the fields a B-tree header record occupies.
    const HR: usize = 14 + 106;

    /// Build a minimal one-node B-tree at allocation block 10.
    fn build_tree(
        node_size: u16,
        total_nodes: u32,
        root_kind: i8,
    ) -> (MemoryDevice, ForkData, u32) {
        let block_size = 4096u32;
        let mut dev = MemoryDevice::zeroed(64 * 1024);
        let mut node = vec![0u8; usize::from(node_size)];

        // Node descriptor: header node, three records.
        node[0..4].copy_from_slice(&0u32.to_be_bytes());
        node[4..8].copy_from_slice(&0u32.to_be_bytes());
        node[8] = root_kind as u8;
        node[9] = 0;
        node[10..12].copy_from_slice(&3u16.to_be_bytes());

        // Header record at offset 14.
        let h = 14usize;
        node[h..h + 2].copy_from_slice(&1u16.to_be_bytes()); // treeDepth
        node[h + 2..h + 6].copy_from_slice(&0u32.to_be_bytes()); // rootNode
        node[h + 18..h + 20].copy_from_slice(&node_size.to_be_bytes());
        node[h + 20..h + 22].copy_from_slice(&516u16.to_be_bytes()); // maxKeyLength
        node[h + 22..h + 26].copy_from_slice(&total_nodes.to_be_bytes());
        node[h + 26..h + 30].copy_from_slice(&0u32.to_be_bytes()); // freeNodes
        node[h + 36] = 0; // btreeType
        node[h + 37] = 0xCF; // keyCompareType: case folding

        // Offset array at the end of the node: three records plus the free
        // offset, descending addresses.
        // Records grow upward from offset 14; slot values ascend with the index
        // and slot addresses descend from the end of the node.
        let rec0 = 14u16; // the BTHeaderRec
        let rec1 = (HR) as u16; // the 128-byte user area
        let rec2 = (HR + 128) as u16; // the node map bitmap
        let set_slot = |n: &mut Vec<u8>, i: usize, v: u16| {
            let at = usize::from(node_size) - 2 * i - 2;
            n[at..at + 2].copy_from_slice(&v.to_be_bytes());
        };
        set_slot(&mut node, 0, rec0);
        set_slot(&mut node, 1, rec1);
        set_slot(&mut node, 2, rec2);
        set_slot(&mut node, 3, rec2);

        let off = 2u64 * u64::from(block_size);
        dev.as_mut_slice()[off as usize..off as usize + node.len()].copy_from_slice(&node);

        let fork = ForkData {
            // Eight allocation blocks of 4096 hold eight 4096-byte nodes, so
            // every node number below total_nodes is really addressable.
            logical_size: u64::from(total_nodes) * u64::from(node_size),
            clump_size: 0,
            total_blocks: 8,
            extents: {
                let mut r = crate::format::extents::ExtentRecord::EMPTY;
                r.raw[0] = ExtentDescriptor {
                    start_block: 2,
                    block_count: 8,
                };
                r
            },
        };
        (dev, fork, block_size)
    }

    #[test]
    fn opens_a_tree_and_reads_its_header() {
        let (dev, fork, bs) = build_tree(4096, 8, 1);
        let bt = BTreeFile::open(&dev, &fork, bs, true).unwrap();
        assert_eq!(bt.node_size(), 4096);
        assert_eq!(bt.header().total_nodes, 8);
        assert_eq!(bt.header().tree_depth, 1);
        assert!(bt.has_big_keys());
        assert!(!bt.is_case_sensitive());
    }

    #[test]
    fn reads_the_header_node_with_records_addressable() {
        let (dev, fork, bs) = build_tree(4096, 8, 1);
        let bt = BTreeFile::open(&dev, &fork, bs, true).unwrap();
        let bytes = bt.read_node_bytes(HEADER_NODE_NUM).unwrap();
        let node = bt.parse_node(&bytes).unwrap();
        assert_eq!(node.kind(), NodeKind::Header);
        assert_eq!(node.num_records(), 3);
        // Record 0 is the header record itself.
        assert!(node.record(0).unwrap().len() >= 106);
    }

    #[test]
    fn re_reads_when_the_declared_node_size_differs_from_the_provisional_one() {
        // A 1024-byte tree on a 4096-byte-block volume: the provisional read
        // uses 4096, the header says 1024, so everything must be redone.
        let (dev, fork, bs) = build_tree(1024, 16, 1);
        let bt = BTreeFile::open(&dev, &fork, bs, true).unwrap();
        assert_eq!(bt.node_size(), 1024);
        let bytes = bt.read_node_bytes(0).unwrap();
        assert_eq!(bytes.len(), 1024);
        let node = bt.parse_node(&bytes).unwrap();
        assert_eq!(node.num_records(), 3);
    }

    #[test]
    fn rejects_node_numbers_outside_the_tree() {
        let (dev, fork, bs) = build_tree(4096, 8, 1);
        let bt = BTreeFile::open(&dev, &fork, bs, true).unwrap();
        assert!(bt.read_node_bytes(8).is_err());
        assert!(bt.read_node_bytes(u32::MAX).is_err());
        assert!(bt.node_offset(8).is_err());
        assert!(bt.read_node_bytes(7).is_ok());
    }

    #[test]
    fn rejects_a_tree_whose_node_size_is_512_on_hfsplus() {
        let (dev, fork, bs) = build_tree(512, 8, 1);
        assert!(BTreeFile::open(&dev, &fork, bs, true).is_err());
        assert!(BTreeFile::open(&dev, &fork, bs, false).is_ok());
    }

    #[test]
    fn rejects_a_fork_too_small_to_hold_a_node() {
        let (dev, fork, _) = build_tree(4096, 8, 1);
        let tiny = ForkData {
            logical_size: 100,
            ..fork
        };
        assert!(BTreeFile::open(&dev, &tiny, 4096, true).is_err());
    }

    #[test]
    fn a_buffer_of_the_wrong_size_is_refused() {
        let (dev, fork, bs) = build_tree(4096, 8, 1);
        let bt = BTreeFile::open(&dev, &fork, bs, true).unwrap();
        assert!(bt.read_node_into(0, &mut [0u8; 100]).is_err());
        assert!(bt.read_node_into(0, &mut vec![0u8; 4096]).is_ok());
    }

    #[test]
    fn an_empty_fork_is_refused() {
        let dev = MemoryDevice::zeroed(4096);
        assert!(BTreeFile::open(&dev, &ForkData::EMPTY, 4096, true).is_err());
    }
}
