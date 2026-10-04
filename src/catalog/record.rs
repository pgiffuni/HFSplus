//! Catalog records: folders, files and thread records.
//!
//! Mining reference: Apple `core/hfs_format.h` for the layouts, and
//! `core/hfs_catalog.c` for how they are built and read. Apple names the
//! builders `buildrecord`, `builddesc`, `buildthread` and `buildkey`, and the
//! readers `cat_lookup`, `cat_idlookup` and `cat_getdirentries`.
//!
//! # The four record types
//!
//! ```c
//! enum {
//!     kHFSPlusFolderRecord       = 1,   /* Folder record */
//!     kHFSPlusFileRecord         = 2,   /* File record */
//!     kHFSPlusFolderThreadRecord = 3,   /* Folder thread record */
//!     kHFSPlusFileThreadRecord   = 4    /* File thread record */
//! };
//! ```
//!
//! The type is the first `i16` of the record body, which follows the key.
//!
//! # Thread records are the reason a directory can be walked
//!
//! For every file and folder, the catalog also stores a *thread record* whose
//! key is `(parentID = kHFSRootParentID, name = empty)`. A thread record's body
//! holds the object's CNID and its full name. Because every thread record shares
//! the same key, they sort into one contiguous range at the very start of the
//! catalog, so a full inventory of the volume can be produced by scanning that
//! one range instead of walking every directory.
//!
//! The thread record for the *folder containing an object* is a different thing:
//! it is the record whose key is the folder's own name and whose parentID is the
//! containing folder, and it repeats the folder's CNID and name. That is what
//! resolves `.` and `..` without a search.

use super::cnid::Cnid;
use crate::endian::{Be, Cursor};
use crate::error::{Error, Result};
use crate::format::fork::ForkData;
use crate::timestamp::HfsTimestamp;

/// `kHFSPlusFolderRecord`.
pub const K_HFS_PLUS_FOLDER_RECORD: i16 = 1;

/// `kHFSPlusFileRecord`.
pub const K_HFS_PLUS_FILE_RECORD: i16 = 2;

/// `kHFSPlusFolderThreadRecord`.
pub const K_HFS_PLUS_FOLDER_THREAD_RECORD: i16 = 3;

/// `kHFSPlusFileThreadRecord`.
pub const K_HFS_PLUS_FILE_THREAD_RECORD: i16 = 4;

/// Byte size of a catalog folder record.
pub const FOLDER_RECORD_SIZE: usize = 88;

/// Byte size of a catalog file record.
pub const FILE_RECORD_SIZE: usize = 248;

/// Byte size of a thread record before the `HFSUniStr255` name.
///
/// `recordType` (2) + `reserved` (2) + `parentID` (4). The name's own `u16`
/// length follows at this offset, so the first code unit is 10 bytes in.
pub const THREAD_RECORD_FIXED_SIZE: usize = 8;

/// Byte offset of the `HFSUniStr255` length within a thread record.
pub const THREAD_RECORD_NAME_LEN_OFFSET: usize = THREAD_RECORD_FIXED_SIZE;

/// Byte offset of the first UTF-16 code unit within a thread record.
pub const THREAD_RECORD_NAME_OFFSET: usize = THREAD_RECORD_FIXED_SIZE + 2;

/// Byte size of `struct HFSPlusBSDInfo`.
pub const BSD_INFO_SIZE: usize = 16;

/// Byte size of `struct FndrDirInfo` / `struct FndrFileInfo`.
pub const FINDER_USER_INFO_SIZE: usize = 16;

/// Byte size of `struct FndrOpaqueInfo`.
pub const FINDER_OPAQUE_INFO_SIZE: usize = 16;

/// `kHFSHasAttributesMask`: the object has extended attributes.
pub const K_HFS_HAS_ATTRIBUTES_MASK: u16 = 0x0004;

/// `kHFSHasSecurityMask`: the object has ACLs.
pub const K_HFS_HAS_SECURITY_MASK: u16 = 0x0008;

/// `kHFSHasFolderCountMask`: an HFSX folder keeps a separate sub-folder count.
///
/// This flag exists only on HFSX, and it makes `folderCount` meaningful. On HFS+
/// the field is always zero and the flag is never set.
pub const K_HFS_HAS_FOLDER_COUNT_MASK: u16 = 0x0010;

/// `kHFSHasLinkChainMask`: the object belongs to a hard link chain.
pub const K_HFS_HAS_LINK_CHAIN_MASK: u16 = 0x0020;

/// `kHFSHasChildLinkMask`: an HFSX folder has a child that is a directory link.
pub const K_HFS_HAS_CHILD_LINK_MASK: u16 = 0x0040;

/// `kHFSHasDateAddedMask`: the Finder info carries a date-added timestamp.
pub const K_HFS_HAS_DATE_ADDED_MASK: u16 = 0x0080;

/// `kHFSCatExpandedTimesMask`: **this item** uses expanded, non-MacOS timestamps.
///
/// Per-item, and distinct from the volume-wide `kHFSExpandedTimesMask`. A
/// volume in classic Mac time can hold individual items stamped in expanded
/// time, and this bit is the only thing that says so. Mining reference: Apple
/// `core/hfs_format.h`, and `core/hfs_catalog.c`, which ORs
/// `kHFSCatExpandedTimesMask` into a file's `ca_flags` when it decodes the dates
/// so that `hfs_getattrlist` reports the right semantics for that one item.
pub const K_HFS_CAT_EXPANDED_TIMES_MASK: u16 = 0x1000;

/// POSIX `S_IFMT` mask, from the mode word of `HFSPlusBSDInfo`.
pub const S_IFMT: u16 = 0o170000;
/// `S_IFDIR`.
pub const S_IFDIR: u16 = 0o040000;
/// `S_IFREG`.
pub const S_IFREG: u16 = 0o100000;
/// `S_IFLNK`.
pub const S_IFLNK: u16 = 0o120000;
/// `S_IFBLK`.
pub const S_IFBLK: u16 = 0o060000;
/// `S_IFCHR`.
pub const S_IFCHR: u16 = 0o020000;
/// `S_IFIFO`.
pub const S_IFIFO: u16 = 0o010000;
/// `S_IFSOCK`.
pub const S_IFSOCK: u16 = 0o140000;

/// `SF_IMMUTABLE`.
pub const SF_IMMUTABLE: u8 = 0x0001;
/// `SF_APPEND`.
pub const SF_APPEND: u8 = 0x0004;
/// `UF_NODUMP`.
pub const UF_NODUMP: u8 = 0x0001;
/// `UF_IMMUTABLE`.
pub const UF_IMMUTABLE: u8 = 0x0002;
/// `UF_APPEND`.
pub const UF_APPEND: u8 = 0x0004;
/// `UF_OPAQUE`.
pub const UF_OPAQUE: u8 = 0x0008;
/// `UF_NODEV`: do not interpret device special files.
pub const UF_NODEV: u8 = 0x0010;

/// Decoded `struct HFSPlusBSDInfo`.
///
/// Mining reference: Apple `core/hfs_format.h`:
///
/// ```c
/// struct HFSPlusBSDInfo {
///     u_int32_t     ownerID;    /* user-id of owner or hard link chain previous link */
///     u_int32_t     groupID;    /* group-id of owner or hard link chain next link */
///     u_int8_t     adminFlags;
///     u_int8_t     ownerFlags;
///     u_int16_t     fileMode;
///     union { iNodeNum; linkCount; rawDevice; } special;
/// };
/// ```
///
/// The comment on `ownerID` is the key to the whole struct: for an object in a
/// hard link chain these two fields are **not** a uid and gid but the previous
/// and next link in the chain. See [`FileRecord::is_hard_link`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BsdInfo {
    /// `ownerID`, or `prevLinkID` for a hard link.
    pub owner_id: u32,
    /// `groupID`, or `nextLinkID` for a hard link.
    pub group_id: u32,
    /// `adminFlags`: super-user-changeable flags.
    pub admin_flags: u8,
    /// `ownerFlags`: owner-changeable flags.
    pub owner_flags: u8,
    /// `fileMode`: file type and permission bits.
    pub file_mode: u16,
    /// `special`: link count, indirect node number, or device number.
    pub special: u32,
}

impl BsdInfo {
    /// Parse from the 16 bytes at `off` within `bytes`.
    pub fn parse(bytes: &[u8], off: usize) -> Result<Self> {
        let be = Be::new(bytes);
        Ok(BsdInfo {
            owner_id: be.u32(off)?,
            group_id: be.u32(off + 4)?,
            admin_flags: be.u8(off + 8)?,
            owner_flags: be.u8(off + 9)?,
            file_mode: be.u16(off + 10)?,
            special: be.u32(off + 12)?,
        })
    }

    /// The `S_IFMT` portion of the mode.
    pub fn file_type(&self) -> u16 {
        self.file_mode & S_IFMT
    }

    /// Encode into `out`, which must be at least [`BSD_INFO_SIZE`] bytes.
    ///
    /// The inverse of [`BsdInfo::parse`]. Needed because a mutation has to write
    /// a record back: `fileMode` is how a file becomes a directory or a symlink,
    /// so a writer that could not write it could not change a file's type.
    pub fn write_to(&self, out: &mut [u8]) -> Result<()> {
        let available = out.len();
        let dst = out.get_mut(..BSD_INFO_SIZE).ok_or(Error::Truncated {
            what: "bsd info write",
            needed: BSD_INFO_SIZE,
            available,
        })?;
        dst[0..4].copy_from_slice(&self.owner_id.to_be_bytes());
        dst[4..8].copy_from_slice(&self.group_id.to_be_bytes());
        dst[8] = self.admin_flags;
        dst[9] = self.owner_flags;
        dst[10..12].copy_from_slice(&self.file_mode.to_be_bytes());
        dst[12..BSD_INFO_SIZE].copy_from_slice(&self.special.to_be_bytes());
        Ok(())
    }

    /// Whether the file type is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }

    /// Whether the file type is a regular file.
    pub fn is_regular(&self) -> bool {
        self.file_type() == S_IFREG
    }

    /// Whether the file type is a symbolic link.
    ///
    /// HFS stores symlinks as files whose data fork holds the target path.
    /// Mining reference: Apple `core/hfs_vnodeops.c` treats a file with
    /// `S_IFLNK` as a symlink and reads its data fork as the target.
    pub fn is_symlink(&self) -> bool {
        self.file_type() == S_IFLNK
    }

    /// Permission bits, without the file type.
    pub fn permissions(&self) -> u16 {
        self.file_mode & !S_IFMT
    }

    /// Whether the file mode marks this object as a symlink.
    pub fn mode_is_symlink(&self) -> bool {
        self.is_symlink()
    }
}

/// A catalog record, as parsed from a leaf node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogRecord {
    /// `kHFSPlusFolderRecord`.
    Folder(FolderRecord),
    /// `kHFSPlusFileRecord`.
    File(Box<FileRecord>),
    /// A thread record, of either flavour.
    Thread(ThreadRecord),
}

impl CatalogRecord {
    /// The record type word.
    pub fn record_type(&self) -> i16 {
        match self {
            CatalogRecord::Folder(_) => K_HFS_PLUS_FOLDER_RECORD,
            CatalogRecord::File(_) => K_HFS_PLUS_FILE_RECORD,
            CatalogRecord::Thread(t) => t.record_type,
        }
    }

    /// The record's CNID, for the record types that carry one in their body.
    ///
    /// Thread records return `None`: their body holds the object's *parent*, and
    /// the object's own CNID lives in the thread record's key. A caller that has
    /// a thread record must read the key it was found under.
    pub fn cnid(&self) -> Option<Cnid> {
        match self {
            CatalogRecord::Folder(f) => Some(f.folder_id),
            CatalogRecord::File(f) => Some(f.file_id),
            CatalogRecord::Thread(_) => None,
        }
    }

    /// Whether this is a thread record.
    pub fn is_thread(&self) -> bool {
        matches!(self, CatalogRecord::Thread(_))
    }
}

/// `struct HFSPlusCatalogFolder`, 88 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderRecord {
    /// The record type word, always [`K_HFS_PLUS_FOLDER_RECORD`].
    pub record_type: i16,
    /// User flags; see the `kHFS*Mask` constants in this module.
    pub flags: u16,
    /// Number of children, counting reserved pseudo-files.
    pub valence: u32,
    /// This folder's CNID.
    pub folder_id: Cnid,
    /// `createDate`, raw.
    pub create_date: u32,
    /// `contentModDate`, raw.
    pub content_mod_date: u32,
    /// `attributeModDate`, raw.
    pub attribute_mod_date: u32,
    /// `accessDate`, raw.
    pub access_date: u32,
    /// `backupDate`, raw.
    pub backup_date: u32,
    /// Permissions and ownership.
    pub bsd_info: BsdInfo,
    /// 16 bytes of `FndrDirInfo`.
    pub user_info: [u8; FINDER_USER_INFO_SIZE],
    /// 16 bytes of `FndrOpaqueInfo`, whose first four bytes are the
    /// `FndrExtendedDirInfo` `date_added` on volumes that set
    /// [`K_HFS_HAS_DATE_ADDED_MASK`].
    pub finder_info: [u8; FINDER_OPAQUE_INFO_SIZE],
    /// Text encoding hint for this folder's name.
    pub text_encoding: u32,
    /// Sub-folder count; only meaningful with [`K_HFS_HAS_FOLDER_COUNT_MASK`],
    /// which only HFSX sets.
    pub folder_count: u32,
}

impl FolderRecord {
    /// An all-zero folder record.
    ///
    /// Only for constructing a placeholder in code paths that already filtered
    /// thread records out; a real folder record is never zero.
    pub const EMPTY: FolderRecord = FolderRecord {
        record_type: K_HFS_PLUS_FOLDER_RECORD,
        flags: 0,
        valence: 0,
        folder_id: Cnid(0),
        create_date: 0,
        content_mod_date: 0,
        attribute_mod_date: 0,
        access_date: 0,
        backup_date: 0,
        bsd_info: BsdInfo {
            owner_id: 0,
            group_id: 0,
            admin_flags: 0,
            owner_flags: 0,
            file_mode: 0,
            special: 0,
        },
        user_info: [0; 16],
        finder_info: [0; 16],
        text_encoding: 0,
        folder_count: 0,
    };

    /// Parse a 88-byte folder record body.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < FOLDER_RECORD_SIZE {
            return Err(Error::Truncated {
                what: "catalog folder record",
                needed: FOLDER_RECORD_SIZE,
                available: bytes.len(),
            });
        }
        let be = Be::new(bytes);
        let mut user_info = [0u8; FINDER_USER_INFO_SIZE];
        let mut finder_info = [0u8; FINDER_OPAQUE_INFO_SIZE];
        user_info.copy_from_slice(bytes.get(48..64).ok_or(Error::Truncated {
            what: "folder userInfo",
            needed: 64,
            available: bytes.len(),
        })?);
        finder_info.copy_from_slice(bytes.get(64..80).ok_or(Error::Truncated {
            what: "folder finderInfo",
            needed: 80,
            available: bytes.len(),
        })?);

        Ok(FolderRecord {
            record_type: be.i16(0)?,
            flags: be.u16(2)?,
            valence: be.u32(4)?,
            folder_id: Cnid(be.u32(8)?),
            create_date: be.u32(12)?,
            content_mod_date: be.u32(16)?,
            attribute_mod_date: be.u32(20)?,
            access_date: be.u32(24)?,
            backup_date: be.u32(28)?,
            bsd_info: BsdInfo::parse(bytes, 32)?,
            user_info,
            finder_info,
            text_encoding: be.u32(80)?,
            folder_count: be.u32(84)?,
        })
    }
    /// Encode into `out`, which must be at least [`FOLDER_RECORD_SIZE`] bytes.
    ///
    /// The inverse of [`FolderRecord::parse`]. `valence` is the reason this
    /// exists: a folder's child count lives in its record, so creating a file
    /// inside one means writing the folder back, and a `valence` written as
    /// anything but a count makes the folder read as having children that do not
    /// exist.
    pub fn write_to(&self, out: &mut [u8]) -> Result<()> {
        let available = out.len();
        let dst = out.get_mut(..FOLDER_RECORD_SIZE).ok_or(Error::Truncated {
            what: "folder record write",
            needed: FOLDER_RECORD_SIZE,
            available,
        })?;
        dst.fill(0);
        dst[0..2].copy_from_slice(&self.record_type.to_be_bytes());
        dst[2..4].copy_from_slice(&self.flags.to_be_bytes());
        dst[4..8].copy_from_slice(&self.valence.to_be_bytes());
        dst[8..12].copy_from_slice(&self.folder_id.0.to_be_bytes());
        dst[12..16].copy_from_slice(&self.create_date.to_be_bytes());
        dst[16..20].copy_from_slice(&self.content_mod_date.to_be_bytes());
        dst[20..24].copy_from_slice(&self.attribute_mod_date.to_be_bytes());
        dst[24..28].copy_from_slice(&self.access_date.to_be_bytes());
        dst[28..32].copy_from_slice(&self.backup_date.to_be_bytes());
        self.bsd_info.write_to(&mut dst[32..48])?;
        dst[48..64].copy_from_slice(&self.user_info);
        dst[64..80].copy_from_slice(&self.finder_info);
        dst[80..84].copy_from_slice(&self.text_encoding.to_be_bytes());
        // dst[84..88] is reserved3, zeroed by the fill above.
        Ok(())
    }

    /// Encode to a [`FOLDER_RECORD_SIZE`]-byte array.
    pub fn to_bytes(&self) -> [u8; FOLDER_RECORD_SIZE] {
        let mut out = [0u8; FOLDER_RECORD_SIZE];
        // Writing into a correctly sized array cannot fail.
        let _ = self.write_to(&mut out);
        out
    }
}

/// `struct HFSPlusCatalogFile`, 248 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileRecord {
    /// The record type word, always [`K_HFS_PLUS_FILE_RECORD`].
    pub record_type: i16,
    /// User flags; see the `kHFS*Mask` constants in this module.
    pub flags: u16,
    /// Reserved, initialised to zero.
    ///
    /// Mining reference: `core/hfs_format.h` aliases this to `hl_firstLinkID`,
    /// so it carries the first link of a hard link chain when
    /// [`K_HFS_HAS_LINK_CHAIN_MASK`] is set.
    pub reserved1: u32,
    /// This file's CNID.
    pub file_id: Cnid,
    /// `createDate`, raw.
    pub create_date: u32,
    /// `contentModDate`, raw.
    pub content_mod_date: u32,
    /// `attributeModDate`, raw.
    pub attribute_mod_date: u32,
    /// `accessDate`, raw.
    pub access_date: u32,
    /// `backupDate`, raw.
    pub backup_date: u32,
    /// Permissions and ownership.
    pub bsd_info: BsdInfo,
    /// 16 bytes of `FndrFileInfo`: `fdType`, `fdCreator`, `fdFlags`, location.
    pub user_info: [u8; FINDER_USER_INFO_SIZE],
    /// 16 bytes of `FndrOpaqueInfo`, whose first four bytes are the
    /// `FndrExtendedFileInfo` `date_added` on volumes that set
    /// [`K_HFS_HAS_DATE_ADDED_MASK`].
    pub finder_info: [u8; FINDER_OPAQUE_INFO_SIZE],
    /// Text encoding hint for this file's name.
    pub text_encoding: u32,
    /// Reserved, initialised to zero.
    pub reserved2: u32,
    /// The data fork.
    pub data_fork: ForkData,
    /// The resource fork.
    pub resource_fork: ForkData,
}

impl FileRecord {
    /// An all-zero file record.
    ///
    /// Only for constructing a placeholder where a thread record has already been
    /// filtered out; a real file record is never zero.
    pub const EMPTY: FileRecord = FileRecord {
        record_type: K_HFS_PLUS_FILE_RECORD,
        flags: 0,
        reserved1: 0,
        file_id: Cnid(0),
        create_date: 0,
        content_mod_date: 0,
        attribute_mod_date: 0,
        access_date: 0,
        backup_date: 0,
        bsd_info: BsdInfo {
            owner_id: 0,
            group_id: 0,
            admin_flags: 0,
            owner_flags: 0,
            file_mode: 0,
            special: 0,
        },
        user_info: [0; 16],
        finder_info: [0; 16],
        text_encoding: 0,
        reserved2: 0,
        data_fork: ForkData::EMPTY,
        resource_fork: ForkData::EMPTY,
    };

    /// Parse a 248-byte file record body.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < FILE_RECORD_SIZE {
            return Err(Error::Truncated {
                what: "catalog file record",
                needed: FILE_RECORD_SIZE,
                available: bytes.len(),
            });
        }
        let be = Be::new(bytes);
        let mut user_info = [0u8; FINDER_USER_INFO_SIZE];
        let mut finder_info = [0u8; FINDER_OPAQUE_INFO_SIZE];
        user_info.copy_from_slice(bytes.get(48..64).ok_or(Error::Truncated {
            what: "file userInfo",
            needed: 64,
            available: bytes.len(),
        })?);
        finder_info.copy_from_slice(bytes.get(64..80).ok_or(Error::Truncated {
            what: "file finderInfo",
            needed: 80,
            available: bytes.len(),
        })?);

        // The two forks start on a double-long boundary. Counting the scalars
        // above gives 88, and the comment in Apple's struct records exactly
        // this alignment note.
        let mut cur = Cursor::at(bytes, 88, "catalog file forks");
        let data_fork = ForkData::read(&mut cur)?;
        let resource_fork = ForkData::read(&mut cur)?;

        Ok(FileRecord {
            record_type: be.i16(0)?,
            flags: be.u16(2)?,
            reserved1: be.u32(4)?,
            file_id: Cnid(be.u32(8)?),
            create_date: be.u32(12)?,
            content_mod_date: be.u32(16)?,
            attribute_mod_date: be.u32(20)?,
            access_date: be.u32(24)?,
            backup_date: be.u32(28)?,
            bsd_info: BsdInfo::parse(bytes, 32)?,
            user_info,
            finder_info,
            text_encoding: be.u32(80)?,
            reserved2: be.u32(84)?,
            data_fork,
            resource_fork,
        })
    }

    /// Whether this file has extended attributes.
    /// Encode into `out`, which must be at least [`FILE_RECORD_SIZE`] bytes.
    ///
    /// The inverse of [`FileRecord::parse`], and byte-for-byte the same layout,
    /// so a record read from an image and written straight back is identical to
    /// what was read. The test that asserts that is the one worth having here:
    /// a writer that got a single field's width wrong would produce a record
    /// that still parses, just wrongly -- which is how `fileMode` was once
    /// written as a u32 and overran into the next record.
    pub fn write_to(&self, out: &mut [u8]) -> Result<()> {
        let available = out.len();
        let dst = out.get_mut(..FILE_RECORD_SIZE).ok_or(Error::Truncated {
            what: "file record write",
            needed: FILE_RECORD_SIZE,
            available,
        })?;
        // Zeroed first so that every reserved byte is written as zero rather
        // than left holding whatever the image had there.
        dst.fill(0);
        dst[0..2].copy_from_slice(&self.record_type.to_be_bytes());
        dst[2..4].copy_from_slice(&self.flags.to_be_bytes());
        dst[8..12].copy_from_slice(&self.file_id.0.to_be_bytes());
        dst[12..16].copy_from_slice(&self.create_date.to_be_bytes());
        dst[16..20].copy_from_slice(&self.content_mod_date.to_be_bytes());
        dst[20..24].copy_from_slice(&self.attribute_mod_date.to_be_bytes());
        dst[24..28].copy_from_slice(&self.access_date.to_be_bytes());
        dst[28..32].copy_from_slice(&self.backup_date.to_be_bytes());
        self.bsd_info.write_to(&mut dst[32..48])?;
        dst[48..64].copy_from_slice(&self.user_info);
        dst[64..80].copy_from_slice(&self.finder_info);
        dst[80..84].copy_from_slice(&self.text_encoding.to_be_bytes());
        // dst[84..88] is reserved2, left zero by the fill above.
        self.data_fork.write_to(&mut dst[88..168])?;
        self.resource_fork
            .write_to(&mut dst[168..FILE_RECORD_SIZE])?;
        Ok(())
    }

    /// Encode to a [`FILE_RECORD_SIZE`]-byte array.
    pub fn to_bytes(&self) -> [u8; FILE_RECORD_SIZE] {
        let mut out = [0u8; FILE_RECORD_SIZE];
        // Writing into a correctly sized array cannot fail.
        let _ = self.write_to(&mut out);
        out
    }

    pub fn has_attributes(&self) -> bool {
        self.flags & K_HFS_HAS_ATTRIBUTES_MASK != 0
    }

    /// Whether this file is a *hard link* rather than an indirect node.
    ///
    /// `kHFSHasLinkChainMask` in the record flags: set on hard links, clear on
    /// the indirect node that owns the data and on ordinary files.
    pub fn has_link_chain(&self) -> bool {
        self.flags & K_HFS_HAS_LINK_CHAIN_MASK != 0
    }
    /// `bsdInfo.special` is a union that means one thing on an indirect node and
    /// another on a link, and the discriminator is **not** the value. It is
    /// `kHFSHasLinkChainMask` in the record's flags:
    ///
    /// - flag set: this record is a hard link, and `special` is `hl_linkReference`,
    ///   the CNID of the indirect node that owns the real data.
    /// - flag clear: this record is the indirect node itself (or an ordinary
    ///   file), and `special` is `hl_linkCount`.
    ///
    /// Mining reference: Apple `core/hfs_format.h` documents the union members
    /// and aliases them (`hl_linkReference`, `hl_linkCount`), and
    /// `core/hfs_catalog.c` states the rule in prose:
    ///
    /// ```c
    /// Set kHFSHasLinkChainBit for hard links, and reset it for all other
    /// items. Also set linkCount to 1 for regular files.
    /// ```
    ///
    /// The same code then *repairs* volumes where the bit is wrong, because some
    /// regular files carry the bit with a link count above one (rdar://8505977).
    pub fn is_hard_link(&self) -> bool {
        self.flags & K_HFS_HAS_LINK_CHAIN_MASK != 0
    }

    /// The link count, meaningful when this record is not a hard link.
    pub fn link_count(&self) -> u32 {
        self.bsd_info.special
    }

    /// For a hard link, the CNID of the indirect node holding the real data.
    pub fn link_reference(&self) -> Option<Cnid> {
        if self.is_hard_link() {
            Some(Cnid(self.bsd_info.special))
        } else {
            None
        }
    }

    /// For an indirect node, the CNID of its first hard link.
    ///
    /// Mining reference: `core/hfs_format.h` aliases the file record's
    /// `reserved1` to `hl_firstLinkID`, "valid only if HasLinkChain flag is set".
    pub fn first_link_id(&self) -> Option<Cnid> {
        if self.has_link_chain() && self.reserved1 != 0 {
            Some(Cnid(self.reserved1))
        } else {
            None
        }
    }

    /// Whether this file's timestamps are expanded (Unix) times.
    ///
    /// Per-item, not per-volume. See [`K_HFS_CAT_EXPANDED_TIMES_MASK`].
    pub fn has_expanded_times(&self) -> bool {
        self.flags & K_HFS_CAT_EXPANDED_TIMES_MASK != 0
    }

    /// The `fdType` four-character code, as stored big-endian.
    pub fn fd_type(&self) -> u32 {
        u32::from_be_bytes([
            self.user_info[0],
            self.user_info[1],
            self.user_info[2],
            self.user_info[3],
        ])
    }

    /// The `fdCreator` four-character code, as stored big-endian.
    pub fn fd_creator(&self) -> u32 {
        u32::from_be_bytes([
            self.user_info[4],
            self.user_info[5],
            self.user_info[6],
            self.user_info[7],
        ])
    }

    /// The `date_added` field, when the object carries one.
    ///
    /// Present only when [`K_HFS_HAS_DATE_ADDED_MASK`] is set; otherwise the
    /// first four bytes of the opaque Finder info are something else and must not
    /// be interpreted as a timestamp.
    ///
    /// Mining reference: `struct FndrExtendedFileInfo` / `FndrExtendedDirInfo`
    /// place `date_added` first, and `core/hfs_catalog.c` reads it only when the
    /// flag is set.
    pub fn date_added(&self) -> Option<u32> {
        if self.flags & K_HFS_HAS_DATE_ADDED_MASK == 0 {
            return None;
        }
        Some(u32::from_be_bytes([
            self.finder_info[0],
            self.finder_info[1],
            self.finder_info[2],
            self.finder_info[3],
        ]))
    }

    /// Whether this file's data fork holds a symbolic link target.
    pub fn is_symlink(&self) -> bool {
        self.bsd_info.is_symlink()
    }

    /// Decode a timestamp with this file's own epoch convention.
    ///
    /// The per-item `kHFSCatExpandedTimesMask` overrides the volume-wide flag, so
    /// the flag must be passed in from the volume and combined here.
    pub fn timestamp(&self, raw: u32, volume_expanded: bool) -> HfsTimestamp {
        HfsTimestamp::new(raw, volume_expanded || self.has_expanded_times())
    }
}

/// A thread record: the object's CNID and full name, with no fork data.
///
/// Mining reference: Apple `struct HFSPlusCatalogThread`:
///
/// ```c
/// struct HFSPlusCatalogThread {
///     int16_t     recordType;
///     int16_t     reserved;
///     u_int32_t     parentID;
///     HFSUniStr255     nodeName;    /* name of this catalog node (variable length) */
/// };
/// ```
///
/// The name is variable length: the record is only as long as it needs to be, so
/// the parser must not require 520 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRecord {
    /// [`K_HFS_PLUS_FOLDER_THREAD_RECORD`] or [`K_HFS_PLUS_FILE_THREAD_RECORD`].
    pub record_type: i16,
    /// Reserved, initialised to zero.
    pub reserved: u16,
    /// The object's **parent** CNID, copied from the main record's key.
    ///
    /// This is the field Apple names `parentID`, and for a thread record it is
    /// literally that: the parent of the object being described. The object's
    /// own CNID is *not* here -- it is the thread record's **key** parentID.
    ///
    /// Mining reference: Apple `core/hfs_catalog.c` `buildthread` copies the
    /// main record's key straight into the thread record:
    ///
    /// ```c
    /// rec->parentID = key->parentID;
    /// bcopy(&key->nodeName, &rec->nodeName, ...);
    /// ```
    ///
    /// while `buildthreadkey` builds the thread's own key from the object's
    /// CNID. Verified against a real volume: for the root folder, whose CNID is 2
    /// and whose parent is 1, the thread key is `(parentID = 2, name = "")` and
    /// the thread record body is `(parentID = 1, name = "BasicVolume")`.
    pub parent_id: Cnid,
    /// The object's name as raw big-endian UTF-16 code units.
    pub node_name: Vec<u16>,
}

impl ThreadRecord {
    /// Parse a thread record body of variable length.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < THREAD_RECORD_NAME_OFFSET {
            return Err(Error::Truncated {
                what: "catalog thread record",
                needed: THREAD_RECORD_NAME_OFFSET,
                available: bytes.len(),
            });
        }
        let be = Be::new(bytes);
        let record_type = be.i16(0)?;
        let reserved = be.u16(2)?;
        let parent_id = Cnid(be.u32(4)?);

        let name_len = be.u16(THREAD_RECORD_NAME_LEN_OFFSET)? as usize;
        let available = bytes.len() - THREAD_RECORD_NAME_OFFSET;
        let name_bytes = name_len
            .checked_mul(2)
            .ok_or(Error::overflow("thread record name"))?;
        if name_bytes > available {
            return Err(Error::Truncated {
                what: "thread record name",
                needed: name_bytes,
                available,
            });
        }
        let mut node_name = Vec::with_capacity(name_len);
        for i in 0..name_len {
            node_name.push(be.u16(THREAD_RECORD_NAME_OFFSET + i * 2)?);
        }

        Ok(ThreadRecord {
            record_type,
            reserved,
            parent_id,
            node_name,
        })
    }

    /// The object's name as a `String`, for diagnostics only.
    pub fn name_string(&self) -> String {
        String::from_utf16_lossy(&self.node_name)
    }

    /// Whether this is a folder thread record.
    pub fn is_folder(&self) -> bool {
        self.record_type == K_HFS_PLUS_FOLDER_THREAD_RECORD
    }

    /// Whether this is a file thread record.
    pub fn is_file(&self) -> bool {
        self.record_type == K_HFS_PLUS_FILE_THREAD_RECORD
    }
}

/// Parse a catalog record body, given the record type the key implied.
///
/// `body` is the record bytes *after* the key, including any pad byte.
pub fn parse_record(body: &[u8]) -> Result<CatalogRecord> {
    if body.len() < 2 {
        return Err(Error::Truncated {
            what: "catalog record",
            needed: 2,
            available: body.len(),
        });
    }
    let record_type = Be::new(body).i16(0)?;
    match record_type {
        K_HFS_PLUS_FOLDER_RECORD => Ok(CatalogRecord::Folder(FolderRecord::parse(body)?)),
        K_HFS_PLUS_FILE_RECORD => Ok(CatalogRecord::File(Box::new(FileRecord::parse(body)?))),
        K_HFS_PLUS_FOLDER_THREAD_RECORD | K_HFS_PLUS_FILE_THREAD_RECORD => {
            Ok(CatalogRecord::Thread(ThreadRecord::parse(body)?))
        }
        other => Err(Error::invalid(
            "catalog record type",
            format!("unknown record type {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    /// A record read from a real image must re-encode to exactly those bytes.
    ///
    /// This is the assertion `FileRecord::write_to` exists to make possible, and
    /// it is the one that would have caught the `fileMode` width bug: packed as a
    /// u32, `write_to` still produced a record that *parsed* -- with the wrong
    /// mode, and four bytes stolen from the following record. Both symptoms are
    /// invisible to any test that round-trips through the parser.
    ///
    /// Built from literal bytes rather than a fixture so the test needs no image,
    /// but every field is non-zero on purpose: a zero field cannot tell a field
    /// that is written from one that is merely left zeroed.
    #[test]
    fn a_file_record_round_trips_through_the_same_bytes() {
        let mut bytes = [0u8; FILE_RECORD_SIZE];
        // recordType, flags and the three catalog flag bits mkfs sets on a file.
        bytes[0..2].copy_from_slice(&2i16.to_be_bytes());
        bytes[2..4].copy_from_slice(&0x0004u16.to_be_bytes());
        bytes[8..12].copy_from_slice(&42u32.to_be_bytes()); // fileID
        bytes[12..16].copy_from_slice(&1_000_000_001u32.to_be_bytes()); // createDate
        bytes[16..20].copy_from_slice(&1_000_000_002u32.to_be_bytes()); // contentModDate
        bytes[20..24].copy_from_slice(&1_000_000_003u32.to_be_bytes()); // attributeModDate
        bytes[24..28].copy_from_slice(&1_000_000_004u32.to_be_bytes()); // accessDate
        bytes[28..32].copy_from_slice(&1_000_000_005u32.to_be_bytes()); // backupDate
                                                                        // BSDInfo: owner, group, adminFlags, ownerFlags, fileMode, special.
        bytes[32..36].copy_from_slice(&501u32.to_be_bytes());
        bytes[36..40].copy_from_slice(&20u32.to_be_bytes());
        bytes[40] = 3;
        bytes[41] = 7;
        // 0o100644 -- a regular file. A u32 here would eat bytes 44..46.
        bytes[42..44].copy_from_slice(&0o100644u16.to_be_bytes());
        bytes[44..48].copy_from_slice(&9u32.to_be_bytes());
        bytes[48..64].copy_from_slice(&[0x11u8; 16]); // userInfo
        bytes[64..80].copy_from_slice(&[0x22u8; 16]); // finderInfo
        bytes[80..84].copy_from_slice(&4u32.to_be_bytes()); // textEncoding
                                                            // reserved2 at 84..88 stays zero, as it must be written.
                                                            // dataFork: logicalSize, clumpSize, totalBlocks, one extent.
        bytes[88..96].copy_from_slice(&1234u64.to_be_bytes());
        bytes[96..100].copy_from_slice(&65_536u32.to_be_bytes());
        bytes[100..104].copy_from_slice(&1u32.to_be_bytes());
        bytes[104..108].copy_from_slice(&500u32.to_be_bytes()); // startBlock
        bytes[108..112].copy_from_slice(&1u32.to_be_bytes()); // blockCount
                                                              // resourceFork: empty, but its blockCount is still a field that must be
                                                              // written -- a writer that skipped it would truncate a resource fork.
        bytes[168..176].copy_from_slice(&0u64.to_be_bytes());

        let record = FileRecord::parse(&bytes).expect("parse");
        assert_eq!(record.file_id.0, 42);
        assert_eq!(record.bsd_info.file_mode, 0o100644);
        assert_eq!(record.bsd_info.special, 9);
        assert_eq!(record.data_fork.logical_size, 1234);
        assert_eq!(
            record
                .data_fork
                .extents
                .iter()
                .next()
                .expect("one extent")
                .start_block,
            500
        );
        assert_eq!(
            record.to_bytes(),
            bytes,
            "a record written back must be byte-identical to what was read"
        );
    }

    #[test]
    fn bsd_info_round_trips() {
        let info = BsdInfo {
            owner_id: 0xDEAD_BEEFu32,
            group_id: 0x0BADF00Du32,
            admin_flags: 0xAB,
            owner_flags: 0xCD,
            file_mode: 0o040755,
            special: 0x0102_0304,
        };
        let mut bytes = [0u8; BSD_INFO_SIZE];
        info.write_to(&mut bytes).expect("write");
        assert_eq!(
            BsdInfo::parse(&bytes, 0).expect("parse"),
            info,
            "BSDInfo must round-trip, because a mutation changes fileMode"
        );
    }

    /// A short destination is an error, not a partial write.
    ///
    /// The writer checks the length once, up front, so a caller cannot get half a
    /// record -- which would be worse than none, because the node would then hold a
    /// record that parses as something else.
    #[test]
    fn writing_into_a_short_buffer_is_refused() {
        let record = FileRecord::EMPTY;
        assert!(record.write_to(&mut [0u8; FILE_RECORD_SIZE - 1]).is_err());
        assert!(record.write_to(&mut [0u8; 0]).is_err());
    }

    use super::*;
    use crate::format::extents::ExtentDescriptor;

    /// Build a 248-byte file record body.
    fn file_body() -> Vec<u8> {
        let mut b = vec![0u8; FILE_RECORD_SIZE];
        b[0..2].copy_from_slice(&K_HFS_PLUS_FILE_RECORD.to_be_bytes());
        b[2..4].copy_from_slice(&0u16.to_be_bytes());
        b[8..12].copy_from_slice(&16u32.to_be_bytes()); // fileID
        b[12..16].copy_from_slice(&1_000u32.to_be_bytes()); // createDate
        b[16..20].copy_from_slice(&2_000u32.to_be_bytes()); // contentModDate
        b[32..36].copy_from_slice(&501u32.to_be_bytes()); // ownerID
        b[36..40].copy_from_slice(&20u32.to_be_bytes()); // groupID
        b[40] = 0; // adminFlags
        b[41] = 0; // ownerFlags
        b[42..44].copy_from_slice(&(S_IFREG | 0o644).to_be_bytes());
        b[44..48].copy_from_slice(&1u32.to_be_bytes()); // linkCount
        b[48..52].copy_from_slice(b"TEXT"); // fdType
        b[52..56].copy_from_slice(b"Mkmt"); // fdCreator
        b[80..84].copy_from_slice(&0u32.to_be_bytes());
        b[84..88].copy_from_slice(&0u32.to_be_bytes());
        // data fork at 88
        b[88..96].copy_from_slice(&4096u64.to_be_bytes()); // logicalSize
        b[96..100].copy_from_slice(&4096u32.to_be_bytes()); // clumpSize
        b[100..104].copy_from_slice(&1u32.to_be_bytes()); // totalBlocks
        b[104..108].copy_from_slice(&100u32.to_be_bytes()); // extents[0].startBlock
        b[108..112].copy_from_slice(&1u32.to_be_bytes()); // extents[0].blockCount
        b
    }

    /// Build an 88-byte folder record body.
    fn folder_body() -> Vec<u8> {
        let mut b = vec![0u8; FOLDER_RECORD_SIZE];
        b[0..2].copy_from_slice(&K_HFS_PLUS_FOLDER_RECORD.to_be_bytes());
        b[4..8].copy_from_slice(&5u32.to_be_bytes()); // valence
        b[8..12].copy_from_slice(&2u32.to_be_bytes()); // folderID
        b[42..44].copy_from_slice(&(S_IFDIR | 0o755).to_be_bytes());
        b[80..84].copy_from_slice(&0u32.to_be_bytes());
        b
    }

    fn thread_body(record_type: i16, parent: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut b = Vec::new();
        b.extend_from_slice(&record_type.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&parent.to_be_bytes());
        b.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for u in units {
            b.extend_from_slice(&u.to_be_bytes());
        }
        b
    }

    #[test]
    fn record_sizes_match_apple() {
        // The thread record's name length sits at offset 8 and its first code
        // unit at 10, which is easy to get wrong by reading parentID as 2 bytes.
        assert_eq!(THREAD_RECORD_FIXED_SIZE, 8);
        assert_eq!(THREAD_RECORD_NAME_LEN_OFFSET, 8);
        assert_eq!(THREAD_RECORD_NAME_OFFSET, 10);
        assert_eq!(FOLDER_RECORD_SIZE, 88);
        assert_eq!(FILE_RECORD_SIZE, 248);
        assert_eq!(BSD_INFO_SIZE, 16);
        assert_eq!(FINDER_USER_INFO_SIZE, 16);
        assert_eq!(FINDER_OPAQUE_INFO_SIZE, 16);
    }

    #[test]
    fn file_record_round_trips() {
        let body = file_body();
        let f = FileRecord::parse(&body).unwrap();
        assert_eq!(f.record_type, K_HFS_PLUS_FILE_RECORD);
        assert_eq!(f.file_id, Cnid(16));
        assert_eq!(f.create_date, 1_000);
        assert_eq!(f.bsd_info.owner_id, 501);
        assert_eq!(f.bsd_info.group_id, 20);
        assert_eq!(f.bsd_info.file_mode, S_IFREG | 0o644);
        assert_eq!(f.fd_type(), u32::from_be_bytes(*b"TEXT"));
        assert_eq!(f.fd_creator(), u32::from_be_bytes(*b"Mkmt"));
        assert_eq!(f.data_fork.logical_size, 4096);
        assert_eq!(f.data_fork.total_blocks, 1);
        assert_eq!(
            f.data_fork.extents.raw[0],
            ExtentDescriptor {
                start_block: 100,
                block_count: 1
            }
        );
        assert_eq!(f.resource_fork.logical_size, 0);
        assert!(f.bsd_info.is_regular());
        assert!(!f.bsd_info.is_dir());
    }

    #[test]
    fn folder_record_round_trips() {
        let b = folder_body();
        let f = FolderRecord::parse(&b).unwrap();
        assert_eq!(f.record_type, K_HFS_PLUS_FOLDER_RECORD);
        assert_eq!(f.folder_id, Cnid(2));
        assert_eq!(f.valence, 5);
        assert!(f.bsd_info.is_dir());
    }

    #[test]
    fn thread_record_round_trips_and_is_variable_length() {
        let b = thread_body(K_HFS_PLUS_FOLDER_THREAD_RECORD, 2, "TestVol");
        let t = ThreadRecord::parse(&b).unwrap();
        assert_eq!(t.record_type, K_HFS_PLUS_FOLDER_THREAD_RECORD);
        assert_eq!(t.parent_id, Cnid(2));
        assert_eq!(t.name_string(), "TestVol");
        assert!(t.is_folder());
        assert!(!t.is_file());
        // 8 fixed bytes plus a u16 length plus the name; not the full 520.
        assert_eq!(b.len(), THREAD_RECORD_NAME_OFFSET + 14);

        let empty = thread_body(K_HFS_PLUS_FILE_THREAD_RECORD, 1, "");
        let t = ThreadRecord::parse(&empty).unwrap();
        assert_eq!(t.node_name.len(), 0);
        assert_eq!(t.parent_id, Cnid(1));
    }

    #[test]
    fn parse_record_dispatches_on_type() {
        assert!(matches!(
            parse_record(&file_body()).unwrap(),
            CatalogRecord::File(_)
        ));
        assert!(matches!(
            parse_record(&folder_body()).unwrap(),
            CatalogRecord::Folder(_)
        ));
        assert!(matches!(
            parse_record(&thread_body(K_HFS_PLUS_FILE_THREAD_RECORD, 2, "x")).unwrap(),
            CatalogRecord::Thread(_)
        ));
    }

    #[test]
    fn unknown_record_types_are_rejected() {
        let mut b = file_body();
        b[0..2].copy_from_slice(&99i16.to_be_bytes());
        assert!(matches!(
            parse_record(&b),
            Err(Error::InvalidField {
                field: "catalog record type",
                ..
            })
        ));
    }

    #[test]
    fn short_records_are_truncated_not_panicked_on() {
        for len in [0usize, 1, 2, 4, 87] {
            assert!(
                FolderRecord::parse(&vec![0u8; len]).is_err(),
                "folder {len}"
            );
        }
        for len in [0usize, 1, 2, 7, 87, 247] {
            assert!(FileRecord::parse(&vec![0u8; len]).is_err(), "file {len}");
        }
        for len in [0usize, 1, 8, 9] {
            assert!(
                ThreadRecord::parse(&vec![0u8; len]).is_err(),
                "thread {len}"
            );
        }
        assert!(ThreadRecord::parse(&[0u8; THREAD_RECORD_NAME_OFFSET]).is_ok());
    }

    #[test]
    fn a_thread_name_longer_than_the_record_is_truncated() {
        let mut b = thread_body(K_HFS_PLUS_FILE_THREAD_RECORD, 2, "abc");
        // Claim more units than the record holds.
        b[THREAD_RECORD_NAME_LEN_OFFSET..THREAD_RECORD_NAME_OFFSET]
            .copy_from_slice(&100u16.to_be_bytes());
        assert!(matches!(
            ThreadRecord::parse(&b),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn per_item_expanded_times_are_distinct_from_the_volume_flag() {
        let mut b = file_body();
        let f = FileRecord::parse(&b).unwrap();
        assert!(!f.has_expanded_times());
        assert_eq!(f.timestamp(f.create_date, true).to_unix(), 1_000);
        assert_eq!(f.timestamp(f.create_date, false).to_unix(), 0); // pre-1904 clamps

        b[2..4].copy_from_slice(&K_HFS_CAT_EXPANDED_TIMES_MASK.to_be_bytes());
        let f = FileRecord::parse(&b).unwrap();
        assert!(f.has_expanded_times());
        // The item bit overrides a classic-mode volume.
        assert_eq!(f.timestamp(f.create_date, false).to_unix(), 1_000);
    }

    #[test]
    fn date_added_is_only_read_when_the_flag_is_set() {
        let mut b = file_body();
        // Put a plausible timestamp in the opaque Finder info.
        b[64..68].copy_from_slice(&0xC0FFEE00u32.to_be_bytes());
        let f = FileRecord::parse(&b).unwrap();
        assert_eq!(f.date_added(), None, "must not guess without the flag");

        b[2..4].copy_from_slice(&K_HFS_HAS_DATE_ADDED_MASK.to_be_bytes());
        let f = FileRecord::parse(&b).unwrap();
        assert_eq!(f.date_added(), Some(0xC0FFEE00));
    }

    #[test]
    fn hard_link_status_comes_from_the_flags_not_from_special() {
        // A plain file: the bit is clear, so `special` is the link count.
        let b = file_body();
        let f = FileRecord::parse(&b).unwrap();
        assert!(!f.is_hard_link());
        assert_eq!(f.link_count(), 1);
        assert_eq!(f.link_reference(), None);

        // Setting the bit makes it a hard link, and `special` becomes a
        // reference to the indirect node instead of a count. The same `special`
        // value now means something else entirely.
        let mut b = file_body();
        b[2..4].copy_from_slice(&K_HFS_HAS_LINK_CHAIN_MASK.to_be_bytes());
        b[44..48].copy_from_slice(&42u32.to_be_bytes());
        b[4..8].copy_from_slice(&7u32.to_be_bytes()); // reserved1 = hl_firstLinkID
        let f = FileRecord::parse(&b).unwrap();
        assert!(f.is_hard_link());
        assert!(f.has_link_chain());
        assert_eq!(f.link_reference(), Some(Cnid(42)));
        assert_eq!(f.first_link_id(), Some(Cnid(7)));

        // reserved1 is only a first-link pointer when the bit is set.
        let mut b = file_body();
        b[4..8].copy_from_slice(&7u32.to_be_bytes());
        let f = FileRecord::parse(&b).unwrap();
        assert_eq!(f.first_link_id(), None);
    }

    #[test]
    fn file_type_helpers() {
        let mk = |mode: u16| BsdInfo {
            owner_id: 0,
            group_id: 0,
            admin_flags: 0,
            owner_flags: 0,
            file_mode: mode,
            special: 0,
        };
        assert!(mk(S_IFDIR | 0o755).is_dir());
        assert!(mk(S_IFREG | 0o644).is_regular());
        assert!(mk(S_IFLNK | 0o777).is_symlink());
        assert_eq!(mk(S_IFREG | 0o644).permissions(), 0o644);
        assert_eq!(mk(S_IFLNK | 0o777).permissions(), 0o777);
        assert!(!mk(S_IFCHR | 0o666).is_regular());
    }
}
