# On-disk format notes

Working notes from mining Apple's `core/` sources. Each entry states the finding,
why it is easy to get wrong, and the Apple source that settles it. Apple's
repository is the authority: <https://github.com/apple-oss-distributions/hfs> at
commit `d1bac2f062e6e9c0dfcce302d9aacb10173d0eea` (mirrored, at the same
commit, at <https://github.com/pgiffuni/apple-hfs>).

## Volume header placement

The volume header is at byte offset **1024**, i.e. the second 512-byte sector.

- `core/hfs_format.h` — `struct HFSMasterDirectoryBlock`, `drEmbedSigWord`.
- `core/hfs_vfsutils.c` — `hfs_MountHFSPlusVolume`.

Sector 0 is the driver descriptor on a partitioned disk and the classic MDB slot
is reused for the HFS+ header on a standalone image. On a wrapper disk the MDB's
`drEmbedSigWord` at offset `0x7C` announces an embedded HFS+ volume that follows.

## Signatures and versions are validated as a pair

```
kHFSSigWord     = 0x4244  ('BD')   classic HFS
kHFSPlusSigWord = 0x482B  ('H+')   HFS+,  requires kHFSPlusVersion = 4
kHFSXSigWord    = 0x4858  ('HX')   HFSX,  requires kHFSXVersion    = 5
```

`core/hfs_vfsutils.c` `hfs_ValidateHFSPlusVolumeHeader` branches on the signature
and then requires the matching version. An HFS+ signature with version 5 is
**not** an HFSX volume; it is a corrupt HFS+ volume.

`kHFSSigWord` is rejected by the HFS+ validator even though it is a genuine HFS
family signature. Recognising it and refusing it is the correct behaviour.

## There is no volume name in the volume header

This is the easiest mistake to make, and it is invisible until the arithmetic
does not add up.

`struct HFSPlusVolumeHeader` has 112 bytes of scalars followed by five
`HFSPlusForkData` structures of 80 bytes:

```
112 + 5 * 80 = 512 = exactly one sector
```

Any extra 28-byte field would overflow the sector. There is no name field.

The **volume name is the name of the root folder**, which lives in the catalog
B-tree as a `cndrDir` record with CNID `kHFSRootFolderID = 2`.
`core/hfs_vfsutils.c` `hfs_MountHFSPlusVolume` does exactly that:

```c
retval = cat_idlookup(hfsmp, kHFSRootFolderID, 0, 0, &cndesc, &cnattr, NULL);
bcopy(cndesc.cd_nameptr, vcb->vcbVN, min(255, cndesc.cd_namelen));
```

Consequences for this crate:

- Field offsets: scalars `0..112`, `allocationFile` at `112`, `extentsFile` at
  `192`, `catalogFile` at `272`, `attributesFile` at `352`, `startupFile` at
  `432`.
- `hfsinspect` reports the root folder CNID, not a name, until the catalog is
  parsed.

## Field offsets

Offsets are within the 512-byte header at image offset 1024.

| Offset | Field | Width |
| --- | --- | --- |
| 0 | `signature` | u16 |
| 2 | `version` | u16 |
| 4 | `attributes` | u32 |
| 8 | `lastMountedVersion` | u32 (four-char code, e.g. `10.0`) |
| 12 | `journalInfoBlock` | u32 |
| 16 / 20 / 24 / 28 | `createDate` / `modifyDate` / `backupDate` / `checkedDate` | u32 |
| 32 / 36 | `fileCount` / `folderCount` | u32 |
| 40 | `blockSize` | u32 |
| 44 | `totalBlocks` | u32 |
| 48 | `freeBlocks` | u32 |
| 52 | `nextAllocation` | u32 |
| 56 / 60 | `rsrcClumpSize` / `dataClumpSize` | u32 |
| 64 | `nextCatalogID` | u32 |
| 68 | `writeCount` | u32 |
| 72 | `encodingsBitmap` | u64 |
| 80 | `finderInfo` | 32 bytes |
| 112… | five `HFSPlusForkData` | 80 bytes each |

Confirmed against a live image: a 32 MiB volume reports `blockSize = 4096` at
offset 1064 and `totalBlocks = 8192` at offset 1068.

## Volume attribute bits

Mining reference: the volume attribute enum in `core/hfs_format.h`. Note the
numbering, which is not the obvious one.

| Bit | Mask | Name |
| --- | --- | --- |
| 7 | `0x00000080` | `kHFSVolumeHardwareLockMask` |
| **8** | **`0x00000100`** | **`kHFSVolumeUnmountedMask`** |
| 9 | `0x00000200` | `kHFSVolumeSparedBlocksMask` |
| 10 | `0x00000400` | `kHFSVolumeNoCacheRequiredMask` |
| 11 | `0x00000800` | `kHFSBootVolumeInconsistentMask` |
| 12 | `0x00001000` | `kHFSCatalogNodeIDsReusedMask` |
| 13 | `0x00002000` | `kHFSVolumeJournaledMask` |
| 14 | `0x00004000` | `kHFSVolumeInconsistentMask` |
| 15 | `0x00008000` | `kHFSVolumeSoftwareLockMask` |
| 29 | `0x20000000` | `kHFSExpandedTimesMask` |
| 30 | `0x40000000` | `kHFSContentProtectionMask` |
| 31 | `0x80000000` | `kHFSUnusedNodeFixMask` |

`kHFSVolumeUnmountedBit` is **bit 8**, not bit 15. Bit 15 is the software lock.
A freshly created volume from `mkfs.hfsplus` has `attributes = 0x80000100`,
which is unmounted **and** software-locked; reading bit 15 as "clean" gets both
halves wrong.

Bits 16-31 exist only in HFS+, since the classic MDB has just 16 attribute bits.
`kHFSMDBAttributesMask = 0x8380` selects the bits shared with classic HFS.

## Block size validation

`core/hfs_vfsutils.c` `hfs_ValidateHFSPlusVolumeHeader`:

```c
if (blockSize < 512 || !powerof2(blockSize)) return (EINVAL);
```

512 through 65536 are all legal in practice; the corpus covers 1024, 4096, 8192
and 16384.

## Alternate (backup) volume header

A second copy lives **1024 bytes before the end of the volume** — the
second-to-last 512-byte sector.

`core/hfs.h`:

```c
#define HFS_ALT_SECTOR(blksize, blkcnt) (((blkcnt) - 1) - (512 / (blksize)))
```

`core/hfs_vfsutils.c` evaluates it with the *logical* block size, always 512, so
it collapses to `total_sectors - 2`, i.e. byte offset `volume_bytes - 1024`.

This is emphatically **not** "one allocation block from the end". With 4096-byte
blocks the two differ by 3072 bytes, and the allocation-block reading lands on
zeros. Verified on a 32 MiB / 4096-byte-block volume: the second `0x482B`
occurrence in the image is at `33554432 - 1024 = 33553408`.

`hfs_MountHFSPlusVolume` also distinguishes *innocuous spare sectors* (partition
larger than filesystem by less than one allocation block) from the *degenerate*
case, in which it maintains **two** alternate headers: one 1024 bytes before the
end of the partition and one 1024 bytes before the end of the filesystem. For an
image whose partition and filesystem coincide — every image in this corpus — the
two positions are identical.

### It is written, not read

The alternate header exists so that a volume can be *repaired* after the primary
is damaged. It is **not** a mount fallback, and treating it as one is a mistake
worth recording before write support arrives — the sort of assumption a Linux- or
ZFS-shaped filesystem would invite.

`hfs_MountHFSPlusVolume` computes `hfs_partition_avh_sector` and
`hfs_fs_avh_sector`, but grepping the whole of `core/` finds no read of either at
mount: they are computed on the mount path and consumed by `core/hfs_resize.c`,
which writes them. So a volume whose primary header will not parse **does not
mount**, here or on macOS — `VolumeHeader::read_from` refusing it is correct
behaviour, not a missing feature, and `tests/images/malformed/` asserts it.

The corollary for a writer, when there is one: both alternate headers must be
updated on every write that changes the volume header, and the two positions
diverge as soon as the partition is larger than the filesystem.

## Extents overflow is keyed on allocated blocks, not logical size

`totalBlocks` is the number of allocation blocks a fork occupies, across all
extents including overflow records. The extents overflow B-tree is consulted only
when the eight inline descriptors do not account for all of them.

`core/hfs_extents.c` (`hfs_ext_realloc`) writes inline extents for the first
`kHFSPlusExtentDensity` (8) descriptors and then loops

```c
for (; ndx < count; ndx += 8) {
    BTInsertRecord(tree, &iter->bt_iter, &fbd, sizeof(HFSPlusExtentRecord));
    key->startBlock += hfs_total_blocks(&extents[ndx], kHFSPlusExtentDensity);
}
```

So the overflow key's `startBlock` is a **cumulative count of allocation blocks
already described by preceding groups** — an offset within the fork's block
space. It is not a physical block number and not a byte offset.

Consequence: a file with a large `logicalSize` and few allocated blocks
never touches the extents overflow B-tree. The unallocated region is a hole, not
an extent. Getting this wrong makes every sparse file look like it needs overflow
records.

`totalBlocks * blockSize` cannot overflow a `u64`: both factors are `u32`, so the
product always fits. The checked multiply is retained so that widening the
inputs later cannot introduce a silent wrap.

## Timestamps

Mac OS `u32` seconds since 1904-01-01 UTC; `MAC_GMT_FACTOR = 2082844800`
(`core/hfs.h`), the 66-year interval to 1970-01-01 plus 17 leap days.

Treating this as a plain subtraction is numerically right for post-1970 values
and wrong as a specification. `core/MacOSStubs.c` `to_bsd_time` /
`to_hfs_time` preserve three behaviours:

```c
time_t to_bsd_time(u_int32_t hfs_time, bool expanded)
{
    u_int32_t gmt = hfs_time;
    if (expanded) return (time_t) gmt;
    if (gmt > MAC_GMT_FACTOR) gmt -= MAC_GMT_FACTOR;
    else                       gmt = 0;  /* don't let date go negative! */
    return (time_t) gmt;
}
```

1. Pre-epoch timestamps **clamp to zero**, they do not go negative. A 1960 file
   reports as the Unix epoch.
2. In classic mode, `to_hfs_time(0) == 0`: zero means *unset*, not 1904-01-01.
   Applying the offset would fabricate a date.
3. `kHFSExpandedTimesBit` (bit 29 of the volume attributes) means timestamps are
   *already* Unix seconds. No conversion, and zero is a real date.

`core/hfs_vfsutils.c` passes `attributes & kHFSExpandedTimesMask` into the
conversion for volume-header timestamps. A timestamp therefore cannot be decoded
without also knowing the epoch of the volume that owns it; `HfsTimestamp` keeps
the two together for that reason.

## Volume attributes and mount refusal

`core/hfs_vfsutils.c` refuses to mount a **dirty, non-journaled** volume
read-write:

```c
if ((hfsmp->hfs_flags & HFS_READ_ONLY) == 0 && hfsmp->jnl == NULL &&
    (SWAP_BE32(vhp->attributes) & kHFSVolumeUnmountedMask) == 0) {
    return (EINVAL);
}
```

This is why the unmounted bit must be read correctly: getting it wrong would let
a dirty volume be mounted writable and corrupted further.

## Journal presence

The `kHFSVolumeJournaledBit` attribute is authoritative. `journalInfoBlock` is
only meaningful when that bit is set, and on an unwrapped volume that field
overlaps spare space. `core/hfs_vfsutils.c` branches on the attribute before
consulting the block.

For read-only mounting, a journal must **never** cause the source image to be
modified. Journal replay belongs to a later milestone and must be a pure
function of the image.

## Content protection

`kHFSContentProtectionMask` (bit 30) says the volume *may* contain protected
files. It says nothing about whether any individual file is protected. That
metadata lives in the attributes B-tree and in `extended attributes`, and
understanding it is not the same as being able to decrypt anything: Apple's
`hfs_cprotect.c` is metadata handling, while the keys live outside the
filesystem. See `core/hfs_cprotect.c` when that investigation starts.

## B-tree node geometry

An HFS+ B-tree node is a fixed-size block with three regions:

```text
0                                                        node_size
+--------+--------------------------------------------+
| 14-byte|  records, growing UPWARD from offset 14     |
|  node  |                                             |
| descr. |                                             |
+--------+----------------------------------------------+
|                     free space                        |
+------------------------------------------------------+
|  offset array: (numRecords + 1) u16, growing DOWNWARD|
+-------------------------------------------+---------+
```

Two things here are easy to get backwards:

- The offset array sits at the **end** of the node and grows downward. Slot `i`
  is stored at `node + nodeSize - (i << 1) - 2`.
- Because slot *addresses* descend while record *offsets* ascend, record `i`
  occupies `[offset[i], offset[i + 1])`. Reading the ranges as
  `[offset[i+1], offset[i])` inverts every record.

Mining reference: Apple `core/BTreeNodeOps.c`:

```c
#define GetRecordOffset(btreePtr,node,index) \
    (*(short *) ((u_int8_t *)(node) + (btreePtr)->nodeSize - ((index) << 1) - kOffsetSize))

pos = (u_int16_t *) ((Ptr)node + btreePtr->nodeSize - (index << 1) - kOffsetSize);
return  *(pos-1) - *pos;                    /* GetRecordSize */
```

and `GetNodeFreeSize`, verbatim:

```c
freeOffset = GetRecordOffset (btreePtr, node, node->numRecords);
return btreePtr->nodeSize - freeOffset - (node->numRecords << 1) - kOffsetSize;
```

The node descriptor is not subtracted from the free space: it lies below the
records, outside that gap. `GetRecordOffset` itself does no bounds checking;
every accessor in `src/btree/node.rs` range-checks first, because `numRecords`
comes off the disk and the offset array holds only `nodeSize / 2` slots.

## B-tree node size is not the allocation block size

Node numbers are contiguous and fixed-size apart, so a node's byte offset within
its fork is `node_number * node_size`. That byte offset must then be split by the
volume's **allocation block size**, not by the node size, before the extent
mapper can be asked for a physical block.

On a default volume the two coincide: `mkfs.hfsplus` picks a 4096-byte
allocation block size *and* 4096-byte B-tree nodes. Every other volume in the
corpus separates them:

| Image | allocation block | catalog node | extents node | attributes node |
| --- | --- | --- | --- | --- |
| `basic-hfsplus` | 4096 | 4096 | 4096 | 8192 |
| `basic-hfsplus-1k` | 1024 | 4096 | 4096 | 8192 |
| `basic-hfsplus-8k` | 8192 | 4096 | 4096 | 8192 |
| `basic-hfsplus-16k` | 16384 | 4096 | 4096 | 8192 |

Two conclusions, both of which produced wrong code before being found:

1. **There is no rule that a volume's three B-trees share a node size.**
   hfsprogs formats the attributes tree with 8192-byte nodes regardless of the
   allocation block size.
2. Splitting the node offset by the node size instead of the allocation block
   size is *invisible on a default volume* and wrong everywhere else. It reads
   zeroes on `basic-hfsplus-8k`, which is how it was caught: the catalog's
   `first_leaf_node` appeared to be an empty index node instead of the leaf
   holding the root folder record.

## Opening a B-tree: the nodeSize chicken-and-egg problem

`nodeSize` lives inside node 0, and finding node `n` needs `nodeSize`. Apple
reads node 0 at the device's logical block size, parses the header record at
offset 14, and re-reads if the declared size differs:

```c
if ( btreePtr->nodeSize != nodeRec.blockSize ) {
    err = SetBTreeBlockSize (..., btreePtr->nodeSize, 32);
    ReleaseBTreeBlock (..., kTrashBlock);
    GetNode (btreePtr, kHeaderNodeNum, 0, &nodeRec);
}
```

Mining reference: Apple `core/BTree.c` `BTOpenPath`. `src/btree/io.rs`
`BTreeFile::open` performs the same sequence.

## B-tree header validation

`core/BTreeMiscOps.c` `VerifyHeader` rejects a header unless all of the
following hold, and `src/btree/header.rs` reproduces each rule:

| Check | Rule |
| --- | --- |
| `nodeSize` | one of 512, 1024, 2048, 4096, 8192, 16384, 32768 |
| `nodeSize` on HFS+ | must not be 512 |
| `totalNodes * nodeSize` | must not exceed the fork's logical size |
| `freeNodes` | must be below `totalNodes` |
| `rootNode`, `firstLeafNode`, `lastLeafNode` | must be below `totalNodes` |
| `treeDepth` | at most `kMaxTreeDepth` = 16 |
| `btreeType` | 0, 128.. (`kUserBTreeType`), or 255 |

The 512-byte rule is Apple asserting explicitly:

```c
PanicIf((...vcbSigWord != 0x4244) && (header->nodeSize == 512),
        " BTOpenPath: wrong node size for HFS+ volume!");
```

An empty tree has `treeDepth == 0` and `leafRecords == 0`. That is a valid
state, not a corrupt header, and `BTGetInformation` reports it faithfully.

## Key encoding

Every node record begins with a key. The length prefix is 16 bits when the tree
sets `kBTBigKeysMask` and 8 bits otherwise, and a key's on-disk size is the
prefix plus the body **rounded up to an even number**:

```c
if ( btreePtr->attributes & kBTBigKeysMask )
    keySize = keyLength + sizeof(u_int16_t);
else
    keySize = keyLength + sizeof(u_int8_t);
if ( M_IsOdd (keySize) )
    ++keySize;                    // add pad byte
```

Apple does not trust the stored attribute bit but re-derives it from the key
length, with an explicit admission that the attribute is unreliable:

```c
if ( btreePtr->maxKeyLength > 40 )
    btreePtr->attributes |= (kBTBigKeysMask + kBTVariableIndexKeysMask);
       // "we need a way to save these attributes"
```

So the threshold is **strictly greater than 40**. Measured across the corpus:
catalog `maxKeyLength` is 516 and attributes 264, so both are big-key trees;
the extents tree's 10-byte keys are not. `has_big_keys` in
`src/btree/key.rs` follows Apple's rule rather than the stored bit alone.

The extents and attributes *trees* are present on every volume and empty:
`mkfs.hfsplus` allocates both forks, but no file it creates is ever large enough
to overflow. So their key lengths are measurable, and `tests/journal_replay.rs`'s
sibling `tests/btree_conformance.rs` checks them against what the formatter wrote.
`tools/mkfiles.py` later fills the extents tree in deliberately.

## Key maximum lengths

| Tree | Constant | Value |
| --- | --- | --- |
| catalog | `kHFSPlusCatalogKeyMaximumLength` | 516 (`u32 parentID` + `HFSUniStr255` 512) |
| extents | `kHFSPlusExtentKeyMaximumLength` | 10 (`u8 forkType` + `u8 pad` + `u32 fileID` + `u32 startBlock`) |
| attributes | `kHFSPlusAttrKeyMaximumLength` | 266 (`u16 pad` + `u32 fileID` + `u32 startBlock` + `u16 attrNameLen` + `UniChar attrName[127]`)

All three are defined in `core/hfs_format.h` as `sizeof(Key) - sizeof(u_int16_t)`.

All three are also written into each tree's header by the formatter, so the
corpus is ground truth for them rather than a reading of the same header the
parser produces. Measured on `journaled-hfsplus`: catalog 516, extents 10,
attributes 266. Two of these were wrong here and no test noticed -- the extents
key dropped `forkType` and `pad`, the attributes key dropped `pad` -- because
nothing in the corpus ever decoded a key of either kind. `tests/
btree_conformance.rs` now checks all three against the corpus.

The extents value is the one worth stating in full, because the key is short
enough that the two middle fields look like padding and are not:

```c
struct HFSPlusExtentKey {
    u_int16_t  keyLength;   /* length of key, excluding this field */
    u_int8_t   forkType;    /* 0 = data fork, FF = resource fork */
    u_int8_t   pad;         /* make the other fields align on 32-bit */
    u_int32_t  fileID;
    u_int32_t  startBlock;
} __attribute__((aligned(2), packed));
```

`forkType` is what keeps the two forks of one file apart, since both share a
`fileID` and live in the same tree. Omitting it -- as an earlier revision here
did -- puts `fileID` at offset 2, where the fork type and pad byte actually are,
so every overflow lookup decodes the wrong key and matches nothing. A file past
its eighth extent then reads as all holes.

The keys are ordered by `fileID` and then `startBlock`; `forkType` is not part
of the ordering, so a file's data and resource extents sort adjacently.
`startBlock` is a *file* allocation block number, not a physical one, so the
second group of a fork with eight inline extents is keyed 8, the third 16, and
so on.

## Case sensitivity comes from keyCompareType, not the signature

The volume signature does not decide name comparison. The catalog B-tree's own
`keyCompareType` does. Measured across the corpus:

| Image | Signature | Catalog `keyCompareType` | Comparison |
| --- | --- | --- | --- |
| HFS+ volumes | `0x482B` | `0xCF` (`kHFSCaseFolding`) | case-insensitive |
| `hfsx-case-sensitive` | `0x4858` | `0xBC` (`kHFSBinaryCompare`) | case-sensitive |
| `hfsx-case-insensitive` | `0x482B` | `0xCF` | case-insensitive |

Mining reference: `core/hfs_format.h` defines `kHFSCaseFolding = 0xCF` and
`kHFSBinaryCompare = 0xBC`; `core/hfs_catalog.c` (`cat_binarykeycompare`)
dispatches on the value.

Note that `hfsx-case-insensitive` has an HFS+ signature, not an HFSX one:
`mkfs.hfsplus` only emits `kHFSXSigWord` for `-s`. An HFSX signature means the
volume supports case sensitivity, not that it is enabled.

### A real disagreement: keyCompareType in the other two trees

Apple's `newfs_hfs` writes `kHFSBinaryCompare` into the attributes tree header:

```c
bthp->keyCompareType = kHFSBinaryCompare;
```

Mining reference: Apple `core/hfs_btreeio.c`, in the attributes tree creation
path.

hfsprogs 540.1 writes **0** into both the attributes and extents tree headers.
Observed on every corpus image.

The two implementations disagree, so the disagreement is recorded rather than
resolved by preference. It is harmless in practice: those trees are keyed by
(CNID, attribute name) and (CNID, block offset), never by a user-visible file
name, so the comparison rule is never exercised. The consequence for this crate
is that an unrecognised `keyCompareType` must be **preserved verbatim** as
`KeyCompareType::Unknown(raw)` rather than coerced into a known variant.

## Volume name in the catalog

The root folder record — CNID `kHFSRootFolderID` = 2 — carries the volume name.
In every corpus image it sits at a fixed offset within the catalog's first leaf
node: 22 bytes into the record area, i.e. 14 bytes of node descriptor plus
`0x001C` of key prefix and key body. The two records in that leaf are the root
folder record and its thread record, which is what `leafRecords == 2` on a
freshly formatted volume means.

## Catalog record layout

| Record | Size | Key | Body carries |
| --- | --- | --- | --- |
| `kHFSPlusFolderRecord` (1) | 88 | `(parent, name)` | its own CNID in `folderID` |
| `kHFSPlusFileRecord` (2) | 248 | `(parent, name)` | its own CNID in `fileID` |
| `kHFSPlusFolderThreadRecord` (3) | variable | `(ownCNID, "")` | its **parent** and **name** |
| `kHFSPlusFileThreadRecord` (4) | variable | `(ownCNID, "")` | its **parent** and **name** |

Mining reference: Apple `core/hfs_format.h` for the layouts; `core/hfs_catalog.c`
`buildkey`, `buildthread` and `buildthreadkey` for who writes what.

The thread layout is the corner that surprises people. A thread record's **body**
`parentID` is the object's *parent*, and the object's own CNID is the thread
record's **key** `parentID`. `buildthread` copies the main record's key straight
into the body, while `buildthreadkey` builds the thread key from the node's CNID:

```c
rec->parentID = key->parentID;
bcopy(&key->nodeName, &rec->nodeName, sizeof(UniChar) * (key->nodeName.length + 1));
```

Verified on `basic-hfsplus`, whose root folder has CNID 2, name `BasicVolume`,
parent 1: the folder record is keyed `(1, "BasicVolume")` and the thread record is
keyed `(2, "")` with body `(parentID = 1, name = "BasicVolume")`.

Two consequences:

- **Thread keys are not one contiguous range.** Each has a different `parentID`, so
  enumerating a whole volume means scanning the catalog, not one key range. The
  opposite is a natural assumption to make and it is wrong.
- `CatalogRecord::cnid()` returns `Option` and is `None` for thread records, so the
  body cannot be silently misread as a CNID.

## Hard links: the discriminator is a flag, not a value

`bsdInfo.special` is a union meaning `hl_linkCount` on the indirect node and
`hl_linkReference` on a hard link. The two are told apart by
`kHFSHasLinkChainMask` in the record flags, **not** by the magnitude of the value.

Mining reference: `core/hfs_format.h` documents the union members and aliases them
(`hl_firstLinkID` is the file record's `reserved1`, `hl_prevLinkID` is
`bsdInfo.ownerID`, `hl_nextLinkID` is `bsdInfo.groupID`); `core/hfs_catalog.c` states
the rule in prose and even repairs volumes that get it wrong:

```c
Set kHFSHasLinkChainBit for hard links, and reset it for all other
items. Also set linkCount to 1 for regular files.
```

because rdar://8505977 shows regular files carrying the bit with a count above
one. Inferring the role from the value would misread exactly those files.

## Name comparison

Mining reference: `core/UnicodeWrappers.c` (`FastUnicodeCompare`,
`UnicodeBinaryCompare`) and `core/UCStringCompareData.h`.

Two corrections to widely held beliefs, both established by enumerating Apple's
generated tables rather than by assumption:

### HFS+ does not normalise names

There is no decomposition table in `UCStringCompareData.h`. The only folding data
is `gLatinCaseFold`, `gLowerCaseTable` and `gCompareTable`, and the last is for
legacy 8-bit Mac Script names used by classic HFS. So:

```
"cafe" + U+0301  !=  U+00E9
```

Composed and decomposed spellings are **different names**. The belief that they
are equal comes from classic HFS, which did compare through a decomposition table.

### The Latin-1 supplement is almost entirely identity

`gLatinCaseFold` spans U+0000-U+00FF but changes only **34** entries: ASCII A-Z,
plus the four letters with no precomposed upper/lower pair.

| Range | Folded? |
| --- | --- |
| U+0041-U+005A | yes, to lowercase |
| U+00C6, U+00D0, U+00D8, U+00DE | yes (AE, Eth, O-stroke, Thorn) |
| all other U+00C0-U+00DE | **no** |
| U+0100-U+01FF (Latin Extended-A) | a subset: D-stroke, Eng, kra, digraphs |
| U+0391-U+03A9 (Greek capitals) | yes |
| U+0410-U+04FF (Cyrillic) | yes |
| U+0531-U+0556 (Armenian) | yes |
| U+10A0-U+10C5 (Georgian) | yes |
| U+2160-U+216F (Roman numerals) | yes |
| U+FF21-U+FF3A (fullwidth Latin) | yes |

So `À` does **not** match `à` on an HFS+ volume, while `Α` does match `α`. This is
a genuine interoperability hazard between macOS and Linux: the same directory can
be listed under different names depending on which folding rules apply.

### Ignorable characters

Exactly sixteen characters fold to zero and are skipped. All are bidi, zero-width
or deprecated formatting characters:

```
U+200C U+200D U+200E U+200F U+202A-U+202E U+206A-U+206F U+FEFF
```

None is a combining mark. `tests` assert the documented list against the generated
table so the two cannot drift apart.

## Which comparator a volume uses

Case sensitivity needs an HFSX signature **and** a catalog `keyCompareType` of
`kHFSBinaryCompare`. Mining reference: `core/hfs_vfsutils.c`
(`hfs_MountHFSPlusVolume`):

```c
retval = BTOpenPath(catalog_vp, (KeyCompareProcPtr) CompareExtendedCatalogKeys);
...
if ((hfsmp->hfs_flags & HFS_X) && BTGetInformation(...) == 0) {
    if (btinfo.keyCompareType == kHFSBinaryCompare) {
        hfsmp->hfs_flags |= HFS_CASE_SENSITIVE;
        BTOpenPath(catalog_vp, (KeyCompareProcPtr) cat_binarykeycompare);
    }
}
```

So the folding comparator is the default, the binary one is installed only under
that conjunction, and on a plain HFS+ volume `keyCompareType` is not consulted at
all. An HFSX signature *permits* case sensitivity without enabling it — which is
why the corpus's case-insensitive HFSX image has an HFS+ signature.

## Formatter behaviours worth knowing

Observed on every corpus image:

- The root folder's `fileMode` is **0**, so no permission bits can be inferred
  from it. The root is a directory because its record type says so.
- The root folder's `ownerID` and `groupID` are **0**.
- A **journaled** volume's root contains two real directory entries,
  `.journal` and `.journal_info_block`, with their own CNIDs and thread records.
  The root's `valence` counts them and agrees with `read_dir`. A non-journaled
  fresh root is empty.
- `valence` is a reliable cross-check: it equals the number of entries `read_dir`
  returns for that directory.

## The journal

Mining reference: Apple `core/hfs_journal.h`, `core/hfs_journal.c`, and
`core/hfs_vfsutils.c` for where the journal is located at mount time.

### Finding the journal

1. The volume header's `kHFSVolumeJournaledBit` must be set. On a non-journaled
   volume `journalInfoBlock` overlaps spare space and holds whatever was there, so
   it must not be consulted.
2. `journalInfoBlock` names the *allocation block* holding a `JournalInfoBlock`.
3. That block's `offset` and `size` place the journal on the device. Both must be
   non-zero and the extent must lie inside the device.

Measured on the corpus: `journalInfoBlock = 2`, journal offset 12288 with
4096-byte blocks (7168 with 1024-byte blocks), size 524288 in both cases.

### The journal header is not big-endian

Every other HFS+ structure is big-endian. This one is **written in the native
byte order of whichever machine wrote it**, and its `endian` field records that so
a reader can tell:

```c
#define JOURNAL_HEADER_MAGIC  0x4a4e4c78   // 'JNLx'
#define ENDIAN_MAGIC          0x12345678
#define OLD_JOURNAL_HEADER_MAGIC 0x4a484452 // 'JHDR'
```

`mkfs.hfsplus` on x86 therefore writes a little-endian header inside a
big-endian filesystem, and a reader that assumes big-endian finds a magic of zero.
The magic itself is recognised in both orders — that is Apple's
`jhdr->magic == SWAP32(JOURNAL_HEADER_MAGIC)` check — and the `endian` sentinel
then selects how the numeric fields are read.

Field offsets, for a 64-bit `off_t`:

| Offset | Field |
| --- | --- |
| 0 | `magic` u32 |
| 4 | `endian` u32 |
| 8 | `start` u64 — first transaction |
| 16 | `end` u64 — free space begins |
| 24 | `size` u64 — total journal bytes |
| 32 | `blhdr_size` u32 |
| 36 | `checksum` u32 |
| 40 | `jhdr_size` u32 |
| 44 | `sequence_num` u32 |

### The checksum field is zeroed before hashing

The checksum field lies inside the byte range that is checksummed, so it must be
zeroed first or it could never validate. Apple's writer:

```c
jnl->jhdr->sequence_num = sequence_num;
jnl->jhdr->checksum = 0;
jnl->jhdr->checksum = calc_checksum((char *)jnl->jhdr, JOURNAL_HEADER_CKSUM_SIZE);
```

and its verifier saves the original, zeroes the field, and recomputes.

`JOURNAL_HEADER_CKSUM_SIZE` is `offsetof(journal_header, sequence_num)` = 44.
Apple's comment explains the odd cutoff: the struct gained `sequence_num` after
the checksum was defined, and checksumming it would invalidate every existing
journal. `BLHDR_CHECKSUM_SIZE` is 32, chosen to cover the block-list header
fields and its first entry.

### The checksum is a discard-shift, not a rotate

```c
cksum = (cksum << 8) ^ (cksum + *(unsigned char *)ptr);
return (~cksum);
```

The `<< 8` is a plain C shift on `unsigned int`, so the top bits fall off. A
reimplementation using `wrapping_add` and `rotate_left(8)` produces different
values and would reject every real journal. Observable difference: four bytes of
`0xFF` leave the accumulator at `0xFEFFFFFC`, whereas a rotation would give
`0xFFFFFFFF`.

### A bad journal header checksum is not fatal

`core/hfs_journal.c` `journal_open` prints a diagnostic and then:

```c
if (orig_checksum != checksum) {
    printf("jnl: %s: open: journal checksum is bad ...
", ...);
    //goto bad_journal;
}
```

The `goto` is commented out, so a volume with a stale journal-header checksum
still mounts. Reporting it as information rather than refusing the mount is
deliberate: refusing would leave the filesystem missing every recent change.

Two details matter for a reimplementation, and both produce false mismatches on
perfectly good journals if missed:

- The comparison is guarded by `if (jhdr->magic == JOURNAL_HEADER_MAGIC)`, so a
  legacy `JHDR` journal is not judged on a checksum written before the field
  existed. `Journal::header_checksum_ok()` returns `None` in that case rather
  than `Some(false)`, because reporting a mismatch for a checksum that was never
  computed would be a lie.
- The checksum is computed over the bytes **as stored**, before any swapping.
  Apple's verifier runs `calc_checksum` over the raw buffer and swaps the saved
  value separately. So a checksum must be taken over the on-disk bytes with the
  checksum field zeroed, and compared against the numerically-decoded value.

### Every `mkfs_hfsplus -J` volume has an uninitialised journal

Measured on both journaled corpus images:

```
flags = 0x00000005   kJIJournalInFSMask | kJIJournalNeedInitMask
journal offset = 12288 (4096-byte blocks) or 7168 (1024-byte blocks)
journal size = 524288
journal header = all zeros
```

`kJIJournalNeedInitMask` means the journal exists but no transaction has ever been
written, so the header area is untouched. A read-only mount must treat that as
"nothing to replay", not as corruption.

The corpus therefore proves detection, validation and the empty replay path, and
**cannot** prove transaction replay: that needs a volume crashed mid-transaction,
which cannot be produced without macOS or fault injection. `tests/journal_conformance.rs`
asserts that gap so it stays visible rather than being implied by the absence of
a test. Two tools below close it, in two different senses.

### Closing the gap: tools/makejournal.py

That gap is closed by writing a real journal into a journaled image rather than by
hand-waving it. `tools/makejournal.py` emits a journal header, one transaction,
one block list and the replacement block data, following the layout above, and
clears `kJIJournalNeedInitMask`. Three images are produced, covering both header
byte orders and two block geometries.

Writing that tool found a bug in the reader: the offset of a block list's data
was being recomputed later by walking the earlier block lists, and it advanced by
the *journal header* size where it had to advance by the *block-list header*
size. Nothing had ever read a real transaction, so it went unnoticed. Each block
list now records its own `data_offset` while the walk runs, which removes the
re-derivation entirely rather than fixing the arithmetic in place.

A fourth image, `journal-replay-multi`, holds **three** transactions, two of
which rewrite the same block. It exists because a single-transaction journal
cannot show whether the walk crosses transaction boundaries, whether a later
transaction supersedes an earlier write to the same block, or whether replay
truncates at damage. Writing it found the `binfo[0]` bug above.

What these images verify:

- **Precedence**: a replayed block reads from the journal and differs from the
  device.
- **Non-interference**: an untouched block still reads from the device, and the
  volume still mounts and reads its catalog through the overlaid device.
- **Refusal**: a block whose recorded checksum does not match its data stops the
  replay rather than presenting corrupted metadata as current.
- **Byte order**: the little-endian header is detected, not assumed.
- **Read-only**: the image is byte-identical afterwards.

What they do **not** verify: repairing a torn catalog. That is a different kind
of problem, and it took a different tool — see below.

### Closing the second gap: tools/mktorn.py

`makejournal.py` can only rewrite a block the filesystem does not reference,
because it has no way to write a catalog record. So it proves *precedence* and
*non-interference*, but nothing about the reason a journal exists: recovering a
metadata write that never reached the disk.

`tools/mktorn.py` writes a real catalog change into the journal and leaves the
on-disk catalog alone. The image it produces is exactly what a machine that lost
power mid-write leaves behind — a filesystem that is internally consistent but
older than its journal. Three blocks go into one transaction, which is what a
`create` does:

```text
block 0                  the volume header, with nextCatalogID advanced
catalog header node      leafRecords incremented
catalog leaf node        the new file record and its thread record
```

The file record and thread record are **cloned from records the formatter
already wrote**, with the CNID changed and the data fork emptied. Cloning
matters: the flags, timestamps, Finder info and permissions are Apple's bytes,
so the new record cannot differ from its neighbours in any way unrelated to the
test. The insertion point is chosen so the key order is the same under every
comparator the volume might use, and then asserted rather than assumed.

Writing it found three things that the replay tests could not have found, all of
them real:

- **`kBTLeafNode` is `-1`, not `0`.** The node kinds are a signed byte, so a leaf
  is `0xFF` on disk. Writing `0` produces a node that parses as an *index* node,
  and the reader correctly refuses it. The suite that would have caught this
  needed a catalog record to exist first, because nothing else writes a leaf node.
- **`keyLength` excludes itself**, and `HFSUniStr255`'s length is a `u16`, not a
  `u32`. Packing the key as `>HII` shifts every following field by two bytes.
- **A record's size comes from its `recordType`, not from the gap to the next
  key.** The last record is followed by the node's free space, so a gap-derived
  size is wrong by however much space is unused — which is most of a sparse node.

What the resulting image verifies, in `tests/journal_recovery.rs`:

- The **stale** view lists two files; the **replayed** view lists three. The two
  lists are asserted to *differ*, which is the negative control that stops every
  other assertion in the suite from being vacuous.
- The recovered file resolves **by name and by CNID**, so it is reachable by both
  routes a mount uses. A thread record with the wrong key would leave the file
  visible in a listing but unreachable by identity.
- The on-disk filesystem passes `fsck.hfsplus` on a copy. A file that lives only
  in the journal is not a disk defect, and a checker that complained would be
  wrong about what it found. This is what separates a crash-consistent volume
  from a corrupt one.
- `nextCatalogID` advances past the recovered CNID, so a later write cannot reuse
  it.
- Recovery is **idempotent** and leaves the image **byte-identical**.

### The journal is a ring, and `bnum` is in journal blocks

Two things about the replay that are invisible on every image this project can
generate, and both of which were wrong here before being measured.

**The journal wraps.** When the writer reaches the end it starts again just
after the header, so a block list's replacement data can be split across the
wrap. A read that crosses the end is ordinary and must succeed:

```c
if (offset >= jnl->jhdr->size) {
    offset = jnl->jhdr->jhdr_size + (offset - jnl->jhdr->size);
}
```

A ring that wrapped is a *full* ring — the writer only laps itself once every
block has been used — so there is no small fixture for it. `Journal::read_bytes`
exposes the wrapping read and `tests/journal_replay.rs` exercises it against a
real image.

**`bnum` is measured in `jhdr_size`, not the volume's block size.**
`add_block` computes `block_start = block_num * jhdr_size`. The two agree on
every volume Apple writes and on every image here, so a volume whose logical
block size differs from its journal's — which is what `journal_open` calls a
resized volume — is the only place the difference shows, and there it moves every
replayed block.

### `binfo[0]` is the sequence slot, not a block

The single most consequential detail of the replay algorithm, and the easiest to
miss. `_blk_info` is:

```c
typedef struct _blk_info {
    int32_t    bsize;
    union { int32_t cksum; uint32_t sequence_num; } b;
} _blk_info;
```

and Apple's replay loop starts at index **1**:

```c
for (i = 1; i < blhdr->num_blocks; i++) { ... add_block(...) }
```

So `binfo[0]` holds the transaction's sequence number and `num_blocks` counts
that slot. A block list describing one block has `num_blocks == 2`. Replaying
`binfo[0]` as a block would fabricate a filesystem block out of a transaction
counter, silently, on a real macOS journal.

The same word is read as a checksum when `BLHDR_CHECK_CHECKSUMS` is set and as a
sequence number otherwise, and a recorded checksum of zero means "do not verify".

A `bnum` of `-1` means the block was *killed* and must be skipped rather than
replayed, while its size still steps the data cursor.

### Replay truncates on damage rather than being abandoned

Apple does not model transaction boundaries during replay at all: it reads block
list headers from `start` to `end` and applies each in order, using
`BLHDR_FIRST_HEADER` only to note where a transaction began. On any failure it
goes to `bad_txn_handling`:

```c
/* Journal replay got error before it found any valid transations, abort replay */
if (txn_start_offset == 0) { ... goto bad_replay; }
/* Repeated error during journal replay, abort replay */
if (replay_retry_count == 3) { ... goto bad_replay; }
replay_retry_count++;

/* ... retry replaying all the good transactions that we found before
 * getting the error. */
jnl->jhdr->start = orig_jnl_start;
jnl->jhdr->end = txn_start_offset;
goto restart_replay;
```

So a damaged block list or block **truncates** the replay at the start of the
transaction that owns it, keeps everything before it, and retries — abandoning
the journal only if nothing good was found or after three failures. Apple's
reason is stated in its own comment above the checksum test:

```c
// XXXdbg - if these checks fail, we should replay as much
//         as we can in the hopes that it will still leave the
//         drive in a better state than if we didn't replay
//         anything
```

A read-only mount that refused the whole journal would instead show a
filesystem missing *every* recent change, which is a worse state than one
missing changes from the damage point onwards. This crate therefore reports
truncation through `Journal::truncation()` rather than raising an error.

### Replay validates block numbers before using them

Apple sanity-checks the whole list before applying any of it, and rejects a
negative block number that is not the killed sentinel:

```c
if (blhdr->binfo[i].bnum < 0 && blhdr->binfo[i].bnum != (off_t)-1) {
    printf("... bogus block number 0x%llx\n", ...);
    bad_blocks = 1;
    goto bad_txn_handling;
}
```

Skipping this would let a `bnum` with the high bit set compute a device offset far
outside the image. The downstream bounds check would eventually catch it, but
only after the offset had been multiplied out, so the check belongs where Apple
put it: before the contents are used, and it drops the transaction rather than
the journal.

### The overlay short-reads at end of file

`OverlaidDevice` is a `BlockDevice`, whose contract is that a read running past
the end is an error. But the overlaid read is answering a *POSIX* read, where end
of file is reported by returning fewer bytes. So a request that starts inside a
replayed block and runs past the end of the device must return the overlay bytes
it already has and then stop, not fail:

```text
nothing wrong: propagate Truncated, the file read errors out
wrong:         return the overlay bytes, then short-read the tail
```

Getting this backwards makes a mount report EIO for the tail of a file whose head
it had just read successfully.

### Journal replay is read-only by construction

Replayed blocks go into an in-memory overlay, and reads consult it in preference
to the device. Nothing opens the device for writing, so a wrong replay cannot
damage the image. `tests/journal_conformance.rs` hashes the whole image before
and after mounting, replaying and reading, and requires it to be byte-identical.

## Checklist for any new structure

Before adding a parser:

1. Find the struct in `core/hfs_format.h`. Copy field order and widths exactly.
2. Find the corresponding swap routine in `core/hfs_endian.c` to confirm the
   field order and to see how Apple handles wide integers.
3. Check the field arithmetic. Sum the widths. If it does not land on a sensible
   boundary, a field is missing or misplaced — this is how the phantom volume
   name was caught.
4. Find where Apple validates it. Match the validation order so a corrupt image
   fails at the same point.
5. Never `unsafe`-cast a buffer over image bytes. Use the checked accessors in
   `src/endian/`.
