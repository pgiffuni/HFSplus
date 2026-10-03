# Installed HFS tooling: what actually works

Findings from the development environment (Ubuntu 26.04, x86-64). Every claim
below was established by running the tool, not by reading a man page.

**Licence note.** `hfsutils` and `hfsplus` are GPL-2.0. They may be *executed*
to build images but their source must not be read, copied, or translated. See
`LICENSE-README.md`. `hfsprogs` is APSL-2.0 (Apple) and is the tool this project
relies on.

## Packages

| Package | Version | Licences | Role here |
| --- | --- | --- | --- |
| `hfsprogs` | 540.1.linux3-6build1 | APSL-2.0 (Apple), MIT | **Primary.** `mkfs.hfsplus`, `fsck.hfsplus` |
| `hfsplus` | 1.0.4-19build1 | GPL-2.0 | `hp*` tools; exec-only |
| `hfsutils` | 3.2.6-16ubuntu2 | GPL-2.0 | `h*` tools; exec-only, **classic HFS only** |
| `libhfsp0t64` | 1.0.4-19build1 | GPL-2.0 | shared library behind `hp*` |

## `hfsprogs` — the one that matters

### `mkfs.hfsplus` — creates HFS+, HFSX and journaled volumes

```
mkfs.hfsplus [-N [partition-size]] [options] special-device
  -h            create a classic HFS filesystem
  -N            print parameters without creating
  -s            case-sensitive filenames
  -w            add an HFS wrapper (Mac OS 9 bootable)
  -J [size]     make the volume journaled
  -D journal-dev use an external journal device
  -G group-id   root directory group
  -U user-id    root directory user
  -M octal mask root directory permissions
  -b size       allocation block size (4096 optimal)
  -c list       clump sizes: a= b= c= d= e= r=
  -i id         starting catalog node id
  -n list       b-tree node sizes: c= e= a=
  -v name       volume name
```

Verified behaviour:

| Feature | Flag | Result |
| --- | --- | --- |
| HFS+ | *(default)* | signature `0x482B`, version `4` |
| HFSX case-sensitive | `-s` | signature `0x4858`, version `5` |
| Journaled | `-J` | `kHFSVolumeJournaledBit` set, `journalInfoBlock` non-zero |
| Classic HFS | `-h` | signature `0x4244` |
| Block size | `-b 1024/8192/16384` | honoured; `-b 1024` warns that it is below the B-tree node size |
| Journal size | `-J 8192` | silently clamped up to 8192 kB ("too small, reset to 8192k") |

`-w` does **not** create an Apple partition map: sector 0 of the resulting image
is all zeros. It embeds a classic HFS wrapper volume for Mac OS 9.

### `fsck.hfsplus` — the independent checker

```
fsck.hfsplus [-Edfglpqruy] [-B path] [-b size] [-c size] [-m mode] device
  E exit on first major error        p fix normal inconsistencies only
  f force fsck even if clean        q quick: clean / dirty / failure
  g GUI output mode                 r rebuild catalog btree
  l live fsck (test-only)           u usage
  d debugging output                v version
  x XML output mode                 y assume yes
  n assume no response
```

It checks extents overflow, catalog, multi-linked files, catalog hierarchy, the
attributes file, the volume bitmap and volume information.

**It modifies the image it checks.** This is the single most important fact about
using it in a test pipeline:

```
$ cp corrupted.img probe.img
$ fsck.hfsplus probe.img      # reports "appears to be OK"
$ sha256sum probe.img corrupted.img
```

Observed behaviour: given a volume whose primary volume header was deliberately
corrupted, `fsck.hfsplus` restored the header from the backup copy 1024 bytes
before the end of the volume, rewrote `checkedDate`, and returned the image to a
state byte-identical to the pristine original. Consequences:

1. Never point `fsck.hfsplus` at a test fixture or a canonical image. Copy first.
2. It is a **repair tool, not a conformance oracle**. "fsck says OK" does not mean
   "a mount would accept this".
3. `tools/genmalformed.sh` originally ran the checker over its own fixtures and
   silently undid every corruption. It now runs against a throwaway copy.

#### What the port does not check

`hfsprogs` is an unofficial port of Apple's `fsck_hfs`, and it is a faithful one:
111 of the 119 message strings in Apple's `lib_fsck_hfs/fsck_hfs_strings.c` are
present verbatim in the installed binary, and some of the rest are present
reworded (`"Volume bitmap needs repair for under-allocation"` became
`"...needs minor repair..."`, `"Journal needs to be replayed"` became
`"Journal need to be replayed"`). That is why it is usable as an arbiter at all.

Seven checks are absent, and one of them matters here:

| Absent check | Consequence |
| --- | --- |
| `Bad information for symbolic link`, `Symbolic link ... has bad length`, `Bad symbolic link is` | **Symlinks are not validated at all.** |
| `Invalid Finder info for {file,directory} hard link` | Finder info on hard-link members is unchecked. |
| `B-tree node is split across extents` | A B-tree whose nodes straddle non-contiguous extents is accepted. |
| `Journal need[s] to be replayed but volume is read-only` | Present but reworded; see below. |

Verified empirically rather than inferred from the strings: taking
`journal-with-files.img` and zeroing the data fork of the `link` symlink — which
Apple rejects as bad information for a symbolic link — passes the port's catalog
check without comment. The only complaints are about the bitmap and the free
block count, and those follow from the zeroed fork rather than from the link.

So the symlink fixture's validity rests on the format rather than on the checker,
and `tests/volume_files.rs` says so where it relies on it.

Two consequences for how results are reported:

- "fsck says OK" is evidence that the structures it *does* examine are
  consistent. It is not evidence that the volume is wholly sound, and for the
  fields the port skips it is not evidence at all.
- `fsck` leaves `journal-torn-catalog.img` byte-identical and reports it sound,
  which shows it validated the stale filesystem without writing the replay. That
  is a weaker claim than "fsck never replays the journal", which is what the
  test used to say and what the byte comparison can actually support.

### `mkfs.hfs` / `fsck.hfs` — classic HFS

Classic HFS only, in practice: `mkfs.hfs` defaults to HFS+ just like
`mkfs.hfsplus` and takes the same options. `fsck.hfs` handles classic volumes and
the HFS-wrapper case.

## `hfsplus` (`hp*` tools, libhfsp 1.0.4) — GPL-2.0

`hpcd hpcopy hpfsck hpls hpmkdir hpmount hppwd hprm hpumount`

`hpmount` can locate a volume through an Apple partition map, and `libhfsp`
contains genuine HFS+ support (`volume_read_wrapper` accepts `0x482B`). Two
limitations observed:

- `volume_read_wrapper` matches only `0x4244` (wrapper) and `0x482B` (HFS+). There
  is no branch for `0x4858`, so **HFSX volumes are rejected** with "Neither
  Wrapper nor native HFS+ volume header found".
- Mounting requires either a partition map and an explicit partition number, or a
  bare classic HFS volume. A bare HFS+ or HFSX image is rejected as "not a HFS+
  volume".

Not used in the differential pipeline. Run only if some future need arises that
`hfsprogs` cannot serve.

## `hfsutils` 3.2.6 (`h*` tools) — GPL-2.0, **classic HFS only**

`hformat hmount humount hvol hls hdir hcd hpwd hcopy hdel hmkdir hrmdir
hrename hattrib`

`libhfs/volume.c` `v_readmdb` requires `mdb.drSigWord == HFS_SIGWORD` and reports
"not a Macintosh HFS volume" for anything else. There is no HFS+ code path at
all. Empirically:

| Image | `hmount` |
| --- | --- |
| classic HFS (`0x4244`) | mounts |
| HFS+ (`0x482B`) | `not a Macintosh HFS volume` |
| HFSX (`0x4858`) | `not a Macintosh HFS volume` |
| journaled HFS+ | `not a Macintosh HFS volume` |

`hformat` creates **classic HFS** by default (`0x4244` at offset 1024).

Because it cannot read HFS+ at all, hfsutils is useless as an HFS+ differential
reference. It is also GPL-2.0. **Excluded from this project.**

## This project's own tools

Both read only. Neither opens an image for writing anywhere in its code, so
pointing them at real media is safe; `tests/cli.rs` asserts that running every
option combination leaves the image byte-identical.

```
hfsls [options] <image> [path]
  -l, --long        show mode, size, dates, CNID and Finder codes
  -a, --all          include entries whose names begin with a dot
  -R, --recursive    descend into directories
  -s, --stat         print volume statistics and exit
  -b, --bits         print the allocation bitmap summary
  -j, --journal      report journal detection and replay state
  --json             machine-readable output

hfsinspect [options] <image>...
  --json, --verbose, --btrees
```

Exit status is the same in both, and the three cases stay distinct so a caller
can tell them apart:

| Status | Meaning |
| --- | --- |
| 0 | every image inspected cleanly |
| 1 | a usage error: no arguments, or an unknown option |
| 2 | an image could not be parsed |

A malformed image is a *result*, not a crash. `hfsinspect` reports it and
continues to the next path, so one bad image in a batch does not hide the others;
in `--json` mode each line is a self-contained object carrying its own `ok`
field, so a later failure cannot truncate an earlier result.

Two behaviours worth knowing before relying on the output:

- **`hfsls` does not replay the journal.** It lists the filesystem as it is on
  the disk. Replaying silently would show a user diagnosing a crash a filesystem
  that does not match their media, so the journal report is a separate, explicit
  request via `-j`, and `tests/cli.rs` asserts the distinction.
- **A listed name is the catalog's spelling, not the one typed.** On a
  case-folding volume several spellings reach the same record, and the tool
  reports what is stored. `hfsls image .JOURNAL` prints `.journal`.

## Capability matrix

| Capability | `mkfs.hfsplus` / `fsck.hfsplus` | `hpmount`/`hpls` | `hmount`/`hls` | this project |
| --- | --- | --- | --- | --- |
| Create HFS+ | yes | — | yes, but classic HFS only | no (read-only) |
| Create HFSX | yes (`-s`) | — | no | read |
| Create journaled HFS+ | yes (`-J`) | — | no | read + replay |
| Create classic HFS | yes (`-h`) | — | yes | refuses cleanly |
| Check/repair HFS+ | yes, incl. `-r` rebuild catalog | `hpfsck` | no | no |
| Inspect HFS+ | yes (`-x` XML, `-d` debug) | partial | no | `hfsinspect` |
| Inspect catalog | yes | `hpls` | no | `hfsls`, `hfsinspect --btrees` |
| Inspect extents | yes | — | no | `hfsinspect` |
| Modify HFS+ | no | read-only in practice | no | no |
| Manipulate files/dirs | no | no | classic HFS only | no |
| Metadata / xattrs | reports only | no | no | reads BSD info and dates |

## Reference source versions pinned

| Source | Commit / version |
| --- | --- |
| `apple-oss-distributions/hfs` (mirror: `pgiffuni/apple-hfs`) | `d1bac2f062e6e9c0dfcce302d9aacb10173d0eea` |
| `0x09/hfsfuse` (baseline only) | `8c44b9aa80a8eba541ac3a8b86aad8eecde9277f` |
| `hfsprogs` checker | `540.1.linux3-6build1` |
| `mkfs.hfsplus` | `540.1.linux3` |

Apple's repository also ships `fstyp_hfs`, `hfs_util`, `mount_hfs`, `newfs_hfs`,
`lib_fsck_hfs`, and 48 test cases under `tests/cases/`. Those are the primary
sources for on-disk semantics and for the list of behaviours this implementation
is expected to get right (`test-dir-link.c`, `test-dprotect.c`,
`test-external-jnl.c`, `test-unicode-file-names.c`, `test-hard-links.m`, and so
on).
