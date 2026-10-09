// SPDX-License-Identifier: BSD-2-Clause

//! The FUSE filesystem implementation.
//!
//! Each callback follows the pattern: validate/convert arguments → call the
//! `hfsplus` library → convert the result → reply. No B-tree, extent, catalog,
//! journal, or compression logic lives here.
//!
//! # Thread safety
//!
//! `fuser` requires `Filesystem + Send + Sync + 'static`. The read volume is
//! held in an `Arc` behind an `RwLock` for lazy initialization and
//! invalidation after writes. Write operations open a fresh `WritableVolume`
//! from a newly-opened `FileDevice`, then invalidate the cached read volume.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use fuser::Errno;
use fuser::{AccessFlags, FileHandle, INodeNo, OpenFlags, Request};
use fuser::{CopyFileRangeFlags, ReplyBmap};
use fuser::{FileType, Filesystem, Generation};
use fuser::{
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry,
    ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
};

use hfsplus::blockdev::FileDevice;
use hfsplus::catalog::lookup::DirCursor;
use hfsplus::catalog::Cnid;
use hfsplus::volume::{Object, Volume, WritableVolume};
use hfsplus::Error as HfsError;

use crate::attr::{dir_attrs_to_file_attr, file_attrs_to_file_attr};
use crate::error::errno_of;
use crate::handles::{HandleTable, OpenDir, OpenFile};

/// Attribute cache TTL for the initial implementation.
const TTL: Duration = Duration::from_secs(1);

/// Holds a `FileDevice` and the `Volume` that borrows it.
///
/// Both are owned together so the volume's borrow of the device is valid for
/// the lifetime of this struct. The `FileDevice` is dropped before the `Volume`
/// (Rust drops struct fields in reverse declaration order), so the borrowed
/// reference remains valid.
pub struct VolumeHolder {
    _device: FileDevice,
    volume: Volume<'static, FileDevice>,
}

impl VolumeHolder {
    /// The cached read-only volume.
    #[doc(hidden)]
    pub fn volume(&self) -> &Volume<'static, FileDevice> {
        &self.volume
    }
}

/// Create a `VolumeHolder` by opening `image_path` read-only.
///
/// This encapsulates the transmute from `&'a FileDevice` to `'static`: the
/// `FileDevice` is moved into the `VolumeHolder` alongside the `Volume`, so the
/// borrowed reference remains valid for the holder's lifetime.
fn make_holder(image_path: &str) -> hfsplus::Result<(VolumeHolder, bool)> {
    let device = FileDevice::open(image_path)?;
    let volume = Volume::open(&device)?;
    let expanded = volume.header().has_expanded_times();
    let volume: Volume<'static, FileDevice> = unsafe { std::mem::transmute(volume) };
    Ok((
        VolumeHolder {
            _device: device,
            volume,
        },
        expanded,
    ))
}

/// The FUSE filesystem object.
///
/// Stores the image path and a lazily-initialized, cached read volume behind an
/// `RwLock`. Write operations open a fresh `WritableVolume`, perform the
/// mutation, and then invalidate the cache so subsequent reads observe the new
/// on-disk state.
pub struct HfsPlusFilesystem {
    image_path: String,
    /// Cached read volume. Set to `None` after a write so the next read
    /// re-opens from disk.
    volume: RwLock<Option<Arc<VolumeHolder>>>,
    expanded_times: bool,
    handles: HandleTable,
}

impl HfsPlusFilesystem {
    /// Mount `image_path` as a read-only FUSE filesystem.
    pub fn open(image_path: &str) -> hfsplus::Result<Self> {
        let (holder, expanded) = make_holder(image_path)?;
        Ok(Self {
            image_path: image_path.to_string(),
            volume: RwLock::new(Some(Arc::new(holder))),
            expanded_times: expanded,
            handles: HandleTable::default(),
        })
    }

    /// Mount `image_path` as a writable FUSE filesystem.
    ///
    /// Validates that the image can be opened read-write and that the volume
    /// is sound. The read volume is cached until invalidated by a write
    /// operation.
    pub fn open_writable(image_path: &str) -> hfsplus::Result<Self> {
        // Validate writability by opening the device read-write.
        let _probe = FileDevice::open_writable(image_path)?;
        drop(_probe);

        let (holder, expanded) = make_holder(image_path)?;
        Ok(Self {
            image_path: image_path.to_string(),
            volume: RwLock::new(Some(Arc::new(holder))),
            expanded_times: expanded,
            handles: HandleTable::default(),
        })
    }

    /// Lazily get a reference to the cached read volume.
    ///
    /// Returns a clone of the `Arc<VolumeHolder>` so the caller can access the
    /// volume without holding the lock. On first access (or after invalidation),
    /// the volume is opened from the image path.
    #[doc(hidden)]
    pub fn get_read_volume(&self) -> hfsplus::Result<Arc<VolumeHolder>> {
        // Fast path: check if the volume is cached.
        {
            let guard = self.volume.read().unwrap();
            if let Some(holder) = guard.as_ref() {
                return Ok(Arc::clone(holder));
            }
        }

        // Slow path: need to initialize. Upgrade to write lock.
        let mut guard = self.volume.write().unwrap();
        if guard.is_none() {
            let (holder, _) = make_holder(&self.image_path)?;
            *guard = Some(Arc::new(holder));
        }
        Ok(Arc::clone(guard.as_ref().unwrap()))
    }

    /// Open a fresh writable volume, run `f`, then invalidate the read cache.
    ///
    /// Each write operation gets its own `FileDevice` opened read-write. After
    /// the closure returns, the cached read volume is set to `None` so the next
    /// read re-opens from disk and sees the mutation.
    #[doc(hidden)]
    pub fn with_writable<R>(
        &self,
        f: impl FnOnce(&mut WritableVolume<FileDevice>) -> hfsplus::Result<R>,
    ) -> hfsplus::Result<R> {
        let mut device = FileDevice::open_writable(&self.image_path)?;
        let result = {
            let mut wvol = WritableVolume::open(&mut device)?;
            f(&mut wvol)
        };

        // Invalidate the cached read volume.
        let mut guard = self.volume.write().unwrap();
        *guard = None;

        result
    }

    /// Read current file data and merge new data at `offset`, via a fresh
    /// read-write device. Returns the merged data and invalidates the cache.
    ///
    /// Opens one `FileDevice` in read-write mode: first a `Volume` is opened for
    /// reading the existing content, then a `WritableVolume` for writing. The
    /// volume is dropped between the two phases so the mutable borrow can
    /// proceed.
    #[doc(hidden)]
    pub fn read_modify_write(&self, cnid: u32, offset: u64, data: &[u8]) -> hfsplus::Result<()> {
        self.read_modify_write_n(cnid, offset, data)?;
        Ok(())
    }

    /// Read current file data and merge new data at `offset`, via a fresh
    /// read-write device. Returns the number of bytes written and invalidates
    /// the cache.
    ///
    /// Opens one `FileDevice` in read-write mode: first a `Volume` is opened for
    /// reading the existing content, then a `WritableVolume` for writing. The
    /// volume is dropped between the two phases so the mutable borrow can
    /// proceed.
    #[doc(hidden)]
    pub fn read_modify_write_n(
        &self,
        cnid: u32,
        offset: u64,
        data: &[u8],
    ) -> hfsplus::Result<usize> {
        let mut device = FileDevice::open_writable(&self.image_path)?;

        // Phase 1: read existing content through a read-only Volume.
        let merged = {
            let vol = Volume::open(&device)?;
            let obj = vol.lookup_cnid(Cnid(cnid))?;
            match &obj {
                Some(object) => {
                    let current = vol.read(object, 0, usize::MAX)?;
                    let end = (offset + data.len() as u64) as usize;
                    let mut merged = current;
                    if merged.len() < end {
                        merged.resize(end, 0);
                    }
                    merged[(offset as usize)..end].copy_from_slice(data);
                    merged
                }
                None => return Err(HfsError::NotFound { what: "inode" }),
            }
        };
        // `vol` is dropped here, releasing the shared borrow on `device`.

        let written = merged.len();

        // Phase 2: write merged content through a writable volume.
        {
            let mut wvol = WritableVolume::open(&mut device)?;
            wvol.write_file_contents(cnid, &merged)?;
        }

        // Invalidate the cached read volume.
        let mut guard = self.volume.write().unwrap();
        *guard = None;

        Ok(written)
    }

    /// Look up an object by CNID from the cached volume.
    ///
    /// Look up an object by CNID from the cached volume.
    ///
    /// Convenience for callbacks that need both the object and the volume.
    #[doc(hidden)]
    pub fn lookup_cnid(&self, cnid: Cnid) -> hfsplus::Result<Option<Object>> {
        let holder = self.get_read_volume()?;
        holder.volume.lookup_cnid(cnid)
    }

    /// Invalidate the cached read volume so the next read sees fresh data.
    #[doc(hidden)]
    pub fn invalidate_volume(&self) {
        let mut guard = self.volume.write().unwrap();
        *guard = None;
    }
}

/// Internal helper: convert an `hfsplus::Error` to a FUSE `Errno`.
fn fuse_errno(e: &hfsplus::Error) -> Errno {
    errno_of(e)
}

/// Convert a FUSE name (bytes) to UTF-16 for HFS+.
fn name_to_utf16(name: &OsStr) -> Option<Vec<u16>> {
    std::str::from_utf8(name.as_bytes())
        .ok()
        .map(|s| s.encode_utf16().collect())
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
fn utf16_to_bytes(name: &[u16]) -> Vec<u8> {
    let s = String::from_utf16_lossy(name);
    s.into_bytes()
}

/// Build a synthetic `FileAttr` for a file created by `create_file`.
///
/// The catalog record has just been written; a fresh read may not yet see it
/// on all backends, so synthesize the best-effort attributes.
fn synthetic_file_attr(cnid: u32) -> fuser::FileAttr {
    let now = std::time::SystemTime::now();
    fuser::FileAttr {
        ino: fuser::INodeNo(cnid as u64),
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        flags: 0,
    }
}

/// Build a synthetic `FileAttr` for a directory created by `create_folder`.
fn synthetic_dir_attr(cnid: u32) -> fuser::FileAttr {
    let now = std::time::SystemTime::now();
    fuser::FileAttr {
        ino: fuser::INodeNo(cnid as u64),
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        flags: 0,
    }
}

impl Filesystem for HfsPlusFilesystem {
    fn init(
        &mut self,
        _req: &Request,
        _config: &mut fuser::KernelConfig,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn destroy(&mut self) {
        let mut guard = self.volume.write().unwrap();
        *guard = None;
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let utf16 = match name_to_utf16(name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let parent_cnid = Cnid(parent.0 as u32);

        match self.get_read_volume() {
            Ok(holder) => match holder.volume.lookup(parent_cnid, &utf16) {
                Ok(Some(object)) => {
                    let attr = object_to_attr(&object, self.expanded_times);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                Ok(None) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {}

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.lookup_cnid(Cnid(ino.0 as u32)) {
            Ok(Some(object)) => {
                let attr = object_to_attr(&object, self.expanded_times);
                reply.attr(&TTL, &attr);
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.lookup_cnid(Cnid(ino.0 as u32)) {
            Ok(Some(object)) => match self.get_read_volume() {
                Ok(holder) => match holder.volume.read_link(&object) {
                    Ok(target) => reply.data(target.as_bytes()),
                    Err(e) => reply.error(fuse_errno(&e)),
                },
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        // FUSE_SEEK_DATA = 4, FUSE_SEEK_HOLE = 8 (same as Linux userspace constants).
        const SEEK_DATA: i32 = 4;
        const SEEK_HOLE: i32 = 8;

        let cnid = Cnid(ino.0 as u32);
        let result = match self.get_read_volume() {
            Ok(holder) => match self.lookup_cnid(cnid) {
                Ok(Some(object)) if !object.is_dir() => match whence {
                    SEEK_DATA => match holder.volume.seek_data(&object, offset as u64) {
                        Ok(Some(pos)) => Ok(pos),
                        Ok(None) => Err(Errno::ENXIO),
                        Err(e) => Err(fuse_errno(&e)),
                    },
                    SEEK_HOLE => match holder.volume.seek_hole(&object, offset as u64) {
                        Ok(Some(pos)) => Ok(pos),
                        Ok(None) => Err(Errno::ENXIO),
                        Err(e) => Err(fuse_errno(&e)),
                    },
                    _ => Err(Errno::EINVAL),
                },
                Ok(Some(_)) => Err(Errno::EISDIR),
                Ok(None) => Err(Errno::ENOENT),
                Err(e) => Err(fuse_errno(&e)),
            },
            Err(e) => Err(fuse_errno(&e)),
        };
        match result {
            Ok(pos) => reply.offset(pos as i64),
            Err(e) => reply.error(e),
        }
    }

    fn bmap(&self, _req: &Request, ino: INodeNo, _blocksize: u32, idx: u64, reply: ReplyBmap) {
        // FUSE passes the logical block index; the block size is the FUSE-level
        // block size (the volume allocation block size). Translate the index to a
        // byte offset and ask the library for the physical device byte offset.
        let holder = match self.get_read_volume() {
            Ok(h) => h,
            Err(e) => {
                reply.error(fuse_errno(&e));
                return;
            }
        };
        let cnid = Cnid(ino.0 as u32);
        match holder.volume.lookup_cnid(cnid) {
            Ok(Some(object)) => {
                if object.is_dir() {
                    reply.error(Errno::EISDIR);
                    return;
                }
                let block_size = u64::from(holder.volume.header().block_size);
                let offset = idx * block_size;
                match holder.volume.bmap(&object, offset) {
                    Ok(phys) => reply.bmap(phys / block_size),
                    Err(e) => reply.error(fuse_errno(&e)),
                }
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.lookup_cnid(Cnid(ino.0 as u32)) {
            Ok(Some(object)) if !object.is_dir() => {
                let fh = self
                    .handles
                    .insert_file(OpenFile::new_file(Cnid(ino.0 as u32)));
                reply.opened(FileHandle(fh), fuser::FopenFlags::empty());
            }
            Ok(Some(_)) => reply.error(Errno::EISDIR),
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
        match self.get_read_volume() {
            Ok(holder) => match holder.volume.lookup_cnid(cnid) {
                Ok(Some(object)) => match holder.volume.read(&object, offset, size as usize) {
                    Ok(data) => reply.data(&data),
                    Err(e) => reply.error(fuse_errno(&e)),
                },
                Ok(None) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(fuse_errno(&e)),
            },
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
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.lookup_cnid(Cnid(ino.0 as u32)) {
            Ok(Some(object)) if object.is_dir() => {
                let cursor = DirCursor::start();
                let fh = self.handles.insert_dir(OpenDir {
                    cnid: Cnid(ino.0 as u32),
                    cursor,
                });
                reply.opened(FileHandle(fh), fuser::FopenFlags::empty());
            }
            Ok(Some(_)) => reply.error(Errno::ENOTDIR),
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

        let cursor = self
            .handles
            .get_dir(fh.0)
            .map(|h| h.cursor)
            .unwrap_or_else(DirCursor::start);

        match self.get_read_volume() {
            Ok(holder) => match holder.volume.read_dir_plus(cnid, cursor, 0) {
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
            },
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

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let cnid = Cnid(ino.0 as u32);
        let skip = offset as usize;

        let cursor = self
            .handles
            .get_dir(fh.0)
            .map(|h| h.cursor)
            .unwrap_or_else(DirCursor::start);

        match self.get_read_volume() {
            Ok(holder) => match holder.volume.read_dir_plus(cnid, cursor, 0) {
                Ok((entries, _next_cursor)) => {
                    for (sent, entry) in (0_u64..).zip(entries.iter().skip(skip)) {
                        let entry_ino = fuser::INodeNo(entry.cnid().0 as u64);
                        let attr = object_to_attr(entry, self.expanded_times);
                        let name_bytes = entry.name();
                        let name = utf16_to_bytes(name_bytes);
                        let os_name = std::ffi::OsStr::from_bytes(&name);
                        if reply.add(entry_ino, sent, os_name, &TTL, &attr, Generation(0)) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.get_read_volume() {
            Ok(holder) => match holder.volume.statfs() {
                Ok(stat) => {
                    reply.statfs(
                        stat.total_blocks as u64,
                        stat.free_blocks as u64,
                        stat.free_blocks as u64,
                        stat.file_count as u64 + stat.folder_count as u64,
                        0,
                        stat.block_size,
                        stat.max_name_len * 4,
                        stat.block_size,
                    );
                }
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    // =========================================================================
    // Writable operations
    // =========================================================================

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let utf16 = match name_to_utf16(name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_cnid = Cnid(parent.0 as u32);
        match self.with_writable(|wvol| wvol.create_file(parent_cnid.0, &utf16)) {
            Ok(cnid) => {
                let fh = self.handles.insert_file(OpenFile::new_file(Cnid(cnid)));
                let attr = synthetic_file_attr(cnid);
                reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    fuser::FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let utf16 = match name_to_utf16(name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_cnid = Cnid(parent.0 as u32);
        match self.with_writable(|wvol| wvol.create_folder(parent_cnid.0, &utf16)) {
            Ok(cnid) => {
                let attr = synthetic_dir_attr(cnid);
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let utf16 = match name_to_utf16(name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_cnid = Cnid(parent.0 as u32);
        match self.with_writable(|wvol| wvol.remove(parent_cnid.0, &utf16)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let utf16 = match name_to_utf16(name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_cnid = Cnid(parent.0 as u32);
        match self.with_writable(|wvol| wvol.remove(parent_cnid.0, &utf16)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let utf16 = match name_to_utf16(newname) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_cnid = newparent.0 as u32;
        let target_cnid = ino.0 as u32;
        match self.with_writable(|wvol| wvol.create_hard_link(parent_cnid, &utf16, target_cnid)) {
            Ok(cnid) => match self.lookup_cnid(Cnid(cnid)) {
                Ok(Some(object)) => {
                    let attr = object_to_attr(&object, self.expanded_times);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                _ => reply.error(Errno::ENOENT),
            },
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let from_name = match name_to_utf16(name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let to_name = match name_to_utf16(newname) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let from_parent = Cnid(parent.0 as u32);
        let to_parent = Cnid(newparent.0 as u32);
        match self
            .with_writable(|wvol| wvol.rename(from_parent.0, &from_name, to_parent.0, &to_name))
        {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let cnid = ino.0 as u32;
        match self.read_modify_write(cnid, offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let cnid = ino.0 as u32;

        if let Some(new_size) = size {
            match self.with_writable(|wvol| wvol.truncate_file(cnid, new_size)) {
                Ok(_) => {}
                Err(e) => {
                    reply.error(fuse_errno(&e));
                    return;
                }
            }
        }

        let has_meta_changes =
            mode.is_some() || uid.is_some() || gid.is_some() || atime.is_some() || mtime.is_some();
        if has_meta_changes {
            let changes = hfsplus::volume::FileMetadataChanges {
                mode,
                uid,
                gid,
                atime: atime.and_then(|t| match t {
                    fuser::TimeOrNow::SpecificTime(t) => t
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs() as i64),
                    fuser::TimeOrNow::Now => Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                }),
                mtime: mtime.and_then(|t| match t {
                    fuser::TimeOrNow::SpecificTime(t) => t
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs() as i64),
                    fuser::TimeOrNow::Now => Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                }),
                ctime: None,
            };
            match self.with_writable(|wvol| wvol.modify_file_metadata(cnid, changes)) {
                Ok(_) => {}
                Err(e) => {
                    reply.error(fuse_errno(&e));
                    return;
                }
            }
        }

        // Report the (possibly updated) attributes from a fresh read.
        match self.lookup_cnid(Cnid(cnid)) {
            Ok(Some(object)) => {
                let attr = object_to_attr(&object, self.expanded_times);
                reply.attr(&TTL, &attr);
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let utf16 = match name_to_utf16(link_name) {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_cnid = parent.0 as u32;
        let target = target.to_string_lossy();
        let target_bytes = target.as_bytes();
        match self.with_writable(|wvol| wvol.create_symlink(parent_cnid, &utf16, target_bytes)) {
            Ok(cnid) => {
                let attr = synthetic_file_attr(cnid);
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let cnid = ino.0 as u32;
        let name_str = match std::str::from_utf8(name.as_bytes()) {
            Ok(s) => s,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.with_writable(|wvol| wvol.setxattr(cnid, name_str, value)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let cnid = ino.0 as u32;
        let name_str = match std::str::from_utf8(name.as_bytes()) {
            Ok(s) => s,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.with_writable(|wvol| wvol.removexattr(cnid, name_str)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let cnid = Cnid(ino.0 as u32);
        let name_str = match std::str::from_utf8(name.as_bytes()) {
            Ok(s) => s,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.get_read_volume() {
            Ok(holder) => match holder.volume.lookup_cnid(cnid) {
                Ok(Some(object)) => match holder.volume.getxattr(&object, name_str) {
                    Ok(Some(value)) => {
                        if size == 0 {
                            reply.size(value.len() as u32);
                        } else if value.len() <= size as usize {
                            reply.data(&value);
                        } else {
                            reply.error(Errno::ERANGE);
                        }
                    }
                    Ok(None) => reply.error(Errno::ENODATA),
                    Err(e) => reply.error(fuse_errno(&e)),
                },
                Ok(None) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let cnid = Cnid(ino.0 as u32);
        match self.get_read_volume() {
            Ok(holder) => match holder.volume.lookup_cnid(cnid) {
                Ok(Some(object)) => match holder.volume.listxattr(&object) {
                    Ok(names) => {
                        // FUSE expects a NUL-separated list of attribute names.
                        let mut buf = Vec::new();
                        for n in &names {
                            buf.extend_from_slice(n.as_bytes());
                            buf.push(0);
                        }
                        if size == 0 {
                            reply.size(buf.len() as u32);
                        } else if buf.len() <= size as usize {
                            reply.data(&buf);
                        } else {
                            reply.error(Errno::ERANGE);
                        }
                    }
                    Err(e) => reply.error(fuse_errno(&e)),
                },
                Ok(None) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(fuse_errno(&e)),
            },
            Err(e) => reply.error(fuse_errno(&e)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // In the writable implementation, each write operation commits its own
        // journal transaction before returning. There is nothing to flush here.
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // Transactions are committed atomically on each write, so flush is a no-op.
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // Each directory-modifying operation (create, mkdir, unlink, rmdir,
        // rename) commits a journal transaction before returning, so there is
        // nothing to flush on directory sync.
        reply.ok();
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        // Linux fallocate mode flags:
        const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
        const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;

        let cnid = ino.0 as u32;

        if mode & FALLOC_FL_PUNCH_HOLE == FALLOC_FL_PUNCH_HOLE {
            // Requires KEEP_SIZE per POSIX.
            match self.with_writable(|wvol| wvol.punch_hole(cnid, offset, length)) {
                Ok(_) => reply.ok(),
                Err(e) => reply.error(fuse_errno(&e)),
            }
        } else if mode == 0 || mode == FALLOC_FL_KEEP_SIZE {
            // Default: allocate space (possibly extending the file). HFS+
            // allocates on write, so we zero-extend to ensure the blocks exist.
            // The library's truncate_file handles both growth (allocate + zero)
            // and shrink.
            let target = offset.saturating_add(length);
            match self.with_writable(|wvol| wvol.truncate_file(cnid, target)) {
                Ok(_) => reply.ok(),
                Err(e) => reply.error(fuse_errno(&e)),
            }
        } else {
            reply.error(Errno::EOPNOTSUPP);
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        _fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        _fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let cnid_in = Cnid(ino_in.0 as u32);
        let cnid_out = Cnid(ino_out.0 as u32);

        // Read source data.
        let holder = match self.get_read_volume() {
            Ok(h) => h,
            Err(e) => {
                reply.error(fuse_errno(&e));
                return;
            }
        };

        let data = match holder.volume.lookup_cnid(cnid_in) {
            Ok(Some(object)) if !object.is_dir() => {
                match holder.volume.read(&object, offset_in, len as usize) {
                    Ok(d) => d,
                    Err(e) => {
                        reply.error(fuse_errno(&e));
                        return;
                    }
                }
            }
            Ok(Some(_)) => {
                reply.error(Errno::EISDIR);
                return;
            }
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(fuse_errno(&e));
                return;
            }
        };

        drop(holder);

        // Perform read-modify-write on the destination: read existing content,
        // splice in the new data, write back.
        let written = match self.read_modify_write_n(cnid_out.0, offset_out, &data) {
            Ok(n) => n,
            Err(e) => {
                reply.error(fuse_errno(&e));
                return;
            }
        };

        reply.written(written as u32);
    }
}
