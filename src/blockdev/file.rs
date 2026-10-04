//! [`BlockDevice`] over a regular file on the host filesystem.
//!
//! This is the only backend needed for image-based testing, which is why the
//! test corpus is built from image files rather than loop devices: image files
//! are reproducible, cheap, diffable, and safe to hand to an independent
//! checker such as `fsck.hfsplus`.
//!
//! Mining reference: the offset-at-a-time interface here mirrors the raw device
//! path in Apple `core/FileExtentMapping.c` (`MapFileBlockC` /
//! `MapFileBlockC_noPerm`), which converts a physical block number to a byte
//! offset with a single shift and issues one `dev_read` for the resulting run.
//! Keeping the shift inside this backend means the format layer above deals
//! only in byte offsets.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{BlockDevice, BlockDeviceMut};
use crate::error::{Error, Result};

/// A block device backed by a file, opened read-only or read-write.
#[derive(Debug)]
pub struct FileDevice {
    file: File,
    path: PathBuf,
    writable: bool,
    /// Length captured at open time.
    ///
    /// Cached because `metadata()` on every `len()` call would dominate the
    /// cost of small random reads, and because holding a stable value keeps
    /// bounds checks meaningful even if the image is modified underneath us.
    len: u64,
}

impl FileDevice {
    /// Open an existing image read-only.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|e| Error::Io {
            message: format!("{}: {}", path.display(), e),
        })?;
        let len = file.metadata().map_err(|e| Error::io(&e))?.len();
        Ok(FileDevice {
            file,
            path: path.to_path_buf(),
            writable: false,
            len,
        })
    }

    /// Open an existing image read-write.
    ///
    /// Callers are responsible for having verified that the volume is not
    /// mounted elsewhere; on Linux, mount ownership is enforced by the FUSE
    /// layer rather than here.
    pub fn open_writable<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| Error::Io {
                message: format!("{}: {}", path.display(), e),
            })?;
        let len = file.metadata().map_err(|e| Error::io(&e))?.len();
        Ok(FileDevice {
            file,
            path: path.to_path_buf(),
            writable: true,
            len,
        })
    }

    /// Create `path` with `len` bytes of length and open it read-write.
    ///
    /// The file is created sparse: a freshly created HFS+ image is mostly free
    /// space, so materialising zeros up front would waste both time and disk.
    /// HFS free-space accounting starts from the volume header, not from what
    /// the backing store actually allocated.
    pub fn create<P: AsRef<Path>>(path: P, len: u64) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| Error::Io {
                message: format!("{}: {}", path.display(), e),
            })?;
        file.set_len(len).map_err(|e| Error::io(&e))?;
        Ok(FileDevice {
            file,
            path: path.to_path_buf(),
            writable: true,
            len,
        })
    }

    /// Path this device was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the device was opened for writing.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Sync and close, consuming the device.
    pub fn into_file(mut self) -> Result<File> {
        if self.writable {
            self.file.flush().map_err(|e| Error::io(&e))?;
            self.file.sync_all().map_err(|e| Error::io(&e))?;
        }
        Ok(self.file)
    }
}

impl BlockDevice for FileDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::overflow("file read"))?;
        if end > self.len {
            return Err(Error::Truncated {
                what: "file read",
                needed: end as usize,
                available: self.len as usize,
            });
        }
        // `read_exact_at` would do, but going through a positioned `Read` keeps
        // the offset arithmetic in one place and works on older toolchains.
        let mut f = &self.file;
        f.seek(SeekFrom::Start(offset)).map_err(|e| Error::io(&e))?;
        f.read_exact(buf).map_err(|e| Error::Io {
            message: format!("{}: read at {}: {}", self.path.display(), offset, e),
        })
    }

    fn len(&self) -> Result<u64> {
        Ok(self.len)
    }
}

impl BlockDeviceMut for FileDevice {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::overflow("file write"))?;
        if end > self.len {
            return Err(Error::Truncated {
                what: "file write",
                needed: end as usize,
                available: self.len as usize,
            });
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io(&e))?;
        self.file.write_all(buf).map_err(|e| Error::Io {
            message: format!("{}: write at {}: {}", self.path.display(), offset, e),
        })
    }

    fn sync(&mut self) -> Result<()> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        self.file.flush().map_err(|e| Error::io(&e))?;
        self.file.sync_all().map_err(|e| Error::io(&e))
    }
}
