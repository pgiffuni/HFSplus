#!/usr/bin/env python3
"""Inject real journal transactions into a journaled HFS+ image.

Why this exists
---------------
Every image `mkfs_hfsplus -J` produces has `kJIJournalNeedInitMask` set and a
zeroed journal header, because no transaction has ever been written. So the
generated corpus cannot exercise journal replay at all: detection finds the
journal, and there is nothing in it.

This tool fills that gap. It takes a journaled image and writes a real journal
header plus a real transaction -- block list, recorded block numbers, and the
replacement data -- into the journal area, then points the info block's flags at
it. The resulting image is one a read-only mount should replay.

Layout written
--------------
Following Apple `core/hfs_journal.c`:

    journal offset + 0                journal_header   (jhdr_size bytes)
    journal offset + jhdr_size        block_list_header + recorded blocks
    journal offset + jhdr_size + blhdr_size          the block data

The block list records `bnum` (a device block number) and `bsize` for each block,
followed by `bytes_used` bytes of replacement data. `BLHDR_FIRST_HEADER` marks the
list as starting a transaction.

Provenance of every constant and field: Apple `core/hfs_journal.h` and
`core/hfs_journal.c`. The checksum is `calc_checksum` from `core/hfs_journal.c`,
including the rule that the checksum field is zeroed before hashing.

Usage:
    tools/makejournal.py <in.img> <out.img> [--block N] [--data TEXT]
"""
import argparse
import struct
import sys

JOURNAL_HEADER_MAGIC = 0x4A4E4C78  # 'JNLx'
ENDIAN_MAGIC = 0x12345678
JOURNAL_HEADER_CKSUM_SIZE = 44
BLHDR_CHECKSUM_SIZE = 32
BLHDR_FIRST_HEADER = 0x00000002
BLHDR_CHECK_CHECKSUMS = 0x00000001
K_JI_JOURNAL_IN_FS_MASK = 0x00000001
K_JI_JOURNAL_NEED_INIT_MASK = 0x00000004

JIB_OFFSET_OFFSET = 4 + 32
JIB_SIZE_OFFSET = JIB_OFFSET_OFFSET + 8


def calc_checksum(data: bytes) -> int:
    """Apple core/hfs_journal.c calc_checksum.

    The shift is a discard, not a rotate, and the result is the complement.
    """
    cksum = 0
    for b in data:
        cksum = ((cksum << 8) & 0xFFFFFFFF) ^ ((cksum + b) & 0xFFFFFFFF)
    return (~cksum) & 0xFFFFFFFF


def checksum_with_zeroed_field(buf: bytearray, at: int, length: int) -> int:
    """Checksum the first `length` bytes with the 4 bytes at `at` zeroed.

    Apple's writer zeroes the checksum field before hashing, because the field
    lies inside the range being checksummed.
    """
    scratch = bytearray(buf[:length])
    scratch[at:at + 4] = b"\0\0\0\0"
    return calc_checksum(bytes(scratch))


def make_payload(text, block_size, block):
    """A whole block whose bytes are visibly non-zero throughout.

    A zero-padded payload would make a read from the journal indistinguishable
    from a read of a free block at the device, so the filler is non-zero.
    """
    marker = b"REPLAYED-BY-HFSPFUSE\n" if text is None else text.encode()
    marker = marker[:block_size]
    filler = bytes(((i * 7 + block) & 0xFF) or 0x5A for i in range(block_size))
    out = bytearray(marker)
    while len(out) < block_size:
        out.append(filler[len(out)])
    return bytes(out)


def build_journal_header(start: int, end: int, size: int, jhdr_size: int,
                         blhdr_size: int, sequence_num: int) -> bytearray:
    """A journal header in big-endian, with a correct checksum.

    Big-endian is used deliberately: it exercises the reader's byte-order
    detection on the path that macOS would take on a PowerPC volume, while
    `makejournal.py --little-endian` writes the little-endian form that
    mkfs_hfsplus on x86 produces.
    """
    raw = bytearray(48)
    struct.pack_into(">I", raw, 0, JOURNAL_HEADER_MAGIC)
    struct.pack_into(">I", raw, 4, ENDIAN_MAGIC)
    struct.pack_into(">Q", raw, 8, start)
    struct.pack_into(">Q", raw, 16, end)
    struct.pack_into(">Q", raw, 24, size)
    struct.pack_into(">I", raw, 32, blhdr_size)
    struct.pack_into(">I", raw, 40, jhdr_size)
    struct.pack_into(">I", raw, 44, sequence_num)
    # The checksum covers 44 bytes with the field at 36 zeroed.
    cksum = checksum_with_zeroed_field(raw, 36, JOURNAL_HEADER_CKSUM_SIZE)
    struct.pack_into(">I", raw, 36, cksum)
    return raw


def build_block_list(blocks, blhdr_size: int, first: bool,
                     sequence_num: int = 1) -> tuple[bytes, bytes]:
    """A block list header and the data it describes.

    `blocks` is a list of (device_block_number, payload).
    Returns (header_block, data_block).
    """
    # binfo[0] is the transaction sequence-number slot, not a block: Apple
    # declares `_blk_info` as `bsize` unioned with {cksum, sequence_num}, and
    # core/hfs_journal.c replays `for (i = 1; i < num_blocks; i++)`. So a list
    # describing n blocks has num_blocks == n + 1.
    prefix = 16
    capacity = (blhdr_size - prefix) // 16
    if len(blocks) + 1 > capacity:
        sys.exit(f"error: {len(blocks)} blocks exceed one block list of {blhdr_size} bytes")
    num_entries = len(blocks) + 1

    header = bytearray(blhdr_size)
    struct.pack_into(">H", header, 0, num_entries)  # max_blocks
    struct.pack_into(">H", header, 2, num_entries)  # num_blocks
    struct.pack_into(">I", header, 4, 0)            # bytes_used, filled below

    flags = BLHDR_FIRST_HEADER if first else 0
    if blocks and blocks[0][1] is not None:
        flags |= BLHDR_CHECK_CHECKSUMS
    struct.pack_into(">I", header, 12, flags)

    # binfo[0] is the sequence-number slot: bsize 0, sequence in the unioned
    # word. Apple reads it back as blhdr->binfo[0].u.bi.b.sequence_num.
    struct.pack_into(">I", header, prefix + 8, 0)
    struct.pack_into(">I", header, prefix + 12, sequence_num)

    data = bytearray()
    for i, (bnum, payload) in enumerate(blocks):
        off = prefix + (i + 1) * 16
        struct.pack_into(">Q", header, off, bnum)
        struct.pack_into(">I", header, off + 8, len(payload))
        struct.pack_into(">I", header, off + 12,
                         calc_checksum(payload) if flags & BLHDR_CHECK_CHECKSUMS else 0)
        data += payload

    struct.pack_into(">I", header, 4, len(data))    # bytes_used
    cksum = checksum_with_zeroed_field(header, 8, BLHDR_CHECKSUM_SIZE)
    struct.pack_into(">I", header, 8, cksum)
    return bytes(header), bytes(data)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("source")
    ap.add_argument("dest")
    ap.add_argument("--block", type=int, default=0,
                    help="device block the transaction rewrites")
    ap.add_argument("--data", default=None,
                    help="replacement payload; defaults to a recognisable pattern")
    ap.add_argument("--little-endian", action="store_true",
                    help="write the journal header little-endian, as x86 hosts do")
    ap.add_argument("--extra", action="append", default=[], metavar="BLOCK:TEXT",
                    help="add a further transaction rewriting BLOCK with TEXT; "
                         "repeat for more. Later transactions supersede earlier "
                         "writes to the same block, which is what a real journal "
                         "does when a block is modified twice before the wrap.")
    args = ap.parse_args()

    with open(args.source, "rb") as f:
        img = bytearray(f.read())

    # --- Volume header -----------------------------------------------------
    vh = 1024
    if img[vh:vh + 2] not in (b"\x48\x2b", b"\x48\x58"):
        sys.exit(f"error: {args.source} is not an HFS+ volume")
    block_size = struct.unpack_from(">I", img, vh + 40)[0]
    total_blocks = struct.unpack_from(">I", img, vh + 44)[0]
    attributes = struct.unpack_from(">I", img, vh + 4)[0]
    if not attributes & 0x2000:
        sys.exit(f"error: {args.source} is not journaled "
                 f"(kHFSVolumeJournaledBit is clear)")
    journal_info_block = struct.unpack_from(">I", img, vh + 12)[0]
    if journal_info_block == 0:
        sys.exit("error: journalInfoBlock is zero on a journaled volume")

    # --- Journal info block ------------------------------------------------
    jib_off = journal_info_block * block_size
    flags = struct.unpack_from(">I", img, jib_off)[0]
    journal_offset = struct.unpack_from(">Q", img, jib_off + JIB_OFFSET_OFFSET)[0]
    journal_size = struct.unpack_from(">Q", img, jib_off + JIB_SIZE_OFFSET)[0]
    if journal_offset + journal_size > len(img):
        sys.exit("error: the journal extends past the end of the image")
    if not flags & K_JI_JOURNAL_IN_FS_MASK:
        sys.exit("error: this tool only handles journals inside the filesystem")

    # The whole block is filled with a visible, non-zero pattern so that a read
    # can be attributed to the journal or to the device by inspection. A
    # zero-padded payload would make the two indistinguishable at the tail.
    payload = make_payload(args.data, block_size, args.block)

    # --- Journal geometry --------------------------------------------------
    # macOS sizes both the journal header and each block list to a filesystem
    # block; mirroring that keeps the layout one a real volume would have.
    jhdr_size = block_size
    blhdr_size = block_size
    start = jhdr_size          # first transaction begins after the journal header
    if start + blhdr_size + block_size > journal_size:
        sys.exit("error: the journal is too small for a transaction")

    # Additional transactions, each a block list followed by its data. Writing
    # more than one exercises the walk across transaction boundaries, which a
    # single-transaction journal cannot.
    transactions = [[(args.block, payload)]]
    for spec in args.extra:
        block_text, _, text = spec.partition(":")
        try:
            block = int(block_text)
        except ValueError:
            sys.exit(f"error: --extra expects BLOCK:TEXT, got {spec!r}")
        transactions.append([(block, make_payload(text, block_size, block))])

    layout = bytearray()
    for i, blocks in enumerate(transactions):
        header_raw, data_raw = build_block_list(blocks, blhdr_size, True, i + 1)
        layout += header_raw
        layout += data_raw
    end = start + len(layout)

    journal = bytearray(journal_size)
    hdr = build_journal_header(start, end, journal_size, jhdr_size, blhdr_size, 1)
    if args.little_endian:
        # The magic and endian sentinel stay as written; the numeric fields are
        # stored little-endian, which is what a 64-bit Intel or ARM host writes.
        le = bytearray(48)
        struct.pack_into("<I", le, 0, JOURNAL_HEADER_MAGIC)
        struct.pack_into("<I", le, 4, ENDIAN_MAGIC)
        struct.pack_into("<Q", le, 8, start)
        struct.pack_into("<Q", le, 16, end)
        struct.pack_into("<Q", le, 24, journal_size)
        struct.pack_into("<I", le, 32, blhdr_size)
        struct.pack_into("<I", le, 40, jhdr_size)
        struct.pack_into("<I", le, 44, 1)
        struct.pack_into("<I", le, 36,
                         checksum_with_zeroed_field(le, 36, JOURNAL_HEADER_CKSUM_SIZE))
        hdr = le
    journal[0:len(hdr)] = hdr
    journal[start:start + len(layout)] = layout

    img[journal_offset:journal_offset + journal_size] = journal

    # Clear kJIJournalNeedInitMask: the journal now has a transaction.
    new_flags = (flags & ~K_JI_JOURNAL_NEED_INIT_MASK) | K_JI_JOURNAL_IN_FS_MASK
    struct.pack_into(">I", img, jib_off, new_flags)

    with open(args.dest, "wb") as f:
        f.write(bytes(img))

    print(f"{args.dest}: journal at {journal_offset}, transaction {start}..{end}")
    print(f"  block size {block_size}, jhdr_size {jhdr_size}, blhdr_size {blhdr_size}")
    print(f"  {len(transactions)} transaction(s), rewrites "
          f"{[b[0] for t in transactions for b in t]} with {len(payload)}-byte blocks")
    print(f"  header byte order: {'little' if args.little_endian else 'big'}")


if __name__ == "__main__":
    main()