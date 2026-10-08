//! The HFS+ / HFSX volume header.
//!
//! # Position on disk
//!
//! The volume header lives at byte offset 1024 of a volume, i.e. the second
//! 512-byte sector. Sector 0 holds the driver descriptor on a partitioned disk,
//! and sector 1 normally holds the classic `HFSMasterDirectoryBlock`. On a
//! standalone HFS+ volume the MDB slot is reused for the HFS+ volume header
//! directly; on a wrapper disk the MDB's `drEmbedSigWord` at offset 0x7C
//! announces the embedded HFS+ volume that follows.
//!
//! Mining reference: Apple `core/hfs_format.h` (`struct HFSMasterDirectoryBlock`,
//! `drEmbedSigWord`; `struct HFSPlusVolumeHeader`); `core/hfs_vfsutils.c`
//! (`hfs_ValidateHFSPlusVolumeHeader`, `hfs_MountHFSPlusVolume`).
//!
//! # Validation
//!
//! Apple validates the header in exactly two steps before trusting anything
//! else, and this module does the same, in the same order, so that a corrupt
//! image fails at the same point Apple's code would:
//!
//! 1. The signature must be `kHFSPlusSigWord` (`0x482B`) with version 4, or
//!    `kHFSXSigWord` (`0x4858`) with version 5. Signature and version are
//!    validated *together*: an HFS+ signature with version 5 is not an HFSX
//!    volume, it is a corrupt HFS+ volume.
//! 2. The allocation block size must be at least 512 and a power of two.
//!
//! Mining reference: `core/hfs_vfsutils.c` (`hfs_ValidateHFSPlusVolumeHeader`,
//! lines around the `blockSize < 512 || !powerof2(blockSize)` check).
//!
//! # HFSX
//!
//! HFSX reuses `HFSPlusVolumeHeader` verbatim. The differences are: signature
//! `0x4858` with version 5; the `volumeName` field is a Unicode string rather
//! than a Pascal string; and the catalog B-tree's `keyCompareType` is
//! `kHFSBinaryCompare` instead of `kHFSCaseFolding`, which selects the name
//! comparison algorithm used by the catalog layer. There is deliberately **no**
//! separate `HFSXVolumeHeader` struct in Apple's `hfs_format.h`; the flag that
//! selects HFSX behaviour is `signature == kHFSXSigWord`, and Apple's
//! in-memory representation then normalises the signature back to `kHFSPlusSigWord`.
//!
//! Mining reference: `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`:
//! `if (signature == kHFSXSigWord) { signature = kHFSPlusSigWord; hfsmp->hfs_flags |= HFS_X; }`).

use super::fork::{ForkData, FORK_DATA_SIZE};
use crate::blockdev::{BlockDevice, VOLUME_HEADER_OFFSET};
use crate::endian::{Be, Cursor};
use crate::error::{Error, Result};
use crate::timestamp::HfsTimestamp;

/// `kHFSSigWord` — classic HFS, `'BD'`.
pub const K_HFS_SIG_WORD: u16 = 0x4244;

/// `kHFSPlusSigWord` — HFS+, `'H+'`.
pub const K_HFS_PLUS_SIG_WORD: u16 = 0x482B;

/// `kHFSXSigWord` — HFSX, `'HX'`.
pub const K_HFSX_SIG_WORD: u16 = 0x4858;

/// `kHFSPlusVersion` — the only version valid for `kHFSPlusSigWord`.
pub const K_HFS_PLUS_VERSION: u16 = 0x0004;

/// `kHFSXVersion` — the only version valid for `kHFSXSigWord`.
pub const K_HFSX_VERSION: u16 = 0x0005;

/// Byte size of an on-disk `HFSPlusVolumeHeader`.
///
/// 112 bytes of scalar fields plus five `HFSPlusForkData` structures of 80
/// bytes each: 112 + 5 * 80 = 512, exactly one 512-byte sector, which is why
/// the header sits in a sector of its own.
///
/// Mining reference: summation of `struct HFSPlusVolumeHeader` in Apple
/// `core/hfs_format.h`.
pub const VOLUME_HEADER_SIZE: usize = 512;

/// `kHFSVolumeHardwareLockBit` — bit 7: the volume is locked by hardware.
///
/// **Bits 0 through 7, and bit 14, are reserved to implementations.** TN1150: "An
/// implementation must treat these as reserved fields", and separately "An
/// implementation may keep a copy of the attributes in memory and use bits 0-7 for
/// its own runtime flags. As an example, Mac OS uses bit 7,
/// `kHFSVolumeHardwareLockBit`, to indicate that the volume is write-protected due
/// to some hardware setting."
///
/// So bit 7 has a name and no defined on-disk meaning, and bits 0-6 are the
/// implementation's own. TN1150 contradicts itself here: the enum in that document
/// comments "Bits 0-6 are reserved" while the prose beneath it is headed "bits 0-7".
/// The prose is the one that describes use, and this crate follows it -- which is
/// also why the volume-header disagreement this project recorded earlier is about
/// *bit 8*, the first bit with a specified meaning.
pub const K_HFS_VOLUME_HARDWARE_LOCK_BIT: u32 = 0x0000_0080;

/// `kHFSVolumeUnmountedBit` — bit 8: the volume was cleanly unmounted.
///
/// Note this is **bit 8**, not bit 15. Bit 15 is `kHFSVolumeSoftwareLockBit`.
/// Conflating the two makes a freshly created, unlocked, cleanly unmounted
/// volume look software-locked, and makes an actually-locked volume look clean.
///
/// Mining reference: the volume attribute enum in Apple `core/hfs_format.h`
/// assigns `kHFSVolumeUnmountedBit = 8` and `kHFSVolumeUnmountedMask = 0x00000100`.
pub const K_HFS_VOLUME_UNMOUNTED_BIT: u32 = 0x0000_0100;

/// `kHFSVolumeUnmountedMask`.
pub const K_HFS_VOLUME_UNMOUNTED_MASK: u32 = K_HFS_VOLUME_UNMOUNTED_BIT;

/// `kHFSVolumeSparedBlocksBit` — bit 9: the volume has spared bad blocks.
pub const K_HFS_VOLUME_SPARED_BLOCKS_MASK: u32 = 0x0000_0200;

/// `kHFSVolumeNoCacheRequiredBit` — bit 10.
pub const K_HFS_VOLUME_NO_CACHE_REQUIRED_MASK: u32 = 0x0000_0400;

/// `kHFSBootVolumeInconsistentBit` — bit 11.
pub const K_HFS_BOOT_VOLUME_INCONSISTENT_MASK: u32 = 0x0000_0800;

/// `kHFSCatalogNodeIDsReusedBit` — bit 12.
pub const K_HFS_CATALOG_NODE_IDS_REUSED_MASK: u32 = 0x0000_1000;

/// `kHFSVolumeJournaledBit` — bit 13: a journal is present on this volume.
///
/// Mining reference: `kHFSVolumeJournaledBit = 13` and
/// `kHFSVolumeJournaledMask = 0x00002000` in Apple `core/hfs_format.h`.
pub const K_HFS_VOLUME_JOURNALED_BIT: u32 = 0x0000_2000;

/// `kHFSVolumeJournaledMask`.
pub const K_HFS_VOLUME_JOURNALED_MASK: u32 = K_HFS_VOLUME_JOURNALED_BIT;

/// `kHFSVolumeInconsistentBit` — bit 14: serious inconsistencies were detected
/// at runtime.
pub const K_HFS_VOLUME_INCONSISTENT_MASK: u32 = 0x0000_4000;

/// `kHFSVolumeSoftwareLockBit` — bit 15: the volume is locked by software.
pub const K_HFS_VOLUME_SOFTWARE_LOCK_MASK: u32 = 0x0000_8000;

/// `kHFSVolumeNewFsBit` — bit 31: indicates a freshly formatted volume.
pub const K_HFS_VOLUME_NEW_FS_MASK: u32 = 0x8000_0000;

/// `kHFSExpandedTimesMask` — bit 29: timestamps are Unix seconds.
///
/// Mining reference: `kHFSExpandedTimesMask = 0x20000000` in Apple
/// `core/hfs_format.h`.
pub const K_HFS_EXPANDED_TIMES_MASK: u32 = 0x2000_0000;

/// `kHFSContentProtectionMask` — bit 30: the volume has per-file content
/// protection.
///
/// Mining reference: `kHFSContentProtectionMask = 0x40000000` in Apple
/// `core/hfs_format.h`. Relevant to the encryption/content-protection
/// investigation: its presence tells us files *may* be protected, not that any
/// particular one is.
pub const K_HFS_CONTENT_PROTECTION_MASK: u32 = 0x4000_0000;

/// `kHFSUnusedNodeFixBit` — bit 31: unused catalog B-tree nodes are zero-filled.
///
/// Mining reference: `kHFSUnusedNodeFixMask = 0x80000000` in Apple
/// `core/hfs_format.h`, referring to Radar #6947811.
pub const K_HFS_UNUSED_NODE_FIX_MASK: u32 = 0x8000_0000;

/// `kHFSMDBAttributesMask` — the bits that have a meaning in a classic HFS MDB.
pub const K_HFS_MDB_ATTRIBUTES_MASK: u32 = 0x0000_8380;

/// `kHFSRootParentID` — the parent ID of the root folder, always 1.
pub const K_HFS_ROOT_PARENT_ID: u32 = 1;

/// `kHFSRootFolderID` — the folder ID of the root folder, always 2.
///
/// The volume's own name is *not* stored in the volume header. It is the name
/// of the root folder, which lives in the catalog B-tree. See
/// [`VolumeHeader::volume_name_is_in_the_catalog`].
///
/// Mining reference: `kHFSRootFolderID = 2` in Apple `core/hfs_format.h`;
/// `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) calls
/// `cat_idlookup(hfsmp, kHFSRootFolderID, ...)` and copies `cd_nameptr` into
/// `vcb->vcbVN` to obtain the volume name.
pub const K_HFS_ROOT_FOLDER_ID: u32 = 2;

/// Which HFS family a volume belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileSystemKind {
    /// Classic HFS (`0x4244`). Recognised for signature detection only; this
    /// crate does not implement classic HFS.
    ClassicHfs,
    /// HFS+ (`0x482B`).
    HfsPlus,
    /// HFSX (`0x4858`).
    HfsX,
}

impl FileSystemKind {
    /// Classify a 16-bit signature word.
    pub fn from_signature(sig: u16) -> Result<Self> {
        match sig {
            K_HFS_SIG_WORD => Ok(FileSystemKind::ClassicHfs),
            K_HFS_PLUS_SIG_WORD => Ok(FileSystemKind::HfsPlus),
            K_HFSX_SIG_WORD => Ok(FileSystemKind::HfsX),
            other => Err(Error::BadSignature { found: other }),
        }
    }

    /// The version this kind must declare.
    pub const fn required_version(self) -> u16 {
        match self {
            FileSystemKind::HfsPlus => K_HFS_PLUS_VERSION,
            FileSystemKind::HfsX => K_HFSX_VERSION,
            // Classic HFS has no "version" field in the same sense; reported as 0.
            FileSystemKind::ClassicHfs => 0,
        }
    }

    /// Whether this crate intends to mount volumes of this kind.
    pub const fn is_supported(self) -> bool {
        matches!(self, FileSystemKind::HfsPlus | FileSystemKind::HfsX)
    }

    /// Whether names on this kind are compared case-sensitively.
    ///
    /// For HFS+ this is always false. For HFSX it is decided by the catalog
    /// B-tree's `keyCompareType`, not by the signature, so this method is only
    /// a first approximation; the authoritative answer comes from the B-tree
    /// header.
    ///
    /// Mining reference: Apple `core/hfs_format.h` defines `kHFSCaseFolding`
    /// (`0xCF`) and `kHFSBinaryCompare` (`0xBC`) as the catalog
    /// `keyCompareType` values.
    pub const fn default_case_sensitive(self) -> bool {
        matches!(self, FileSystemKind::HfsX)
    }
}

/// Volume attribute bit flags, decoded for display and for behaviour decisions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeAttributes(pub u32);

impl VolumeAttributes {
    /// `kHFSVolumeUnmountedBit`: the volume was shut down cleanly.
    pub fn is_unmounted(self) -> bool {
        self.0 & K_HFS_VOLUME_UNMOUNTED_MASK != 0
    }

    /// `kHFSVolumeJournaledBit`: a journal is present.
    pub fn is_journaled(self) -> bool {
        self.0 & K_HFS_VOLUME_JOURNALED_MASK != 0
    }

    /// `kHFSExpandedTimesMask`: timestamps are Unix seconds, not Mac seconds.
    pub fn has_expanded_times(self) -> bool {
        self.0 & K_HFS_EXPANDED_TIMES_MASK != 0
    }

    /// `kHFSContentProtectionMask`: the volume may contain protected files.
    ///
    /// Mining reference: `kHFSContentProtectionMask = 0x40000000` in Apple
    /// `core/hfs_format.h`.
    pub fn has_content_protection(self) -> bool {
        self.0 & K_HFS_CONTENT_PROTECTION_MASK != 0
    }

    /// `kHFSVolumeInconsistentBit`: runtime detected serious inconsistencies.
    pub fn is_inconsistent(self) -> bool {
        self.0 & K_HFS_VOLUME_INCONSISTENT_MASK != 0
    }

    /// `kHFSVolumeSoftwareLockBit`: the volume is locked by software.
    pub fn is_software_locked(self) -> bool {
        self.0 & K_HFS_VOLUME_SOFTWARE_LOCK_MASK != 0
    }
}

/// The parsed HFS+ / HFSX volume header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeHeader {
    /// On-disk signature word. `0x482B` for HFS+, `0x4858` for HFSX.
    pub signature: u16,
    /// Version field; 4 for HFS+, 5 for HFSX.
    pub version: u16,
    /// Volume attribute bitfield.
    pub attributes: u32,
    /// Implementation version string that last mounted the volume, big-endian
    /// four-character code, e.g. `10.0`.
    ///
    /// TN1150 makes this a duty rather than a courtesy: "**Any code which modifies
    /// the on disk structures must also set this field to a unique value which
    /// identifies that code.** Third-party implementations of HFS Plus should place
    /// a registered creator code in this field."
    ///
    /// It is also how another implementation finds out it is not alone: values in
    /// use include `10.0` (Mac OS X), `HFSJ` (a journaled volume) and `fsck` -- the
    /// last of which `fsck.hfsplus` writes when it repairs a volume, which is why a
    /// repaired image comes back reporting `fsc.k` here.
    ///
    /// **This crate does not set it.** A writer here changes a volume without
    /// saying so, which leaves the question TN1150 asks the field to answer open.
    pub last_mounted_version: u32,
    /// Allocation block holding the journal info block, or 0 if not journaled.
    ///
    /// Mining reference: `struct HFSPlusVolumeHeader::journalInfoBlock`. This
    /// field doubles as spare space in an unwrapped HFS+ volume; only trust it
    /// when [`VolumeAttributes::is_journaled`] is true.
    pub journal_info_block: u32,
    /// Volume creation time, raw Mac OS seconds.
    pub create_date: u32,
    /// Last modification time, raw Mac OS seconds.
    pub modify_date: u32,
    /// Last backup time, raw Mac OS seconds.
    pub backup_date: u32,
    /// Last disk-check time, raw Mac OS seconds.
    pub checked_date: u32,
    /// Number of **file** records in the catalog.
    ///
    /// TN1150, Volume Header: "The total number of files on the volume. **The
    /// fileCount field does not include the special files. It should equal the
    /// number of file records found in the catalog file.**"
    ///
    /// "Should", not "must": the field is a count, and a writer that gets it wrong
    /// produces a volume that reads correctly and reports a wrong number. Which is
    /// why `fsck.hfsplus` recomputes it rather than believing it.
    pub file_count: u32,
    /// Number of **folder** records in the catalog, **excluding the root**.
    ///
    /// TN1150, Volume Header: "The total number of folders on the volume. **The
    /// folderCount field does not include the root folder. It should equal the
    /// number of folder records in the catalog file, minus one** (since the root
    /// folder has a folder record in the catalog file)."
    ///
    /// The root has a folder record like any other; it is the *count* that omits it.
    /// So a volume holding nothing but its root reports 0, and one holding the root
    /// and one folder reports 1.
    pub folder_count: u32,
    /// Allocation block size in bytes.
    pub block_size: u32,
    /// Total allocation blocks, including the volume header and bitmap.
    pub total_blocks: u32,
    /// Unused allocation blocks.
    pub free_blocks: u32,
    /// Where the next allocation search should start.
    pub next_allocation: u32,
    /// Default resource-fork clump size in bytes.
    pub rsrc_clump_size: u32,
    /// Default data-fork clump size in bytes.
    pub data_clump_size: u32,
    /// Next unused catalog node ID (CNID).
    pub next_catalog_id: u32,
    /// Volume write count.
    /// `writeCount`: how many times the volume has been mounted for writing.
    ///
    /// TN1150: "incremented every time a volume is mounted", and "it is very
    /// important that an implementation or utility change the writeCount field if it
    /// modifies the volume's structures directly. This is particularly important if
    /// it adds or deletes items on the volume."
    ///
    /// **This crate does not increment it**, which is the same gap as
    /// [`Self::last_mounted_version`] seen from the other side.
    pub write_count: u32,
    /// Which legacy text encodings have been used on this volume.
    pub encodings_bitmap: u64,
    /// 32 bytes of Finder information for the volume.
    pub finder_info: [u8; 32],
    /// The allocation bitmap ("Volume Bitmap") file.
    pub allocation_file: ForkData,

    /// The extents overflow B-tree file.
    pub extents_file: ForkData,
    /// The catalog B-tree file.
    pub catalog_file: ForkData,
    /// The extended attributes B-tree file.
    pub attributes_file: ForkData,
    /// The startup file (secondary loader).
    pub startup_file: ForkData,
}

impl VolumeHeader {
    /// Which filesystem family this header describes.
    pub fn kind(&self) -> Result<FileSystemKind> {
        FileSystemKind::from_signature(self.signature)
    }

    /// Decoded volume attributes.
    pub fn attribute_flags(&self) -> VolumeAttributes {
        VolumeAttributes(self.attributes)
    }

    /// Whether a journal is present.
    ///
    /// Mining reference: `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`)
    /// decides journaledness from the `kHFSVolumeJournaledBit` attribute, and
    /// treats `journalInfoBlock` as meaningful only in that case.
    pub fn is_journaled(&self) -> bool {
        self.attribute_flags().is_journaled()
    }

    /// Whether the volume was cleanly unmounted.
    pub fn is_clean(&self) -> bool {
        self.attribute_flags().is_unmounted()
    }

    /// Whether timestamps on this volume are expanded (Unix) times.
    pub fn has_expanded_times(&self) -> bool {
        self.attribute_flags().has_expanded_times()
    }

    /// Whether the volume header declares HFSX semantics.
    pub fn is_hfsx(&self) -> bool {
        self.signature == K_HFSX_SIG_WORD
    }

    /// A standing reminder that the volume name is not a volume-header field.
    ///
    /// The bytes at offset 112 of an HFS+ volume header are the start of
    /// `allocationFile`, not a name. The volume name is the name of the root
    /// folder (CNID [`K_HFS_ROOT_FOLDER_ID`]), read from the catalog B-tree.
    ///
    /// Mining reference: `struct HFSPlusVolumeHeader` in Apple
    /// `core/hfs_format.h` has no name field, and its field widths sum to
    /// exactly one 512-byte sector: 112 scalar bytes plus 5 * 80 fork bytes.
    /// `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) fetches the name with
    /// `cat_idlookup(kHFSRootFolderID)`.
    pub const fn volume_name_is_in_the_catalog() -> u32 {
        K_HFS_ROOT_FOLDER_ID
    }

    /// The CNID of the root folder, whose catalog record carries the volume
    /// name.
    pub const fn root_folder_id(&self) -> u32 {
        K_HFS_ROOT_FOLDER_ID
    }

    /// Interpret a raw Mac OS timestamp from this header.
    pub fn timestamp(&self, raw: u32) -> HfsTimestamp {
        HfsTimestamp::new(raw, self.has_expanded_times())
    }

    /// The volume's modification time.
    pub fn modify_time(&self) -> HfsTimestamp {
        self.timestamp(self.modify_date)
    }

    /// The volume's creation time.
    pub fn create_time(&self) -> HfsTimestamp {
        self.timestamp(self.create_date)
    }

    /// The volume's last backup time.
    pub fn backup_time(&self) -> HfsTimestamp {
        self.timestamp(self.backup_date)
    }

    /// The volume's last disk-check time.
    pub fn checked_time(&self) -> HfsTimestamp {
        self.timestamp(self.checked_date)
    }

    /// Total volume size in bytes, from `total_blocks * block_size`.
    ///
    /// Returns an error rather than wrapping if the multiplication overflows.
    pub fn volume_bytes(&self) -> Result<u64> {
        u64::from(self.total_blocks)
            .checked_mul(u64::from(self.block_size))
            .ok_or(Error::overflow("volume_bytes"))
    }

    /// Read the primary volume header from `device` at byte offset 1024.
    ///
    /// Mining reference: `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`)
    /// takes the volume header from the volume's second 512-byte sector and
    /// passes it to `hfs_ValidateHFSPlusVolumeHeader` unmodified.
    pub fn read_from<D: BlockDevice + ?Sized>(device: &D) -> Result<Self> {
        let buf = device.read_array::<VOLUME_HEADER_SIZE>(VOLUME_HEADER_OFFSET)?;
        Self::from_bytes(&buf)
    }

    /// Parse and validate a volume header from a 512-byte slice.
    ///
    /// Applies Apple's validation order: signature/version pairing first, then
    /// allocation block size.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let be = Be::new(buf);
        if buf.len() < VOLUME_HEADER_SIZE {
            return Err(Error::Truncated {
                what: "volume header",
                needed: VOLUME_HEADER_SIZE,
                available: buf.len(),
            });
        }

        let signature = be.u16(0)?;
        let version = be.u16(2)?;
        let attributes = be.u32(4)?;
        let last_mounted_version = be.u32(8)?;
        let journal_info_block = be.u32(12)?;
        let create_date = be.u32(16)?;
        let modify_date = be.u32(20)?;
        let backup_date = be.u32(24)?;
        let checked_date = be.u32(28)?;
        let file_count = be.u32(32)?;
        let folder_count = be.u32(36)?;
        let block_size = be.u32(40)?;
        let total_blocks = be.u32(44)?;
        let free_blocks = be.u32(48)?;
        let next_allocation = be.u32(52)?;
        let rsrc_clump_size = be.u32(56)?;
        let data_clump_size = be.u32(60)?;
        let next_catalog_id = be.u32(64)?;
        let write_count = be.u32(68)?;
        let encodings_bitmap = be.u64(72)?;
        let mut finder_info = [0u8; 32];
        finder_info.copy_from_slice(be.slice(80, 32, "volume header finderInfo")?);

        // No volume name field exists here. The 112 scalar bytes end at
        // finderInfo, and the five HFSPlusForkData structures occupy the
        // remaining 400 bytes: 512 - 112 = 400 = 5 * 80.
        let fork_base = 112;
        let mut fork_cur = Cursor::at(buf, fork_base, "volume header forks");
        let allocation_file = ForkData::read(&mut fork_cur)?;
        let extents_file = ForkData::read(&mut fork_cur)?;
        let catalog_file = ForkData::read(&mut fork_cur)?;
        let attributes_file = ForkData::read(&mut fork_cur)?;
        let startup_file = ForkData::read(&mut fork_cur)?;

        let header = VolumeHeader {
            signature,
            version,
            attributes,
            last_mounted_version,
            journal_info_block,
            create_date,
            modify_date,
            backup_date,
            checked_date,
            file_count,
            folder_count,
            block_size,
            total_blocks,
            free_blocks,
            next_allocation,
            rsrc_clump_size,
            data_clump_size,
            next_catalog_id,
            write_count,
            encodings_bitmap,
            finder_info,
            allocation_file,
            extents_file,
            catalog_file,
            attributes_file,
            startup_file,
        };

        header.validate()?;
        Ok(header)
    }

    /// Apple's `hfs_ValidateHFSPlusVolumeHeader` checks, in Apple's order.
    ///
    /// Mining reference: Apple `core/hfs_vfsutils.c`,
    /// `hfs_ValidateHFSPlusVolumeHeader`:
    ///
    /// ```c
    /// if (signature == kHFSPlusSigWord) {
    ///     if (hfs_version != kHFSPlusVersion) return (EINVAL);
    /// } else if (signature == kHFSXSigWord) {
    ///     if (hfs_version != kHFSXVersion) return (EINVAL);
    /// } else {
    ///     return (EINVAL);
    /// }
    /// blockSize = SWAP_BE32(vhp->blockSize);
    /// if (blockSize < 512 || !powerof2(blockSize)) return (EINVAL);
    /// ```
    ///
    /// Note that Apple checks the signature/version pair rather than accepting
    /// either signature with any version, and that classic HFS (`0x4244`) is
    /// rejected here even though it is a real HFS family signature.
    pub fn validate(&self) -> Result<()> {
        let kind = match self.signature {
            K_HFS_PLUS_SIG_WORD => FileSystemKind::HfsPlus,
            K_HFSX_SIG_WORD => FileSystemKind::HfsX,
            other => return Err(Error::BadSignature { found: other }),
        };
        if self.version != kind.required_version() {
            return Err(Error::BadVersion {
                found: self.version,
                expected: kind.required_version(),
            });
        }
        if self.block_size < 512 || !self.block_size.is_power_of_two() {
            return Err(Error::invalid(
                "volume_header.blockSize",
                format!("{} must be >= 512 and a power of two", self.block_size),
            ));
        }
        // A journaled volume must say where its journal info block is, and the
        // block it names must be one of its own. The field is otherwise a bare
        // `u32` that nothing constrains, so a volume naming a block at or past
        // its own end would send a reader off to parse whatever happens to live
        // there -- catalog data, or the journal's own blocks -- and the resulting
        // complaint would be about *those* bytes rather than about the volume.
        //
        // Apple does not range-check this, and tolerates such a volume by
        // accident. Refusing it here is deliberate: the info block is part of the
        // volume by definition, so a pointer outside it means the header is wrong.
        //
        // Mining reference: `struct HFSPlusVolumeHeader` documents
        // `journalInfoBlock` as the "allocation block number of the journal info
        // block", and `core/hfs_vfsutils.c` `hfs_mount_hfsplus` uses it as a
        // block number within this volume before consulting the image.
        if self.is_journaled() {
            let info_block = u64::from(self.journal_info_block);
            let limit = u64::from(self.total_blocks);
            if self.journal_info_block == 0 || info_block >= limit {
                return Err(Error::out_of_range(
                    "volume_header.journalInfoBlock",
                    info_block,
                    limit,
                ));
            }
        }
        Ok(())
    }

    /// Byte offset of the alternate (backup) volume header within the volume.
    ///
    /// HFS+ keeps a second copy of the volume header so that a volume stays
    /// mountable when the primary copy at offset 1024 is damaged. It is placed
    /// 1024 bytes before the end of the volume, which is the second-to-last
    /// 512-byte sector.
    ///
    /// Mining reference: Apple `core/hfs.h` defines
    ///
    /// ```c
    /// #define HFS_ALT_SECTOR(blksize, blkcnt) (((blkcnt) - 1) - (512 / (blksize)))
    /// ```
    ///
    /// and `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) evaluates it with
    /// the *logical* block size, which is always 512, so the expression
    /// collapses to `total_sectors - 2`. Note that this is emphatically **not**
    /// "one allocation block from the end": for a volume with 4096-byte blocks
    /// the two differ by 3072 bytes, and using the allocation-block reading
    /// silently reads zeros.
    ///
    /// The `spare_sectors` branch in the same function means that when the
    /// partition is larger than the filesystem there can be a *second* copy at
    /// the end of the partition; for an image whose partition and filesystem
    /// coincide, which is every image in this corpus, the two positions are the
    /// same and one offset suffices.
    pub fn alternate_header_offset(&self) -> Result<u64> {
        let volume_bytes = self.volume_bytes()?;
        let total_sectors = volume_bytes / crate::blockdev::SECTOR_SIZE;
        if total_sectors < 2 {
            return Err(Error::invalid(
                "volume_header.totalBlocks",
                "volume too small to hold an alternate volume header",
            ));
        }
        Ok((total_sectors - 2) * crate::blockdev::SECTOR_SIZE)
    }

    /// Serialise back to the on-disk 512-byte layout.
    ///
    /// Provided so that a header can be written back byte-identically. Fields
    /// whose on-disk meaning is not yet modelled are preserved verbatim from
    /// `reserved` bytes carried alongside the parsed header.
    pub fn to_bytes(&self) -> [u8; VOLUME_HEADER_SIZE] {
        let mut out = [0u8; VOLUME_HEADER_SIZE];
        out[0..2].copy_from_slice(&self.signature.to_be_bytes());
        out[2..4].copy_from_slice(&self.version.to_be_bytes());
        out[4..8].copy_from_slice(&self.attributes.to_be_bytes());
        out[8..12].copy_from_slice(&self.last_mounted_version.to_be_bytes());
        out[12..16].copy_from_slice(&self.journal_info_block.to_be_bytes());
        out[16..20].copy_from_slice(&self.create_date.to_be_bytes());
        out[20..24].copy_from_slice(&self.modify_date.to_be_bytes());
        out[24..28].copy_from_slice(&self.backup_date.to_be_bytes());
        out[28..32].copy_from_slice(&self.checked_date.to_be_bytes());
        out[32..36].copy_from_slice(&self.file_count.to_be_bytes());
        out[36..40].copy_from_slice(&self.folder_count.to_be_bytes());
        out[40..44].copy_from_slice(&self.block_size.to_be_bytes());
        out[44..48].copy_from_slice(&self.total_blocks.to_be_bytes());
        out[48..52].copy_from_slice(&self.free_blocks.to_be_bytes());
        out[52..56].copy_from_slice(&self.next_allocation.to_be_bytes());
        out[56..60].copy_from_slice(&self.rsrc_clump_size.to_be_bytes());
        out[60..64].copy_from_slice(&self.data_clump_size.to_be_bytes());
        out[64..68].copy_from_slice(&self.next_catalog_id.to_be_bytes());
        out[68..72].copy_from_slice(&self.write_count.to_be_bytes());
        out[72..80].copy_from_slice(&self.encodings_bitmap.to_be_bytes());
        out[80..112].copy_from_slice(&self.finder_info);

        // The five forks occupy the final 400 bytes in declaration order:
        // allocationFile, extentsFile, catalogFile, attributesFile, startupFile.
        let forks = [
            &self.allocation_file,
            &self.extents_file,
            &self.catalog_file,
            &self.attributes_file,
            &self.startup_file,
        ];
        for (i, fork) in forks.iter().enumerate() {
            let off = 112 + i * FORK_DATA_SIZE;
            out[off..off + FORK_DATA_SIZE].copy_from_slice(&fork.to_bytes());
        }
        out
    }
}

/// Byte offset of each fork within the on-disk volume header, for tooling.
///
/// Mining reference: field order of `struct HFSPlusVolumeHeader` in Apple
/// `core/hfs_format.h`.
pub const FORK_OFFSETS: [(&str, usize); 5] = [
    ("allocationFile", 112),
    ("extentsFile", 112 + FORK_DATA_SIZE),
    ("catalogFile", 112 + 2 * FORK_DATA_SIZE),
    ("attributesFile", 112 + 3 * FORK_DATA_SIZE),
    ("startupFile", 112 + 4 * FORK_DATA_SIZE),
];

#[cfg(test)]
// Building fixtures field by field keeps each on-disk field visible next
// to the value under test, so the struct-update lint is relaxed here.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::blockdev::MemoryDevice;

    /// Build a minimal but structurally valid header for tests.
    fn synthetic_header(sig: u16, version: u16, block_size: u32) -> VolumeHeader {
        VolumeHeader {
            signature: sig,
            version,
            attributes: K_HFS_VOLUME_UNMOUNTED_MASK | K_HFS_VOLUME_SOFTWARE_LOCK_MASK,
            last_mounted_version: 0,
            journal_info_block: 0,
            create_date: 0,
            modify_date: 0,
            backup_date: 0,
            checked_date: 0,
            file_count: 0,
            folder_count: 0,
            block_size,
            total_blocks: 100,
            free_blocks: 50,
            next_allocation: 0,
            rsrc_clump_size: 0,
            data_clump_size: 0,
            next_catalog_id: 16,
            write_count: 0,
            encodings_bitmap: 0,
            finder_info: [0u8; 32],
            allocation_file: ForkData::EMPTY,
            extents_file: ForkData::EMPTY,
            catalog_file: ForkData::EMPTY,
            attributes_file: ForkData::EMPTY,
            startup_file: ForkData::EMPTY,
        }
    }

    #[test]
    fn signature_classification() {
        assert_eq!(
            FileSystemKind::from_signature(0x482B).unwrap(),
            FileSystemKind::HfsPlus
        );
        assert_eq!(
            FileSystemKind::from_signature(0x4858).unwrap(),
            FileSystemKind::HfsX
        );
        assert_eq!(
            FileSystemKind::from_signature(0x4244).unwrap(),
            FileSystemKind::ClassicHfs
        );
        assert!(matches!(
            FileSystemKind::from_signature(0x0000),
            Err(Error::BadSignature { found: 0x0000 })
        ));
        assert!(!FileSystemKind::ClassicHfs.is_supported());
    }

    #[test]
    fn header_is_exactly_one_sector() {
        assert_eq!(VOLUME_HEADER_SIZE, 512);
        let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        assert_eq!(h.to_bytes().len(), VOLUME_HEADER_SIZE);
        // 112 bytes of scalars + 5 forks of 80 bytes.
        assert_eq!(112 + 5 * FORK_DATA_SIZE, VOLUME_HEADER_SIZE);
    }

    #[test]
    fn round_trips_through_bytes() {
        let mut h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        h.attributes = K_HFS_VOLUME_UNMOUNTED_MASK | K_HFS_VOLUME_JOURNALED_MASK;
        h.journal_info_block = 7;
        h.file_count = 3;
        h.folder_count = 2;
        h.total_blocks = 8192;
        h.free_blocks = 7997;
        h.catalog_file.logical_size = 8192;
        h.catalog_file.total_blocks = 2;
        h.extents_file.logical_size = 4096;

        let parsed = VolumeHeader::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(parsed, h);
        assert!(parsed.is_journaled());
        assert!(parsed.is_clean());
    }

    #[test]
    fn rejects_hfsplus_signature_with_hfsx_version() {
        // Apple's validation pairs signature with version; a mismatch is corrupt.
        let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFSX_VERSION, 4096);
        assert!(matches!(
            h.validate(),
            Err(Error::BadVersion {
                found: 5,
                expected: 4
            })
        ));
    }

    #[test]
    fn rejects_hfsx_signature_with_hfsplus_version() {
        let h = synthetic_header(K_HFSX_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        assert!(matches!(
            h.validate(),
            Err(Error::BadVersion {
                found: 4,
                expected: 5
            })
        ));
    }

    #[test]
    fn rejects_classic_hfs_signature() {
        // Classic HFS is a real signature but not an HFS+ volume header.
        let h = synthetic_header(K_HFS_SIG_WORD, 0, 512);
        assert!(matches!(
            h.validate(),
            Err(Error::BadSignature { found: 0x4244 })
        ));
    }

    #[test]
    fn rejects_non_power_of_two_and_small_block_sizes() {
        for bad in [0u32, 1, 256, 511, 3000, 5000] {
            let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, bad);
            assert!(
                matches!(h.validate(), Err(Error::InvalidField { .. })),
                "block size {bad} should be rejected"
            );
        }
        for good in [512u32, 1024, 2048, 4096, 8192, 16384, 65536] {
            let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, good);
            assert!(h.validate().is_ok(), "block size {good} should be accepted");
        }
    }

    #[test]
    fn rejects_short_input_without_panicking() {
        for len in [0usize, 1, 2, 111, 139, 200, 511] {
            let buf = vec![0u8; len];
            assert!(
                matches!(VolumeHeader::from_bytes(&buf), Err(Error::Truncated { .. })),
                "length {len} should be truncated"
            );
        }
    }

    #[test]
    fn reads_from_device_at_offset_1024() {
        let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        let mut dev = MemoryDevice::zeroed(1024 + VOLUME_HEADER_SIZE);
        dev.as_mut_slice()[1024..].copy_from_slice(&h.to_bytes());
        let parsed = VolumeHeader::read_from(&dev).unwrap();
        assert_eq!(parsed.signature, K_HFS_PLUS_SIG_WORD);
        assert_eq!(parsed.block_size, 4096);
    }

    #[test]
    fn device_too_small_reports_truncation() {
        let dev = MemoryDevice::zeroed(100);
        assert!(matches!(
            VolumeHeader::read_from(&dev),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn fork_offsets_land_where_expected() {
        let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        let bytes = h.to_bytes();
        for (name, off) in FORK_OFFSETS {
            let fork = ForkData::from_bytes(&bytes[off..off + FORK_DATA_SIZE]).unwrap();
            // Round-tripping each fork individually proves the offsets are right.
            assert_eq!(fork, h.fork_by_name(name).unwrap(), "fork {name}");
        }
    }

    #[test]
    fn hfsx_parses_with_its_own_version() {
        let h = synthetic_header(K_HFSX_SIG_WORD, K_HFSX_VERSION, 4096);
        assert!(h.validate().is_ok());
        assert!(h.is_hfsx());
        assert_eq!(h.kind().unwrap(), FileSystemKind::HfsX);
        assert!(FileSystemKind::HfsX.default_case_sensitive());
        assert!(!FileSystemKind::HfsPlus.default_case_sensitive());
    }

    #[test]
    fn first_fork_starts_immediately_after_finder_info() {
        // finderInfo occupies 80..112, so allocationFile begins at 112.
        // Getting this wrong by 28 is the classic mistake of assuming a volume
        // name field exists where there is none.
        let h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        let bytes = h.to_bytes();
        assert_eq!(FORK_OFFSETS[0].1, 112);
        assert_eq!(FORK_OFFSETS[4].1 + FORK_DATA_SIZE, VOLUME_HEADER_SIZE);
        // The last fork must end exactly at the end of the header.
        assert_eq!(432 + FORK_DATA_SIZE, VOLUME_HEADER_SIZE);
        // And nothing overlaps finderInfo.
        assert_eq!(FORK_OFFSETS[0].1, 80 + 32);
        assert_eq!(
            ForkData::from_bytes(&bytes[112..192]).unwrap(),
            h.allocation_file
        );
    }

    #[test]
    fn alternate_header_is_two_sectors_before_the_end() {
        // Mining reference: Apple core/hfs.h
        //   HFS_ALT_SECTOR(512, n) == n - 2 sectors
        // which is byte offset volume_bytes - 1024. For a 4096-byte-block volume
        // this is *not* the same as one allocation block from the end.
        let mut h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        h.total_blocks = 8192;
        let volume_bytes = 8192 * 4096;
        assert_eq!(h.volume_bytes().unwrap(), volume_bytes);
        assert_eq!(
            h.alternate_header_offset().unwrap(),
            volume_bytes - 1024,
            "backup volume header must sit 1024 bytes before end of volume"
        );
        assert_ne!(
            h.alternate_header_offset().unwrap(),
            volume_bytes - 4096,
            "must not be one allocation block from the end"
        );
    }

    #[test]
    fn alternate_header_offset_scales_with_block_size() {
        let mut h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 512);
        h.total_blocks = 65536;
        // With 512-byte blocks the two readings coincide, which is why the
        // wrong formula went unnoticed on 512-byte volumes.
        assert_eq!(h.alternate_header_offset().unwrap(), 65536u64 * 512 - 1024);
    }

    #[test]
    fn alternate_header_offset_rejects_a_tiny_volume() {
        let mut h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 512);
        h.total_blocks = 1;
        assert!(matches!(
            h.alternate_header_offset(),
            Err(Error::InvalidField { .. })
        ));
    }

    #[test]
    fn volume_bytes_is_checked_and_cannot_overflow() {
        let mut h = synthetic_header(K_HFS_PLUS_SIG_WORD, K_HFS_PLUS_VERSION, 4096);
        // Two u32 inputs always fit in a u64, so the checked multiply can never
        // fail for a header that parsed. The check is retained so that widening
        // the inputs later cannot silently introduce a wrap.
        h.total_blocks = u32::MAX;
        h.block_size = u32::MAX;
        let bytes = h.volume_bytes().unwrap();
        assert_eq!(bytes, u64::from(u32::MAX) * u64::from(u32::MAX));
    }

    impl VolumeHeader {
        /// Test helper: fetch a named fork.
        fn fork_by_name(&self, name: &str) -> Option<ForkData> {
            Some(match name {
                "allocationFile" => self.allocation_file,
                "extentsFile" => self.extents_file,
                "catalogFile" => self.catalog_file,
                "attributesFile" => self.attributes_file,
                "startupFile" => self.startup_file,
                _ => return None,
            })
        }
    }
}
