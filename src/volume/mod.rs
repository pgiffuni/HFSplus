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

use crate::blockdev::BlockDevice;
use crate::catalog::cnid::{Cnid, ROOT_FOLDER_ID};
use crate::catalog::lookup::Catalog;
use crate::catalog::record::{BsdInfo, CatalogRecord, FileRecord, FolderRecord};
use crate::error::{Error, Result};
use crate::file::ForkReader;
use crate::format::fork::ForkData;
use crate::format::volume_header::{FileSystemKind, VolumeHeader};

mod bitmap;

pub use bitmap::{bytes_for_blocks, AllocationBitmap};
use crate::timestamp::HfsTimestamp;

/// A mounted, read-only HFS+ or HFSX volume.
pub struct Volume<'a, D: ?Sized> {
    device: &'a D,
    header: VolumeHeader,
    catalog: Catalog<'a, D>,
    kind: FileSystemKind,
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

        Ok(Volume { device, header, catalog, kind })
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
        let Some(CatalogRecord::Thread(thread)) =
            self.catalog.lookup(self.root_cnid(), &[])?
        else {
            return Err(Error::NotFound { what: "root folder thread record" });
        };
        if thread.node_name.is_empty() {
            return Err(Error::NotFound { what: "root folder name" });
        }
        Ok(String::from_utf16_lossy(&thread.node_name))
    }

    /// Look up `name` inside `parent`.
    pub fn lookup(&self, parent: Cnid, name: &[u16]) -> Result<Option<Object>> {
        let Some(record) = self.catalog.lookup(parent, name)? else {
            return Ok(None);
        };
        // Thread records describe a directory entry from the outside; a
        // directory listing must surface the object itself.
        if record.is_thread() {
            return Ok(None);
        }
        Ok(Object::from_record(name.to_vec(), record, self.header.has_expanded_times()))
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
        self.fork_reader(&f.record.data_fork).read(offset, len)
    }

    /// Read the whole data fork, bounded by `limit` bytes.
    pub fn read_file(&self, file: &Object, limit: usize) -> Result<Vec<u8>> {
        let f = file.as_file()?;
        self.fork_reader(&f.record.data_fork).read_all(limit)
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
        self.fork_reader(&f.record.resource_fork).read(offset, len)
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
        let bytes = self.fork_reader(&f.record.data_fork).read_all(MAX_TARGET)?;
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
            free_bytes: u64::from(self.header.free_blocks).checked_mul(u64::from(bs)).ok_or(
                Error::overflow("statfs free bytes"),
            )?,
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
        crate::journal::Journal::open(
            self.device,
            self.header.journal_info_block,
            self.header.block_size,
        )
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

    fn fork_reader(&self, fork: &ForkData) -> ForkReader<'a, D> {
        ForkReader::new(self.device, fork, self.header.block_size)
    }
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
    fn from_record(
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
    fn from_main_record(
        name: Vec<u16>,
        record: CatalogRecord,
        volume_expanded: bool,
    ) -> Self {
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

    /// Borrow the file attributes, if this is a file.
    fn as_file(&self) -> Result<&FileAttrs> {
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
        let obj = Object::from_record(
            "TestVol".encode_utf16().collect(),
            R::Folder(folder),
            false,
        )
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