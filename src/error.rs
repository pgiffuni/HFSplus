//! Structured errors for HFS+ parsing and filesystem operations.
//!
//! Every value that originates on disk is untrusted, so the whole crate
//! funnels failures through [`Error`] rather than panicking. A malformed
//! image must produce a diagnosable error, never a crash: the project's
//! malformed-image corpus exists specifically to prove that property.
//!
//! Mining reference: Apple `core/hfs_vfsutils.c` and `core/fsck_hfs/` both
//! return `EINVAL` / `EIO` / `ENOSPC` style codes rather than asserting on
//! unexpected on-disk values; `core/hfs_vfsutils.c`
//! (`hfs_ValidateHFSPlusVolumeHeader`) is the model for rejecting a bad
//! volume header early, before any other structure is trusted.

use std::fmt;

/// A parse or I/O failure in the HFS+ stack.
///
/// The variants deliberately carry the offending value and the field it came
/// from, because when a real-world image fails to mount, the first question is
/// always "which field, and what did it say".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A field held a value the format does not permit.
    ///
    /// Used where Apple's code would `return (EINVAL)`, e.g. a non-power-of-two
    /// allocation block size.
    InvalidField {
        /// Dotted path of the offending field, e.g. `volume_header.blockSize`.
        field: &'static str,
        /// Detail describing what was expected.
        expected: String,
    },

    /// A structure would read or write past the end of the supplied buffer.
    ///
    /// On-disk length fields are never trusted: a caller must be able to ask
    /// for a record and get `Truncated` rather than an out-of-bounds access.
    Truncated {
        /// Name of the structure being decoded.
        what: &'static str,
        /// Bytes required by the decoder.
        needed: usize,
        /// Bytes actually available.
        available: usize,
    },

    /// Arithmetic on decoded values overflowed or produced an impossible
    /// result.
    ///
    /// The classic instance is `totalBlocks * blockSize`, both attacker
    /// controlled, which must be checked rather than allowed to wrap.
    Overflow {
        /// Description of the computation that failed.
        what: &'static str,
    },

    /// A block number or byte offset lies outside the backing device or the
    /// declared volume extent.
    OutOfRange {
        /// Name of the quantity that was out of range.
        what: &'static str,
        /// The offending value.
        value: u64,
        /// The inclusive upper bound it exceeded.
        limit: u64,
    },

    /// An allocation block number is outside the volume's allocation range.
    BadBlockNumber {
        /// The offending allocation block number.
        block: u32,
        /// The volume's total allocation block count.
        total_blocks: u32,
    },

    /// The volume signature was not a recognised HFS family signature.
    BadSignature {
        /// The 16-bit signature that was read.
        found: u16,
    },

    /// The volume header version did not match the signature's required
    /// version.
    BadVersion {
        /// The version field that was read.
        found: u16,
        /// The version required for this signature.
        expected: u16,
    },

    /// A named structure was expected but absent, e.g. no journal on a
    /// journaled volume.
    NotFound {
        /// Name of the missing structure or record.
        what: &'static str,
    },

    /// A name comparison or lookup failed to find the requested entry.
    NotFoundKey {
        /// The key that was searched for, rendered for diagnostics.
        key: String,
    },

    /// The underlying block device failed.
    Io {
        /// Operating-system error text.
        message: String,
    },

    /// There is no free space for the requested number of blocks.
    ///
    /// Distinct from [`Error::OutOfRange`] because a caller retries differently:
    /// an out-of-range block is a bug in the caller, while a full volume is
    /// ordinary. Mining reference: Apple `core/VolumeAllocation.c` returns
    /// `dskFulErr` from `BlockFindAny` when no extent is large enough, and the
    /// callers distinguish it from every other error explicitly.
    NoSpace {
        /// Blocks that were asked for.
        requested: u32,
        /// Blocks actually free.
        available: u64,
    },

    /// The operation requires write access but the device was opened
    /// read-only.
    ReadOnly,

    /// The requested feature or compression type is not implemented.
    ///
    /// Used where Apple's code would `return (ENOTSUP)` — for example, a
    /// compression type this reader does not yet decode.
    Unsupported {
        /// What was asked for.
        what: String,
    },
}

impl Error {
    /// Convenience constructor for [`Error::InvalidField`].
    pub fn invalid(field: &'static str, expected: impl Into<String>) -> Self {
        Error::InvalidField {
            field,
            expected: expected.into(),
        }
    }

    /// Convenience constructor for [`Error::Overflow`].
    pub fn overflow(what: &'static str) -> Self {
        Error::Overflow { what }
    }

    /// Convenience constructor for [`Error::OutOfRange`].
    pub fn out_of_range(what: &'static str, value: u64, limit: u64) -> Self {
        Error::OutOfRange { what, value, limit }
    }

    /// Map an underlying [`std::io::Error`] into [`Error::Io`].
    pub fn io(err: &std::io::Error) -> Self {
        Error::Io {
            message: err.to_string(),
        }
    }

    /// Convenience constructor for [`Error::NoSpace`].
    pub fn no_space(requested: u32, available: u64) -> Self {
        Error::NoSpace {
            requested,
            available,
        }
    }

    /// Convenience constructor for [`Error::Unsupported`].
    pub fn unsupported(what: impl Into<String>) -> Self {
        Error::Unsupported { what: what.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidField { field, expected } => {
                write!(f, "invalid field {field}: {expected}")
            }
            Error::Truncated {
                what,
                needed,
                available,
            } => {
                write!(f, "truncated {what}: need {needed} bytes, have {available}")
            }
            Error::Overflow { what } => write!(f, "arithmetic overflow computing {what}"),
            Error::OutOfRange { what, value, limit } => {
                write!(f, "{what} {value} exceeds limit {limit}")
            }
            Error::BadBlockNumber {
                block,
                total_blocks,
            } => {
                write!(
                    f,
                    "allocation block {block} outside volume of {total_blocks} blocks"
                )
            }
            Error::BadSignature { found } => {
                write!(f, "unrecognised volume signature 0x{found:04x}")
            }
            Error::BadVersion { found, expected } => {
                write!(f, "volume version 0x{found:04x}, expected 0x{expected:04x}")
            }
            Error::NotFound { what } => write!(f, "{what} not present on this volume"),
            Error::NotFoundKey { key } => write!(f, "no such entry: {key}"),
            Error::Io { message } => write!(f, "i/o error: {message}"),
            Error::NoSpace {
                requested,
                available,
            } => write!(
                f,
                "no space: {requested} blocks requested, {available} free"
            ),
            Error::ReadOnly => write!(f, "filesystem opened read-only"),
            Error::Unsupported { what } => {
                write!(f, "not supported: {what}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::io(&err)
    }
}

/// Convenience alias for fallible HFS+ operations.
pub type Result<T> = std::result::Result<T, Error>;
