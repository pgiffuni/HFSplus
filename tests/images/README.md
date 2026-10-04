# HFS+ / HFSX test corpus

Images are **committed**, at 1 MiB each -- 2 MiB for the 16 KiB-block volume,
which needs more blocks to cover the same metadata. They were not committed
before, because they were 32 MiB each and that is not something to put in Git.

They are now committed so that the suite runs anywhere. The previous policy
meant `cargo test` required `hfsprogs` to be installed, which is a reasonable
assumption on the machine that developed this and a false one anywhere else --
a fresh FreeBSD box being the case that matters. A committed corpus makes the
tests portable; the cost is 31 MB, and the images stay sparse on disk so they
cost far less than that in a working tree.

Regenerating is still how you change one, and `tests/corpus_completeness.rs`
checks that the derived ones regenerate byte-identically -- so a committed image
and a regenerated one cannot quietly diverge.

## Layout

```
tests/images/
    README.md        this file
    manifests/       one TOML per image: the test specification (committed)
    generated/       built by tools/genimages.sh   (committed)
    malformed/       built by tools/genmalformed.sh (committed)
    public/          externally sourced images     (gitignored, see below)
    macos/           built on macOS with real Apple metadata (gitignored)
    expected/        expected tree listings and hashes for comparison (committed)
```

## Reproducing everything

```sh
tools/genimages.sh          # 9 good images + 3 with real journal transactions
tools/genmanifests.sh       # one manifest per image, from ground truth
tools/genmalformed.sh       # 11 deliberately corrupted images
cargo test                  # 293 tests
```

`genimages.sh` skips images that already exist; pass `--force` to rebuild. The
manifests are always regenerated.

## Why the manifests are generated, not hand-written

`tools/genmanifests.sh` reads each image's volume header and runs
`fsck.hfsplus` on it, then writes both facts into the manifest. That ordering
matters: the manifest is captured **before** anyone consults the Rust parser, so
a parser bug cannot end up redefining what the parser is supposed to produce.
`tests/volume_header_conformance.rs` then compares the parser against the
recorded expectations.

Only the descriptive fields — `[source]`, `[features]`, `[expect]`, `[[files]]`
— are ever edited by hand. `[volume]`, `[forks]` and `[verify]` are not.

## Manifest format

TOML. One file per image in `manifests/`.

```toml
name = "basic-hfsplus"
filesystem = "HFS+"          # HFS+ | HFSX | HFS
journaled = false
case_sensitive = false

[source]
type = "generated"           # generated | public | macos
tool = "mkfs.hfsplus"
tool_version = "540.1.linux3"
tool_licence = "APSL-2.0"
image = "tests/images/generated/basic-hfsplus.img"
sha256_informational = "..." # provenance only; see "Determinism"

[volume]                     # transcribed from the header, do not hand-edit
block_size = 4096
total_blocks = 8192
free_blocks = 7997
volume_bytes = 33554432
next_catalog_id = 16
file_count = 0
folder_count = 0
attributes = "0x80000100"
unmounted = true
journal_info_block = 0
expanded_times = false

[forks.catalogFile]          # one table per special file
logical_size = 262144
total_blocks = 64

[features]
catalog_btree = true
extents_overflow_btree = true
attributes_btree = true
journal = false
unicode = true
hard_links = false
directory_hard_links = false
resource_forks = false
xattrs = false
finder_info = false
compression = false
sparse_files = false

[expect]
outcome = "mount-readonly"   # mount | mount-readonly | reject-cleanly
expected_error = ""          # substring the error must contain when rejecting

[verify]
checker = "fsck.hfsplus"
checker_version = "540.1.linux3-6build1"
verdict = "OK"
checker_output_contains = "appears to be OK"

[[files]]                    # per-file expectations for image-content tests
path = "hello.txt"
sha256 = "..."
```

## Determinism

HFS+ volume headers embed creation, modification and last-check timestamps, so
regenerating on a different day changes a handful of bytes and the SHA-256.
Consequences, applied consistently:

- `sha256_informational` is provenance, not a fixture identity.
- No test compares a committed image byte-for-byte.
- Tests that need byte-exact behaviour regenerate into a temp directory.
- When a byte-exact image genuinely matters, fix the generator's clock, not a
  digest in a test.

Verified: deleting `generated/`, rebuilding with `tools/genimages.sh`, and
regenerating manifests changes **only** the `sha256_informational` line of each
manifest. Every field a test asserts on — signature, version, `blockSize`,
`totalBlocks`, `freeBlocks`, `nextCatalogID`, the five forks, and the
`fsck.hfsplus` verdict — is identical across runs. Timestamps that are genuinely
constant are therefore asserted exactly, which is what makes the conformance
suite meaningful.

## Coverage now

Nine generated images, all accepted by `fsck.hfsplus`:

| Image | Exercises |
| --- | --- |
| `basic-hfsplus` | baseline HFS+, 4096-byte blocks |
| `basic-hfsplus-1k` | 1024-byte allocation blocks (below B-tree node size) |
| `basic-hfsplus-8k` | 8192-byte allocation blocks |
| `basic-hfsplus-16k` | 16384-byte allocation blocks |
| `hfsx-case-sensitive` | HFSX, signature `0x4858`, version 5 |
| `hfsx-case-insensitive` | HFS+ signature, case-insensitive comparison |
| `journaled-hfsplus` | journaled HFS+, `journalInfoBlock` set |
| `journaled-hfsplus-1k` | journaled HFS+ with 1024-byte blocks |
| `classic-hfs` | classic HFS, signature `0x4244`: must be recognised and refused |

### Journal replay

Every image `mkfs.hfsplus -J` produces has `kJIJournalNeedInitMask` set and a
zeroed journal header, because no transaction has ever been written. So the
generated corpus proves journal *detection* but cannot exercise *replay* at all:
there is nothing in the journal.

`tools/makejournal.py` closes that gap. It writes a real journal header, a real
transaction, a real block list and real replacement data into a journaled image,
following Apple `core/hfs_journal.c`'s layout:

```text
journal + 0                 journal_header    (jhdr_size bytes)
journal + jhdr_size         block_list_header  (blhdr_size bytes)
journal + jhdr_size + blhdr_size   the replacement block data
```

Three images are produced, so the reader's byte-order detection and the geometry
are both exercised:

| Image | Exercises |
| --- | --- |
| `journal-replay-be` | big-endian journal header |
| `journal-replay-le` | little-endian journal header, as an x86 or ARM host writes |
| `journal-replay-1k` | a 1 KiB volume, so a different block geometry |
| `journal-replay-multi` | three transactions, two rewriting the same block |

The multi image is the one that matters most: a single-transaction journal cannot
show whether the walk crosses transaction boundaries, whether a later transaction
supersedes an earlier write to the same block, or whether replay truncates at
damage. Writing it is what found the `binfo[0]` sequence-slot bug described in
`docs/hfs-format.md`.

Each of those rewrites a block the filesystem does not currently reference. That
is deliberate, and it has a consequence worth stating plainly: they verify the
overlay for **precedence** (a replayed block reads from the journal, and differs
from the device), for **non-interference** (an untouched block still reads from
the device, and the volume still mounts through the overlay) and for **refusal**
(a block failing its recorded checksum stops the replay). They do **not** verify
that replay *repairs* anything, because a block nothing references repairs
nothing.

## Torn metadata (`tests/images/replayed/journal-torn-catalog.img`)

Produced by `tools/mktorn.py`, and the only image here that tests the reason a
journal exists.

It writes a real catalog change — a file record and its thread record — into a
journal transaction and leaves the on-disk catalog alone. The result is a volume
in the state a machine that lost power mid-write leaves behind: the filesystem is
internally consistent but older than its journal.

That gives the suite a property nothing else can have. Read straight off the
image the root folder has two entries; read through the replay overlay it has
three. `fsck.hfsplus` accepts the image, because the on-disk filesystem really is
sound — a file that exists only in the journal is not a disk defect. So the
image is crash-consistent rather than corrupt, and recovering it is the only way
to see the file.

The test asserts the two listings *differ*, which is the negative control that
keeps the rest of the suite from being vacuous. It also checks the recovered file
resolves by name **and** by CNID, that `nextCatalogID` advanced past it, that a
second recovery gives the same answer, and that the image is byte-identical
afterwards. See `docs/hfs-format.md` for what writing the tool uncovered.

## Files with data (`tests/images/generated/journal-with-files.img`)

Produced by `tools/mkfiles.py` from `journaled-hfsplus`, and the only image in
the corpus with a file that has content.

`fragmented.bin` is eight single-block extents at physically scattered blocks,
each block beginning with its own block number. `link` is a symbolic link whose
target lives in its data fork. Records are cloned from the volume's own, and the
allocation bitmap, file count, free block count and root valence are all updated,
so `fsck.hfsplus` accepts the image and leaves it byte-identical — the check that
the generator wrote something coherent rather than merely self-consistent.

A sparse file is deliberately **not** here: see the limitations list below.

## A journal on another device (`tests/images/replayed/journal-external.img`)

Produced by `tools/makejournal.py --external-journal`. A journal may live on its
own partition, named by `ext_jnl_uuid` — which is what a Time Machine volume has
— and `mkfs.hfsplus` cannot create that, so the state is written by hand.

Two things make it worth having. The reader has a branch for it that no other
image reached, and that branch was wrong: it validated the journal's `offset` as
non-zero, but `offset` is read only when the journal is *inside* the filesystem,
so a correct external-journal volume was refused with a complaint about a field
that means nothing there. And `hfsls -j` printed `journal: none` for a volume
whose own header says `journaled: true`, which is the opposite of the truth.

`fsck.hfsplus` accepts it and leaves it byte-identical.

Thirteen malformed images, none of which may mount.

Two of them point `journalInfoBlock` outside the volume — at `totalBlocks`, and at
a value large enough to overflow the block offset. `fsck.hfsplus` accepts both: it
does not range-check that field either, so this is a place where the two
implementations disagree and this one is stricter. The rationale is that the info
block is part of the volume by definition, so a pointer outside it means the
header is wrong — and without the bound the resulting refusal is a coincidence of
whatever bytes happen to sit there.

## Not yet covered

These need a macOS host or externally sourced material. The directories exist;
the recipes do not yet.

### Content and naming (`macos/`)

Requires real Apple metadata that `mkfs.hfsplus` cannot produce:

- composed vs decomposed Unicode names, and names that differ only by
  normalisation
- very long names, punctuation, leading-dot names
- resource forks, FinderInfo, user xattrs
- compressed files (zlib, LZVN, LZFSE via `decmpfs`)
- hard links, and Time Machine-style directory hard links
- multi-extent files spilling past the inline eight -- covered
  synthetically in `tests/extents_overflow.rs`, but no image exercises it through
  `Volume::read`
- sparse files: not merely absent but *impossible* in an HFS+ data fork.
  `fsck.hfsplus` rejects both encodings -- an extent with `startBlock == 0` and
  a non-zero count, and a `logicalSize` beyond the allocated blocks -- and
  `MapFileBlockC` has no zero-fill path for either. A zero-start descriptor is the
  attributes file's gap marker. So there is no such fixture to add
- a directory with many entries

### Third-party images (`public/`)

Searched for, not yet adopted. Anything added must record source, URL, licence,
checksum, filesystem type, known contents, and whether redistribution is
permitted. Images whose licence is unclear stay out of the repository: a
manifest plus an acquisition script is committed instead.

Candidates worth evaluating: digital-forensics datasets, filesystem test
collections, academic filesystem corpora, open-source filesystem test suites.

### Still to be built locally

- populated files and directories, once the write engine exists
- multi-extent and fragmented files, which need enough data to overflow the
  eight inline extent descriptors
- corrupted B-tree nodes, catalog records and extent records — the malformed set
  currently corrupts the volume header and fork geometry only

## A warning about `fsck.hfsplus`

**`fsck.hfsplus` modifies the image it checks.** Given a volume whose primary
header it dislikes, it restores that header from the backup copy 1024 bytes
before the end of the volume and rewrites `checkedDate`. An image with a
deliberately destroyed signature came back byte-identical to the pristine
original.

So: never point it at a fixture. Copy first. It is also a *repair* tool, not a
conformance oracle — "appears to be OK" does not mean a mount would accept the
volume. `tests/malformed_safety.rs::the_checker_repairs_rather_than_refuses` and
`tests/volume_header_conformance.rs::the_checker_does_not_mutate_its_input` keep
this finding executable rather than merely documented.

## Licensing

Images built with `mkfs.hfsplus` (`hfsprogs`, APSL-2.0, Apple) carry no
copyleft obligation. GPL tools may be *run* to build an image if some future case
requires one, but no GPL source is read or vendored. See `LICENSE-README.md`.
