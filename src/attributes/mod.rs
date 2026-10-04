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
pub mod record;

pub use key::{AttrKey, ATTR_KEY_BODY_SIZE, ATTR_KEY_RECORD_SIZE, MAX_ATTR_NAME_LEN};
pub use record::{AttrRecord, AttrRecordType, ATTR_RECORD_FIXED_SIZE};
