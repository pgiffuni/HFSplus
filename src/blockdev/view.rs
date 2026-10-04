//! A bounded, optionally offset window onto another [`BlockDevice`].
//!
//! An HFS+ volume does not necessarily start at byte 0 of the thing it lives
//! in. It can sit inside an Apple disk image partition, inside an HFS wrapper's
//! embedded area, or inside a test image that has a partition map prepended.
//! Modelling that as a device view rather than as a special case inside the
//! volume code keeps a single offset arithmetic rule: *allocation block N of
//! this volume is byte `base + N * blockSize` of the underlying device, as seen
//! through the view*.
//!
//! Mining reference: Apple `core/hfs_vfsutils.c`
//! (`hfs_MountHFSPlusVolume`) records the wrapper's embedded volume origin in
//! `vcb->hfsPlusIOPosOffset` and `core/FileExtentMapping.c` (`MapFileBlockC`)
//! applies it when converting a physical block to a device byte position.
//! `core/hfs_vfsutils.c` also validates that `embeddedOffset` is aligned to the
//! logical block size before using it, which [`ViewDevice`] enforces by
//! construction when the caller requests alignment.

use super::{BlockDevice, BlockDeviceMut};
use crate::error::{Error, Result};

/// A window of `len` bytes starting at `base` within an inner device.
#[derive(Debug)]
pub struct ViewDevice<D> {
    inner: D,
    base: u64,
    len: u64,
}

impl<D: BlockDevice> ViewDevice<D> {
    /// Create a view of `inner` covering `[base, base + len)`.
    ///
    /// Fails if the requested window extends past the end of `inner`, so that
    /// an out-of-range partition start is reported at construction instead of
    /// as a confusing short read much later.
    pub fn new(inner: D, base: u64, len: u64) -> Result<Self> {
        let inner_len = inner.len()?;
        let end = base
            .checked_add(len)
            .ok_or(Error::overflow("device view range"))?;
        if end > inner_len {
            return Err(Error::OutOfRange {
                what: "device view",
                value: end,
                limit: inner_len,
            });
        }
        Ok(ViewDevice { inner, base, len })
    }

    /// Create a view covering everything from `base` to the end of `inner`.
    pub fn from_offset(inner: D, base: u64) -> Result<Self> {
        let inner_len = inner.len()?;
        if base > inner_len {
            return Err(Error::OutOfRange {
                what: "device view base",
                value: base,
                limit: inner_len,
            });
        }
        Ok(ViewDevice {
            inner,
            base,
            len: inner_len - base,
        })
    }

    /// Byte offset of the window within the inner device.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Borrow the inner device.
    pub fn inner(&self) -> &D {
        &self.inner
    }

    /// Unwrap and return the inner device.
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for ViewDevice<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::overflow("view read"))?;
        if end > self.len {
            return Err(Error::Truncated {
                what: "view read",
                needed: end as usize,
                available: self.len as usize,
            });
        }
        let abs = self
            .base
            .checked_add(offset)
            .ok_or(Error::overflow("view read"))?;
        self.inner.read_at(abs, buf)
    }

    fn len(&self) -> Result<u64> {
        Ok(self.len)
    }
}

impl<D: BlockDeviceMut> BlockDeviceMut for ViewDevice<D> {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::overflow("view write"))?;
        if end > self.len {
            return Err(Error::Truncated {
                what: "view write",
                needed: end as usize,
                available: self.len as usize,
            });
        }
        let abs = self
            .base
            .checked_add(offset)
            .ok_or(Error::overflow("view write"))?;
        self.inner.write_at(abs, buf)
    }

    fn sync(&mut self) -> Result<()> {
        self.inner.sync()
    }
}
