# HFS+ Encryption: supported and unsupported

## Status: Apple has dropped encryption support

Apple has deprecated and is dropping HFS+ encryption (FileVault / content
protection). New deployments should use APFS with FileVault. This document
records the three-tier distinction this crate makes regarding HFS+
encryption, based on Apple's implementation in the HFS+ filesystem.

## 1. Ordinary volumes (supported)

This crate reads, writes, and journals HFS+ / HFSX volumes without
encryption, including volumes whose **underlying block device** has already
been decrypted (e.g. an APFS container decrypted by `diskarbitrationd`, or a
DMG decrypted by `hdiutil attach`). Such volumes have **no**
`kHFSContentProtectionMask` (bit 30) set in the volume header's
`attributes` field, and no file on the volume carries a
`com.apple.system.cprotect` extended attribute.

No encryption-specific handling is required: the filesystem is presented
exactly as Apple's on-disk format describes it, and all read/write/journal
operations work as documented.

## 2. Encrypted-file / unknown encryption metadata (detection, no passthrough)

A volume **may** declare `kHFSContentProtectionMask` (bit 30, value
`0x40000000`) in its header. This is a **volume-level hint** that content
protection *may* be in use — it does not mean any particular file is
encrypted, and it does **not** indicate full-volume encryption (which is an
HFSX+ feature handled at a different layer).

Per-file encryption is tracked via an extended attribute named
`com.apple.system.cprotect` (`struct cprotect` in Apple's `hfs_cprotect.h`).
This attribute is **not** the same as `com.apple.decmpfs` (compression) or
`com.apple.FinderInfo` — it carries a separate header describing the file's
protection class (`CP_PCLASS`) and encryption key class.

### Current behavior

This crate now **fails closed** on content-protected files and volumes:

- **Volume-level rejection**: `make_holder()` in `hfsplus-fuse` checks the
  volume header flag via `VolumeHeader::has_content_protection()` and refuses
  to mount volumes with `kHFSContentProtectionMask` set, returning an
  explicit error. This is a fast-path: the bit is clear on volumes whose
  files cannot be content-protected.
- **Per-file detection**: `try_decompress()` in `src/volume/mod.rs` checks
  for the `com.apple.system.cprotect` extended attribute on individual files
  and returns `Error::unsupported("content protection: ...")` instead of
  returning ciphertext. This catches the case Apple's format warns about: the
  volume flag only means files *may* be protected, not that all are.
- **xattr protection**: The FUSE adapter blocks `setxattr`/`removexattr` on
  all system-managed attribute names (including `com.apple.system.cprotect`)
  and filters `com.apple.decmpfs` and `com.apple.system.cprotect` from
  `getxattr`/`listxattr` responses, so protected metadata is never exposed as
  ordinary data.

### Apple's read-path ordering

In Apple's `hfs_readwrite.c::hfs_vnop_read()`, the read path checks in this
order:

1. **Compression** — `hfs_file_is_compressed()` checks `UF_COMPRESSED` in
   `c_bsdflags`. If compressed, `decmpfs_read_compressed()` handles the
   entire read and returns before any content-protection logic runs.
2. **Content protection** — `cp_handle_vnop(vp, CP_READ_ACCESS, ...)`
   gates access against the system's unlocked state. If the protection class
   is restricted and the device is locked, the read is denied.
3. **Raw ciphertext path** — when a file is opened with the `FENCRYPTED`
   flag (`0x02000000` in file descriptor flags, set via `FOPEN_RAW_ENCRYPTED`
   or `open(..., O_ENCRYPTED)`), the `IO_ENCRYPTED` I/O flag is set. This
   bypasses key restoration, skips content-protection checks, and reads the
   **raw encrypted bytes** from disk. Key rolling (`hfs_key_roll_up_to`)
   may be performed for the file's encryption key.

Files without a `cprotect` entry (`c_cpentry == NULL`) pass through
`cp_handle_vnop` without restriction and are read normally.

## 3. Native HFS+ per-file encryption (not implemented — future milestone)

Full support for content-protected files — including key management, key
unlocking via the system keychain, and transparent decryption of data forks
and resource forks — is a **separate research and implementation milestone**.

The Apple source for this lives in:

- `core/hfs_cprotect.c` — content protection access checks, key class
  resolution, lock state management
- `core/hfs_cprotect.h` — `CONTENT_PROTECTION_XATTR_NAME`, `struct cprotect`,
  protection class constants
- `core/hfs_vnops.c` — `hfs_vnop_open` sets the `FENCRYPTED` flag;
  `hfs_file_is_compressed` handles the compressed-vs-non-compressed
  distinction
- `core/hfs_readwrite.c` — `hfs_vnop_read` read path with CP gating
- `core/hfs_format.h` — `kHFSContentProtectionMask`

### What this crate would need to implement

1. **Detect** the `com.apple.system.cprotect` xattr on a file during
   `lookup_cnid` / `read`, returning a structured error if present.
2. **Parse** the `cprotect` attribute header to determine the protection
   class (e.g. `CP_PCLASS_COMPLETE`, `CP_PCLASS_NO protection`).
3. **Key management** — interface with the system keychain
   (`/private/var/db/keybags`) to retrieve and unwrap per-file DEK (data
   encryption key). This is the macOS-specific integration that cannot be
   replicated in a userspace filesystem library without external key
   provisioning.
4. **Transparent decryption** — decrypt the data fork and resource fork
   contents on read, and encrypt on write, using the file's key.

Until decryption support is implemented (milestone 3+ above), content-protected
files produce an `Error::unsupported` rather than returning ciphertext. This
detection is now in place.

## Interaction with compression

There is **no mutual exclusion** between `com.apple.decmpfs` and
`com.apple.system.cprotect` in Apple's source. Both are stored as regular
HFS+ extended attributes. A file may be both compressed and content-protected.

The read-path ordering (compression checked before content protection) means:

- A compressed + encrypted file is decompressed first (if unlocked), then
  decrypted.
- A compressed file opened with `FENCRYPTED` returns raw compressed bytes
  (the `decmpfs` path is checked by `hfs_file_is_compressed`, not by
  encryption flags, so the raw-read bypass still applies).

This crate's `try_decompress()` in `src/volume/mod.rs` now checks for the
`com.apple.system.cprotect` attribute **before** attempting decompression, so
a compressed + encrypted file will produce `Error::unsupported` rather than a
cryptic decompression failure. This prevents the decompression path from being
entered with ciphertext-compressed data.
