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
                ..FileRecord::EMPTY.bsd_info
            },
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
        if parent_cnid.0 == ROOT_FOLDER_ID.0 && parent == ROOT_FOLDER_ID.0 && is_folder {
            return Err(Error::invalid(
                "remove",
                "the root folder cannot be deleted",
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
        self.device.write_at(at, &buf)?;

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
        self.device.write_at(
            crate::blockdev::VOLUME_HEADER_OFFSET + offset,
            &value.to_be_bytes(),
        )?;
        // Every header write has to move the copy at the end of the volume with it.
        // See `sync_backup_header`.
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
        self.device.write_at(at + offset, &value.to_be_bytes())
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
    /// Mining reference: `core/hfs_vfsops.c` writes the "alternate volume header
    /// located at 1024 bytes before end of the partition"; the same file notes that
    /// where the filesystem size equals the partition size this is the only such
    /// header worth tracking.
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
        self.device.write_at(volume_bytes - 1024, &buf)
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
        self.device.write_at(at, bytes)
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
                self.device.write_at(at_node_offset, &buf)?;
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
        self.device.write_at(at, &fork.to_bytes())?;
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
        self.device.write_at(at + offset, &value.to_be_bytes())
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
        self.device.write_at(node_at, &patched)
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
            self.device.write_at(at, &bytes[i..i + span])?;
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
