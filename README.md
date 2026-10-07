# HFS+ — Rust implementation

A pure-Rust, read-oriented implementation of the HFS+ filesystem,
with the first steps toward a writable, journaled filesystem exposed
through FUSE.

## Status

| Area | Status |
|---|---|
| HFS+ / HFSX on-disk parsing | Complete |
| Catalog B-tree | Complete |
| Extent mapping and SEEK_DATA/SEEK_HOLE | Complete |
| Allocation bitmap | Complete |
| Extended attributes | Complete |
| Resource forks | Complete |
| Journal replay (crash recovery) | Complete |
| Journal transaction assembly | Partial — see [roadmap](#roadmap) |
| decmpfs compression (zlib) | Complete |
| decmpfs compression (LZVN, LZFSE) | Incomplete (tracked) |
| Writable volume (creation) | Partial |
| FUSE adapter | Not started |

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

The library is `#![deny(unsafe_code)]` throughout. Disk images are treated as
untrusted input: every field, offset, and count is bounds-checked.

## Design rules

- **Mining provenance** — every item derived from Apple carries a rustdoc
  comment explaining its on-disk layout and invariants. The `.c`/`.h` origin
  lives in [`docs/source-map.md`](docs/source-map.md), one entry per item.
- **No kernel scaffolding** — Apple's source is XNU kernel code; locking,
  `vnode`, `buf_meta_t`, and `vfs_context` are not translated.
- **Structured errors** — malformed input yields `Error`, never panics.

## Reference sources

In authority order:

1. [Apple HFS source](https://github.com/apple-oss-distributions/hfs),
   commit `d1bac2f062e6e9c0dfcce302d9aacb10173d0eea`
2. Apple HFS documentation
3. Apple's own tests in that repository (`tests/cases/`, `lib_fsck_hfs/`)
4. `0x09/hfsfuse` — compatibility baseline only (GPL-2.0, never copied here)
5. `hfsprogs` 540.1 (`mkfs.hfsplus`, `fsck.hfsplus`) — used as an external
   process for image generation and correctness arbitration

## Testing

```sh
cargo test                                  # full suite
cargo +nightly clippy --all-targets -- -D warnings
cargo fmt -- --check
```

The image corpus (10 good images, 15 malformed images, one per image manifest)
is committed to `tests/images/`. See `tests/corpus_completeness.rs` for the
reproducibility check.

## Documentation

- [`docs/hfs-format.md`](docs/hfs-format.md) — on-disk format and source
  disagreements
- [`docs/source-map.md`](docs/source-map.md) — Apple-to-Rust provenance map
- [`docs/mutation-invariants.md`](docs/mutation-invariants.md) — rules that
  mutations must preserve
- [`docs/dev-tools.md`](docs/dev-tools.md) — tools and their capabilities

## Roadmap

See [docs/roadmap.md](docs/roadmap.md) — a living document that tracks progress
against the project brief.

## License

New code: BSD-2-Clause. Derived Apple code: APSL-1.2. See
[`LICENSE-README.md`](LICENSE-README.md) for details.