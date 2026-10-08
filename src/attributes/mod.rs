// SPDX-License-Identifier: APSL-1.2

//! The Attributes File: a second B-tree, holding what is not in the catalog.
//!
//! # Four things that are not the same
//!
//! HFS+ stores an object's metadata in three different places, and they are
//! routinely confused. Getting them apart is most of the work in this subsystem:
//!
//! | | where it lives |
//! | --- | --- |
//! | data fork | `dataFork` in the catalog record |
//! | resource fork | `rsrcFork` in the catalog record — a real fork, not an attribute |
//! | FinderInfo | an attribute in *this* tree |
//! | named attributes | attributes in this tree |
//!
//! FinderInfo is the one that surprises people. The catalog record's
//! `HFSPlusBSDInfo` is 16 bytes and has no FinderInfo field; HFS+ kept it in the
//! attributes tree, where classic HFS had no equivalent to move.
//!
//! And a POSIX extended attribute is a fourth thing again — a name/value pair
//! that a caller outside this filesystem asked for. Which attributes become
//! `getxattr` is a decision for the FUSE adapter, not here.
//!
//! # Structure
//!
//! The tree is keyed by [`AttrKey`] and holds [`AttrRecord`] values. An
//! attribute's value is either stored inside the record or held in allocation
//! blocks described by a fork, with continuation records chained through the key's
//! `startBlock`. That chaining is the same shape as the catalog's extents
//! overflow, but keyed inside this tree rather than the shared extents tree.

pub mod key;
pub mod names;
pub mod record;

pub use key::{AttrKey, ATTR_KEY_BODY_SIZE, ATTR_KEY_RECORD_SIZE, MAX_ATTR_NAME_LEN};
pub use names::{is_system_attribute, SYSTEM_ATTRIBUTE_NAMES};
pub use record::{AttrRecord, AttrRecordType, ATTR_RECORD_FIXED_SIZE};

use crate::blockdev::BlockDevice;
use crate::btree::io::BTreeFile;
use crate::btree::node::{Node, NodeKind};
use crate::error::{Error, Result};
use crate::format::extents::{ExtentDescriptor, ExtentRecord, INLINE_EXTENT_COUNT};
use crate::format::fork::ForkData;

/// One attribute with its value resolved.
///
/// The three storage forms are folded into one answer, because a caller asking
/// "what is this attribute's value" does not care which was used. That folding is
/// where the inline/fork split has to be got right: a forked value is not in the
/// record at all, so a reader that only looks at the record sees an empty value
/// that is indistinguishable from a legitimately empty attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// The attribute's name, as stored.
    pub name: String,
    /// Its value, read from wherever it was stored.
    pub value: Vec<u8>,
}

/// The Attributes File: a B-tree of named attributes keyed by owner.
///
/// Mining reference: `core/hfs_attrlist.c` builds a per-file attribute list by
/// scanning the tree for one `fileID`, which is the shape used here.
impl<D: BlockDevice + ?Sized> std::fmt::Debug for AttributesFile<'_, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttributesFile")
            .field("tree", &self.tree)
            .field("block_size", &self.block_size)
            .finish()
    }
}

pub struct AttributesFile<'a, D: ?Sized> {
    tree: BTreeFile<'a, D>,
    device: &'a D,
    block_size: u32,
}

impl<'a, D: BlockDevice + ?Sized> AttributesFile<'a, D> {
    /// Open the attributes tree from the volume header's attributes fork.
    pub fn open(device: &'a D, fork: &ForkData, block_size: u32, hfs_plus: bool) -> Result<Self> {
        Ok(AttributesFile {
            tree: BTreeFile::open(device, fork, block_size, hfs_plus)?,
            device,
            block_size,
        })
    }

    /// The tree's header, for callers that want its geometry.
    pub fn header(&self) -> &crate::btree::header::BTreeHeader {
        self.tree.header()
    }

    /// Whether the tree holds no attributes at all.
    ///
    /// A freshly formatted volume allocates the fork and builds an empty tree, so
    /// this is the common case and worth not treating as an error.
    pub fn is_empty(&self) -> bool {
        self.tree.header().leaf_records == 0
    }

    /// Every attribute belonging to `cnid`, with values resolved.
    ///
    /// Records for one file are contiguous, because `fileID` is the most
    /// significant part of the key, so the scan stops at the first record
    /// belonging to a different file instead of reading the whole tree.
    pub fn attributes_for(&self, cnid: u32) -> Result<Vec<Attribute>> {
        let max_key = self.tree.header().max_key_length as usize;
        let total_nodes = self.tree.header().total_nodes;
        let last_leaf = self.tree.header().last_leaf_node;
        let mut node_num = self.tree.header().first_leaf_node;
        let mut budget = total_nodes;

        // A fragmented attribute is several records: the first names the fork and
        // the rest carry its continuation extents. They are collected together
        // because returning the first alone would look like an empty value.
        let mut out: Vec<Attribute> = Vec::new();
        let mut assembling: Option<Assembling> = None;

        while budget > 0 && node_num != 0 && node_num < total_nodes {
            budget -= 1;
            let bytes = self.tree.read_node_bytes(node_num)?;
            let node: Node<'_> = self.tree.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }

            for index in 0..node.num_records() {
                let record = node.record(index)?;
                let Some((_raw_key, body)) = split_attr_record(record) else {
                    continue;
                };
                // `from_record` takes the whole node record, prefix included, so
                // that it pairs with `to_record` and with the tests. Handing it
                // only the bytes after the length would make every key look one
                // field short.
                let key = AttrKey::from_record(record, max_key).map_err(|_| {
                    Error::invalid(
                        "attribute key",
                        format!("a record in node {node_num} has an unreadable key"),
                    )
                })?;

                if key.file_id != cnid {
                    // Records are grouped by file, so a larger fileID means the
                    // rest of the tree is not ours.
                    if key.file_id > cnid {
                        Self::finish(assembling.take(), &mut out, self)?;
                        return Ok(out);
                    }
                    continue;
                }

                let attr = AttrRecord::from_record(body).map_err(|e| {
                    Error::invalid(
                        "attribute record",
                        format!("node {node_num} holds a record that is not an attribute: {e}"),
                    )
                })?;

                match attr {
                    AttrRecord::Inline { value } => {
                        Self::finish(assembling.take(), &mut out, self)?;
                        out.push(Attribute {
                            name: key.name,
                            value,
                        });
                    }
                    AttrRecord::Fork { fork } => {
                        // Any attribute still being assembled is finished first:
                        // two `Fork` records cannot share a name, because the
                        // name is part of the key.
                        Self::finish(assembling.take(), &mut out, self)?;
                        assembling = Some(Assembling {
                            cnid: key.file_id,
                            name: key.name,
                            fork,
                            extents: Vec::new(),
                        });
                    }
                    AttrRecord::Extents { extents } => match assembling.as_mut() {
                        Some(a) => a.extents.push(extents),
                        None => {
                            return Err(Error::invalid(
                                "attribute record",
                                "a continuation record with no attribute to continue",
                            ))
                        }
                    },
                }
            }

            if node_num == last_leaf {
                break;
            }
            let next = node.descriptor().f_link;
            if next == 0 {
                break;
            }
            node_num = next;
        }

        Self::finish(assembling.take(), &mut out, self)?;
        Ok(out)
    }

    /// Read a fragmented attribute's value and push it.
    fn finish(
        assembling: Option<Assembling>,
        out: &mut Vec<Attribute>,
        file: &AttributesFile<'_, D>,
    ) -> Result<()> {
        let Some(a) = assembling else { return Ok(()) };
        let value = file.read_fork_value(&a.fork, &a.extents)?;
        out.push(Attribute {
            name: a.name,
            value,
        });
        Ok(())
    }

    /// Read a value stored in allocation blocks.
    ///
    /// The first eight extents come from the `Fork` record and any further from
    /// the continuation records, so the two are concatenated before reading.
    /// This is deliberately *not* routed through the extents overflow B-tree:
    /// attribute continuation records live in this tree, keyed by `startBlock`,
    /// which is a different mechanism from a fork overflowing the shared tree even
    /// though both use "running count of blocks already described".
    /// Every allocation block any attribute value occupies in this tree.
    ///
    /// Needed by anything accounting for the volume's used blocks -- the
    /// allocation bitmap does not know about them, so a checker that walks only
    /// the catalog will report every one of them as orphaned. That is not a
    /// hypothetical: a FinderInfo large enough to be forked is unremarkable on a
    /// real macOS volume.
    pub fn allocated_blocks(&self) -> Result<Vec<u64>> {
        Ok(self.forked_values()?.1)
    }

    /// The forked attributes in the tree, and every block their values occupy.
    ///
    /// Returns `(attributes, blocks)` rather than only one of them so a caller
    /// that walks the tree once can answer both questions without paying for the
    /// walk twice.
    pub fn forked_values(&self) -> Result<(Vec<ForkedAttribute>, Vec<u64>)> {
        let max_key = self.tree.header().max_key_length as usize;
        let total_nodes = self.tree.header().total_nodes;
        let last_leaf = self.tree.header().last_leaf_node;
        let mut node_num = self.tree.header().first_leaf_node;
        let mut budget = total_nodes;

        let mut attributes: Vec<ForkedAttribute> = Vec::new();
        let mut blocks: Vec<u64> = Vec::new();
        let mut assembling: Option<Assembling> = None;

        let flush = |assembling: &mut Option<Assembling>,
                     attributes: &mut Vec<ForkedAttribute>,
                     blocks: &mut Vec<u64>| {
            if let Some(a) = assembling.take() {
                blocks.extend(
                    collect_extents(&a.fork, &a.extents)
                        .iter()
                        .flat_map(|d| {
                            (0..d.block_count).map(move |o| u64::from(d.start_block) + u64::from(o))
                        })
                        .collect::<Vec<u64>>(),
                );
                attributes.push(ForkedAttribute {
                    cnid: a.cnid,
                    name: a.name,
                    fork: a.fork,
                });
            }
        };

        while budget > 0 && node_num != 0 && node_num < total_nodes {
            budget -= 1;
            let bytes = self.tree.read_node_bytes(node_num)?;
            let node: Node<'_> = self.tree.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }
            for index in 0..node.num_records() {
                let record = node.record(index)?;
                let Some((_, body)) = split_attr_record(record) else {
                    continue;
                };
                let key = match AttrKey::from_record(record, max_key) {
                    Ok(k) => k,
                    Err(_) => continue,
                };
                let attr = match AttrRecord::from_record(body) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                match attr {
                    AttrRecord::Inline { .. } => {
                        flush(&mut assembling, &mut attributes, &mut blocks);
                    }
                    AttrRecord::Fork { fork } => {
                        flush(&mut assembling, &mut attributes, &mut blocks);
                        assembling = Some(Assembling {
                            cnid: key.file_id,
                            name: key.name,
                            fork,
                            extents: Vec::new(),
                        });
                    }
                    AttrRecord::Extents { extents } => {
                        if let Some(a) = assembling.as_mut() {
                            a.extents.push(extents);
                        }
                    }
                }
            }
            if node_num == last_leaf {
                break;
            }
            let next = node.descriptor().f_link;
            if next == 0 {
                break;
            }
            node_num = next;
        }
        flush(&mut assembling, &mut attributes, &mut blocks);
        Ok((attributes, blocks))
    }

    fn read_fork_value(&self, fork: &ForkData, continuation: &[ExtentRecord]) -> Result<Vec<u8>> {
        let mut extents: Vec<ExtentDescriptor> = Vec::new();
        for descriptor in fork.extents.iter() {
            if extents.len() == INLINE_EXTENT_COUNT {
                break;
            }
            extents.push(*descriptor);
        }
        for record in continuation {
            for descriptor in record.iter() {
                extents.push(*descriptor);
            }
        }

        let limit = usize::try_from(fork.logical_size)
            .map_err(|_| Error::overflow("attribute value length"))?;
        let mut value = Vec::with_capacity(limit.min(1 << 20));

        for descriptor in &extents {
            if value.len() >= limit {
                break;
            }
            if descriptor.block_count == 0 {
                break;
            }
            // A missing block reads as zero rather than failing: a sparse
            // attribute is legal, and refusing would make an attribute with a
            // hole unreadable. The bound on `descriptor.start_block` is the device's.
            for offset in 0..descriptor.block_count {
                if value.len() >= limit {
                    break;
                }
                let at = (u64::from(descriptor.start_block) + u64::from(offset))
                    .checked_mul(u64::from(self.block_size))
                    .ok_or(Error::overflow("attribute block offset"))?;
                let mut block = vec![0u8; self.block_size as usize];
                match self.device.read_at(at, &mut block) {
                    Ok(()) => {}
                    Err(e) => {
                        // Past the end of the device is a hole, not corruption;
                        // anywhere else is a fault.
                        if !matches!(e, Error::Truncated { .. }) {
                            return Err(e);
                        }
                        block.iter_mut().for_each(|b| *b = 0);
                    }
                }
                let take = (limit - value.len()).min(block.len());
                value.extend_from_slice(&block[..take]);
            }
        }
        Ok(value)
    }
}

/// Every extent of a forked value: the fork's own first eight, then each
/// continuation record's.
///
/// Deliberately not going through the extents overflow B-tree: these records
/// live in *this* tree, keyed by `startBlock`, which is a different mechanism that
/// happens to use the same "running count of blocks already described" key.
fn collect_extents(fork: &ForkData, continuation: &[ExtentRecord]) -> Vec<ExtentDescriptor> {
    let mut extents: Vec<ExtentDescriptor> = Vec::new();
    for descriptor in fork.extents.iter() {
        if extents.len() == INLINE_EXTENT_COUNT {
            break;
        }
        extents.push(*descriptor);
    }
    for record in continuation {
        for descriptor in record.iter() {
            extents.push(*descriptor);
        }
    }
    extents
}

/// Split a node record into its key and body.
///
/// The same rule every HFS+ B-tree uses -- a 16-bit length covering the rest of
/// the record -- but returning the key's *bytes* rather than a parsed value. The
/// catalog's equivalent returns a `CatalogKey`, which would mean decoding an
/// attribute key with the catalog's layout. The two trees share B-tree machinery
/// and nothing else, and routing one through the other's parser is precisely the
/// conflation this module exists to prevent.
pub fn split_attr_record(record: &[u8]) -> Option<(&[u8], &[u8])> {
    if record.len() < 2 {
        return None;
    }
    let declared = usize::from(u16::from_be_bytes([record[0], record[1]]));
    let total = declared.checked_add(2)?;
    if total > record.len() {
        return None;
    }
    Some((&record[2..total], &record[total..]))
}

/// A forked attribute whose continuation records have not been read yet.
struct Assembling {
    cnid: u32,
    name: String,
    fork: ForkData,
    extents: Vec<ExtentRecord>,
}

/// A forked attribute: its owner, its name, and the fork its value lives in.
///
/// The fork carries only its own first eight extents; the rest are in
/// continuation records, which [`AttributesFile::forked_values`] has already folded
/// into the returned block list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkedAttribute {
    /// CNID of the object the attribute belongs to.
    pub cnid: u32,
    /// The attribute's name.
    pub name: String,
    /// The fork its value occupies.
    pub fork: ForkData,
}
