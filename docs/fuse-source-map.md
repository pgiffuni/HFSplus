# FUSE adapter source map

This file tracks the origin of logic in the `hfsplus-fuse` adapter crate, which
translates between FUSE protocol types and HFS+ types. The adapter is BSD-2-Clause.

## Architecture

The adapter crate (`hfsplus-fuse`) depends on the `hfsplus` library and on
`fuser` (MIT). It contains no Apple-derived code. Every filesystem algorithm —
B-tree traversal, extent allocation, catalog manipulation, journal assembly,
Unicode normalization, compression decoding — lives in the `hfsplus` library.
The adapter only translates:

- FUSE inode numbers ↔ HFS+ Catalog Node IDs (CNIDs)
- FUSE file handles ↔ internal handle table entries
- `fuser::FileAttr` ↔ HFS+ `Object` attributes (mode, timestamps, sizes)
- `hfsplus::Error` ↔ FUSE `Errno`
- FUSE callback arguments ↔ `Volume` / `WritableVolume` API calls

## What is NOT translated from Apple here

All HFS+ algorithms are mapped to `hfsplus` library calls:

### Read-only path

- `lookup` → `Volume::lookup` (Apple: `cat_lookup` / `cat_idlookup`)
- `read_dir` → `Volume::read_dir_plus` (Apple: `cat_getdirentries`)
- `read` → `Volume::read` (Apple: `hfs_read`)
- `readlink` → `Volume::read_link` (Apple: `hfs_readlink`)
- `statfs` → `Volume::statfs` (Apple: `hfs_getattrlist` / `hfs_bstatfs`)
- `getxattr` → `Volume::getxattr` (Apple: `hfs_getxattr`)
- `listxattr` → `Volume::listxattr` (Apple: `hfs_listxattr`)

### Writable path

- `create` → `WritableVolume::create_file` (Apple: `cat_create`)
- `mkdir` → `WritableVolume::create_folder` (Apple: `cat_create` for folders)
- `unlink` → `WritableVolume::remove` (Apple: `cat_delete`)
- `rename` → `WritableVolume::rename` (Apple: `cat_rename`)
- `write` → read-modify-write via `Volume::read` + `WritableVolume::write_file_contents`
  (Apple: `hfs_vnop_write`)
- `setattr` (size) → `WritableVolume::truncate_file` (Apple: `hfs_vnop_setattr`)
- `setattr` (mode/uid/gid) → `WritableVolume::modify_file_metadata`
- `link` → `WritableVolume::create_hard_link` (reads via `Volume::resolved_fork`)
- `setxattr` → `WritableVolume::setxattr` (Apple: `hfs_vnop_setxattr`)
- `getxattr` → `Volume::getxattr` (Apple: `hfs_getxattr`)
- `listxattr` → `Volume::listxattr` (Apple: `hfs_listxattr`)
- `removexattr` → `WritableVolume::removexattr` (Apple: `hfs_removexattr`)
- `symlink` → `WritableVolume::create_symlink` (Apple: `hfs_mksymlink`)
- `lseek` → `Volume::seek_data` / `Volume::seek_hole` (Apple: `hfs_seek_data` / `hfs_seek_hole`)
- `readdirplus` → `Volume::read_dir_plus` (Apple: `cat_getdirentries`)
- `bmap` → `Volume::bmap` (Apple: `MapFileBlockC`)
- `fallocate` → `WritableVolume::punch_hole` (Apple: `TruncateFileC`); zeroes data without releasing blocks; default mode via `truncate_file`
- `copy_file_range` → `Volume::read` + `WritableVolume::write_file_contents` (Apple: `hfs_vnop_copyfile`)

See `docs/source-map.md` for the Apple-to-Rust mappings of those library functions.

## FUSE adapter structure

| File | Responsibility |
| --- | --- |
| `src/main.rs` | CLI entry point; parses `-w`/`--writable` flag, configures mount options |
| `src/filesystem.rs` | `Filesystem` trait impl: all FUSE callbacks, volume lifecycle management |
| `src/handles.rs` | `HandleTable`: FUSE file-handle to internal state mapping |
| `src/attr.rs` | `FileAttr` conversion: HFS+ modes, timestamps, sizes ↔ FUSE attributes |
| `src/error.rs` | Single translation point: `hfsplus::Error` → `fuser::Errno` |

## Thread safety strategy

The `HfsPlusFilesystem` struct stores:
- `image_path: String` — path to reopen the device for writes
- `volume: RwLock<Option<Arc<VolumeHolder>>>` — cached read volume
- `handles: HandleTable` — internally `Mutex`-guarded

Read callbacks acquire a read lock, lazily initialize the volume if needed,
clone the `Arc`, and release the lock. Write callbacks open a fresh
`FileDevice` + `WritableVolume` (not using the cache), then set the cache to
`None`.
