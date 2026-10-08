// SPDX-License-Identifier: APSL-1.2

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
//! # This crate reads journals and writes transactions
//!
//! Everything below the transaction-committer level is the *recovery* side: it
//! walks the block lists and overlays the recorded blocks. The writer side is
//! the port of `end_transaction` + `write_journal_header` from `core/hfs_journal.c`,
//! and it is built in three layers:
//!
//! - [`TransactionBuffer`] holds dirty block before-images in memory, porting
//!   Apple's `block_list_header_in_memory` buffers — a block is copied into the
//!   buffer before the home block is mutated, so the journal always carries the
//!   original contents for replay.
//! - [`commit_transaction`] encodes the buffer, writes the block data to the
//!   journal ring (wrapping at `size`), syncs, advances `end`, bumps
//!   `sequence_num`, then writes the header — the barrier ordering that makes a
//!   torn transaction recoverable.
//! - [`write_journal_header`] encodes and stores the header at byte zero of the
//!   journal.
//!
//! The commit path is wired in and called by every mutating write path in
//! [`crate::volume::WritableVolume`]. Each public mutation method begins a
//! journal transaction, routes all disk writes through
//! [`crate::volume::WritableVolume::journal_write`] (which records before-images
//! in the [`TransactionBuffer`] before mutating home blocks), and commits via
//! [`end_transaction`](crate::volume::WritableVolume::end_transaction) →
//! [`commit_transaction`]. On failure the transaction is abandoned without
//! advancing the journal header, so uncommitted writes are invisible to replay.
//!
//! A journaled volume can be opened for writing: [`crate::volume::WritableVolume`]
//! reads the journal info block and header during [`open`](crate::volume::WritableVolume::open)
//! and stores them in a [`JournalState`](crate::volume::JournalState) that
//! [`TransactionBuffer`] fills on the first mutation.
//!
//! That has a measurable consequence for verification, and it is not a subtlety:
//! `fsck.hfsplus` prints "Checking Journaled HFS Plus volume" or "Checking
//! non-journaled HFS Plus Volume" and takes a **different code path** either way.
//! Volumes this crate writes are now checked along the journalled path, which
//! is the path a real macOS volume takes.
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
pub use info::{ByteOrder, JournalFlags, JournalHeader, JournalInfoBlock, SpaceCheck, END_BLK_NUM};
pub use replay::{
    commit_transaction, encode_block_list, encode_transaction, write_journal_header, Journal,
    RecordedWrite, ReplayedBlock, Transaction, TransactionBuffer, MAX_BLOCKS_PER_LIST,
};
