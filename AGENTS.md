# Working in this repository

A userspace HFS+/HFSX filesystem implementation in Rust, initially exposed
through FUSE. The HFS library must never depend on FUSE; the FUSE adapter
depends on the library.

## Reference sources, in authority order

1. **Apple's HFS source**, <https://github.com/apple-oss-distributions/hfs>,
   pinned at `d1bac2f062e6e9c0dfcce302d9aacb10173d0eea`. This is the
   authority. Clone it outside the repository and mine `core/`.
   `pgiffuni/apple-hfs` is a mirror of that repository at the same commit and
   is an acceptable substitute when the canonical one is unreachable; cite
   the canonical URL regardless.
2. Apple HFS documentation and format definitions.
3. Apple's own tests, shipped in the same repository: `tests/cases/` (48 cases),
   `lib_fsck_hfs/`, `fstyp_hfs`, `hfs_util`, `newfs_hfs`.
4. `0x09/hfsfuse` — compatibility **baseline only**. It is GPL-2.0 and defines
   the minimum read-only feature set. Never copy, translate, link, or vendor it.
5. `hfsprogs` 540.1 (`mkfs.hfsplus`, `fsck.hfsplus`) — APSL-2.0, Apple. Used as
   an external process for image generation and as the arbiter of correctness.

## Hard rules

- **Cite the mining source.** Every item derived from Apple states its origin in
  rustdoc: which file, which function, and which on-disk structures are involved.
  Verify with `rg -n 'Mining reference: Apple' src/`.
- **Explain, do not paste.** Describe an Apple algorithm in original words. Never
  paste large source fragments into documentation.
- **No `unsafe` over image bytes.** `#![deny(unsafe_code)]` is set. Use the
  checked accessors in `src/endian/`. Disk images are untrusted input.
- **Never trust an on-disk length.** Bounds-check every field, every offset, every
  count. Malformed input must produce a structured `Error`, never a panic.
- **Do not translate kernel code mechanically.** Apple code is XNU kernel code:
  locking, `vnode`, `buf_meta_t`, `vfs_context` have no meaning in a userspace
  library. Translate the algorithm and the data structure; drop the kernel
  scaffolding.
- **Licences.** New code is BSD-2-Clause. Code derived from Apple stays APSL-1.2
  and keeps its attribution. See `LICENSE-README.md`.
  - Running a GPL *executable* to build a test image is fine.
  - Reading, copying or translating GPL *source* is not. That rules out
    `hfsutils` and `hfsplus`, both GPL-2.0.
  - Verify a dependency's licence before making it mandatory.

## Investigate disagreements

When sources disagree, find out why. Do not pick one and move on. Findings so
far, all recorded in `docs/hfs-format.md`:

- The volume name is **not** in the volume header. It is the root folder's name,
  in the catalog B-tree. `112 + 5 * 80 = 512` gives it away.
- `kHFSVolumeUnmountedBit` is **bit 8**, not bit 15. Bit 15 is the software lock.
- The backup volume header is at `volume_bytes - 1024`, not one allocation block
  from the end.
- Extents overflow is keyed on *allocated* blocks, not logical size, so sparse
  files never consult the extents B-tree.
- `fsck.hfsplus` **modifies** the image it checks, and repairs a bad primary
  header from the backup. It is not a conformance oracle, and it must never be
  pointed at a fixture.

## Tests

```sh
cargo test                                        # everything
cargo +nightly clippy --all-targets -- -D warnings
cargo fmt -- --check
```

Those three are exactly what `.github/workflows/ci.yml` runs, and the last two
have `-D warnings` / `--check` because a warning or a formatting diff fails the
build. Run them before pushing, not after CI tells you.

The clippy leg is **nightly** on purpose: stable clippy lags the lints by a
release, and two key-length constants in this crate were wrong for exactly as
long as the stable lints stayed quiet about them. A newer nightly can therefore
introduce a lint that fails the build on an unrelated commit -- that is the price
of catching things early, and `cargo +nightly clippy --all-targets` locally is
what keeps it from arriving unannounced.

`clippy::pedantic` is deliberately *not* enabled. It wants a `# Errors` section
on every fallible function and `#[must_use]` on nearly everything, which is a
large opinionated diff for lints this codebase has no opinion about.

Images are generated *and committed*. They are small -- 1 MiB each, 2 MiB for the
16 KiB-block volume -- because a suite that needs `hfsprogs` to run is a suite
that only runs where `hfsprogs` is installed, which is not every platform worth
testing on. A committed corpus means `cargo test` works on a fresh FreeBSD box.

```sh
tools/genimages.sh        # 9 good images, each verified with fsck.hfsplus
tools/genmanifests.sh     # one manifest per image, captured from ground truth
tools/genmalformed.sh     # 14 deliberately corrupted images
```

Regenerating is still the way to change them, and the images stay reproducible:
`tests/corpus_completeness.rs` checks every fixture exists and that the derived
ones regenerate byte-identically. It exists because sixty-odd tests skip
silently when a fixture is missing, which would turn a broken recipe into a green
run.

A manifest is the test specification. `[volume]`, `[forks]` and `[verify]` are
generated from the image and from the Apple checker; only `[source]`,
`[features]`, `[expect]` and `[[files]]` are hand-edited.

## Commits

Small and logically separable, matching the milestone progression in the project
brief: `test:`, `format:`, `btree:`, `catalog:`, `unicode:`, `fuse:`, `write:`,
`journal:`, `compression:`. Never mix format parsing, FUSE code and tests in
one commit. Do not commit unless asked.
