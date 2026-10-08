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
- FUSE callback arguments ↔ `Volume` API calls

## What is NOT translated from Apple here

All HFS+ algorithms are mapped to `hfsplus` library calls:

- `lookup` → `Volume::lookup` (Apple: `cat_lookup` / `cat_idlookup`)
- `read_dir` → `Volume::read_dir_plus` (Apple: `cat_getdirentries`)
- `read` → `Volume::read` (Apple: `hfs_read`)
- `readlink` → `Volume::read_link` (Apple: `hfs_readlink`)
- `statfs` → `Volume::statfs` (Apple: `hfs_getattrlist` / `hfs_bstatfs`)

See `docs/source-map.md` for the Apple-to-Rust mappings of those library functions.
