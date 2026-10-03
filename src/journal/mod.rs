//! HFS+ journaling.
//!
//! Mining reference: Apple `core/hfs_journal.h` and `core/hfs_journal.c`, plus
//! `core/hfs_vfsutils.c` where the journal is located at mount time.
//!
//! # How a journal is found
//!
//! Three steps, in this order:
//!
//! 1. The volume header's `kHFSVolumeJournaledBit` must be set. `journalInfoBlock`
//!    is only meaningful then; on an unwrapped volume that field overlaps spare
//!    space and holds whatever was left there.
//! 2. `journalInfoBlock` names the *allocation block* holding a `JournalInfoBlock`.
//! 3. That block's `offset` and `size` place the journal on the device.
//!
//! Mining reference: `core/hfs_vfsutils.c` (`hfs_MountHFSPlusVolume`) reads the
//! volume header, and `core/hfs_journal.c` (`journal_open`) reads the info block
//! and validates `flags`.

pub mod checksum;
pub mod info;
pub mod replay;

pub use checksum::{calc_checksum, BLHDR_CHECKSUM_SIZE, JOURNAL_HEADER_CKSUM_SIZE};
pub use info::{JournalFlags, JournalHeader, JournalInfoBlock, END_BLK_NUM};
pub use replay::{Journal, ReplayedBlock, Transaction};
