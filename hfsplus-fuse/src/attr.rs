// SPDX-License-Identifier: BSD-2-Clause

//! Attribute conversion from HFS+ objects to FUSE `FileAttr`.
//!
//! This module is the single place where HFS+ timestamps, modes, and sizes are
//! translated into the `fuser::FileAttr` structure expected by the FUSE layer.
//! No other module should construct a `FileAttr`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hfsplus::catalog::record::{S_IFDIR, S_IFLNK, S_IFREG};
use hfsplus::catalog::Cnid;
use hfsplus::timestamp::to_bsd_time;
use hfsplus::volume::{DirAttrs, FileAttrs, Times};

/// Convert an HFS+ timestamp to a `SystemTime`.
///
/// HFS+ timestamps are either Mac OS epoch seconds (classic mode) or Unix
/// seconds (expanded-times mode). This function delegates to the library's
/// `to_bsd_time`, which handles both modes, the clamp-to-zero rule for
/// pre-epoch values, and the expanded-times epoch. Zero in classic mode
/// represents "never set" and is mapped to the Unix epoch (1970-01-01).
pub fn hfs_time_to_systemtime(raw: u32, expanded: bool) -> SystemTime {
    let secs = to_bsd_time(raw, expanded);
    UNIX_EPOCH + Duration::from_secs(secs as u64)
}

/// Convert the five timestamps of an HFS+ object into FUSE attribute fields.
pub fn times_to_fileattr(
    times: &Times,
    expanded: bool,
) -> (SystemTime, SystemTime, SystemTime, SystemTime) {
    let atime = hfs_time_to_systemtime(times.accessed.raw, expanded);
    let mtime = hfs_time_to_systemtime(times.modified.raw, expanded);
    let ctime = hfs_time_to_systemtime(times.attribute_modified.raw, expanded);
    // Birth time: if the created timestamp is the classic "unset" sentinel
    // (zero), fall back to the Unix epoch rather than a fabricated date.
    let crtime = if times.created.is_unset() {
        UNIX_EPOCH
    } else {
        hfs_time_to_systemtime(times.created.raw, expanded)
    };
    (atime, mtime, ctime, crtime)
}

/// Compute the POSIX mode for a directory.
fn dir_mode(dir: &DirAttrs) -> u32 {
    let perms = u32::from(dir.bsd_info.file_mode);
    perms | (S_IFDIR as u32)
}

/// Compute the POSIX mode for a file.
fn file_mode(file: &FileAttrs) -> u32 {
    let perms = u32::from(file.bsd_info.file_mode);
    let type_bits: u32 = if file.record.is_symlink() {
        S_IFLNK as u32
    } else {
        S_IFREG as u32
    };
    perms | type_bits
}

/// Build a `fuser::FileAttr` from a directory's attributes.
pub fn dir_attrs_to_file_attr(dir: &DirAttrs, expanded: bool) -> fuser::FileAttr {
    let mode = dir_mode(dir);
    let nlink = 2u32; // directories have . and .. entries
    let (atime, mtime, ctime, crtime) = times_to_fileattr(&dir.times, expanded);

    fuser::FileAttr {
        ino: fuser::INodeNo(dir.cnid.0 as u64),
        size: 0,
        blocks: 0,
        atime,
        mtime,
        ctime,
        crtime,
        kind: fuser::FileType::Directory,
        perm: (mode & 0o7777) as u16,
        nlink,
        uid: dir.bsd_info.owner_id,
        gid: dir.bsd_info.group_id,
        rdev: 0,
        blksize: 0,
        flags: 0,
    }
}

/// Build a `fuser::FileAttr` from a file's attributes.
pub fn file_attrs_to_file_attr(file: &FileAttrs, expanded: bool) -> fuser::FileAttr {
    let mode = file_mode(file);
    let nlink = file.link_count;
    let size = file.data_size;
    let (atime, mtime, ctime, crtime) = times_to_fileattr(&file.times, expanded);
    let kind = if file.record.is_symlink() {
        fuser::FileType::Symlink
    } else {
        fuser::FileType::RegularFile
    };

    fuser::FileAttr {
        ino: fuser::INodeNo(file.cnid.0 as u64),
        size,
        blocks: size / 512,
        atime,
        mtime,
        ctime,
        crtime,
        kind,
        perm: (mode & 0o7777) as u16,
        nlink,
        uid: file.bsd_info.owner_id,
        gid: file.bsd_info.group_id,
        rdev: 0,
        blksize: 0,
        flags: 0,
    }
}

/// Convert an `hfsplus::Cnid` to a FUSE inode number.
///
/// Per the FUSE architecture doc, FUSE inode numbers are HFS+ Catalog Node IDs
/// (CNIDs). This is a thin identity conversion.
pub fn cnid_to_inode(cnid: Cnid) -> u64 {
    cnid.0 as u64
}
