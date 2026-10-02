# Licensing

This repository contains code under **two** licenses. The split is tracked at
file granularity and is also recorded in every derived file's module
documentation.

## 1. BSD-2-Clause — original code

`LICENSE` (BSD-2-Clause) covers original glue code, tooling, tests and the
test harness: crate scaffolding, the block-device abstraction, image
generation scripts, the manifest format, the FUSE adapter, and the build and
CI plumbing.

## 2. APSL-1.2 — code derived from Apple's HFS implementation

Apple's HFS source (<https://github.com/pgiffuni/apple-hfs>, commit
`d1bac2f062e6e9c0dfcce302d9aacb10173d0eea`) is distributed under the Apple
Public Source License version 1.2, reproduced verbatim as
`LICENSE-APPLE.md`. Files in this repository that translate, or that reproduce
the layout of, Apple's on-disk structures and algorithms remain under APSL-1.2
and **must not** be relicensed as BSD-2-Clause merely because they have been
rewritten in Rust.

The module-level `//!` documentation of every derived file names the Apple
source file and function that was mined, so the provenance of each translation
is greppable:

```
$ rg -n 'Mining reference: Apple' src/
```

Currently derived files:

| File | Apple reference |
| --- | --- |
| `src/format/volume_header.rs` | `core/hfs_format.h` (`struct HFSPlusVolumeHeader`), `core/hfs_vfsutils.c` (`hfs_ValidateHFSPlusVolumeHeader`) |
| `src/format/fork.rs` | `core/hfs_format.h` (`struct HFSPlusForkData`, `struct HFSPlusExtentDescriptor`) |
| `src/format/extents.rs` | `core/hfs_format.h` (`struct HfsPlusExtentRecord`) |
| `src/timestamp.rs` | `core/MacOSStubs.c` (`to_bsd_time`, `to_hfs_time`), `core/hfs_format.h` (`kHFSExpandedTimesBit`) |
| `src/endian/mod.rs` | `core/hfs_endian.c`, `core/hfs_endian.h` |

## 3. Rules

- **Never** remove or alter Apple copyright, license or attribution notices
  from derived files.
- **Never** introduce GPL-licensed code into this repository. This rules out
  direct translation of `0x09/hfsfuse`, which is GPL-2.0; that project is used
  only as an external behavioural reference and is never vendored here.
- Any new third-party dependency (compression codec, Unicode table, etc.) must
  have its licence verified as compatible **before** it becomes mandatory.
  Optional, licence-clean implementations are preferred.

## 4. Installed reference tooling

Reference implementations are used by **invoking their binaries as external
processes**. No GPL source is read, copied, vendored, linked, or translated.

Two distinct uses are permitted, and they carry different obligations:

| Use | Permitted | Notes |
| --- | --- | --- |
| Running a GPL **executable** to produce a test image | Yes | Execution is not distribution. Output is a data file, not GPL code. The corpus may therefore be built with `hformat` or any other installed formatter if that is the only way to obtain a needed image. |
| Reading, copying, translating or linking GPL **source** | **No** | Not as a specification, not as a port, not as vendored code. |

| Package / project | Licence | Role |
| --- | --- | --- |
| `hfsprogs` 540.1 (`mkfs.hfsplus`, `fsck.hfsplus`) | APSL-2.0, Apple Inc. | **Primary.** Image generation and the independent checker for differential testing. Apple-authored, so it also satisfies the Apple-source-first hierarchy. |
| `pgiffuni/apple-hfs` | APSL-1.2 | **Primary.** Structures, algorithms, semantics. Mined and cited. |
| `hfsutils` 3.2.6 (`hmount`, `hls`, `hformat`, …) | GPL-2.0 | **Executable only.** May be run to build images. Never read as a reference; never vendored. |
| `hfsplus` 1.0.4 (`hpmount`, `hpls`, libhfsp) | GPL-2.0 | **Executable only.** Same rule. |
| `0x09/hfsfuse` | GPL-2.0 | **Behavioural baseline only.** Defines the read-only feature set to meet or exceed. Never copied, translated, linked, or vendored. |

This is recorded because it is easy to lose: both `hfsutils` and `hfsplus` are
installed, both look like obvious HFS+ tooling, and both are GPL-2.0. Neither may
be consulted as a source of truth. When an on-disk question arises the order of
authority is Apple's `core/` sources, then Apple's format documentation, then
`hfsprogs`, which is itself Apple code.

### Consequence for differential testing

Only Apple-derived tooling is used to decide whether our output is *correct*:

```
   mkfs.hfsplus (hfsprogs, Apple)          our Rust implementation
            |                                       |
            v                                       v
        image A  --->  modify  --->  image B  --->  fsck.hfsplus  --->  pass?
```

`fsck.hfsplus` is the arbiter. A GPL tool is never the judge of correctness,
because a disagreement with it is not evidence of a bug on our side.


`LICENSE-README.md` records this because it is easy to lose: both `hfsutils`
and `hfsplus` are installed on the development machine, both look like obvious
HFS+ tooling, and both are GPL-2.0. Neither may be consulted as a source of
truth. When an on-disk question arises, the order of authority is Apple's
`core/` sources, then Apple's format documentation, then `hfsprogs`, which is
itself Apple code.

