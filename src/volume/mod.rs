//! The read-only HFS+ filesystem.
//!
//! # What this type is
//!
//! A mounted volume: the volume header, the catalog B-tree, the allocation
//! bitmap and the journal's presence, presented as the handful of operations a
//! read-only FUSE adapter needs — `lookup`, `getattr`, `readdir`, `read`,
//! `readlink` and `statfs`. It performs no I/O of its own beyond the block
//! device, and it never writes: a read-only mount must be incapable of
//! modifying the image, not merely unwilling to.
//!
//! Mining reference: Apple `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) is
//! the equivalent, and `core/hfs_statfs.c` (`hfs_getattrlist`,
//! `hfs_bstatfs`) is where the statistics reported here come from. The catalog
//! operations correspond to `core/hfs_catalog.c`'s `cat_lookup`,
//! `cat_getdirentries` and `cat_idlookup`.
//!
//! # CNIDs cross the boundary, and only CNIDs
//!
//! Identity in this layer is the catalog node ID. That is the filesystem's own
//! inode number, it survives renames, and it is what the extents and attributes
//! B-trees key on. Mapping it to a FUSE inode number is the adapter's job and is
//! deliberately not done here, because a lossy mapping would break the caching
//! that makes a FUSE mount usable.

use crate::blockdev::{BlockDevice, BlockDeviceMut};
use crate::btree::node::NODE_DESCRIPTOR_SIZE;
use crate::btree::ExtentKey;
use crate::catalog::cnid::{Cnid, ROOT_FOLDER_ID, ROOT_PARENT_ID};
use crate::catalog::lookup::{Catalog, DirCursor};
use crate::catalog::record::{
    BsdInfo, CatalogRecord, FileRecord, FolderRecord, S_IFDIR, S_IFLNK, S_IFREG,
};
use crate::error::{Error, Result};
use crate::extent::OverflowResolver;
use crate::file::{ForkOverflow, ForkReader, TreeOverflow};
use crate::format::fork::ForkData;
use crate::format::volume_header::{FileSystemKind, VolumeHeader};
use crate::journal::info::{JournalHeader, JournalInfoBlock};
use crate::journal::replay::{commit_transaction, TransactionBuffer};

mod bitmap;

use crate::timestamp::HfsTimestamp;
pub use bitmap::{bytes_for_blocks, AllocationBitmap};

use crate::attributes::AttributesFile;

/// A mounted, read-only HFS+ or HFSX volume.
pub struct Volume<'a, D: ?Sized> {
    device: &'a D,
    header: VolumeHeader,
    catalog: Catalog<'a, D>,
    kind: FileSystemKind,
    /// The extents overflow B-tree, opened on first use.
    ///
    /// Opened lazily because a volume whose forks all fit inline never touches
    /// it, and on such a volume it may not even exist. Held here rather than
    /// built per read because a `ForkOverflow` borrows the tree it resolves
    /// against.
    extents: std::cell::OnceCell<TreeOverflow<'a, D>>,
    /// The attributes B-tree, opened on first use.
    ///
    /// Opened lazily for the same reason as `extents`: a volume whose files
    /// have no extended attributes never touches it, and the tree may be empty
    /// on a formatted volume.
    attributes: std::cell::OnceCell<AttributesFile<'a, D>>,
}

impl<'a, D: BlockDevice + ?Sized> std::fmt::Debug for Volume<'a, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("filesystem", &self.kind)
            .field("block_size", &self.header.block_size)
            .field("total_blocks", &self.header.total_blocks)
            .field("journaled", &self.header.is_journaled())
            .finish()
    }
}

impl<'a, D: BlockDevice + ?Sized> Volume<'a, D> {
    /// Open `device` as a read-only volume.
    ///
    /// Mining reference: Apple `hfs_MountHFSPlusVolume` performs the same
    /// sequence — validate the header, then open the extents, catalog and
    /// attributes B-trees against it — and rejects a volume whose primary header
    /// does not validate. It also refuses to mount a dirty, non-journaled volume
    /// read-write; this layer is read-only throughout, so that check does not
    /// apply, but the dirty bit is reported by [`Volume::is_clean`] so a caller
    /// can warn.
    /// What a mount owes the volume, and does not yet.
    ///
    /// TN1150, Volume Attributes, states three obligations around mounting, and
    /// this crate meets none of them. They are recorded here rather than left
    /// implicit, because each is a field on disk that a reader elsewhere will look
    /// at:
    ///
    /// - `kHFSVolumeUnmountedBit` (bit 8): "An implementation **must clear this bit**
    ///   on the media when it mounts a volume for writing. An implementation must
    ///   set this bit on the media as the last step of unmounting a writable volume,
    ///   after all other volume information has been flushed. **If an implementation
    ///   is asked to mount a volume where this bit is clear, it must assume the
    ///   volume is inconsistent**, and do appropriate consistency checking before
    ///   using the volume."
    /// - `kHFSBootVolumeInconsistentBit` (bit 11): the same, inverted. Set on mount
    ///   for writing, cleared as the last step of unmounting.
    /// - `writeCount`: "incremented every time a volume is mounted... **It is very
    ///   important that an implementation or utility change the writeCount field if
    ///   it modifies the volume's structures directly. This is particularly
    ///   important if it adds or deletes items on the volume.**"
    ///
    /// And one that belongs to any *writer* rather than to a mount:
    ///
    /// - `lastMountedVersion`: "**Any code which modifies the on disk structures
    ///   must also set this field to a unique value which identifies that code.**
    ///   Third-party implementations of HFS Plus should place a registered creator
    ///   code in this field."
    ///
    /// The last one is why `fsck.hfsplus` writes `fsc.k` into it and calls that a
    /// repair: the field is how another implementation learns that something other
    /// than Mac OS X has been writing here. A library that changes a volume without
    /// setting it leaves exactly that question open.
    ///
    /// The last item on that list -- clearing bit 8 on mount -- is also why
    /// [`Self::open`] does not treat a clear bit as fatal today. It reads the bit
    /// but does not act on it, which is a deliberate gap rather than an oversight:
    /// the check TN1150 describes is a full consistency pass, and a reader that
    /// performed one on every open would be far slower than one that does not.
    ///
    pub fn open(device: &'a D) -> Result<Self> {
        let header = VolumeHeader::read_from(device)?;
        let kind = header.kind()?;
        if !kind.is_supported() {
            return Err(Error::invalid(
                "volume_header.signature",
                format!("{} is not HFS+ or HFSX", kind_label(kind)),
            ));
        }

        let catalog = Catalog::open(
            device,
            &header.catalog_file,
            header.block_size,
            header.is_hfsx(),
        )?;

        // A journaled volume must be able to replay its journal before it is
        // handed to a caller. Otherwise the volume opens on whatever is on the
        // disk, and that filesystem is *stale*: every write the journal held is
        // missing, with nothing saying so.
        //
        // Apple makes this a mount failure rather than a warning --
        // `hfs_vfsops.c` `hfs_mount_existing` treats a NULL journal from
        // `journal_open` as EINVAL. Here it surfaces as an error from `open`, and
        // the journal itself is still fetched separately through
        // [`Volume::journal`], which borrows the same device.
        //
        // The check is one journal-header read: `Journal::open` parses the info
        // block, the header and any block lists, and the overlay it builds is
        // dropped with the temporary.
        //
        // `Ok(None)` is *not* a failure: it is how an external journal reports
        // itself, and such a volume is perfectly sound -- its journal is simply on
        // a partition this reader was not given.
        if header.is_journaled() {
            crate::journal::Journal::open(device, header.journal_info_block, header.block_size)?;
        }

        Ok(Volume {
            device,
            header,
            catalog,
            kind,
            extents: std::cell::OnceCell::new(),
            attributes: std::cell::OnceCell::new(),
        })
    }

    /// The volume header.
    pub fn header(&self) -> &VolumeHeader {
        &self.header
    }

    /// Which HFS family this is.
    pub fn kind(&self) -> FileSystemKind {
        self.kind
    }

    /// The catalog.
    pub fn catalog(&self) -> &Catalog<'a, D> {
        &self.catalog
    }

    /// The device this volume reads through.
    ///
    /// For a caller that needs a structure the `Volume` does not wrap -- the
    /// extents overflow B-tree, when resolving a fork whose extents spill. That
    /// tree is not catalog-shaped, so it cannot be reached through
    /// [`Volume::catalog`]. Mining reference: `core/hfs_extents.c` opens it from
    /// the same device the catalog came from.
    pub fn device(&self) -> &'a D {
        self.device
    }

    /// Read a fork whole, up to `limit` bytes.
    ///
    /// For the special files: the allocation bitmap is not a catalog object, so
    /// there is no [`Object`] to read it through.
    ///
    /// Overflow extents are not resolved. A caller needing a fork that spills
    /// must walk the extents B-tree, as [`crate::check::fork_blocks`] does.
    pub fn read_fork(&self, fork: &ForkData, limit: usize) -> Result<Vec<u8>> {
        ForkReader::new(self.device, fork, self.header.block_size).read_all(limit)
    }

    /// Whether the volume was cleanly unmounted.
    ///
    /// A dirty volume needs `fsck` before it should be written to.
    pub fn is_clean(&self) -> bool {
        self.header.is_clean()
    }

    /// Whether a journal is present.
    pub fn is_journaled(&self) -> bool {
        self.header.is_journaled()
    }

    /// Whether names on this volume are case-sensitive.
    pub fn is_case_sensitive(&self) -> bool {
        self.catalog.is_case_sensitive()
    }

    /// The CNID of the root folder.
    pub fn root_cnid(&self) -> Cnid {
        ROOT_FOLDER_ID
    }

    /// The volume's name, which is the root folder's catalog name.
    ///
    /// The root folder's thread record is keyed by the root's **own CNID** with an
    /// empty name, and its body repeats the name, so one lookup finds it.
    ///
    /// Mining reference: `hfs_MountHFSPlusVolume` reads the name with
    /// `cat_idlookup(kHFSRootFolderID)` and copies `cd_nameptr` into `vcbVN`.
    pub fn name(&self) -> Result<String> {
        let Some(CatalogRecord::Thread(thread)) = self.catalog.lookup(self.root_cnid(), &[])?
        else {
            return Err(Error::NotFound {
                what: "root folder thread record",
            });
        };
        if thread.node_name.is_empty() {
            return Err(Error::NotFound {
                what: "root folder name",
            });
        }
        Ok(String::from_utf16_lossy(&thread.node_name))
    }

    /// Look up `name` inside `parent`.
    pub fn lookup(&self, parent: Cnid, name: &[u16]) -> Result<Option<Object>> {
        // The name is taken from the catalog key the search landed on, not from
        // the request. On a case-folding volume several spellings reach the same
        // record, and reporting the caller's spelling would name a file that does
        // not exist: `Volume::name` is derived from this, and so is anything a
        // FUSE mount returns to a process that listed the directory by another
        // spelling.
        let Some((stored, record)) = self.catalog.lookup_named(parent, name)? else {
            return Ok(None);
        };
        // Thread records describe a directory entry from the outside; a
        // directory listing must surface the object itself.
        if record.is_thread() {
            return Ok(None);
        }
        Ok(Object::from_record(
            stored,
            record,
            self.header.has_expanded_times(),
        ))
    }

    /// List a directory's entries.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_getdirentries`.
    pub fn read_dir(&self, parent: Cnid) -> Result<Vec<Object>> {
        let names = self.catalog.read_dir(parent)?;
        let mut out = Vec::with_capacity(names.len());
        for entry in names {
            // One lookup per entry to fetch its metadata. This is the shape Apple
            // uses too: the catalog key gives the name cheaply, and the record
            // gives the attributes. Caching both together is a later optimisation
            // and must not change what readdir returns.
            let Some(record) = self.catalog.lookup(parent, &entry.name)? else {
                continue;
            };
            if record.is_thread() {
                continue;
            }
            if let Some(object) =
                Object::from_record(entry.name, record, self.header.has_expanded_times())
            {
                out.push(object);
            }
        }
        Ok(out)
    }

    /// List a directory's entries with metadata inlined, resuming from `cursor`.
    ///
    /// Like [`Volume::read_dir`] but fetches each child's record in the same
    /// B-tree pass, avoiding the per-entry second lookup. This is the entry
    /// point for FUSE READDIRPLUS: the returned [`Object`] already carries
    /// name, CNID, permissions, timestamps, and sizes, so the adapter has
    /// everything it needs for `fuse_reply_entry` without another catalog
    /// descent.
    ///
    /// At most `limit` entries are returned. If the directory is not exhausted,
    /// the returned [`DirCursor`] advances the position so the next call resumes
    /// exactly where this one left off. When the directory is exhausted the
    /// cursor does not advance (it stays at the end), so a caller can detect
    /// completion by comparing cursors.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_getdirentries` does a single
    /// forward walk of the leaf chain; the per-entry `catalog.lookup` in
    /// [`Volume::read_dir`] is a separate descent that this method removes.
    pub fn read_dir_plus(
        &self,
        parent: Cnid,
        cursor: DirCursor,
        limit: usize,
    ) -> Result<(Vec<Object>, DirCursor)> {
        let (entries, next_cursor) = self.catalog.read_dir_records(parent, cursor, limit)?;
        let volume_expanded = self.header.has_expanded_times();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(object) = Object::from_record(entry.name, entry.record, volume_expanded) {
                out.push(object);
            }
        }
        Ok((out, next_cursor))
    }

    /// Resolve a CNID to its object, wherever it is in the tree.
    ///
    /// This is one B-tree descent rather than a scan, because an object's thread
    /// record is keyed by the object's **own** CNID with an empty name. Reading it
    /// gives the parent and the name, and one more descent fetches the record
    /// itself.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_idlookup` answers the same
    /// question with `cat_findposition`, which scans forward from a hint rather
    /// than descending. The thread record makes a descent possible here, and it
    /// is why every object having a thread record matters.
    pub fn lookup_cnid(&self, cnid: Cnid) -> Result<Option<Object>> {
        let Some(CatalogRecord::Thread(thread)) = self.catalog.lookup(cnid, &[])? else {
            return Ok(None);
        };
        let Some(record) = self.catalog.lookup(thread.parent_id, &thread.node_name)? else {
            return Ok(None);
        };
        if record.is_thread() {
            return Ok(None);
        }
        Ok(Object::from_record(
            thread.node_name,
            record,
            self.header.has_expanded_times(),
        ))
    }

    /// Read `len` bytes from a file's data fork at `offset`.
    pub fn read(&self, file: &Object, offset: u64, len: usize) -> Result<Vec<u8>> {
        let f = file.as_file()?;
        self.fork_reader(&f.record.data_fork, ExtentKey::DATA_FORK, file_id(f))
            .read(offset, len)
    }

    /// Read the whole data fork, bounded by `limit` bytes.
    pub fn read_file(&self, file: &Object, limit: usize) -> Result<Vec<u8>> {
        let f = file.as_file()?;
        self.fork_reader(&f.record.data_fork, ExtentKey::DATA_FORK, file_id(f))
            .read_all(limit)
    }

    /// SEEK_DATA: the byte offset of the next allocated region at or after
    /// `offset` in the file's data fork.
    ///
    /// Returns `None` when no extent covers or follows the offset. This is a
    /// wrapper around [`ForkReader::seek_data`] that resolves the file's fork
    /// (including overflow extents) before delegating.
    pub fn seek_data(&self, file: &Object, offset: u64) -> Result<Option<u64>> {
        let f = file.as_file()?;
        self.fork_reader(&f.record.data_fork, ExtentKey::DATA_FORK, file_id(f))
            .seek_data(offset)
    }

    /// SEEK_HOLE: the byte offset of the next hole at or after `offset` in the
    /// file's data fork.
    ///
    /// Returns `None` when the entire fork is allocated past the offset. This is
    /// a wrapper around [`ForkReader::seek_hole`] that resolves the file's fork
    /// before delegating.
    pub fn seek_hole(&self, file: &Object, offset: u64) -> Result<Option<u64>> {
        let f = file.as_file()?;
        self.fork_reader(&f.record.data_fork, ExtentKey::DATA_FORK, file_id(f))
            .seek_hole(offset)
    }

    /// BMAP: translate a file byte offset to a device byte offset.
    ///
    /// The offset must be aligned to the volume's allocation block size, as
    /// FUSE's BMAP opcode requires. Returns
    /// [`Error::OutOfRange`](crate::error::Error::OutOfRange) when the block is
    /// past the file's allocated extents (a sparse hole).
    ///
    /// Mining reference: Apple `core/FileExtentMapping.c` `MapFileBlockC`
    /// performs this same fork-block to device-offset translation for the
    /// kernel's BMAP path.
    pub fn bmap(&self, file: &Object, offset: u64) -> Result<u64> {
        let f = file.as_file()?;
        self.fork_reader(&f.record.data_fork, ExtentKey::DATA_FORK, file_id(f))
            .bmap(offset)
    }

    /// Read `len` bytes from a file's resource fork.
    ///
    /// A file with no resource fork reads empty rather than failing, because the
    /// absence of a fork is normal and not an error condition.
    pub fn read_resource(&self, file: &Object, offset: u64, len: usize) -> Result<Vec<u8>> {
        let f = file.as_file()?;
        if f.record.resource_fork.logical_size == 0 {
            return Ok(Vec::new());
        }
        self.fork_reader(
            &f.record.resource_fork,
            ExtentKey::RESOURCE_FORK,
            file_id(f),
        )
        .read(offset, len)
    }

    /// The attributes B-tree for this volume, opened lazily.
    ///
    /// Returns `None` when the volume has no attributes fork (empty or absent),
    /// which is the normal case on volumes whose files have no extended
    /// attributes.
    fn attributes(&self) -> Result<Option<&AttributesFile<'_, D>>> {
        if self.header.attributes_file.logical_size == 0 {
            return Ok(None);
        }
        if self.attributes.get().is_none() {
            let attrs = AttributesFile::open(
                self.device,
                &self.header.attributes_file,
                self.header.block_size,
                true,
            )?;
            self.attributes
                .set(attrs)
                .map_err(|_| Error::invalid("attributes", "already initialised"))?;
        }
        // `set` cannot fail after the get-check above; unwrap is safe because
        // the cell was empty and we just filled it.
        Ok(self.attributes.get())
    }

    /// Read the value of an extended attribute named `name` from `file`.
    ///
    /// Returns `Ok(None)` when the file has no such attribute, so a caller can
    /// distinguish "absent" from "error". A file with no attributes tree at all
    /// returns `Ok(None)` for every name without touching the device.
    ///
    /// Mining reference: `core/hfs_xattr.c` `hfs_vnop_getattrlist` resolves a
    /// named attribute through the attributes B-tree for one `fileID`.
    pub fn getxattr(&self, file: &Object, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(attrs) = self.attributes()? else {
            return Ok(None);
        };
        if attrs.is_empty() {
            return Ok(None);
        }
        let cnid = file.cnid().0;
        let list = attrs.attributes_for(cnid)?;
        for attr in &list {
            if attr.name == name {
                return Ok(Some(attr.value.clone()));
            }
        }
        Ok(None)
    }

    /// List the names of all extended attributes on `file`.
    ///
    /// Returns an empty vector when the file has no attributes. The names are
    /// the raw on-disk strings, not prefixed with anything.
    pub fn listxattr(&self, file: &Object) -> Result<Vec<String>> {
        let Some(attrs) = self.attributes()? else {
            return Ok(Vec::new());
        };
        if attrs.is_empty() {
            return Ok(Vec::new());
        }
        let cnid = file.cnid().0;
        let list = attrs.attributes_for(cnid)?;
        Ok(list.into_iter().map(|a| a.name).collect())
    }

    /// The target of a symbolic link.
    ///
    /// Mining reference: HFS stores a symlink as a file whose data fork holds
    /// the target path. `core/hfs_vfsops.c` reads it there; there is no separate
    /// on-disk structure.
    pub fn read_link(&self, file: &Object) -> Result<String> {
        let f = file.as_file()?;
        if !f.record.is_symlink() {
            return Err(Error::invalid("readlink", "not a symbolic link"));
        }
        // A target is bounded by PATH_MAX; a longer one means a corrupt fork, and
        // allocating for it would be the wrong response to a bad on-disk length.
        const MAX_TARGET: usize = 4096;

        // A symlink's target *is* its data fork, so an empty one names nothing.
        // Returning "" would be a path, and a caller would try to resolve it --
        // against the process's working directory, in the worst case. That is the
        // kind of wrong answer a refusal is for.
        //
        // Mining reference: `core/hfs_xattr.c` reads a link target out of the
        // file's data fork for HFSPlus, so the fork and the target cannot
        // disagree; an empty fork is a corrupt symlink rather than a link to "".
        if f.record.data_fork.logical_size == 0 {
            return Err(Error::invalid(
                "readlink",
                "the symlink has an empty data fork, so it has no target",
            ));
        }

        let bytes = self
            .fork_reader(&f.record.data_fork, ExtentKey::DATA_FORK, file_id(f))
            .read_all(MAX_TARGET)?;
        // `write_journal_header` shows the target is stored without a terminator,
        // but a writer that appends one is not a fault, so trailing NULs are
        // trimmed rather than refused.
        let s = String::from_utf8_lossy(&bytes);
        Ok(s.trim_end_matches('\0').to_string())
    }

    /// Statistics for `statfs`.
    ///
    /// Mining reference: `core/hfs_statfs.c` `hfs_bstatfs` reports the volume
    /// header's block counts divided by the block size, and the header's own
    /// file and folder counts.
    pub fn statfs(&self) -> Result<StatFs> {
        let bs = self.header.block_size;
        Ok(StatFs {
            block_size: bs,
            total_blocks: self.header.total_blocks,
            free_blocks: self.header.free_blocks,
            total_bytes: self.header.volume_bytes()?,
            free_bytes: u64::from(self.header.free_blocks)
                .checked_mul(u64::from(bs))
                .ok_or(Error::overflow("statfs free bytes"))?,
            file_count: self.header.file_count,
            folder_count: self.header.folder_count,
            // An HFS+ volume has no sub-directory limit to report.
            max_name_len: 255,
            journaled: self.header.is_journaled(),
        })
    }

    /// The volume's journal, if it has one.
    ///
    /// The returned [`Journal`] can hand out an overlaid device whose reads
    /// consult the replayed blocks. Neither this nor the overlay writes to the
    /// image: the volume type exposes no write path at all.
    ///
    /// Returns `Ok(None)` for a volume with no journal. The attribute bit is
    /// checked *before* `journalInfoBlock` is used, because on a non-journaled
    /// volume that field overlaps spare space and holds whatever was left there.
    /// Mining reference: `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) only
    /// opens a journal when `kHFSVolumeJournaledBit` is set.
    pub fn journal(&self) -> Result<Option<crate::journal::Journal<'a, D>>> {
        if !self.header.is_journaled() {
            return Ok(None);
        }

        // `journalInfoBlock` is bounded by `VolumeHeader::validate`, so it names a
        // block of this volume by the time we get here.
        crate::journal::Journal::open(
            self.device,
            self.header.journal_info_block,
            self.header.block_size,
        )
    }

    /// The volume's journal info block, when the journal lives on another device.
    ///
    /// [`Volume::journal`] returns `None` for such a volume, because there is no
    /// journal in the image to replay. That is the right answer for a reader and
    /// the wrong answer for a diagnostic: "no journal" and "the journal is
    /// somewhere else" are different states, and a caller asked to report on the
    /// volume needs to tell them apart.
    ///
    /// Mining reference: `core/hfs_vfsutils.c` `hfs_mount_hfsplus` branches on
    /// `kJIJournalInFSMask` and, on the other path, calls `open_journal_dev` with
    /// `ext_jnl_uuid` and `machine_serial_num`. Locating that device is a
    /// mount-time policy decision, so this only reports what the block says.
    pub fn external_journal(&self) -> Result<Option<crate::journal::info::JournalInfoBlock>> {
        if !self.header.is_journaled() || self.header.journal_info_block == 0 {
            return Ok(None);
        }
        let bs = u64::from(self.header.block_size);
        let len = usize::try_from(bs).map_err(|_| Error::overflow("journal info block size"))?;
        let mut buf = vec![0u8; len];
        let at = u64::from(self.header.journal_info_block) * bs;
        self.device.read_at(at, &mut buf)?;
        let info = crate::journal::info::JournalInfoBlock::parse(&buf)?;
        Ok(if info.flag_set().in_filesystem() {
            None
        } else {
            Some(info)
        })
    }

    /// The volume's allocation bitmap.
    pub fn allocation_bitmap(&self) -> Result<AllocationBitmap<'a, D>> {
        AllocationBitmap::open(
            self.device,
            &self.header.allocation_file,
            self.header.block_size,
            self.header.total_blocks,
        )
    }

    /// A reader for `fork`, with overflow extents resolved when it needs them.
    ///
    /// A fork holds at most `kHFSPlusExtentDensity` (8) extents inline. Anything
    /// past that keeps the rest in the **extents overflow B-tree**, keyed on the
    /// fork's CNID and the index of the first extent it holds. Without a
    /// resolver a fork that overflows would silently read only its first eight
    /// extents: every offset past that boundary would look like a hole, so the
    /// read would return zeros instead of the file.
    ///
    /// Mining reference: Apple `core/hfs_extents.c` `extoffset` walks
    /// `fabs->extents` and, once past the inline density, looks the remaining
    /// extents up in the extents B-tree by CNID. `core/hfs_vfsops.c` reads
    /// through that mapping, so a file that overflows is a normal file, not a
    /// special case.
    fn fork_reader(&self, fork: &ForkData, fork_type: u8, file_id: u32) -> ForkReader<'_, D> {
        if !fork.needs_overflow() {
            return ForkReader::new(self.device, fork, self.header.block_size);
        }
        // A fork whose overflow tree is absent or unreadable must still read its
        // inline extents rather than failing outright, because the first eight
        // are on hand.
        match self.overflow_resolver(fork_type, file_id) {
            Some(resolver) => {
                ForkReader::with_overflow(self.device, fork, self.header.block_size, resolver)
            }
            None => ForkReader::new(self.device, fork, self.header.block_size),
        }
    }

    /// A resolver over the extents overflow B-tree for one fork.
    ///
    /// Returns `None` when the volume has no extents file, or it cannot be read.
    /// That is not an error at this level: the fork's own inline extents are
    /// still readable, and a volume with no overflow file cannot have a fork
    /// that overflows.
    fn overflow_resolver(
        &self,
        fork_type: u8,
        file_id: u32,
    ) -> Option<Box<dyn OverflowResolver + '_>> {
        let fork = &self.header.extents_file;
        if fork.logical_size == 0 {
            return None;
        }
        // `OnceCell::get_or_try_init` is unstable, so try the existing value
        // first and only build on a miss. The double `get` is not a race in the
        // intended sense: two threads may each open the tree, and both results
        // are equivalent, so the second `set` is simply ignored.
        if self.extents.get().is_none() {
            if let Ok(tree) =
                crate::btree::io::BTreeFile::open(self.device, fork, self.header.block_size, true)
            {
                let _ = self.extents.set(TreeOverflow::new(tree));
            }
        }
        let tree = self.extents.get()?;
        Some(Box::new(ForkOverflow::for_fork(tree, fork_type, file_id)))
    }
}

/// A volume opened for mutation.
///
/// This type exists to make the trust boundary explicit rather than to expose
/// mutation. Opening one performs exactly the same work as [`Volume::open`] --
/// header validation, then journal detection, then journal replay -- and then
/// stops. Anything that changes the volume has to be asked for by name. On a
/// journaled volume, each such mutation is wrapped in a journal transaction:
/// before-images are recorded before the home blocks are modified, and committed
/// to the journal ring at the end of the operation, so that a crash mid-write
/// is recoverable by replay.
///
/// # Why a separate type at all
///
/// Because "this volume is safe to change" is a different claim from "this
/// volume can be read", and after Milestone 5 the two are not the same claim
/// either: a journaled volume that has *not* been replayed serves a stale
/// filesystem. Reading it is a legitimate choice with a visible cost.
/// Changing it is not, because a write lands on a filesystem the writer never
/// saw.
///
/// So the difference is not read versus write access to the bytes -- that is
/// [`crate::blockdev::BlockDeviceMut`], and it is a different axis. This type
/// is about having *established* that the on-disk state is current, which
/// reading does not require and writing does.
///
/// # What opening one guarantees
///
/// Everything [`Volume::open`] establishes, and nothing is skipped for being
/// asked in write mode. In particular a journal that cannot be replayed is
/// refused here exactly as it is there: a volume whose journal is damaged has
/// writes the journal holds that have not been applied, and offering to
/// modify it would be offering to work from a filesystem that does not exist.
///
/// Mining reference: `core/hfs_vfsops.c` `hfs_mount_existing` refuses a NULL
/// journal from `journal_open` with `EINVAL` rather than mounting, and the
/// mount path runs every structural check before any write is possible.
///
/// # Why it owns the device rather than borrowing a `Volume`
///
/// A writer needs `&mut D`, and a [`Volume`] holds `&D`. Both at once is not
/// borrowable, so this does not hold one. Instead it runs the *same* validation
/// by opening a temporary `Volume` over the shared reborrow, keeps the two facts
/// worth keeping -- the header and whether a journal was replayed -- and drops
/// it before taking the mutable borrow. One validation path, not two that could
/// drift: `from_validated` was the alternative, and it is what made this look
/// impossible rather than merely awkward.
///
/// # It is an exclusive handle
///
/// There is deliberately no `volume()` accessor returning a `Volume`. Every
/// cached view of these bytes is invalidated by the first successful mutation,
/// so a view handed out from here would be a way to read stale data while
/// holding a writer. To read, drop this and open a new [`Volume`]. That is a
/// small cost and it makes the invalidation impossible to forget.
pub struct WritableVolume<'d, D: ?Sized> {
    /// Exclusive access to the bytes, for as long as this exists.
    device: &'d mut D,
    /// The validated header, kept so a mutation does not re-read it and so the
    /// facts validation established travel with the handle.
    header: VolumeHeader,
    /// The filesystem kind, decided once during validation.
    kind: FileSystemKind,
    /// Whether a journal was replayed while validating.
    journal_replayed: bool,
    /// The journal state, if the volume is journaled. Holds the info block that
    /// locates the journal on the device and the mutable header whose `end` and
    /// `sequence_num` advance on every commit.
    journal: Option<JournalState>,
}

/// Journal state for a writable volume.
///
/// The info block locates the journal on the device; the header tracks the
/// ring's cursors and sequence number. An active transaction buffer, if present,
/// accumulates before-images for the in-flight commit. The header is `None` when
/// the journal has never been written to (`kJIJournalNeedInitMask`): it is
/// initialised lazily on the first commit.
struct JournalState {
    info: JournalInfoBlock,
    header: Option<JournalHeader>,
    tx: Option<TransactionBuffer>,
}

impl<D: ?Sized> std::fmt::Debug for WritableVolume<'_, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the device. The interesting question about this
        // type is what it established, and printing the device would bury that.
        f.debug_struct("WritableVolume")
            .field("filesystem", &self.kind)
            .field("journal_was_replayed", &self.journal_replayed)
            .field("journaled", &self.journal.is_some())
            .finish_non_exhaustive()
    }
}

impl<'d, D: BlockDeviceMut + ?Sized> WritableVolume<'d, D> {
    /// Validate `device` for mutation and take it.
    ///
    /// `BlockDeviceMut` rather than `BlockDevice`, because opening for mutation
    /// is a claim about intent and a read-only device cannot satisfy it -- better
    /// at compile time than at the first write.
    pub fn open(device: &'d mut D) -> Result<Self> {
        // The shared reborrow ends here, which is what makes the mutable borrow
        // below legal. Explicit rather than relying on the drop order.
        let volume = Volume::open(&*device)?;
        let journal_replayed = volume.journal()?.is_some();
        let header = volume.header;
        let kind = volume.kind;
        let journal_info_block = header.journal_info_block;
        let block_size = header.block_size;
        drop(volume);

        // Read the journal info block and header, if this is a journaled volume.
        // The info block lives at journal_info_block * block_size, one allocation
        // block. On a non-journaled volume journal_info_block is zero and is
        // ignored.
        let journal = if journal_replayed && journal_info_block != 0 {
            let bs = u64::from(block_size);
            let info_at = u64::from(journal_info_block)
                .checked_mul(bs)
                .ok_or(Error::overflow("journal info block offset"))?;
            let mut info_buf =
                vec![0u8; usize::try_from(bs).map_err(|_| Error::overflow("block size"))?];
            device.read_at(info_at, &mut info_buf)?;
            let info = JournalInfoBlock::parse(&info_buf)?;

            // Read the journal header at info.offset. For a journal that has
            // never been written to (kJIJournalNeedInitMask), the header area is
            // all-zeroes and `JournalHeader::parse` returns `None`; that is
            // treated as "needs initialisation" and the header is created lazily
            // on the first commit.
            let mut hdr_buf =
                vec![0u8; usize::try_from(info.size).map_err(|_| Error::overflow("journal size"))?];
            let want = hdr_buf.len().min(4096);
            device.read_at(info.offset, &mut hdr_buf[..want])?;
            let header = JournalHeader::parse(&hdr_buf[..want])?;

            Some(JournalState {
                info,
                header,
                tx: None,
            })
        } else {
            None
        };

        Ok(WritableVolume {
            device,
            header,
            kind,
            journal_replayed,
            journal,
        })
    }

    /// The validated header.
    pub fn header(&self) -> &VolumeHeader {
        &self.header
    }

    /// The filesystem kind, decided during validation.
    pub fn kind(&self) -> FileSystemKind {
        self.kind
    }

    /// Whether a journal was replayed during validation.
    ///
    /// False for a volume with no journal at all, and false for one whose
    /// journal lives on another device. A writer needs to know which, and
    /// needs to be told rather than infer it.
    ///
    /// On a journaled volume whose journal had transactions, this is `true`
    /// and the writer journals every mutation. On a new journal that has never
    /// been written to (`kJIJournalNeedInitMask` set), this is `false` and the
    /// first mutation initializes the journal header.
    pub fn journal_was_replayed(&self) -> bool {
        self.journal_replayed
    }

    /// Whether this volume is journaled.
    ///
    /// On a journaled volume, every mutating operation is wrapped in a journal
    /// transaction before the bytes are written to their home blocks. This is
    /// the writable counterpart of [`Volume::journal`].
    pub fn is_journaled(&self) -> bool {
        self.journal.is_some()
    }

    /// Record a block's before-image in the current transaction, then write.
    ///
    /// On a journalled volume this is the only path to disk: the block's current
    /// contents are copied into the transaction buffer before the new bytes go
    /// out, so a crash before commit leaves the journal holding a before-image
    /// that replay can apply or that the home write can make durable.
    ///
    /// On a non-journaled volume this is a plain write.
    fn journal_write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if let Some(jnl) = &mut self.journal {
            let tx = jnl.tx.get_or_insert_with(TransactionBuffer::new);
            // The journal addresses blocks in jhdr_size units, which on every
            // volume Apple writes today is the volume block size.
            let blk = u64::from(self.header.block_size);
            let block_start = offset / blk * blk;
            let block_num = block_start / blk;
            // Only record the before-image the first time a block is touched in
            // this transaction; later writes to the same block are covered by the
            // initial record.
            if !tx.writes().iter().any(|w| w.bnum == block_num) {
                let mut before =
                    vec![0u8; usize::try_from(blk).map_err(|_| Error::overflow("block size"))?];
                self.device.read_at(block_start, &mut before)?;
                tx.record_write(block_num, before);
            }
        }
        self.device.write_at(offset, data)
    }

    /// Begin a journal transaction.
    ///
    /// On a non-journaled volume this is a no-op: writes go straight to disk.
    /// On a journaled volume it starts an empty [`TransactionBuffer`] that
    /// [`journal_write`](Self::journal_write) fills with before-images, closed
    /// by [`end_transaction`](Self::end_transaction).
    fn begin_transaction(&mut self) -> Result<()> {
        if let Some(jnl) = &mut self.journal {
            // A transaction already open from the same caller is a logic error.
            if jnl.tx.is_some() {
                return Err(Error::invalid("journal", "a transaction is already open"));
            }
            jnl.tx = Some(TransactionBuffer::new());
        }
        Ok(())
    }

    /// Begin a journal transaction, returning whether this call started one.
    ///
    /// Like [`begin_transaction`](Self::begin_transaction) but reuses an already
    /// open transaction instead of erroring, so a method that may run inside an
    /// outer transaction can still guarantee its writes are journaled when called
    /// directly. The caller must pass the return value to
    /// [`end_transaction`](Self::end_transaction) or
    /// [`abandon_transaction`](Self::abandon_transaction) only when it is `true`.
    fn maybe_begin_transaction(&mut self) -> Result<bool> {
        let jnl = match &mut self.journal {
            Some(j) => j,
            None => return Ok(false),
        };
        if jnl.tx.is_some() {
            return Ok(false);
        }
        jnl.tx = Some(TransactionBuffer::new());
        Ok(true)
    }

    /// Commit the current journal transaction, if one is open.
    ///
    /// On a non-journaled volume this is a no-op -- there is nothing to commit
    /// because the writes were direct. On a journaled volume the accumulated
    /// before-images are flushed to the journal ring and the header's `end`
    /// cursor and `sequence_num` advance, making the transaction durable and
    /// replayable.
    fn end_transaction(&mut self) -> Result<()> {
        let Some(mut jnl) = self.journal.take() else {
            return Ok(());
        };
        let Some(tx) = jnl.tx.take() else {
            self.journal = Some(jnl);
            return Ok(());
        };
        let result = if !tx.is_empty() {
            // Lazily initialise an uninitialized journal (one whose info block has
            // kJIJournalNeedInitMask set and whose header is zeroed). Apple's writer
            // does the same: it writes the header before the first transaction and
            // clears the flag so a reader does not mistake it for an empty journal.
            if jnl.header.is_none() {
                let info = jnl.info;
                let header = self.make_journal_header(&info)?;
                crate::journal::replay::write_journal_header(self.device, &info, &header)?;
                jnl.header = Some(header);
                // Clear kJIJournalNeedInitMask in the info block's flags.
                let flags = info.flags & !crate::journal::info::K_JI_JOURNAL_NEED_INIT_MASK;
                jnl.info.flags = flags;
                let bs = u64::from(self.header.block_size);
                let info_at = u64::from(self.header.journal_info_block)
                    .checked_mul(bs)
                    .ok_or(Error::overflow("journal info block offset"))?;
                // self.journal is temporarily None, so this writes directly.
                self.journal_write(info_at, &flags.to_be_bytes())?;
            }
            let header = jnl.header.as_mut().expect("header was just initialised");
            let blhdr_size = header.blhdr_size;
            commit_transaction(
                self.device,
                &jnl.info,
                header,
                &tx,
                blhdr_size,
                /* check_blocks */ true,
                /* pending */ 0,
            )
        } else {
            Ok(())
        };
        self.journal = Some(jnl);
        result
    }

    /// Build a fresh journal header for an uninitialized journal.
    ///
    /// Mining reference: `core/hfs_journal.c` `journal_init` sets
    /// `jhdr_start = jhdr_end = jhdr_size`, `size = jnl_size`, and clears
    /// `kJIJournalNeedInitMask` before the first use.
    fn make_journal_header(&self, info: &JournalInfoBlock) -> Result<JournalHeader> {
        use crate::journal::info::{ByteOrder, ENDIAN_MAGIC, JOURNAL_HEADER_MAGIC};
        let jhdr_size = u64::from(self.header.block_size);
        let blhdr_size = jhdr_size;
        Ok(JournalHeader {
            magic: JOURNAL_HEADER_MAGIC,
            endian: ENDIAN_MAGIC,
            start: jhdr_size,
            end: jhdr_size,
            size: info.size,
            blhdr_size: u32::try_from(blhdr_size).map_err(|_| Error::overflow("blhdr_size"))?,
            checksum: 0,
            jhdr_size: u32::try_from(jhdr_size).map_err(|_| Error::overflow("jhdr_size"))?,
            sequence_num: 0,
            byte_order: ByteOrder::Big,
        })
    }

    /// Abandon the current transaction without committing.
    ///
    /// Drops the transaction buffer so the journal header is not advanced and
    /// the half-applied writes are invisible to a future replay.
    fn abandon_transaction(&mut self) {
        if let Some(jnl) = &mut self.journal {
            jnl.tx.take();
        }
    }

    /// Replace a file's contents, without changing its allocation.
    ///
    /// The first mutation, and deliberately the narrowest one that is still real:
    /// the new bytes must fit the blocks the fork already owns, so no allocation,
    /// no extent change and no B-tree split is involved. That is the point -- it
    /// exercises serialisation, the leaf write and the timestamps on their own,
    /// so a failure says which of them is wrong.
    ///
    /// Takes a CNID rather than a resolved object, so the caller reads the
    /// catalog with a read-only [`Volume`] first and drops it before opening
    /// this. Holding both at once is not borrowable, and the CNID is the whole of
    /// what this needs.
    ///
    /// The data blocks and the catalog's leaf node are written, and the record's
    /// `logicalSize` and timestamps are updated. Nothing else: no bitmap, no
    /// volume header.
    ///
    /// # What it refuses, and why
    ///
    /// - **Growth.** A file that outgrows its blocks needs an allocator, and an
    ///   allocator that guessed at a free block would be worse than no mutation.
    ///   The error names the shortfall rather than truncating the write.
    /// - **A length change in the record.** See [`Self::replace_catalog_record`].
    ///
    /// Mining reference: `core/hfs_cnode.c` `hfs_update` sets `c_touch_modtime`
    /// and `c_touch_chgtime` on a content change, which become `contentModDate`
    /// and `attributeModDate`. `accessDate` is deliberately not touched: Apple
    /// defers an atime-only update to vnode recycle rather than writing it
    /// immediately, and this library has no recycle point to defer to. Leaving it
    /// alone is also the only choice that does not invent semantics -- an
    /// atime a writer had to guess at would be worse than a stale one.
    pub fn write_file_contents(&mut self, cnid: u32, data: &[u8]) -> Result<()> {
        self.begin_transaction()?;
        let result = self.write_file_contents_inner(cnid, data);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            // Abandon the partial transaction: drop the buffer without committing,
            // so the journal header is not advanced and the uncommitted writes are
            // invisible to replay.
            self.abandon_transaction();
        }
        result
    }

    fn write_file_contents_inner(&mut self, cnid: u32, data: &[u8]) -> Result<()> {
        let block_size = self.header.block_size;
        let expanded = self.header.has_expanded_times();

        // The fork's blocks, as device block numbers, in logical order. No overflow
        // here: a fork that overflowed needs the extents tree, which is allocation
        // work rather than a serialisation change.
        let mut record = self.read_file_record(cnid)?;

        let owned = record.data_fork.total_blocks as usize;
        let capacity = owned * block_size as usize;
        if data.len() > capacity {
            // Round up to whole blocks, the way `AddExtents` computes
            // `blocksToAdd` with `howmany`. A request of one byte over a block
            // boundary needs a whole extra block, and rounding down would leave
            // the file with fewer blocks than its logical size requires.
            // `howmany(n, d)` rather than `div_ceil`, which is 1.73 and this crate
            // is 1.70.
            let needed = (data.len() + block_size as usize - 1) / block_size as usize;
            let extra = needed - owned;
            self.grow_fork(&mut record, extra)?;
        }

        // The fork's blocks, as device block numbers, in logical order. Built after
        // any growth, so a newly allocated extent is written like any other.
        let blocks: Vec<u32> = record
            .data_fork
            .extents
            .iter()
            .flat_map(|e| (0..e.block_count).map(move |o| e.start_block + o))
            .collect();

        // The data blocks first: until the record says so, the old length is
        // still what a reader will ask for, so writing the blocks before the
        // record is the order that never exposes a torn file.
        for (i, block) in blocks.iter().enumerate() {
            let start = i * block_size as usize;
            let end = (start + block_size as usize).min(data.len());
            if end <= start {
                break;
            }
            let at = u64::from(*block) * u64::from(block_size);
            self.journal_write(at, &data[start..end])?;
        }

        // Then the record: logicalSize, and the two timestamps a content change
        // touches. One clock read for both, so the record does not disagree with
        // itself across a second boundary.
        record.data_fork.logical_size = data.len() as u64;
        let now = crate::timestamp::now_hfs(expanded).map_err(|e| Error::Io {
            message: e.to_string(),
        })?;
        record.content_mod_date = now;
        record.attribute_mod_date = now;
        self.replace_catalog_record(cnid, &record)
    }

    /// Create an empty file called `name` inside folder `parent`, and return its CNID.
    ///
    /// The first mutation that adds something rather than changing something, and
    /// therefore the first that has to keep four structures in step: the catalog's
    /// two new records, the containing folder's child count, and the volume
    /// header's next-CNID counter. A file created in only three of those is a file
    /// that cannot be found by name, cannot be counted, or will be handed the same
    /// CNID twice.
    ///
    /// # Why two records and not one
    ///
    /// A file is a *file record*, keyed by `(parent folder, name)`, plus a *thread
    /// record*, keyed by `(its own CNID, no name)`. The thread record is what makes
    /// a lookup by CNID possible at all -- walking file records alone finds a name,
    /// never an object. `cat_create` builds both, and a file with no thread record
    /// is a file this crate's own checker reports as `missing_thread`.
    ///
    /// # The CNID
    ///
    /// Taken from the header's `nextCatalogID` and the header written back with
    /// the next value. Nothing else allocates CNIDs on HFS+, and reusing one would
    /// give two objects the same identity -- which for a filesystem means a hard
    /// link, silently, rather than an error.
    ///
    /// The header goes down *first*, before the catalog is touched, so a failure
    /// later in this method leaves a CNID consumed rather than a record claiming one
    /// the header would hand out again. The cost is a gap in the sequence on every
    /// refused create, which is harmless: HFS+ requires only that `nextCatalogID`
    /// exceed every CNID in use, never that they be contiguous.
    ///
    /// # What it refuses
    ///
    /// - **A name that already exists**, rather than creating a second record under
    ///   a key that is already there. A leaf with duplicate keys still parses and
    ///   still searches; it just answers with whichever comes first, forever.
    /// - **A folder that is not a folder.** CNID 1 is the volume's root by
    ///   convention, but nothing guarantees it, and writing a child count into a
    ///   file record would corrupt it.
    /// - **A leaf with no room**, named as the node-split it would need. A node
    ///   cannot grow: splitting one means redistributing records between two nodes
    ///   and updating the parent index, which is Milestone 8C's remaining half.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_create` and `catrec_update`,
    /// with `buildkey` and `buildthread` for the two records; `cat_create` calls
    /// `newcatalogid()` for the CNID and `incvalency()` for the parent's count.
    pub fn create_file(&mut self, parent: u32, name: &[u16]) -> Result<u32> {
        self.begin_transaction()?;
        let result = self.create_file_inner(parent, name);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            self.abandon_transaction();
        }
        result
    }

    fn create_file_inner(&mut self, parent: u32, name: &[u16]) -> Result<u32> {
        use crate::catalog::key::CatalogKey;
        use crate::catalog::record::{
            FileRecord, K_HFS_PLUS_FILE_THREAD_RECORD, S_IFREG, THREAD_RECORD_NAME_LEN_OFFSET,
        };

        if name.is_empty() {
            return Err(Error::invalid("create", "a file cannot have an empty name"));
        }
        let parent_cnid = Cnid(parent);
        let cnid = self.header.next_catalog_id;
        if cnid <= ROOT_FOLDER_ID.0 {
            return Err(Error::invalid(
                "volume_header.nextCatalogID",
                format!(
                    "{cnid} is at or below the reserved CNID range, so it cannot \
                     identify a new file"
                ),
            ));
        }

        // The containing folder, so its child count can be incremented. Read before
        // anything is written, so a bad CNID costs nothing.
        // Validated before anything is written, so a bad parent CNID costs nothing.
        self.read_folder_record(parent)?;
        let now =
            crate::timestamp::now_hfs(self.header.has_expanded_times()).map_err(|e| Error::Io {
                message: e.to_string(),
            })?;

        let file = FileRecord {
            record_type: crate::catalog::record::K_HFS_PLUS_FILE_RECORD,
            file_id: Cnid(cnid),
            create_date: now,
            content_mod_date: now,
            attribute_mod_date: now,
            access_date: now,
            backup_date: now,
            bsd_info: crate::catalog::record::BsdInfo {
                file_mode: S_IFREG | 0o644,
                // `special` is `hl_linkCount` on a record that is not a link, and
                // Apple sets it to 1 for a regular file. Zero is not "no links" so
                // much as "never counted".
                special: 1,
                ..FileRecord::EMPTY.bsd_info
            },
            // `kHFSThreadExistsMask`: "this bit indicates that the file has a thread
            // record. As all files in HFS Plus have thread records, this bit must be
            // set." (TN1150, Catalog File.) Without it a reader may assume there is
            // no thread record and refuse to build the reverse mapping, so a file
            // created without it is findable by name and not by CNID.
            flags: crate::catalog::record::K_HFS_THREAD_EXISTS_MASK,
            ..FileRecord::EMPTY
        };
        // The thread record, built by hand because its length follows its name and
        // `ThreadRecord::parse` is the only encoder it has. Layout: recordType 0..2,
        // reserved 2..4, parentID 4..8, name length 8..10, then the name.
        // Fixed prefix, the two-byte name length, then the name. `FIXED_SIZE` is
        // the prefix *before* the length, so the length has to be added too.
        let mut thread = vec![0u8; THREAD_RECORD_NAME_LEN_OFFSET + 2 + name.len() * 2];
        thread[0..2].copy_from_slice(&K_HFS_PLUS_FILE_THREAD_RECORD.to_be_bytes());
        thread[4..8].copy_from_slice(&parent_cnid.0.to_be_bytes());
        thread[THREAD_RECORD_NAME_LEN_OFFSET..THREAD_RECORD_NAME_LEN_OFFSET + 2]
            .copy_from_slice(&(name.len() as u16).to_be_bytes());
        for (i, unit) in name.iter().enumerate() {
            let at = THREAD_RECORD_NAME_LEN_OFFSET + 2 + i * 2;
            thread[at..at + 2].copy_from_slice(&unit.to_be_bytes());
        }

        // Order: the header first, so a CNID is never handed out twice even if the
        // catalog write below fails. The reverse would let a retry reuse it.
        self.write_header_u32(64, cnid + 1)?;

        let child_key = CatalogKey::for_child(parent_cnid, name);
        let thread_key = CatalogKey::for_child(Cnid(cnid), &[]);
        let mut child = child_key.to_record();
        child.extend_from_slice(&file.to_bytes());
        let mut thread_record = thread_key.to_record();
        thread_record.extend_from_slice(&thread);

        // The two records go in together or not at all.
        //
        // A file record with no thread record is the worst of the possible partial
        // states: the checker reports it as `missing_thread`, `fsck.hfsplus` rejects
        // the volume, and the file is unreachable by CNID because a thread record is
        // what a CNID resolves through. An orphan *thread* record is milder -- no
        // check calls it out -- but the containing folder's `valence` is counted
        // from thread records, so an orphan makes the declared child count disagree
        // with what is there.
        //
        // So this rolls back rather than reordering. Reordering the two inserts
        // would avoid the missing thread and introduce the valence disagreement
        // instead, which is a trade and not a fix.
        //
        // The CNID counter is *not* rolled back. It is written first, before
        // anything else, precisely so that a failure here cannot hand the same CNID
        // out twice; undoing it would reintroduce that. A gap in the CNID sequence
        // is harmless -- HFS+ requires only that `nextCatalogID` exceed every CNID
        // in use, never that they be contiguous.
        let undo = |w: &mut Self| {
            // Best effort: a rollback that itself fails leaves the volume as it is,
            // which is no worse than not having tried, and reporting the rollback
            // failure instead of the original error would hide why the create
            // failed.
            let _ = w.remove_catalog_record(&thread_key);
            let _ = w.remove_catalog_record(&child_key);
        };

        self.insert_catalog_record(&child)?;
        if let Err(e) = self.insert_catalog_record(&thread_record) {
            undo(self);
            return Err(e);
        }
        if let Err(e) = self.bump_folder_valence(parent, now) {
            undo(self);
            return Err(e);
        }
        // Both counters advance in memory as well as on disk. Writing
        // `self.header.file_count + 1` without incrementing it writes the *same*
        // value on every call, so a volume that gains three files reports one --
        // and fsck counts files independently, so it is fsck that catches it.
        let file_count = self.header.file_count + 1;
        self.header.file_count = file_count;
        self.header.next_catalog_id = cnid + 1;
        self.write_header_u32(32, file_count)?;
        Ok(cnid)
    }

    /// Delete the object called `name` in folder `parent`, and return its CNID.
    ///
    /// The inverse of [`Self::create_file`], and it is the first mutation here
    /// that *removes* something, which is why it is worth reading what it refuses:
    ///
    /// - **The root folder**, and anything at or below the reserved CNID range.
    /// - **A file with blocks.** `unlink` refuses a file that still has data; the
    ///   blocks would have to be released, and a file's allocation is what its
    ///   extents describe, so removing the record first would strand them.
    /// - **A folder with children.** Same reason, one level up: a folder's
    ///   `valence` is its child count, and a folder that claims children it does
    ///   not have is what `fsck.hfsplus` reports as "Invalid directory item
    ///   count".
    ///
    /// # Order, and rollback
    ///
    /// Apple's `cat_delete` removes the record and then the thread record, and if
    /// the *second* fails it marks the volume inconsistent rather than trying to
    /// recover. That is a kernel with a mount to invalidate; a library has neither,
    /// so this puts both records back instead.
    ///
    /// Both records' bytes are read before anything is written, so a rollback is an
    /// insert of bytes that are known-good rather than a reconstruction. The CNID
    /// counter is not involved -- a delete consumes nothing.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_delete`, including its preflight
    /// (`cd_cnid < kHFSFirstUserCatalogNodeID || cd_parentcnid == kHFSRootParentID`
    /// is `EINVAL`) and its "delete thread record, and on error mark the volume
    /// inconsistent". The valence and count adjustments live in the unlink/rmdir
    /// path above it, which is where they belong: `cat_delete` is the record layer.
    pub fn remove(&mut self, parent: u32, name: &[u16]) -> Result<u32> {
        self.begin_transaction()?;
        let result = self.remove_inner(parent, name);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            self.abandon_transaction();
        }
        result
    }

    fn remove_inner(&mut self, parent: u32, name: &[u16]) -> Result<u32> {
        let parent_cnid = Cnid(parent);
        let object = {
            use crate::catalog::lookup::Catalog;
            let catalog = Catalog::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            catalog.lookup(parent_cnid, name)?.ok_or(Error::NotFound {
                what: "catalog entry",
            })?
        };
        let (cnid, is_folder, blocked) = match &object {
            crate::catalog::record::CatalogRecord::File(f) => (
                f.file_id.0,
                false,
                f.data_fork.total_blocks > 0 || f.data_fork.logical_size > 0,
            ),
            crate::catalog::record::CatalogRecord::Folder(fo) => {
                (fo.folder_id.0, true, fo.valence > 0)
            }
            // A thread record cannot be named, so a lookup by name cannot return
            // one; this arm exists only so the match is total.
            crate::catalog::record::CatalogRecord::Thread(_) => {
                return Err(Error::invalid(
                    "remove",
                    "a thread record cannot be deleted by name",
                ))
            }
        };

        if cnid <= ROOT_FOLDER_ID.0 {
            return Err(Error::invalid(
                "remove",
                format!("CNID {cnid} is reserved, so it cannot identify a deletable object"),
            ));
        }
        // `cat_delete`'s preflight, verbatim in effect: a CNID below the first user
        // one is reserved, and that test is what refuses the root folder -- its CNID
        // is 2, and the reserved range ends at 15. A second clause refuses an entry
        // whose *parent* is the root's own parent, CNID 1, which only the root's own
        // catalog entry has.
        //
        // Note what is deliberately not here: a test on the parent's CNID. The root
        // folder's name is keyed by parentID 1, not by its own CNID, so a check like
        // "parent is 2 and the entry is a folder" refuses an ordinary directory
        // instead -- which is exactly what an earlier version of this did.
        if parent == ROOT_PARENT_ID.0 {
            return Err(Error::invalid(
                "remove",
                "an entry directly under parent 1 is the root folder's own entry",
            ));
        }
        if blocked {
            return Err(Error::invalid(
                "remove",
                if is_folder {
                    "the folder is not empty".to_string()
                } else {
                    "the file still has contents; truncating it first would release \
                     the blocks"
                        .to_string()
                },
            ));
        }

        // Read both records before touching either, so a rollback puts back exactly
        // what was there.
        let child_key = crate::catalog::key::CatalogKey::for_child(parent_cnid, name);
        let thread_key = crate::catalog::key::CatalogKey::for_child(Cnid(cnid), &[]);
        let child_bytes = self
            .catalog_record_bytes(&child_key)?
            .ok_or(Error::NotFound {
                what: "catalog record",
            })?;
        let thread_bytes = self
            .catalog_record_bytes(&thread_key)?
            .ok_or(Error::NotFound {
                what: "thread record",
            })?;

        self.remove_catalog_record(&child_key)?;
        if let Err(e) = self.remove_catalog_record(&thread_key) {
            let _ = self.insert_catalog_record(&child_bytes);
            return Err(e);
        }
        if let Err(e) = self.change_folder_valence(parent, -1) {
            let _ = self.insert_catalog_record(&thread_bytes);
            let _ = self.insert_catalog_record(&child_bytes);
            return Err(e);
        }

        // The header's counts. A folder's removal decrements `folderCount`; a
        // file's decrements `fileCount`.
        let (offset, current) = if is_folder {
            (36u64, self.header.folder_count)
        } else {
            (32u64, self.header.file_count)
        };
        let updated = current
            .checked_sub(1)
            .ok_or_else(|| Error::overflow("volume header count"))?;
        if let Err(e) = self.write_header_u32(offset, updated) {
            let _ = self.change_folder_valence(parent, 1);
            let _ = self.insert_catalog_record(&thread_bytes);
            let _ = self.insert_catalog_record(&child_bytes);
            return Err(e);
        }
        if is_folder {
            self.header.folder_count = updated;
        } else {
            self.header.file_count = updated;
        }
        Ok(cnid)
    }

    /// Change a folder's declared child count by `delta`.
    fn change_folder_valence(&mut self, cnid: u32, delta: i64) -> Result<()> {
        let mut record = self.read_folder_record(cnid)?;
        let valence = i64::from(record.valence)
            .checked_add(delta)
            .filter(|v| *v >= 0)
            .ok_or_else(|| Error::overflow("folder valence"))?;
        record.valence = u32::try_from(valence).map_err(|_| Error::overflow("folder valence"))?;
        record.content_mod_date = crate::timestamp::now_hfs(self.header.has_expanded_times())
            .map_err(|e| Error::Io {
                message: e.to_string(),
            })?;
        let body = record.to_bytes();
        self.replace_catalog_body(cnid, &body)
    }

    /// Whether `candidate` is `cursor` or one of the folders above it.
    ///
    /// Walks up from `cursor` through thread records, which are the only place a
    /// folder records its parent, and stops at the root.
    ///
    /// The walk is bounded by the depth rather than by a visited set: a corrupted
    /// cycle in the thread records would otherwise loop forever, and a depth bound
    /// turns that into "not an ancestor", which is the answer that leaves the
    /// caller's own check to catch it.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_rename` traverses the destination
    /// path "all the way back to the root making sure that source directory is not
    /// encountered", after refusing the obvious cases outright -- the root, the
    /// destination directory itself, and the destination's own parent.
    fn folder_is_ancestor(&self, candidate: u32, cursor: u32) -> Result<bool> {
        use crate::catalog::cnid::Cnid;
        use crate::catalog::lookup::Catalog;
        use crate::catalog::record::CatalogRecord;

        let catalog = Catalog::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let mut cursor = cursor;
        // A path cannot be longer than the number of objects in the catalog, and
        // one folder per level is the shape; 64 is generous for a real volume and
        // small enough to be a hard stop.
        let mut budget = 64u32;
        while cursor > ROOT_FOLDER_ID.0 && budget > 0 {
            budget -= 1;
            let Some(CatalogRecord::Thread(thread)) = catalog.lookup(Cnid(cursor), &[])? else {
                // No thread record, so no parent is recorded and the walk ends here.
                return Ok(false);
            };
            let parent = thread.parent_id.0;
            if parent == candidate {
                return Ok(true);
            }
            cursor = parent;
        }
        Ok(false)
    }

    /// Create a hard link named `name` in `parent` to the file `target`, and return
    /// the **link's** CNID.
    ///
    /// # What happens, which is not "add a second name"
    ///
    /// `hfs_makelink` does two catalog operations:
    ///
    /// 1. **`cat_rename`** the file's own record into the private folder as
    ///    `iNode<cnid>`. It keeps its CNID and its forks and becomes the **indirect
    ///    node**.
    /// 2. **`createindirectlink`** -- a link record where the name was, in the user's
    ///    folder, whose `hl_linkReference` is that indirect node.
    ///
    /// So the user's file does not keep its record: the name becomes a link and the
    /// data lives elsewhere under an `iNode` name. That is what the private folder
    /// is for.
    ///
    /// # The order, which is the whole implementation
    ///
    /// `cat_rename` is an explicit list and this follows it: find at the old
    /// location, **insert** at the new key, **remove** from the old key, **remove**
    /// the old thread record, **insert** the new thread record. Then
    /// `cat_createlink` for the link, which inserts **thread first** -- the reverse
    /// of `cat_create`, and for the same reason an orphan thread is inert while a
    /// record with no thread cannot be found.
    ///
    /// Every bug in the attempts at this was in the sequencing rather than the
    /// model, including one that removed the target's record twice and then
    /// reported it missing. So the list is written down here, next to the code.
    ///
    /// # The values, and which authority fixes each
    ///
    /// | Field | Value | Authority |
    /// | --- | --- | --- |
    /// | link's FinderInfo type/creator | `hlnk` / `hfs+` | TN1150 |
    /// | link's flags | chain + thread-exists | TN1150, `createindirectlink` |
    /// | link's `hl_linkReference` | **the inode's CNID** | `lib_fsck_hfs`: "same as inode ID for file hard links created post-Tiger" |
    /// | link's forks | **empty** | TN1150; copying them is "Overlapped extent allocation" |
    /// | link's mode | `0444` | `createindirectlink` |
    /// | inode's `linkCount` | 1 | `lib_fsck_hfs` compares it against the links found |
    /// | inode's name | `iNode<cnid>` | `MAKE_INODE_NAME`, which TN1150 also gives |
    /// | inode's `hl_firstLinkID` | the link's CNID | "Valid only if ... indirect nodes only" |
    ///
    /// **What makes a record a link is its FinderInfo**, not its location:
    /// `lib_fsck_hfs` decides `islink` for a file record from
    /// `fdType == kHardLinkFileType && fdCreator == kHFSPlusCreator` alone -- no
    /// flag test and no check on which folder the record is in. That is worth
    /// knowing, because it means the chain flag is not what makes something a link;
    /// the checker *adds* it during repair ("we upgrade all pre-Leopard file hard
    /// links to Leopard hard links on any file hard link repairs") and counts the
    /// record either way.
    ///
    /// # The link reference is not the CNID
    ///
    /// TN1150: "The link reference is not related to catalog node IDs. When a new
    /// indirect node file is created, it is assigned a new link reference randomly
    /// chosen from the range 100 to 1073741923", and a reference of 0 is invalid.
    /// Apple reuses the CNID anyway for file links, which is what `lib_fsck_hfs`
    /// assumes, so the two coincide on a real volume. This reuses it too, and
    /// names the inode after it -- which on this fixture is below 100 and so
    /// outside the documented range. Recorded rather than papered over; the next
    /// implementation should allocate from the range.
    ///
    /// # Verified against Apple's sources; not against `fsck.hfsplus`
    ///
    ///
    /// This operation is **not** gated on `fsck.hfsplus`, deliberately, and the
    /// reason is worth stating because it is a departure from the project's usual
    /// rule.
    ///
    /// `fsck.hfsplus` is not a conformance oracle -- `AGENTS.md` says so, and says
    /// it modifies the image it checks. For most of this crate's mutations that
    /// distinction does no work, because where `fsck` objected TN1150 and
    /// `lib_fsck_hfs` independently agreed with it: the empty-fork overlap, the stale
    /// index separator, the miscounted folders were all real.
    ///
    /// Hard links are the case where they do not. Apple's own writer
    /// (`hfs_makelink`, `createindirectlink`) and Apple's own checker
    /// (`lib_fsck_hfs/dfalib/HardLinkCheck.c`) disagree about the chain fields, and
    /// the checker's position is visibly a *migration* rather than a validation:
    /// "Now that we are in repair, all hard links should have this bit set because
    /// we upgrade all pre-Leopard file hard links to Leopard hard links on any file
    /// hard link repairs", and a link without the bit is one it tells you to
    /// "ignore ... from all check". A checker whose hard-link pass rewrites records
    /// during repair is not a neutral arbiter of that structure.
    ///
    /// So this is verified against what Apple's sources *say* the structure is --
    /// every field value below is transcribed from one of them, and `lib_fsck_hfs`
    /// confirmed the two that were wrong -- and `fsck`'s objection is recorded as a
    /// disagreement in `docs/source-map.md` rather than treated as falsity. What
    /// `fsck` still does to a volume with a hard link is one byte of flags plus a
    /// message; that it cannot *repair* the volume at all ("could not be repaired")
    /// is consistent with a failing comparison between two of its own hash tables
    /// rather than with a field this crate writes wrongly.
    ///
    /// The disagreement is written up in `docs/source-map.md` rather than asserted
    /// in a test, because a test can only record one of two outcomes and the point
    /// is that they have not been reconciled.
    ///
    /// # What it refuses
    ///
    /// A target that already has a link. Threading is `hfs_makelink`'s
    /// `c_linkcount == 2` case, and it walks `hl_prevLinkID`/`hl_nextLinkID`.
    ///
    /// Mining reference: `core/hfs_link.c` `hfs_makelink` and `createindirectlink`;
    /// `core/hfs_catalog.c` `cat_rename` and `cat_createlink`; `core/hfs.h`
    /// `MAKE_INODE_NAME`; `core/hfs_format.h` `HFS_INODE_PREFIX` and the `hl_*`
    /// aliases; TN1150's Hard Links section; `lib_fsck_hfs/dfalib/HardLinkCheck.c`.
    pub fn create_hard_link(&mut self, parent: u32, name: &[u16], target: u32) -> Result<u32> {
        self.begin_transaction()?;
        let result = self.create_hard_link_inner(parent, name, target);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            self.abandon_transaction();
        }
        result
    }

    fn create_hard_link_inner(&mut self, parent: u32, name: &[u16], target: u32) -> Result<u32> {
        use crate::catalog::key::CatalogKey;
        use crate::catalog::record::{
            FileRecord, K_HFS_HAS_LINK_CHAIN_MASK, K_HFS_THREAD_EXISTS_MASK, S_IFREG,
            THREAD_RECORD_NAME_LEN_OFFSET,
        };

        if name.is_empty() {
            return Err(Error::invalid("link", "a link cannot have an empty name"));
        }
        let parent_cnid = Cnid(parent);
        self.read_folder_record(parent)?;
        let private_folder = self.ensure_file_hardlinks_folder()?;

        let original = self.read_file_record(target)?;
        if original.has_link_chain() {
            return Err(Error::invalid(
                "link",
                format!("CNID {target} is itself a link; link to the indirect node"),
            ));
        }

        let link_cnid = self.header.next_catalog_id;
        if link_cnid <= ROOT_FOLDER_ID.0 {
            return Err(Error::invalid(
                "volume_header.nextCatalogID",
                format!("{link_cnid} is at or below the reserved CNID range"),
            ));
        }
        let now =
            crate::timestamp::now_hfs(self.header.has_expanded_times()).map_err(|e| Error::Io {
                message: e.to_string(),
            })?;

        // Step 1: find the record at its old location. Its key comes from its thread
        // record, not from `name` -- `name` is the link being created, which may be
        // anywhere, and the file being linked is wherever it already is.
        let target_thread_key = CatalogKey::for_child(Cnid(target), &[]);
        let old_thread = self
            .catalog_record_bytes(&target_thread_key)?
            .ok_or(Error::NotFound {
                what: "thread record",
            })?;
        let key_size = 2 + usize::from(u16::from_be_bytes([old_thread[0], old_thread[1]]));
        let body = old_thread.get(key_size..).ok_or(Error::Truncated {
            what: "thread record body",
            needed: key_size,
            available: old_thread.len(),
        })?;
        let target_parent = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
        let name_len = usize::from(u16::from_be_bytes([body[8], body[9]]));
        let target_name: Vec<u16> = body
            .get(10..10 + name_len * 2)
            .ok_or(Error::Truncated {
                what: "thread record name",
                needed: 10 + name_len * 2,
                available: body.len(),
            })?
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect();
        let target_key = CatalogKey::for_child(Cnid(target_parent), &target_name);
        let old_record = self
            .catalog_record_bytes(&target_key)?
            .ok_or(Error::NotFound {
                what: "catalog record",
            })?;

        // "Has this file been linked already?" is asked by looking for its indirect
        // node, not by reading `linkCount`: `lib_fsck_hfs` wants that count to be 1
        // for a file with one hard link, so it cannot double as "how many links".
        let inode_name: Vec<u16> = format!("{}{}", INODE_NAME_PREFIX, target)
            .encode_utf16()
            .collect();
        let inode_key = CatalogKey::for_child(Cnid(private_folder), &inode_name);
        if self.catalog_record_bytes(&inode_key)?.is_some() {
            return Err(Error::invalid(
                "link",
                format!(
                    "CNID {target} already has a hard link; a second one means \
                     threading the chain, which this does not implement yet"
                ),
            ));
        }
        let link_key = CatalogKey::for_child(parent_cnid, name);

        // A file thread record: `recordType(2) reserved(2) parentID(4) nameLen(2) name`.
        let thread_body = |parent: u32, n: &[u16]| -> Vec<u8> {
            let mut v = vec![0u8; THREAD_RECORD_NAME_LEN_OFFSET + 2 + n.len() * 2];
            v[0..2].copy_from_slice(
                &crate::catalog::record::K_HFS_PLUS_FILE_THREAD_RECORD.to_be_bytes(),
            );
            v[4..8].copy_from_slice(&parent.to_be_bytes());
            v[THREAD_RECORD_NAME_LEN_OFFSET..THREAD_RECORD_NAME_LEN_OFFSET + 2]
                .copy_from_slice(&(n.len() as u16).to_be_bytes());
            for (i, unit) in n.iter().enumerate() {
                let at = THREAD_RECORD_NAME_LEN_OFFSET + 2 + i * 2;
                v[at..at + 2].copy_from_slice(&unit.to_be_bytes());
            }
            v
        };

        // The indirect node: the file's own record, moved. It keeps its forks -- the
        // data lives here -- and its count is the number of links pointing at it.
        let mut indnode = original;
        indnode.bsd_info.special = 1; // linkCount
                                      // The inode's flags are NOT changed. It is the original file's record moved
                                      // by `cat_rename`, which does not touch flags; `createindirectlink` sets
                                      // `ca_recflags` on the link it creates and on nothing else. Setting the
                                      // chain bit here makes `HardLinkCheck.c` bucket the inode under `special` --
                                      // which on an inode is the link count, not a reference -- so it lands in a
                                      // bucket no link joins.
                                      // `hl_firstLinkID` is documented "Valid only if HasLinkChain flag is set
                                      // (indirect nodes only)", so the head is marked here.
        indnode.reserved1 = link_cnid;
        indnode.attribute_mod_date = now;
        let mut inode_record = inode_key.to_record();
        inode_record.extend_from_slice(&indnode.to_bytes());

        let mut inode_thread = target_thread_key.to_record();
        inode_thread.extend_from_slice(&thread_body(private_folder, &inode_name));

        // The link record: built from `createindirectlink`'s attribute set rather than
        // by copying the target, because three things in it are not a copy's.
        let mut link = FileRecord {
            file_id: Cnid(link_cnid),
            create_date: now,
            content_mod_date: now,
            attribute_mod_date: now,
            access_date: now,
            backup_date: now,
            flags: K_HFS_HAS_LINK_CHAIN_MASK | K_HFS_THREAD_EXISTS_MASK,
            text_encoding: K_TEXT_ENCODING_MAC_UNICODE,
            ..FileRecord::EMPTY
        };
        link.bsd_info.file_mode = S_IFREG | 0o444;
        link.bsd_info.admin_flags = UF_IMMUTABLE;
        link.bsd_info.owner_flags = UF_IMMUTABLE;
        link.bsd_info.special = target; // hl_linkReference: the inode's CNID
        link.bsd_info.owner_id = 0; // hl_prevLinkID
        link.bsd_info.group_id = 0; // hl_nextLinkID
                                    // TN1150: the type and creator go in **userInfo** -- the `FileInfo` at
                                    // offset 48. `finderInfo` is the `ExtendedFileInfo` at 64 and has no such
                                    // fields, so writing them there produces a record nothing recognises as a
                                    // link: `CatalogCheck.c` decides `islink` from
                                    // `file->userInfo.fdType == kHardLinkFileType &&
                                    //  file->userInfo.fdCreator == kHFSPlusCreator`, and a record that fails
                                    // that test is treated as an ordinary file -- whose `special` is then read
                                    // as a link *count*, and a count of 17 is "incorrect number of links".
        link.user_info[0..4].copy_from_slice(&K_HARD_LINK_FILE_TYPE.to_be_bytes());
        link.user_info[4..8].copy_from_slice(&K_HFS_PLUS_CREATOR.to_be_bytes());
        link.user_info[8..10].copy_from_slice(&K_HAS_BEEN_INITED.to_be_bytes());
        link.data_fork = crate::format::fork::ForkData::EMPTY;
        link.resource_fork = crate::format::fork::ForkData::EMPTY;
        let mut link_record = link_key.to_record();
        link_record.extend_from_slice(&link.to_bytes());

        let link_thread_key = CatalogKey::for_child(Cnid(link_cnid), &[]);
        let mut link_thread = link_thread_key.to_record();
        link_thread.extend_from_slice(&thread_body(parent, name));

        // Does the link land where the file was? If so the user-visible side keeps
        // exactly one entry and neither folder's count moves.
        let same_slot = target_parent == parent && target_name == name;
        let undo = |w: &mut Self| {
            let _ = w.remove_catalog_record(&link_thread_key);
            let _ = w.remove_catalog_record(&link_key);
            let _ = w.remove_catalog_record(&target_thread_key);
            let _ = w.remove_catalog_record(&inode_key);
            let _ = w.insert_catalog_record(&old_record);
            let _ = w.insert_catalog_record(&old_thread);
            let _ = w.change_folder_valence(private_folder, -1);
            if !same_slot {
                let _ = w.change_folder_valence(parent, 1);
                let _ = w.change_folder_valence(target_parent, -1);
            }
        };

        self.write_header_u32(64, link_cnid + 1)?;

        // Steps 2..5: the move, in cat_rename's order.
        self.insert_catalog_record(&inode_record)?;
        if let Err(e) = self.remove_catalog_record(&target_key) {
            let _ = self.remove_catalog_record(&inode_key);
            return Err(e);
        }
        if let Err(e) = self.remove_catalog_record(&target_thread_key) {
            let _ = self.insert_catalog_record(&old_record);
            let _ = self.remove_catalog_record(&inode_key);
            return Err(e);
        }
        if let Err(e) = self.insert_catalog_record(&inode_thread) {
            let _ = self.insert_catalog_record(&old_record);
            let _ = self.insert_catalog_record(&old_thread);
            let _ = self.remove_catalog_record(&inode_key);
            return Err(e);
        }

        // cat_createlink: thread first, then the record.
        if let Err(e) = self.insert_catalog_record(&link_thread) {
            undo(self);
            return Err(e);
        }
        if let Err(e) = self.insert_catalog_record(&link_record) {
            undo(self);
            return Err(e);
        }
        if let Err(e) = self.change_folder_valence(private_folder, 1) {
            undo(self);
            return Err(e);
        }
        if !same_slot {
            if let Err(e) = self.change_folder_valence(target_parent, -1) {
                let _ = self.change_folder_valence(private_folder, -1);
                undo(self);
                return Err(e);
            }
            if let Err(e) = self.change_folder_valence(parent, 1) {
                let _ = self.change_folder_valence(target_parent, 1);
                let _ = self.change_folder_valence(private_folder, -1);
                undo(self);
                return Err(e);
            }
        }

        self.header.next_catalog_id = link_cnid + 1;
        let file_count = self.header.file_count + 1;
        self.header.file_count = file_count;
        if let Err(e) = self.write_header_u32(32, file_count) {
            if !same_slot {
                let _ = self.change_folder_valence(parent, -1);
                let _ = self.change_folder_valence(target_parent, 1);
            }
            let _ = self.change_folder_valence(private_folder, -1);
            undo(self);
            return Err(e);
        }
        Ok(link_cnid)
    }

    /// Find the private folder for file hard links, creating it if it is not there.
    ///
    /// Every hard link's record belongs *inside* it, named by the link's own CNID --
    /// `hfs_makelink` renames the link inode in there rather than leaving it where
    /// the user asked for it. So a volume with a link but no such folder is not a
    /// volume `fsck` can find the chain in, which is what the count complaints were
    /// about.
    ///
    /// Idempotent: a second call finds the existing folder rather than making a
    /// Idempotent: a second call finds the existing folder rather than making a
    /// second one, because two folders with the same name is a catalog with two
    /// answers to every question about it.
    ///
    /// On a journaled volume, a folder creation is wrapped in a journal
    /// transaction. When called from [`Self::create_hard_link`], a transaction is
    /// already open and is reused rather than started again.
    pub fn ensure_file_hardlinks_folder(&mut self) -> Result<u32> {
        use crate::catalog::lookup::Catalog;

        let root = ROOT_FOLDER_ID;
        let name: Vec<u16> = FILE_HARDLINKS_FOLDER.encode_utf16().collect();
        let started_own = self.maybe_begin_transaction()?;
        // The lookup and the create are separate borrows: `Catalog` holds a shared
        // reborrow of the device, and `create_folder` needs it mutably.
        let existing = {
            let catalog = Catalog::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            catalog.lookup(root, &name)?
        };
        let result = match existing {
            Some(crate::catalog::record::CatalogRecord::Folder(f)) => Ok(f.folder_id.0),
            Some(other) => Err(Error::invalid(
                "catalog",
                format!(
                    "{FILE_HARDLINKS_FOLDER} exists but is a {other:?}, not a \
                     folder, so it cannot hold hard link records"
                ),
            )),
            None => self.create_folder_inner(root.0, &name),
        };
        if started_own {
            if result.is_ok() {
                self.end_transaction()?;
            } else {
                self.abandon_transaction();
            }
        }
        result
    }

    /// Create an empty folder called `name` in `parent`, and return its CNID.
    ///
    /// The same shape as [`Self::create_file`] with a folder record and a folder
    /// thread record in place of the file ones, and the same all-or-nothing
    /// guarantee: the CNID counter advances first and is not rolled back, and
    /// everything after it is undone if a later step fails.
    ///
    /// `folderCount` in the volume header counts folders and, unlike `fileCount`,
    /// **excludes the root** -- so a volume holding one folder and nothing else
    /// reports 1, not 2. `fsck.hfsplus` normalises a reported 1 to 0, and counting
    /// the root here is the kind of plausible value only an independent checker
    /// catches.
    pub fn create_folder(&mut self, parent: u32, name: &[u16]) -> Result<u32> {
        self.begin_transaction()?;
        let result = self.create_folder_inner(parent, name);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            self.abandon_transaction();
        }
        result
    }

    fn create_folder_inner(&mut self, parent: u32, name: &[u16]) -> Result<u32> {
        use crate::catalog::key::CatalogKey;
        use crate::catalog::record::{FolderRecord, THREAD_RECORD_NAME_LEN_OFFSET};

        if name.is_empty() {
            return Err(Error::invalid(
                "create",
                "a folder cannot have an empty name",
            ));
        }
        // Validated before anything is written, so a bad parent costs nothing.
        self.read_folder_record(parent)?;

        let cnid = self.header.next_catalog_id;
        if cnid <= ROOT_FOLDER_ID.0 {
            return Err(Error::invalid(
                "volume_header.nextCatalogID",
                format!("{cnid} is at or below the reserved CNID range"),
            ));
        }
        let now =
            crate::timestamp::now_hfs(self.header.has_expanded_times()).map_err(|e| Error::Io {
                message: e.to_string(),
            })?;

        let folder = FolderRecord {
            folder_id: Cnid(cnid),
            create_date: now,
            content_mod_date: now,
            attribute_mod_date: now,
            access_date: now,
            backup_date: now,
            ..FolderRecord::EMPTY
        };
        let mut thread = vec![0u8; THREAD_RECORD_NAME_LEN_OFFSET + 2 + name.len() * 2];
        thread[0..2].copy_from_slice(
            &crate::catalog::record::K_HFS_PLUS_FOLDER_THREAD_RECORD.to_be_bytes(),
        );
        thread[4..8].copy_from_slice(&Cnid(parent).0.to_be_bytes());
        thread[THREAD_RECORD_NAME_LEN_OFFSET..THREAD_RECORD_NAME_LEN_OFFSET + 2]
            .copy_from_slice(&(name.len() as u16).to_be_bytes());
        for (i, unit) in name.iter().enumerate() {
            let at = THREAD_RECORD_NAME_LEN_OFFSET + 2 + i * 2;
            thread[at..at + 2].copy_from_slice(&unit.to_be_bytes());
        }

        let child_key = CatalogKey::for_child(Cnid(parent), name);
        let thread_key = CatalogKey::for_child(Cnid(cnid), &[]);
        let mut child = child_key.to_record();
        child.extend_from_slice(&folder.to_bytes());
        let mut thread_record = thread_key.to_record();
        thread_record.extend_from_slice(&thread);

        self.write_header_u32(64, cnid + 1)?;
        let undo = |w: &mut Self| {
            let _ = w.remove_catalog_record(&thread_key);
            let _ = w.remove_catalog_record(&child_key);
        };

        self.insert_catalog_record(&child)?;
        if let Err(e) = self.insert_catalog_record(&thread_record) {
            undo(self);
            return Err(e);
        }
        if let Err(e) = self.change_folder_valence(parent, 1) {
            undo(self);
            return Err(e);
        }
        let folder_count = self
            .header
            .folder_count
            .checked_add(1)
            .ok_or_else(|| Error::overflow("volume_header.folderCount"))?;
        if let Err(e) = self.write_header_u32(36, folder_count) {
            let _ = self.change_folder_valence(parent, -1);
            undo(self);
            return Err(e);
        }
        self.header.folder_count = folder_count;
        self.header.next_catalog_id = cnid + 1;
        Ok(cnid)
    }

    /// Rename, or move, the object at `(from_parent, from_name)` to
    /// `(to_parent, to_name)`, and return its CNID.
    ///
    /// The object keeps its CNID and its forks -- a move is a change to the catalog,
    /// not to the data -- so the CNID is returned rather than newly allocated, and
    /// neither `fileCount` nor `folderCount` moves.
    ///
    /// # The four steps, and why in that order
    ///
    /// 1. **Insert** the record under the new key. First, because the new record
    ///    carries the old one's body, so nothing has to be reconstructed if the
    ///    insert fails.
    /// 2. **Remove** the record from the old key. If that fails the new one is
    ///    removed again -- the destination is then untouched, and a caller that
    ///    retries sees the original name.
    /// 3. **Replace** the thread record. Its key is `(cnid, "")` either way, so this
    ///    is a remove and an insert under one key rather than a move, and the thread
    ///    record's *body* is where an object's name is written down. A reader
    ///    resolves a name through it, so leaving it stale is what makes a renamed
    ///    file reachable by the old name and not the new one.
    /// 4. **Adjust the two folders' child counts**, and only if the folder changed.
    ///
    /// Mining reference: `core/hfs_catalog.c` `cat_rename`, whose steps are
    /// "insert cnode at new location", "remove cnode from old location",
    /// "remove cnode's old thread record", "insert cnode's new thread record" --
    /// and which refuses a destination name already in use with `EEXIST` unless the
    /// parents are the same.
    pub fn rename(
        &mut self,
        from_parent: u32,
        from_name: &[u16],
        to_parent: u32,
        to_name: &[u16],
    ) -> Result<u32> {
        self.begin_transaction()?;
        let result = self.rename_inner(from_parent, from_name, to_parent, to_name);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            self.abandon_transaction();
        }
        result
    }

    fn rename_inner(
        &mut self,
        from_parent: u32,
        from_name: &[u16],
        to_parent: u32,
        to_name: &[u16],
    ) -> Result<u32> {
        use crate::catalog::key::CatalogKey;
        use crate::catalog::record::{
            CatalogRecord, FolderRecord, THREAD_RECORD_FIXED_SIZE, THREAD_RECORD_NAME_LEN_OFFSET,
        };

        if from_name.is_empty() || to_name.is_empty() {
            return Err(Error::invalid("rename", "a name cannot be empty"));
        }
        let from_parent_cnid = Cnid(from_parent);
        let to_parent_cnid = Cnid(to_parent);

        let object = {
            use crate::catalog::lookup::Catalog;
            let catalog = Catalog::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            catalog
                .lookup(from_parent_cnid, from_name)?
                .ok_or(Error::NotFound {
                    what: "catalog entry",
                })?
        };
        let is_folder = matches!(object, CatalogRecord::Folder(_));
        let cnid = match &object {
            CatalogRecord::File(f) => f.file_id.0,
            CatalogRecord::Folder(fo) => fo.folder_id.0,
            CatalogRecord::Thread(_) => {
                return Err(Error::invalid(
                    "rename",
                    "a thread record has no name to rename",
                ))
            }
        };

        let old_key = CatalogKey::for_child(from_parent_cnid, from_name);
        let new_key = CatalogKey::for_child(to_parent_cnid, to_name);
        let old_record = self
            .catalog_record_bytes(&old_key)?
            .ok_or(Error::NotFound {
                what: "catalog record",
            })?;

        // A destination that is already taken is `EEXIST`, not an overwrite -- with
        // one exception that looks like an overwrite and is not.
        //
        // On a case-insensitive volume `Readme.txt` and `README.TXT` are *one key* to
        // the tree. `cat_rename` allows that collision only after confirming the
        // record it found has the same record type and the same CNID, and refuses
        // everything else with `EEXIST`; Apple's comment on the branch is "the old
        // name is a case variant and must be removed". So this is one object under
        // two spellings -- a re-key, not a move -- and it is decided here with the
        // tree's comparator, because the insert below runs *before* the remove and
        // would refuse it as a duplicate before anything could notice.
        //
        // A record's identity is its CNID, at offset 8 of the body in both a file
        // record and a folder record; the body's length says which type it is.
        let identity = |record: &[u8]| -> Option<u32> {
            let key_len = usize::from(u16::from_be_bytes([record[0], record[1]]));
            let body = record.get(2 + key_len..)?;
            if body.len() != crate::catalog::record::FILE_RECORD_SIZE
                && body.len() != crate::catalog::record::FOLDER_RECORD_SIZE
            {
                return None;
            }
            Some(u32::from_be_bytes([body[8], body[9], body[10], body[11]]))
        };
        // A folder may not be moved beneath itself.
        //
        // Moving `/a` into `/a/b` would make the path to `/a` run through `/a`, and
        // every lookup of it afterwards would have to decide where to stop. Apple
        // refuses the obvious cases outright -- the root, the destination folder
        // itself, and the destination's own parent -- and then walks the destination
        // path back to the root looking for the folder being moved.
        //
        // Refused before anything is written, because a rename that has already
        // inserted the new record and then discovers the cycle has left the volume
        // with two records for one object.
        if is_folder && from_parent != to_parent {
            if cnid == ROOT_FOLDER_ID.0 {
                return Err(Error::invalid("rename", "the root folder cannot be moved"));
            }
            if cnid == to_parent || self.folder_is_ancestor(cnid, to_parent)? {
                return Err(Error::invalid(
                    "rename",
                    format!(
                        "CNID {cnid} is the destination folder or one of the folders \
                         above it, so moving it there would make the path to it run \
                         through itself"
                    ),
                ));
            }
        }

        let folded = self.catalog_record_bytes_folded(&new_key)?;
        let rekey = from_parent == to_parent
            && folded.as_deref().and_then(identity) == identity(&old_record);
        if folded.is_some() && !rekey {
            return Err(Error::invalid(
                "rename",
                format!(
                    "CNID {to_parent} already has a child named {:?}",
                    String::from_utf16_lossy(to_name)
                ),
            ));
        }
        // Reading the destination folder validates it before anything is written.
        self.read_folder_record(to_parent)?;

        let old_key = CatalogKey::for_child(from_parent_cnid, from_name);
        let new_key = CatalogKey::for_child(to_parent_cnid, to_name);
        let now =
            crate::timestamp::now_hfs(self.header.has_expanded_times()).map_err(|e| Error::Io {
                message: e.to_string(),
            })?;

        // Rebuild the record under the new key: same object, same CNID, same
        // forks, with the modification time moved.
        let mut new_record = new_key.to_record();
        match &object {
            CatalogRecord::File(f) => {
                let mut f = *f.as_ref();
                f.content_mod_date = now;
                f.attribute_mod_date = now;
                new_record.extend_from_slice(&f.to_bytes());
            }
            CatalogRecord::Folder(fo) => {
                let mut fo = *fo;
                fo.content_mod_date = now;
                fo.attribute_mod_date = now;
                new_record.extend_from_slice(&fo.to_bytes());
            }
            CatalogRecord::Thread(_) => unreachable!("checked above"),
        }

        // The thread record, whose body carries the object's new name.
        let mut thread = vec![0u8; THREAD_RECORD_NAME_LEN_OFFSET + 2 + to_name.len() * 2];
        thread[0..2].copy_from_slice(
            &(if is_folder {
                crate::catalog::record::K_HFS_PLUS_FOLDER_THREAD_RECORD
            } else {
                crate::catalog::record::K_HFS_PLUS_FILE_THREAD_RECORD
            })
            .to_be_bytes(),
        );
        thread[4..8].copy_from_slice(&to_parent_cnid.0.to_be_bytes());
        thread[THREAD_RECORD_NAME_LEN_OFFSET..THREAD_RECORD_NAME_LEN_OFFSET + 2]
            .copy_from_slice(&(to_name.len() as u16).to_be_bytes());
        for (i, unit) in to_name.iter().enumerate() {
            let at = THREAD_RECORD_NAME_LEN_OFFSET + 2 + i * 2;
            thread[at..at + 2].copy_from_slice(&unit.to_be_bytes());
        }
        let thread_key = CatalogKey::for_child(Cnid(cnid), &[]);
        let mut new_thread = thread_key.to_record();
        new_thread.extend_from_slice(&thread);

        let old_record = self
            .catalog_record_bytes(&old_key)?
            .ok_or(Error::NotFound {
                what: "catalog record",
            })?;
        let old_thread = self
            .catalog_record_bytes(&thread_key)?
            .ok_or(Error::NotFound {
                what: "thread record",
            })?;

        // Steps 1 and 2, in one order or the other.
        //
        // Normally: insert the new key, then remove the old one. The insert first
        // because the new record carries the old body, so nothing has to be
        // reconstructed if it fails.
        //
        // For a re-key the other way round. The two spellings are one key to the
        // tree, so inserting before removing finds the very record that is about to
        // leave and refuses it as a duplicate. The bytes are already read, so a
        // failure puts them straight back.
        //
        // And for a re-key there is only *one* removal: removing in the branch above
        // and again here is a second attempt at a record that is gone, which is what
        // made the first attempt at this report "catalog record to remove" for a
        // record it had just deleted itself.
        if rekey {
            self.remove_catalog_record(&old_key)?;
            if let Err(e) = self.insert_catalog_record(&new_record) {
                let _ = self.insert_catalog_record(&old_record);
                return Err(e);
            }
        } else {
            self.insert_catalog_record(&new_record)?;
            if let Err(e) = self.remove_catalog_record(&old_key) {
                let _ = self.remove_catalog_record(&new_key);
                return Err(e);
            }
        }
        // Step 3.
        self.remove_catalog_record(&thread_key)?;
        if let Err(e) = self.insert_catalog_record(&new_thread) {
            let _ = self.insert_catalog_record(&old_thread);
            let _ = self.insert_catalog_record(&old_record);
            return Err(e);
        }
        // Step 4.
        if from_parent != to_parent {
            self.change_folder_valence(from_parent, -1)?;
            if let Err(e) = self.change_folder_valence(to_parent, 1) {
                let _ = self.change_folder_valence(from_parent, 1);
                let _ = self.remove_catalog_record(&new_key);
                let _ = self.insert_catalog_record(&old_thread);
                let _ = self.insert_catalog_record(&old_record);
                return Err(e);
            }
        }
        let _ = (FolderRecord::EMPTY, THREAD_RECORD_FIXED_SIZE);
        Ok(cnid)
    }

    /// The bytes of the catalog record whose key *compares equal* to this one.
    ///
    /// Exact keys are what removal and rollback need -- they name one specific
    /// record. This is for the one place that needs the tree's own notion of
    /// equality, which on a case-insensitive volume is not string equality: a name
    /// differing only in case is the same key to the tree, so a rename between two
    /// spellings is a re-key rather than a move, and no amount of comparing names
    /// as strings can tell it from a genuine collision.
    ///
    /// `CatalogKey::from_record(&key.to_record(), max)` is deliberate: it goes
    /// through the same bytes the tree stores, so the search key is the key the
    /// caller asked for rather than a reconstruction of it. That the round trip is
    /// exact is asserted by `a_key_survives_being_encoded_and_decoded_again`.
    fn catalog_record_bytes_folded(
        &self,
        key: &crate::catalog::key::CatalogKey,
    ) -> Result<Option<Vec<u8>>> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::NodeKind;
        use crate::catalog::key::CatalogKey;
        use crate::catalog::lookup::split_record;
        use crate::unicode::Ordering;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let incoming =
            CatalogKey::from_record(&key.to_record(), usize::from(bt.header().max_key_length))?;
        let catalog = crate::catalog::lookup::Catalog::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let btree_header = *bt.header();
        let mut node_num = btree_header.first_leaf_node;
        let mut budget = btree_header.total_nodes;
        while budget > 0 && node_num != 0 {
            budget -= 1;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }
            for index in 0..node.num_records() {
                let record = node.record(index)?;
                let Some((existing, _)) = split_record(record) else {
                    continue;
                };
                if catalog.compare_keys(&existing, &incoming) == Ordering::Equal {
                    return Ok(Some(record.to_vec()));
                }
            }
            if node_num == btree_header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
            if node_num == 0 || node_num >= btree_header.total_nodes {
                break;
            }
        }
        Ok(None)
    }

    /// The bytes of the catalog record under exactly this key, if it is there.
    ///
    /// Whole records, so a caller can put one back. That is what makes a removal
    /// reversible, and a removal has to be reversible: a delete touches two records
    /// and a folder's child count, and a failure after the first leaves a file
    /// record with no thread record.
    fn catalog_record_bytes(
        &self,
        key: &crate::catalog::key::CatalogKey,
    ) -> Result<Option<Vec<u8>>> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::NodeKind;
        use crate::catalog::lookup::split_record;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let btree_header = *bt.header();
        let mut node_num = btree_header.first_leaf_node;
        let mut budget = btree_header.total_nodes;
        while budget > 0 && node_num != 0 {
            budget -= 1;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }
            for index in 0..node.num_records() {
                let record = node.record(index)?;
                let Some((existing, _)) = split_record(record) else {
                    continue;
                };
                if existing.parent_id == key.parent_id && existing.name == key.name {
                    return Ok(Some(record.to_vec()));
                }
            }
            if node_num == btree_header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
            if node_num == 0 || node_num >= btree_header.total_nodes {
                break;
            }
        }
        Ok(None)
    }

    /// Remove the catalog record with exactly this key, if it is there.
    ///
    /// The inverse of [`Self::insert_catalog_record`] for one key, and what makes a
    /// create rollback-able. Records are located by key rather than by CNID because
    /// the keys are what the caller built and what it has to name again: the thread
    /// record's CNID is in its *key*, not its body, and the file record's key is
    /// `(parent, name)`.
    ///
    /// `leafRecords` is decremented, because it counts records across all leaves
    /// and this is one fewer.
    fn remove_catalog_record(&mut self, key: &crate::catalog::key::CatalogKey) -> Result<()> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::{num_records, remove_record};

        let hit = self.catalog_record_bytes(key)?;
        let Some(bytes_of) = hit else {
            return Err(Error::NotFound {
                what: "catalog record to remove",
            });
        };
        // Re-locate: the walk above and the edit below must agree on the node and
        // index, and doing it twice is cheaper than holding a borrow across the
        // write.
        let node_num = self.leaf_for(
            &bytes_of,
            self.btree_header_u32(crate::btree::header::FIRST_LEAF_OFFSET)?,
        )?;
        let node_index = {
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            let mut found = None;
            for index in 0..node.num_records() {
                let record = node.record(index)?;
                let Some((existing, _)) = crate::catalog::lookup::split_record(record) else {
                    continue;
                };
                if existing.parent_id == key.parent_id && existing.name == key.name {
                    found = Some(index);
                    break;
                }
            }
            found
        };
        let Some(index) = node_index else {
            return Err(Error::NotFound {
                what: "catalog record to remove",
            });
        };

        let (at, buf, count) = {
            use crate::btree::io::BTreeFile;
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            let mut buf = bt.read_node_bytes(node_num)?;
            remove_record(&mut buf, usize::from(index))?;
            let count = num_records(&buf)?;
            (bt.node_offset(node_num)?, buf, count)
        };
        self.journal_write(at, &buf)?;

        let leaf_records = self.btree_header_leaf_records()?;
        let leaf_records = leaf_records.checked_sub(1).ok_or_else(|| {
            Error::invalid("BTHeaderRec.leafRecords", "a remove took it below zero")
        })?;
        self.write_btree_header_u32(crate::btree::header::LEAF_RECORDS_OFFSET, leaf_records)?;

        let _ = count;
        // A removal can change the leaf's *first* record, which is what the index
        // separator for it is.
        self.refresh_index()
    }

    /// Read a folder record by CNID.
    fn read_folder_record(&self, cnid: u32) -> Result<crate::catalog::record::FolderRecord> {
        let bytes = match self.find_catalog_record_any(cnid)? {
            Some(hit) => hit,
            None => {
                return Err(Error::NotFound {
                    what: "folder record",
                })
            }
        };
        if bytes.len() != crate::catalog::record::FOLDER_RECORD_SIZE {
            return Err(Error::invalid(
                "catalog record",
                format!(
                    "CNID {cnid} has a {}-byte record, which is neither a file \
                     ({}) nor a folder ({}), so it is not a folder",
                    crate::catalog::record::FILE_RECORD_SIZE,
                    bytes.len(),
                    crate::catalog::record::FOLDER_RECORD_SIZE
                ),
            ));
        }
        crate::catalog::record::FolderRecord::parse(&bytes)
    }

    /// Increment a folder's child count and touch its modification time.
    ///
    /// A folder's `valence` is the number of children it has, and a name lookup
    /// that never consults it still works -- which is exactly why it goes stale
    /// unnoticed. `fsck.hfsplus` counts.
    fn bump_folder_valence(&mut self, cnid: u32, now: u32) -> Result<()> {
        let mut record = self.read_folder_record(cnid)?;
        record.valence = record
            .valence
            .checked_add(1)
            .ok_or_else(|| Error::overflow("folder valence"))?;
        record.content_mod_date = now;
        let body = record.to_bytes();
        self.replace_catalog_body(cnid, &body)
    }

    /// Write one `u32` field of the volume header.
    ///
    /// Offsets are named at the call site as a bare number, which is a readability
    /// problem this does not solve -- but a symbolic constant per header field
    /// would be a wider change than this method, and getting it wrong is caught by
    /// `fsck.hfsplus` recomputing the count.
    fn write_header_u32(&mut self, offset: u64, value: u32) -> Result<()> {
        self.journal_write(
            crate::blockdev::VOLUME_HEADER_OFFSET + offset,
            &value.to_be_bytes(),
        )?;
        self.sync_backup_header()
    }

    /// Split the first leaf so `record` has somewhere to go, and insert it.
    ///
    /// Called only when an insertion into a leaf has been refused for want of room.
    /// Four structures change, each for its own reason:
    ///
    /// 1. **A new leaf node**, allocated from the header node's map. The records are
    ///    divided at the midpoint by *bytes*, not by count: a leaf holding one huge
    ///    record and many tiny ones, divided by count, would leave one half nearly
    ///    empty and the other overfull, and the overfull one would split again
    ///    immediately.
    /// 2. **The leaf chain**, which is doubly linked, so inserting a leaf touches
    ///    three links and not one. `before: leaf <-> next` becomes
    ///    `leaf <-> new <-> next`: the new leaf inherits `next`, and `next`'s
    ///    `bLink` is repointed at it. Giving the new leaf an `fLink` of zero --
    ///    what a "the new leaf is last" shortcut yields -- truncates the chain, and
    ///    fsck reports the remainder as an invalid sibling link while every leaf it
    ///    can still reach looks fine.
    /// 3. **The parent**, which gains a key pointing at the new leaf.
    /// 4. **The header**, which loses two `freeNodes`.
    ///
    /// `leafRecords` does **not** change: a split redistributes records between two
    /// nodes, so the total across all leaves is the same before and after.
    ///
    /// # Is the leaf its own parent?
    ///
    /// Tested as `root_node == leaf_num`, *not* `tree_depth == 0`. Apple counts the
    /// leaf level in `treeDepth`: `BTInsertRecord`'s empty-tree case creates a leaf
    /// and sets `treeDepth = 1`, so a tree with one leaf has `treeDepth == 1` and its
    /// root *is* the leaf. Reaching for `depth == 0` finds a tree of depth 1 with a
    /// leaf as its root, concludes there is a parent, and writes an index record
    /// into a leaf -- which still parses and still searches, and returns a leaf that
    /// does not contain the key.
    ///
    /// # What it refuses
    ///
    /// A tree more than one level deep, whose parent index node would itself need
    /// splitting. That is the same algorithm one level up, and refusing by name
    /// beats a half-split index node.
    ///
    /// Mining reference: `core/BTree.c` `BTInsertRecord` checks the fit and hands off
    /// to `InsertTree`; `GetNewNode` sets the new node's kind and height, and
    /// `firstLeafNode`/`lastLeafNode` are updated with the control block *before*
    /// the node is written, because `UpdateNode` compares the node's height against
    /// `treeDepth`.
    fn split_leaf_and_insert(&mut self, record: &[u8], at: u16) -> Result<()> {
        use crate::btree::header::{
            allocate_node, FREE_NODES_OFFSET, LAST_LEAF_OFFSET, ROOT_NODE_OFFSET, TREE_DEPTH_OFFSET,
        };

        let node_size = self.catalog_node_size()?;
        // 2 is the normal depth for a catalog with an index node above its leaves,
        // so it is not a case to refuse. 3 or more means the leaf's parent is *not*
        // the root, and adding a key to it may require splitting that parent too --
        // the same algorithm one level up, and refused rather than half-done.
        let tree_depth = self.btree_header_u16(TREE_DEPTH_OFFSET)?;
        if tree_depth > 2 {
            return Err(Error::invalid(
                "catalog leaf",
                format!(
                    "the catalog is {tree_depth} levels deep and its leaf is full; \
                     splitting the parent index node as well is not implemented"
                ),
            ));
        }

        let mut total_nodes = self.btree_header_u32(crate::btree::header::TOTAL_NODES_OFFSET)?;
        let mut free_nodes = self.btree_header_u32(FREE_NODES_OFFSET)?;
        let first_leaf = self.btree_header_u32(crate::btree::header::FIRST_LEAF_OFFSET)?;
        // The leaf the key belongs to, not necessarily the first one. With an index
        // node above the leaves they stop being interchangeable, and splitting the
        // wrong one produces halves whose keys straddle another leaf's.
        let leaf_num = self.leaf_for(record, first_leaf)?;
        let last_leaf = self.btree_header_u32(LAST_LEAF_OFFSET)?;
        let root_node = self.btree_header_u32(ROOT_NODE_OFFSET)?;
        let leaf_is_root = root_node == leaf_num;

        // Two nodes when the leaf is the root -- a new leaf and a new index node --
        // and one otherwise. Checked before allocating anything, so a refusal leaves
        // the map exactly as it was.
        let needed = if leaf_is_root { 2u32 } else { 1u32 };
        if free_nodes < needed {
            // No node free. Growing the catalog is the alternative to refusing, and
            // it is what makes a volume writable more than a few dozen times. The
            // node count asked for is Apple's: one per index level of depth, plus
            // the two this split needs, plus every node currently in use -- because
            // the nodes that are in use are the ones that were full, and they will
            // need splitting again.
            let want = u32::from(tree_depth)
                .saturating_add(1)
                .saturating_add(total_nodes - free_nodes)
                .saturating_add(needed);
            self.grow_catalog(want)?;
            // Both counts come back from the header rather than being assumed:
            // growth raised `totalNodes`, and `allocate_node` refuses to hand out a
            // node at or above it, so a stale value here looks exactly like a full
            // tree.
            free_nodes = self.btree_header_u32(crate::btree::header::FREE_NODES_OFFSET)?;
            total_nodes = self.btree_header_u32(crate::btree::header::TOTAL_NODES_OFFSET)?;
            if free_nodes < needed {
                return Err(Error::no_space(needed, u64::from(free_nodes)));
            }
        }

        let mut header = self.read_catalog_node(0)?;
        let new_leaf = allocate_node(&mut header, total_nodes, &mut free_nodes)?;
        let new_root = if leaf_is_root {
            Some(allocate_node(&mut header, total_nodes, &mut free_nodes)?)
        } else {
            None
        };
        // The map goes to disk before anything points into it. A node allocated and
        // not yet referenced is a spare node; a node referenced and not yet
        // allocated is a tree that follows a link off its own map.
        self.write_catalog_node(0, &header)?;
        self.write_btree_header_u32(FREE_NODES_OFFSET, free_nodes)?;

        let leaf = self.read_catalog_node(leaf_num)?;
        // A node's height is `treeDepth` less the number of index levels above it:
        // the root sits at `treeDepth`, and a leaf under a single index node at 1.
        // A one-leaf catalog is the degenerate case -- the root *is* the leaf -- and
        // `mkfs.hfsplus` writes it at height 1 with `treeDepth` 1, which is the same
        // rule.
        //
        // So the leaves keep height 1 across a split, and the index node introduced
        // above them takes the new `treeDepth`. Getting that backwards is invisible
        // until a catalog has two levels, at which point every node in it disagrees
        // with the depth.
        let height = leaf[9];
        let count = usize::from(crate::btree::node::num_records(&leaf)?);
        let split_at = Self::split_point(&leaf, count, node_size)?;
        let (lower, upper) = Self::divide(&leaf, split_at, record, at)?;

        let old_f_link = u32::from_be_bytes([leaf[0], leaf[1], leaf[2], leaf[3]]);
        // The new leaf inherits the old leaf's forward link and points back at it;
        // the old leaf's forward link becomes the new leaf, and its own back link is
        // unchanged because its predecessor has not moved.
        self.write_catalog_node(
            new_leaf,
            &Self::build_node(node_size, 0xFF, height, old_f_link, leaf_num, &upper.0)?,
        )?;
        // The split leaf keeps its own back link. Zeroing it is only right when the
        // split leaf is the first, and a tree with one leaf is the only place that
        // is true -- so zeroing it unconditionally breaks the chain the moment a
        // second leaf exists, which fsck reports as an invalid sibling link.
        self.write_catalog_node(
            leaf_num,
            &Self::build_node(
                node_size,
                0xFF,
                height,
                new_leaf,
                u32::from_be_bytes([leaf[4], leaf[5], leaf[6], leaf[7]]),
                &lower.0,
            )?,
        )?;
        if old_f_link != 0 {
            let mut successor = self.read_catalog_node(old_f_link)?;
            successor[4..8].copy_from_slice(&new_leaf.to_be_bytes());
            self.write_catalog_node(old_f_link, &successor)?;
        }
        if last_leaf == leaf_num {
            self.write_btree_header_u32(LAST_LEAF_OFFSET, new_leaf)?;
        }

        // The parent. Either a brand-new root, or -- since the tree is one level of
        // index above the leaves -- the root index node, rebuilt.
        //
        // Rebuilt rather than patched, and that is not a shortcut. A key inserted at
        // a searched position is correct only if the search agrees with the
        // partitioning the insert is meant to maintain; one wrong comparison and the
        // index describes leaves its keys do not lead to. fsck reports that as an
        // invalid index link and no reader can detect it -- the tree still searches,
        // and returns a leaf that does not contain the key. Walking the chain cannot
        // be wrong, because the chain is the truth.
        if let Some(root) = new_root {
            let node = Self::build_node(
                node_size,
                // `kBTIndexNode` is 0. `kBTHeaderNode` is 1, and using it produces a
                // node that parses as a header -- so the `BTHeaderRec` at offset 14
                // is read as its first record, and fsck reports "Invalid key
                // length" from inside a record that is not a record at all.
                0x00,
                // The root index node's height is the tree's depth -- the root sits
                // at `treeDepth` -- and this is the *new* depth, one more than the
                // tree the leaf was the root of. Writing the old depth here gives the
                // root the same height as the leaves below it, which fsck reports as
                // an invalid node height.
                (tree_depth + 1) as u8,
                0,
                0,
                &[
                    Self::index_record(&lower.0[0], leaf_num)?,
                    Self::index_record(&upper.0[0], new_leaf)?,
                ],
            )?;
            self.write_catalog_node(root, &node)?;
            self.write_btree_header_u32(ROOT_NODE_OFFSET, root)?;
            // `treeDepth` counts the index levels *above* the leaves, so a tree with
            // an index node is one deeper than the tree it replaces. Apple creates
            // the first leaf with `treeDepth = 1`, and the split that introduces an
            // index node makes it 2 -- leaving it at 1 is what makes fsck report
            // "Invalid node height", because the root index node is then shallower
            // than the tree it roots.
            //
            // The depth goes in *before* the root pointer: `UpdateNode` compares a
            // node's height against `treeDepth`, and a writer that sees the new root
            // before the depth is 2 reads a header describing a deeper tree than the
            // nodes do.
            self.write_btree_header_u16(TREE_DEPTH_OFFSET, tree_depth + 1)?;
        } else {
            let records = self.index_records_for_chain()?;
            let root = self.read_catalog_node(root_node)?;
            let node = Self::build_node(node_size, 0x00, root[9], 0, 0, &records)?;
            self.write_catalog_node(root_node, &node)?;
        }
        Ok(())
    }

    /// One index record per leaf, in chain order: each leaf's first record's key,
    /// then that leaf's node number.
    ///
    /// The index record is `[u16 keyLength][key][u32 child]` -- the key copied
    /// verbatim from the leaf record, including its length prefix, so the two can
    /// never disagree about how long the key is. Mining reference:
    /// `GetChildNodeNum` reads the child from `CalcKeySize` bytes past the record,
    /// and `CalcKeySize` is `key.length16 + 2` with no masking.
    ///
    /// Bounded by the node count, so a chain with a cycle in it terminates instead
    /// of spinning.
    fn index_records_for_chain(&self) -> Result<Vec<Vec<u8>>> {
        let total = self.btree_header_u32(crate::btree::header::TOTAL_NODES_OFFSET)?;
        let mut records = Vec::new();
        let mut cursor = self.btree_header_u32(crate::btree::header::FIRST_LEAF_OFFSET)?;
        let mut seen = 0u32;
        while cursor != 0 && seen < total {
            seen += 1;
            let leaf = self.read_catalog_node(cursor)?;
            if leaf[8] != 0xFF {
                return Err(Error::invalid(
                    "catalog leaf chain",
                    format!("node {cursor} is in the leaf chain but is not a leaf"),
                ));
            }
            // A leaf with no records has no first key, and an index record needs
            // one -- a key every search in this subtree would be compared against.
            if crate::btree::node::num_records(&leaf)? == 0 {
                return Err(Error::invalid(
                    "catalog leaf chain",
                    format!("node {cursor} is an empty leaf in the chain"),
                ));
            }
            let first = Self::record_at(&leaf, 0)?;
            records.push(Self::index_record(&first, cursor)?);
            cursor = u32::from_be_bytes([leaf[0], leaf[1], leaf[2], leaf[3]]);
        }
        Ok(records)
    }

    /// Build a node of `kind` and `height` holding `records`.
    ///
    /// Laid out as `mkfs.hfsplus` writes it: the descriptor first, then records
    /// ascending from byte 14, then the offset array at the end. `fLink` is at 0 and
    /// `bLink` at 4 -- a descriptor's links come *before* its kind and height, and
    /// swapping them produces a node that parses but links nowhere.
    fn build_node(
        node_size: usize,
        kind: u8,
        height: u8,
        f_link: u32,
        b_link: u32,
        records: &[Vec<u8>],
    ) -> Result<Vec<u8>> {
        use crate::btree::node::{set_record_count, write_offset, NODE_DESCRIPTOR_SIZE};

        let total: usize = records.iter().map(Vec::len).sum();
        let needed = NODE_DESCRIPTOR_SIZE + total + (records.len() + 1) * 2;
        if needed > node_size {
            return Err(Error::no_space(needed as u32, node_size as u64));
        }
        let mut node = vec![0u8; node_size];
        node[0..4].copy_from_slice(&f_link.to_be_bytes());
        node[4..8].copy_from_slice(&b_link.to_be_bytes());
        node[8] = kind;
        node[9] = height;
        let mut at = NODE_DESCRIPTOR_SIZE;
        for (i, rec) in records.iter().enumerate() {
            node[at..at + rec.len()].copy_from_slice(rec);
            write_offset(&mut node, i, at)?;
            at += rec.len();
        }
        write_offset(&mut node, records.len(), at)?;
        set_record_count(&mut node, records.len() as u16)?;
        Ok(node)
    }

    /// An index record: the key, then the child.
    ///
    /// `record` is a whole leaf record, and only its *key* is copied -- the bytes
    /// up to and including the key. Copying the body too produces an index record
    /// whose child is not where a reader looks for it: `GetChildNodeNum` reads the
    /// child `CalcKeySize` bytes past the record's start, which is immediately
    /// after the key, so the first four bytes of the *body* are read as the child.
    /// On a catalog that is the file record's type and flags word, and fsck reports
    /// the result as an invalid index link -- while a reader that happens to take
    /// the child from the end of the record works perfectly, which is what makes
    /// it so easy to ship.
    fn index_record(record: &[u8], child: u32) -> Result<Vec<u8>> {
        let key_len = usize::from(u16::from_be_bytes([record[0], record[1]]));
        let end = 2usize.checked_add(key_len).ok_or(Error::Truncated {
            what: "catalog key",
            needed: key_len + 2,
            available: record.len(),
        })?;
        let mut rec = record
            .get(..end)
            .ok_or(Error::Truncated {
                what: "catalog key",
                needed: end,
                available: record.len(),
            })?
            .to_vec();
        rec.extend_from_slice(&child.to_be_bytes());
        Ok(rec)
    }

    /// Record `index`'s bytes.
    ///
    /// Bounds-checked rather than sliced: a node with no records has no slot 1 to
    /// read an end offset from, and the slot that is there belongs to whatever was
    /// in the node before -- which can be *larger*, giving an inverted range. That
    /// is a panic, and a panic on an untrusted image is the one thing this crate
    /// must never do.
    fn record_at(node: &[u8], index: usize) -> Result<Vec<u8>> {
        let count = usize::from(crate::btree::node::num_records(node)?);
        if index >= count {
            return Err(Error::invalid(
                "btree node record",
                format!("record {index} of a node holding {count}"),
            ));
        }
        let start = crate::btree::node::read_offset(node, index)?;
        let end = crate::btree::node::read_offset(node, index + 1)?;
        let bytes = node.get(start..end).ok_or(Error::Truncated {
            what: "btree node record",
            needed: end,
            available: node.len(),
        })?;
        Ok(bytes.to_vec())
    }

    /// The index at which to divide a leaf's records, by bytes.
    fn split_point(node: &[u8], count: usize, node_size: usize) -> Result<usize> {
        let mut used = 0usize;
        let mut split = 0usize;
        for i in 0..count {
            let start = crate::btree::node::read_offset(node, i)?;
            let end = crate::btree::node::read_offset(node, i + 1)?;
            used += end - start;
            split = i + 1;
            if used.saturating_mul(2) >= node_size {
                break;
            }
        }
        // Both halves non-empty, and never a no-op.
        Ok(split.clamp(1, count.saturating_sub(1).max(1)))
    }

    /// Divide a leaf's records at `split_at`, putting `record` into whichever half
    /// its insertion index falls in.
    fn divide(
        node: &[u8],
        split_at: usize,
        record: &[u8],
        at: u16,
    ) -> Result<(SplitHalf, SplitHalf)> {
        let count = usize::from(crate::btree::node::num_records(node)?);
        let mut lower = Vec::new();
        let mut upper = Vec::new();
        for i in 0..count {
            let rec = Self::record_at(node, i)?;
            if i < split_at {
                lower.push(rec);
            } else {
                upper.push(rec);
            }
        }
        let target = usize::from(at);
        if target <= split_at {
            lower.insert(target.min(lower.len()), record.to_vec());
        } else {
            upper.insert(
                target.saturating_sub(split_at).min(upper.len()),
                record.to_vec(),
            );
        }
        // A split must leave *both* halves non-empty. With one record to divide --
        // a leaf holding a single record, which a leaf holding only a folder and
        // its thread does -- `split_at` lands such that one half would take nothing
        // but the new record's neighbour, and a leaf with no records has no offset
        // slot to read, so the tree's own descent cannot find it. One record moves
        // across to keep the invariant.
        if upper.is_empty() && lower.len() > 1 {
            upper.push(lower.pop().expect("lower is not empty"));
        } else if lower.is_empty() && upper.len() > 1 {
            lower.push(upper.remove(0));
        }
        Ok((SplitHalf(lower), SplitHalf(upper)))
    }

    /// Read a `u16` field of the catalog's B-tree header record.
    fn btree_header_u16(&self, offset: u64) -> Result<u16> {
        let node = self.read_catalog_node(0)?;
        let at = crate::btree::header::HEADER_RECORD_OFFSET + offset as usize;
        let bytes = node.get(at..at + 2).ok_or(Error::Truncated {
            what: "BTHeaderRec",
            needed: at + 2,
            available: node.len(),
        })?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Write a `u16` field of the catalog's B-tree header record.
    fn write_btree_header_u16(&mut self, offset: u64, value: u16) -> Result<()> {
        use crate::btree::io::BTreeFile;
        let at = {
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            bt.node_offset(0)? + crate::btree::header::HEADER_RECORD_OFFSET as u64
        };
        self.journal_write(at + offset, &value.to_be_bytes())
    }

    /// Copy the volume header to the copy at the end of the volume.
    ///
    /// HFS+ keeps a second, identical volume header in the last 1024 bytes of the
    /// volume, and `fsck.hfsplus` compares the two: a primary header that has moved
    /// on without it reports "Volume header needs minor repair", and the repair
    /// rewrites *this* copy from the primary. That repair is right and it is also a
    /// repair, so a volume this crate produced would come back from `fsck` having
    /// been modified -- which is the one thing a test asserting `fsck` accepts an
    /// image cannot allow.
    ///
    /// Apple writes it in the same transaction as the primary, as part of an
    /// unmount or a header update; a library with no unmount to hang it on has to
    /// write it whenever the header changes.
    ///
    /// **It does not have to, and TN1150 says it should not.** "The implementation
    /// should only update this copy when the length or location of one of the
    /// special files changes." The copy exists so that a repair utility finding the
    /// primary header unusable can still learn where the special files are; a count
    /// that has moved is not that kind of information, and the primary holds it.
    ///
    /// So this is called only where a fork's extents, `totalBlocks` or
    /// `logicalSize` change, and *not* from `write_header_u32`. It was originally
    /// called from there, because a catalog that grew left the two headers
    /// describing different forks and `fsck.hfsplus` reported "Volume header needs
    /// minor repair" -- so the obvious conclusion was to sync on everything.
    ///
    /// That conclusion was drawn from a misreading, and is worth correcting here
    /// rather than quietly. The experiment:
    ///
    /// - Both headers identical, **primary**'s `freeBlocks` off by one from the
    ///   bitmap: fsck reports "Invalid volume free block count" and repairs the
    ///   primary.
    /// - Both headers identical, **backup**'s `freeBlocks` off by one from the
    ///   bitmap: fsck reports **"appears to be OK"** and changes nothing.
    ///
    /// So `lib_fsck_hfs` validates the *primary* against the allocation bitmap and
    /// does not compare the two headers' counts at all. The backup can therefore
    /// hold a stale `freeBlocks` indefinitely, exactly as TN1150 says it may, and
    /// the cost of syncing on every header write -- a kilobyte read and a kilobyte
    /// write for every `nextCatalogID` bump -- buys nothing.
    ///
    /// Mining reference: `core/hfs_vfsops.c` writes the "alternate volume header
    /// located at 1024 bytes before end of the partition"; TN1150, Volume Header,
    /// for when it should be written and that it is "intended for use solely by disk
    /// repair utilities".
    fn sync_backup_header(&mut self) -> Result<()> {
        let volume_bytes = u64::from(self.header.total_blocks) * u64::from(self.header.block_size);
        if volume_bytes <= 1024 {
            // A volume that cannot hold both headers has no backup to keep in step,
            // and this is not a volume any formatter produces.
            return Ok(());
        }
        // The primary header is at byte 1024, not at 0 -- the first kilobyte of an
        // HFS+ volume is reserved, and the boot blocks live there.
        let mut buf = [0u8; 1024];
        self.device
            .read_at(crate::blockdev::VOLUME_HEADER_OFFSET, &mut buf)?;
        self.journal_write(volume_bytes - 1024, &buf)
    }

    /// Read a `u32` field of the catalog's B-tree header record.
    fn btree_header_u32(&self, offset: u64) -> Result<u32> {
        let node = self.read_catalog_node(0)?;
        let at = crate::btree::header::HEADER_RECORD_OFFSET + offset as usize;
        let bytes = node.get(at..at + 4).ok_or(Error::Truncated {
            what: "BTHeaderRec",
            needed: at + 4,
            available: node.len(),
        })?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// The catalog B-tree's node size, from the tree header rather than recomputed.
    fn catalog_node_size(&self) -> Result<usize> {
        use crate::btree::io::BTreeFile;
        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        Ok(bt.node_size())
    }

    /// Write one catalog node's bytes.
    fn write_catalog_node(&mut self, node_num: u32, bytes: &[u8]) -> Result<()> {
        use crate::btree::io::BTreeFile;
        let at = {
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            bt.node_offset(node_num)?
        };
        self.journal_write(at, bytes)
    }

    /// Locate any catalog record whose body names `cnid` in its `fileID`/`folderID`.
    ///
    /// File and folder records both carry the object's CNID in their body at the
    /// same offset -- 8..12 -- so one walk finds either, and the record's length
    /// says which. Thread records do not: they carry the *parent's* CNID in their
    /// body and the object's in their key, so they are matched by key instead and
    /// handled separately.
    fn find_catalog_record_any(&self, cnid: u32) -> Result<Option<Vec<u8>>> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::NodeKind;
        use crate::catalog::lookup::split_record;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let btree_header = *bt.header();
        let mut node_num = btree_header.first_leaf_node;
        let mut budget = btree_header.total_nodes;

        while budget > 0 && node_num != 0 {
            budget -= 1;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }
            for index in 0..node.num_records() {
                let Some((_key, body)) = split_record(node.record(index)?) else {
                    continue;
                };
                if body.len() < 12 {
                    continue;
                }
                let id = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
                if id == cnid
                    && (body.len() == crate::catalog::record::FILE_RECORD_SIZE
                        || body.len() == crate::catalog::record::FOLDER_RECORD_SIZE)
                {
                    return Ok(Some(body.to_vec()));
                }
            }
            if node_num == btree_header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
            if node_num == 0 || node_num >= btree_header.total_nodes {
                break;
            }
        }
        Ok(None)
    }

    /// Insert a catalog record at its sorted position.
    ///
    /// The key order is the catalog's, so this is a binary search for the first key
    /// greater than the one being inserted and an insertion there. A leaf with no
    /// room is refused by name: splitting means redistributing records across two
    /// nodes and updating the parent's index, and a node that grew past its
    /// allocated blocks would need allocation as well.
    ///
    /// Mining reference: the search half is the same descent a reader already
    /// performs; the insert half is `BTInsertRecord` into `core/BTree.c`.
    fn insert_catalog_record(&mut self, record: &[u8]) -> Result<()> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::{insert_record, NodeKind};
        use crate::catalog::key::CatalogKey;
        use crate::catalog::lookup::split_record;
        use crate::unicode::Ordering;

        // Everything that reads the device happens inside this one scope, because
        // `BTreeFile` and `Catalog` both borrow it and the write below needs it
        // mutably. The only things that cross the boundary are owned values.
        let (at_node_offset, patched, first_changed) = {
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            let btree_header = *bt.header();
            // The leaf the key belongs to, not necessarily the first: with an index
            // node above the leaves they stop being interchangeable.
            let node_num = self.leaf_for(record, btree_header.first_leaf_node)?;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                return Err(Error::invalid(
                    "catalog",
                    "the catalog's first node is not a leaf, which no HFS+ volume has",
                ));
            }

            // The first key greater than or equal to the one being inserted. An
            // equal key means the name is taken, and inserting beside it would
            // leave a leaf with two records under one key -- which still parses,
            // still searches, and answers with whichever comes first, forever.
            let incoming =
                CatalogKey::from_record(record, usize::from(btree_header.max_key_length))
                    .map_err(|e| Error::invalid("catalog key", e.to_string()))?;
            let mut at = node.num_records();
            let mut duplicate = false;
            {
                let catalog = crate::catalog::lookup::Catalog::open(
                    &*self.device,
                    &self.header.catalog_file,
                    self.header.block_size,
                    self.header.is_hfsx(),
                )?;
                for index in 0..node.num_records() {
                    let Some((existing, _)) = split_record(node.record(index)?) else {
                        continue;
                    };
                    match catalog.compare_keys(&existing, &incoming) {
                        Ordering::Less => continue,
                        Ordering::Equal => {
                            duplicate = true;
                            at = index;
                            break;
                        }
                        Ordering::Greater => {
                            at = index;
                            break;
                        }
                    }
                }
            }
            if duplicate {
                return Err(Error::invalid(
                    "catalog",
                    format!(
                        "CNID {} already has a child named {:?}",
                        incoming.parent_id.0,
                        incoming.name_string()
                    ),
                ));
            }

            let mut buf = bytes.clone();
            let fits = insert_record(&mut buf, usize::from(at), record).is_ok();
            // Whether the insertion moved this leaf's first record, which is the
            // only thing an in-place insert can change that the index cares about.
            let first_changed =
                !fits || Self::record_at(&bytes, 0).ok() != Self::record_at(&buf, 0).ok();
            (
                bt.node_offset(node_num)?,
                if fits { Some(buf) } else { None },
                first_changed,
            )
        };
        // The index the record would have taken, needed only by the split, which
        // divides the records itself and puts `record` in whichever half this falls
        // in. Computed outside the scope above because that scope holds the only
        // borrow of the device that lets the split allocate nodes.
        let at = if patched.is_some() {
            0
        } else {
            self.insert_index(record)?
        };
        match patched {
            Some(buf) => {
                // Did this insertion change the leaf's *first* record? If so, the
                // index separator pointing at this leaf is now stale, and it has to
                // be refreshed even though nothing split.
                //
                // A separator is the first key of the subtree it points at, so an
                // in-place insert at the front of a leaf moves it without any node
                // changing hands. Nothing else notices: the stale separator is still
                // a valid key, still in order relative to its neighbours, and the
                // keys it now bounds still live in that leaf -- until one of them
                // does not, at which point `fsck.hfsplus` reports "Invalid index
                // key" on a tree whose every other property is right.
                self.journal_write(at_node_offset, &buf)?;
                if first_changed {
                    self.refresh_index()?;
                }
                // `leafRecords` in the B-tree header counts the records in *all*
                // leaf nodes. Leaving it alone is not a cosmetic omission: fsck
                // counts the records it finds and reports "Invalid leaf record
                // count" when the header disagrees, and every other implementation
                // reading this tree uses the field to size its leaf-node map.
                self.write_btree_header_u32(
                    crate::btree::header::LEAF_RECORDS_OFFSET,
                    self.btree_header_leaf_records()? + 1,
                )
            }
            // The split writes the leaf itself, new record included.
            None => {
                self.split_leaf_and_insert(record, at)?;
                // `leafRecords` counts records across *all* leaves, and the record
                // that caused the split is one of them. A split redistributes what
                // is already there -- that part changes no count -- but the record
                // that triggered it is new, so the header still advances by one.
                // Forgetting this leaves the count short by one per split, which
                // fsck reports as "Invalid leaf record count" and which no reader
                // would notice.
                self.write_btree_header_u32(
                    crate::btree::header::LEAF_RECORDS_OFFSET,
                    self.btree_header_leaf_records()? + 1,
                )
            }
        }
    }

    /// Grow the catalog so it holds at least `want_nodes` nodes.
    ///
    /// The only way a B-tree gets bigger, and it is two separate things that are
    /// easy to confuse:
    ///
    /// 1. **The file gets more blocks**, which is ordinary fork extension: allocate
    ///    contiguously, append an extent, and raise the fork's `logicalSize` and
    ///    `totalBlocks` in the volume header. The catalog's extents live *there*,
    ///    not in the catalog, so every one of those is a volume-header write.
    /// 2. **The node map gets more records**, which is arithmetic on the header
    ///    node. A map record covers `record_length * 8` nodes, and once `totalNodes`
    ///    passes what the existing records cover, the new nodes need map records of
    ///    their own -- nodes that describe the node map, allocated from the node
    ///    map. The map records are therefore written *and then* their own bits are
    ///    marked, in that order, because the bit for a new map node lives in a map
    ///    record that did not exist when the node did.
    ///
    /// # The growth size
    ///
    /// A request smaller than the fork's clump size is raised to it, and the whole
    /// is rounded up to a multiple of the node size. So a tree needing one more node
    /// grows by eight, and the volume pays for a clump whether it needed one node or
    /// thirty-two. That is Apple's behaviour and it is worth keeping: growth
    /// infrequent and large is what keeps the catalog from needing a map record
    /// every few files.
    ///
    /// Mining reference: `core/BTreeAllocate.c` `ExtendBTree` for the map
    /// arithmetic (`mapNodeRecSize = nodeSize - sizeof(BTNodeDescriptor) - 6`, the
    /// new map nodes numbered from `oldTotalNodes` and chained by `fLink`), and
    /// `core/hfs_btreeio.c` `ExtendBTreeFile` for the file half -- `bytesToAdd` is
    /// raised to `ff_clumpsize`, and the allocation is contiguous
    /// (`kEFContigMask | kEFMetadataMask | kEFNoClumpMask`), retried from
    /// `vcb->nextAllocation`.
    fn grow_catalog(&mut self, want_nodes: u32) -> Result<()> {
        use crate::btree::header::{FREE_NODES_OFFSET, TOTAL_NODES_OFFSET};
        use crate::format::extents::ExtentDescriptor;

        let node_size = self.catalog_node_size()? as u64;
        let clump = u64::from(self.header.catalog_file.clump_size.max(1));
        let old_total = self.btree_header_u32(TOTAL_NODES_OFFSET)?;
        let want_nodes = want_nodes.max(old_total + 1);
        let eof = self.header.catalog_file.logical_size;
        let min_eof = u64::from(want_nodes) * node_size;
        if eof >= min_eof {
            return Ok(());
        }

        // Raised to the clump, then rounded up to whole nodes.
        let mut bytes_to_add = min_eof - eof;
        if bytes_to_add < clump {
            bytes_to_add = clump;
        }
        // `div_ceil` is 1.73 and this crate is 1.70.
        bytes_to_add = (bytes_to_add + node_size - 1) / node_size * node_size;

        // Blocks needed, and where they go.
        let blocks_needed = u32::try_from(bytes_to_add / node_size)
            .map_err(|_| Error::overflow("catalog fork block count"))?;
        let mut map = self.load_allocation_map()?;
        let hint = self.header.next_allocation.max(1);

        // Contiguous first, as `kEFContigMask` asks: a node is read with one I/O, so
        // a tree whose nodes are scattered pays for it on every read afterwards.
        // `reserve` searches from the hint and then from the first allocatable
        // block, so it covers the whole range exactly once, and it returns the whole
        // run or an error. A B-tree cannot be extended by part of a node, which is
        // why the short-run case is not a result here.
        let start = map.reserve(hint, blocks_needed)?;
        let count = blocks_needed;

        // The catalog's extents live in the volume header, so extending the fork is
        // a header write: append the extent, then raise the two counts.
        let mut fork = self.header.catalog_file;
        let slot = fork.extents.next_free().ok_or_else(|| {
            Error::invalid(
                "catalogFile",
                "the catalog fork has no free inline extent; growing it past eight \
                 extents needs the extents B-tree, which is not implemented",
            )
        })?;
        fork.extents.set(
            slot,
            ExtentDescriptor {
                start_block: start,
                block_count: count,
            },
        )?;
        fork.total_blocks += count;
        fork.logical_size += u64::from(count) * node_size;

        let free_blocks = self
            .header
            .free_blocks
            .checked_sub(count)
            .ok_or_else(|| Error::overflow("volume_header.freeBlocks"))?;
        // Bitmap and free count first: a crash in between leaves the fork claiming
        // blocks the bitmap still calls free, which is the one ordering that lets
        // two things share them.
        self.write_allocation_bitmap(&map, free_blocks)?;
        self.write_catalog_fork(&fork, free_blocks)?;
        // The in-memory header moves with the on-disk one *here*, not at the end of
        // this method. Everything below re-opens the B-tree through
        // `self.header.catalog_file`, and the volume header is the only place that
        // fork's length is recorded -- so a stale copy here pairs the new
        // `totalNodes` with the old `logicalSize` and the tree refuses to open.
        self.header.catalog_file = fork;

        let new_total = u32::try_from(fork.logical_size / node_size).unwrap_or(old_total + count);
        let new_map_nodes = self.extend_node_map(old_total, new_total)?;
        let added = new_total - old_total;
        self.write_btree_header_u32(TOTAL_NODES_OFFSET, new_total)?;
        self.write_btree_header_u32(
            FREE_NODES_OFFSET,
            self.btree_header_u32(FREE_NODES_OFFSET)? + added - new_map_nodes,
        )?;

        self.header.free_blocks = free_blocks;
        Ok(())
    }

    /// Add map records to cover `new_total` nodes, and return how many it took.
    ///
    /// Each new map node is one node of the tree, so it is numbered from
    /// `old_total` and costs a node as well as a record.
    fn extend_node_map(&mut self, old_total: u32, new_total: u32) -> Result<u32> {
        use crate::btree::node::{set_record_count, write_offset, NODE_DESCRIPTOR_SIZE};

        let node_size = self.catalog_node_size()?;
        let map_rec_size = node_size
            .checked_sub(NODE_DESCRIPTOR_SIZE + 6)
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                Error::invalid(
                    "BTHeaderRec.nodeSize",
                    format!("{node_size} is too small for a node map record"),
                )
            })?;

        // How many nodes the existing records already describe. The header node
        // holds the first of them at record index 2; any further map nodes are
        // chained by the header node's `fLink`.
        let header = self.read_catalog_node(0)?;
        let header_records = crate::btree::node::num_records(&header)? as usize;
        let mut total_bits = 0u64;
        let mut cursor = header[0..4]
            .iter()
            .fold(0u32, |a, b| (a << 8) | u32::from(*b));
        let mut budget = 64u32;
        loop {
            let map_len = map_record_len(&header, 2, header_records)?;
            total_bits += u64::from(map_len as u32) * 8;
            if cursor == 0 || budget == 0 {
                break;
            }
            budget -= 1;
            let node = self.read_catalog_node(cursor)?;
            let records = crate::btree::node::num_records(&node)? as usize;
            map_rec_size_check(node_size);
            total_bits += u64::from(map_record_len(&node, 0, records)? as u32) * 8;
            cursor = u32::from_be_bytes([node[0], node[1], node[2], node[3]]);
        }
        if u64::from(new_total) <= total_bits {
            return Ok(0);
        }

        let extra_bits = u64::from(new_total - old_total);
        let new_map_nodes = u32::try_from((extra_bits >> 3) / map_rec_size as u64 + 1)
            .map_err(|_| Error::overflow("node map count"))?;

        // Write the map nodes, chained. The last one's `fLink` is zero, and the
        // previous map node -- the header node, when there is only one map record --
        // points at the first of them.
        for i in 0..new_map_nodes {
            let node_num = old_total + i;
            let mut node = vec![0u8; node_size];
            node[0..4].copy_from_slice(&(old_total + i + 1).to_be_bytes());
            if i == new_map_nodes - 1 {
                node[0..4].copy_from_slice(&0u32.to_be_bytes());
            }
            node[8] = 0x02; // kBTMapNode
            write_offset(&mut node, 0, NODE_DESCRIPTOR_SIZE)?;
            // The free-space offset Apple writes at `nodeSize - 4`: a map node has
            // no records after the map itself, and the offset array holds two
            // entries for the single record and the free offset.
            write_offset(&mut node, 1, node_size - 6)?;
            set_record_count(&mut node, 1)?;
            self.write_catalog_node(node_num, &node)?;
        }
        // Chain the last existing map node to the first new one.
        let last_existing = if header[0..4]
            .iter()
            .fold(0u32, |a, b| (a << 8) | u32::from(*b))
            == 0
        {
            0
        } else {
            // Walk to the end of the existing chain.
            let mut cursor = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let mut budget = 64u32;
            while budget > 0 && cursor != 0 {
                budget -= 1;
                let node = self.read_catalog_node(cursor)?;
                let next = u32::from_be_bytes([node[0], node[1], node[2], node[3]]);
                if next == 0 {
                    break;
                }
                cursor = next;
            }
            cursor
        };
        if last_existing == 0 {
            let mut h = self.read_catalog_node(0)?;
            h[0..4].copy_from_slice(&old_total.to_be_bytes());
            self.write_catalog_node(0, &h)?;
        } else {
            let mut node = self.read_catalog_node(last_existing)?;
            node[0..4].copy_from_slice(&old_total.to_be_bytes());
            self.write_catalog_node(last_existing, &node)?;
        }

        // Mark each new map node's own bit. They must be marked *after* they are
        // written: the bit for a new map node lives in a map record that did not
        // exist when the node did.
        for i in 0..new_map_nodes {
            let node_num = old_total + i;
            let header = self.read_catalog_node(0)?;
            let bits_covered = map_record_len(&header, 2, header_records)? * 8;
            if (node_num as usize) < bits_covered {
                // Inside a record the header node already holds.
                let mut h = self.read_catalog_node(0)?;
                crate::btree::header::set_map_bit(&mut h, 2, node_num)?;
                self.write_catalog_node(0, &h)?;
            } else {
                // Beyond it: the bit lives in one of the map nodes just written.
                let first_new = old_total;
                let into = node_num - first_new;
                let per_record = map_rec_size * 8;
                let which = (into / per_record as u32) as usize;
                let node_num_of_record = first_new + which as u32;
                let bit_in_record = into % per_record as u32;
                let mut node = self.read_catalog_node(node_num_of_record)?;
                crate::btree::header::set_map_bit(&mut node, 0, bit_in_record)?;
                self.write_catalog_node(node_num_of_record, &node)?;
            }
        }
        Ok(new_map_nodes)
    }

    /// Write the catalog fork and the volume header's free count.
    fn write_catalog_fork(
        &mut self,
        fork: &crate::format::fork::ForkData,
        free_blocks: u32,
    ) -> Result<()> {
        let at = crate::blockdev::VOLUME_HEADER_OFFSET + 112 + 80 * 2; // the catalog is the third of the five forks
        self.journal_write(at, &fork.to_bytes())?;
        // A fork's length and location changed, which is the one thing the alternate
        // header exists to record. See `sync_backup_header`.
        self.sync_backup_header()?;
        self.write_header_u32(48, free_blocks)
    }

    /// Rebuild the index from the leaf chain.
    ///
    /// Cheap, and deliberately the blunt instrument: the chain is the truth about
    /// which keys each leaf holds, and an index built from it cannot disagree. The
    /// alternative -- finding the one separator that moved and rewriting it in
    /// place -- needs the same walk to find it, plus a length-changing record
    /// replacement underneath, since the new first record's key need not be the
    /// same length as the old one's.
    ///
    /// Does nothing while the tree is one level deep, where there is no index to
    /// refresh: the leaf is the root.
    fn refresh_index(&mut self) -> Result<()> {
        let tree_depth = self.btree_header_u16(crate::btree::header::TREE_DEPTH_OFFSET)?;
        if tree_depth < 2 {
            return Ok(());
        }
        let root_node = self.btree_header_u32(crate::btree::header::ROOT_NODE_OFFSET)?;
        let node_size = self.catalog_node_size()?;
        let records = self.index_records_for_chain()?;
        let root = self.read_catalog_node(root_node)?;
        let node = Self::build_node(node_size, 0x00, root[9], 0, 0, &records)?;
        self.write_catalog_node(root_node, &node)
    }

    /// The leaf that should receive `record`.
    ///
    /// The *first* leaf only while there is one leaf. With an index node above
    /// them, leaves stop being interchangeable: inserting into the first leaf puts
    /// a key where it does not belong, and the damage shows later rather than now
    /// -- the leaf still searches, still returns a record for some other name, and
    /// the next split divides it into halves whose keys *straddle* another leaf's.
    ///
    /// So the tree is descended rather than guessed at, and the descent uses the
    /// same rule as the reader's -- `Catalog::descend_index`: the child for a key
    /// is the **first** index record whose key is greater than or equal to it.
    ///
    /// # Which way round that is
    ///
    /// An HFS+ index record holds the *first key of the subtree it points at*, not
    /// the last key of the subtree before it. So the child for `K` is the first
    /// record with key >= K -- a lower bound. Reaching for the greatest key <= K
    /// instead, which is the other plausible reading, sends every key to the last
    /// leaf whose first key does not exceed it: keys accumulate in the wrong leaves,
    /// the leaves stop being contiguous ranges, and a split of a leaf produces two
    /// halves whose keys interleave with another leaf's. The tree still searches,
    /// and still returns a record for some other name.
    ///
    /// The comparison is the tree's own comparator, because a byte comparison would
    /// put `file10` before `file9` on a case-insensitive volume.
    fn leaf_for(&self, record: &[u8], fallback_leaf: u32) -> Result<u32> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::NodeKind;
        use crate::catalog::key::CatalogKey;
        use crate::catalog::lookup::split_record;
        use crate::unicode::Ordering;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let btree_header = *bt.header();
        let max_key = usize::from(btree_header.max_key_length);
        let incoming = CatalogKey::from_record(record, max_key)
            .map_err(|e| Error::invalid("catalog key", e.to_string()))?;
        let catalog = crate::catalog::lookup::Catalog::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;

        let mut node_num = btree_header.root_node;
        // Bounded by the node count, so a cycle in a corrupted tree terminates.
        let mut budget = btree_header.total_nodes;
        while budget > 0 && node_num != 0 {
            budget -= 1;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            match node.kind() {
                NodeKind::Leaf => return Ok(node_num),
                NodeKind::Header => return Ok(fallback_leaf),
                _ => {}
            }

            // Lower bound over the node's keys.
            let mut lo = 0u16;
            let mut hi = node.num_records();
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let Some((k, _)) = split_record(node.record(mid)?) else {
                    return Err(Error::invalid(
                        "catalog index record",
                        "key could not be decoded",
                    ));
                };
                // Upper bound, not a lower one: the child for a key is the record
                // whose key is the greatest not exceeding it.
                //
                // This was a lower bound while the reader's `descend_index` had
                // already been corrected to an upper one, and the two disagreed --
                // the reader right, the writer wrong. A key above the last
                // separator in a *parentID=2* leaf range then resolved to the leaf
                // holding the thread records, which sorts after every one of them:
                // the record went to a valid, correctly ordered leaf that simply was
                // not the one it belonged in, and the chain's leaves overlapped in
                // key space.
                if catalog.compare_keys(&k, &incoming) == Ordering::Greater {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            // A key greater than every index key belongs to the child of the *last*
            // index record, not to the first leaf. That case is not exotic: it is
            // every key above the largest one in the tree when the last leaf has
            // room, which is the common case while a tree is growing. Falling back
            // to `firstLeafNode` there puts those keys in the first leaf, and since
            // they sort above the first leaf's own keys the leaf stops being a
            // contiguous range -- so the next split cuts it into halves that
            // interleave with another leaf's, and the chain is no longer in key
            // order.
            if node.num_records() == 0 {
                return Ok(fallback_leaf);
            }
            // `lo` is now the number of separators not greater than the key, so the
            // answer is one before it -- the *upper* bound's answer. Clamping it to
            // `num_records - 1` instead is the lower bound's fallback, and mixing
            // the two sends any key above the last separator to the last separator's
            // leaf, which is the leaf holding the thread records.
            let lo = lo.saturating_sub(1);
            let body = match split_record(node.record(lo)?) {
                Some((_, body)) => body,
                None => return Ok(fallback_leaf),
            };
            if body.len() < 4 {
                return Err(Error::invalid(
                    "catalog index record",
                    format!(
                        "index record body is {} bytes, too short for a child",
                        body.len()
                    ),
                ));
            }
            node_num = u32::from_be_bytes([
                body[body.len() - 4],
                body[body.len() - 3],
                body[body.len() - 2],
                body[body.len() - 1],
            ]);
        }
        Ok(fallback_leaf)
    }

    /// The index at which `record` would be inserted into the leaf it belongs to.
    fn insert_index(&self, record: &[u8]) -> Result<u16> {
        use crate::btree::io::BTreeFile;
        use crate::catalog::key::CatalogKey;
        use crate::catalog::lookup::split_record;
        use crate::unicode::Ordering;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let max_key = usize::from(bt.header().max_key_length);
        let leaf = self.leaf_for(record, bt.header().first_leaf_node)?;
        let bytes = bt.read_node_bytes(leaf)?;
        let node = bt.parse_node(&bytes)?;
        let incoming = CatalogKey::from_record(record, max_key)
            .map_err(|e| Error::invalid("catalog key", e.to_string()))?;
        let catalog = crate::catalog::lookup::Catalog::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        for index in 0..node.num_records() {
            let Some((existing, _)) = split_record(node.record(index)?) else {
                continue;
            };
            if catalog.compare_keys(&existing, &incoming) != Ordering::Less {
                return Ok(index);
            }
        }
        Ok(node.num_records())
    }

    /// Read `leafRecords` from the catalog's B-tree header.
    fn btree_header_leaf_records(&self) -> Result<u32> {
        use crate::btree::io::BTreeFile;
        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        Ok(bt.header().leaf_records)
    }

    /// Write one `u32` field of the catalog's B-tree header.
    ///
    /// `offset` is relative to the `HFSBTreeHeader` record, which begins *after*
    /// the node descriptor at offset 14 of node 0. Getting that wrong is not a
    /// subtle corruption: offset 8 of node 0 is the descriptor's `kind` and
    /// `height`, so writing `leafRecords` there turns the header node into
    /// something that no longer parses as a node.
    fn write_btree_header_u32(&mut self, offset: u64, value: u32) -> Result<()> {
        use crate::btree::io::BTreeFile;
        let at = {
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            bt.node_offset(0)? + NODE_DESCRIPTOR_SIZE as u64
        };
        self.journal_write(at + offset, &value.to_be_bytes())
    }

    /// Replace the record whose body names `cnid`, whatever its type.
    ///
    /// File and folder records both carry their object's CNID at the same offset in
    /// the body, so one walk finds either and the caller supplies the bytes. The
    /// replacement must still be the same length as what it replaces; see
    /// [`Self::replace_catalog_record`].
    fn replace_catalog_body(&mut self, cnid: u32, body: &[u8]) -> Result<()> {
        let (node_num, node_at, offset, len) = match self.find_catalog_body(cnid)? {
            Some(hit) => hit,
            None => {
                return Err(Error::NotFound {
                    what: "catalog record",
                })
            }
        };
        if len != body.len() {
            return Err(Error::invalid(
                "write",
                format!(
                    "the replacement record is {} bytes and the one it replaces is \
                     {len}; a length change moves every later record in the node",
                    body.len()
                ),
            ));
        }
        let patched = {
            let mut buf = self.read_catalog_node(node_num)?;
            if buf.len() < offset + len {
                return Err(Error::out_of_range(
                    "catalog record end offset",
                    (offset + len) as u64,
                    buf.len() as u64,
                ));
            }
            buf[offset..offset + len].copy_from_slice(body);
            buf
        };
        // `node_at` comes from the extent mapper, not `node_num * node_size`: a
        // catalog that does not start at block 0 -- which is every volume this
        // crate generates, and most real ones -- would otherwise be written over
        // the allocation bitmap and the volume header.
        self.journal_write(node_at, &patched)
    }

    /// Locate the record whose body names `cnid`.
    ///
    /// Returns `(node number, the node's byte address, the record's offset within
    /// the node, the record's length)`. The address is from the extent mapper.
    fn find_catalog_body(&self, cnid: u32) -> Result<Option<(u32, u64, usize, usize)>> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::NodeKind;
        use crate::catalog::lookup::split_record;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let btree_header = *bt.header();
        let mut node_num = btree_header.first_leaf_node;
        let mut budget = btree_header.total_nodes;

        while budget > 0 && node_num != 0 {
            budget -= 1;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }
            for index in 0..node.num_records() {
                let Some((key, body)) = split_record(node.record(index)?) else {
                    continue;
                };
                if body.len() < 12 {
                    continue;
                }
                let id = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
                if id == cnid {
                    return Ok(Some((
                        node_num,
                        bt.node_offset(node_num)?,
                        node.record_offset(index)? + 2 + key.key_length,
                        body.len(),
                    )));
                }
            }
            if node_num == btree_header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
            if node_num == 0 || node_num >= btree_header.total_nodes {
                break;
            }
        }
        Ok(None)
    }

    /// Read one catalog node's bytes.
    fn read_catalog_node(&self, node_num: u32) -> Result<Vec<u8>> {
        use crate::btree::io::BTreeFile;
        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        bt.read_node_bytes(node_num)
    }

    /// Free a file's blocks beyond `new_len`, shrinking its allocation.
    ///
    /// Truncation in the sense Apple means it: the file keeps
    /// `howmany(new_len, blockSize)` blocks, and every block past that is released
    /// to the volume. Setting `logicalSize` alone is *not* truncation — a file
    /// whose length shrank while its blocks stayed allocated is a volume slowly
    /// filling up, which is the failure this method exists to prevent.
    ///
    /// # The rounding, and why it is up
    ///
    /// The new size is rounded **up** to a block boundary, so truncating 5000
    /// bytes leaves two blocks, not one. Truncating to 4097 therefore frees
    /// nothing, and a caller cannot use it to shave the tail of a partial block.
    /// That is Apple's behaviour and it follows from the format: a block is the
    /// unit of allocation, so the only sizes a file can have are multiples of it.
    ///
    /// # Zero is special
    ///
    /// A zero length frees *every* block, including the last extent. The
    /// `truncateToExtent` option means "round out to the end of the containing
    /// extent" and has no meaning at zero — there is no containing extent, and
    /// keeping one would leave a zero-length file holding storage.
    ///
    /// # Why this needs no B-tree mutation
    ///
    /// A freed extent is not removed from the record; its `startBlock` and
    /// `blockCount` are both set to zero, and a zeroed descriptor *is* the
    /// terminator. So the record keeps its length, which is the same property that
    /// lets [`Self::grow_fork`] append extents without touching the extents tree.
    /// The two are the same fact seen from opposite ends, and it is the reason
    /// allocation and deallocation are both testable before Milestone 8C.
    ///
    /// # Order of operations
    ///
    /// The bitmap is freed and flushed *before* the catalog record is rewritten —
    /// the same order as [`Self::grow_fork`], and the same reason. A crash in
    /// between leaves a file whose record claims fewer blocks than it has: the
    /// extra blocks are orphans, which is recoverable. The reverse order would
    /// leave a record claiming blocks the bitmap has handed to something else.
    ///
    /// Mining reference: `core/FileExtentMapping.c` `TruncateFileC`, which rounds
    /// with `howmany`, shortens the containing extent by
    /// `extentNextBlock - nextBlock`, zeroes the descriptors of every following
    /// extent, and takes the `peof == 0` path first.
    pub fn truncate_file(&mut self, cnid: u32, new_len: u64) -> Result<()> {
        self.begin_transaction()?;
        let result = self.truncate_file_inner(cnid, new_len);
        if result.is_ok() {
            self.end_transaction()?;
        } else {
            self.abandon_transaction();
        }
        result
    }

    fn truncate_file_inner(&mut self, cnid: u32, new_len: u64) -> Result<()> {
        let block_size = self.header.block_size;
        let mut record = self.read_file_record(cnid)?;
        let keep = (new_len as usize + block_size as usize - 1) / block_size as usize;

        // A request that both keeps every block *and* does not shorten the file
        // changes nothing at all, so it is refused rather than answered by
        // rewriting the record with the same numbers and moving the modification
        // time -- which would look like a successful truncate of nothing.
        //
        // Keeping the blocks while shortening the length is *not* refused: that is
        // how a file gives up the tail of its last partial block without releasing
        // it, and Apple allows it -- `TruncateFileC` shortens by
        // `extentNextBlock - nextBlock` blocks and writes the new length
        // regardless of whether that count was zero.
        if keep >= record.data_fork.total_blocks as usize
            && new_len >= record.data_fork.logical_size
        {
            return Err(Error::invalid(
                "truncate",
                format!(
                    "{new_len} bytes keeps all {} of the file's block(s) and does \
                     not shorten it; that is not a truncation",
                    record.data_fork.total_blocks
                ),
            ));
        }
        if keep == 0 {
            // Every block goes, and every descriptor is zeroed, so the record's
            // length is unchanged and there is nothing left to describe.
            self.release_blocks(&mut record, 0)?;
            record.data_fork.logical_size = 0;
        } else {
            self.release_blocks(&mut record, keep)?;
            record.data_fork.logical_size = new_len;
        }
        self.touch_record(&mut record)?;
        self.replace_catalog_record(cnid, &record)
    }

    /// Release every block of `record`'s data fork past `keep` blocks.
    ///
    /// Walks the descriptors rather than trusting `totalBlocks`, because the two
    /// can disagree on a damaged volume and the descriptors are what both the
    /// bitmap and the reader believe.
    ///
    /// Ranges are collected first and released afterwards, because the map has to
    /// be loaded from the device -- which needs a shared borrow -- and mutating it
    /// happens in memory. Releasing one range at a time would reload it per
    /// extent for no benefit: the writes are coalesced when the bitmap goes out.
    fn release_blocks(&mut self, record: &mut FileRecord, keep: usize) -> Result<()> {
        use crate::format::extents::{ExtentDescriptor, EMPTY_DESCRIPTOR};

        let mut file_block = 0usize;
        let mut freed = 0u32;
        let mut ranges: Vec<(u32, u32)> = Vec::new();

        let descriptors: Vec<(usize, u32, u32)> = record
            .data_fork
            .extents
            .raw
            .iter()
            .enumerate()
            .take_while(|(_, d)| !d.is_terminator())
            .map(|(i, d)| (i, d.start_block, d.block_count))
            .collect();

        for (slot, start_block, block_count) in descriptors {
            // How many of this descriptor's blocks lie before the new end.
            let keep_here = keep.saturating_sub(file_block);

            if keep_here == 0 {
                // Wholly past the new end: release all of it and zero the
                // descriptor, which is what makes it a terminator. Apple does
                // exactly this -- `startBlock = 0; blockCount = 0` -- rather than
                // compacting the record, because the record's length must not
                // change.
                ranges.push((start_block, block_count));
                freed += block_count;
                record.data_fork.extents.set(slot, EMPTY_DESCRIPTOR)?;
            } else if (keep_here as u32) < block_count {
                // Partly kept: release the tail and shorten. The start block is
                // unchanged, so the kept prefix stays where it was.
                let drop_from = start_block + keep_here as u32;
                let drop_count = block_count - keep_here as u32;
                ranges.push((drop_from, drop_count));
                freed += drop_count;
                record.data_fork.extents.set(
                    slot,
                    ExtentDescriptor {
                        start_block,
                        block_count: keep_here as u32,
                    },
                )?;
            }
            // Fully kept: leave the descriptor exactly as it is.
            file_block += block_count as usize;
        }

        record.data_fork.total_blocks = keep as u32;
        if freed == 0 {
            return Ok(());
        }

        let mut map = self.load_allocation_map()?;
        for (start, count) in ranges {
            map.release(start, count)?;
        }
        let free_blocks = self
            .header
            .free_blocks
            .checked_add(freed)
            .ok_or_else(|| Error::overflow("volume_header.freeBlocks"))?;
        self.write_allocation_bitmap(&map, free_blocks)
    }

    /// Read the allocation bitmap as a mutable map.
    fn load_allocation_map(&self) -> Result<crate::alloc::AllocationMap> {
        use crate::alloc::AllocationMap;
        let fork = &self.header.allocation_file;
        // The allocation file is read whole, using its own declared length: it is
        // a full allocation block on every volume, not merely the bytes the bitmap
        // needs. The same rule the checker uses, so the two cannot disagree about
        // which bits are set.
        let limit = usize::try_from(fork.logical_size).unwrap_or(1 << 20);
        let bytes = {
            let reader = ForkReader::new(&*self.device, fork, self.header.block_size);
            reader.read(0, limit)?
        };
        Ok(AllocationMap::from_bytes(&bytes, self.header.total_blocks)?
            .with_alloc_limit(self.header.total_blocks))
    }

    /// Set both modification timestamps from one clock read.
    fn touch_record(&mut self, record: &mut FileRecord) -> Result<()> {
        let now =
            crate::timestamp::now_hfs(self.header.has_expanded_times()).map_err(|e| Error::Io {
                message: e.to_string(),
            })?;
        record.content_mod_date = now;
        record.attribute_mod_date = now;
        Ok(())
    }

    /// Give `record`'s data fork `extra` more blocks, and put them on disk.
    ///
    /// Order of operations, and why each step is where it is:
    ///
    /// 1. **Check there is a free extent slot.** An `HFSPlusExtentRecord` is eight
    ///    fixed slots, and the ninth extent lives in the extents B-tree. Adding a
    ///    ninth is a B-tree insert, which is Milestone 8C; refusing here names that
    ///    rather than overflowing a fixed array.
    /// 2. **Check the header's free count against the bitmap.** Apple trusts
    ///    `freeBlocks` for its disk-full short-circuit without recounting, because
    ///    the kernel holds the mount and updates both together. There is no such
    ///    lock here and the image is untrusted, so a disagreement means the image is
    ///    damaged -- and allocating on top of it would destroy the evidence.
    /// 3. **Reserve**, then extend the extent record in memory.
    /// 4. **Flush the bitmap and the header's free count to disk**, before the data
    ///    blocks and before the catalog record.
    ///
    /// Step 4 is where it has to be. A crash after it leaves blocks that are
    /// marked allocated but that nothing references -- orphans, which
    /// [`crate::check`] reports and `fsck.hfsplus` reclaims. The reverse order
    /// leaves a catalog record pointing at blocks the bitmap calls free, so a later
    /// writer could hand the same blocks to a second file. Leaking space is
    /// recoverable; two files sharing blocks is not.
    fn grow_fork(&mut self, record: &mut FileRecord, extra: usize) -> Result<()> {
        use crate::alloc::AllocationMap;
        use crate::format::extents::ExtentDescriptor;

        if record.data_fork.extents.next_free().is_none() {
            return Err(Error::invalid(
                "write",
                format!(
                    "the file's data fork already uses all {} inline extents; a \
                     ninth extent has to go in the extents B-tree, which is not \
                     implemented",
                    record.data_fork.extents.raw.len()
                ),
            ));
        }

        // The allocation file is read whole, using its own declared length: it is a
        // full allocation block on every volume, not merely the bytes the bitmap
        // needs. Same as the checker reads it, so the two cannot disagree about
        // which bits are set.
        let fork = &self.header.allocation_file;
        let limit = usize::try_from(fork.logical_size).unwrap_or(1 << 20);
        let bytes = {
            let reader = ForkReader::new(&*self.device, fork, self.header.block_size);
            reader.read(0, limit)?
        };
        let mut map = AllocationMap::from_bytes(&bytes, self.header.total_blocks)?
            .with_alloc_limit(self.header.total_blocks);

        // Apple's disk-full short-circuit, and the check that makes it safe to
        // trust: the header's count must agree with the bitmap before it is used.
        let declared = self.header.free_blocks;
        let counted = self
            .header
            .total_blocks
            .saturating_sub(map.count_allocated().min(u64::from(u32::MAX)) as u32);
        if counted != declared {
            return Err(Error::invalid(
                "volume_header.freeBlocks",
                format!(
                    "the header says {declared} free block(s) but the allocation \
                     bitmap has {counted}; refusing to allocate on a volume whose \
                     free count and bitmap disagree"
                ),
            ));
        }

        // Ask for space adjacent to the file's last extent. That is the whole
        // reason a growing file gets one extent instead of eight: the allocator is
        // told where the file already is, so first fit finds the block just after
        // it rather than the first gap anywhere on the volume.
        let hint = record
            .data_fork
            .extents
            .iter()
            .last()
            .map_or(1, |e| e.end_block().map_or(1, |end| end as u32 + 1));
        let start = map.reserve(hint, extra as u32)?;

        let slot = record
            .data_fork
            .extents
            .next_free()
            .expect("checked above that a slot is free");
        record.data_fork.extents.set(
            slot,
            ExtentDescriptor {
                start_block: start,
                block_count: extra as u32,
            },
        )?;
        record.data_fork.total_blocks += extra as u32;

        // The bitmap goes to disk before the data blocks and before the record.
        // See the method doc for why that order and not the other.
        self.write_allocation_bitmap(&map, declared - extra as u32)?;
        Ok(())
    }

    /// Write the allocation bitmap, and the volume header's free count.
    ///
    /// One whole allocation block at a time, because that is the unit a checker
    /// reads. The header is written *after* the bitmap, never before: a header
    /// claiming fewer free blocks than the bitmap shows is an inconsistency this
    /// crate would rather not create even transiently.
    fn write_allocation_bitmap(
        &mut self,
        map: &crate::alloc::AllocationMap,
        free_blocks: u32,
    ) -> Result<()> {
        use crate::extent::mapper::ExtentMapper;

        let block_size = self.header.block_size as usize;
        let fork = &self.header.allocation_file;
        let mapper = ExtentMapper::new(fork, self.header.block_size);
        let bytes = map.as_bytes();

        let mut i = 0usize;
        while i < bytes.len() {
            let in_block = i % block_size;
            // A whole block at a time where the bitmap starts on a block
            // boundary; otherwise one byte, because a partial-block write would
            // clobber whatever shares the block.
            let span = if in_block == 0 && block_size >= 64 {
                block_size.min(bytes.len() - i)
            } else {
                1
            };
            let at = mapper.map_to_device_offset((i / block_size) as u32, in_block as u64)?;
            self.journal_write(at, &bytes[i..i + span])?;
            i += span;
        }
        self.write_header_u32(48, free_blocks)
    }

    /// Read a file's catalog record.
    ///
    /// A plain read through the B-tree. The record is *parsed*, not the raw
    /// bytes: a mutation changes fields, and a mutation that rewrote untouched
    /// fields from raw bytes would depend on every reserved byte round-tripping.
    fn read_file_record(&self, cnid: u32) -> Result<crate::catalog::record::FileRecord> {
        use crate::catalog::record::FileRecord;
        match self.find_catalog_record(cnid)? {
            CatalogHit::Record { bytes, .. } => Ok(FileRecord::parse(&bytes)?),
            CatalogHit::Thread => Err(Error::NotFound {
                what: "file record",
            }),
        }
    }

    /// Replace one file record in place, leaving its key alone.
    ///
    /// The record is located by walking the leaf chain rather than by offset, so
    /// this works regardless of the tree's shape -- and so it does not depend on
    /// the tree having one leaf, which is not a property HFS+ guarantees.
    ///
    /// The replacement is written only when it is the same length as what it
    /// replaces. A length change moves every later record in the node and needs
    /// the offset array rebuilt, which is a B-tree mutation rather than a record
    /// replacement; refusing is honest about which this is.
    ///
    /// Note the borrow: the node is located and its bytes are patched inside a
    /// scope that holds a *shared* reborrow of the device, and the write happens
    /// after that scope ends. Keeping `BTreeFile` alive across `write_at` would
    /// need two borrows of the same bytes at once.
    fn replace_catalog_record(
        &mut self,
        cnid: u32,
        record: &crate::catalog::record::FileRecord,
    ) -> Result<()> {
        let body = record.to_bytes();
        let hit = match self.find_catalog_record(cnid)? {
            CatalogHit::Record {
                node_num,
                offset,
                len,
                ..
            } => {
                if len != body.len() {
                    return Err(Error::invalid(
                        "write",
                        format!(
                            "the replacement record is {} bytes and the one it replaces \
                             is {}; a length change moves every later record in the node",
                            body.len(),
                            len
                        ),
                    ));
                }
                (node_num, offset, len)
            }
            CatalogHit::Thread => {
                return Err(Error::NotFound {
                    what: "file record",
                })
            }
        };
        let (node_num, offset, len) = hit;

        // Patch inside the scope that still holds the shared reborrow, and write
        // after it ends: keeping a `BTreeFile` alive across `write_at` would need
        // two borrows of the same bytes at once.
        let (at, patched) = {
            use crate::btree::io::BTreeFile;
            let bt = BTreeFile::open(
                &*self.device,
                &self.header.catalog_file,
                self.header.block_size,
                self.header.is_hfsx(),
            )?;
            let mut buf = bt.read_node_bytes(node_num)?;
            if buf.len() < offset + len {
                return Err(Error::OutOfRange {
                    what: "catalog record end offset",
                    value: (offset + len) as u64,
                    limit: buf.len() as u64,
                });
            }
            buf[offset..offset + len].copy_from_slice(&body);
            // `node_offset`, not `node_num * node_size`: a node's byte address is
            // wherever the fork's extents put it, which is only the same as that
            // product when the fork is contiguous from block 0. A catalog that
            // starts elsewhere -- or that overflows into the extents tree -- would
            // be written to the wrong place, silently.
            (bt.node_offset(node_num)?, buf)
        };
        self.journal_write(at, &patched)
    }

    /// Locate a catalog record by CNID, walking the leaf chain.
    ///
    /// Returns which record matched, because a thread record has the file's CNID
    /// as its key parent and a file record has the *parent folder's* CNID -- so
    /// "a record whose key names this CNID" finds both, and only the caller knows
    /// which one it wanted.
    fn find_catalog_record(&self, cnid: u32) -> Result<CatalogHit> {
        use crate::btree::io::BTreeFile;
        use crate::btree::node::NodeKind;

        let bt = BTreeFile::open(
            &*self.device,
            &self.header.catalog_file,
            self.header.block_size,
            self.header.is_hfsx(),
        )?;
        let btree_header = *bt.header();
        let mut node_num = btree_header.first_leaf_node;
        // Bounded by the node count, so a corrupted `fLink` cycle terminates
        // instead of spinning. `next <= 0` can never happen for a u32, so the
        // bound is the only exit a cycle has.
        let mut budget = btree_header.total_nodes;

        while budget > 0 && node_num != 0 {
            budget -= 1;
            let bytes = bt.read_node_bytes(node_num)?;
            let node = bt.parse_node(&bytes)?;
            if node.kind() != NodeKind::Leaf {
                break;
            }
            for index in 0..node.num_records() {
                let (key, body) = match crate::catalog::lookup::split_record(node.record(index)?) {
                    Some(parts) => parts,
                    None => continue,
                };
                // A *file* record does not key on the file: its key parentID is the
                // containing folder, and the file's own CNID is `fileID` inside the
                // body. So matching a file by CNID means reading the body, and the
                // record type has to be checked first -- a folder record is 88
                // bytes and lives at the same key.
                if body.len() == crate::catalog::record::FILE_RECORD_SIZE
                    && i16::from_be_bytes([body[0], body[1]]) == CATALOG_FILE_RECORD
                    && u32::from_be_bytes([body[8], body[9], body[10], body[11]]) == cnid
                {
                    return Ok(CatalogHit::Record {
                        node_num,
                        offset: node.record_offset(index)? + 2 + key.key_length,
                        len: body.len(),
                        bytes: body.to_vec(),
                    });
                }
                // A *thread* record does key on the object, which is what makes it
                // the reverse-lookup index. Matching one here means the caller asked
                // for a file and found only its thread.
                if key.key_length == 0 && key.parent_id.0 == cnid {
                    return Ok(CatalogHit::Thread);
                }
            }
            if node_num == btree_header.last_leaf_node {
                break;
            }
            node_num = node.descriptor().f_link;
            if node_num == 0 || node_num >= btree_header.total_nodes {
                break;
            }
        }
        Err(Error::NotFound {
            what: "file record",
        })
    }
}

/// The length of map record `index` in a node with `records` records.
fn map_record_len(node: &[u8], index: usize, records: usize) -> Result<usize> {
    if index >= records {
        return Err(Error::invalid(
            "node map",
            format!("map record {index} of a node holding {records}"),
        ));
    }
    let start = crate::btree::node::read_offset(node, index)?;
    let end = crate::btree::node::read_offset(node, index + 1)?;
    if end < start || end > node.len() {
        return Err(Error::invalid(
            "node map",
            format!(
                "map record {index} spans {start}..{end} in a {} byte node",
                node.len()
            ),
        ));
    }
    Ok(end - start)
}

/// A no-op kept for the symmetry of the map-node loop.
fn map_rec_size_check(_node_size: usize) {}

/// The name Apple's private folder for **file** hard links carries.
///
/// Four U+2500 BOX DRAWINGS LIGHT HORIZONTAL, then "HFS+ Private Data" -- 65
/// UTF-16 units. The box-drawing prefix is what makes it sort before anything a
/// user would name, and it is not decoration: a volume's private folders are
/// found *by name*, so the name has to be exactly this.
///
/// Mining reference: `HFSPLUSMETADATAFOLDER` in `core/hfs_format.h`, used as
/// `hfs_private_names[FILE_HARDLINKS]` in `core/hfs_link.c`.
pub const FILE_HARDLINKS_FOLDER: &str = "\u{2500}\u{2500}\u{2500}\u{2500}HFS+ Private Data";

/// The name Apple's private folder for **directory** hard links carries.
///
/// ".HFS+ Private Directory Data" followed by CR -- 29 units. The trailing CR
/// is in Apple's definition and is easy to lose when transcribing it.
pub const DIR_HARDLINKS_FOLDER: &str = ".HFS+ Private Directory Data\r";

/// `kHFSThreadExistsMask` -- the record's flags say its thread record exists.
/// `createindirectlink` sets it alongside `kHFSHasLinkChainMask`, and a link record
/// without it is one `fsck.hfsplus` does not expect.
pub const K_HFS_THREAD_EXISTS_MASK: u16 = 0x0002;

/// `UF_IMMUTABLE`, which `createindirectlink` sets on a hard link: a link is not
/// something to write through, only a second name to read by.
pub const UF_IMMUTABLE: u8 = 0x0002;

/// `kHardLinkFileType` -- `'hlnk'`, in a hard link's **`userInfo`**.
///
/// Which is `FileInfo` at offset 48 and *not* `finderInfo`, the `ExtendedFileInfo`
/// at 64, which has no type or creator fields. `lib_fsck_hfs` decides a file record
/// is a link from `userInfo.fdType == kHardLinkFileType &&
/// userInfo.fdCreator == kHFSPlusCreator`, and a record that fails that test is
/// treated as an ordinary file -- whose `special` is then read as a link *count*,
/// and a count of 17 produces "File has incorrect number of links (It should be 1
/// instead of 17)".
///
/// Worth stating because the value does not read as letters. An earlier version of
/// this constant was `0x686C_6C6E`, which is `'hlln'` -- two nibbles transposed,
/// in the same edit that fixed an overflow in the literal. Nothing in the source
/// would have shown it; decoding the bytes out of a generated image did.
pub const K_HARD_LINK_FILE_TYPE: u32 = 0x686C_6E6B;

/// `kHFSPlusCreator` -- `'hfs+'`, in a hard link's FinderInfo.
pub const K_HFS_PLUS_CREATOR: u32 = 0x6866_732B;

/// `kHasBeenInited`, in a hard link's FinderInfo flags.
pub const K_HAS_BEEN_INITED: u16 = 0x0100;

/// `kTextEncodingMacUnicode`, which `cat_createlink` passes to `buildrecord` for a
/// link.
pub const K_TEXT_ENCODING_MAC_UNICODE: u32 = 0;

/// The prefix Apple gives an indirect node's name in the private folder.
///
/// `HFS_INODE_PREFIX` in `core/hfs_format.h`, used by `MAKE_INODE_NAME` in
/// `core/hfs.h` as `"%s%d"` -- so the name is `iNode` followed by the CNID in
/// decimal. Directory hard links use `dir_` instead (`HFS_DIRINODE_PREFIX`).
///
/// Not decoration, and not a detail: the name is how `fsck.hfsplus` recognises an
/// indirect node. A record named with the bare CNID is not recognised, and the
/// checker clears its link-chain flag without saying why.
pub const INODE_NAME_PREFIX: &str = "iNode";

/// The prefix Apple gives a directory indirect node's name.
pub const DIR_INODE_NAME_PREFIX: &str = "dir_";

/// One half of a divided leaf.
///
/// A newtype so the signature reads as two halves rather than as
/// `Result<(Vec<Vec<u8>>, Vec<Vec<u8>>)>`, which says nothing about which is which.
struct SplitHalf(Vec<Vec<u8>>);

/// `kHFSPlusCatalogFile`, the catalog record type for a file.
const CATALOG_FILE_RECORD: i16 = 2;

/// What a search of the catalog found for a CNID.
enum CatalogHit {
    /// A file or folder record, with its node, its offset *within the node* and
    /// its length.
    Record {
        node_num: u32,
        offset: usize,
        len: usize,
        bytes: Vec<u8>,
    },
    /// A thread record. Its key parentID is the object's own CNID, so a search
    /// by CNID finds it, and it carries no fields worth writing back.
    Thread,
}

/// The CNID a fork's overflow extents are keyed on.
///
/// Both forks of a file share the CNID, and the extents B-tree key includes the
/// fork type so they cannot collide. Mining reference: `core/hfs_extents.c` keys
/// on `fileID` and `forkType`, with `kDataForkType = 0` and
/// `kResourceForkType = 0xFF`.
fn file_id(f: &FileAttrs) -> u32 {
    f.record.file_id.0
}

fn kind_label(kind: FileSystemKind) -> &'static str {
    match kind {
        FileSystemKind::HfsPlus => "HFS+",
        FileSystemKind::HfsX => "HFSX",
        FileSystemKind::ClassicHfs => "HFS",
    }
}

/// A resolved catalog object: a directory or a file.
///
/// # Where an object's data actually lives
///
/// HFS+ spreads one object's contents across three mechanisms, and they are not
/// interchangeable. Keeping them apart here is deliberate: allocation, extents
/// overflow and truncation all depend on a fork being a fork, and a reader that
/// reaches for the wrong one gets a wrong answer rather than an error.
///
/// | | where it is | reached by |
/// | --- | --- | --- |
/// | data fork | `dataFork` in the catalog record | [`Volume::read`] |
/// | resource fork | `resourceFork` in the catalog record | [`Volume::read_resource`] |
/// | FinderInfo | an *attribute*, not a field | [`crate::attributes`] |
/// | named attributes | the attributes tree | [`crate::attributes`] |
/// | a POSIX xattr | whichever of the above the adapter chose | the FUSE layer |
///
/// Three things follow that are easy to get wrong:
///
/// - **The resource fork is a real fork.** macOS *also* surfaces it as an
///   extended attribute named `com.apple.ResourceFork`, and that is the only
///   stream `getnamedstream` answers, so the name is the POSIX boundary's, not the
///   filesystem's. Internally it must stay a fork.
/// - **FinderInfo is not in the catalog record.** The 16-byte
///   `HFSPlusBSDInfo` has no FinderInfo field; HFS+ kept FinderInfo in the
///   attributes tree, where classic HFS had no equivalent to move it to. A reader
///   looking for it in the catalog record will not find it, because it is not there.
/// - **A compressed file's data fork does not hold the file's contents.** It
///   holds decmpfs data, and the logical bytes come from decompressing it -- and
///   the resource fork of such a file may be *hidden*, i.e. reported empty
///   rather than read. So "the data fork is short" can mean compressed rather
///   than truncated.
///
/// Mining reference: `core/hfs_format.h` `struct HFSPlusCatalogFile`, which
/// carries `dataFork` and `resourceFork` but no FinderInfo; `core/hfs_xattr.c`
/// `hfs_vnop_getnamedstream` for the name; `core/hfs_readwrite.c`
/// `hfs_read` for the compressed and hidden-resource-fork branches.
///
/// The variants mirror the record types, because the two are not
/// interchangeable: a directory has no forks, and a file's `is_symlink` answer
/// lives in its permissions rather than in its name.
// The File variant carries the whole 248-byte catalog record so that reading a
// fork needs no second lookup. That makes the enum lopsided, which is worth a
// note rather than an `allow`: boxing it would add an indirection on every read
// to save a few dozen bytes on a stack value.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    /// A directory.
    Directory(DirAttrs),
    /// A file, including a symbolic link.
    File(FileAttrs),
}

/// A directory's attributes, as the filesystem layer sees them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirAttrs {
    /// The directory's CNID.
    pub cnid: Cnid,
    /// Its name, from the key the record was found under.
    pub name: Vec<u16>,
    /// Number of children, as recorded by the formatter.
    pub valence: u32,
    /// Permissions and ownership.
    pub bsd_info: BsdInfo,
    /// Finder information.
    pub user_info: [u8; 16],
    /// Opaque Finder information.
    pub finder_info: [u8; 16],
    /// File's five timestamps.
    pub times: Times,
}

/// A file's attributes, as the filesystem layer sees them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttrs {
    /// The file's CNID.
    pub cnid: Cnid,
    /// Its name, from the key the record was found under.
    pub name: Vec<u16>,
    /// Permissions and ownership.
    pub bsd_info: BsdInfo,
    /// Finder information.
    pub user_info: [u8; 16],
    /// Opaque Finder information.
    pub finder_info: [u8; 16],
    /// `fdType` four-character code.
    pub fd_type: u32,
    /// `fdCreator` four-character code.
    pub fd_creator: u32,
    /// Whether extended attributes are present.
    pub has_attributes: bool,
    /// Whether this file is a hard link.
    pub is_hard_link: bool,
    /// Link count, meaningful when not a hard link.
    pub link_count: u32,
    /// Data fork's logical size.
    pub data_size: u64,
    /// Resource fork's logical size, which is what a resource-fork view reports.
    pub resource_size: u64,
    /// The catalog record, kept so fork access needs no second lookup.
    pub record: FileRecord,
    /// File's five timestamps.
    pub times: Times,
}

/// The five timestamps every catalog record carries.
///
/// Mining reference: `core/hfs_format.h` lists `createDate`, `contentModDate`,
/// `attributeModDate`, `accessDate` and `backupDate` on both record types. All
/// five are present, which is itself unusual: most filesystems have two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    /// Creation.
    pub created: HfsTimestamp,
    /// Last content modification.
    pub modified: HfsTimestamp,
    /// Last attribute modification.
    pub attribute_modified: HfsTimestamp,
    /// Last access.
    pub accessed: HfsTimestamp,
    /// Last backup.
    pub backed_up: HfsTimestamp,
}

impl Object {
    /// Build from a catalog record, taking the name from the key.
    ///
    /// Returns `None` for a thread record. A thread record is not an object, and
    /// turning one into a zeroed file would make a directory listing report
    /// phantom entries rather than dropping them.
    pub(crate) fn from_record(
        name: Vec<u16>,
        record: CatalogRecord,
        volume_expanded: bool,
    ) -> Option<Self> {
        Some(match record {
            CatalogRecord::Thread(_) => return None,
            other => Self::from_main_record(name, other, volume_expanded),
        })
    }

    /// Build from a folder or file record.
    fn from_main_record(name: Vec<u16>, record: CatalogRecord, volume_expanded: bool) -> Self {
        match record {
            CatalogRecord::Folder(f) => Object::Directory(DirAttrs {
                cnid: f.folder_id,
                name,
                valence: f.valence,
                bsd_info: f.bsd_info,
                user_info: f.user_info,
                finder_info: f.finder_info,
                times: folder_times(&f, volume_expanded),
            }),
            CatalogRecord::File(boxed) => {
                let record = *boxed;
                Object::File(FileAttrs {
                    cnid: record.file_id,
                    name,
                    bsd_info: record.bsd_info,
                    user_info: record.user_info,
                    finder_info: record.finder_info,
                    fd_type: record.fd_type(),
                    fd_creator: record.fd_creator(),
                    has_attributes: record.has_attributes(),
                    is_hard_link: record.is_hard_link(),
                    link_count: record.link_count(),
                    data_size: record.data_fork.logical_size,
                    resource_size: record.resource_fork.logical_size,
                    record,
                    times: file_times(&record, volume_expanded),
                })
            }
            // from_record rejects thread records, so reaching this arm means a
            // caller bypassed it. Returning a zeroed file would hide the mistake,
            // so the arm is unreachable by construction and states why.
            CatalogRecord::Thread(_) => unreachable!("from_record filters thread records"),
        }
    }

    /// The object's CNID.
    pub fn cnid(&self) -> Cnid {
        match self {
            Object::Directory(d) => d.cnid,
            Object::File(f) => f.cnid,
        }
    }

    /// The object's name.
    pub fn name(&self) -> &[u16] {
        match self {
            Object::Directory(d) => &d.name,
            Object::File(f) => &f.name,
        }
    }

    /// The name as a `String`, for diagnostics only.
    pub fn name_string(&self) -> String {
        String::from_utf16_lossy(self.name())
    }

    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self, Object::Directory(_))
    }

    /// Whether this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        match self {
            Object::Directory(_) => false,
            Object::File(f) => f.bsd_info.is_symlink(),
        }
    }

    /// Permissions and ownership.
    pub fn bsd_info(&self) -> BsdInfo {
        match self {
            Object::Directory(d) => d.bsd_info,
            Object::File(f) => f.bsd_info,
        }
    }

    /// The POSIX mode: type bits plus permission bits.
    ///
    /// This is what `st_mode` in `struct stat` would carry. The type bits come
    /// from the record (folder vs. file), the permission bits from the BSD info
    /// word. A caller that needs only the permission bits can mask with
    /// `0o7777`.
    pub fn mode(&self) -> u32 {
        let perms = u32::from(self.bsd_info().file_mode);
        let type_bits: u32 = if self.is_symlink() {
            S_IFLNK as u32
        } else if self.is_dir() {
            S_IFDIR as u32
        } else {
            S_IFREG as u32
        };
        perms | type_bits
    }

    /// The object's five timestamps.
    pub fn times(&self) -> Times {
        match self {
            Object::Directory(d) => d.times,
            Object::File(f) => f.times,
        }
    }

    /// The data fork's logical size in bytes.
    ///
    /// A directory has no fork, and reporting zero is better than pretending.
    pub fn data_size(&self) -> u64 {
        match self {
            Object::Directory(_) => 0,
            Object::File(f) => f.data_size,
        }
    }

    /// The resource fork's logical size in bytes.
    ///
    /// This is what a resource-fork view reports as its size. Zero means the file
    /// has no resource fork, which is the common case and not an error.
    pub fn resource_size(&self) -> u64 {
        match self {
            Object::Directory(_) => 0,
            Object::File(f) => f.resource_size,
        }
    }

    /// Whether this file has a resource fork.
    pub fn has_resource_fork(&self) -> bool {
        self.resource_size() > 0
    }

    /// The link count as POSIX `st_nlinks` would report it.
    ///
    /// For files this is the `linkCount` (or `iNodeNum`) field from the record's
    /// BSD info, depending on whether the file is a hard link (see
    /// [`FileRecord::link_count`]). For directories it is 2 -- the `.` and `..`
    /// entries -- unless the file is in the private hardlinks folder, in which
    /// case directory hard-link counts are tracked separately.
    pub fn nlink(&self) -> u32 {
        match self {
            Object::Directory(_) => 2,
            Object::File(f) => f.link_count,
        }
    }

    /// The file record behind this object, if it is a file.
    ///
    /// A caller that needs the fork geometry -- to check an extent layout, or to
    /// tell a single-extent fork from a multi-extent one -- has no other way to
    /// see it, because [`Object::data_size`] reports only the logical size.
    pub fn as_file(&self) -> Result<&FileAttrs> {
        match self {
            Object::File(f) => Ok(f),
            Object::Directory(_) => Err(Error::invalid("object", "not a file")),
        }
    }
}

fn folder_times(f: &FolderRecord, volume_expanded: bool) -> Times {
    let ts = |raw: u32| HfsTimestamp::new(raw, volume_expanded);
    Times {
        created: ts(f.create_date),
        modified: ts(f.content_mod_date),
        attribute_modified: ts(f.attribute_mod_date),
        accessed: ts(f.access_date),
        backed_up: ts(f.backup_date),
    }
}

fn file_times(f: &FileRecord, volume_expanded: bool) -> Times {
    Times {
        created: f.timestamp(f.create_date, volume_expanded),
        modified: f.timestamp(f.content_mod_date, volume_expanded),
        attribute_modified: f.timestamp(f.attribute_mod_date, volume_expanded),
        accessed: f.timestamp(f.access_date, volume_expanded),
        backed_up: f.timestamp(f.backup_date, volume_expanded),
    }
}

/// Volume statistics, as `statfs` would report them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatFs {
    /// `f_bsize`: the volume's allocation block size.
    pub block_size: u32,
    /// `f_blocks`: total allocation blocks.
    pub total_blocks: u32,
    /// `f_bfree`: unused allocation blocks.
    pub free_blocks: u32,
    /// Total volume size in bytes.
    pub total_bytes: u64,
    /// Unused space in bytes.
    pub free_bytes: u64,
    /// Number of files the volume header records.
    pub file_count: u32,
    /// Number of folders the volume header records.
    pub folder_count: u32,
    /// Longest supported name, in code units.
    pub max_name_len: u32,
    /// Whether a journal is present.
    pub journaled: bool,
}

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::catalog::record::S_IFREG;

    #[test]
    fn classic_hfs_is_refused_at_open() {
        use crate::blockdev::MemoryDevice;
        let mut dev = MemoryDevice::zeroed(4096);
        // A classic HFS signature at the volume header offset.
        dev.as_mut_slice()[1024..1026].copy_from_slice(&0x4244u16.to_be_bytes());
        assert!(Volume::open(&dev).is_err());
    }

    #[test]
    fn an_empty_device_is_refused() {
        use crate::blockdev::MemoryDevice;
        let dev = MemoryDevice::zeroed(1024);
        assert!(Volume::open(&dev).is_err());
    }

    #[test]
    fn objects_carry_their_name_and_cnid() {
        use crate::catalog::record::CatalogRecord as R;
        use crate::catalog::record::K_HFS_PLUS_FOLDER_RECORD;
        let folder = FolderRecord {
            record_type: K_HFS_PLUS_FOLDER_RECORD,
            flags: 0,
            valence: 0,
            folder_id: ROOT_FOLDER_ID,
            create_date: 0,
            content_mod_date: 0,
            attribute_mod_date: 0,
            access_date: 0,
            backup_date: 0,
            bsd_info: BsdInfo {
                owner_id: 0,
                group_id: 0,
                admin_flags: 0,
                owner_flags: 0,
                file_mode: crate::catalog::record::S_IFDIR,
                special: 0,
            },
            user_info: [0; 16],
            finder_info: [0; 16],
            text_encoding: 0,
            folder_count: 0,
        };
        let obj = Object::from_record("TestVol".encode_utf16().collect(), R::Folder(folder), false)
            .expect("a folder record becomes an object");
        assert!(obj.is_dir());
        assert!(!obj.is_symlink());
        assert_eq!(obj.cnid(), ROOT_FOLDER_ID);
        assert_eq!(obj.name_string(), "TestVol");
    }

    #[test]
    fn a_symlink_is_told_apart_by_its_mode() {
        let obj = Object::from_record(
            "link".encode_utf16().collect(),
            CatalogRecord::File(Box::new(FileRecord {
                record_type: 2,
                flags: 0,
                reserved1: 0,
                file_id: Cnid(16),
                create_date: 0,
                content_mod_date: 0,
                attribute_mod_date: 0,
                access_date: 0,
                backup_date: 0,
                bsd_info: BsdInfo {
                    owner_id: 0,
                    group_id: 0,
                    admin_flags: 0,
                    owner_flags: 0,
                    file_mode: crate::catalog::record::S_IFLNK | 0o777,
                    special: 1,
                },
                user_info: [0; 16],
                finder_info: [0; 16],
                text_encoding: 0,
                reserved2: 0,
                data_fork: ForkData::EMPTY,
                resource_fork: ForkData::EMPTY,
            })),
            false,
        )
        .expect("a file record becomes an object");
        assert!(obj.is_symlink());
        assert!(!obj.is_dir());
        assert_eq!(obj.cnid(), Cnid(16));
        assert_eq!(obj.bsd_info().permissions(), 0o777);
    }

    #[test]
    fn a_regular_file_is_not_a_symlink() {
        let obj = Object::from_record(
            vec![0x66],
            CatalogRecord::File(Box::new(FileRecord {
                record_type: 2,
                flags: 0,
                reserved1: 0,
                file_id: Cnid(16),
                create_date: 0,
                content_mod_date: 0,
                attribute_mod_date: 0,
                access_date: 0,
                backup_date: 0,
                bsd_info: BsdInfo {
                    owner_id: 0,
                    group_id: 0,
                    admin_flags: 0,
                    owner_flags: 0,
                    file_mode: S_IFREG | 0o644,
                    special: 1,
                },
                user_info: [0; 16],
                finder_info: [0; 16],
                text_encoding: 0,
                reserved2: 0,
                data_fork: ForkData::EMPTY,
                resource_fork: ForkData::EMPTY,
            })),
            false,
        )
        .expect("a file record becomes an object");
        assert!(!obj.is_dir());
        assert!(!obj.is_symlink());
        assert_eq!(obj.cnid(), Cnid(16));
    }

    #[test]
    fn mode_combines_type_and_permission_bits() {
        // Symlink with 0777 permissions: mode should be S_IFLNK | 0o777.
        let symlink = Object::from_record(
            "link".encode_utf16().collect(),
            CatalogRecord::File(Box::new(FileRecord {
                record_type: 2,
                flags: 0,
                reserved1: 0,
                file_id: Cnid(16),
                create_date: 0,
                content_mod_date: 0,
                attribute_mod_date: 0,
                access_date: 0,
                backup_date: 0,
                bsd_info: BsdInfo {
                    owner_id: 0,
                    group_id: 0,
                    admin_flags: 0,
                    owner_flags: 0,
                    file_mode: S_IFLNK | 0o777,
                    special: 1,
                },
                user_info: [0; 16],
                finder_info: [0; 16],
                text_encoding: 0,
                reserved2: 0,
                data_fork: ForkData::EMPTY,
                resource_fork: ForkData::EMPTY,
            })),
            false,
        )
        .expect("a symlink becomes an object");
        assert_eq!(symlink.mode(), u32::from(S_IFLNK) | 0o777);

        // Regular file with 0644: mode should be S_IFREG | 0o644.
        let reg = Object::from_record(
            vec![0x66],
            CatalogRecord::File(Box::new(FileRecord {
                record_type: 2,
                flags: 0,
                reserved1: 0,
                file_id: Cnid(16),
                create_date: 0,
                content_mod_date: 0,
                attribute_mod_date: 0,
                access_date: 0,
                backup_date: 0,
                bsd_info: BsdInfo {
                    owner_id: 0,
                    group_id: 0,
                    admin_flags: 0,
                    owner_flags: 0,
                    file_mode: S_IFREG | 0o644,
                    special: 1,
                },
                user_info: [0; 16],
                finder_info: [0; 16],
                text_encoding: 0,
                reserved2: 0,
                data_fork: ForkData::EMPTY,
                resource_fork: ForkData::EMPTY,
            })),
            false,
        )
        .expect("a regular file becomes an object");
        assert_eq!(reg.mode(), u32::from(S_IFREG) | 0o644);
        assert_eq!(
            reg.mode() & 0o7777,
            0o644,
            "permission bits must be preserved"
        );
    }

    #[test]
    fn nlink_is_two_for_directories_and_from_record_for_files() {
        use crate::catalog::record::K_HFS_PLUS_FOLDER_RECORD;
        // Directory: nlink is always 2 (`.` and `..`).
        let folder = FolderRecord {
            record_type: K_HFS_PLUS_FOLDER_RECORD,
            flags: 0,
            valence: 0,
            folder_id: ROOT_FOLDER_ID,
            create_date: 0,
            content_mod_date: 0,
            attribute_mod_date: 0,
            access_date: 0,
            backup_date: 0,
            bsd_info: BsdInfo {
                owner_id: 0,
                group_id: 0,
                admin_flags: 0,
                owner_flags: 0,
                file_mode: S_IFDIR | 0o755,
                special: 0,
            },
            user_info: [0; 16],
            finder_info: [0; 16],
            text_encoding: 0,
            folder_count: 0,
        };
        let obj = Object::from_record(
            "dir".encode_utf16().collect(),
            CatalogRecord::Folder(folder),
            false,
        )
        .expect("a folder record becomes an object");
        assert_eq!(obj.nlink(), 2);

        // File with link_count 3.
        let file = Object::from_record(
            "file".encode_utf16().collect(),
            CatalogRecord::File(Box::new(FileRecord {
                record_type: 2,
                flags: 0,
                reserved1: 0,
                file_id: Cnid(16),
                create_date: 0,
                content_mod_date: 0,
                attribute_mod_date: 0,
                access_date: 0,
                backup_date: 0,
                bsd_info: BsdInfo {
                    owner_id: 0,
                    group_id: 0,
                    admin_flags: 0,
                    owner_flags: 0,
                    file_mode: S_IFREG | 0o644,
                    special: 3,
                },
                user_info: [0; 16],
                finder_info: [0; 16],
                text_encoding: 0,
                reserved2: 0,
                data_fork: ForkData::EMPTY,
                resource_fork: ForkData::EMPTY,
            })),
            false,
        )
        .expect("a file record becomes an object");
        assert_eq!(file.nlink(), 3);
    }

    #[test]
    fn a_thread_record_never_becomes_an_object() {
        // A thread record describes an entry from the outside. Turning one into a
        // zeroed file would put a phantom entry in every directory listing, so the
        // conversion refuses instead.
        let thread = CatalogRecord::Thread(crate::catalog::record::ThreadRecord {
            record_type: 3,
            reserved: 0,
            parent_id: ROOT_FOLDER_ID,
            node_name: "TestVol".encode_utf16().collect(),
        });
        assert!(Object::from_record(vec![], thread, false).is_none());
    }

    #[test]
    fn reading_a_directory_as_a_file_is_an_error() {
        let dir = Object::from_record(
            "d".encode_utf16().collect(),
            CatalogRecord::Folder(crate::catalog::record::FolderRecord {
                folder_id: Cnid(16),
                ..crate::catalog::record::FolderRecord::EMPTY
            }),
            false,
        )
        .expect("a folder record is a directory");
        assert!(dir.is_dir());
        assert!(matches!(dir.as_file(), Err(Error::InvalidField { .. })));
    }

    #[test]
    fn per_item_expanded_times_override_the_volume_flag() {
        use crate::catalog::record::K_HFS_CAT_EXPANDED_TIMES_MASK;
        let mk = |flags: u16| {
            Object::from_record(
                vec![0x66],
                CatalogRecord::File(Box::new(FileRecord {
                    record_type: 2,
                    flags,
                    reserved1: 0,
                    file_id: Cnid(16),
                    create_date: 1_000_000_000,
                    content_mod_date: 1_000_000_000,
                    attribute_mod_date: 0,
                    access_date: 0,
                    backup_date: 0,
                    bsd_info: BsdInfo {
                        owner_id: 0,
                        group_id: 0,
                        admin_flags: 0,
                        owner_flags: 0,
                        file_mode: S_IFREG | 0o644,
                        special: 1,
                    },
                    user_info: [0; 16],
                    finder_info: [0; 16],
                    text_encoding: 0,
                    reserved2: 0,
                    data_fork: ForkData::EMPTY,
                    resource_fork: ForkData::EMPTY,
                })),
                false,
            )
            .expect("a file record becomes an object")
        };

        // Classic mode: 1e9 seconds since 1904 precedes the Unix epoch, and
        // Apple's to_bsd_time clamps it to zero rather than going negative.
        let classic = mk(0);
        assert_eq!(classic.times().created.to_unix(), 0);

        // The same raw value with the per-item bit set is already Unix seconds,
        // and therefore *not* clamped: 1e9 Unix seconds is 2001, whereas the same
        // value as Mac seconds would be before 1970.
        let expanded = mk(K_HFS_CAT_EXPANDED_TIMES_MASK);
        assert_eq!(expanded.times().created.to_unix(), 1_000_000_000);
    }
}
