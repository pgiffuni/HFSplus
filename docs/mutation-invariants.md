# Mutation invariants

The rules a mutation must preserve. Written down *before* any mutation exists, so
that the first one is built against them rather than discovering them.

This is Milestone 7E of the roadmap. It is a document and a set of assertions
that already exist, not new behaviour: every invariant here is already checked by
`hfsck`, and the work of 7E was to state them and prove the coverage rather than
to add checks.

# Why before

Three reasons, and the third is the one that bites.

**A writer will satisfy them by accident, once.** The read path never has to
maintain `logicalSize <= totalBlocks * blockSize`, because nothing changes it.
A writer touches both fields in different places, and the invariant is exactly
what falls between them.

**The tempting shortcut is to weaken the checker.** When a mutation cannot
maintain an invariant, the fast move is to relax the check that notices. That
trades a hard failure for a silent one and leaves the corpus tests agreeing with
whatever the mutation happens to do — the checkers would then be validating the
mutation's assumptions rather than the format.

**The independent checker is the only witness that matters.** If this crate's
checker and this crate's mutation share a mistaken assumption, they will agree
with each other and both be wrong. `fsck.hfsplus` is a port of Apple's code and
carries its own assumptions, so agreement is evidence — not proof, and not
substitute for reading Apple.

# The invariants

## Forks

```text
logical_size  <=  total_blocks * allocation_block_size
total_blocks  ==  sum(block_count for every extent, inline and overflow)
```

The first is the **non-sparse** invariant, and it is the one this crate is most
likely to violate. An HFS+ data fork has no representation for a hole. A
zero-start extent descriptor is the *attributes* file's gap marker, and
`MapFileBlockC` has no zero-fill path — it returns whatever
`SearchExtentFile` finds and errors if it finds nothing. So `logical_size`
beyond the blocks behind it is a corrupt record, not a sparse file, and a reader
that zero-fills it is inventing bytes rather than recovering them.

A logical size *below* the physical size is ordinary: a file's last block is
normally only partly used. These are inequalities, not equalities.

Enforced by `ForkData::validate`, from
`lib_fsck_hfs/dfalib/CatalogCheck.c` `CheckFileData`.

## Extents

```text
inline_extents  ++  overflow_extents  ==  the fork's complete extent sequence
overflow_key.startBlock  ==  blocks already described by preceding groups
```

The key is the running count, not a group index: for a fork with eight inline
extents the second group is keyed 8, the third 16. Getting that wrong is the
classic bug in this area, and it makes the second group unreachable.

Enforced by `src/extent/mapper.rs` and by the extents-tree fixture that fills
`tests/images/generated/journal-with-files.img`'s overflow tree deliberately.

## Allocation

```text
for every allocated block:   the bitmap bit is set
for every referenced block:  the bitmap bit is set
free_blocks                  ==  free allocation blocks below allocLimit
```

Both directions. The bitmap is the only structure that says a block is in use, so
a fork whose extents were written without updating the bitmap is a file that
reads correctly and belongs to nothing.

Enforced by `check::check`, cross-read from
`lib_fsck_hfs/dfalib/VolumeBitmapCheck.c` and `SVerify1.c` `CheckBitmapRange`.

Two regions are never allocated: the reserved prefix below the allocation file's
first extent, and the tail above `allocLimit`, where the backup volume header
lives. Neither size is a fixed number of blocks — the prefix depends on the
volume's block size, and `allocLimit` depends on where the filesystem ends
inside its partition.

## B-trees

```text
node_map            ==  the nodes reachable from the root
key order           ==  strictly increasing within every leaf
key_length          <=  maxKeyLength, and the key fits its record
free space          ==  between the lowest record and the offset array
unused nodes        ==  entirely zero
```

The node map is the one that will bite a writer. Adding a leaf in place changes
what is reachable without touching the map, and the result is a tree that reads
correctly and disagrees with itself about which nodes it owns. `fsck.hfsplus`
reports `E_BadMapN`; so does `hfsck`, and
`tests/images/replayed/stale-node-map.img` is the fixture.

Enforced by `check::check`, cross-read from `lib_fsck_hfs/dfalib/SVerify2.c`
`BTCheck`, `CmpBTreeMap`, `BTMapChk` and `BTCheckUnusedNodes`.

## Catalog

```text
one key, one record identity:  a record's CNID equals its key's parentID
every object has a thread record
every thread record's parentID equals a live folder's CNID
valence                      ==  the number of thread records naming that folder
nextCatalogID                >   every CNID in use
```

The thread records are what make an object reachable by name, so a broken one is
invisible damage rather than visible damage. `nextCatalogID` failing to advance
is worse still: the next created file is handed a CNID that already identifies an
existing one, and a lookup by CNID then finds the wrong file.

## Volume header

```text
free_blocks ==  the volume's actual free allocation blocks
journalInfoBlock names a block of this volume, when the volume is journaled
```

The header's counts are recomputed by Apple's checker rather than trusted, so a
mutation cannot leave them stale.

## Journal

Carried over from Milestone 5, and unchanged:

```text
clean            ==  start == end
bnum             in jhdr_size units, not the volume's block size
the journal wraps at its end
the reserved prefix depends on the volume's block size
replay happens before any stale filesystem state is exposed
a journal that cannot be replayed refuses to mount
```

## The rule that covers all of them

```text
mutate -> serialise -> reopen -> hfsck -> fsck.hfsplus
```

Every mutation operation, at every level, must survive that round trip. The three
levels are not alternatives:

1. **Structure** — the serialized bytes are what you intended.
2. **Round trip** — reopening yields the native state you intended.
3. **Independent** — `fsck.hfsplus`, where it has the check, accepts it.

A mutation passing only the first has written bytes. Passing the first two has
written a filesystem. Passing all three has written one that something derived
from Apple's own code agrees with.

# Coverage, as of this writing

One mutation exists: `WritableVolume::write_file_contents`, replacing or growing
a file's contents in place.

| Invariant | Holds because | Tested by |
| --- | --- | --- |
| Structure | `FileRecord::write_to` is the inverse of `parse`, so a record read and written back is the same bytes | `a_file_record_round_trips_through_the_same_bytes` -- byte equality against the input, not a re-parse |
| Round trip | the data blocks and the record are written in the order that never exposes a torn file: blocks first, then the length that points at them | `writing_shorter_contents_keeps_the_volume_readable`, `writing_the_same_number_of_bytes_keeps_the_allocation`, `writing_an_empty_file_leaves_no_trailing_bytes`, `a_partial_trailing_block_is_read_back_as_the_tail_and_not_beyond` |
| Independent | `fsck.hfsplus` accepts every written image | `assert_fsck_clean` in `tests/write.rs`, on every write above |

When a write *allocates*, the bitmap and the volume header's `freeBlocks` have to
agree with each other and with the catalog's extents — three places, one fact.
`growing_a_file_allocates_exactly_the_blocks_it_needs` asserts all three moved by
one, and then runs this crate's own checker, which is a genuinely different check
from `fsck.hfsplus`: it compares the bitmap against every extent the catalog
describes, so it sees a block marked allocated with nothing pointing at it, or an
extent pointing at a block the bitmap calls free. A writer that updated the bitmap
and the record in the wrong order passes `fsck` — which repairs orphans — and
fails there.

That check has teeth, which was verified rather than assumed: marking one block
allocated with nothing else changed makes it report that block as orphaned. A
checker that returned "clean" unconditionally would make the assertion above
worthless.

`truncate_file` closes the gap the previous revision listed as missing, and its
own property is the round trip: grow a file, then empty it, and the volume must
return to exactly what it was — one block *fewer* than before, because the file's
original block was released too. Coming back to the original count would mean that
block had leaked, which is the specific failure an allocator and a deallocator that
disagree would produce, and which nothing above would notice until the volume
filled.

What is *not* yet true of any mutation here:

- **No structural change.** The record is replaced only at its original length,
  because a length change moves every later record in the node. No B-tree node
  has been created, split or deleted, so the extent mapper and the node
  descriptor are exercised as readers only. Growth stays inside the eight inline
  extent slots for the same reason: the record's size cannot change.
- **No journal.** `WritableVolume::open` refuses a journaled volume, so every
  mutation above runs on a volume with no journal. The checker therefore never
  has to reason about a transaction it did not write.
- **No creation.** Every mutation here needs a CNID that already exists.
- **Partial blocks are not reclaimable.** A file whose length uses part of its
  last block cannot give that block back by truncation, because the new size
  rounds *up*. Reclaiming it means rewriting the file, which is a different
  operation and is not implemented.

The gap these leave is specific: an invariant can be enforced by the checker and
still be impossible for a mutation to maintain, because the checker sees only
the final state and the mutation has to hold the property across every
intermediate step. That is why the order of writes above is asserted rather than
merely documented -- an interrupted write is not something `fsck.hfsplus` is
going to be shown.
