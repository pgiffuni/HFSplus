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

One mutation entry point exists, `WritableVolume`, with four operations on it:
`write_file_contents` (overwrite, grow or truncate in place), `truncate_file`,
`create_file`, and the node splitting that `create_file` needs once a catalog leaf
fills.

| Invariant | Holds because | Tested by |
| --- | --- | --- |
| Structure | `FileRecord::write_to` is the inverse of `parse`, so a record read and written back is the same bytes | `a_file_record_round_trips_through_the_same_bytes` -- byte equality against the input, not a re-parse |
| Round trip | the data blocks and the record are written in the order that never exposes a torn file: blocks first, then the length that points at them | `writing_shorter_contents_keeps_the_volume_readable`, `writing_the_same_number_of_bytes_keeps_the_allocation`, `writing_an_empty_file_leaves_no_trailing_bytes`, `a_partial_trailing_block_is_read_back_as_the_tail_and_not_beyond` |
| Independent | `fsck.hfsplus` accepts every written image | `assert_fsck_clean` in `tests/write.rs`, on every write and create above |

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

`create_file` is the first mutation that *adds* rather than changes, and it keeps
four structures in step: the file record, its thread record, the parent's child
count, and the header's next-CNID counter. A file in three of the four is a file
that cannot be found by name, cannot be found by CNID, or will be handed the same
identity twice — and only the thread record is what makes a lookup by CNID work at
all.

Node mutation is asserted by the property rather than by the bytes:
`every_record_is_still_findable_after_an_insertion` inserts at every position and
checks that each original record is intact, that the new one is where it was asked
for, and that the used region grew by exactly one record.

Node mutation is asserted by the property rather than by the bytes, because the
bytes were wrong four times before they were right:

| Property | Test |
| --- | --- |
| every record survives an insertion, at its new offset, with its bytes intact | `every_record_is_still_findable_after_an_insertion`, at every index from 0 to the record count |
| removal is the inverse of insertion | `removal_is_the_inverse_of_insertion`, at every index |
| a split leaves a tree a reader can still search | `creating_enough_files_splits_the_catalog_and_leaves_it_consistent`, and `every_file_survives_a_split_findable_by_name_and_by_cnid` — every file, by both routes |
| a split keeps the header's counts honest | `leafRecords` recounted against the leaves; one index record per leaf |
| an index separator is still the first key of its subtree | `an_insert_at_the_front_of_a_leaf_refreshes_that_leaves_index_separator`, which walks every separator against its leaf's first key |

That last one exists because a split that forgets to advance `leafRecords` for the
record that *caused* it leaves the count short by one, and `fsck` recounts.

**Two independent checkers, and where they agree.** Node splitting was the first
mutation to produce an image with an index node, and therefore the first to
exercise the reader's tree descent. Three reader bugs surfaced at once: a child
pointer read one past its own end, a search key above every separator refused
instead of descending, and `check::check` walking the leaf chain by `bLink`. All
three are invisible on a one-leaf catalog, which is every image the corpus
committed before this. That is the argument for a committed fixture with a split
catalog, and it is the one gap in the corpus now.

The separator test was checked for teeth before being trusted: with the refresh
disabled it fails on exactly the property `fsck` reports — the separator's key
length is the thread record's 6 where the leaf's real first key is 28.

## The one defect Milestone 8 leaves behind

`lookup_cnid` misses some files once the catalog has grown past a single node. Every
file is findable by name at any size this crate can produce, and `fsck.hfsplus`
accepts the volume, so this is a reader bug rather than a damaged volume -- but it
is a real one, and it was **unreachable** rather than absent until catalog growth
existed: a single-node catalog tops out at 47 files, so nothing could create the
conditions.

`every_file_is_still_findable_by_cnid_before_the_catalog_grows` pins the working
half, so the boundary is a fact rather than a suspicion. 47 is where growth first
happens, so that test covers the whole of the previously reachable range.

Not yet diagnosed. By-name and by-CNID differ in one respect: the by-name search
compares a `(parent, name)` key and the by-CNID search a `(cnid, "")` key, and the
latter's keys are *thread* keys, which all sort past every `(parent, name)` key.
So the two search disjoint parts of the key space, and a defect that affected only
the second would look exactly like this.

What is *not* yet true of any mutation here:

- **The catalog cannot grow.** Splitting allocates a node from the header's map,
  and that is the only node source implemented. Extending the B-tree file means
  allocating blocks for it, so a catalog with eight nodes fills at about forty
  files and then reports `NoSpace` — honestly, and with nothing written.
- **No folders.** A file can be created in the root or any existing folder, but no
  folder can be created, and `folderCount` in the header is never written.

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
