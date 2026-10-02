//! The HFS+ catalog: the filesystem's namespace.
//!
//! # Where names really live
//!
//! The volume header has no name field. A volume's name is the name of its root
//! folder, which is a `kHFSPlusFolderRecord` with CNID `kHFSRootFolderID` in this
//! B-tree. See [`volume_header::VolumeHeader::volume_name_is_in_the_catalog`].
//!
//! Mining reference: Apple `core/hfs_catalog.c` implements this namespace and
//! `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) opens it.
//!
//! # Identity is CNID, not path
//!
//! Every object has a 32-bit catalog node ID. A path is a sequence of names that
//! must be resolved one component at a time, because HFS+ directories are stored
//! as a flat sorted key range: a directory's children are the consecutive keys
//! with that directory's CNID as `parentID`.
//!
//! Two consequences shape this module. First, a lookup is a B-tree descent, not a
//! walk. Second, and more usefully, a *full* enumeration of the volume is one
//! scan of the thread-record range rather than a recursive walk, because every
//! object has a thread record and thread records are keyed by an empty name under
//! the single parent CNID 1.

pub mod cnid;
pub mod key;
pub mod lookup;
pub mod record;

pub use cnid::{
    BAD_DIR_FILE_ID, CATALOG_FILE_ID, Cnid, EXTENTS_FILE_ID, FIRST_USER_CATALOG_NODE_ID,
    ROOT_FOLDER_ID, ROOT_PARENT_ID,
};
pub use key::{CatalogKey, NameComparison};
pub use lookup::{Catalog, CatalogEntry};
pub use record::{CatalogRecord, FileRecord, FolderRecord, ThreadRecord};
