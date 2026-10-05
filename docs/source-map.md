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
| Differences | refuses a length change, and refuses a journaled volume. Growth is implemented (see below) |

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

Milestones 9 through 13 depend on all of these. Resource fork semantics (7A.1) and
compression metadata (7B.2) are done and appear above.

| Area | Apple | Will become |
| --- | --- | --- |
| Catalog record updates | `core/hfs_catalog.c` `cat_update`, `catrec_update`, `buildrecord`; `core/hfs_xattr.c` | Milestone 9B |
| A rename between two spellings of one name | `core/hfs_catalog.c` `cat_rename`'s `btExists` path | done |
| Moving a folder beneath itself | `core/hfs_catalog.c` `cat_rename`'s cycle check | done |
| Attribute-list and FinderInfo writes | `core/hfs_xattr.c` | Milestone 11 |
| **The private hardlinks folder** | `core/hfs_link.c` `hfs_private_names`, `HFSPLUSMETADATAFOLDER` in `core/hfs_format.h` | Milestone 10, first |
| **Creating** a hard link | `core/hfs_catalog.c` `cat_createlink`; `core/hfs_link.c` `hfs_makelink` | blocked -- `fsck` clears the chain flag |
| **Threading** a second link | `cat_lookup_lastlink`, `cat_lookup_siblinglinks`, `hl_firstLinkID` | blocked -- see below |
| **The firstlink attribute** | `core/hfs_link.c` `setfirstlink`/`getfirstlink`, `FIRST_LINK_XATTR_NAME`; directory links only | Milestone 10 |
| **The attributes-file writer** | `core/hfs_xattr.c` | Milestone 11 |
| **Unlinking** through a link | `cat_delete` refusing a record with siblings | Milestone 10 |
| Extents overflow | `core/hfs_extents.c` `extents_search`; overflow records | Milestone 8D |
| Splitting an index node | `core/BTreeNodeOps.c` `SplitRecord`, `SplitLeafNode`; `core/BTree.c` `BTInsertRecord`'s split path | Milestone 8G |
| Freeing B-tree nodes | `core/BTreeAllocate.c` `ReleaseNode`, `free_nodes` | Milestone 8F |
| The metadata zone | `core/VolumeAllocation.c` `HFS_METADATA_ZONE`, `hfs_metazone_end`; `core/hfs_meta_zone.c` | not planned |
| Journal writes | `core/hfs_journal.c` `write_journal_header`, `end_transaction` | Milestone 12 |

### The "exchange" is not an exchange

`cat_rename`'s `btExists` path was recorded here as a same-parent rename becoming an
*exchange*, which is wrong, and reading it properly is worth the correction.

The path allows the collision in exactly one case: after the insert reports that the
destination exists, it searches there and compares — and proceeds only when

```c
if ((fromtype != recp->recordType) || (from_cdp->cd_cnid != cnid)) {
        result = EEXIST;
        goto exit;
}
/* The old name is a case variant and must be removed */
```

Same record type *and* same CNID. Anything else is `EEXIST`. So the case is not two
objects swapping names; it is **one object under two spellings**, which on a
case-insensitive volume are the same key to the tree. The comment says so: "the old
name is a case variant".

That makes it a re-key rather than a move, and it is why the ordering differs. The
insert runs *before* the remove — the right order in general, because the new record
carries the old body and nothing has to be reconstructed if the insert fails. But for
a re-key the insert finds the very record that is about to leave and refuses it as a
duplicate, so this case has to remove first, and the bytes are already read so a
failure puts them straight back.

**Implemented**, after two attempts that were reverted and a third that worked. What
the three established, in the order they were learned:

- **The committed tree already refused the case-folded collision and lost nothing**,
  which is now a test. That was worth knowing first: it said the hazard was a
  property to keep rather than a bug to chase.
- **A key survives the on-disk encoding.** This was the *suspected* cause -- a folded
  lookup builds its search key through the same bytes the tree stores, so a lossy
  round trip would explain a search for a key that is not the one asked for. It is
  exact, so the hypothesis was wrong, and ruling it out is worth as much as having
  had it.
- **The actual cause was a double removal.** The re-key branch removed the old record
  and then step 2 removed it *again*, which correctly reports `NotFound` -- for a
  record the branch had just deleted itself. The error named the remove, and pointed
  at the remove, and the bug was in the remove.

That last one is the lesson worth keeping: two attempts failed with an error that
looked like a lookup problem, and the answer was one line of control flow that a
careful read of the diff would have found immediately. Two failed attempts produced a
*sharper* hypothesis than either, and it was the control flow -- but the cheapest
next step after the first failure was to read what had just been written.

A folder may also not be moved beneath itself or any of its own descendants.
`cat_rename` refuses the obvious cases outright -- the root, the destination folder
itself, and the destination's own parent -- and then traverses the destination path
"all the way back to the root making sure that source directory is not encountered".
The walk goes up through thread records, since those are the only place a folder
records its parent, and it is bounded by depth rather than by a visited set: a
corrupted cycle in the thread records would otherwise loop forever, and a depth bound
answers "not an ancestor", which leaves the caller's own check to catch it. Only two
of the three illegal moves are caught by comparing the folder with the destination
alone, which is what the walk is for.

A re-key also needs the steps the other way round, because the two spellings are one
key to the tree: inserting the new one before removing the old finds the record that
is about to leave and refuses it as a duplicate. The bytes are already read for the
rollback, so that ordering is safe. `a_case_variant_rename_is_a_rekey_rather_than_a_move`
exercises it both ways, and asserts the record *count* as well -- a re-key done as a
move would leave four entries where there should be three.

## Hard links: what `fsck` says, and why writing one is blocked

Hard links are the next milestone and they are **not started**. The reading side is
already in place from an earlier milestone -- `is_hard_link()`, `link_count()`,
`link_reference()`, `first_link_id()`, and a `BsdInfo` whose fields already
document that `ownerID` is "`ownerID`, or `prevLinkID` for a hard link". What is
missing is the private hardlinks directory, and a first attempt at the writing side
established exactly why.

### What was built and measured

A `create_hard_link` following `cat_createlink` -- a new CNID, the thread record
inserted **first**, a file record carrying the target's forks and permissions with
`kHFSHasLinkChainMask` set and `hl_linkReference` naming the indirect node -- and a
refusal for a second link, since threading a chain needs the head that lives in the
private directory.

`fsck.hfsplus` on the result, first run on a genuinely fresh image:

```
File record has hard link chain flag (id = 18)
File has incorrect number of links (id = 18)  (It should be 1 instead of 17)
File has incorrect number of links (id = 17)  (It should be 1 instead of 2)
```

### One bug found, and fixed

The first run also said:

```
Overlapped extent allocation (id = 17, /orig.bin)
Overlapped extent allocation (id = 18, /alias.bin)
Invalid volume free block count  (It should be 218 instead of 219)
```

The overlap is a real bug and an instructive one: the link's file record was built
from the target's, **extents included**, so two catalog records described the same
blocks -- allocated once, claimed twice, with nothing on the way back to say which
claim to believe. **A file hard link is a name, not a second owner of the data.**
Both forks on the link's record must be empty. Fixing that removed the overlap *and*
the free-block-count complaint, and is worth keeping whatever else happens.

### What mining `hfs_link.c` established, and what a measurement then changed

`core/hfs_link.c`'s first comment says where the chain head lives:

```c
/*
 * Private directories where hardlink inodes reside.
 */
const char *hfs_private_names[] = {
        HFSPLUSMETADATAFOLDER,      /* FILE HARDLINKS */
        HFSPLUS_DIR_METADATA_FOLDER /* DIRECTORY HARDLINKS */
};

/*
 * Hardlink inodes save the head of their link chain in a
 * private extended attribute.
 */
static int  setfirstlink(struct hfsmount * hfsmp, cnid_t fileid, cnid_t firstlink);
```

So three facts, and they are *different* from what a checker's output alone
suggested:

- **Link records live in a private folder, named by their own CNID.**
  `HFSPLUSMETADATAFOLDER` is four U+2500 BOX DRAWINGS LIGHT HORIZONTAL followed by
  "HFS+ Private Data" -- 21 UTF-16 units, 29 UTF-8 bytes.
  `HFSPLUS_DIR_METADATA_FOLDER` is ".HFS+ Private Directory Data" plus CR. The
  trailing CR is in Apple's definition and is precisely what gets lost transcribing a
  `#define` into a doc comment and back, so both names are asserted in the tests.
  `hfs_makelink` *renames* a link inode in there rather than leaving it where the
  user asked for it, and `cat_lookup_siblinglinks` special-cases a link whose parent
  is that folder.
- **The chain head is marked on the first link, pointing at itself.**
  `hfs_makelink` sets `ca_firstlink = linkcnid` immediately after
  `cd_cnid = linkcnid`.
- **Only *directory* links use an extended attribute** for the head:
  `com.apple.system.hfs.firstlink`, holding the CNID as a **decimal string** --
  `snprintf(..., "%lu", firstlink)`.

### Three corrections, measured

A first attempt at `create_hard_link` got three things wrong, each caught by
`fsck.hfsplus`:

- **A regular file's `linkCount` is 1, not 0.** `special` is `hl_linkCount` on a
  record that is not a link, and Apple says "set linkCount to 1 for regular files".
  Zero is not "no links" so much as "never counted" -- and it silently disables a
  guard of the form "refuse if the count is above 1". Fixed.
- **The user's folder gains no catalog child.** The link's record lives in the
  private folder, so there is nothing in the folder the user named to count.
  Bumping it as well makes `fsck` report "Invalid directory item count (It should be
  3 instead of 4)" for the root. Fixed.
- **A link's `special` is 1, not the reference.** `fsck` reads a link record's count
  out of that same union member and reports "It should be 1 instead of 17" when it
  holds the reference. So a link does not carry `hl_linkReference` there, which
  contradicts `#define hl_linkReference bsdInfo.special.iNodeNum`. **Unresolved.**

One more, from the very first attempt: **a link's forks must be empty.** Copying
the target's extents into the link's record makes two catalog records describe the
same blocks -- allocated once, claimed twice -- which is "Overlapped extent
allocation", and which nothing on the way back can adjudicate. A link is a *name*,
not a second owner of the data.

### What is still missing, precisely

With all four fixed, **every count complaint disappears**. `fsck` reports only

```
File record has hard link chain flag (id = 19)
```

and then repairs the image, with no complaint attached. Diffing the repair against
the original shows what it does:

```
offset 111601 (catalog block 27, within 1009):  ours=0x20  fsck=0x00
```

`0x20` is `kHFSHasLinkChainMask`, in the middle of a catalog record. So `fsck`
**clears the chain flag**: it does not accept the record as a link, and it does so
quietly, which is worse than a complaint. (The other eight differing bytes are
`fsck` writing its own "fsc.k" signature into the primary and backup volume headers,
which is not a repair at all.)

| | |
| --- | --- |
| The private folder | created, Apple's exact name, `fsck` accepts it with **zero** differences |
| The link's location | inside the private folder, named by its own CNID in decimal |
| Its forks | empty |
| `hl_firstLinkID` | the head link points at itself |
| `hl_linkReference` | `special` per `hfs_format.h`, but `fsck` reads `1` there -- **unresolved** |

So `create_hard_link` is **not** written. The folder it needs is, and the link-count
fix that came out of the attempt is, but a writer whose records `fsck` quietly
rewrites is not something to ship.

The next step is the unresolved row, and the way in is the diff: `fsck` is deleting
the chain flag, so it does not think that record is a link. That points at the
*flags* or the record's position rather than at `special` -- and the first thing to
re-examine is whether the record is where `fsck` looks for one. `hfs_makelink`'s
rename is into `hfs_private_desc[FILE_HARDLINKS]`, and `cat_lookup_siblinglinks`
identifies such a link by its *parent* being that folder's CNID, so a link whose
parent is anything else is not a link as far as Apple is concerned either.

### A trap worth naming

`fsck.hfsplus` **modifies the image it checks** -- `AGENTS.md` says so, and it cost
two rounds of false confidence here. A probe that ran the checker on its own output
and then diffed a copy of the *repaired* image reported "appears to be OK" with zero
differences, which read as success for a volume the checker had just rewritten. The
only trustworthy measurement is a checker run against an image nothing has touched,
and the diff has to come from a copy made *before* that run.

## What Milestone 8 has and has not reached

**Milestone 9 is complete.** Done and `fsck`-verified: overwrite, grow and truncate
a file's contents within its eight inline extents; create a file, create a folder,
rename or move either, and remove either -- each keeping the file record, its
thread record, the containing folder's child count and the volume header's counters
in step, and each all-or-nothing; insert, remove and split a node; grow the catalog
when a split runs out of nodes.

Four things that were true only of the first of those, and are now true of all of
them: a create writes its CNID counter first and does not roll it back, so a
failure can never hand the same CNID out twice; the catalog records go in or out
together, so a file record with no thread record is unreachable rather than merely
inconsistent; and the volume header's backup copy at the end of the image moves
with the primary, because `fsck.hfsplus` compares them and repairs the stale one.

Three limits are structural rather than unfinished, and each is refused by name
rather than approximated:

- **The catalog grows, but in clumps, and only eight times.** `ExtendBTreeFile`
  raises any request below the fork's clump size to it, so a tree needing one more
  node grows by eight on this corpus; and the catalog's extents live in the volume
  header, which has eight inline slots. Past that the answer is a refusal naming the
  extents B-tree (Milestone 8D). The 1 MiB corpus volumes run out of contiguous
  clumps long before that.
- **A fork cannot overflow into the extents tree.** Nine extents and the answer is
  a refusal naming 8D. That is also what stops a catalog growing indefinitely.
- **Nothing can be written to a journaled volume.** `WritableVolume::open` refuses
  one, because a write that is not journalled leaves a journal that does not
  describe the volume.

Two things are refused for a different reason, because the alternative is worse
than a refusal. A catalog more than two levels deep, whose parent index node would
itself need splitting, is Milestone 8G; and a leaf split divides by bytes, so a
tree that would end up three levels deep is refused rather than half-split.

One bug was found by a *boundary* rather than by reasoning, and is worth recording
for that reason: a catalog was accepted at forty files and rejected at forty-one,
with nothing else different about the tree. An index separator is the first key of
the subtree it points at, so inserting a record at the front of a leaf moves it
without any node changing hands -- and a stale separator is still a valid key, still
in order, and still bounds keys that live in that leaf, so nothing local detects
it. `fsck.hfsplus` reports "Invalid index key". See `docs/hfs-format.md`.
