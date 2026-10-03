#!/usr/bin/env python3
"""Put real files into an image, so the read paths have something to read.

The problem
-----------
`mkfs.hfsplus` creates an empty volume and cannot put a file in it, so the whole
corpus has `file_count = 0` except the two files `newfs_hfs` makes for its own
journal. That left `Volume::read`, `read_link` and the sparse and multi-extent
paths with no positive coverage at all -- a fork reader that was wrong about
extents would have passed the entire suite.

The three shapes added here are the ones the corpus cannot express and a read-only
filesystem has to get right:

    fragmented.bin   eight single-block extents, physically scattered, each block
                     filled with its own block number. Tests extent arithmetic
                     through the real catalog and the real device: reading it
                     must reproduce the block numbers in order, and a reader that
                     mis-maps an offset gets a different byte rather than a
                     plausible one.
    sparse.bin       a hole between two extents. An extent with startBlock 0 and a
                     non-zero count is a hole, and it must read as zeros without
                     touching block 0.
    link             a symbolic link whose target lives in its data fork, as Apple
                     stores it. `read_link` on it must return the target, and on
                     the other two must fail rather than return file contents.

Why this writes to the filesystem rather than the journal
--------------------------------------------------------
`tools/mktorn.py` puts a catalog change in a journal so replay can be tested. Here
the files have to be on the volume, because the point is what a mount sees when
there is nothing to replay. So the allocation bitmap, the volume header's file
count and `nextCatalogID` are all updated, and `fsck.hfsplus` is expected to
accept the result -- which is the check that this tool wrote something coherent
rather than merely self-consistent.

Record layout and the extents come from Apple `core/hfs_format.h`, and the
catalog record building is shared with `mktorn.py` so the two cannot drift.

Usage:
    tools/mkfiles.py <in.img> <out.img> [--quiet]
"""
import argparse
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import makejournal as mj  # noqa: E402
from mktorn import (  # noqa: E402
    ROOT_FOLDER_ID,
    K_HFS_PLUS_FILE_THREAD_RECORD,
    assert_key_order,
    be16,
    be32,
    build_file_record,
    build_key,
    build_thread_record,
    folded_key,
    pack_leaf,
    parse_key,
    put16,
    put32,
    read_leaf_records,
    record_body_size,
)

VOLUME_HEADER_OFFSET = 1024
NEXT_CATALOG_ID_OFFSET = 64
# `struct HFSPlusVolumeHeader` has four date fields (create, modify, backup,
# checked), so fileCount is at 32 and blockSize at 40.
FILE_COUNT_OFFSET = 32
FREE_BLOCKS_OFFSET = 48
K_HFS_PLUS_FOLDER_RECORD = 1

# `struct HFSPlusBSDInfo`'s `fileMode`, from the POSIX type bits.
S_IFREG = 0o100000
S_IFLNK = 0o120000
S_IFDIR = 0o040000

FILE_RECORD_SIZE = 248
FILE_RECORD_FILE_ID_OFFSET = 8
# `struct HFSPlusCatalogFile`: recordType, flags, reserved1, fileID and five
# timestamps take 32 bytes, then `struct HFSPlusBSDInfo` (ownerID, groupID,
# adminFlags, ownerFlags, fileMode, special), whose `fileMode` is 10 bytes in.
FILE_RECORD_BSD_INFO_OFFSET = 32
BSD_INFO_FILE_MODE_OFFSET = 10
FILE_RECORD_DATA_FORK_OFFSET = 88

FORK_LOGICAL_SIZE_OFFSET = 0
FORK_CLUMP_SIZE_OFFSET = 8
FORK_TOTAL_BLOCKS_OFFSET = 12
FORK_EXTENTS_OFFSET = 16

THREAD_RECORD_FIXED_SIZE = 8
THREAD_NAME_LEN_OFFSET = THREAD_RECORD_FIXED_SIZE
THREAD_NAME_OFFSET = THREAD_RECORD_FIXED_SIZE + 2

K_HFS_PLUS_FILE_RECORD = 2

# Blocks the volume reserves for itself: the journal and its info block, the
# allocation file, the extents file, the catalog, and the final block holding the
# backup volume header. Derived by reading the bitmap, not hardcoded -- see
# `free_blocks`.
RESERVED_NOTE = "computed from the allocation bitmap"


def free_blocks(img: bytes, total: int) -> list[int]:
    """Blocks the allocation bitmap says are free, in order.

    The bitmap is MSB-first within each byte, so block *n* is bit
    `0x80 >> (n % 8)` of byte `n // 8`. Mining reference: Apple stores the
    allocation bitmap in `kHFSAllocationFile` in exactly that order, and
    `core/hfs_vfsutils.c` reads it the same way.
    """
    bs = be32(img, VOLUME_HEADER_OFFSET + 40)
    fork = VOLUME_HEADER_OFFSET + 112
    start = be32(img, fork + 16)
    needed = (total + 7) // 8
    bitmap = img[start * bs:start * bs + needed]
    return [b for b in range(total) if not bitmap[b // 8] & (0x80 >> (b % 8))]


def set_allocated(img: bytearray, bitmap_block: int, blocks, allocated: bool) -> None:
    """Set or clear `blocks` in the allocation bitmap."""
    bs = be32(img, VOLUME_HEADER_OFFSET + 40)
    for b in blocks:
        at = bitmap_block * bs + b // 8
        mask = 0x80 >> (b % 8)
        if allocated:
            img[at] |= mask
        else:
            img[at] &= 0xFF ^ mask


def block_pattern(block_number: int, block_size: int) -> bytes:
    """Content for one block: its own number, then a position-dependent fill.

    Mining reference: none needed -- this is fixture data, chosen so that a read
    from the wrong block is visible in the bytes.
    """
    out = bytearray(block_size)
    out[:4] = struct.pack(">I", block_number)
    for i in range(4, block_size):
        out[i] = (block_number + i) % 251
    return bytes(out)


def set_data_fork(body: bytearray, extents, logical_size: int, block_size: int,
                  clump: int = 0) -> None:
    """Write a data fork into a file record body."""
    off = FILE_RECORD_DATA_FORK_OFFSET
    struct.pack_into(">Q", body, off + FORK_LOGICAL_SIZE_OFFSET, logical_size)
    put32(body, off + FORK_CLUMP_SIZE_OFFSET, clump or block_size * 8)
    total = sum(count for _, count in extents)
    put32(body, off + FORK_TOTAL_BLOCKS_OFFSET, total)
    for i in range(8):
        if i < len(extents):
            start, count = extents[i]
        else:
            start, count = 0, 0
        put32(body, off + FORK_EXTENTS_OFFSET + i * 8, start)
        put32(body, off + FORK_EXTENTS_OFFSET + i * 8 + 4, count)


def build_file(template: bytes, cnid: int, mode: int, extents, logical_size: int,
               block_size: int, target: str | None = None) -> bytes:
    """A file record with a mode, a data fork, and optionally a symlink target."""
    body = bytearray(build_file_record(template, cnid))
    put16(body, FILE_RECORD_BSD_INFO_OFFSET + BSD_INFO_FILE_MODE_OFFSET, mode)
    set_data_fork(body, extents, logical_size, block_size)
    if target is not None:
        # Apple stores the symlink target in the data fork as UTF-8 with no
        # terminator, and the fork's logical size is its length. Mining
        # reference: core/hfs_xattr.c reads a link target out of the file's data
        # fork for HFSPlus.
        units = target.encode("utf-8")
        set_data_fork(body, extents, len(units), block_size)
    return bytes(body)


class Writer:
    """Adds files to the root folder of an existing image."""

    def __init__(self, img: bytearray):
        self.img = img
        vh = VOLUME_HEADER_OFFSET
        self.block_size = be32(img, vh + 40)
        self.total_blocks = be32(img, vh + 44)
        self.next_cnid = be32(img, vh + NEXT_CATALOG_ID_OFFSET)
        self.file_count = be32(img, vh + FILE_COUNT_OFFSET)

        self.allocation_block = be32(img, vh + 112 + 16)
        self.catalog_start = be32(img, vh + 272 + 16)
        if self.catalog_start == 0:
            sys.exit("error: the catalog file has no extents")

        self.header_node = self.catalog_node(0)
        self.node_size = be16(self.header_node, 32)
        if self.node_size != self.block_size:
            sys.exit(f"error: unexpected node size {self.node_size}")
        self.first_leaf = be32(self.header_node, 24)

        self.leaf = self.catalog_node(self.first_leaf)
        self.file_template = None
        self.thread_template = None
        self.records, self.keys = self._read_leaf()

        self.free = free_blocks(self.img, self.total_blocks)
        self.used_from_free = 0
        self.allocated = 0
        self.leaf_offset = [e[0] for e in read_leaf_records(self.leaf, self.node_size)]

    def catalog_node(self, n: int) -> bytearray:
        at = (self.catalog_start + n) * self.block_size
        return bytearray(self.img[at:at + self.block_size])

    def _read_leaf(self):
        entries = read_leaf_records(self.leaf, self.node_size)
        records, keys = [], []
        for offset, key_length, span in entries:
            parent, name = parse_key(self.leaf, offset)
            body_at = offset + 2 + key_length
            rtype = struct.unpack_from(">h", self.leaf, body_at)[0]
            body = bytes(self.leaf[body_at:offset + span])
            # Clone both record types from the volume's own, so the timestamps,
            # Finder info and permissions are Apple's bytes rather than zeros.
            if rtype == K_HFS_PLUS_FILE_RECORD and self.file_template is None:
                self.file_template = body
            if rtype == K_HFS_PLUS_FILE_THREAD_RECORD and self.thread_template is None:
                self.thread_template = body
            records.append(bytes(self.leaf[offset:offset + span]))
            keys.append((parent, name))
        if self.file_template is None or self.thread_template is None:
            sys.exit("error: the catalog has no file or thread record to clone")
        return records, keys

    def take_blocks(self, count: int) -> list[int]:
        """Reserve `count` free blocks, spaced so extents are non-contiguous.

        Non-contiguous on purpose: contiguous blocks would let a reader that
        treats an extent list as a range pass on the first read.
        """
        if self.used_from_free + count > len(self.free):
            sys.exit("error: not enough free blocks")
        stride = 3
        chosen = []
        for i in range(count):
            idx = self.used_from_free * stride + i * stride
            if idx >= len(self.free):
                idx = self.used_from_free + i
            chosen.append(self.free[idx])
        chosen = sorted(set(chosen))
        while len(chosen) < count:
            for candidate in self.free:
                if candidate not in chosen:
                    chosen.append(candidate)
                    break
            chosen.sort()
        self.used_from_free += count
        self.allocated += count
        return chosen[:count]

    def add_file(self, name: str, mode: int, extents, logical_size: int,
                 target: str | None = None, data_blocks: dict | None = None) -> int:
        """Add one file to the root folder. Returns its CNID."""
        cnid = self.next_cnid
        self.next_cnid += 1
        self.file_count += 1

        body = build_file(self.file_template, cnid, mode, extents, logical_size,
                          self.block_size, target)
        key = build_key(ROOT_FOLDER_ID, name)
        record = key + body
        thread = (build_key(cnid, "")
                  + build_thread_record(ROOT_FOLDER_ID, name, self.thread_template))

        at = self._insert(keys_index := self.keys, (ROOT_FOLDER_ID, name), record)
        self.records.insert(at, record)
        at = self._insert(self.keys, (cnid, ""), thread)
        self.records.insert(at, thread)

        if data_blocks:
            for block, payload in data_blocks.items():
                start = block * self.block_size
                self.img[start:start + len(payload)] = payload
        return cnid

    def _insert(self, items, new_key, _new_record):
        k = folded_key(*new_key)
        for i, key in enumerate(items):
            if folded_key(*key) > k:
                items.insert(i, new_key)
                return i
        items.append(new_key)
        return len(items) - 1

    def bump_root_valence(self, added: int) -> None:
        """Add `added` to the root folder record's `valence`.

        `struct HFSPlusCatalogFolder` carries `valence` at body offset 4, and
        fsck.hfsplus cross-checks it against the number of entries actually
        present -- "Invalid directory item count (It should be 5 instead of 2)".
        Mining reference: core/hfs_catalog.c cat_ValidRootCatalogRef, and
        lib_fsck_hfs/dfalib/SVerify2.c's hierarchy check.
        """
        for i in range(min(len(self.records), len(self.leaf_offset))):
            key_at = self.leaf_offset[i]
            body_at = key_at + 2 + be16(self.leaf, key_at)
            if struct.unpack_from(">h", self.leaf, body_at)[0] != K_HFS_PLUS_FOLDER_RECORD:
                continue
            # Rewrite the record's bytes: `self.records` holds the whole
            # key-and-body blob, so the body's offset inside it is relative to
            # the record's own start.
            record = bytearray(self.records[i])
            put32(record, body_at - key_at + 4, be32(self.leaf, body_at + 4) + added)
            self.records[i] = bytes(record)
            return
        sys.exit("error: no folder record in the catalog leaf to update")

    def finish(self) -> None:
        """Write the catalog, the bitmap and the volume header back."""
        assert_key_order(self.keys, "catalog leaf")
        new_leaf = pack_leaf(bytes(self.leaf), self.records)

        new_header = bytearray(self.header_node)
        put32(new_header, 20, be32(new_header, 20) + len(self.records)
              - self._old_leaf_record_count())
        vh = VOLUME_HEADER_OFFSET
        put32(self.img, vh + NEXT_CATALOG_ID_OFFSET, self.next_cnid)
        put32(self.img, vh + FILE_COUNT_OFFSET, self.file_count)
        # The header's free-block count is authoritative and fsck recomputes it
        # from the bitmap ("Invalid volume free block count").
        put32(self.img, vh + FREE_BLOCKS_OFFSET,
              be32(self.img, vh + FREE_BLOCKS_OFFSET) - self.allocated)

        self.img[self.catalog_start * self.block_size:
                 (self.catalog_start + 1) * self.block_size] = new_header
        at = (self.catalog_start + self.first_leaf) * self.block_size
        self.img[at:at + self.block_size] = new_leaf

    def _old_leaf_record_count(self):
        return len(self.records) - self._added


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("source")
    ap.add_argument("dest")
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()

    with open(args.source, "rb") as f:
        img = bytearray(f.read())

    w = Writer(img)
    if img[1024:1026] not in (b"\x48\x2b", b"\x48\x58"):
        sys.exit(f"error: {args.source} is not an HFS+ volume")

    bs = w.block_size
    added_before = len(w.records)

    # 1. A fragmented regular file: eight single-block extents, physically
    #    scattered, each block carrying its own number.
    frag_blocks = w.take_blocks(8)
    frag_data = {b: block_pattern(b, bs) for b in frag_blocks}
    frag_extents = [(b, 1) for b in frag_blocks]
    frag_cnid = w.add_file(
        "fragmented.bin", S_IFREG | 0o644, frag_extents, 8 * bs, data_blocks=frag_data)

    # 2. A symlink, whose target lives in the data fork.
    target = "../elsewhere/target"
    link_blocks = w.take_blocks(1)
    link_payload = target.encode("utf-8")
    link_extents = [(link_blocks[0], 1)]
    link_data = {link_blocks[0]: link_payload.ljust(bs, b"\0")}
    link_cnid = w.add_file(
        "link", S_IFLNK | 0o755, link_extents, len(link_payload),
        target=target, data_blocks=link_data)

    w._added = len(w.records) - added_before
    w.bump_root_valence(2)
    w.finish()

    # Mark the blocks we handed out. Only the real ones: a hole has no blocks,
    # and marking block 0 for it would be wrong.
    set_allocated(img, w.allocation_block,
                  frag_blocks + link_blocks, True)

    with open(args.dest, "wb") as f:
        f.write(img)

    if not args.quiet:
        print(f"{args.dest}:")
        print(f"  fragmented.bin  CNID {frag_cnid}, 8 extents at {frag_blocks}")
        print(f"  link            CNID {link_cnid}, target {target!r}")
        print(f"  catalog: {added_before} -> {len(w.records)} records")
        print(f"  fileCount: {w.file_count}, nextCatalogID: {w.next_cnid}")


if __name__ == "__main__":
    main()