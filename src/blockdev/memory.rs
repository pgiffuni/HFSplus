// SPDX-License-Identifier: BSD-2-Clause

//! [`BlockDevice`] over an in-memory buffer.
//!
//! Useful for building malformed images in unit tests: a synthetic volume can
//! be assembled field by field and then handed to the parser, which is how the
//! truncated-metadata and bad-B-tree-node cases in the corpus are produced
//! without needing a byte-level patch script.
//!
//! Mining reference: Apple's own fuzzing entry points (see
//! `Fuzzing.xcconfig` and `hfs_util/hfsutil_fuzzmain.c` in the Apple HFS
//! source) drive the parser from raw byte buffers rather than from a device,
//! which is the same boundary this type provides.

use super::{BlockDevice, BlockDeviceMut};
use crate::error::{Error, Result};

/// A block device backed by a `Vec<u8>`.
#[derive(Debug)]
pub struct MemoryDevice {
    data: Vec<u8>,
    writable: bool,
}

impl MemoryDevice {
    /// Wrap an existing buffer read-only.
    pub fn new(data: Vec<u8>) -> Self {
        MemoryDevice {
            data,
            writable: false,
        }
    }

    /// Wrap an existing buffer read-write.
    pub fn new_writable(data: Vec<u8>) -> Self {
        MemoryDevice {
            data,
            writable: true,
        }
    }

    /// Create a zero-filled buffer of `len` bytes, read-write.
    pub fn zeroed(len: usize) -> Self {
        MemoryDevice {
            data: vec![0u8; len],
            writable: true,
        }
    }

    /// Borrow the underlying bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    /// Mutably borrow the underlying bytes.
    ///
    /// For test fixtures that need to plant a specific malformed structure.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Consume the device and return the buffer.
    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }
}

impl BlockDevice for MemoryDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let available = self.data.len();
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::overflow("memory read"))?;
        let end_usize = usize::try_from(end).map_err(|_| Error::OutOfRange {
            what: "memory read offset",
            value: end,
            limit: available as u64,
        })?;
        let src = self
            .data
            .get(offset as usize..end_usize)
            .ok_or(Error::Truncated {
                what: "memory read",
                needed: end as usize,
                available,
            })?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.data.len() as u64)
    }
}

impl BlockDeviceMut for MemoryDevice {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        let available = self.data.len();
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::overflow("memory write"))?;
        let end_usize = usize::try_from(end).map_err(|_| Error::OutOfRange {
            what: "memory write offset",
            value: end,
            limit: available as u64,
        })?;
        let dst = self
            .data
            .get_mut(offset as usize..end_usize)
            .ok_or(Error::Truncated {
                what: "memory write",
                needed: end as usize,
                available,
            })?;
        dst.copy_from_slice(buf);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}
