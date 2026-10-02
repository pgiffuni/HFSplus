# On-disk format notes

Working notes from mining Apple's `core/` sources. Each entry states the finding,
why it is easy to get wrong, and the Apple source that settles it. Apple's
repository is the authority: <https://github.com/pgiffuni/apple-hfs> at commit
`d1bac2f062e6e9c0dfcce302d9aacb10173d0eea`.

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

Consequence: a sparse file with a large `logicalSize` and few allocated blocks
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
