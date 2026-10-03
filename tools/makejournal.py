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

OLD_JOURNAL_HEADER_MAGIC = 0x4A484452
JOURNAL_HEADER_MAGIC = 0x4A4E4C78  # 'JNLx'
ENDIAN_MAGIC = 0x12345678
JOURNAL_HEADER_CKSUM_SIZE = 44
BLHDR_CHECKSUM_SIZE = 32
BLHDR_FIRST_HEADER = 0x00000002
BLHDR_CHECK_CHECKSUMS = 0x00000001
K_JI_JOURNAL_IN_FS_MASK = 0x00000001
K_JI_JOURNAL_ON_OTHER_DEVICE_MASK = 0x00000002
K_JI_JOURNAL_NEED_INIT_MASK = 0x00000004

# A recognisable but well-formed 16-byte `uuid_string_t` for the
# external-journal fixture. An all-zero UUID is not a valid one, and the point of
# the fixture is a journal reference that is *correct* about being elsewhere.
#
# Sixteen bytes, which is asserted below: assigning a slice of the wrong length
# to a `bytearray` silently *resizes* it rather than failing, so a short constant
# here truncates the whole image by the difference. That happened, and the
# symptom was a B-tree header reading a byte-for-node-size value from three
# blocks away.
EXTERNAL_JOURNAL_UUID = bytes.fromhex("4846532b65787465726e616c00010000")
assert len(EXTERNAL_JOURNAL_UUID) == 16, "ext_jnl_uuid is 16 bytes"

VOLUME_HEADER_OFFSET = 1024

# `struct block_list_header`'s five fixed fields, before `binfo[]`.
BLHDR_PREFIX_SIZE = 16
# The block list checksum covers the first 32 bytes, with its own field zeroed.
BLHDR_CHECKSUM_SIZE = 32

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


def write_external_journal(img: bytearray, journal_info_block: int, args) -> None:
    """Rewrite the info block to name a journal on a *different* device.

    A journal can live on its own volume, named by a GPT UUID in
    `struct JournalInfoBlock.ext_jnl_uuid`. `mkfs_hfsplus` cannot create that, so
    the state is written by hand -- and it is worth having, because a reader must
    decline it rather than look for a journal inside the image and find nothing.

    Two things change, and they are what the format says:

      - `kJIJournalOnOtherDeviceMask` is set and `kJIJournalInFSMask` cleared, so
        the journal is not where the flags say it is.
      - `offset` is zeroed. It is the journal's byte offset *in this volume*, and
        the journal is not in this volume, so leaving it pointing into this
        image would invite a reader to replay a journal that is not here.
        `size` is deliberately left alone: Apple passes `jib_size` to
        `open_journal_dev` on the external path as well, using it to match the
        partition.

    The journal's own blocks stay allocated and the bitmap is left alone: they are
    still in use by whatever the real external journal replaced, and clearing the
    bits would introduce a different fault from the one this fixture is about.

    Mining reference: `struct JournalInfoBlock` in `core/hfs_format.h`, and
    `core/hfs_journal.c` `hfs_journal_open`, which returns before touching the
    volume when the journal is on another device.
    """
    bs = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 40)[0]
    jib_off = journal_info_block * bs

    flags = struct.unpack_from(">I", img, jib_off)[0]
    flags &= ~K_JI_JOURNAL_IN_FS_MASK
    flags |= K_JI_JOURNAL_ON_OTHER_DEVICE_MASK
    struct.pack_into(">I", img, jib_off, flags)

    # `offset` is the journal's byte offset *in this volume*, and the journal is
    # not in this volume, so it is zeroed. `size` is left alone: Apple passes
    # `jib_size` to `open_journal_dev` on the external path too, using it to
    # match the partition, so zeroing it would describe a journal of no length.
    struct.pack_into(">Q", img, jib_off + JIB_OFFSET_OFFSET, 0)

    # `ext_jnl_uuid` is a 16-byte `uuid_string_t` at offset 52.
    uuid_at = jib_off + 52
    if len(img[uuid_at:uuid_at + 16]) != len(EXTERNAL_JOURNAL_UUID):
        sys.exit("internal error: the uuid slice does not fit; refusing to resize the image")
    img[uuid_at:uuid_at + 16] = EXTERNAL_JOURNAL_UUID

    with open(args.dest, "wb") as f:
        f.write(bytes(img))
    print(f"{args.dest}: journal moved off this device")
    print(f"  journalInfoBlock {journal_info_block}, flags now 0x{flags:08x}")
    print("  kJIJournalOnOtherDeviceMask set, kJIJournalInFSMask clear")
    print(f"  ext_jnl_uuid = {EXTERNAL_JOURNAL_UUID.hex()}")
    print("  offset zeroed: it described a position in this volume, and the journal is not here")


def write_legacy_header(img: bytearray, args) -> None:
    """Rewrite the journal header's magic to the pre-'JNLx' value.

    Apple accepts both `JOURNAL_HEADER_MAGIC` ('JNLx') and
    `OLD_JOURNAL_HEADER_MAGIC` ('JHDR'), then *converts* the old one to the new --
    "XXXdbg - convert old style magic numbers to the new one". It converts only
    after deciding not to check the checksum: Apple guards that with
    `if (magic == JOURNAL_HEADER_MAGIC)` and the comment "only check if we're the
    current journal header magic value".

    So a legacy header is a journal that must replay, and whose checksum is not
    consulted. Both halves matter, and a corpus image from a current macOS cannot
    show either. Rewriting the magic leaves the stored checksum stale, which is
    exactly what a journal converted from the old format looks like on disk.

    Mining reference: `core/hfs_journal.c` `journal_open`, the magic test and the
    conversion that follows it.
    """
    bs = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 40)[0]
    vh = VOLUME_HEADER_OFFSET
    journal_info_block = struct.unpack_from(">I", img, vh + 12)[0]
    jib_off = journal_info_block * bs
    journal_offset = struct.unpack_from(">Q", img, jib_off + JIB_OFFSET_OFFSET)[0]

    struct.pack_into(">I", img, journal_offset, OLD_JOURNAL_HEADER_MAGIC)

    with open(args.dest, "wb") as f:
        f.write(bytes(img))
    stored = struct.unpack_from(">I", img, journal_offset + 36)[0]
    print(f"{args.dest}: journal header magic rewritten to 'JHDR'")
    print(f"  at byte {journal_offset}, block {journal_offset // bs}")
    print(f"  stored checksum left at 0x{stored:08x}, which now does not match:")
    print("  Apple does not check the checksum for a legacy header, and neither")
    print("  does this crate -- so the stale value must not stop the replay")


def _block_list_offsets(img: bytearray) -> tuple[int, list[tuple[int, int]]]:
    """Byte offsets of every block list in a written journal.

    Returns the journal's byte offset and, per block list, the offsets of its
    `max_blocks` field and of `binfo[0]`'s sequence word. Walking is the only
    way to find them: the lists are a chain, each one's data length given by the
    previous one's `bytes_used`, and nothing records where the last one ends.

    Mining reference: `core/hfs_journal.c` `replay_journal` advances exactly this
    way, by `blhdr_offset += bytes_used`.
    """
    bs = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 40)[0]
    vh = VOLUME_HEADER_OFFSET
    jib_off = struct.unpack_from(">I", img, vh + 12)[0] * bs
    journal_offset = struct.unpack_from(">Q", img, jib_off + JIB_OFFSET_OFFSET)[0]
    start = struct.unpack_from(">Q", img, journal_offset + 8)[0]
    end = struct.unpack_from(">Q", img, journal_offset + 16)[0]
    blhdr_size = struct.unpack_from(">I", img, journal_offset + 32)[0]

    out = []
    at = start
    while at < end:
        max_blocks_at = journal_offset + at
        seq_at = journal_offset + at + BLHDR_PREFIX_SIZE + 12
        out.append((max_blocks_at, seq_at))
        used = struct.unpack_from(">I", img, journal_offset + at + 4)[0]
        if used == 0:
            break
        at += blhdr_size + used
    return journal_offset, out


def _refresh_blhdr_checksum(img: bytearray, at: int, blhdr_size: int) -> None:
    """Recompute a block list header's checksum after patching its fields.

    Apple's checksum covers the whole header *and* `binfo[0]`, so patching the
    sequence number or `max_blocks` invalidates it. Without this the fixtures
    trip the checksum first and the rule under test is never reached -- a test
    that passes because of a different check is worse than no test.

    Mining reference: `core/hfs_journal.h` gives `checksum` the comment
    "on-disk: checksum of this header and binfo[0]".
    """
    struct.pack_into(">I", img, at + 8, 0)
    csum = checksum_with_zeroed_field(img[at:at + blhdr_size], 8, BLHDR_CHECKSUM_SIZE)
    struct.pack_into(">I", img, at + 8, csum)


def patch_sequences(img: bytearray, values: list[int]) -> None:
    """Set each block list's transaction sequence number.

    Sequence numbers are what tell a journal that continues from one that was
    reset: `replay_journal` truncates when a list's number is neither the
    previous one nor one more. Nothing else in the image records which generation
    a list belongs to, so the numbers are the whole of it.

    Mining reference: `core/hfs_journal.c`, the `last_sequence_num` comparison.
    """
    journal_offset, lists = _block_list_offsets(img)
    if len(values) > len(lists):
        sys.exit(f"error: {len(values)} sequences for {len(lists)} block lists")
    blhdr_size = struct.unpack_from(
        ">I", img, journal_offset + 32)[0]
    at = struct.unpack_from(">Q", img, journal_offset + 8)[0]
    for i, ((_max_at, seq_at), value) in enumerate(zip(lists, values)):
        struct.pack_into(">I", img, seq_at, value)
        _refresh_blhdr_checksum(img, journal_offset + at, blhdr_size)
        # Walk to the next list so each checksum is refreshed at its own offset.
        used = struct.unpack_from(">I", img, journal_offset + at + 4)[0]
        at += blhdr_size + used
        del i
    print(f"  sequences set to {values}, checksums refreshed")


def patch_max_blocks(img: bytearray, at_list: int, value: int) -> None:
    """Overwrite one block list's `max_blocks`.

    `max_blocks` is how many blocks the list could hold, so it cannot exceed the
    blocks the journal has. `replay_journal` rejects a larger value.
    """
    journal_offset, lists = _block_list_offsets(img)
    if at_list >= len(lists):
        sys.exit(f"error: block list {at_list} does not exist ({len(lists)} present)")
    max_blocks_at, _ = lists[at_list]
    struct.pack_into(">H", img, max_blocks_at, value)
    blhdr_size = struct.unpack_from(
        ">I", img, journal_offset + 32)[0]
    start = struct.unpack_from(">Q", img, journal_offset + 8)[0]
    at = start
    for _ in range(at_list):
        used = struct.unpack_from(">I", img, journal_offset + at + 4)[0]
        at += blhdr_size + used
    _refresh_blhdr_checksum(img, journal_offset + at, blhdr_size)
    print(f"  block list {at_list}: max_blocks set to {value}, checksum refreshed")


def patch_zero_bsize(img: bytearray, args) -> None:
    """Zero one block list entry's `bsize`, as `replay_journal` refuses.

    The data cursor through a block list advances by each entry's size, so a zero
    here desynchronises every *later* entry in the same list: they would be read
    from the wrong offset and replay plausible nonsense. Apple prints "invalid
    bsize" and truncates the transaction; a reader that skips the entry instead
    produces garbage that looks like a successful replay.

    Mining reference: `core/hfs_journal.c` `replay_journal`, the `size == 0`
    test inside the block loop.
    """
    journal_offset, lists = _block_list_offsets(img)
    at_list = args.bad_bsize - 1
    if at_list >= len(lists):
        sys.exit(f"error: block list {at_list} does not exist ({len(lists)} present)")
    blhdr_size = struct.unpack_from(">I", img, journal_offset + 32)[0]
    start = struct.unpack_from(">Q", img, journal_offset + 8)[0]
    at = start
    for _ in range(at_list):
        used = struct.unpack_from(">I", img, journal_offset + at + 4)[0]
        at += blhdr_size + used

    # binfo[0] is the sequence slot, so the first block entry is index 1.
    bsize_at = journal_offset + at + BLHDR_PREFIX_SIZE + BLHDR_PREFIX_SIZE + 8
    struct.pack_into(">I", img, bsize_at, 0)
    _refresh_blhdr_checksum(img, journal_offset + at, blhdr_size)
    print(f"  block list {at_list}: first entry's bsize set to 0, checksum refreshed")


def patch_replay_rules(img: bytearray, args) -> None:
    """Build a journal that breaks one of the replay rules, for the tests.

    Both faults are ones Apple refuses and the reader previously did not, so
    without a fixture for each there is no evidence the refusal is wired in.

    Mining reference: `core/hfs_journal.c` `replay_journal` -- the
    `last_sequence_num` comparison, and the `max_blocks > size / jhdr_size` test.
    """
    with open(args.source, "rb") as f:
        original = f.read()
    # The rules apply to a journal with transactions in it, so start from one of
    # the generated replay images rather than a fresh volume.
    if args.dest.endswith(".img"):
        pass

    print(f"{args.dest}: replay-rule fixtures applied on top of the source")
    if args.bad_sequence:
        journal_offset, lists = _block_list_offsets(img)
        n = len(lists)
        if n < 2:
            sys.exit("error: the source journal has fewer than two block lists")
        # 1, 2, ... is the normal progression. Jumping from the second onwards is
        # what a journal that was reset and appended to looks like, and is the
        # case Apple truncates.
        values = [1] + [9] * (n - 1)
        patch_sequences(img, values)
    if args.bad_bsize:
        patch_zero_bsize(img, args)
    if args.bad_max_blocks:
        bs = struct.unpack_from(">I", img, VOLUME_HEADER_OFFSET + 40)[0]
        vh = VOLUME_HEADER_OFFSET
        jib_off = struct.unpack_from(">I", img, vh + 12)[0] * bs
        journal_offset = struct.unpack_from(">Q", img, jib_off + JIB_OFFSET_OFFSET)[0]
        size = struct.unpack_from(">Q", img, jib_off + JIB_SIZE_OFFSET)[0]
        jhdr_size = struct.unpack_from(">I", img, journal_offset + 40)[0] or bs
        capacity = size // jhdr_size
        patch_max_blocks(img, args.bad_max_blocks - 1, capacity + 1)

    with open(args.dest, "wb") as f:
        f.write(bytes(img))


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
    ap.add_argument("--bad-sequence", dest="bad_sequence", action="store_true",
                    help="rewrite the transaction sequence numbers so they jump")
    ap.add_argument("--bad-bsize", dest="bad_bsize", type=int, default=0,
                    metavar="LIST",
                    help="zero block list LIST's first entry bsize")
    ap.add_argument("--bad-max-blocks", dest="bad_max_blocks", type=int,
                    default=0, metavar="LIST",
                    help="inflate block list LIST's max_blocks beyond the journal")
    ap.add_argument("--legacy-header", dest="legacy_header",
                    action="store_true",
                    help="rewrite the journal header magic to the old 'JHDR' value")
    ap.add_argument("--external-journal", dest="external_journal",
                    action="store_true",
                    help="rewrite the info block to name a journal on another device")
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

    if args.external_journal:
        return write_external_journal(img, journal_info_block, args)
    if args.legacy_header:
        return write_legacy_header(img, args)
    if args.bad_sequence or args.bad_max_blocks or args.bad_bsize:
        return patch_replay_rules(img, args)

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