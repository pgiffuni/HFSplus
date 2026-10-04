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

## Attributes File

| | |
| --- | --- |
| Apple | `core/hfs_format.h` `struct HFSPlusAttrKey`, `HFSPlusAttrData`, `HFSPlusAttrForkData`, `HFSPlusAttrExtents`, `union HFSPlusAttrRecord`, and the `kHFSPlusAttr{InlineData,ForkData,Extents}` enum |
| Structures | `AttrKey`, `AttrRecord` |
| Invariants | `keyLength` excludes itself but includes the `pad` that follows it, so the body is 266 and the record 268; the type values 0x10/0x20/0x30 are disjoint from the catalog's 1..4 and 1000; a fork attribute's value continues through further records chained by the key's `startBlock` |
| Rust | `src/attributes/key.rs`, `src/attributes/record.rs`, `src/attributes/mod.rs` |
| Differences | the `pad` is read and discarded rather than validated — Apple writes zero and reads nothing, so refusing a non-zero value would reject volumes macOS mounts. The obsolete `HFSPlusAttrInlineData` spelling decodes identically, since only the struct name changed and not the type value or the layout. |

Two layout errors here were the same shape as the extent key's, which is now
worth naming as a habit rather than a coincidence:

- the key body was computed without the `pad`, giving 264 rather than 266
- `HFSPlusAttrData.reserved` is an *array* of two, so `attrSize` is at offset 12
  and the value at 16; reading them four bytes early took the length out of the
  reserved field

Both were caught by unit tests rather than by the corpus, because no image in it
carries an attribute — `mkfs.hfsplus` creates no files.

Four rules the fixture had to get right, none of them obvious and two of them
learned by having `fsck.hfsplus` reject the image:

- **Keys order by CNID, then name length, then name content, then startBlock.**
  Length before content is the surprising part: `"z"` sorts *after* `"abc"` however
  its letters fall. A tree that compares names as strings is wrong in a way that
  only shows on names of differing length.
- **A `Fork` record's key must have `startBlock == 0`**, and a continuation
  record's key must carry the number of blocks described *so far* — a running
  count, not a block number. The same convention the catalog's extents overflow
  key uses, and the same easy mistake in both places.
- **A file with attributes must say so.** The `kHFSHasAttributesMask` flag in the
  catalog record is the file's claim that the tree holds something for it, and
  `fsck` compares that count against what the tree contains. A volume where a file
  has attributes but does not declare them reads correctly and fails its own
  consistency check.
- **Allocating blocks means updating three things**: the bitmap, the volume
  header's free count, and — for an attribute — the declaring flag.

And one this crate got wrong, which is the kind worth recording: the checker
walked catalog forks and the allocation bitmap but **not the attributes tree**, so
every block holding a forked attribute value looked orphaned. That is not a
hypothetical: a FinderInfo large enough to be forked is unremarkable on a real
macOS volume, so the checker would have reported a false positive on real media
rather than only on a fixture.

Three distinctions this module exists to keep straight, since they are routinely
conflated:

```text
data fork        catalog record's dataFork
resource fork    catalog record's rsrcFork   -- a real fork, not an attribute
FinderInfo       an attribute in this tree
```

FinderInfo is the one that surprises: the catalog record's `HFSPlusBSDInfo` is 16
bytes with **no** FinderInfo field. HFS+ kept FinderInfo in the attributes tree,
where classic HFS had no equivalent to move it to. A POSIX extended attribute is
a fourth thing again — which attributes become `getxattr` is a decision for the
FUSE adapter, not for this module.

## Resource forks and FinderInfo

| | |
| --- | --- |
| Apple | `core/hfs_format.h` `struct HFSPlusCatalogFile`; `core/hfs_xattr.c` `hfs_vnop_getnamedstream`; `core/hfs_readwrite.c` `hfs_read` |
| Structures | `ForkData` for both forks; `FileAttrs` carries both |
| Invariants | a resource fork is a real fork — allocation, extents overflow and truncation apply to it as to any other; its *name* at the POSIX boundary is unrelated to that |
| Rust | `src/volume/mod.rs` `Object` documents the four-way split; `src/attributes/names.rs` |
| Differences | none yet: mutation does not exist. What is new is the distinction being written down where it is read. |

Three findings, none of which the crate had recorded:

- **A resource fork is a catalog fork, not an attribute.** macOS also exposes it
  as `com.apple.ResourceFork`, and that is the only stream `getnamedstream`
  supports -- it answers `ENOATTR` for every other name. So the name belongs to
  the POSIX boundary, and modelling the fork internally as an xattr would lose
  the fork identity that allocation depends on.
- **FinderInfo is not in the catalog record at all.** The 16-byte
  `HFSPlusBSDInfo` has no FinderInfo field. HFS+ kept FinderInfo in the
  attributes tree, where classic HFS had no equivalent to move it to. Earlier in
  this project FinderInfo's placement was assumed from classic HFS, and the
  assumption was wrong in a way nothing would have caught.
- **A compressed file's data fork does not contain the file's contents.** It
  contains decmpfs data; the logical bytes come from decompressing it. And
  `hfs_hides_rsrc` means such a file's resource fork is *reported empty* rather
  than read -- so "the resource fork is empty" can mean hidden, not absent, and
  "the data fork is short" can mean compressed rather than truncated.

## Compression metadata

| | |
| --- | --- |
| Apple | `core/hfs_vnops.c` `hfs_vnop_listxattr`, `hfs_vnop_getxattr`; `core/hfs_readwrite.c` `hfs_read`; the decmpfs reader is not vendored here |
| Structures | a decmpfs disk header, carried as an attribute's value |
| Invariants | the attribute is hidden from the extended-attribute interface |
| Rust | `src/attributes/names.rs` — the name, and `is_compressed` |
| Differences | nothing decodes a decmpfs payload, by design |

Where the metadata lives: an attribute named `com.apple.decmpfs`, **filtered out
of `listxattr` and `getxattr`**. So a reader that enumerates attributes does not
see it, and one that reads the data fork gets compressed bytes rather than the
file's contents.

Two wrong answers this produces, both silent:

- "the data fork is shorter than the logical size" can mean compressed, not
  truncated.
- "this file has no attributes" can mean it has compression metadata that was
  hidden.

The name is **corroborated, not mined**: `core/` uses the macro
`DECMPFS_XATTR_NAME` but its definition is in a decmpfs header this tree does not
vendor, and `livefiles_hfs_plugin/lf_hfs_vnode.c` spells the same literal. Two
implementations agreeing is weaker evidence than the authority, and it is recorded
as such rather than presented as mined.

Decoding is deliberately absent. The roadmap's instruction is not to implement
compression mutation because the metadata can be parsed, and the read side has the
same shape of trap: a reader that meets a compressed file must say so rather than
serve compressed bytes as if they were the file.

The other attribute names HFS+ writes for its own bookkeeping are pinned in
`src/attributes/names.rs`, verified against the source rather than recalled: they
are exact, case-sensitive strings that are part of the on-disk key, and no case
folding applies to them.

## Writable volume

| | |
| --- | --- |
| Apple | `core/hfs_vfsops.c` `hfs_mount_existing`; `core/hfs_vfsutils.c` `hfs_mount_hfsplus` |
| Structures | none — a capability, not a layout |
| Invariants | every structural check and the journal replay run before any write is possible; a volume whose journal cannot be replayed does not mount |
| Rust | `src/volume/mod.rs` `WritableVolume`, and `BlockDeviceMut` in `src/blockdev/mod.rs` |
| Differences | the type now owns a `&mut D` rather than borrowing a `Volume`, so that a write can reach the bytes at all. `from_validated` is gone with it. |

Two separate axes, which the roadmap is right to keep apart:

- **Write capability** is `BlockDeviceMut`, a trait `Volume` is not bounded by. A
  read-only code path cannot acquire it by accident.
- **Established currency** is `WritableVolume`. Reading a journaled volume that
  has not been replayed serves a *stale* filesystem; that is a legitimate choice
  with a visible cost. Changing it is not, because a write lands on a filesystem
  the writer never saw.

`WritableVolume::open` validates by opening a temporary `Volume` over a shared
reborrow of the device, keeping the header and whether a journal was replayed,
and dropping the temporary before taking the mutable borrow. So there is still
exactly one validation path and it is `Volume::open`, which cannot drift from the
read route -- but the handle owns the device rather than borrowing a `Volume`,
because `&D` and `&mut D` at once is not borrowable. It is also therefore an
*exclusive* handle: there is no accessor returning a `Volume`, because every
cached view is invalidated by the first successful write, and a view handed out
from here would be a way to read stale data while holding a writer.

It records whether a journal was replayed, because a writer needs that fact told
to it rather than inferring it. It is always `false` today, since `open` refuses
a journaled volume; it is kept because the refusal is a property of what is
implemented, not of the volume.

## First mutation: file contents in place

| | |
| --- | --- |
| Apple | `core/hfs_cnode.c` `hfs_update` (`c_touch_modtime`, `c_touch_chgtime`); `core/hfs_vfsops.c` `hfs_bwrite`; `core/hfs_readwrite.c` `do_hfs_truncate` for the size half |
| Structures | none new. `FileRecord::write_to` and `BsdInfo::write_to` serialise what `parse` already deserialises; `ForkData::write_to` and `ExtentRecord::to_bytes` already existed |
| Invariants | data blocks are written *before* the catalog record that names their length, so an interrupted write leaves a file that reads as its old contents rather than as torn new ones; the record is replaced only at the same length, because a length change moves every later record in the node |
| Rust | `src/volume/mod.rs` `write_file_contents`, `replace_catalog_record`, `find_catalog_record`; `src/catalog/record.rs` `FileRecord::write_to` |
| Differences | refuses growth, refuses a length change, and refuses a journaled volume. Each refusal names the reason rather than degrading |

Two things this got wrong before it was right, both found by asking what
`fsck.hfsplus` says rather than by reading the code again:

- The CNID of a file is `fileID` in the record *body*, not the key's parentID --
  the key names the containing folder. Only a *thread* record keys on the object
  itself. Matching the key found the root folder for every file on it.
- A node's byte address comes from the fork's extent mapper (`node_offset`), not
  from `node_num * node_size`. Those agree only while a fork is contiguous from
  block 0, and the second write went to the wrong place silently.

And one from synthesising the fixture rather than reading a real image:

- `fileMode` is a `u16`. Packed as a `u32` it wrote four bytes and overran into
  the next record. `FileRecord::write_to` now has a test asserting a record
  re-encodes to exactly the bytes it was parsed from -- a round trip through the
  parser would not have caught it, because the result still parsed.

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
| Resource fork semantics | `core/hfs_xattr.c`, `FileMgrInternal.h` | 7A.1 |
| Catalog mutation | `core/hfs_catalog.c` `cat_create`, `cat_delete`, `cat_rename`, `cat_update`, `catrec_update`, `buildkey`, `buildrecord`, `buildthread` | Milestone 9 |
| Hard links | `core/hfs_catalog.c` `cat_createlink`, `cat_lookuplink`, `cat_lookup_siblinglinks`, `cat_lookup_lastlink` | Milestone 10 |
| Compression metadata | `core/hfs_attrlist.c`, `core/hfs_cnode.c` (`decmpfs`) | 7B.2 |
| B-tree mutation | `core/BTreeWrapper.c` `InsertRecord`, `SplitRecord`, `BTUpdateRecord`; `core/hfs_btreeio.c` | Milestone 8C |
| Fork allocation | `core/hfs_readwrite.c`; `core/VolumeAllocation.c` `BlockFindAny` | Milestone 8A, 8B |
| Journal writes | `core/hfs_journal.c` `write_journal_header`, `end_transaction` | Milestone 12 |

Until those rows are filled, this crate can change a file's existing bytes and
nothing else: it cannot grow a file, create or remove one, or write to a volume
with a journal.
