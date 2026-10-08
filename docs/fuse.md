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

- `OpenFile { cnid, is_dir }` — allocated on `OPEN`, tracks a regular file.
- `OpenDir { cnid, cursor }` — allocated on `OPENDIR`, tracks a directory
  cursor for resumable `READDIR`.
- Handles are stored in a `HandleTable` (internally `Mutex<HashMap<u64, _>>`),
  indexed by a monotonically increasing 64-bit token.

## Thread safety

`fuser::Filesystem` requires `Send + Sync + 'static`. The `hfsplus::Volume`
holds its lazy-initialized B-trees in `std::sync::OnceLock`, making it `Sync`
when the backing device is `Sync`. The FUSE adapter wraps the volume in
`Arc<Volume<'static, FileDevice>>` and accesses it through `&self`, so read
operations are lock-free at the FUSE layer. The handle table is internally
synchronized with a `Mutex`.

The lifetime extension from the device to `'static` is done with a documented
`transmute` in the adapter: both the `FileDevice` and the `Volume` are owned
together in the same struct, so the borrow is always valid. This is the same
pattern used by FUSE implementations that tie a volume to its block device.

## Supported operations (read-only)

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
| `DESTROY` | — (no-op) |

## Mount options

The initial implementation mounts read-only (`-o ro`) with auto-unmount.
Write operations are not yet supported.

## FreeBSD compatibility

The adapter uses `fuser`, which provides a single API across Linux and
FreeBSD. The platform-specific differences (e.g. `fusermount` vs `fusermount3`
for unmounting) are handled in the test harness, not in the adapter.

## Operation compatibility matrix

| Operation      | Linux | FreeBSD |
| --- | --- | --- |
| mount          | yes | yes |
| lookup         | yes | yes |
| read           | yes | yes |
| readdir        | yes | yes |
| xattr           | planned | planned |
| lseek           | planned | planned |
| bmap            | planned | planned |

## Known limitations

- Write operations (`WRITE`, `CREATE`, `MKDIR`, `UNLINK`, `RENAME`, etc.) are
  not yet implemented. The mount is always read-only.
- `ACCESS` always succeeds; full permission checking awaits writable semantics.
- `READDIR` fetches all entries in a single batch; large directories are not
  paginated yet.
- Hard links and resource forks are visible (via the catalog) but not through
  POSIX link operations.
