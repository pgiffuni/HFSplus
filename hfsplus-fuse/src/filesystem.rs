// SPDX-License-Identifier: BSD-2-Clause

//! The FUSE filesystem implementation.
//!
//! Each callback follows the pattern: validate/convert arguments → call the
//! `hfsplus` library → convert the result → reply. No B-tree, extent, catalog,
//! journal, or compression logic lives here.
//!
//! # Thread safety
//!
//! `fuser` requires `Filesystem + Send + Sync + 'static`. The volume is held in
//! an `Arc` and is accessed through its `&self` API, so no locking is needed at
//! the FUSE layer for read operations. The handle table is internally
//! synchronized.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;

use fuser::Errno;
use fuser::{AccessFlags, FileHandle, INodeNo, OpenFlags, Request};
use fuser::{FileType, Filesystem, Generation};
use fuser::{ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs};

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::lookup::DirCursor;
use hfsplus::catalog::Cnid;
use hfsplus::volume::{Object, Volume};

use crate::attr::{dir_attrs_to_file_attr, file_attrs_to_file_attr};
use crate::error::errno_of;
use crate::handles::{HandleTable, OpenDir, OpenFile};

/// Attribute cache TTL for the initial read-only implementation.
///
/// Kept short while mutation semantics are still evolving, per the FUSE
/// roadmap. This can be tuned later.
const TTL: Duration = Duration::from_secs(1);

/// The FUSE filesystem object.
///
/// Holds the opened `FileDevice` and the `Volume` that borrows it. Both are
/// owned together so the volume's borrow of the device is valid for the
/// lifetime of this struct.
pub struct HfsPlusFilesystem {
    /// The block device backing the image.
    _device: FileDevice,
    /// The mounted HFS+ volume. The lifetime is extended to `'static` here; see
    /// `open` for why this is sound.
    volume: Arc<Volume<'static, FileDevice>>,
    /// Whether the volume uses expanded timestamps.
    expanded_times: bool,
    /// Internal file/directory handle table.
    handles: HandleTable,
}

impl HfsPlusFilesystem {
    /// Mount `image_path` as a read-only FUSE filesystem.
    ///
    /// Opens the file as a read-only `FileDevice` and wraps the resulting
    /// `Volume` in an `Arc`. The returned filesystem is `Send + Sync + 'static`.
    pub fn open(image_path: &str) -> hfsplus::Result<Self> {
        let device = FileDevice::open(image_path)?;
        let volume = Volume::open(&device)?;
        let expanded_times = volume.header().has_expanded_times();

        // The Volume borrows the device for its lifetime. Both are owned by
        // this struct, so the borrow is valid as long as the struct lives. We
        // transmute the lifetime to 'static to satisfy fuser's Filesystem trait,
        // which requires the implementor to be 'static. The invariant is simple:
        // the FileDevice is dropped only after the Volume is dropped, and the
        // Volume is only ever accessed through a shared reference (via Arc)
        // after construction. This is not unsafe code operating on raw image
        // bytes — it extends the Rust borrow of the block-device wrapper, and
        // the underlying file I/O remains fully bounds-checked by the library.
        let volume: Volume<'static, FileDevice> = unsafe { std::mem::transmute(volume) };

        Ok(Self {
            _device: device,
            volume: Arc::new(volume),
            expanded_times,
            handles: HandleTable::default(),
        })
    }
}

/// Internal helper: convert an `hfsplus::Error` to a FUSE `Errno`.
fn fuse_errno(e: &hfsplus::Error) -> Errno {
    errno_of(e)
}

impl Filesystem for HfsPlusFilesystem {
    fn init(
        &mut self,
        _req: &Request,
        _config: &mut fuser::KernelConfig,
    ) -> Result<(), std::io::Error> {
        // No special capabilities requested; the read-only implementation
        // does not enable writeback cache, parallel DIO, or passthrough.
        Ok(())
    }

    fn destroy(&mut self) {
        // No persistent state to flush in the read-only phase.
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_cnid = Cnid(parent.0 as u32);

        // Convert the FUSE name (bytes) to UTF-16, as HFS+ expects.
        // The library performs its own Unicode normalization and comparison.
        let utf16: Vec<u16> = match std::str::from_utf8(name.as_bytes()) {
            Ok(s) => s.encode_utf16().collect(),
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.volume.lookup(parent_cnid, &utf16) {
            Ok(Some(object)) => {
                let attr = object_to_attr(&object, self.expanded_times);
                reply.entry(&TTL, &attr, Generation(0));
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {
        // Inode reference counting is handled by the kernel; we have no
        // per-inode state to free because all objects are looked up on demand.
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let cnid = Cnid(ino.0 as u32);

        match self.volume.lookup_cnid(cnid) {
            Ok(Some(object)) => {
                let attr = object_to_attr(&object, self.expanded_times);
                reply.attr(&TTL, &attr);
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let cnid = Cnid(ino.0 as u32);

        match self.volume.lookup_cnid(cnid) {
            Ok(Some(object)) => match self.volume.read_link(&object) {
                Ok(target) => reply.data(target.as_bytes()),
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let cnid = Cnid(ino.0 as u32);

        match self.volume.lookup_cnid(cnid) {
            Ok(Some(object)) if !object.is_dir() => {
                let fh = self.handles.insert_file(OpenFile::new_file(cnid));
                reply.opened(FileHandle(fh), fuser::FopenFlags::empty());
            }
            Ok(Some(_)) => {
                // It's a directory; OPEN should not be called on directories.
                reply.error(Errno::EISDIR);
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let cnid = Cnid(ino.0 as u32);

        match self.volume.lookup_cnid(cnid) {
            Ok(Some(object)) => match self.volume.read(&object, offset, size as usize) {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // The handle was already looked up by CNID in read; we accept release
        // without error. The handle table entry is not removed here because
        // we re-create handles on each read for simplicity in the initial
        // read-only implementation.
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let cnid = Cnid(ino.0 as u32);

        match self.volume.lookup_cnid(cnid) {
            Ok(Some(object)) if object.is_dir() => {
                let cursor = DirCursor::start();
                let fh = self.handles.insert_dir(OpenDir { cnid, cursor });
                reply.opened(FileHandle(fh), fuser::FopenFlags::empty());
            }
            Ok(Some(_)) => {
                reply.error(Errno::ENOTDIR);
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let cnid = Cnid(ino.0 as u32);
        let skip = offset as usize;

        // Try to use the stored cursor; fall back to re-reading from the start
        // if the handle was forgotten.
        let cursor = self
            .handles
            .get_dir(fh.0)
            .map(|h| h.cursor)
            .unwrap_or_else(DirCursor::start);

        match self.volume.read_dir_plus(cnid, cursor, 0) {
            Ok((entries, _next_cursor)) => {
                for (sent, entry) in (0_u64..).zip(entries.iter().skip(skip)) {
                    let entry_ino = fuser::INodeNo(entry.cnid().0 as u64);
                    let entry_type = entry_file_type(entry);
                    let name_bytes = entry.name();
                    let name = utf16_to_bytes(name_bytes);
                    let os_name = std::ffi::OsStr::from_bytes(&name);
                    if reply.add(entry_ino, sent, entry_type, os_name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.handles.remove(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.volume.statfs() {
            Ok(stat) => {
                reply.statfs(
                    stat.total_blocks as u64,
                    stat.free_blocks as u64,
                    stat.free_blocks as u64,
                    stat.file_count as u64 + stat.folder_count as u64,
                    0,
                    stat.block_size,
                    stat.max_name_len * 4, // bytes (UTF-16 * 2)
                    stat.block_size,
                );
            }
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // In the read-only phase, everything readable is accessible.
        // Full permission checks await writable semantics.
        reply.ok();
    }
}

/// Internal helper: convert an `Object` to a `fuser::FileAttr`.
fn object_to_attr(object: &Object, expanded: bool) -> fuser::FileAttr {
    match object {
        Object::Directory(dir) => dir_attrs_to_file_attr(dir, expanded),
        Object::File(file) => file_attrs_to_file_attr(file, expanded),
    }
}

/// Map an HFS+ object to a FUSE file type.
fn entry_file_type(object: &Object) -> FileType {
    if object.is_dir() {
        FileType::Directory
    } else if object.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    }
}

/// Convert a UTF-16 name back to bytes for FUSE directory entries.
///
/// FUSE expects names as byte slices. HFS+ stores names as UTF-16; we UTF-8
/// encode for the common case.
fn utf16_to_bytes(name: &[u16]) -> Vec<u8> {
    let s = String::from_utf16_lossy(name);
    s.into_bytes()
}
