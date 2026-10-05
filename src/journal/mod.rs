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
//!
//! # This crate reads journals and does not write them
//!
//! Everything here so far is the *recovery* side. Apple's writer does something
//! quite different, and the difference is not a detail this module can defer.
//!
//! On a journalled volume, **a writer does not write the catalog**. It opens a
//! *transaction*, mutates, and commits; the journal captures the blocks the
//! mutation touched and the filesystem structures are brought up to date
//! afterwards. `hfs_start_transaction` and `hfs_end_transaction` bracket every
//! mutating path in the kernel — `core/hfs_catalog.c`, `core/hfs_cnode.c`,
//! `core/hfs_btreeio.c` and `core/hfs_cprotect.c` each call `hfs_start_transaction`
//! before changing anything.
//!
//! So the direct block writes elsewhere in this crate reproduce the *recovery* path,
//! not the writer's. That is coherent for a volume with no journal, and it is why
//! [`crate::volume::WritableVolume`] refuses a journalled volume outright rather
//! than pretending to write one — but it also means every volume this crate mutates
//! is a **non-journalled** volume, which is a shape macOS does not produce.
//!
//! That has a measurable consequence for verification, and it is not a subtlety:
//! `fsck.hfsplus` prints "Checking Journaled HFS Plus volume" or "Checking
//! non-journaled HFS Plus Volume" and takes a **different code path** either way.
//! Every volume this crate writes is checked along the non-journalled path, which
//! is not the path a real volume would be checked along.
//!
//! # What a transaction is, and the one invariant that is easy to get wrong
//!
//! A transaction is a journal lock plus a block list. The invariant is **lock
//! order**, and Apple enforces it by assertion rather than by convention:
//!
//! ```c
//! /* You cannot start a transaction while holding a system
//!  * file lock. (unless the transaction is nested.) */
//! if (hfsmp->jnl && journal_owner(hfsmp->jnl) != thread) {
//!         if (hfsmp->hfs_catalog_cp && hfsmp->hfs_catalog_cp->c_lockowner == thread) {
//!                 panic("hfs_start_transaction: bad lock order (cat before jnl)\n");
//!         }
//!         if (hfsmp->hfs_attribute_cp && hfsmp->hfs_attribute_cp->c_lockowner == thread) {
//!                 panic("hfs_start_transaction: bad lock order (attr before jnl)\n");
//!         }
//! }
//! ```
//!
//! The journal lock is **outermost**. A catalog or attribute lock may be held only
//! *inside* a transaction, never before one — and the penalty for getting it
//! backwards is a `panic`, not an error return. Callers show the shape: in
//! `core/hfs_vfsutils.c` the pattern is to *end* the transaction, start a new one,
//! and only then `hfs_systemfile_lock(hfsmp, SFL_CATALOG | SFL_ATTRIBUTE |
//! SFL_EXTENTS | SFL_BITMAP, HFS_EXCLUSIVE_LOCK)`.
//!
//! A userspace library has no lock hierarchy to violate, so this is not a
//! constraint on the port — but the *ordering* it encodes is, because it tells you
//! what a transaction is scoped around: the system-file locks, not the individual
//! record writes. That is the unit that must be atomic, and it is wider than any
//! single mutation this crate currently performs.
//!
//! Mining reference: `core/hfs_vfsutils.c` `hfs_start_transaction` (the lock-order
//! assertion), `core/hfs.h` (the `HFS_RDONLY_DOWNGRADE` contract), and the callers
//! in `core/hfs_catalog.c`, `core/hfs_cnode.c`, `core/hfs_btreeio.c`,
//! `core/hfs_cprotect.c` and `core/hfs_hotfiles.c`.

pub mod checksum;
pub mod info;
pub mod replay;

pub use checksum::{calc_checksum, BLHDR_CHECKSUM_SIZE, JOURNAL_HEADER_CKSUM_SIZE};
pub use info::{JournalFlags, JournalHeader, JournalInfoBlock, END_BLK_NUM};
pub use replay::{Journal, ReplayedBlock, Transaction};
