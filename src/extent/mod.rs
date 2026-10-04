//! Extent mapping: translating fork-relative blocks to physical blocks.
//!
//! Mining reference: Apple `core/FileExtentMapping.c` (`MapFileBlockC`)
//! performs this translation for the kernel's I/O path, driven by the iterator
//! in `core/hfs_extents.c`. This module is the userspace equivalent: same
//! algorithm, no locking, no `vnode`, no buffer cache.
//!
//! See [`mapper`] for the mapping chain and for the subtlety that overflow is
//! keyed on *allocated* blocks rather than logical size.

pub mod mapper;

pub use mapper::{ExtentMapper, NoOverflow, OverflowResolver};
