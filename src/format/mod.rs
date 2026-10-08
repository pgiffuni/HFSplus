// SPDX-License-Identifier: BSD-2-Clause

//! On-disk format structures for the HFS+ family.
//!
//! This layer is deliberately thin and deliberately paranoid. It decodes
//! fixed-layout byte structures into typed Rust values and validates nothing
//! beyond what the structure itself can be checked against. Higher layers
//! ([`crate::btree`], [`crate::catalog`]) interpret the *meaning* of those
//! values.
//!
//! # Mining reference
//!
//! Every structure here is transcribed from Apple's `core/hfs_format.h`, and
//! the decode order matches `core/hfs_endian.c`'s swap routines for the same
//! structures, so a field-by-field diff against Apple is possible.
//!
//! # Safety posture
//!
//! Disk images are untrusted input. No structure in this module is decoded via
//! pointer casts; every field goes through [`crate::endian`] accessors with an
//! explicit bounds check, and every integer result that feeds an allocation or
//! an offset is range-checked before use. Malformed input yields an
//! [`crate::error::Error`], never a panic.

pub mod extents;
pub mod fork;
pub mod volume_header;
pub mod writer;

pub use extents::{ExtentDescriptor, ExtentRecord, INLINE_EXTENT_COUNT};
pub use fork::{ForkData, ForkType};
pub use volume_header::{FileSystemKind, VolumeAttributes, VolumeHeader};
pub use writer::format_volume;
