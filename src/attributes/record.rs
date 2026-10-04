//! What an attribute record holds: the value inline, in allocation blocks, or
//! continued.
//!
//! # Three record types, one union
//!
//! ```text
//! offset  size  meaning
//!      0     4  recordType -- also the first field of every variant
//! ```
//!
//! ```text
//! inline  (0x10)   reserved[2], attrSize, then the bytes
//! fork    (0x20)   reserved, then a full 80-byte HFSPlusForkData
//! extents (0x30)   reserved, then an 8-descriptor extent record
//! ```
//!
//! The inline form stores the value in the record, which is the common case and
//! the only one that costs no allocation. The fork form stores it in allocation
//! blocks and can be *continued* by further `extents` records, chained through
//! the key's `startBlock`. So a large attribute is several B-tree records, not one
//! — which means "an attribute has a value" and "an attribute is one record" are
//! different questions, and code that assumes the second will truncate the first.
//!
//! # The obsolete fourth type
//!
//! An older inline layout also used record type `0x10`, with the same three fixed
//! fields. Apple renamed the struct and kept the type value, so both spellings
//! decode identically here — the layout did not change, only the name.
//!
//! Mining reference: Apple `core/hfs_format.h`, the `kHFSPlusAttrInlineData`,
//! `kHFSPlusAttrForkData` and `kHFSPlusAttrExtents` enum, and the
//! `HFSPlusAttrData`, `HFSPlusAttrForkData` and `HFSPlusAttrExtents` structs
//! whose union is `HFSPlusAttrRecord`.

use crate::endian::Cursor;
use crate::error::{Error, Result};
use crate::format::extents::ExtentRecord;
use crate::format::fork::ForkData;

// Building fixtures field by field keeps each on-disk field visible next to the
// behaviour under test, so the struct-update lint is relaxed here -- as it is in
// the crate's other fixture modules.
/// Bytes before any variant's payload: the record type.
///
/// Mining reference: every attribute record struct begins with
/// `u_int32_t recordType`, and the union `HFSPlusAttrRecord` is read as a single
/// `u_int32_t` to select the variant.
pub const ATTR_RECORD_FIXED_SIZE: usize = 4;

/// Record type values, chosen not to collide with the catalog's.
///
/// Mining reference: the enum beside `HFSPlusAttrRecord` in `core/hfs_format.h`,
/// which states the values "were chosen so that they wouldn't conflict with the
/// catalog record types".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrRecordType {
    /// The value is inside the record.
    Inline,
    /// The value is in allocation blocks; more `Extents` records may follow.
    Fork,
    /// Further extents for the preceding `Fork` record.
    Extents,
}

impl AttrRecordType {
    /// Decode a record type, refusing unknown values.
    ///
    /// The catalog's own record types are deliberately *not* accepted: a
    /// mistyped attributes tree would otherwise decode a catalog record as an
    /// attribute.
    pub fn from_u32(raw: u32) -> Result<Self> {
        match raw {
            0x10 => Ok(AttrRecordType::Inline),
            0x20 => Ok(AttrRecordType::Fork),
            0x30 => Ok(AttrRecordType::Extents),
            other => Err(Error::invalid(
                "attribute recordType",
                format!("0x{other:x} is not an attributes record type"),
            )),
        }
    }

    /// The on-disk value for this type.
    pub fn to_u32(self) -> u32 {
        match self {
            AttrRecordType::Inline => 0x10,
            AttrRecordType::Fork => 0x20,
            AttrRecordType::Extents => 0x30,
        }
    }
}

/// One attribute record, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrRecord {
    /// The value is inside the record.
    Inline {
        /// The value's bytes. Never longer than the record allows.
        value: Vec<u8>,
    },
    /// The value is in allocation blocks described by the following extents.
    Fork {
        /// The fork's size and first eight extents.
        fork: ForkData,
    },
    /// Further extents for a preceding `Fork`.
    Extents {
        /// Eight more extents.
        extents: ExtentRecord,
    },
}

impl AttrRecord {
    /// This record's type.
    pub fn record_type(&self) -> AttrRecordType {
        match self {
            AttrRecord::Inline { .. } => AttrRecordType::Inline,
            AttrRecord::Fork { .. } => AttrRecordType::Fork,
            AttrRecord::Extents { .. } => AttrRecordType::Extents,
        }
    }

    /// Decode a record from a node record.
    pub fn from_record(record: &[u8]) -> Result<Self> {
        if record.len() < ATTR_RECORD_FIXED_SIZE + 4 {
            return Err(Error::Truncated {
                what: "attribute record",
                needed: ATTR_RECORD_FIXED_SIZE + 4,
                available: record.len(),
            });
        }
        let mut c = Cursor::new(record, "attribute record");
        let raw = c.u32()?;
        match AttrRecordType::from_u32(raw)? {
            AttrRecordType::Inline => {
                // recordType, reserved[2], attrSize, then the value. `reserved`
                // is an *array* of two, so attrSize is at 12 and the value at 16;
                // reading them four bytes early would find part of the reserved
                // field and take the length from the wrong place.
                const INLINE_FIXED: usize = 4 + 8 + 4;
                if record.len() < INLINE_FIXED {
                    return Err(Error::Truncated {
                        what: "inline attribute record",
                        needed: INLINE_FIXED,
                        available: record.len(),
                    });
                }
                c.skip(8)?;
                let attr_size = c.u32()? as usize;
                let available = record.len() - INLINE_FIXED;
                if attr_size > available {
                    return Err(Error::Truncated {
                        what: "inline attribute value",
                        needed: attr_size,
                        available,
                    });
                }
                Ok(AttrRecord::Inline {
                    value: record[INLINE_FIXED..INLINE_FIXED + attr_size].to_vec(),
                })
            }
            AttrRecordType::Fork => {
                // recordType, then reserved, then the fork.
                let fork = ForkData::from_bytes(&record[8..])?;
                Ok(AttrRecord::Fork { fork })
            }
            AttrRecordType::Extents => {
                // recordType, then reserved, then the extent record.
                let extents = ExtentRecord::from_bytes(&record[8..])?;
                Ok(AttrRecord::Extents { extents })
            }
        }
    }

    /// Encode this record.
    pub fn to_record(&self) -> Result<Vec<u8>> {
        match self {
            AttrRecord::Inline { value } => {
                let mut out = Vec::with_capacity(12 + value.len());
                out.extend_from_slice(&AttrRecordType::Inline.to_u32().to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&(value.len() as u32).to_be_bytes());
                out.extend_from_slice(value);
                Ok(out)
            }
            AttrRecord::Fork { fork } => {
                let mut out = Vec::with_capacity(4 + 4 + 80);
                out.extend_from_slice(&AttrRecordType::Fork.to_u32().to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&fork.to_bytes());
                Ok(out)
            }
            AttrRecord::Extents { extents } => {
                let mut out = Vec::with_capacity(4 + 4 + 64);
                out.extend_from_slice(&AttrRecordType::Extents.to_u32().to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&extents.to_bytes());
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible next to the
// behaviour under test, so the struct-update lint is relaxed here -- as it is in
// the crate's other fixture modules.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn the_type_values_do_not_collide_with_the_catalog() {
        // Apple's stated reason for choosing these numbers.
        assert_eq!(AttrRecordType::Inline.to_u32(), 0x10);
        assert_eq!(AttrRecordType::Fork.to_u32(), 0x20);
        assert_eq!(AttrRecordType::Extents.to_u32(), 0x30);
        // The catalog's types are 1..4 and 1,000 for folders, so none of these
        // can be mistaken for one.
        for catalog in [1u32, 2, 3, 4, 1000] {
            assert!(
                AttrRecordType::from_u32(catalog).is_err(),
                "catalog record type {catalog} must not decode as an attribute"
            );
        }
    }

    #[test]
    fn an_inline_value_round_trips() {
        for value in [
            Vec::new(),
            vec![0u8],
            b"com.apple.ResourceFork data".to_vec(),
            vec![0xABu8; 400],
        ] {
            let record = AttrRecord::Inline {
                value: value.clone(),
            }
            .to_record()
            .expect("encode");
            assert_eq!(
                AttrRecord::from_record(&record).expect("decode"),
                AttrRecord::Inline { value }
            );
        }
    }

    #[test]
    fn a_fork_and_an_extents_record_round_trip() {
        let mut fork = ForkData::default();
        fork.logical_size = 100_000;
        fork.total_blocks = 25;
        fork.extents.raw[0].start_block = 500;
        fork.extents.raw[0].block_count = 8;
        let record = AttrRecord::Fork { fork }.to_record().expect("encode");
        assert_eq!(record.len(), 4 + 4 + 80, "type, reserved, and a full fork");
        assert_eq!(
            AttrRecord::from_record(&record).expect("decode"),
            AttrRecord::Fork { fork }
        );

        let mut extents = ExtentRecord::default();
        extents.raw[3].start_block = 700;
        extents.raw[3].block_count = 4;
        let record = AttrRecord::Extents { extents }.to_record().expect("encode");
        assert_eq!(
            record.len(),
            4 + 4 + 64,
            "type, reserved, and eight descriptors"
        );
        assert_eq!(
            AttrRecord::from_record(&record).expect("decode"),
            AttrRecord::Extents { extents }
        );
    }

    #[test]
    fn a_value_longer_than_the_record_is_refused_not_truncated() {
        // The dangerous direction: returning a short value would look like a
        // successful read of a shorter attribute.
        let mut record = AttrRecord::Inline {
            value: b"abcd".to_vec(),
        }
        .to_record()
        .expect("encode");
        // attrSize lives at offset 12: recordType, then reserved[2].
        record[12..16].copy_from_slice(&4096u32.to_be_bytes());
        let err = AttrRecord::from_record(&record).expect_err("must be refused");
        assert!(
            err.to_string().contains("attribute value"),
            "the error must name the value, got {err}"
        );
    }

    #[test]
    fn an_unknown_record_type_is_refused() {
        // Long enough to clear the length guard, so the refusal is about the type.
        let mut record = 0x40u32.to_be_bytes().to_vec();
        record.resize(20, 0);
        let err = AttrRecord::from_record(&record).expect_err("must be refused");
        assert!(
            err.to_string().contains("not an attributes record type"),
            "got {err}"
        );
    }

    #[test]
    fn a_record_too_short_for_its_type_is_refused() {
        for len in 0..12usize {
            let mut record = AttrRecord::Inline { value: Vec::new() }
                .to_record()
                .expect("encode");
            record.truncate(len);
            assert!(
                AttrRecord::from_record(&record).is_err(),
                "an inline record truncated to {len} bytes must be refused"
            );
        }
    }
}
