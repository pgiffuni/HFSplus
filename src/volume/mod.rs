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
use crate::btree::ExtentKey;
use crate::catalog::cnid::{Cnid, ROOT_FOLDER_ID};
use crate::catalog::lookup::Catalog;
use crate::catalog::record::{BsdInfo, CatalogRecord, FileRecord, FolderRecord};
use crate::error::{Error, Result};
use crate::extent::OverflowResolver;
use crate::file::{ForkOverflow, ForkReader, TreeOverflow};
use crate::format::fork::ForkData;
use crate::format::volume_header::{FileSystemKind, VolumeHeader};

mod bitmap;

use crate::timestamp::HfsTimestamp;
pub use bitmap::{bytes_for_blocks, AllocationBitmap};

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
/// stops. Anything that changes the volume has to be asked for by name.
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
}

impl<D: ?Sized> std::fmt::Debug for WritableVolume<'_, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the device. The interesting question about this
        // type is what it established, and printing the device would bury that.
        f.debug_struct("WritableVolume")
            .field("filesystem", &self.kind)
            .field("journal_was_replayed", &self.journal_replayed)
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
        drop(volume);

        if journal_replayed {
            return Err(Error::invalid(
                "write",
                "journalled writes are not implemented; a write without a journal entry \
                 would leave a journal that does not describe the volume",
            ));
        }

        Ok(WritableVolume {
            device,
            header,
            kind,
            journal_replayed,
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
    /// Always false today: [`WritableVolume::open`] refuses a journaled volume
    /// outright. It is kept because the refusal is a property of what is
    /// implemented, not of the volume, and a writer that could not tell the two
    /// apart would have no way to notice when the refusal is lifted.
    pub fn journal_was_replayed(&self) -> bool {
        self.journal_replayed
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
    /// volume header, no journal.
    ///
    /// # What it refuses, and why
    ///
    /// - **Growth.** A file that outgrows its blocks needs an allocator, and an
    ///   allocator that guessed at a free block would be worse than no mutation.
    ///   The error names the shortfall rather than truncating the write.
    /// - **A journaled volume.** Refused in [`WritableVolume::open`], before any
    ///   byte is written.
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
            self.device.write_at(at, &data[start..end])?;
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
        use crate::blockdev::VOLUME_HEADER_OFFSET;
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
            self.device.write_at(at, &bytes[i..i + span])?;
            i += span;
        }
        self.device
            .write_at(VOLUME_HEADER_OFFSET + 48, &free_blocks.to_be_bytes())
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
        self.device.write_at(at, &patched)
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
    fn from_record(name: Vec<u16>, record: CatalogRecord, volume_expanded: bool) -> Option<Self> {
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
        assert!(!obj.is_symlink());
        assert!(!obj.is_dir());
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
