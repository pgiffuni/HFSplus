// SPDX-License-Identifier: BSD-2-Clause

//! Internal file handle table for the FUSE adapter.
//!
//! FUSE assigns an opaque 64-bit file handle to each `OPEN` or `OPENDIR`
//! request. Rather than encoding a raw pointer into that handle (which would
//! be unsafe and fragile), we maintain a small internal table. A FUSE
//! file-handle token indexes into this table and resolves to an [`OpenFile`]
//! or [`OpenDir`], which wraps the HFS+ object state needed across multiple
//! callbacks (`READ`, `RELEASE`, `FLUSH`, etc.).

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use hfsplus::catalog::lookup::DirCursor;
use hfsplus::catalog::Cnid;

/// The next file handle token to issue.
static NEXT_FH: AtomicU64 = AtomicU64::new(1);

/// A file handle entry: the HFS+ CNID of the open file and whether it is a
/// directory handle.
pub struct OpenFile {
    /// The HFS+ Catalog Node ID of the open file.
    pub cnid: Cnid,
    /// Whether this handle was opened as a directory.
    pub is_dir: bool,
}

impl OpenFile {
    /// Create a handle for a regular file.
    pub fn new_file(cnid: Cnid) -> Self {
        Self {
            cnid,
            is_dir: false,
        }
    }

    /// Create a handle for a directory.
    pub fn new_dir(cnid: Cnid) -> Self {
        Self { cnid, is_dir: true }
    }
}

/// A directory handle: stores the cursor so `READDIR` can resume.
pub struct OpenDir {
    /// The HFS+ Catalog Node ID of the open directory.
    pub cnid: Cnid,
    /// The resumable cursor for the next `READDIR` call.
    pub cursor: DirCursor,
}

/// The single table mapping FUSE file-handle tokens to internal state.
///
/// `HandleTable` is `Sync` to satisfy `fuser`'s `Filesystem + Send + Sync`
/// `+ 'static` requirement. Access is guarded by a `Mutex` rather than a global
/// lock on the volume, so lookups and reads can proceed concurrently.
#[derive(Default)]
pub struct HandleTable {
    /// Maps a FUSE file-handle value to either an open file or directory.
    entries: Mutex<HashMap<u64, HandleEntry>>,
}

/// What the handle table stores for each open handle.
pub enum HandleEntry {
    File(Arc<OpenFile>),
    Dir(Arc<OpenDir>),
}

impl HandleTable {
    /// Allocate a new handle for an open file, returning the FUSE token.
    pub fn insert_file(&self, file: OpenFile) -> u64 {
        let token = NEXT_FH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.entries
            .lock()
            .unwrap()
            .insert(token, HandleEntry::File(Arc::new(file)));
        token
    }

    /// Allocate a new handle for an open directory, returning the FUSE token.
    pub fn insert_dir(&self, dir: OpenDir) -> u64 {
        let token = NEXT_FH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.entries
            .lock()
            .unwrap()
            .insert(token, HandleEntry::Dir(Arc::new(dir)));
        token
    }

    /// Retrieve the file handle for `token`, if any.
    pub fn get_file(&self, token: u64) -> Option<Arc<OpenFile>> {
        match self.entries.lock().unwrap().get(&token) {
            Some(HandleEntry::File(arc)) => Some(Arc::clone(arc)),
            _ => None,
        }
    }

    /// Retrieve the directory handle for `token`, if any.
    pub fn get_dir(&self, token: u64) -> Option<Arc<OpenDir>> {
        match self.entries.lock().unwrap().get(&token) {
            Some(HandleEntry::Dir(arc)) => Some(Arc::clone(arc)),
            _ => None,
        }
    }

    /// Remove a handle, returning the entry that was stored.
    pub fn remove(&self, token: u64) -> Option<HandleEntry> {
        self.entries.lock().unwrap().remove(&token)
    }
}
