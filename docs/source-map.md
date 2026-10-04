# Source map

Where each piece of this crate came from, and what was deliberately changed.

Apple's source is the authority for HFS semantics. This crate is the authority for
how they are arranged in Rust. Neither substitutes for the other: a translation
is correct when it preserves Apple's *behaviour*, not when it resembles Apple's
*code*, and the differences below are the interesting part.

# The rule

Every substantial item goes through the same six steps, and the last three are
what this file records:

1. Locate the Apple implementation.
2. Identify the data structures and the invariants they maintain.
3. Identify the callers and callees, to see which invariants are local and which
   are the caller's problem.
4. Read the on-disk representation.
5. Translate the algorithm into Rust.
6. Record it here: Apple file, Apple function, structures, invariants, Rust
   destination, Rust function, and what was intentionally left out.

A translation is not done until it is tested at three levels — the serialized
bytes, a round trip, and an independent checker. `tests/checker.rs` and
`hfsck.hfsplus` cover the third for everything the corpus can express.

# Where the Apple source is

`https://github.com/apple-oss-distributions/hfs`, pinned at
`d1bac2f062e6e9c0dfcce302d9aacb10173d0eea`.
`https://github.com/pgiffuni/apple-hfs` is a mirror of that repository at the
same commit and is an acceptable substitute when the canonical one is
unreachable.

# Sources, by area

265 references to Apple source across `src/`, in 225 places marked
`Mining reference: Apple`. Counted by reference rather than by marker, because a
single translation often cites the same file for several fields. The files cited
most:

| Apple file | refs | what it governs here |
| --- | --- | --- |
| `core/hfs_format.h` | 46 | every on-disk structure: header, forks, extents, catalog records, B-tree nodes, journal header |
| `core/hfs_vfsutils.c` | 42 | mount-time validation, signature and version pairing, reserved regions |
| `core/hfs_journal.c` | 28 | replay, the info block, the header checksum, the ring |
| `core/hfs_catalog.c` | 25 | CNID allocation, thread records, key construction, lookup |
| `core/hfs_extents.c` | 20 | fork growth, the overflow tree, extent iteration |
| `core/hfs_endian.c` | 10 | the byte-swapping layer |
| `core/hfs_journal.h` | 9 | `journal_header`, `block_list_header`, `block_info` |
| `core/BTreeNodeOps.c` | 11 | node descriptor parsing, record addressing, key sizing |
| `core/hfs_btreeio.c` | 4 | B-tree node access, node sizing, the big-keys rule |
| `core/hfs.h` | 3 | `HFS_ALT_SECTOR` |
| `core/hfs_vfsops.c` | 3 | mount-time journal gating |
| `core/hfs_statfs.c` | 2 | the `statfs` fields |
| `core/MacOSStubs.c` | 5 | `to_bsd_time`, `to_hfs_time` — the timestamp rules |
| `core/BTreeMiscOps.c` | 2 | header node initialisation |
| `lib_fsck_hfs/dfalib/*.c` | 7 | what an independent checker validates, cross-read rather than copied |

Also cited once each: `core/hfs_cnode.c`, `core/hfs_catalog.h`, `core/hfs_endian.h`,
`core/hfs_hotfiles.c`, `core/hfs_vnodeops.c`, `core/hfs_xattr.c`, `core/BTreeScanner.c`,
`lib_fsck_hfs/dfalib/SRepair.c`, and `livefiles_hfs_plugin/lf_hfs_utils.c` (a second
copy of Apple's timestamp conversions, which is useful corroboration that they were
deliberate).

## Volume header

| | |
| --- | --- |
| Apple | `core/hfs_format.h` `struct HFSPlusVolumeHeader`; `core/hfs_vfsutils.c` `hfs_mount_hfsplus`, `IsHFSX`, `IsValidVolumeName` |
| Structures | `HFSPlusVolumeHeader`, the five `HFSPlusForkData` special files |
| Invariants | signature and version must pair (`HFSPlusSig`/`kHFSPlusVersion`, `HFSXSig`/`kHFSXVersion`); `blockSize` is a power of two and at least 512; a journaled volume's `journalInfoBlock` must name one of its own blocks |
| Rust | `src/format/volume_header.rs` |
| Differences | Apple's mount recovers from a damaged primary by reading the alternate header. It does not — `core/` computes `hfs_partition_avh_sector` and `hfs_fs_avh_sector` on the mount path but only `core/hfs_resize.c` ever reads them. So a damaged primary refuses to mount here too, and that is correct rather than a gap. Documented in `docs/hfs-format.md`. |

## B-tree

| | |
| --- | --- |
| Apple | `core/BTreeNodeOps.c` `GetRecordOffset`, `GetRecordSize`, `GetRecordAddress`, `CalcKeySize`; `core/BTree.c` `BTOpenPath`; `core/hfs_btreeio.c` |
| Structures | `BTNodeDescriptor`, `BTHeaderRec`, the three key layouts |
| Invariants | a key's `keyLength` excludes itself; the offset array holds `numRecords + 1` entries with the last marking free space; a node's height is one more than its parent's |
| Rust | `src/btree/` (`node.rs`, `header.rs`, `key.rs`, `io.rs`) |
| Differences | Apple's node cache and `buf_meta_t` are gone. The record model is byte-identical, which matters: `tests/btree_conformance.rs` measures record spans against the corpus and the crate's offsets must agree exactly. |

Two key layouts were wrong here and are now pinned by tests:

- `HFSPlusExtentKey` omits nothing — it is `keyLength + forkType + pad + fileID +
  startBlock`, ten bytes of body and twelve on disk. An earlier revision dropped
  `forkType` and `pad`, which put `fileID` at the offset of the fork type. Every
  extents lookup then decoded the wrong key and matched nothing.
- `HFSPlusAttrKey` is 266 bytes of body; an earlier revision dropped its `pad` and
  said 264. The corpus's own attributes tree header says 266.

## Catalog

| | |
| --- | --- |
| Apple | `core/hfs_catalog.c` `cat_lookup`, `cat_rebuild`; `core/hfs_cnode.c`; `core/BTreeScanner.c` |
| Structures | `HFSPlusCatalogKey`, `HFSPlusCatalogFile`, `HFSPlusCatalogFolder`, `HFSPlusCatalogThread` |
| Invariants | a thread record's key parentID is the object's CNID and its name is empty; a thread record's body names the object's *parent*; CNIDs 0 and 1 are reserved and 2 is the root folder |
| Rust | `src/catalog/` |
| Differences | `Catalog::all_objects` resolves through thread records, so an object with a missing thread record is invisible to it. `Catalog::all_records` walks the leaves directly for that reason, and the checker uses it — a file whose thread record is broken must be *seen* so the damage is reported, not hidden. |

## Extents and forks

| | |
| --- | --- |
| Apple | `core/hfs_extents.c` `hfs_ext_realloc`, `hfs_ext_iter_init`, `hfs_ext_iter_next_group`; `core/FileExtentMapping.c` `MapFileBlockC`; `core/hfs_format.h` `struct HFSPlusForkData` |
| Structures | `HFSPlusForkData`, `HFSPlusExtentRecord`, `HFSPlusExtentDescriptor` |
| Invariants | an overflow group's key is the running count of blocks already described; `totalBlocks` counts inline *and* overflow blocks |
| Rust | `src/format/fork.rs`, `src/format/extents.rs`, `src/extent/mapper.rs`, `src/file/mod.rs` |
| Differences | `MapFileBlockC` returns whatever `SearchExtentFile` finds and errors if it finds nothing — there is no zero-fill path. So `logical_size` beyond the blocks behind it is not a sparse file but a corrupt record, and `ForkData::validate` refuses it. |

## Allocation

| | |
| --- | --- |
| Apple | `core/VolumeAllocation.c` `BlockFindAny`, `BlockAllocateXxx`, `BlockDeallocateXxx`, `hfs_isallocated` |
| Structures | the allocation bitmap, MSB first |
| Invariants | the reserved prefix below the allocation file's first extent, and the reserved tail above `allocLimit`, are never handed out |
| Rust | `src/alloc/mod.rs`, `src/volume/bitmap.rs` |
| Differences | no free-extent cache, no summary table, no speculative `HFS_ALLOC_TENTATIVE`. The search is a plain first-fit from a hint with one wrap, which is `BlockFindAny` without the caching that only pays off under load. |

## Journal

| | |
| --- | --- |
| Apple | `core/hfs_journal.c` `replay_journal`, `journal_open`, `journal_is_clean`, `calc_checksum`, `AddBlockToCoalesceBuffer`; `core/hfs_journal.h` |
| Structures | `JournalInfoBlock`, `journal_header`, `block_list_header`, `block_info` |
| Invariants | `binfo[0]` is the transaction sequence slot, not a block, so replay starts at index 1; a block's `bnum` is in `jhdr_size` units, not the volume's block size; the journal is a ring and a read may wrap; damage truncates to `txn_start_offset` and aborts only when no good transaction exists |
| Rust | `src/journal/` |
| Differences | no retry loop. Apple restarts with `end = txn_start_offset` and gives up after three attempts, which yields the same transactions as truncating once; it differs only for a transient device error. No conversion of legacy `'JHDR'` magic in place — that is a write, and this crate does not write. |

## Timestamps

| | |
| --- | --- |
| Apple | `core/MacOSStubs.c` `to_bsd_time`, `to_hfs_time` (duplicated in `livefiles_hfs_plugin/lf_hfs_utils.c`); `core/hfs_format.h` `struct HFSPlusBSDInfo` |
| Structures | the five `u32` timestamps in every catalog record |
| Invariants | classic mode is seconds from 1904-01-01 with the Unix epoch 2,082,844,800 seconds later; pre-epoch values clamp to zero rather than going negative; zero means "never set" and must not be shifted; a volume with `kHFSVolumeHasExpandedTimesMask` stores Unix seconds already |
| Rust | `src/timestamp.rs` |
| Differences | the native representation is `HfsTimestamp`, not `SystemTime`. `SystemTime` cannot represent 1904 and has no "unset", and converting through it would lose both the clamp and the zero-means-unset rule. |

## Checker

| | |
| --- | --- |
| Apple | `lib_fsck_hfs/dfalib/SVerify1.c`, `SVerify2.c`, `SUtils.c` `AllocBTN`, `VolumeBitmapCheck.c`, `CatalogCheck.c` `CheckFileData` |
| Structures | none — this is validation logic, not on-disk layout |
| Invariants | the checks themselves, which are cross-read from the independent implementation rather than invented |
| Rust | `src/check/mod.rs`, `src/bin/hfsck.rs` |
| Differences | read-only, and deliberately so. `fsck_hfs` repairs as well as reports, and pointing it at a fixture once undid the corruption it was diagnosing. There is no repair mode to mis-invoke. |

An important caveat, recorded because it decides what "checker agrees" means:
`hfsprogs` is an *unofficial* port carrying 111 of Apple's 119 `fsck_hfs`
messages. Seven checks are absent, including all symlink validation. So passing
`fsck.hfsplus` is evidence of agreement with an implementation derived from
Apple's — not proof of Apple compatibility.

# Not yet mined

Milestones 7 through 13 depend on all of these, and none has been translated:

| Area | Apple | Will become |
| --- | --- | --- |
| Attributes File | `core/hfs_attrlist.c`, `hfs_attrlist.h` | `src/attributes/` |
| Resource fork semantics | `core/hfs_xattr.c`, `FileMgrInternal.h` | 7A.1 |
| Catalog mutation | `core/hfs_catalog.c` `cat_create`, `cat_delete`, `cat_rename`, `cat_update`, `catrec_update`, `buildkey`, `buildrecord`, `buildthread` | Milestone 9 |
| Hard links | `core/hfs_catalog.c` `cat_createlink`, `cat_lookuplink`, `cat_lookup_siblinglinks`, `cat_lookup_lastlink` | Milestone 10 |
| Compression metadata | `core/hfs_attrlist.c`, `core/hfs_cnode.c` (`decmpfs`) | 7B.2 |
| B-tree mutation | `core/BTreeWrapper.c` `InsertRecord`, `SplitRecord`, `BTUpdateRecord`; `core/hfs_btreeio.c` | Milestone 8 |
| Fork mutation | `core/hfs_readwrite.c` | Milestone 8A, 8B |
| Journal writes | `core/hfs_journal.c` `write_journal_header`, `end_transaction` | Milestone 12 |

Until those rows are filled, this crate can read and check an HFS+ volume. It
cannot change one.
