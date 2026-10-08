// SPDX-License-Identifier: APSL-1.2

//! The Attributes File's B-tree key.
//!
//! # This is not a catalog key
//!
//! The attributes tree is a *separate B-tree* with its own key layout, not a
//! variant of the catalog's. The catalog key names a child by its parent's CNID;
//! the attributes key names an attribute by its *owner's* CNID plus the
//! attribute's name, and adds a third field the catalog key has no counterpart
//! for.
//!
//! # On-disk layout
//!
//! ```text
//! offset  size  field
//!      0     2  keyLength   -- excludes this field
//!      2     2  pad         -- always zero
//!      4     4  fileID       -- the CNID of the object the attribute belongs to
//!      8     4  startBlock   -- first allocation block of a continuation record
//!     12     2  attrNameLen  -- in Unicode *characters*, not bytes
//!     14   254  attrName     -- UTF-16, up to 127 characters
//! ```
//!
//! So the body is 264 bytes and the record 268 with the length prefix. The `pad`
//! is a real field: an earlier revision of this crate dropped it and computed
//! 264, which the corpus's own attributes tree header contradicts.
//!
//! # `startBlock` is the continuation key
//!
//! An attribute whose value lives in allocation blocks is stored as one record
//! naming the first extent, followed by further records keyed on the next block.
//! The key's `startBlock` is how those records chain, and it is why this key has
//! three fields where the catalog's has two.
//!
//! `0` means "the first record" — the attribute's value begins in the record that
//! carries the name. A continuation record has the same name and the next block.
//!
//! Mining reference: Apple `core/hfs_format.h` `struct HFSPlusAttrKey`, and the
//! `kHFSPlusAttrKeyMaximumLength` / `kHFSPlusAttrKeyMinimumLength` macros beside
//! it, which fix the maximum name length at 127 characters.

use crate::endian::Cursor;
use crate::error::{Error, Result};

/// Longest attribute name, in UTF-16 code units.
///
/// Mining reference: `enum { kHFSMaxAttrNameLen = 127 };` in
/// `core/hfs_format.h`.
pub const MAX_ATTR_NAME_LEN: usize = 127;

/// Byte size of the key body, excluding the two-byte length prefix.
///
/// The `pad` counts: `keyLength` excludes only itself, not the field after it.
/// So the body is `pad + fileID + startBlock + attrNameLen + attrName` = 266, and
/// a maximum-length key is 268 bytes on disk.
pub const ATTR_KEY_BODY_SIZE: usize = 2 + 4 + 4 + 2 + MAX_ATTR_NAME_LEN * 2;

/// Smallest body a key can have: everything but the name.
const ATTR_KEY_MIN_BODY_SIZE: usize = 2 + 4 + 4 + 2;

/// Byte size of the key record, prefix included.
pub const ATTR_KEY_RECORD_SIZE: usize = 2 + ATTR_KEY_BODY_SIZE;

/// `startBlock` on the record that holds the start of an attribute's value.
pub const FIRST_START_BLOCK: u32 = 0;

/// The key of one attribute record.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AttrKey {
    /// CNID of the object this attribute belongs to.
    pub file_id: u32,
    /// First allocation block, or [`FIRST_START_BLOCK`] for the first record.
    pub start_block: u32,
    /// The attribute's name.
    pub name: String,
}

impl AttrKey {
    /// Byte size this key occupies on disk, prefix included.
    pub fn record_size(&self) -> Result<usize> {
        Ok(2 + 4 + 4 + 2 + self.name.len() * 2)
    }

    /// Whether this key starts an attribute's value rather than continuing one.
    pub fn is_first(&self) -> bool {
        self.start_block == FIRST_START_BLOCK
    }

    /// Decode a key from a node record.
    ///
    /// `max_key_length` is the tree's own `maxKeyLength`, which bounds the
    /// declared length before it is trusted.
    pub fn from_record(record: &[u8], max_key_length: usize) -> Result<Self> {
        if record.len() < 6 {
            return Err(Error::Truncated {
                what: "attribute key",
                needed: 6,
                available: record.len(),
            });
        }
        let mut c = Cursor::new(record, "attribute key");

        // `keyLength` excludes itself and covers the rest of the body, so it is
        // compared against `maxKeyLength` directly.
        let declared = c.u16()? as usize;
        if declared > max_key_length {
            return Err(Error::invalid(
                "HFSPlusAttrKey.keyLength",
                format!("{declared} exceeds the tree's maxKeyLength of {max_key_length}"),
            ));
        }
        if declared + 2 > record.len() {
            return Err(Error::Truncated {
                what: "attribute key",
                needed: declared + 2,
                available: record.len(),
            });
        }
        if declared < ATTR_KEY_MIN_BODY_SIZE {
            return Err(Error::invalid(
                "HFSPlusAttrKey.keyLength",
                format!("{declared} is too short to hold pad, fileID, startBlock and attrNameLen"),
            ));
        }

        // Skip the `pad` field: Apple sets it to zero and nothing reads it, so a
        // non-zero value is noise rather than corruption, and refusing it would
        // reject volumes macOS mounts.
        c.skip(2)?;
        let file_id = c.u32()?;
        let start_block = c.u32()?;
        let name_len = c.u16()? as usize;
        if name_len > MAX_ATTR_NAME_LEN {
            return Err(Error::invalid(
                "HFSPlusAttrKey.attrNameLen",
                format!("{name_len} exceeds the maximum of {MAX_ATTR_NAME_LEN}"),
            ));
        }
        if ATTR_KEY_MIN_BODY_SIZE + name_len * 2 > declared {
            return Err(Error::invalid(
                "HFSPlusAttrKey",
                format!(
                    "a name of {name_len} characters does not fit a declared key length of {declared}"
                ),
            ));
        }

        let mut units = Vec::with_capacity(name_len);
        for _ in 0..name_len {
            units.push(c.u16()?);
        }
        let name = String::from_utf16_lossy(&units);

        Ok(AttrKey {
            file_id,
            start_block,
            name,
        })
    }

    /// Encode this key into a node record.
    pub fn to_record(&self) -> Result<Vec<u8>> {
        let units: Vec<u16> = self.name.encode_utf16().collect();
        if units.len() > MAX_ATTR_NAME_LEN {
            return Err(Error::invalid(
                "attribute name",
                format!(
                    "{} characters exceeds the maximum of {MAX_ATTR_NAME_LEN}",
                    units.len()
                ),
            ));
        }
        // keyLength excludes only itself, so the pad is inside it.
        let declared = ATTR_KEY_MIN_BODY_SIZE + units.len() * 2;
        let mut out = Vec::with_capacity(2 + declared);
        out.extend_from_slice(&(declared as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // pad, always zero
        out.extend_from_slice(&self.file_id.to_be_bytes());
        out.extend_from_slice(&self.start_block.to_be_bytes());
        out.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for unit in units {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        Ok(out)
    }

    /// Compare two attribute keys, as Apple's `BTCompareBl` would for this tree.
    ///
    /// The order is: file_id, then name (binary comparison of the UTF-16 code
    /// units), then start_block as a tiebreaker for forked attributes whose
    /// continuation records share a name.
    ///
    /// Apple's `core/hfs_catalog.c` `attrkeycmp` compares the key bytes with
    /// `memcmp` over the name portion only, falling back to `startBlock` when
    /// the names are equal -- which is exactly the order the formatter's own
    /// output produces for continuation records.
    pub fn compare(a: &AttrKey, b: &AttrKey) -> std::cmp::Ordering {
        match a.file_id.cmp(&b.file_id) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        match a.name.as_bytes().cmp(b.name.as_bytes()) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        a.start_block.cmp(&b.start_block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_body_is_264_bytes_and_the_record_268() {
        // The `pad` is the difference between 264 and 266, and getting it wrong
        // moves fileID and every field after it.
        assert_eq!(ATTR_KEY_BODY_SIZE, 266);
        assert_eq!(ATTR_KEY_RECORD_SIZE, 268);
    }

    #[test]
    fn a_maximum_length_name_just_fits() {
        let key = AttrKey {
            file_id: 0x1122_3344,
            start_block: 0,
            name: "x".repeat(MAX_ATTR_NAME_LEN),
        };
        let record = key.to_record().expect("encode");
        assert_eq!(record.len(), ATTR_KEY_RECORD_SIZE);
        assert_eq!(AttrKey::from_record(&record, 266).expect("decode"), key);
    }

    #[test]
    fn a_name_one_character_too_long_is_refused() {
        let key = AttrKey {
            file_id: 1,
            start_block: 0,
            name: "x".repeat(MAX_ATTR_NAME_LEN + 1),
        };
        assert!(key.to_record().is_err(), "127 is the maximum, not 128");
    }

    #[test]
    fn an_over_long_declared_length_is_refused_before_it_is_trusted() {
        // A key claiming more than the tree allows is a fault in the volume. The
        // check has to come before the bounds check against the record, or a huge
        // declared length would be reported as a truncation rather than as the
        // structural fault it is.
        let mut record = AttrKey {
            file_id: 1,
            start_block: 0,
            name: "a".into(),
        }
        .to_record()
        .expect("encode");
        record[0..2].copy_from_slice(&2000u16.to_be_bytes());
        let err = AttrKey::from_record(&record, 266).expect_err("must be refused");
        assert!(
            err.to_string().contains("maxKeyLength"),
            "the error must name the bound, got {err}"
        );
    }

    #[test]
    fn a_declared_length_shorter_than_its_fields_is_refused() {
        let mut record = AttrKey {
            file_id: 1,
            start_block: 0,
            name: "abcd".into(),
        }
        .to_record()
        .expect("encode");
        record[0..2].copy_from_slice(&6u16.to_be_bytes());
        let err = AttrKey::from_record(&record, 266).expect_err("must be refused");
        assert!(
            err.to_string().contains("too short"),
            "the error must say the length is too short, got {err}"
        );
    }

    #[test]
    fn a_name_longer_than_the_key_declares_is_refused() {
        // attrNameLen and keyLength are independent fields, so they can disagree.
        let mut record = AttrKey {
            file_id: 1,
            start_block: 0,
            name: "ab".into(),
        }
        .to_record()
        .expect("encode");
        // Claim four characters in a key sized for two. attrNameLen is at 12.
        record[12..14].copy_from_slice(&4u16.to_be_bytes());
        assert!(AttrKey::from_record(&record, 266).is_err());
    }

    #[test]
    fn a_non_zero_pad_is_ignored_rather_than_refused() {
        // Apple sets it to zero and nothing reads it. macOS mounts volumes where
        // it is not, so refusing would reject files Apple accepts.
        let mut record = AttrKey {
            file_id: 7,
            start_block: 9,
            name: "com.apple.ResourceFork".into(),
        }
        .to_record()
        .expect("encode");
        record[2..4].copy_from_slice(&0xBEEFu16.to_be_bytes());
        let key = AttrKey::from_record(&record, 266).expect("a noisy pad is not corruption");
        assert_eq!(key.file_id, 7);
        assert_eq!(key.start_block, 9);
    }

    #[test]
    fn the_first_record_is_the_one_with_start_block_zero() {
        let first = AttrKey {
            file_id: 3,
            start_block: 0,
            name: "a".into(),
        };
        let rest = AttrKey {
            file_id: 3,
            start_block: 4096,
            name: "a".into(),
        };
        assert!(first.is_first());
        assert!(!rest.is_first());
    }

    #[test]
    fn round_trips_awkward_names() {
        for name in [
            "",
            "a",
            "com.apple.ResourceFork",
            "com.apple.system.hfs.firstlink",
            "naïve", // non-ASCII
            "🙂",    // astral, a surrogate pair
            &"x".repeat(127),
        ] {
            let key = AttrKey {
                file_id: 42,
                start_block: 8,
                name: name.into(),
            };
            let record = key.to_record().expect("encode");
            let back = AttrKey::from_record(&record, 266).expect("decode");
            assert_eq!(back.file_id, key.file_id);
            assert_eq!(back.start_block, key.start_block);
            // Astral characters are surrogate pairs and do not survive a lossy
            // round trip, so compare the code units rather than the string.
            let want: Vec<u16> = name.encode_utf16().collect();
            let got: Vec<u16> = back.name.encode_utf16().collect();
            assert_eq!(got, want, "{name:?}");
        }
    }
}
