// SPDX-License-Identifier: BSD-2-Clause

//! FUSE adapter for the `hfsplus` library.
//!
//! This crate exposes an HFS+/HFSX filesystem image through FUSE. It performs
//! only POSIX-to-HFS+ translation: inode mapping, handle management, `FileAttr`
//! conversion, errno conversion, and timestamp conversion. All filesystem
//! algorithms remain in the core `hfsplus` library.
//!
//! # Architecture
//!
//! ```text
//!          FUSE kernel layer
//!                  |
//!                  v
//!     +-----------------------+
//!     |     hfsplus-fuse      |
//!     | inode/handle mapping  |
//!     | errno conversion      |
//!     | FileAttr conversion   |
//!     | FUSE callbacks        |
//!     +-----------+-----------+
//!                 |
//!                 v
//!     +-----------------------+
//!     |      hfsplus          |
//!     | Volume / WritableVol  |
//!     | Catalog / Extents     |
//!     | Journal / Compression |
//!     +-----------------------+
//!                 |
//!                 v
//!           HFS+/HFSX image
//! ```
//!
//! The [`HfsPlusFilesystem`] is intentionally thin. Each callback follows this
//! pattern:
//!
//! 1. validate/convert arguments;
//! 2. call the `hfsplus` library;
//! 3. convert the result;
//! 4. reply.
//!
//! Filesystem algorithms — B-tree traversal, extent allocation, catalog
//! manipulation, journal assembly, Unicode normalization, compression
//! decoding — must live in `hfsplus`, never here.

pub mod attr;
pub mod error;
pub mod filesystem;
pub mod handles;

pub use attr::{
    cnid_to_inode, dir_attrs_to_file_attr, file_attrs_to_file_attr, hfs_time_to_systemtime,
    times_to_fileattr,
};
pub use error::errno_of;
pub use filesystem::{HfsPlusFilesystem, VolumeHolder};
pub use handles::{HandleEntry, HandleTable, OpenDir, OpenFile};
