# HFSPfuse — a userspace HFS+/HFSX filesystem in Rust

A read/write implementation of Apple's HFS+ (Mac OS Extended) and HFSX
journaling file systems, structured as a library with FUSE and command-line
adapters. All on-disk layout and algorithm logic is derived from Apple's own
`hfs` source tree and verified against `fsck.hfsplus`.

## Binaries

| Binary | What it does |
| --- | --- |
| `hnewfs` | Formats a file or block device as HFS+ or HFSX. Supports volume naming, block and node sizing, case-sensitive (HFSX) volumes, and journaled volumes (`-J`). |
| `hfsck` | Read-only consistency checker (`fsck_hfs` equivalent). Validates the catalog B-tree, extents overflow, allocation bitmap, extended attributes, and journal. |
| `hfsls` | Lists an image's contents (`hls` equivalent) without mounting it. Recursive (`-R`) and JSON output. |
| `hfsinspect` | Prints volume-header facts, B-tree summaries, and raw node dumps for a given image. |

## Library structure

The crate compiles as `hfsplus`. The FUSE adapter is a separate crate that
depends on the library.

```
src/
├── lib.rs              crate root, re-exports the public API
├── blockdev/           block-device abstraction (file, memory, view layers)
├── endian/             checked big-endian accessors — no `unsafe` over image bytes
├── btree/              generic B-tree node I/O and key comparison
├── catalog/            catalog B-tree keys, records, CNIDs
├── attributes/         extended-attributes B-tree
├── extent/             extent mapping (logical → physical block)
├── format/             on-disk structure definitions and the `hnewfs` writer
├── journal/            journal info block, checksum, and read-only replay
├── volume/             read-only mounted-volume view
├── alloc/              allocation bitmap search and block claims
├── file/               file extent mapping
├── unicode/            name comparison and case-folding tables
├── compression/        decmpfs LZFSE/LZVN/LZ4/zlib decoders
├── error.rs            structured error types (never panics on malformed input)
└── check/              consistency-check helpers used by `hfsck`

src/bin/
├── hnewfs.rs
├── hfsck.rs
├── hfsls.rs
└── hfsinspect.rs
```

## Status

The project follows a milestone roadmap documented in `docs/roadmap.md`.
Recent milestones include write support (overwrite, grow, truncate, create,
rename, remove), journaling, resource forks, hard links, and extended
attributes — all `fsck.hfsplus`-verified.

## Documentation

- `docs/hfs-format.md` — on-disk format, including known disagreements with
  secondary sources
- `docs/source-map.md` — where each translated structure comes from in Apple's
  source tree
- `docs/mutation-invariants.md` — rules every write must preserve
- `docs/dev-tools.md` — external tools used for image generation and checking
- `LICENSE-README.md` — licensing details

## Building

```sh
cargo build
cargo test                          # all tests (corpus included)
cargo +nightly clippy --all-targets -- -D warnings
cargo fmt -- --check
```

The clippy leg is nightly on purpose; see `AGENTS.md` for why.
