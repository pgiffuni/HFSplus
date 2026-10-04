//! Catalog node IDs, the identity type of the HFS+ filesystem.
//!
//! # CNIDs are the real identity
//!
//! HFS+ identifies every file and folder by a 32-bit catalog node ID. That is
//! the filesystem's inode number, allocated monotonically from
//! `nextCatalogID` in the volume header. It is *not* a FUSE inode number and must
//! not be replaced by one.
//!
//! A CNID is stable across renames, which is what makes it the right key for
//! `readdirplus` and for caching. It is also the key of the extents overflow
//! B-tree and of the attributes B-tree, so a fork discovered through a catalog
//! record can be found in both without a second lookup by path.
//!
//! Mining reference: Apple `core/hfs_catalog.c` uses CNIDs throughout
//! (`cat_idlookup`, `cat_lookuplink`, `cat_lookupbykey`) and `core/hfs_cnode.c`
//! stores `c_cnid` as the node's identity. `core/hfs_extents.c` keys overflow
//! records on `kHFSPlusExtentKey.fileID`, a CNID.
//!
//! # Reserved CNIDs
//!
//! Mining reference: Apple `core/hfs_format.h`:
//!
//! ```c
//! kHFSRootParentID       = 1,     /* Parent ID of the root folder */
//! kHFSRootFolderID       = 2,     /* Folder ID of the root folder */
//! kHFSExtentsFileID      = 3,     /* reserved CNID of extents overflow pseudo-file */
//! kHFSCatalogFileID      = 4,     /* reserved CNID of catalog pseudo-file */
//! kHFSBadDirFileID       = 5,     /* reserved CNID of corrupt directory pseudo-file */
//! kHFSFirstUserCatalogNodeID = 16, /* first CNID available to the user */
//! ```

/// A catalog node ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cnid(pub u32);

impl std::fmt::Display for Cnid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u32> for Cnid {
    fn from(v: u32) -> Self {
        Cnid(v)
    }
}

impl From<Cnid> for u32 {
    fn from(c: Cnid) -> u32 {
        c.0
    }
}

/// `kHFSRootParentID`: the parent ID of the root folder, always 1.
///
/// Every thread record uses this as its key's `parentID`, which is how thread
/// records for different files share one contiguous key range.
pub const ROOT_PARENT_ID: Cnid = Cnid(1);

/// `kHFSRootFolderID`: the folder ID of the root folder, always 2.
///
/// The volume name is this folder's catalog name.
pub const ROOT_FOLDER_ID: Cnid = Cnid(2);

/// `kHFSExtentsFileID`: the extents overflow B-tree pseudo-file.
pub const EXTENTS_FILE_ID: Cnid = Cnid(3);

/// `kHFSCatalogFileID`: the catalog B-tree pseudo-file.
pub const CATALOG_FILE_ID: Cnid = Cnid(4);

/// `kHFSBadDirFileID`: the corrupt-directory pseudo-file.
pub const BAD_DIR_FILE_ID: Cnid = Cnid(5);

/// `kHFSFirstUserCatalogNodeID`: the first CNID available to the user.
pub const FIRST_USER_CATALOG_NODE_ID: Cnid = Cnid(16);

impl Cnid {
    /// Whether this is one of the five reserved pseudo-file IDs.
    ///
    /// A directory listing must never contain these: they are not directory
    /// entries. `valence` in the root folder record counts them, which is a
    /// known source of off-by-one confusion.
    pub const fn is_reserved(self) -> bool {
        self.0 >= ROOT_PARENT_ID.0 && self.0 <= BAD_DIR_FILE_ID.0
    }

    /// Whether this CNID was allocated for a real user object.
    pub const fn is_user(self) -> bool {
        self.0 >= FIRST_USER_CATALOG_NODE_ID.0
    }

    /// A short name for diagnostics and for `statfs`-style reporting.
    pub const fn describe(self) -> &'static str {
        match self.0 {
            1 => "root parent",
            2 => "root folder",
            3 => "extents overflow",
            4 => "catalog",
            5 => "bad directory",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_ids_match_apple() {
        assert_eq!(ROOT_PARENT_ID.0, 1);
        assert_eq!(ROOT_FOLDER_ID.0, 2);
        assert_eq!(EXTENTS_FILE_ID.0, 3);
        assert_eq!(CATALOG_FILE_ID.0, 4);
        assert_eq!(BAD_DIR_FILE_ID.0, 5);
        assert_eq!(FIRST_USER_CATALOG_NODE_ID.0, 16);
    }

    #[test]
    fn reserved_classification() {
        assert!(ROOT_PARENT_ID.is_reserved());
        assert!(ROOT_FOLDER_ID.is_reserved());
        assert!(EXTENTS_FILE_ID.is_reserved());
        assert!(CATALOG_FILE_ID.is_reserved());
        assert!(BAD_DIR_FILE_ID.is_reserved());
        assert!(!Cnid(6).is_reserved());
        assert!(!Cnid(0).is_reserved());

        assert!(Cnid(16).is_user());
        assert!(!ROOT_FOLDER_ID.is_user());
        // 6..15 are neither reserved nor user; they are legal but unused.
        assert!(!Cnid(6).is_user());
    }

    #[test]
    fn orders_numerically() {
        assert!(Cnid(2) < Cnid(16));
        assert_eq!(Cnid(7), Cnid::from(7u32));
        assert_eq!(u32::from(Cnid(7)), 7);
    }

    #[test]
    fn describes_the_reserved_ones_only() {
        assert_eq!(ROOT_FOLDER_ID.describe(), "root folder");
        assert_eq!(Cnid(99).describe(), "");
    }
}
