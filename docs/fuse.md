# FUSE adapter (`hfsplus-fuse`)

A thin FUSE adapter over the `hfsplus` library, built on `fuser` 0.18.

## Architecture

```text
        FUSE kernel layer
                |
                v
     +-----------------------+
     |     hfsplus-fuse      |
     | inode/handle mapping  |
     | errno conversion      |
     | FileAttr conversion   |
     | FUSE callbacks        |
     +-----------+-----------+
                 |
                 v
     +-----------------------+
     |      hfsplus          |
     | Volume / WritableVol  |
     | Catalog / Extents     |
     | Journal / Compression |
     +-----------------------+
                 |
                 v
           HFS+/HFSX image
```

## Inode numbers

FUSE inode numbers are HFS+ Catalog Node IDs (CNIDs). The mapping is a plain
identity cast (`cnid.0 as u64`). This is the filesystem's own stable identity:
it survives renames, and it is what the extents overflow B-tree and the
attributes B-tree key on.

## File handles

- `OpenFile { cnid, is_dir }` — allocated on `OPEN`/`CREATE`, tracks a regular file.
- `OpenDir { cnid, cursor }` — allocated on `OPENDIR`, tracks a directory cursor
  for resumable `READDIR`.
- Handles are stored in a `HandleTable` (internally `Mutex<HashMap<u64, _>>`),
  indexed by a monotonically increasing 64-bit token.

## Thread safety

`fuser::Filesystem` requires `Send + Sync + 'static`. The read volume is held in
an `Arc` behind an `RwLock`, enabling lazy initialization and cache
invalidation after writes. Read operations clone the `Arc` and proceed without
holding the lock. The handle table is internally synchronized with a `Mutex`.

### Read path

Read operations (`LOOKUP`, `GETATTR`, `READ`, `READDIR`, etc.) acquire a read
lock, lazily initialize the cached `Volume` if unset, clone the `Arc`, and
proceed. The lock is released immediately after the clone.

### Write path

Write operations (`CREATE`, `UNLINK`, `WRITE`, etc.) open a fresh
`FileDevice` read-write and a `WritableVolume` from it. Each public
`WritableVolume` method is self-contained within a journal transaction
(`begin_transaction` / `end_transaction`), so the write is committed before the
device is closed. After the write, the cached read volume is invalidated
(set to `None`) so the next read re-opens from disk.

The `WRITE` callback uses a read-modify-write pattern: it opens the device
read-write, reads the existing content through a `Volume`, drops that volume,
then opens a `WritableVolume` to write the merged data. Both borrows are
sequential (not overlapping), so this is sound without `unsafe`.

### Lifetime extension

The `Volume` borrows its `FileDevice` (`&'a D`). Both are co-located in
`VolumeHolder`, and the lifetime is extended to `'static` via `transmute` in the
adapter. This is sound because:
- The `FileDevice` is declared before `Volume` in the struct, so Rust drops the
  volume before the device (reverse declaration order).
- The `Volume` is only ever accessed through a shared reference (via `Arc`).
- This operates on the block-device wrapper, not raw image bytes.

## Supported operations

### Read-only operations

| FUSE operation | Library call            |
| --- | --- |
| `INIT` | — (no special capabilities) |
| `LOOKUP` | `Volume::lookup` |
| `FORGET` | — (no-op; inode lifecycle is kernel-managed) |
| `GETATTR` | `Volume::lookup_cnid` + `FileAttr` conversion |
| `ACCESS` | — (always allows read access) |
| `OPEN` | `Volume::lookup_cnid` + `HandleTable::insert_file` |
| `READ` | `Volume::read` |
| `RELEASE` | — (no-op) |
| `OPENDIR` | `Volume::lookup_cnid` + `HandleTable::insert_dir` |
| `READDIR` | `Volume::read_dir_plus` |
| `RELEASEDIR` | `HandleTable::remove` |
| `READLINK` | `Volume::read_link` |
| `STATFS` | `Volume::statfs` |
| `DESTROY` | — (drops the cached volume) |

### Writable operations

| FUSE operation | Library call            |
| --- | --- |
| `CREATE` | `WritableVolume::create_file` |
| `MKDIR`  | `WritableVolume::create_folder` |
| `UNLINK` | `WritableVolume::remove` |
| `RMDIR`  | `WritableVolume::remove` |
| `RENAME` | `WritableVolume::rename` |
| `WRITE`  | read-modify-write via `Volume::read` + `WritableVolume::write_file_contents` |
| `SETATTR` (size) | `WritableVolume::truncate_file` (supports growth and shrink) |
| `SETATTR` (mode/uid/gid) | `WritableVolume::modify_file_metadata` |
| `SETATTR` (atime/mtime) | `WritableVolume::modify_file_metadata` |
| `LINK` | `WritableVolume::create_hard_link` (reads via `Volume::resolved_fork`) |
| `SETXATTR` | `WritableVolume::setxattr` |
| `GETXATTR` | `Volume::getxattr` |
| `LISTXATTR` | `Volume::listxattr` |
| `REMOVEXATTR` | `WritableVolume::removexattr` |
| `FSYNC` | — (each write commits a journal transaction) |
| `FSYNCDIR` | — (each directory mutation commits a journal transaction) |
| `FLUSH` | — (transactions are atomic per write) |
| `SYMLINK` | `WritableVolume::create_symlink` |
| `LSEEK` | `Volume::seek_data` / `Volume::seek_hole` (SEEK_DATA, SEEK_HOLE) |
| `READDIRPLUS` | `Volume::read_dir_plus` (entries with full attributes) |
| `BMAP` | `Volume::bmap` (logical block → physical device block) |
| `FALLOCATE` | `WritableVolume::punch_hole` (FALLOC_FL_PUNCH_HOLE: zeroes data) / `truncate_file` (default: grow/shrink) |
| `COPY_FILE_RANGE` | `Volume::read` + `read_modify_write` (server-side copy) |

## Mount options

The adapter supports two modes:

- **Read-only** (default): `hfsplus-fuse <image> <mountpoint>`
- **Writable** (`-w`/`--writable`): `hfsplus-fuse <image> <mountpoint> -w`

Read-only mounts use `-o ro`; writable mounts use `-o rw`. Both use
`FSName("hfsplus")` for identification.

## FreeBSD compatibility

The adapter uses `fuser`, which provides a single API across Linux and
FreeBSD. The platform-specific differences (e.g. `fusermount` vs `fusermount3`
for unmounting) are handled in the test harness, not in the adapter.

## Known limitations

- `ACCESS` always succeeds; full permission checking awaits writable semantics.
- `READDIR` fetches all entries in a single batch; large directories are not
  paginated yet.
- `WRITE` uses a full read-modify-write cycle; sparse writes and large-file
  partial writes are correct but not optimal.
- `FALLOCATE` punch-hole zeroes the affected data but does not release
  allocation blocks. HFS+ inline extent records are a dense chain starting at
  logical block 0 with no slot for interior holes; removing descriptors would
  shift surviving data, so blocks remain allocated and the data is zeroed
  instead. SEEK_DATA/SEEK_HOLE are block-granular: a hole within a block is
  not visible until the next block boundary.
