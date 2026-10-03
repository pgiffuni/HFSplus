#!/usr/bin/env bash
#
# Build the generated slice of the HFS+ test corpus.
#
# Every image is produced by `mkfs.hfsplus` (hfsprogs 540.1, APSL-2.0, Apple).
# It is an external process, not a dependency of the crate: this project's
# licence obligations are unaffected, and the corpus stays reproducible on any
# machine with hfsprogs installed.
#
# Determinism
# -----------
# HFS+ volume headers embed creation, modification and last-check timestamps,
# so two runs of this script on different days produce images that differ in a
# few bytes. To keep the corpus reproducible:
#
#   * images are regenerated only when missing (or with --force),
#   * a SHA-256 is recorded in the manifest after generation,
#   * tests that need byte-exact reproducibility regenerate into a temp
#     directory rather than comparing committed images byte-for-byte.
#
# Usage:
#   tools/genimages.sh [--force] [output-dir]

set -euo pipefail

FORCE=0
if [[ "${1:-}" == "--force" ]]; then
  FORCE=1
  shift
fi

OUT_DIR="${1:-tests/images/generated}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT_DIR="${REPO_ROOT}/${OUT_DIR#"${REPO_ROOT}"/}"

MKDIR=$(command -v mkfs.hfsplus || true)
if [[ -z "${MKDIR}" ]]; then
  for candidate in /usr/sbin/mkfs.hfsplus /sbin/mkfs.hfsplus; do
    [[ -x ${candidate} ]] && MKDIR=${candidate} && break
  done
fi
if [[ -z "${MKDIR}" ]]; then
  echo "error: mkfs.hfsplus not found (install hfsprogs)" >&2
  exit 1
fi

FSCK=$(command -v fsck.hfsplus || echo /usr/sbin/fsck.hfsplus)

mkdir -p "${OUT_DIR}"

# make_image <name> <size-MiB> <mkfs-args...>
make_image() {
  local name="$1"; shift
  local size_mb="$1"; shift
  local img="${OUT_DIR}/${name}.img"
  local bytes=$(( size_mb * 1024 * 1024 ))

  if [[ -f ${img} && ${FORCE} -eq 0 ]]; then
    echo "  = ${name}.img (exists, skipped; use --force to rebuild)"
    return 0
  fi

  # Create sparse: an HFS+ volume is mostly free space, so allocating the
  # blocks up front wastes both time and disk.
  truncate -s "${bytes}" "${img}"
  "${MKDIR}" "$@" "${img}" >/dev/null
  echo "  + ${name}.img ($(( $(stat -c %s "${img}") / 1024 / 1024 )) MiB)  mkfs.hfsplus $*"
}

echo "mkfs.hfsplus : ${MKDIR}"
echo "fsck.hfsplus : ${FSCK}"
echo "output       : ${OUT_DIR}"
echo

echo "BASIC"
make_image basic-hfsplus      32 -v "BasicVolume"
make_image basic-hfsplus-1k   32 -v "SmallBlocks"  -b 1024
make_image basic-hfsplus-8k   64 -v "LargeBlocks"  -b 8192
make_image basic-hfsplus-16k  64 -v "HugeBlocks"   -b 16384

echo "NAMES / CASE"
# HFSX case-sensitive: -s changes the volume signature to kHFSXSigWord (0x4858).
make_image hfsx-case-sensitive  32 -v "CaseSensitive" -s
make_image hfsx-case-insensitive 32 -v "CaseInsensitive"

echo "JOURNALING"
# -J creates a journaled volume. hfsprogs clamps the size to its own minimum.
make_image journaled-hfsplus    32 -v "Journaled" -J
make_image journaled-hfsplus-1k 32 -v "Journaled1K" -J -b 1024

echo "LEGACY"
# -h creates a classic HFS volume wrapped for Mac OS 9 bootability. This crate
# must recognise the signature and refuse it cleanly, not misparse it.
make_image classic-hfs          32 -h -v "ClassicVolume"

echo
echo "JOURNAL REPLAY"
# Every image created with -J has an uninitialised journal, because no
# transaction has ever been written, so the corpus cannot exercise replay at all.
# makejournal.py writes a real journal header, transaction and block list into a
# journaled image so that the replay path has something to replay.
if [[ -x tools/makejournal.py || -f tools/makejournal.py ]]; then
  mkdir -p tests/images/replayed
  # The three names tests/journal_replay.rs expects, so there is one recipe.
  [[ -f tests/images/generated/journaled-hfsplus.img ]] && python3 tools/makejournal.py \
    tests/images/generated/journaled-hfsplus.img \
    tests/images/replayed/journal-replay-be.img --block 200 --data "journal replayed block 200"

  # The little-endian header form, which is what a 64-bit x86 or ARM host writes
  # and the reader has to detect rather than assume.
  [[ -f tests/images/generated/journaled-hfsplus.img ]] && python3 tools/makejournal.py \
    tests/images/generated/journaled-hfsplus.img \
    tests/images/replayed/journal-replay-le.img --block 201 --little-endian \
    --data "journal replayed block 201"

  # A different volume block size, so the geometry is not only exercised at 4K.
  [[ -f tests/images/generated/journaled-hfsplus-1k.img ]] && python3 tools/makejournal.py \
    tests/images/generated/journaled-hfsplus-1k.img \
    tests/images/replayed/journal-replay-1k.img --block 300 \
    --data "journal replayed block 300 on a 1k volume"
fi

echo "verifying every image with the independent checker"
fail=0
for img in "${OUT_DIR}"/*.img; do
  name=$(basename "${img}")
  if out=$("${FSCK}" "${img}" 2>&1) && grep -q "appears to be OK" <<<"${out}"; then
    echo "  ok   ${name}"
  else
    echo "  FAIL ${name}"
    sed 's/^/       /' <<<"${out}"
    fail=1
  fi
done

echo
echo "sha256:"
(cd "${OUT_DIR}" && sha256sum *.img)

if [[ ${fail} -ne 0 ]]; then
  echo >&2
  echo "error: at least one image failed fsck.hfsplus" >&2
  exit 1
fi
