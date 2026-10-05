//! Catalog keys.
//!
//! Mining reference: Apple `core/hfs_format.h`, `struct HFSPlusCatalogKey`:
//!
//! ```c
//! struct HFSPlusCatalogKey {
//!     u_int16_t     keyLength;    /* key length (in bytes) */
//!     u_int32_t     parentID;    /* parent folder ID */
//!     HFSUniStr255     nodeName;    /* catalog node name */
//! };
//! ```
//!
//! Keys sort by `parentID` first and then by name. The name comparison is the
//! interesting half and is deliberately *not* implemented here: it depends on
//! the tree's `keyCompareType`, which selects between HFS decomposition folding
//! and a plain binary comparison. See the `unicode` module.

use crate::endian::Be;
use crate::error::{Error, Result};

/// `kHFSCaseFolding`: names compare case-insensitively.
pub const K_HFS_CASE_FOLDING: u8 = 0xCF;

/// `kHFSBinaryCompare`: names compare as a binary string.
pub const K_HFS_BINARY_COMPARE: u8 = 0xBC;

/// Byte size of the key length prefix, which is always 16 bits for catalog keys.
pub const CATALOG_KEY_PREFIX: usize = 2;

/// Byte size of the `parentID` field.
pub const CATALOG_KEY_PARENT_SIZE: usize = 4;

/// Byte size of the `HFSUniStr255` length prefix.
pub const CATALOG_KEY_NAME_LEN_SIZE: usize = 2;

/// Byte offset of `parentID` within a catalog key.
pub const CATALOG_KEY_PARENT_OFFSET: usize = CATALOG_KEY_PREFIX;

/// Byte offset of the name length within a catalog key.
pub const CATALOG_KEY_NAME_LEN_OFFSET: usize = CATALOG_KEY_PREFIX + CATALOG_KEY_PARENT_SIZE;

/// Byte offset of the first UTF-16 code unit within a catalog key.
pub const CATALOG_KEY_NAME_OFFSET: usize = CATALOG_KEY_NAME_LEN_OFFSET + CATALOG_KEY_NAME_LEN_SIZE;

/// The comparison rule a catalog key is ordered by.
///
/// Mining reference: `core/hfs_catalog.c` (`cat_binarykeycompare`) dispatches on
/// the catalog B-tree's `keyCompareType`, and `core/UnicodeWrappers.c`
/// (`FastUnicodeCompare`) takes a case-folding flag sourced from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameComparison {
    /// `kHFSCaseFolding`: HFS decomposition folding, case-insensitive.
    CaseFolding,
    /// `kHFSBinaryCompare`: plain binary comparison, case-sensitive.
    Binary,
}

impl NameComparison {
    /// Choose the comparison rule from a catalog B-tree's `keyCompareType`.
    ///
    /// Anything other than `kHFSBinaryCompare` folds. That is deliberate: an
    /// unrecognised value means "not the case-sensitive rule", and treating it
    /// as case-sensitive would make a volume appear to lose files that are
    /// present.
    ///
    /// Mining reference: Apple `core/hfs_catalog.c` compares
    /// `keyCompareType == kHFSBinaryCompare` to pick the binary path and
    /// defaults to folding otherwise.
    pub const fn from_key_compare_type(code: u8) -> Self {
        if code == K_HFS_BINARY_COMPARE {
            NameComparison::Binary
        } else {
            NameComparison::CaseFolding
        }
    }

    /// Whether this rule distinguishes case.
    pub const fn is_case_sensitive(self) -> bool {
        matches!(self, NameComparison::Binary)
    }
}

/// A catalog key: parent CNID plus a Unicode name.
///
/// `keyLength` "varies between kHFSPlusCatalogKeyMinimumLength (6) to
/// kHFSPlusCatalogKeyMaximumLength (516)" -- it excludes its own two bytes -- and
/// `parentID` means two different things depending on the record: for a file or
/// folder record it is "the folder containing the file or folder", and for a
/// thread record it is "the CNID of the file or folder **itself**", with an
/// **empty** `nodeName`. (TN1150, Catalog File.)
///
/// That asymmetry is the whole reason a thread record exists: it is what makes a
/// CNID resolvable, since file and folder keys never contain one.
///
/// Keys compare by `parentID` first, as an unsigned 32-bit integer, and then by
/// `nodeName` -- case-insensitively on HFS+, or on a case-insensitive HFSX volume,
/// and as a plain unsigned sequence on a case-sensitive one. The
/// "since files do not contain other files or folders, there are no catalog records
/// whose key has a parentID equal to a file's CNID and a non-zero-length
/// nodeName. These unused key values are reserved" -- which is why the parent IDs
/// below 16 are free for the reserved system files.
///
/// Mining reference: the layout above; the decode order follows
/// `core/hfs_endian.c`'s swap routine for `HFSPlusCatalogKey`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogKey {
    /// Parent folder CNID, or [`crate::catalog::cnid::ROOT_PARENT_ID`] for a
    /// thread record's own key.
    pub parent_id: crate::catalog::cnid::Cnid,
    /// The name as UTF-16 code units, without any length prefix.
    pub name: Vec<u16>,
    /// Total on-disk length of the key body, excluding the 2-byte prefix.
    pub key_length: usize,
}

impl CatalogKey {
    /// The `parentID` of a thread record's key.
    ///
    /// Every thread record shares this, which is what makes them one contiguous
    /// key range: an entire volume can be enumerated by scanning it.
    pub const THREAD_PARENT_ID: crate::catalog::cnid::Cnid = crate::catalog::cnid::Cnid(1);

    /// Decode a catalog key from a node record, which begins with the key.
    ///
    /// `max_key_length` is the tree's `maxKeyLength`, used to bound the
    /// declared length before it is used for anything.
    pub fn from_record(record: &[u8], max_key_length: usize) -> Result<Self> {
        if record.len() < CATALOG_KEY_OFFSET_SAFE {
            return Err(Error::Truncated {
                what: "catalog key",
                needed: CATALOG_KEY_OFFSET_SAFE,
                available: record.len(),
            });
        }
        let be = Be::new(record);
        let key_length = be.u16(0)? as usize;
        if key_length > max_key_length {
            return Err(Error::invalid(
                "HFSPlusCatalogKey.keyLength",
                format!("{key_length} exceeds the tree's maxKeyLength {max_key_length}"),
            ));
        }
        let declared = key_length
            .checked_add(CATALOG_KEY_PREFIX)
            .ok_or(Error::overflow("catalog key length"))?;
        if declared > record.len() {
            return Err(Error::Truncated {
                what: "catalog key",
                needed: declared,
                available: record.len(),
            });
        }

        let parent_id = crate::catalog::cnid::Cnid(be.u32(CATALOG_KEY_PARENT_OFFSET)?);
        let name_len = be.u16(CATALOG_KEY_NAME_LEN_OFFSET)? as usize;
        // `HFSUniStr255` holds at most 255 code units.
        let name_bytes = name_len
            .checked_mul(2)
            .ok_or(Error::overflow("catalog key name"))?;
        let name_end = CATALOG_KEY_NAME_OFFSET
            .checked_add(name_bytes)
            .ok_or(Error::overflow("catalog key name"))?;
        if name_end > declared {
            return Err(Error::Truncated {
                what: "catalog key name",
                needed: name_end,
                available: declared,
            });
        }

        let mut name = Vec::with_capacity(name_len);
        for i in 0..name_len {
            name.push(be.u16(CATALOG_KEY_NAME_OFFSET + i * 2)?);
        }

        Ok(CatalogKey {
            parent_id,
            name,
            key_length,
        })
    }

    /// Build a key for looking a name up inside `parent_id`.
    pub fn for_child(parent_id: crate::catalog::cnid::Cnid, name: &[u16]) -> Self {
        CatalogKey {
            parent_id,
            name: name.to_vec(),
            key_length: CATALOG_KEY_PARENT_SIZE + CATALOG_KEY_NAME_LEN_SIZE + name.len() * 2,
        }
    }

    /// Build the key a thread record for `name` uses.
    pub fn thread(name: &[u16]) -> Self {
        CatalogKey::for_child(Self::THREAD_PARENT_ID, name)
    }

    /// The key an empty thread record uses.
    ///
    /// Thread records are keyed by an empty name under `THREAD_PARENT_ID`.
    pub fn empty_thread() -> Self {
        CatalogKey::thread(&[])
    }

    /// The name as a `String`, for diagnostics only.
    ///
    /// Never compare with this. Use the `unicode` module's comparator.
    pub fn name_string(&self) -> String {
        String::from_utf16_lossy(&self.name)
    }

    /// Total on-disk size of the key, including prefix and the even-byte pad.
    ///
    /// Mining reference: `core/BTreeNodeOps.c` `InsertKeyRecord` rounds
    /// `keyLength + sizeof(prefix)` up to an even byte count.
    pub fn on_disk_size(&self) -> usize {
        let mut size = self.key_length + CATALOG_KEY_PREFIX;
        if size % 2 == 1 {
            size += 1;
        }
        size
    }

    /// Encode this key into a node record, appending a pad byte if needed.
    pub fn to_record(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.on_disk_size());
        out.extend_from_slice(&(self.key_length as u16).to_be_bytes());
        out.extend_from_slice(&self.parent_id.0.to_be_bytes());
        out.extend_from_slice(&(self.name.len() as u16).to_be_bytes());
        for unit in &self.name {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        if out.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    /// Whether this key belongs to the thread record range.
    pub fn is_thread_key(&self) -> bool {
        self.parent_id == Self::THREAD_PARENT_ID
    }
}

/// Minimum bytes needed to read a catalog key's length and parent fields.
const CATALOG_KEY_OFFSET_SAFE: usize = CATALOG_KEY_NAME_LEN_OFFSET;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::cnid::Cnid;

    /// A key must survive the on-disk encoding and come back identical.
    ///
    /// This is the question a case-folding search depends on. A folded lookup
    /// builds its search key by encoding a `CatalogKey` and decoding it again --
    /// `CatalogKey::from_record(&key.to_record(), max)` -- because that is the only
    /// constructor that goes through the same bytes the tree stores. If that round
    /// trip changed anything, the folded lookup would be searching for a key that is
    /// not the one it was asked for, and would report "no such key" for a key that is
    /// demonstrably there.
    ///
    /// The names below include one of even length and one of odd length in *code
    /// units*, because that is where `to_record`'s pad byte would appear if it
    /// appeared at all -- and the answer turns out to be that it never does, since a
    /// UTF-16 name is always a whole number of two-byte units.
    #[test]
    fn a_key_survives_being_encoded_and_decoded_again() {
        for name in [
            "".to_string(),
            "a".to_string(),
            "ab".to_string(),
            "README.TXT".to_string(),
            "Readme.txt".to_string(),
            "\u{4f60}\u{597d}".to_string(),
            "\u{1f600}".to_string(),
        ] {
            let key = CatalogKey::for_child(Cnid(2), &name.encode_utf16().collect::<Vec<_>>());
            let bytes = key.to_record();
            assert_eq!(
                bytes.len() % 2,
                0,
                "an encoded key must be a whole number of bytes, for {name:?}"
            );
            let back = CatalogKey::from_record(&bytes, 516).expect("decode");
            assert_eq!(back.parent_id, key.parent_id, "parentID, for {name:?}");
            assert_eq!(back.name, key.name, "name, for {name:?}");
            assert_eq!(back.key_length, key.key_length, "keyLength, for {name:?}");
            assert_eq!(
                back.on_disk_size(),
                key.on_disk_size(),
                "and therefore the size, for {name:?}"
            );
        }
    }

    #[test]
    fn key_offsets_match_the_struct() {
        // u16 keyLength, u32 parentID, HFSUniStr255{u16 length, UniChar[255]}
        assert_eq!(CATALOG_KEY_PARENT_OFFSET, 2);
        assert_eq!(CATALOG_KEY_NAME_LEN_OFFSET, 6);
        assert_eq!(CATALOG_KEY_NAME_OFFSET, 8);
        assert_eq!(
            CATALOG_KEY_PARENT_SIZE + CATALOG_KEY_NAME_LEN_SIZE + 255 * 2,
            516
        );
    }

    #[test]
    fn round_trips_through_a_record() {
        let key = CatalogKey::for_child(Cnid(2), &"TestVol".encode_utf16().collect::<Vec<_>>());
        let rec = key.to_record();
        let parsed = CatalogKey::from_record(&rec, 516).unwrap();
        assert_eq!(parsed, key);
        assert_eq!(parsed.name_string(), "TestVol");
    }

    #[test]
    fn round_trips_non_ascii_names() {
        let units: Vec<u16> = "café/日本".encode_utf16().collect();
        let key = CatalogKey::for_child(Cnid(16), &units);
        let parsed = CatalogKey::from_record(&key.to_record(), 516).unwrap();
        assert_eq!(parsed.name, units);
    }

    #[test]
    fn empty_thread_key_has_a_fixed_shape() {
        let key = CatalogKey::empty_thread();
        assert!(key.is_thread_key());
        assert_eq!(key.parent_id, Cnid(1));
        assert!(key.name.is_empty());
        // keyLength 6 (parentID + name length), prefix 2 => 8, already even.
        assert_eq!(key.key_length, 6);
        assert_eq!(key.on_disk_size(), 8);
        let parsed = CatalogKey::from_record(&key.to_record(), 516).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn odd_length_keys_get_a_pad_byte() {
        // name of 1 code unit: 4 + 2 + 2 = 8 key body; +2 prefix = 10, even.
        let one = CatalogKey::for_child(Cnid(2), &[0x41]);
        assert_eq!(one.key_length, 8);
        assert_eq!(one.on_disk_size(), 10);

        // A 3-unit name gives 4+2+6 = 12 body; +2 = 14, even.
        let three = CatalogKey::for_child(Cnid(2), &[0x41, 0x42, 0x43]);
        assert_eq!(three.key_length, 12);
        assert_eq!(three.on_disk_size(), 14);

        // key_length counts only the body, so the parity trick lives in on_disk_size.
        let rec = one.to_record();
        assert_eq!(rec.len(), one.on_disk_size());
    }

    #[test]
    fn rejects_a_key_longer_than_the_tree_allows() {
        let key = CatalogKey::for_child(Cnid(2), &"TestVol".encode_utf16().collect::<Vec<_>>());
        let rec = key.to_record();
        // maxKeyLength too small must be refused, not trusted away.
        assert!(matches!(
            CatalogKey::from_record(&rec, 4),
            Err(Error::InvalidField {
                field: "HFSPlusCatalogKey.keyLength",
                ..
            })
        ));
        assert!(CatalogKey::from_record(&rec, 516).is_ok());
    }

    #[test]
    fn rejects_a_declared_length_beyond_the_record() {
        let mut rec = CatalogKey::for_child(Cnid(2), &[0x41]).to_record();
        rec[0..2].copy_from_slice(&4000u16.to_be_bytes());
        // With a permissive max the length check passes and the record itself is
        // the thing that is too short.
        assert!(matches!(
            CatalogKey::from_record(&rec, u16::MAX as usize),
            Err(Error::Truncated { .. })
        ));
        // With the real max it is rejected as out of range instead.
        assert!(matches!(
            CatalogKey::from_record(&rec, 516),
            Err(Error::InvalidField { .. })
        ));
    }

    #[test]
    fn rejects_a_name_longer_than_the_key_declares() {
        // Claim 200 units inside a 6-byte key body.
        let mut rec = vec![0u8; 8];
        rec[0..2].copy_from_slice(&6u16.to_be_bytes());
        rec[2..6].copy_from_slice(&2u32.to_be_bytes());
        rec[6..8].copy_from_slice(&200u16.to_be_bytes());
        assert!(matches!(
            CatalogKey::from_record(&rec, 516),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn short_records_are_refused_not_panicked_on() {
        for len in [0usize, 1, 2, 5, 6, 7] {
            assert!(
                CatalogKey::from_record(&vec![0u8; len], 516).is_err(),
                "length {len}"
            );
        }
    }

    #[test]
    fn comparison_rule_comes_from_key_compare_type() {
        assert_eq!(
            NameComparison::from_key_compare_type(K_HFS_CASE_FOLDING),
            NameComparison::CaseFolding
        );
        assert_eq!(
            NameComparison::from_key_compare_type(K_HFS_BINARY_COMPARE),
            NameComparison::Binary
        );
        assert!(!NameComparison::CaseFolding.is_case_sensitive());
        assert!(NameComparison::Binary.is_case_sensitive());
        // An unrecognised value must not become case-sensitive.
        assert_eq!(
            NameComparison::from_key_compare_type(0),
            NameComparison::CaseFolding
        );
    }

    #[test]
    fn max_name_length_is_255_code_units() {
        // HFSUniStr255's length field is a u16 but Apple caps it at 255 units.
        let units: Vec<u16> = std::iter::repeat(0x41).take(255).collect();
        let key = CatalogKey::for_child(Cnid(16), &units);
        assert_eq!(key.key_length, 4 + 2 + 510);
        assert_eq!(key.key_length, 516);
        let rec = key.to_record();
        assert_eq!(rec.len(), 518);
        assert_eq!(CatalogKey::from_record(&rec, 516).unwrap(), key);
    }
}
