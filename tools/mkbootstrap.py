#!/usr/bin/env python3
"""Build the minimum complete HFS+ catalog, on a volume that has none.

Why this is separate from `mkfiles.py`
---------------------------------------
`mkfiles.py` clones the records `mkfs.hfsplus` already wrote. That is safe --
the template is Apple's own bytes -- but it means the generator cannot start from
an empty catalog, and Milestones 8 and 9 both need to: creating the first file on
a fresh volume is the bottom of the mutation ladder.

An attempt to make `mkfiles.py` do both was reverted, because it emitted catalog
keys with a `keyLength` of 4 where 6 is correct, and `fsck.hfsplus` rejected the
result. The failure was in the synthesised path only; the clone path was fine.

So this is a separate tool with an explicit path, and each piece is checked:

  * the catalog header node, including its node map
  * the root thread record, keyed `(1, volume name)`
  * the root folder record, keyed `(2, "")`
  * key lengths, which are the thing that was wrong

Key lengths, explicitly, because they are what went wrong:

| record | key | keyLength |
| --- | --- | --- |
| root thread | `(parentID = 1, name = volume name)` | `4 + 2 + 2n` |
| root folder | `(parentID = 2, name = "")` | `6` |
| a file | `(parentID = 2, name)` | `6 + 2n` |
| a file's thread | `(fileID, "")` | `6` |

`keyLength` excludes itself but includes everything else, so the four-byte
`parentID` counts and the two-byte name length counts.

Mining reference: `core/hfs_format.h` `struct HFSPlusCatalogKey` and
`HFSPlusCatalogThread`; `newfs_hfs/mkfshfs.c` for the two records a fresh volume
starts with.

A starter file
-------------
`--with-file NAME` adds one regular file with a single block of content, which is
what the first mutation tests need: a volume that is not journaled (so writing to
it does not require a journal, which is a later milestone) and has a file whose
record can be replaced in place.

The record is synthesised here rather than cloned because `mkfiles.py`'s cloning
needs an existing file record and a bootstrapped catalog has none. Keeping
synthesis in one place is the point: the attempt to do it in two tools is what
produced the malformed-key generator that was reverted.

Usage:
    tools/mkbootstrap.py <in.img> <out.img> [--volume NAME] [--with-file NAME]
"""
import argparse
import struct
import sys

VOLUME_HEADER_OFFSET = 1024
CATALOG_FORK_OFFSET = 272

# struct HFSPlusCatalogKey: keyLength, parentID (u32), HFSUniStr255 (u16 count).
CATALOG_KEY_PREFIX = 2
CATALOG_KEY_PARENT_ID = 4
CATALOG_KEY_NAME_LEN = 2

# kHFSPlusCatalogFile, kHFSPlusCatalogThread and kHFSPlusFolderRecord, and
# sizeof(HFSPlusCatalogFolder).
FILE_RECORD = 2
THREAD_RECORD = 4
FILE_THREAD_RECORD = 4
FOLDER_THREAD_RECORD = 3
FOLDER_RECORD = 1
FOLDER_RECORD_SIZE = 88
FILE_RECORD_SIZE = 248

# Offset of a catalog record's data fork: past the type, flags, id, five
# timestamps, reserved, the BSD info, both Finder info blocks, the text encoding
# and the second reserved field.
FILE_DATA_FORK = 88
FILE_BSD_INFO = 32
BSD_FILE_MODE = 10
S_IFREG = 0o100000
# The header node of a freshly formatted catalog holds the BTHeaderRec, which
# carries maxKeyLength; the two values below are Apple's constants.
CATALOG_MAX_KEY_LENGTH = 516
ROOT_PARENT_ID = 1
ROOT_FOLDER_ID = 2

# kHFSCaseFolding, and the two B-tree attribute bits.
KEY_COMPARE_CASE_FOLDING = 0xCF
BT_BIG_KEYS = 0x0000_0002
BT_VARIABLE_INDEX_KEYS = 0x0000_0004
# Nodes the map claims: the header and the one leaf.
NODE_MAP_IN_USE = 2
# kHFSFirstUserCatalogNodeID: the first CNID a file may have. Everything below is
# reserved, with 2 the root folder.
FIRST_USER_CNID = 16


def key(parent_id: int, name: str) -> bytes:
    """`struct HFSPlusCatalogKey`, `keyLength` excluding itself."""
    units = name.encode("utf-16-be")
    declared = CATALOG_KEY_PARENT_ID + CATALOG_KEY_NAME_LEN + len(units)
    return (struct.pack(">H", declared)
            + struct.pack(">I", parent_id)
            + struct.pack(">H", len(units) // 2)
            + units)


def root_folder_record(volume_name: str) -> bytes:
    """The root directory's record, keyed `(1, volume name)`.

    Every object has a record keyed by *(its parent, its own name)*. For the root
    that parent is `kHFSRootParentID` (1) and the name is the volume's name --
    which is why the volume name lives in the catalog and not in the volume header.
    Verified against the records `mkfs.hfsplus` writes, byte for byte: the first
    record in a fresh catalog is a *folder* record with key `(1, name)`, not the
    thread.
    """
    body = bytearray(FOLDER_RECORD_SIZE)
    struct.pack_into(">h", body, 0, FOLDER_RECORD)
    struct.pack_into(">H", body, 2, 0)                # flags
    # valence counts children. The root's own thread record is not one of them --
    # `fsck.hfsplus` normalises this to 0, and counting the root as its own child
    # is the kind of plausible-but-wrong value that only an independent checker
    # catches.
    struct.pack_into(">I", body, 4, 0)                # valence
    struct.pack_into(">I", body, 8, ROOT_FOLDER_ID)   # folderID
    # The five timestamps stay zero: in a classic HFS timestamp zero means "never
    # set", so writing 1904 dates would be a fabrication.
    #
    # HFSPlusBSDInfo: ownerID, groupID, adminFlags, ownerFlags, fileMode, special.
    struct.pack_into(">I", body, 32, 0)               # ownerID
    struct.pack_into(">I", body, 36, 0)               # groupID
    struct.pack_into(">B", body, 40, 0)               # adminFlags
    struct.pack_into(">B", body, 41, 0)               # ownerFlags
    struct.pack_into(">H", body, 42, 0o40755)         # fileMode: S_IFDIR | 0777
    struct.pack_into(">h", body, 44, 0)               # special
    return key(ROOT_PARENT_ID, volume_name) + bytes(body)


def root_folder_thread(volume_name: str) -> bytes:
    """The root directory's thread record, keyed `(2, "")`.

    A thread record's *key* names the object itself with an empty name; its *body*
    carries the parent and **the object's own name**. That asymmetry is the whole
    reason a thread record exists -- it is what lets a name be renamed without
    rewriting a hard-linked sibling's key -- and it is easy to get backwards.

    The record type is `kHFSPlusFolderThreadRecord` (3) for a directory and
    `kHFSPlusFileThreadRecord` (4) for a file. The root is a directory.

    Verified against the record `mkfs.hfsplus` writes: key `(2, "")`, body
    parentID 1, body nodeName the volume's name. An empty body name fails
    `fsck.hfsplus` with "Invalid parent CName in thread record", because the check
    validates the body's name rather than the key's.

    Mining reference: `core/hfs_format.h` `struct HFSPlusCatalogThread`;
    `lib_fsck_hfs/dfalib/CatalogCheck.c` `CheckThread`.
    """
    units = volume_name.encode("utf-16-be")
    body = (struct.pack(">h", FOLDER_THREAD_RECORD)
            + struct.pack(">H", 0)                # reserved
            + struct.pack(">I", ROOT_PARENT_ID)    # the root's parent
            + struct.pack(">H", len(units) // 2)   # the object's own name
            + units)
    return key(ROOT_FOLDER_ID, "") + body


def file_record(cnid: int, start_block: int, logical_size: int, total_blocks: int) -> bytes:
    """A synthesised `kHFSPlusFileRecord` with one block of data.

    Every field the format requires is set; everything else is legitimately zero,
    including the five timestamps, where zero means "never set" rather than 1904.
    """
    body = bytearray(FILE_RECORD_SIZE)
    struct.pack_into(">h", body, 0, FILE_RECORD)
    struct.pack_into(">I", body, 8, cnid)
    struct.pack_into(">H", body, FILE_BSD_INFO + BSD_FILE_MODE, S_IFREG | 0o644)
    # The data fork: logicalSize, clumpSize, totalBlocks, then eight descriptors.
    struct.pack_into(">Q", body, FILE_DATA_FORK, logical_size)
    struct.pack_into(">I", body, FILE_DATA_FORK + 12, total_blocks)
    struct.pack_into(">II", body, FILE_DATA_FORK + 16, start_block, total_blocks)
    return bytes(body)


def file_thread(cnid: int, parent_id: int, name: str) -> bytes:
    """A file's thread record.

    The key's parentID is the *object itself* -- the file's CNID -- while the
    body names the parent and repeats the object's own name. Passing zero for the
    key would leave the record unreachable by name, which `fsck.hfsplus` reports
    as an invalid catalog record type.
    """
    units = name.encode("utf-16-be")
    body = (struct.pack(">h", FILE_THREAD_RECORD)
            + struct.pack(">H", 0)
            + struct.pack(">I", parent_id)
            + struct.pack(">H", len(units) // 2)
            + units)
    return key(cnid, "") + body


def write_catalog(img: bytearray, volume_name: str, starter: str | None) -> None:
    """Replace the catalog file with the two records a fresh volume starts with."""
    bs = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 40)[0]
    fork = VOLUME_HEADER_OFFSET + CATALOG_FORK_OFFSET
    start = struct.unpack_from(">I", img, fork + 16)[0]
    if start == 0:
        sys.exit("error: the volume has no catalog file")
    node_size = bs
    if node_size < 4096:
        sys.exit(f"error: this needs a node of at least 4096 bytes, not {node_size}")

    clump_size = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + CATALOG_FORK_OFFSET + 8)[0]
    header = bytearray(node_size)
    leaf = bytearray(node_size)

    # --- the leaf, with two records ------------------------------------
    # Order matters: the folder record sorts before its thread because the keys
    # differ, and the tree is only valid if they are in ascending order.
    records = [root_folder_record(volume_name), root_folder_thread(volume_name)]
    if starter is not None:
        cnid = FIRST_USER_CNID
        # One free block for the content, taken from the bitmap's free list.
        bitmap_start = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 112 + 16)[0]
        total = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 44)[0]
        free = [b for b in range(1, total)
                if not img[bitmap_start * node_size + b // 8] & (0x80 >> (b % 8))]
        if not free:
            sys.exit("error: no free block for the starter file")
        block = free[-1]
        # A recognisable byte pattern, so a read from the wrong block shows.
        img[block * node_size:(block + 1) * node_size] = bytes(
            range(256)
        ) * (node_size // 256)
        img[bitmap_start * node_size + block // 8] |= 0x80 >> (block % 8)
        # Allocating a block means updating the header's free count too; fsck
        # recomputes it rather than trusting it ("Invalid volume free block count").
        before_free = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 48)[0]
        struct.pack_into(">I", img, VOLUME_HEADER_OFFSET + 48, before_free - 1)
        print(f"  volume header freeBlocks {before_free} -> {before_free - 1}")
        records.append(key(ROOT_FOLDER_ID, starter)
                       + file_record(cnid, block, node_size, 1))
        records.append(file_thread(cnid, ROOT_FOLDER_ID, starter))
        # The root now has one child.
        for i, rec in enumerate(records):
            if struct.unpack_from(">h", rec, 2 + struct.unpack_from(">H", rec, 0)[0])[0] == FOLDER_RECORD:
                rec = bytearray(rec)
                struct.pack_into(">I", rec, 2 + struct.unpack_from(">H", rec, 0)[0] + 4, 1)
                records[i] = bytes(rec)
                break
        print(f"  starter file {starter!r}: CNID {cnid}, block {block}, {node_size} bytes")
        print(f"  fileCount 1, nextCatalogID {FIRST_USER_CNID + 1}")
    records.sort(key=lambda r: key_parts(r))
    assert_key_order(records)
    offsets = []
    at = 14
    for rec in records:
        offsets.append(at)
        leaf[at:at + len(rec)] = rec
        at += len(rec)

    leaf[8] = 0xFF                     # kBTLeafNode
    leaf[9] = 1                         # height
    struct.pack_into(">H", leaf, 10, len(records))
    for i, off in enumerate(offsets):
        struct.pack_into(">H", leaf, node_size - 2 * (i + 1), off)
    struct.pack_into(">H", leaf, node_size - 2 * (len(records) + 1), at)
    # NOT at offset 40. BTNodeDescriptor is 14 bytes, so 40 is inside the record
    # area; only an index node has a free-space field there. The free space is
    # recorded solely in the offset array's terminal entry, above.

    # --- the header node -----------------------------------------------
    # Its three records sit at fixed offsets, and the node map follows them. The
    # offset array's third entry *is* the map position, which is why a header node
    # must never be packed by the same code that packs a leaf.
    # A header node's three records sit at fixed offsets: the BTHeaderRec at 14,
    # then two more, then the node map at 248. The header record is *record 0* --
    # 14, not 120 -- and putting the fields at record 1's offset leaves every one
    # of them reading as zero.
    rec0, rec1, rec2 = 14, 120, 248
    header[8] = 1                       # kBTHeaderNode
    header[9] = 0                       # height
    struct.pack_into(">H", header, 10, 3)

    hdr = rec0                          # BTHeaderRec
    struct.pack_into(">H", header, hdr + 0, 1)             # treeDepth
    struct.pack_into(">I", header, hdr + 2, 1)             # rootNode
    struct.pack_into(">I", header, hdr + 6, len(records))  # leafRecords
    struct.pack_into(">I", header, hdr + 10, 1)            # firstLeafNode
    struct.pack_into(">I", header, hdr + 14, 1)            # lastLeafNode
    struct.pack_into(">H", header, hdr + 18, node_size)     # nodeSize
    struct.pack_into(">H", header, hdr + 20, CATALOG_MAX_KEY_LENGTH)
    struct.pack_into(">I", header, hdr + 22, 8)            # totalNodes
    # freeNodes: totalNodes less the nodes in use. The node map says two -- the
    # header and the leaf -- so 8 - 2 = 6. Derived from the map rather than
    # guessed; an earlier version guessed 7 and fsck.hfsplus corrected it.
    struct.pack_into(">I", header, hdr + 26, 8 - NODE_MAP_IN_USE)
    # clumpSize: the catalog fork's, as mkfs.hfsplus writes it.
    struct.pack_into(">I", header, hdr + 32, clump_size)
    # keyCompareType: kHFSCaseFolding. An HFS+ volume folds case in file names,
    # and this byte is how a reader knows -- the signature alone does not say.
    struct.pack_into(">B", header, hdr + 37, KEY_COMPARE_CASE_FOLDING)
    # attributes: kBTBigKeysMask | kBTVariableIndexKeysMask. Without the first,
    # keys carry a one-byte length and every key length this crate computes from a
    # 16-bit field is wrong -- and it is the bit fsck.hfsplus checks when it says
    # "Invalid B-tree header".
    struct.pack_into(">I", header, hdr + 38, BT_BIG_KEYS | BT_VARIABLE_INDEX_KEYS)
    # Node map at rec2: nodes 0 and 1 are in use. MSB first, like every other
    # bitmap in the format.
    header[rec2] = 0xC0
    for i, off in enumerate([rec0, rec1, rec2, node_size - 8]):
        struct.pack_into(">H", header, node_size - 2 * (i + 1), off)

    img[start * node_size:start * node_size + node_size] = header
    img[(start + 1) * node_size:(start + 1) * node_size + node_size] = leaf

    # The header's counts, so `hfsck` -- which recomputes rather than trusts --
    # agrees.
    # Volume header offsets. These are easy to get wrong by one field, and
    # getting them wrong is silent: `folderCount` sits between `fileCount` and
    # `blockSize`, so writing 1 to the wrong slot sets the *block size* to one and
    # every later parse fails for an unrelated-looking reason.
    vh = VOLUME_HEADER_OFFSET
    struct.pack_into(">I", img, vh + 32, 1 if starter else 0)  # fileCount
    # folderCount excludes the root directory itself, so a volume holding nothing
    # reports 0. fsck.hfsplus normalises 1 to 0, and counting the root here is the
    # kind of plausible value only an independent checker catches.
    struct.pack_into(">I", img, vh + 36, 0)                    # folderCount
    # nextCatalogID must be at least `kHFSFirstUserCatalogNodeID` (16): CNIDs 0-15
    # are reserved, with 2 the root folder. Handing out anything below that would
    # eventually collide with a reserved ID, and fsck.hfsplus normalises a smaller
    # value to 16 with "Volume header needs minor repair".
    struct.pack_into(">I", img, vh + 64,
                     FIRST_USER_CNID + (1 if starter else 0))  # nextCatalogID

    print(f"  catalog at block {start}: {len(records)} records, node map 0xc0")
    for rec, label in ((records[0], "root folder"), (records[1], "root thread")):
        declared = struct.unpack_from(">H", rec, 0)[0]
        print(f"    {label:12s} keyLength {declared}, record {len(rec)} bytes")


def key_parts(rec: bytes) -> tuple:
    """A record's key as `(parentID, nameLength, name)`.

    Sorted on *length before content*, which is what Apple's catalog comparator
    does -- so `"z"` sorts after `"abc"`. Extracted here so the sort and the
    order check use the same reading.
    """
    parent = struct.unpack_from(">I", rec, CATALOG_KEY_PREFIX)[0]
    count = struct.unpack_from(">H", rec, CATALOG_KEY_PREFIX + CATALOG_KEY_PARENT_ID)[0]
    at = CATALOG_KEY_PREFIX + CATALOG_KEY_PARENT_ID + 2
    return (parent, count, rec[at:at + count * 2])


def assert_key_order(records: list[bytes]) -> None:
    """Records must be in ascending key order, or the tree is corrupt.

    Compared on `(parentID, name)` with the *length-first* rule Apple's catalog
    comparator uses, so a shorter name sorts before a longer one regardless of its
    letters.
    """
    parsed = [key_parts(r) for r in records]
    for (p0, n0, b0), (p1, n1, b1) in zip(parsed, parsed[1:]):
        if (p0, n0, b0) >= (p1, n1, b1):
            sys.exit(
                f"error: catalog keys out of order: ({p0},{n0}) then ({p1},{n1})"
            )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("source")
    ap.add_argument("dest")
    ap.add_argument("--with-file", dest="starter", default=None,
                    help="also create one regular file with a single block of content")
    ap.add_argument("--volume", default=None,
                    help="volume name; read from the source when omitted")
    args = ap.parse_args()

    with open(args.source, "rb") as f:
        img = bytearray(f.read())

    name = args.volume
    if name is None:
        # The volume name is not in the header -- it is the root folder's name, so
        # with an empty catalog there is nothing to read it from and the caller
        # must say what it is.
        sys.exit("error: --volume is required; the name is not in the header")

    write_catalog(img, name, args.starter)
    with open(args.dest, "wb") as f:
        f.write(bytes(img))
    print(f"{args.dest}: bootstrapped a catalog for volume {name!r}")


if __name__ == "__main__":
    main()