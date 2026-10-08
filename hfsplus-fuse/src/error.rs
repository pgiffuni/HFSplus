// SPDX-License-Identifier: BSD-2-Clause

//! Errno conversion from `hfsplus::Error` to FUSE `Errno`.
//!
//! This module is the single translation point between HFS library errors and
//! the errno values reported back through FUSE. No other module should map
//! individual library errors to errno values.

use fuser::Errno;
use hfsplus::Error as HfsError;

/// Converts an [`hfsplus::Error`] into the corresponding FUSE [`Errno`].
///
/// This is the single translation point; no other module should map individual
/// library errors to errno values.
pub fn errno_of(error: &HfsError) -> Errno {
    match error {
        HfsError::NotFound { .. } => Errno::ENOENT,
        HfsError::NotFoundKey { .. } => Errno::ENOENT,
        HfsError::OutOfRange { .. } => Errno::EIO,
        HfsError::NoSpace { .. } => Errno::ENOSPC,
        HfsError::ReadOnly => Errno::EROFS,
        HfsError::Unsupported { .. } => Errno::EOPNOTSUPP,
        HfsError::BadSignature { .. } => Errno::EIO,
        HfsError::BadVersion { .. } => Errno::EIO,
        HfsError::Io { .. } => Errno::EIO,
        HfsError::InvalidField { .. } => Errno::EINVAL,
        HfsError::Truncated { .. } => Errno::EIO,
        HfsError::Overflow { .. } => Errno::EIO,
        HfsError::BadBlockNumber { .. } => Errno::EIO,
    }
}

/// Re-exported as the canonical way to convert a raw `i32` errno into FUSE's
/// [`Errno`], used by the few callbacks that produce errno values directly
/// (e.g. `EISDIR`, `ENOTDIR`).
pub fn from_raw(raw: i32) -> Errno {
    Errno::from_i32(raw)
}
