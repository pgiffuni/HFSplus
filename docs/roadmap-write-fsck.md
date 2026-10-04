# Roadmap: writing, then our own `mkfs` and `fsck`

Recorded 2026-10-03 as a standing decision, so the ordering and the reasoning
survive past the conversation that produced it.

## The decision

After write support, this project carries its own formatter and its own checker,
so that mounting and inspecting an image needs nothing installed beyond this
crate. Apple's source is the authority for both, as it is for everything else
here.

The motivation is packaging, not capability. Today the corpus depends on
`hfsprogs` being installed, and `hfsprogs` is an *unofficial* port — present in
Debian, not shipped as an Apple package, and carrying the seven checks listed in
`docs/dev-tools.md` that Apple's own `fsck_hfs` performs. Depending on it means
depending on a port whose completeness is not ours to vouch for.

## What already exists

The write side is thinner than the read side, but the shapes are known:

- `VolumeHeader::to_bytes`, `ForkData::write_to` and `ForkData::to_bytes` encode
  single structures.
- The layouts of every structure we write are pinned by `tests/structure_sizes.rs`
  and confirmed against the corpus.
- `tools/mkfiles.py` and `tools/mktorn.py` write real volumes and real journals in
  Python, and both produce images `fsck.hfsplus` accepts unchanged. That is
  almost a prototype of `mkfs`: a volume header, an allocation bitmap, a catalog
  B-tree with a header node and a leaf, an extents B-tree, and a journal.

What does not exist: an allocator, a B-tree *writer* (insert, split, and the node
map), and any notion of a volume being created or checked.

## Milestone 7 — complete

All five workstreams are done to the level the milestone asks for: the model is
established and documented, not merely sketched.

| workstream | state |
| --- | --- |
| 7A fork model | `src/format/fork.rs`, validated, non-sparse rule documented and enforced |
| 7A.1 resource forks | a real catalog fork; the three findings and the four-way split on `Object` |
| 7B attributes file | key, record and reader; `names.rs` for the system's own entries |
| 7B.2 compression metadata | location and hiding established; decoding deliberately absent |
| 7C timestamps | `HfsTimestamp`; epoch, clamp and unset rules documented and tested |
| 7D writable device | `BlockDeviceMut` and `WritableVolume`, with a test that makes each a failure |
| 7E mutation invariants | `docs/mutation-invariants.md` |

Three things the mining overturned, each of which would have been expensive to
discover later:

- **FinderInfo is not in the catalog record.** The 16-byte `HFSPlusBSDInfo` has no
  such field; HFS+ kept FinderInfo in the attributes tree. An assumption carried
  over from classic HFS, and wrong.
- **A resource fork is a real fork, not an attribute.** macOS also surfaces it as
  `com.apple.ResourceFork`, but that name belongs to the POSIX boundary.
- **A compressed file's data fork does not contain its contents**, and its
  resource fork is reported *empty* rather than read. So "empty" and "truncated"
  both have a second explanation that is not corruption.

What is deliberately not built: any mutation. `WritableVolume` establishes that a
volume is safe to change and then stops, so the next person inherits a boundary
rather than an invitation.

## Where this actually stands

More of step 2 and 3 than expected is done, because both turned out to be
reachable read-only:

- `src/alloc/mod.rs` — the allocator: reserve, release, first-fit search from a
  hint with wrap, and the `orphaned`/`missing` comparison.
- `src/check/mod.rs` — four checks, all read-only: allocation bitmap against the
  catalog's extents in both directions, each fork's declared `totalBlocks`
  against what its extents describe (including the volume's own five forks), and
  `nextCatalogID` against the CNIDs in use.
- Catalog structure: key order within a leaf, key length against `maxKeyLength`,
  a thread record for every object, and folder valence.
- B-tree structure, for every tree the volume has: node heights against the tree
  depth, index child pointers, the leaf sibling chain, and the rule that a node
  nothing reaches must be entirely zero.
- `hfsck` — the tool, with a fourth exit status for "read and found
  inconsistent".

Acceptance: 15 clean images must produce an empty report, 11 malformed ones must
be refused or flagged, and **six** deliberate breakages must be flagged by *both*
this checker and `fsck.hfsplus`, each matched on Apple's own wording rather than
ours — valence is the field name in `struct HFSPlusCatalogFolder`, but the
checker says "Invalid directory item count".

What remains before this could be called a checker:

- The B-tree map — one bit per node, MSB first, living in the header node's
  third record and the map nodes after it. `fsck.hfsplus` compares the stored map
  against one it computes, and building this crate's extents tree by hand showed
  how easy it is to get wrong: a map not updated for a new node is
  "Invalid map node".
- Node size consistency, and the `kBTBigKeysMask` / short-key form.
- The multi-linked-files pass, which needs hard-link resolution first — the
  crate surfaces `is_hard_link` and `link_count` but does not follow a chain
  through the attributes tree.

## Order

1. **Write support** (`write:`) — in-place file and directory modification,
   which needs an allocator before anything else can grow.
2. **An allocator** — shared prerequisite for `mkfs` and for a checker, since
   verifying a volume means deciding what is allocated. `AllocationBitmap` is
   currently read-only; it needs reserve, release, and a search from a hint.
3. **A checker** (`fsck:`) — see the independence caveat below before scoping it.
4. **A formatter** (`mkfs:`) — the Python tooling is the specification; the
   version in-tree should be able to reproduce every image in `tests/images/`.

## The constraint that shapes step 3

**The oracle is Apple's source, not the corpus.** What is correct is decided by
`lib_fsck_hfs`, `core/VolumeAllocation.c`, `core/BTreeWrapper.c` and
`core/hfs_format.h`, read directly. A checker written here is right to the extent
that it reproduces the checks Apple's code performs, and each check should name
the function it came from — the same discipline `Mining reference: Apple` already
imposes on the reader.

Separately from that, an in-tree checker and the reader share the parser, so
**an in-tree checker cannot serve as the oracle for the reader's own tests.** It
will agree with the reader about anything both derive from the same code, which
is exactly the class of bug worth testing for: a mis-parsed extent record is
mis-parsed identically by both, and a checker built on the parser cannot notice.

That is not an argument against writing one — a user needs something to run — but
it fixes its role:

| Question | Answered by |
| --- | --- |
| What *should* the checker do? | Apple's source, mined per check |
| Does this implementation agree with an independent one? | the corpus |
| Is the *reader* correct? | an implementation that shares neither |

So the corpus is a fixture source, not an arbiter. What makes it useful is that it
was generated independently -- by `hfsprogs` and by Python scripts that never use
the Rust parser -- so it can contradict the parser at all, and its manifests record
what each image must yield. That independence is worth more than the convenience
of generating fixtures with the code under test, so the Python generators stay.

Once the in-tree checker exists, the corpus becomes its acceptance test: ten good
images it must accept, and eleven malformed ones it must reject, each for the
specific reason its manifest records. That is a genuine test of the tooling --
and it is a test of *agreement with an independent implementation*, not a proof
of correctness. Correctness comes from the mining.

Two corollaries:

- The checker's coverage should be stated, as `docs/dev-tools.md` now does for the
  port. A checker that silently omits a check is worse than one that says so.
- Where the in-tree checker replaces `fsck.hfsplus` in a test, the test's claim
  weakens: it becomes a consistency check rather than an independent judgement.
  Tests should say which they are.

## A guard on the guards

Sixty-odd tests open with `if !path.exists() { return; }`, because a developer may
not have run the generators. That is a reasonable convenience and a serious hole
in a suite that exists to prove things: a broken recipe would leave every
dependent test skipping, the run green, and the coverage silently gone. Absence
reads as success.

The guards stay and `tests/corpus_completeness.rs` asserts they have nothing to
skip -- naming every fixture the suite depends on, so a missing one fails there by
name rather than passing everywhere else. It also checks that the fixtures carry
the signature they should, which catches a generator writing the right filename
and the wrong bytes, and that the derived fixtures regenerate byte-identically.

The same discipline the checker needed, for the same reason: a check that fires
when it should not trains you to ignore it, and a test that skips when it should
not is worse than no test.

## The one gap no checker here can close

Producing a genuinely *crash-consistent* image — a volume whose journal is newer
than its filesystem because the machine died mid-write — needs either macOS or
fault injection. `tools/mktorn.py` writes the same on-disk state directly, which
covers what replay must do, but not the fact that a real crash produces it.

# Superseded

This was the plan while the milestone was "make a checker". That is done, and the
roadmap has moved on. See:

- `docs/source-map.md` — where every translated item came from, and what is not
  yet mined
- `docs/mutation-invariants.md` — the rules a mutation must preserve, written
  down before any mutation exists

The framing that carried over unchanged: the oracle is Apple's source, and an
external checker is evidence of agreement rather than a source of truth.
