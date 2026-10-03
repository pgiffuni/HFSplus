//! Every structure size the parser depends on, checked against Apple's
//! declarations.
//!
//! # Why this exists
//!
//! Two of these constants were wrong here, by the same kind of omission, and no
//! test in the suite noticed:
//!
//! - `EXTENT_KEY_MAX_LENGTH` was 8, because `struct HFSPlusExtentKey` was
//!   modelled as `keyLength + fileID + startBlock` and Apple's `forkType` and
//!   `pad` were dropped. It is 10.
//! - `ATTR_KEY_MAX_LENGTH` was 264, because `HFSPlusAttrKey`'s `pad` was
//!   dropped. It is 266.
//!
//! Both were invisible because a wrong *maximum* key length only shows up where a
//! key is decoded, and the corpus contains no overflowing file and no attribute:
//! both trees are allocated by `mkfs.hfsplus` but always empty. A wrong *size*
//! is worse, because it decides where the next field begins, and the fields after
//! a dropped one are silently shifted.
//!
//! So the constants are asserted here against the arithmetic of Apple's
//! declarations, written out field by field rather than as a single number, so a
//! reader can see which field the crate believes is where. Where the corpus
//! carries the value -- the three B-tree key lengths, and the catalog file record
//! -- `tests/btree_conformance.rs` checks it against the formatter's own output.
//!
//! Mining reference: `core/hfs_format.h` for `HFSPlusExtentKey`,
//! `HFSPlusAttrKey`, `HFSPlusCatalogKey`, `HFSPlusCatalogFolder`,
//! `HFSPlusCatalogFile`, `HFSPlusCatalogThread`, `HFSPlusForkData`,
//! `HFSPlusExtentRecord`, `HFSPlusVolumeHeader`, `BTNodeDescriptor`,
//! `BTHeaderRec`, `JournalInfoBlock` and `HFSPlusBTreeNode`; all the
//! `kHFSPlus*KeyMaximumLength` values are `sizeof(Key) - sizeof(u_int16_t)`.

use hfsplus::btree::header::HEADER_RECORD_SIZE;
use hfsplus::btree::key::{
    ATTR_KEY_MAX_LENGTH, BIG_KEY_PREFIX, CATALOG_KEY_MAX_LENGTH, EXTENT_KEY_MAX_LENGTH,
    ExtentKey,
};
use hfsplus::btree::node::NODE_DESCRIPTOR_SIZE;
use hfsplus::catalog::record::{
    FILE_RECORD_SIZE, FOLDER_RECORD_SIZE, THREAD_RECORD_FIXED_SIZE,
};
use hfsplus::format::extents::EXTENT_RECORD_SIZE;
use hfsplus::format::fork::FORK_DATA_SIZE;
use hfsplus::format::volume_header::VOLUME_HEADER_SIZE;
use hfsplus::journal::info::JOURNAL_INFO_BLOCK_SIZE;

// --- Keys ---------------------------------------------------------------

/// `struct HFSPlusCatalogKey`: `u16 keyLength + u32 parentID + HFSUniStr255`.
///
/// `HFSUniStr255` is `u16 length + UniChar unicode[255]`, so 2 + 510 = 512. The
/// 255 is a name-length limit, not a field width: writing 127 here is the kind of
/// mistake that yields a key 256 bytes short.
#[test]
fn the_catalog_key_is_516_bytes() {
    const UNISTR255_NAME_LIMIT: usize = 255;
    const UNISTR255: usize = 2 + UNISTR255_NAME_LIMIT * 2;
    let body = 4 + UNISTR255;
    assert_eq!(CATALOG_KEY_MAX_LENGTH, body);
    assert_eq!(CATALOG_KEY_MAX_LENGTH, 516);
    // The record with its length prefix.
    assert_eq!(CATALOG_KEY_MAX_LENGTH + BIG_KEY_PREFIX, 518);
}

/// `struct HFSPlusExtentKey`: `u16 keyLength + u8 forkType + u8 pad + u32
/// fileID + u32 startBlock`.
#[test]
fn the_extents_key_is_10_bytes_and_12_on_disk() {
    let body = 1 + 1 + 4 + 4;
    assert_eq!(EXTENT_KEY_MAX_LENGTH, body, "forkType and pad are fields");
    assert_eq!(EXTENT_KEY_MAX_LENGTH, 10);
    assert_eq!(ExtentKey::ON_DISK_SIZE, EXTENT_KEY_MAX_LENGTH + BIG_KEY_PREFIX);
    assert_eq!(ExtentKey::ON_DISK_SIZE, 12);
}

/// `struct HFSPlusAttrKey`: `u16 keyLength + u16 pad + u32 fileID + u32
/// startBlock + u16 attrNameLen + u16 attrName[127]`.
#[test]
fn the_attributes_key_is_266_bytes() {
    const MAX_ATTR_NAME_LEN: usize = 127;
    let body = 2 + 4 + 4 + 2 + MAX_ATTR_NAME_LEN * 2;
    assert_eq!(ATTR_KEY_MAX_LENGTH, body, "pad is a field, not alignment");
    assert_eq!(ATTR_KEY_MAX_LENGTH, 266);
    assert_eq!(ATTR_KEY_MAX_LENGTH + BIG_KEY_PREFIX, 268);
}

// --- Catalog records ----------------------------------------------------

/// `struct HFSPlusCatalogFolder`: `u16 recordType + u16 flags + u32 valence +
/// u32 folderID + five u32 dates + HFSPlusBSDInfo + FndrDirInfo + FndrOpaqueInfo
/// + u32 textEncoding + u32 reserved`.
#[test]
fn the_folder_record_is_88_bytes() {
    const BSD_INFO: usize = 16;
    const FOLDER_INFO: usize = 16;
    const OPAQUE_INFO: usize = 16;
    let body = 2 + 2 + 4 + 4 + 5 * 4 + BSD_INFO + FOLDER_INFO + OPAQUE_INFO + 4 + 4;
    assert_eq!(FOLDER_RECORD_SIZE, body);
    assert_eq!(FOLDER_RECORD_SIZE, 88);
}

/// `struct HFSPlusCatalogFile`: as the folder record, but `FndrFileInfo` and two
/// `HFSPlusForkData` instead of `FndrDirInfo` and nothing.
///
/// The forks must start on a double-long boundary, which the 88 bytes above
/// happen to satisfy, so the sizes add without padding.
#[test]
fn the_file_record_is_248_bytes_and_the_forks_land_aligned() {
    const BSD_INFO: usize = 16;
    const FILE_INFO: usize = 16;
    const OPAQUE_INFO: usize = 16;
    let before_forks = 2 + 2 + 4 + 4 + 5 * 4 + BSD_INFO + FILE_INFO + OPAQUE_INFO + 4 + 4;
    assert_eq!(
        before_forks, 88,
        "the scalar and info fields total the same as a folder record"
    );
    assert_eq!(before_forks % 8, 0, "and the forks need 8-byte alignment");
    assert_eq!(FORK_DATA_SIZE, 80);
    assert_eq!(FILE_RECORD_SIZE, before_forks + 2 * FORK_DATA_SIZE);
    assert_eq!(FILE_RECORD_SIZE, 248);
}

/// `struct HFSPlusCatalogThread`: `u16 recordType + u16 reserved + u32 parentID`
/// then `HFSUniStr255`, so the fixed part is 8 and the name follows.
#[test]
fn the_thread_record_fixed_part_is_8_bytes() {
    assert_eq!(THREAD_RECORD_FIXED_SIZE, 8);
}

/// `struct HFSPlusExtentRecord`: `kHFSPlusExtentDensity` (8) extent descriptors
/// of `u32 startBlock + u32 blockCount`.
#[test]
fn the_extent_record_is_64_bytes() {
    assert_eq!(EXTENT_RECORD_SIZE, 8 * 8);
    assert_eq!(EXTENT_RECORD_SIZE, 64);
}

/// `struct HFSPlusForkData`: `u64 logicalSize + u32 clumpSize + u32 totalBlocks`
/// then 8 extent descriptors.
#[test]
fn the_fork_data_is_80_bytes() {
    assert_eq!(FORK_DATA_SIZE, 8 + 4 + 4 + EXTENT_RECORD_SIZE);
    assert_eq!(FORK_DATA_SIZE, 80);
}

// --- B-tree -------------------------------------------------------------

/// `struct BTNodeDescriptor` is 14 bytes, and a node's records start there.
#[test]
fn the_node_descriptor_is_14_bytes() {
    let descriptor = 4 + 4 + 1 + 1 + 2 + 2; // fLink, bLink, kind, height, numRecords, reserved
    assert_eq!(NODE_DESCRIPTOR_SIZE, descriptor);
    assert_eq!(NODE_DESCRIPTOR_SIZE, 14);
}

/// `struct BTHeaderRec`. Note `reserved1` is a `u_int16_t`, not a `u_int32_t`;
/// getting that wrong would put `clumpSize` two bytes early and everything after
/// it further out still.
#[test]
fn the_btree_header_record_is_106_bytes() {
    let size = 2        // treeDepth
        + 4            // rootNode
        + 4            // leafRecords
        + 4            // firstLeafNode
        + 4            // lastLeafNode
        + 2            // nodeSize
        + 2            // maxKeyLength
        + 4            // totalNodes
        + 4            // freeNodes
        + 2            // reserved1, a u_int16_t
        + 4            // clumpSize
        + 1            // btreeType
        + 1            // keyCompareType
        + 4            // attributes
        + 16 * 4;      // reserved3
    assert_eq!(size, 106);
    assert_eq!(HEADER_RECORD_SIZE, size);
}

// --- Volume header and journal ------------------------------------------

/// `struct HFSPlusVolumeHeader` is one sector, and its five forks follow it.
#[test]
fn the_volume_header_is_one_sector_and_the_forks_follow_it() {
    assert_eq!(VOLUME_HEADER_SIZE, 512);
    // 512 bytes of scalars, then five 80-byte fork records: 112 + 5 * 80 = 512,
    // which is what puts the allocation fork at 1024 + 112.
    assert_eq!(112 + 5 * FORK_DATA_SIZE, 512);
}

/// `struct JournalInfoBlock`: `u32 flags + u32 device_signature[8] + u64 offset
/// + u64 size + uuid_string_t + char[48] + reserved`.
///
/// `JIB_RESERVED_SIZE` is `(32 * sizeof(u_int32_t)) - sizeof(uuid_string_t) - 48`
/// — Apple deliberately shrank the reserved field so that adding the UUID and the
/// serial number did not change the struct's size. With a 16-byte
/// `uuid_string_t` that is 64.
#[test]
fn the_journal_info_block_is_180_bytes_with_the_reserved_field_shrunk() {
    const UUID_STRING: usize = 16;
    const RESERVED: usize = 32 * 4 - UUID_STRING - 48;

    let size = 4 + 8 * 4 + 8 + 8 + UUID_STRING + 48 + RESERVED;
    assert_eq!(RESERVED, 64, "128 less the UUID and the 48-byte serial number");
    assert_eq!(size, 180);
    assert_eq!(JOURNAL_INFO_BLOCK_SIZE, size);

    // The offsets the parser actually reads, and the two the decomposition above
    // places. Getting the UUID's offset wrong by 21 is possible if the reserved
    // field is assumed to be a fixed 43 or 37 bytes instead of being derived,
    // and nothing else in the crate would notice.
    use hfsplus::journal::info::{JIB_OFFSET_OFFSET, JIB_SIZE_OFFSET};
    assert_eq!(JIB_OFFSET_OFFSET, 36);
    assert_eq!(JIB_SIZE_OFFSET, 44);
    assert_eq!(JIB_SIZE_OFFSET + 8, 52, "which is where the UUID begins");
}