// SPDX-License-Identifier: APSL-1.2

//! Block device abstraction sitting below the HFS format layer.
//!
//! The HFS layers above this one must never learn whether they are talking to
//! an image file, a disk partition or an in-memory buffer. Apple keeps this
//! separation in the kernel via `buf_meta_t` / `vnode` I/O callbacks and, for
//! the raw device case, via `daddr64_t` block addressing plus an explicit
//! `hfsPlusIOPosOffset` applied to every physical read.
//!
//! Mining reference: Apple `core/hfs_vfsutils.c`
//! (`hfs_MountHFSPlusVolume`) records `vcb->hfsPlusIOPosOffset = embeddedOffset`
//! and `core/FileExtentMapping.c` (`MapFileBlockC`) turns a fork-relative block
//! into a device byte offset. That offset is not a property of the *volume*, it
//! is a property of *where this volume happens to live on this device*, so
//! [`ViewDevice`] models it explicitly instead of scattering arithmetic through
//! the format code.
//!
//! Read and write are deliberately split into two traits. The read-only
//! milestone needs nothing but [`BlockDevice`], and making write support
//! separate means a read-only mount can be enforced by the type system instead
//! of by remembering to check a flag at each call site.

pub mod file;
pub mod memory;
pub mod view;

pub use file::FileDevice;
pub use memory::MemoryDevice;
pub use view::ViewDevice;

use crate::error::Result;

/// A read-only, randomly addressable source of bytes.
///
/// Offsets are absolute byte offsets from the start of the device. Callers that
/// think in allocation blocks are responsible for multiplying by the
/// allocation block size, because only the volume knows that size.
///
/// Implementations must return [`crate::error::Error::Truncated`] when a read
/// would run past the end, rather than zero-filling, so that a truncated image
/// is detectable instead of silently containing zeros.
pub trait BlockDevice {
    /// Read exactly `buf.len()` bytes starting at `offset`.
    ///
    /// A short read at the end of the device is an error, not a partial
    /// success: HFS structures are fixed-layout and a half-decoded volume
    /// header is never useful.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Total length of the device in bytes.
    fn len(&self) -> Result<u64>;

    /// Whether the device has zero length.
    fn is_empty(&self) -> bool {
        matches!(self.len(), Ok(0))
    }

    /// Read a fixed-size array at `offset`.
    fn read_array<const N: usize>(&self, offset: u64) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        self.read_at(offset, &mut out)?;
        Ok(out)
    }

    /// Read `len` bytes at `offset` into a fresh `Vec`.
    fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        self.read_at(offset, &mut out)?;
        Ok(out)
    }
}

/// A device that additionally supports writing.
///
/// Kept separate from [`BlockDevice`] so that a read-only mount cannot write,
/// and so the initial read-only implementation is not forced to invent write
/// semantics it does not need.
pub trait BlockDeviceMut: BlockDevice {
    /// Write exactly `buf.len()` bytes at `offset`.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;

    /// Flush any buffered writes to stable storage.
    ///
    /// Must be called before an image is handed to an independent checker:
    /// `fsck.hfsplus` reads the file directly and will not see data that is
    /// still sitting in a `BufWriter`.
    fn sync(&mut self) -> Result<()>;
}

/// Convenience: the block size used by the classic HFS / HFS+ sector layer.
///
/// HFS+ allocation blocks are conventionally 4096 bytes, but the volume
/// header is authoritative and this value is only the granularity at which the
/// first two 512-byte sectors of a volume are addressed.
pub const SECTOR_SIZE: u64 = 512;

/// Byte offset of the primary volume header within an HFS+ volume.
///
/// Apple reserves the first two 512-byte sectors: sector 0 holds the driver
/// descriptor (on a partitioned disk, the partition map) and sector 1 is the
/// `HFSMasterDirectoryBlock`, whose `drEmbedSigWord` at offset 0x7C announces
/// the signature of the embedded HFS+ volume header that follows.
///
/// Mining reference: Apple `core/hfs_format.h` (`struct HFSMasterDirectoryBlock`,
/// `drEmbedSigWord`); `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) treats
/// the embedded volume as beginning at `embeddedOffset`, which for a standalone
/// image is one HFS block.
pub const VOLUME_HEADER_OFFSET: u64 = SECTOR_SIZE * 2;

/// Byte offset of the alternate (backup) volume header, relative to the end of
/// the volume's allocation area.
///
/// HFS+ keeps a second copy of the volume header in the second-to-last
/// allocation block so that a volume remains mountable after the primary copy
/// is damaged.
///
/// Mining reference: Apple `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`)
/// computes `spare_sectors` from `hfs_logical_block_count` and
/// `totalBlocks * blockSize`, then validates the alternate header there.
pub const ALTERNATE_HEADER_BLOCKS_FROM_END: u64 = 2;
