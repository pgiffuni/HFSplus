# Roadmap: HFS+ toward a writable, journaled filesystem

This is the living project brief. It tracks progress against the milestone
ordering and records what has been completed and what remains.

The Oracle is Apple's source, not `fsck.hfsplus`. See
[`AGENTS.md`](AGENTS.md) for the full authority ordering.

---

## Architecture

The HFS library has **no dependency on FUSE**.

```
block device
    v
on-disk structures      (format, endian)
    v
B-tree / extent / alloc  (btree, extent, alloc)
    v
catalog / attributes / forks
    v
volume / file            (volume, file)
    v
FUSE adapter             (future crate)
```

The library is `#![deny(unsafe_code)]` throughout. Disk images are untrusted
input: every field, offset, and count is bounds-checked.

---

## Milestone status

| Milestone | Area | Status |
|---|---|---|
| 1 | HFS+ / HFSX on-disk parsing | Complete |
| 2 | Catalog B-tree | Complete |
| 3 | Extent mapping, SEEK_DATA/SEEK_HOLE | Complete |
| 4 | Allocation bitmap | Complete |
| 5 | Journal replay (crash recovery) | Complete |
| 6 | Extended attributes | Complete |
| 7 | Resource forks | Complete |
| 7B.2 | Compression metadata (detection, hiding) | Complete |
| 7E | Mutation invariants | Complete |
| 8 | Writable volume — basic mutations | Complete |
| 9 | Journal transaction assembly | Done — see notes |
| 10 | Journal transaction coverage for all mutations | In progress |
| 11 | Logical file layer (compression-aware) | Complete |
| **12** | **decmpfs decompression — zlib** | **Complete** |
| 12.1 | decmpfs decompression — LZVN | Tracked |
| 12.2 | decmpfs decompression — LZFSE | Tracked |
| 13 | Writable volume — attribute mutation | Not started |
| 14 | Writable volume — resource fork mutation | Not started |
| 15 | Catalog extent-overflow growth | Not started |
| 16 | In-tree checker (hfsck) | Done — see roadmap-write-fsck.md |
| 17 | In-tree formatter (mkfs) | Not started |
| 18 | FUSE adapter (P0) | Not started |

---

## Phase A — correctness blockers (next)

### A1. Journal transaction coverage audit

Enumerate every mutating public operation and determine for each:

```
What home blocks can change?
What allocation bitmap blocks can change?
What catalog B-tree blocks can change?
What extents-overflow blocks can change?
What Attributes File blocks can change?
What volume-header fields can change?
What must be captured in the transaction before modification?
What operation owns the transaction?
What happens on failure? What on crash + replay?
```

Current mutations: `write_file_contents`, `truncate_file`, `create_file`,
`create_folder`, `rename`, `remove`.

### A2. Journal transaction tests

For each mutation family:

1. Create a known-good image
2. Perform exactly one operation
3. Simulate interruption before home-block completion
4. Reopen / replay
5. Verify filesystem state
6. Run `fsck.hfsplus` on a throwaway copy

Never run `fsck.hfsplus` on a golden test image — it modifies them.

### A3. decmpfs decompression — LZVN / LZFSE

zlib is implemented. LZVN and LZFSE are tracked separately; they reuse the
same decmpfs metadata dispatch, so the architecture is in place.

---

## Phase B — HFS completeness

- Finish attribute mutation (SETXATTR / REMOVEXATTR)
- Finish hard-link lifecycle (all four operations: link, lookup, unlink,
  directory hard links)
- Finish catalog extent-overflow growth (>8 inline extent slots)
- Finish resource-fork mutation
- Finish timestamp / FinderInfo metadata semantics

---

## Phase C — FUSE P0

Add `src/fuse/` (thin adapter). Implement:

```
INIT  LOOKUP  FORGET  GETATTR  ACCESS
OPENDIR  READDIR  RELEASEDIR
OPEN  RELEASE  READ  WRITE  FLUSH  FSYNC
CREATE  MKDIR  UNLINK  RMDIR  RENAME
SYMLINK  READLINK  LINK
SETATTR  STATFS
```

CNID is the FUSE inode number. Do not use host filesystem inode numbers.

---

## Phase D — FUSE P1

- GETXATTR / LISTXATTR / SETXATTR / REMOVEXATTR
- LSEEK (via existing `seek_data` / `seek_hole`)
- READDIRPLUS (via existing `DirCursor` / `DirEntry`)
- Compression-aware logical READ (already implemented)
- Correct compressed-file write rejection (return EOPNOTSUPP)

---

## Phase E — FUSE P2

- BMAP
- FALLOCATE
- COPY_FILE_RANGE
- Safe FUSE cache / writeback configuration

---

## What was just completed (decmpfs compression)

Commit `7a58506` added:

- Pure-Rust zlib inflate decoder (`src/compression/zlib.rs`)
- LZ4 frame decoder (`src/compression/lz4.rs`)
- `DecmpfsHeader`, `CompressionType`, `decompress()` dispatch (`src/compression/mod.rs`)
- Volume-level `try_decompress()` integration (`src/volume/mod.rs`)
- `DECOMPRESSION_NAME` constant and `is_compressed()` detection (`src/attributes/names.rs`)
- 12 integration tests (`tests/compression.rs`)
- `journal-with-compressed.img` fixture (decmpfs zlib type 2, verified with `fsck.hfsplus`)
- `stale-attr-node-map.img` malformed fixture
- 10 bug fixes in `tools/mkfiles.py` (attribute record layout, node descriptors, B-tree header, `free_off`, etc.)

All 511 tests pass. `cargo +nightly clippy --all-targets -- -D warnings` and
`cargo fmt -- --check` are clean.

---

## Rules

- Do not redo existing infrastructure (BMAP, SEEK_DATA/SEEK_HOLE,
  ExtentRanges, DirCursor, READDIRPLUS, basic xattr reading).
- The FUSE layer must be thin; all filesystem semantics live in the library.
- For compressed files, writes return EOPNOTSUPP until a correct
  recompression path exists.
- Never panic on corrupt filesystem input.
- Every mutation must satisfy: serialized -> reopened -> `fsck.hfsplus` accepts.
