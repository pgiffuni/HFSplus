//! Userspace HFS+/HFSX implementation in Rust.
//!
//! # Scope
//!
//! This crate implements the HFS+ and HFSX filesystems against the on-disk
//! definitions and semantics of Apple's HFS implementation. It is a *userspace*
//! library: it reads and writes filesystem images, and it knows nothing about
//! Linux block devices, mount namespaces, or kernel interfaces. A FUSE
//! frontend consumes this library; the library never depends on FUSE.
//!
//! # Layering
//!
//! ```text
//! block device  (blockdev)
//!        v
//! on-disk structures  (format, endian)
//!        v
//! B-tree / extent / allocation engines  (btree, extent)
//!        v
//! catalog / attributes / forks  (catalog, attributes)
//!        v
//! HFS filesystem object model  (volume, file)
//!        v
//! FUSE adapter  (fuse, in a separate crate)
//! ```
//!
//! # Reference sources, in priority order
//!
//! 1. Apple's HFS source, <https://github.com/apple-oss-distributions/hfs>,
//!    pinned at commit `d1bac2f062e6e9c0dfcce302d9aacb10173d0eea`.
//!    `pgiffuni/apple-hfs` is a mirror of that repository at the same commit
//!    and is an acceptable substitute when the canonical one is unreachable.
//! 2. Apple HFS documentation and format definitions.
//! 3. Apple tests and test data shipped with that repository
//!    (`tests/cases/`, `lib_fsck_hfs/`).
//! 4. `0x09/hfsfuse` as a compatibility baseline only. It is GPL-2.0 and is
//!    never copied, translated, or vendored here.
//! 5. HFS utilities installed in the development environment; see
//!    `docs/dev-tools.md`.
//!
//! Where sources disagree the disagreement is investigated and recorded rather
//! than resolved by picking one side silently.
//!
//! # Mining provenance
//!
//! Code derived from Apple's implementation stays under APSL-1.2 and cites its
//! source in module documentation. See `LICENSE-README.md`. Derived code says
//! so explicitly:
//!
//! ```text
//! $ rg -n "Mining reference: Apple" src/
//! ```
//!
//! Original glue code is BSD-2-Clause.

#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod alloc;
pub mod blockdev;
pub mod btree;
pub mod catalog;
pub mod check;
pub mod endian;
pub mod error;
pub mod extent;
pub mod file;
pub mod format;
pub mod journal;
pub mod timestamp;
pub mod unicode;
pub mod volume;

pub use error::{Error, Result};

/// The exact Apple HFS source commit this implementation was mined from.
pub const APPLE_HFS_COMMIT: &str = "d1bac2f062e6e9c0dfcce302d9aacb10173d0eea";

/// Commit of `0x09/hfsfuse` used as the compatibility baseline.
pub const HFSFUSE_BASELINE_COMMIT: &str = "8c44b9aa80a8eba541ac3a8b86aad8eecde9277f";

/// Version string reported by the reference `fsck.hfsplus` used in tests.
pub const HFS_PROGS_VERSION: &str = "540.1";

/// Reads and validates the primary HFS+/HFSX volume header of `device`.
///
/// The volume header is expected at byte offset 1024, matching where
/// `mkfs.hfsplus` and `newfs_hfs` place it on a standalone image.
///
/// # Errors
///
/// Returns [`Error::BadSignature`] if the signature is not a supported HFS+
/// or HFSX one, [`Error::BadVersion`] if the signature and version disagree,
/// and [`Error::Truncated`] if the device is too small to contain a header.
///
/// # Examples
///
/// ```
/// use hfsplus::blockdev::MemoryDevice;
/// use hfsplus::format::volume_header::{VolumeHeader, K_HFS_PLUS_SIG_WORD};
///
/// let mut dev = MemoryDevice::zeroed(32 * 1024 * 1024);
/// // An all-zero image is not a volume; the error must be structural, not a panic.
/// assert!(VolumeHeader::read_from(&dev).is_err());
/// ```
pub fn read_volume_header<D: blockdev::BlockDevice + ?Sized>(
    device: &D,
) -> Result<format::volume_header::VolumeHeader> {
    format::volume_header::VolumeHeader::read_from(device)
}

/// Signature word at byte offset 1024, without validating anything else.
///
/// Useful as a cheap probe when classifying an unknown image, and as the first
/// step of the malformed-image tests: it must fail cleanly on a short image.
///
/// # Examples
///
/// ```
/// use hfsplus::blockdev::MemoryDevice;
/// let dev = MemoryDevice::new(vec![0u8; 1024]);
/// assert!(hfsplus::probe_signature(&dev).is_err());
/// ```
pub fn probe_signature<D: blockdev::BlockDevice + ?Sized>(device: &D) -> Result<u16> {
    device
        .read_array::<2>(blockdev::VOLUME_HEADER_OFFSET)
        .map(u16::from_be_bytes)
}

/// Re-exported so `format::volume_header::K_HFS_PLUS_SIG_WORD` and the
/// unqualified spelling are both available to callers.
pub use format::volume_header::{K_HFSX_SIG_WORD, K_HFS_PLUS_SIG_WORD, K_HFS_SIG_WORD};
