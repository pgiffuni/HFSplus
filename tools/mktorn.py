#!/usr/bin/env python3
"""Write a catalog change into a journal instead of onto the filesystem.

Why this exists
---------------
`tools/makejournal.py` injects a transaction that rewrites a block the
filesystem does not reference. That proves the overlay takes precedence, but it
proves nothing about the thing a journal is *for*: a metadata write that was
journalled and then lost, because the machine stopped before the block reached
the disk. The filesystem on the image is stale, and correct replay has to make
the change visible.

So this tool writes the real thing. It builds a *newer* version of the catalog
-- a root folder containing an extra file -- puts it in a journal transaction,
and leaves the on-disk catalog alone. The result is an image that looks stale
to a naive reader and coherent to one that replays:

    hfsls -R torn.img            ->  .journal, .journal_info_block
    (replayed) read_dir(root)    ->  .journal, .journal_info_block, torn.txt

The image itself is byte-identical before and after either read.

What is written
---------------
Three filesystem blocks in a single transaction, which is what a real
`create` does and which also gives the block-list code a transaction with more
than one block to walk:

    block 0                  the volume header, with nextCatalogID advanced
    catalog header node      leafRecords incremented
    catalog leaf node        the new file record and its thread record

Only the first leaf node is rewritten. It holds every record in this volume, and
it has ample free space, so no split, no index update and no allocation change
is needed.

Record layout
-------------
`struct HFSPlusCatalogKey` from Apple `core/hfs_format.h`: a `keyLength` that
excludes itself, a `parentID`, then `HFSUniStr255` (a `u16` count and that many
UTF-16 code units). `struct HFSPlusCatalogFile` and
`struct HFSPlusCatalogThread` follow the key. Sizes and field order come from
the same header, and the crate's `src/catalog/record.rs` states them
independently.

Ordering
--------
A new record has to land in key order or the B-tree is corrupt. Rather than
reimplement Apple's comparator, the added name is chosen so that its position
is the same under every comparator the volume might use: `torn.txt` begins with
`t`, which is greater than `.` (the first character of both existing root
children) and less than nothing else, and the parent ID 18 is greater than
every parent already present. `assert_key_order` checks the result rather than
assuming it.

Usage:
    tools/mktorn.py <in.img> <out.img> [--name torn.txt] [--cnid 18]
"""
import argparse
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import makejournal as mj  # noqa: E402

VOLUME_HEADER_OFFSET = 1024
# `nextCatalogID` in `struct HFSPlusVolumeHeader`, which is what a create
# advances to reserve the new file's CNID.
NEXT_CATALOG_ID_OFFSET = 64
ROOT_FOLDER_ID = 2

# `struct HFSPlusCatalogKey`: keyLength, parentID, HFSUniStr255.
CATALOG_KEY_PREFIX = 2
CATALOG_KEY_PARENT_SIZE = 4
CATALOG_KEY_NAME_LEN_SIZE = 2
CATALOG_KEY_NAME_LEN_OFFSET = CATALOG_KEY_PREFIX + CATALOG_KEY_PARENT_SIZE
CATALOG_KEY_NAME_OFFSET = CATALOG_KEY_NAME_LEN_OFFSET + CATALOG_KEY_NAME_LEN_SIZE

# `struct BTNodeDescriptor` is 14 bytes, and a leaf node's records start there.
NODE_DESCRIPTOR_SIZE = 14
# Height of a leaf node at the bottom of a depth-1 tree.
LEAF_NODE_HEIGHT = 1

# The node kinds are a signed byte, so `kBTLeafNode` is -1 and lands on disk as
# 0xFF, not 0. Writing 0 would make a leaf indistinguishable from an index node,
# which is a corruption this crate detects -- correctly -- and refuses.
K_BT_LEAF_NODE = 0xFF

K_HFS_PLUS_FOLDER_RECORD = 1
K_HFS_PLUS_FILE_RECORD = 2
K_HFS_PLUS_FOLDER_THREAD_RECORD = 3
K_HFS_PLUS_FILE_THREAD_RECORD = 4

# struct HFSPlusCatalogFile, from core/hfs_format.h.
FILE_RECORD_SIZE = 248
FILE_RECORD_FILE_ID_OFFSET = 8
FILE_RECORD_DATA_FORK_OFFSET = 88
FORK_LOGICAL_SIZE_OFFSET = 0
FORK_TOTAL_BLOCKS_OFFSET = 8
FORK_EXTENTS_OFFSET = 16

# `struct HFSPlusCatalogThread`: recordType (2), reserved (2), parentID (4), then
# `HFSUniStr255`, so the first code unit is 10 bytes in.
THREAD_RECORD_FIXED_SIZE = 8
THREAD_NAME_LEN_OFFSET = THREAD_RECORD_FIXED_SIZE
THREAD_NAME_OFFSET = THREAD_RECORD_FIXED_SIZE + 2

# struct HFSPlusCatalogFolder, from core/hfs_format.h.
FOLDER_RECORD_SIZE = 88


def be16(buf: bytearray, at: int) -> int:
    return struct.unpack_from(">H", buf, at)[0]


def be32(buf: bytearray, at: int) -> int:
    return struct.unpack_from(">I", buf, at)[0]


def put16(buf: bytearray, at: int, value: int) -> None:
    struct.pack_into(">H", buf, at, value)


def put32(buf: bytearray, at: int, value: int) -> None:
    struct.pack_into(">I", buf, at, value)


def build_key(parent: int, name: str) -> bytes:
    """`struct HFSPlusCatalogKey`, with keyLength excluding itself.

    `HFSUniStr255` is a `u16` count followed by that many code units, not a `u32`,
    so the packed form is `>HIH`: 2 bytes of keyLength, 4 of parentID, 2 of name
    count, then the name. Getting that count width wrong shifts the record body
    by two bytes and every following field decodes as noise.
    """
    units = name.encode("utf-16-be")
    count = len(units) // 2
    key_length = CATALOG_KEY_PARENT_SIZE + 2 + len(units)
    return struct.pack(">HIH", key_length, parent, count) + units


def parse_key(node: bytearray, at: int) -> tuple[int, str]:
    parent = be32(node, at + CATALOG_KEY_PREFIX)
    count = be16(node, at + CATALOG_KEY_NAME_LEN_OFFSET)
    units = node[at + CATALOG_KEY_NAME_OFFSET:at + CATALOG_KEY_NAME_OFFSET + count * 2]
    return parent, units.decode("utf-16-be")


def record_body_size(node: bytearray, body_at: int) -> int:
    """Size of a record body from its `recordType`.

    The gap to the next key cannot be used: the last record is followed by the
    node's free space, not by another record, so a gap-derived size would be
    wrong by however much space is unused. The record types are fixed-size
    structures, so the type alone gives the answer.
    """
    rtype = struct.unpack_from(">h", node, body_at)[0]
    if rtype == K_HFS_PLUS_FOLDER_RECORD:
        return FOLDER_RECORD_SIZE
    if rtype == K_HFS_PLUS_FILE_RECORD:
        return FILE_RECORD_SIZE
    if rtype in (K_HFS_PLUS_FOLDER_THREAD_RECORD, K_HFS_PLUS_FILE_THREAD_RECORD):
        # recordType, reserved, parentID, then the name: a `u16` count and the
        # UTF-16 code units it announces.
        return THREAD_NAME_OFFSET + 2 * be16(node, body_at + THREAD_NAME_LEN_OFFSET)
    sys.exit(f"error: unknown catalog record type {rtype}")


def read_leaf_records(node: bytearray, node_size: int) -> list[tuple[int, int, int]]:
    """Return (key offset, keyLength, record span) for each record, in key order."""
    count = be16(node, 10)
    out = []
    for i in range(count):
        offset = be16(node, node_size - 2 * (i + 1))
        key_length = be16(node, offset)
        body_at = offset + 2 + key_length
        span = 2 + key_length + record_body_size(node, body_at)
        out.append((offset, key_length, span))
    return out


def folded_key(parent: int, name: str) -> tuple:
    """A comparison key good enough to *check* order, not to define it.

    Case-folded code-unit order. For the ASCII names this tool deals in it
    agrees with Apple, which is the point: it is used to assert that the chosen
    insertion point is unambiguous rather than to decide it.
    """
    return (parent, name.lower())


def assert_key_order(entries, what: str) -> None:
    for (p0, n0), (p1, n1) in zip(entries, entries[1:]):
        if folded_key(p0, n0) >= folded_key(p1, n1):
            sys.exit(f"error: {what}: keys out of order: {(p0, n0)!r} then {(p1, n1)!r}")


def build_file_record(template: bytes, cnid: int) -> bytes:
    """A file record cloned from an existing one, with a new CNID and no data.

    Cloning the volume's own record means the flags, timestamps, Finder info and
    permissions are Apple's, byte for byte, so the new record cannot differ from
    its neighbours in a way that has nothing to do with the test. The data fork
    is emptied because the point of the record is its existence in the directory,
    not its contents.
    """
    body = bytearray(template)
    put32(body, FILE_RECORD_FILE_ID_OFFSET, cnid)
    fork = FILE_RECORD_DATA_FORK_OFFSET
    struct.pack_into(">Q", body, fork + FORK_LOGICAL_SIZE_OFFSET, 0)
    put32(body, fork + FORK_TOTAL_BLOCKS_OFFSET, 0)
    for e in range(8):
        put32(body, fork + FORK_EXTENTS_OFFSET + e * 8, 0)
        put32(body, fork + FORK_EXTENTS_OFFSET + e * 8 + 4, 0)
    return bytes(body)


def build_thread_record(parent: int, name: str, template: bytes) -> bytes:
    """A thread record: the key is the object's own CNID and an empty name."""
    units = name.encode("utf-16-be")
    body = bytearray(template)
    struct.pack_into(">H", body, 0, K_HFS_PLUS_FILE_THREAD_RECORD)
    put32(body, 4, parent)
    put16(body, THREAD_NAME_LEN_OFFSET, len(units) // 2)
    body[THREAD_NAME_OFFSET:THREAD_NAME_OFFSET + len(units)] = units
    return bytes(body[:THREAD_NAME_OFFSET + len(units)])


def pack_leaf(template: bytes, records: list[bytes]) -> bytes:
    """Lay records out from the front and the offset array from the back.

    `struct BTNodeDescriptor` is 14 bytes, so records start at offset 14. The
    offset array holds `numRecords + 1` entries: the last is the offset of the
    free space, which is where the next record would be.

    Everything outside those two regions is left exactly as the formatter wrote
    it, including the two bytes at offset 40. Those are *not* a leaf node's
    `freeSpaceOffset`: `struct BTNodeDescriptor` is 14 bytes, so 40 falls in the
    free space, and mkfs.hfsplus leaves 1 there. Apple maintains no such field
    for leaves -- only index nodes have one -- so writing a "correct" value would
    be inventing a field that means nothing in this node type.
    """
    node = bytearray(template)
    node_size = len(node)
    node[8] = K_BT_LEAF_NODE
    # A leaf node's height is one more than its parent's, so at the bottom of a
    # depth-1 tree it is 1. Writing 0 makes the node look like a header or map
    # node, and fsck.hfsplus rejects the whole catalog with "Invalid node
    # height" -- which is how the mistake was found. Mining reference:
    # `struct BTNodeDescriptor` in `core/BTree.h` says "zero for header, map;
    # child is one more than parent", and `kBTLeafNode` is -1.
    node[9] = LEAF_NODE_HEIGHT
    put16(node, 10, len(records))

    offset = NODE_DESCRIPTOR_SIZE
    offsets = []
    for rec in records:
        offsets.append(offset)
        node[offset:offset + len(rec)] = rec
        offset += len(rec)
    for i, off in enumerate(offsets):
        struct.pack_into(">H", node, node_size - 2 * (i + 1), off)
    struct.pack_into(">H", node, node_size - 2 * (len(records) + 1), offset)
    return bytes(node)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("source")
    ap.add_argument("dest")
    ap.add_argument("--name", default="torn.txt", help="name of the journalled file")
    ap.add_argument("--cnid", type=int, default=18, help="CNID to give it")
    args = ap.parse_args()

    with open(args.source, "rb") as f:
        img = bytearray(f.read())

    vh = VOLUME_HEADER_OFFSET
    if img[vh:vh + 2] not in (b"\x48\x2b", b"\x48\x58"):
        sys.exit(f"error: {args.source} is not an HFS+ volume")
    block_size = be32(img, vh + 40)
    attributes = be32(img, vh + 4)
    if not attributes & 0x2000:
        sys.exit(f"error: {args.source} is not journaled")
    journal_info_block = be32(img, vh + 12)
    if journal_info_block == 0:
        sys.exit("error: journalInfoBlock is zero on a journaled volume")

    # --- Locate the catalog ------------------------------------------------
    catalog_fork = vh + 272
    catalog_start = be32(img, catalog_fork + 16)
    if catalog_start == 0:
        sys.exit("error: the catalog file has no extents")

    def catalog_node(n: int) -> bytearray:
        at = (catalog_start + n) * block_size
        return bytearray(img[at:at + block_size])

    header_node = catalog_node(0)
    node_size = be16(header_node, 32)
    if node_size != block_size:
        sys.exit(f"error: unexpected node size {node_size}")
    # `treeDepth` is 1 when the root node is itself a leaf, which is the case
    # for a freshly formatted volume with a handful of records. Anything deeper
    # means an index node sits between the root and the leaves, and a single
    # rewritten leaf would not be reachable, so refuse rather than produce an
    # image whose catalog is inconsistent for a reason unrelated to the journal.
    tree_depth = be16(header_node, 14)
    root_node = be32(header_node, 16)
    first_leaf = be32(header_node, 24)
    last_leaf = be32(header_node, 28)
    if tree_depth != 1 or root_node != first_leaf or first_leaf != last_leaf:
        sys.exit(f"error: expected one leaf node at the root, got depth {tree_depth}, "
                 f"root {root_node}, leaves {first_leaf}..{last_leaf}")

    leaf = catalog_node(first_leaf)
    entries = read_leaf_records(leaf, node_size)
    if not entries:
        sys.exit("error: the catalog leaf node holds no records to clone")

    # --- Build the newer leaf ---------------------------------------------
    # Walk the records once, keeping each one's key for ordering and its body so
    # the new records can be cloned from real Apple-written ones.
    records = []
    keys = []
    file_template = thread_template = None
    for offset, key_length, span in entries:
        parent, name = parse_key(leaf, offset)
        record = bytes(leaf[offset:offset + span])
        body_at = offset + 2 + key_length
        rtype = struct.unpack_from(">h", leaf, body_at)[0]
        body = bytes(leaf[body_at:offset + span])
        if rtype == K_HFS_PLUS_FILE_RECORD and file_template is None:
            file_template = body
        if rtype == K_HFS_PLUS_FILE_THREAD_RECORD and thread_template is None:
            thread_template = body
        records.append(record)
        keys.append((parent, name))

    if file_template is None or thread_template is None:
        sys.exit("error: no file record and thread record to clone from")

    new_file = build_key(ROOT_FOLDER_ID, args.name) + build_file_record(file_template, args.cnid)
    new_thread = build_key(args.cnid, "") + build_thread_record(ROOT_FOLDER_ID, args.name, thread_template)

    # Insertion point: the first key that sorts after the new file's key, and
    # after the new thread's key respectively. Both are unambiguous by
    # construction, but assert rather than assume.
    def insert_at(items, new_key, new_blob):
        k = folded_key(*new_key)
        for i, (parent, name) in enumerate(items):
            if folded_key(parent, name) > k:
                items.insert(i, new_key)
                return i
        items.append(new_key)
        return len(items) - 1

    file_at = insert_at(keys, (ROOT_FOLDER_ID, args.name), new_file)
    records.insert(file_at, new_file)
    thread_at = insert_at(keys, (args.cnid, ""), new_thread)
    records.insert(thread_at, new_thread)

    assert_key_order(keys, "catalog leaf")
    new_leaf = pack_leaf(bytes(leaf), records)
    if len(records) != len(entries) + 2:
        sys.exit("error: record accounting went wrong")

    # --- The header node, and the volume header ---------------------------
    new_header = bytearray(header_node)
    put32(new_header, 20, be32(new_header, 20) + 2)      # leafRecords
    # The volume header sits at 1024, so it shares filesystem block 0 with the
    # boot blocks. Rewriting that block is what a real transaction does when
    # `nextCatalogID` advances.
    new_vh_block = bytearray(img[0:block_size])
    put32(new_vh_block, vh + NEXT_CATALOG_ID_OFFSET,
          be32(new_vh_block, vh + NEXT_CATALOG_ID_OFFSET) + 1)

    # --- Journal geometry --------------------------------------------------
    jib_off = journal_info_block * block_size
    flags = be32(img, jib_off)
    journal_offset = struct.unpack_from(">Q", img, jib_off + mj.JIB_OFFSET_OFFSET)[0]
    journal_size = struct.unpack_from(">Q", img, jib_off + mj.JIB_SIZE_OFFSET)[0]
    if journal_offset + journal_size > len(img):
        sys.exit("error: the journal extends past the end of the image")
    if not flags & mj.K_JI_JOURNAL_IN_FS_MASK:
        sys.exit("error: this tool only handles journals inside the filesystem")

    jhdr_size = block_size
    blhdr_size = block_size
    start = jhdr_size
    blocks = [
        (0, bytes(new_vh_block)),
        (catalog_start, bytes(new_header)),
        (catalog_start + first_leaf, new_leaf),
    ]
    header_raw, data_raw = mj.build_block_list(blocks, blhdr_size, True, 1)
    layout = header_raw + data_raw
    if start + len(layout) > journal_size:
        sys.exit("error: the journal is too small for this transaction")
    end = start + len(layout)

    journal = bytearray(journal_size)
    hdr = mj.build_journal_header(start, end, journal_size, jhdr_size, blhdr_size, 1)
    journal[0:len(hdr)] = hdr
    journal[start:start + len(layout)] = layout
    img[journal_offset:journal_offset + journal_size] = journal

    # The journal now holds a real transaction, so it is no longer uninitialised.
    put32(img, jib_off, (flags & ~mj.K_JI_JOURNAL_NEED_INIT_MASK) | mj.K_JI_JOURNAL_IN_FS_MASK)

    with open(args.dest, "wb") as f:
        f.write(bytes(img))

    print(f"{args.dest}: journal at {journal_offset}, transaction {start}..{end}")
    print(f"  adds {args.name!r} as CNID {args.cnid} to the root folder")
    print(f"  rewrites blocks {[b[0] for b in blocks]}: volume header, "
          f"catalog header node {catalog_start}, catalog leaf node {catalog_start + first_leaf}")
    print(f"  the on-disk catalog still has {len(entries)} records; the journal has {len(records)}")
    print("  the image on disk is unchanged: replay is the only way to see the file")


if __name__ == "__main__":
    main()
