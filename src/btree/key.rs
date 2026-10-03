//! B-tree keys: length prefix, padding, and the three HFS+ key types.
//!
//! # Encoding
//!
//! Every record in an HFS+ B-tree node begins with a key. The key's own length
//! prefix is 16 bits when the tree sets `kBTBigKeysMask`, and 8 bits otherwise.
//! All HFS+ trees set it, but the format retains the 8-bit form and Apple
//! re-derives the flag rather than trusting the on-disk attribute:
//!
//! ```c
//! if ( btreePtr->maxKeyLength > 40 )
//!     btreePtr->attributes |= (kBTBigKeysMask + kBTVariableIndexKeysMask);
//! ```
//!
//! Mining reference: Apple `core/BTree.c` `BTOpenPath` (the comment there is
//! Apple's own: *"we need a way to save these attributes"*).
//!
//! A key's on-disk size is the length prefix plus the key body, **rounded up to
//! an even number**. Mining reference: `core/BTreeNodeOps.c`
//! (`InsertKeyRecord`):
//!
//! ```c
//! if ( btreePtr->attributes & kBTBigKeysMask )
//!     keySize = keyLength + sizeof(u_int16_t);
//! else
//!     keySize = keyLength + sizeof(u_int8_t);
//! if ( M_IsOdd (keySize) )
//!     ++keySize;                    // add pad byte
//! ```
//!
//! Getting this wrong desynchronises the record stream for the whole node, so
//! it is encoded here once and used everywhere.
//!
//! # Key types
//!
//! Mining reference: `core/hfs_format.h`, `HFSPlusCatalogKey`,
//! `HFSPlusExtentKey`, `HFSPlusAttrKey`, and the `kHFSPlus*KeyMaximumLength`
//! constants which are all defined as `sizeof(Key) - sizeof(u_int16_t)`.

use crate::endian::Be;
use crate::error::{Error, Result};

/// Byte size of the big-key length prefix.
pub const BIG_KEY_PREFIX: usize = 2;

/// Byte size of the small-key length prefix.
pub const SMALL_KEY_PREFIX: usize = 1;

/// `kHFSPlusCatalogKeyMaximumLength`: the catalog key body, excluding its prefix.
///
/// Mining reference: `core/hfs_format.h` computes this as
/// `sizeof(HFSPlusCatalogKey) - sizeof(u_int16_t)`, and `HFSPlusCatalogKey` is
/// `u16 keyLength + u32 parentID + HFSUniStr255 nodeName`, where
/// `HFSUniStr255` is `u16 length + UniChar unicode[255]` = 512 bytes.
pub const CATALOG_KEY_MAX_LENGTH: usize = 4 + 512;

/// `kHFSPlusExtentKeyMaximumLength`: the extents overflow key body.
///
/// Mining reference: `core/hfs_format.h` computes this as
/// `sizeof(HFSPlusExtentKey) - sizeof(u_int16_t)`, and `HFSPlusExtentKey` is
///
/// ```c
/// struct HFSPlusExtentKey {
///     u_int16_t  keyLength;   /* length of key, excluding this field */
///     u_int8_t   forkType;    /* 0 = data fork, FF = resource fork */
///     u_int8_t   pad;         /* make the other fields align on 32-bit */
///     u_int32_t  fileID;
///     u_int32_t  startBlock;
/// } __attribute__((aligned(2), packed));
/// ```
///
/// so the body is `forkType + pad + fileID + startBlock` = 10 bytes, and the
/// record with its length prefix is 12. `forkType` is not padding: it is what
/// keeps the two forks of one file apart in a shared tree.
pub const EXTENT_KEY_MAX_LENGTH: usize = 1 + 1 + 4 + 4;

/// `kHFSPlusAttrKeyMaximumLength`: the attributes key body.
///
/// Mining reference: `kHFSPlusAttrKeyMaximumLength` is
/// `sizeof(HFSPlusAttrKey) - sizeof(u_int16_t)` in `core/hfs_format.h`, and
/// `HFSPlusAttrKey` is
///
/// ```c
/// struct HFSPlusAttrKey {
///     u_int16_t     keyLength;       /* key length, in bytes */
///     u_int16_t     pad;             /* set to zero */
///     u_int32_t     fileID;
///     u_int32_t     startBlock;
///     u_int16_t     attrNameLen;     /* number of unicode characters */
///     u_int16_t     attrName[kHFSMaxAttrNameLen];  /* kHFSMaxAttrNameLen = 127 */
/// } __attribute__((aligned(2), packed));
/// ```
///
/// so the body is `pad + fileID + startBlock + attrNameLen + attrName` = 266
/// bytes, and the record with its length prefix is 268. The `pad` is a real
/// field, not filler: an earlier revision omitted it and got 264, which the
/// corpus's own attributes tree contradicts -- `mkfs.hfsplus` writes 266 there.
pub const ATTR_KEY_MAX_LENGTH: usize = 2 + 4 + 4 + 2 + 127 * 2;

/// Whether a tree's keys carry a 16-bit length prefix.
///
/// Mining reference: Apple re-derives this from `maxKeyLength > 40` in
/// `core/BTree.c` rather than trusting the stored `attributes` bit, so a corrupt
/// attribute word cannot desynchronise key parsing.
pub const fn has_big_keys(max_key_length: u16, attributes: u32) -> bool {
    attributes & K_BT_BIG_KEYS_MASK != 0 || max_key_length > 40
}

/// `kBTBigKeysMask`.
pub const K_BT_BIG_KEYS_MASK: u32 = 0x0000_0002;

/// `kBTVariableIndexKeysMask`.
pub const K_BT_VARIABLE_INDEX_KEYS_MASK: u32 = 0x0000_0004;

/// A borrowed key inside a node record.
///
/// Holds the record bytes from the key prefix onwards, with the parity padding
/// trimmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRef<'a> {
    bytes: &'a [u8],
    big_keys: bool,
}

impl<'a> KeyRef<'a> {
    /// Interpret `record` as a key, given the tree's key-prefix width.
    ///
    /// `record` is the whole record, key plus data. The key ends at the
    /// declared length; anything after it belongs to the record payload.
    pub fn from_record(record: &'a [u8], big_keys: bool) -> Result<Self> {
        let prefix = if big_keys { BIG_KEY_PREFIX } else { SMALL_KEY_PREFIX };
        let body_len = Be::new(record).u16(0)? as usize;
        let declared = body_len.checked_add(prefix).ok_or(Error::overflow("key length"))?;
        let bytes = record.get(..declared).ok_or(Error::Truncated {
            what: "btree key",
            needed: declared,
            available: record.len(),
        })?;
        Ok(KeyRef { bytes, big_keys })
    }

    /// The key body, excluding the length prefix.
    pub fn body(&self) -> &'a [u8] {
        let prefix = if self.big_keys { BIG_KEY_PREFIX } else { SMALL_KEY_PREFIX };
        &self.bytes[prefix..]
    }

    /// The declared body length from the length prefix.
    pub fn body_len(&self) -> usize {
        self.body().len()
    }

    /// On-disk size of this key, including prefix and the even-byte pad.
    ///
    /// Mining reference: `core/BTreeNodeOps.c` `InsertKeyRecord` rounds
    /// `keyLength + sizeof(prefix)` up to an even byte count.
    pub fn on_disk_size(&self) -> usize {
        let mut size = self.bytes.len();
        if size % 2 == 1 {
            size += 1;
        }
        size
    }

    /// The record payload following this key, with the pad byte removed.
    pub fn payload(&self, record: &'a [u8]) -> Result<&'a [u8]> {
        let end = self
            .on_disk_size()
            .min(record.len())
            .max(self.bytes.len())
            .min(record.len());
        Ok(&record[end..])
    }
}

/// A catalog key: parent CNID plus a Unicode name.
///
/// Mining reference: Apple `core/hfs_format.h` `struct HFSPlusCatalogKey`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogKey<'a> {
    /// Parent folder CNID; `1` (`kHFSRootParentID`) for a thread record.
    pub parent_id: u32,
    /// The node name as raw UTF-16 code units, without the length prefix.
    pub name: &'a [u8],
    /// Whether the tree compares names case-sensitively.
    ///
    /// Taken from the catalog B-tree's `keyCompareType`, not from the volume
    /// signature: `kHFSCaseFolding` (`0xCF`) or `kHFSBinaryCompare` (`0xBC`).
    pub case_sensitive: bool,
}

impl<'a> CatalogKey<'a> {
    /// Decode a catalog key from a record.
    pub fn from_record(record: &'a [u8], case_sensitive: bool) -> Result<Self> {
        let be = Be::new(record);
        if record.len() < BIG_KEY_PREFIX + 4 {
            return Err(Error::Truncated {
                what: "catalog key",
                needed: BIG_KEY_PREFIX + 4,
                available: record.len(),
            });
        }
        // Mining reference: the catalog key is always big-key, since
        // kHFSPlusCatalogKeyMaximumLength (516) is far above the 40-byte
        // threshold Apple uses to derive kBTBigKeysMask.
        let key_len = be.u16(0)? as usize;
        let declared = key_len
            .checked_add(BIG_KEY_PREFIX)
            .ok_or(Error::overflow("catalog key length"))?;
        if declared > record.len() {
            return Err(Error::Truncated {
                what: "catalog key",
                needed: declared,
                available: record.len(),
            });
        }
        let parent_id = be.u32(2)?;
        // HFSUniStr255: u16 length, then that many big-endian u16 code units.
        let name_len = be.u16(6)? as usize;
        let name_bytes = name_len
            .checked_mul(2)
            .and_then(|n| 8usize.checked_add(n))
            .ok_or(Error::overflow("catalog key name"))?;
        let name_end = name_bytes.min(declared);
        let name = record.get(8..name_end).ok_or(Error::Truncated {
            what: "catalog key name",
            needed: name_end,
            available: record.len(),
        })?;

        Ok(CatalogKey { parent_id, name, case_sensitive })
    }

    /// The name as UTF-16 code units.
    pub fn name_units(&self) -> Vec<u16> {
        self.name
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    }

    /// The name as a `String`, for diagnostics only.
    ///
    /// Never use this for comparison. HFS+ name ordering is defined by the
    /// Unicode comparison rules in `core/UnicodeWrappers.c` (`FastRelString`,
    /// `FastUnicodeCompare`), not by UTF-8 byte order or `String::cmp`.
    pub fn name_string(&self) -> String {
        String::from_utf16_lossy(&self.name_units())
    }
}

/// An extents overflow key: fork type, CNID, and a cumulative block offset.
///
/// Mining reference: `core/hfs_format.h` `struct HFSPlusExtentKey`:
/// `u16 keyLength + u8 forkType + u8 pad + u32 fileID + u32 startBlock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentKey {
    /// Which fork of the file these extents describe: 0 data, 0xFF resource.
    ///
    /// Both forks share one B-tree, so this is what stops a data fork's extents
    /// being read as its resource fork's.
    pub fork_type: u8,
    /// CNID of the file whose extents these are.
    pub file_id: u32,
    /// Allocation blocks already described by earlier groups for this file.
    ///
    /// Not a physical block number. `core/hfs_extents.c` advances it by
    /// `hfs_total_blocks(...)` after each group.
    pub start_block: u32,
}

impl ExtentKey {
    /// Byte offset of `forkType` within the key record.
    const FORK_TYPE_OFFSET: usize = BIG_KEY_PREFIX;

    /// Byte offset of `fileID` within the key record.
    const FILE_ID_OFFSET: usize = BIG_KEY_PREFIX + 2;

    /// Byte offset of `startBlock` within the key record.
    const START_BLOCK_OFFSET: usize = BIG_KEY_PREFIX + 6;

    /// Byte size of the key record, prefix included.
    ///
    /// There is no trailing pad: the body is 10 bytes and the prefix 2, so the
    /// total is already even.
    ///
    /// Assumes the 16-bit key-length form. See [`crate::btree::node`]'s note on
    /// `key_size_on_disk` for why that holds and what happens if it does not --
    /// short answer: `hfsck` refuses such a tree rather than misreading it.
    pub const ON_DISK_SIZE: usize = BIG_KEY_PREFIX + EXTENT_KEY_MAX_LENGTH;

    /// `kDataForkType`, from `core/FileExtentMapping.c`.
    pub const DATA_FORK: u8 = 0;
    /// `kResourceForkType`, from `core/FileExtentMapping.c`.
    pub const RESOURCE_FORK: u8 = 0xFF;

    /// A key for a fork's data extents.
    pub fn for_data_fork(file_id: u32, start_block: u32) -> Self {
        ExtentKey { fork_type: Self::DATA_FORK, file_id, start_block }
    }

    /// A key for a fork's resource extents.
    pub fn for_resource_fork(file_id: u32, start_block: u32) -> Self {
        ExtentKey { fork_type: Self::RESOURCE_FORK, file_id, start_block }
    }

    /// Encode this key into a node record.
    pub fn to_record(&self) -> [u8; Self::ON_DISK_SIZE] {
        let mut out = [0u8; Self::ON_DISK_SIZE];
        out[0..2].copy_from_slice(&(EXTENT_KEY_MAX_LENGTH as u16).to_be_bytes());
        out[Self::FORK_TYPE_OFFSET] = self.fork_type;
        out[Self::FILE_ID_OFFSET..Self::FILE_ID_OFFSET + 4]
            .copy_from_slice(&self.file_id.to_be_bytes());
        out[Self::START_BLOCK_OFFSET..Self::START_BLOCK_OFFSET + 4]
            .copy_from_slice(&self.start_block.to_be_bytes());
        out
    }

    /// Decode an extents key from a record.
    pub fn from_record(record: &[u8]) -> Result<Self> {
        let be = Be::new(record);
        if record.len() < Self::ON_DISK_SIZE {
            return Err(Error::Truncated {
                what: "extent key",
                needed: Self::ON_DISK_SIZE,
                available: record.len(),
            });
        }
        let declared = (be.u16(0)? as usize)
            .checked_add(BIG_KEY_PREFIX)
            .ok_or(Error::overflow("extent key length"))?;
        if declared > record.len() {
            return Err(Error::Truncated {
                what: "extent key",
                needed: declared,
                available: record.len(),
            });
        }
        Ok(ExtentKey {
            fork_type: record[Self::FORK_TYPE_OFFSET],
            file_id: be.u32(Self::FILE_ID_OFFSET)?,
            start_block: be.u32(Self::START_BLOCK_OFFSET)?,
        })
    }

    /// Compare two extents keys the way the B-tree orders them.
    ///
    /// Mining reference: `core/hfs_extents.c` and Apple's attributes key
    /// comparator order by `fileID` first, then by `startBlock`, both as plain
    /// 32-bit numbers. Extents keys are never case folded: they contain no names.
    /// `forkType` is not part of the ordering, because one file's data and
    /// resource forks share a `fileID` and must sort adjacently.
    pub fn cmp_key(&self, other: &Self) -> std::cmp::Ordering {
        self.file_id
            .cmp(&other.file_id)
            .then_with(|| self.start_block.cmp(&other.start_block))
    }
}

/// Split an Extents B-tree node record into its key and the 64-byte extent array.
///
/// The Extents B-tree does **not** use catalog keys, so it cannot go through the
/// catalog `split_record`. Mining reference: `core/hfs_format.h`
/// `struct HFSPlusExtentKey`, `u_int16_t keyLength + u32 fileID + u32 startBlock`.
pub fn split_extent_record(record: &[u8]) -> Option<(ExtentKey, &[u8])> {
    let key = ExtentKey::from_record(record).ok()?;
    let off = ExtentKey::ON_DISK_SIZE.min(record.len());
    if off > record.len() {
        return None;
    }
    Some((key, &record[off..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a catalog-key record: big-key prefix, parentID, UniStr255 name.
    fn catalog_record(parent: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let body_len = 4 + 2 + units.len() * 2;
        let mut out = Vec::new();
        out.extend_from_slice(&(body_len as u16).to_be_bytes());
        out.extend_from_slice(&parent.to_be_bytes());
        out.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for u in units {
            out.extend_from_slice(&u.to_be_bytes());
        }
        out
    }

    #[test]
    fn catalog_key_round_trips() {
        let rec = catalog_record(2, "TestVol");
        let k = CatalogKey::from_record(&rec, false).unwrap();
        assert_eq!(k.parent_id, 2);
        assert_eq!(k.name_string(), "TestVol");
        assert!(!k.case_sensitive);
    }

    #[test]
    fn catalog_key_handles_non_ascii_names() {
        let rec = catalog_record(16, "café");
        let k = CatalogKey::from_record(&rec, true).unwrap();
        assert_eq!(k.name_string(), "café");
        assert!(k.case_sensitive);
    }

    #[test]
    fn catalog_key_body_length_is_the_key_length() {
        // "ab" => 4 (parent) + 2 (name length) + 4 (two UTF-16 units) = 10
        let rec = catalog_record(1, "ab");
        let key = KeyRef::from_record(&rec, true).unwrap();
        assert_eq!(key.body_len(), 10);
    }

    #[test]
    fn key_on_disk_size_is_even() {
        // 2-byte prefix + 10-byte body = 12, already even.
        let rec = catalog_record(1, "ab");
        let key = KeyRef::from_record(&rec, true).unwrap();
        assert_eq!(key.on_disk_size(), 12);

        // An odd body gets a pad byte.
        let mut odd = catalog_record(1, "abc");
        odd[0..2].copy_from_slice(&11u16.to_be_bytes());
        let key = KeyRef::from_record(&odd, true).unwrap();
        assert_eq!(key.body_len(), 11);
        assert_eq!(key.on_disk_size(), 14);
    }

    #[test]
    fn big_keys_are_derived_from_max_key_length() {
        // Apple's rule: maxKeyLength > 40 implies big keys, whatever the
        // stored attribute says.
        assert!(has_big_keys(516, 0));
        assert!(has_big_keys(264, 0));
        // Apple's test is strictly greater than 40, so 40 itself is small.
        assert!(!has_big_keys(40, 0));
        assert!(has_big_keys(41, 0));
        assert!(!has_big_keys(8, 0));
        // The stored bit also counts.
        assert!(has_big_keys(8, K_BT_BIG_KEYS_MASK));
    }

    #[test]
    fn catalog_key_constants_match_apple() {
        // HFSUniStr255 is 2 + 255*2 = 512 bytes; the body is 4 + 512 = 516.
        assert_eq!(CATALOG_KEY_MAX_LENGTH, 516);
        // The extents key body is forkType + pad + fileID + startBlock = 10, so
        // it is 12 bytes with the prefix -- still below the 40-byte big-key
        // threshold, and therefore still on the 16-bit length form.
        assert_eq!(EXTENT_KEY_MAX_LENGTH, 10);
        assert_eq!(ExtentKey::ON_DISK_SIZE, 12);
        // 266, confirmed by the corpus's attributes tree header: `mkfs.hfsplus`
        // writes that value, so it is ground truth rather than a reading of the
        // header. The two bytes of `pad` in `HFSPlusAttrKey` are the difference
        // from the 264 an earlier revision used.
        assert_eq!(ATTR_KEY_MAX_LENGTH, 266);
        // Catalog and attributes keys are above the 40-byte big-key threshold;
        // the extents key is below it, so a strict `> 40` test would leave that
        // tree on the 8-bit form unless the stored bit says otherwise.
        assert!(has_big_keys(CATALOG_KEY_MAX_LENGTH as u16, 0));
        assert!(has_big_keys(ATTR_KEY_MAX_LENGTH as u16, 0));
        assert!(!has_big_keys(EXTENT_KEY_MAX_LENGTH as u16, 0));
    }

    #[test]
    fn extent_key_round_trips() {
        let k = ExtentKey::for_data_fork(42, 770);
        let rec = k.to_record();
        // `sizeof(struct HFSPlusExtentKey)`: the prefix plus forkType, pad,
        // fileID and startBlock.
        assert_eq!(ExtentKey::ON_DISK_SIZE, 12);
        assert_eq!(ExtentKey::from_record(&rec).unwrap(), k);

        // And through the record splitter, which extents records go through.
        let mut node_record = rec.to_vec();
        node_record.extend_from_slice(&[0xAA; 64]);
        let (key, body) = split_extent_record(&node_record).expect("split");
        assert_eq!(key, k);
        assert_eq!(body.len(), 64);
        assert!(body.iter().all(|b| *b == 0xAA));
    }

    #[test]
    fn extent_keys_order_by_cnid_then_offset() {
        let a = ExtentKey::for_data_fork(2, 100);
        let b = ExtentKey::for_data_fork(2, 200);
        let c = ExtentKey::for_data_fork(3, 0);
        assert_eq!(a.cmp_key(&b), std::cmp::Ordering::Less);
        assert_eq!(b.cmp_key(&a), std::cmp::Ordering::Greater);
        // CNID dominates: a higher CNID sorts after even with a smaller offset.
        assert_eq!(a.cmp_key(&c), std::cmp::Ordering::Less);
        assert_eq!(a.cmp_key(&a), std::cmp::Ordering::Equal);
    }

    #[test]
    fn a_truncated_extent_record_is_refused() {
        assert!(split_extent_record(&[]).is_none());
        assert!(split_extent_record(&[0u8; 5]).is_none());
        assert!(split_extent_record(&[0u8; 9]).is_none());
        assert!(split_extent_record(&[0u8; 11]).is_none());
    }

    #[test]
    fn the_fork_type_survives_a_round_trip_and_is_not_the_file_id() {
        // The field is what keeps a data fork from adopting its resource fork's
        // extents, and it sits between the length prefix and fileID on disk.
        let data = ExtentKey::for_data_fork(0x1122_3344, 0x5566_7788);
        let record = data.to_record();
        assert_eq!(record[2], ExtentKey::DATA_FORK, "forkType is at offset 2");
        assert_eq!(record[3], 0, "pad is at offset 3");
        assert_eq!(
            &record[4..8],
            &0x1122_3344u32.to_be_bytes(),
            "fileID is at offset 4, not 2"
        );
        assert_eq!(&record[8..12], &0x5566_7788u32.to_be_bytes());

        let back = ExtentKey::from_record(&record).unwrap();
        assert_eq!(back.file_id, 0x1122_3344);
        assert_eq!(back.fork_type, ExtentKey::DATA_FORK);

        let resource = ExtentKey::for_resource_fork(0x1122_3344, 0).to_record();
        assert_eq!(resource[2], 0xFF, "kResourceForkType is 0xFF");
        assert_ne!(
            ExtentKey::from_record(&data.to_record()).unwrap(),
            ExtentKey::from_record(&resource).unwrap(),
            "the two forks of one file must be distinguishable"
        );
    }

    #[test]
    fn a_declared_length_beyond_the_record_is_truncated() {
        let mut rec = catalog_record(2, "TestVol");
        rec[0..2].copy_from_slice(&4096u16.to_be_bytes());
        assert!(matches!(
            CatalogKey::from_record(&rec, false),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn short_records_are_refused_not_panicked_on() {
        for len in [0usize, 1, 2, 5, 7, 8] {
            let rec = vec![0u8; len];
            assert!(CatalogKey::from_record(&rec, false).is_err(), "catalog len {len}");
            assert!(ExtentKey::from_record(&rec).is_err(), "extent len {len}");
        }
    }
}