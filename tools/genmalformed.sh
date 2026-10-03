#!/usr/bin/env bash
#
# Build the malformed slice of the test corpus.
#
# These images exist to prove that every parser in this crate fails safely.
# They are derived from a good generated image by byte-patching, and each one
# then gets a manifest saying exactly which structural error is expected.
#
# No malformed image is ever expected to mount. The requirement under test is
# that the library returns a structured `Error` and never panics, never reads
# out of bounds, and never loops forever.
#
# Usage:
#   tools/genmalformed.sh [output-dir]

set -euo pipefail

OUT_DIR="${1:-tests/images/malformed}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT_DIR="${REPO_ROOT}/${OUT_DIR#"${REPO_ROOT}"/}"

GOOD="${REPO_ROOT}/tests/images/generated/basic-hfsplus.img"
VHDR_OFF=1024   # volume header offset
CAT_OFF=2048    # first allocation block after the 4096-byte volume header

if [[ ! -f ${GOOD} ]]; then
  echo "error: ${GOOD} not found; run tools/genimages.sh first" >&2
  exit 1
fi

mkdir -p "${OUT_DIR}"
rm -f "${OUT_DIR}"/*.img

# patch <name> <python-patch-expression-file-or-inline>
# Each patcher receives the image bytes and returns modified bytes.
patch_with_python() {
  local out="$1"; shift
  python3 - "${GOOD}" "${out}" "$1" <<'PYEOF'
import sys

src, dst, script = sys.argv[1], sys.argv[2], sys.argv[3]
with open(src, "rb") as f:
    data = bytearray(f.read())

exec(script)

with open(dst, "wb") as f:
    f.write(bytes(data))
print(f"  + {dst.split('/')[-1]}")
PYEOF
}

echo "deriving malformed images from $(basename "${GOOD}")"
echo

# 1. Bad volume signature: not an HFS family member at all.
patch_with_python "${OUT_DIR}/bad-signature.img" '
data[1024:1026] = b"\xde\xad"          # neither BD, H+ nor HX
'

# 2. HFS+ signature with the HFSX version number. Apple pairs signature with
#    version, so this is a corrupt HFS+ volume rather than an HFSX volume.
#    The signature is left alone (bytes 48 2b already read as kHFSPlusSigWord);
#    only the version field is changed.
patch_with_python "${OUT_DIR}/hfsplus-sig-hfsx-version.img" '
data[1026:1028] = b"\x00\x05"          # kHFSXVersion on an HFS+ signature
'

# 3. Allocation block size that is not a power of two.
patch_with_python "${OUT_DIR}/bad-block-size.img" '
import struct
struct.pack_into(">I", data, 1024 + 40, 3000)   # blockSize field
'

# 4. Allocation block size below the 512-byte minimum.
patch_with_python "${OUT_DIR}/block-size-too-small.img" '
import struct
struct.pack_into(">I", data, 1024 + 40, 256)    # blockSize field
'

# 5. totalBlocks far larger than the volume can possibly hold.
#    blockSize stays a valid power of two on purpose: the point of this case is
#    not a malformed block size (covered by cases 3 and 4) but a header that is
#    internally well-formed yet describes a volume bigger than the backing
#    device. totalBlocks * blockSize cannot overflow a u64 -- the product of two
#    u32 values always fits -- so the failure has to surface as an out-of-range
#    or truncation error when the alternate header is located, not as an
#    arithmetic overflow.
patch_with_python "${OUT_DIR}/huge-total-blocks.img" '
import struct
struct.pack_into(">I", data, 1024 + 44, 0xFFFFFFFF)  # totalBlocks
'


# 5b. journalInfoBlock naming a block at or past the end of the volume.
#
#     The field is a block number and nothing else constrains it, so a volume
#     naming one outside itself is damaged. Refusing it is better than parsing
#     whatever happens to live at that offset -- which could be catalog data, or
#     the journal's own blocks. Without the bound the refusal is a coincidence of
#     the bytes found there rather than a statement about the volume.
patch_with_python "${OUT_DIR}/journal-info-block-out-of-volume.img" '
import struct
total = struct.unpack_from(">I", data, 1024 + 44)[0]
struct.pack_into(">I", data, 1024 + 4, 0x00002000)     # kHFSVolumeJournaledBit
struct.pack_into(">I", data, 1024 + 12, total)         # journalInfoBlock == totalBlocks
'

# 5c. The same, with a value large enough to overflow the block offset in any
#     case. Checked here so the arithmetic guard and the range guard are told
#     apart by something other than which message appears.
patch_with_python "${OUT_DIR}/journal-info-block-huge.img" '
import struct
struct.pack_into(">I", data, 1024 + 4, 0x00002000)     # kHFSVolumeJournaledBit
struct.pack_into(">I", data, 1024 + 12, 0x7FFFFFFF)
'

# 6. Volume header truncated: the image ends part way through the header.
truncate_image() {
  local out="$1" size="$2"
  local tmp
  tmp=$(mktemp)
  head -c "${size}" "${GOOD}" > "${tmp}"
  mv "${tmp}" "${out}"
  echo "  + $(basename "${out}")"
}

# 7. Image shorter than the volume header can ever be.
truncate_image "${OUT_DIR}/truncated-header.img" 1200

# 8. Image that stops inside the volume header.
truncate_image "${OUT_DIR}/truncated-half-header.img" 1200

# 9. All-zero image: plausible length, no signature at all.
dd if=/dev/zero of="${OUT_DIR}/all-zero.img" bs=1M count=1 status=none
echo "  + all-zero.img"

# 10. Catalog fork that claims extents pointing outside the volume.
patch_with_python "${OUT_DIR}/catalog-extent-out-of-range.img" '
import struct
# catalogFile is the third fork: offset 112 + 2*80 within the header.
base = 1024 + 112 + 2 * 80
# logicalSize = 1 MiB
struct.pack_into(">Q", data, base, 1024 * 1024)
# totalBlocks = 1
struct.pack_into(">I", data, base + 12, 1)
# extents[0] = startBlock 0xFFFF0000, blockCount 1
struct.pack_into(">II", data, base + 16, 0xFFFF0000, 1)
'

# 11. Volume header where the catalog fork claims more blocks than the volume has.
patch_with_python "${OUT_DIR}/fork-blocks-exceed-volume.img" '
import struct
base = 1024 + 112 + 2 * 80
struct.pack_into(">I", data, base + 12, 0x7FFFFFFF)   # totalBlocks
'

# 12. Extent record whose terminator is missing: every slot has a block count.
#     A fork with no terminator would otherwise be iterated past its end.
patch_with_python "${OUT_DIR}/extents-no-terminator.img" '
import struct
base = 1024 + 112 + 2 * 80
for i in range(8):
    struct.pack_into(">II", data, base + 16 + i * 8, i * 2 + 1, 1)
'

echo
# IMPORTANT: fsck.hfsplus REPAIRS rather than merely reports.
#
# Running it on a fixture whose primary volume header was corrupted does not
# fail -- it copies the intact backup header (1024 bytes before the end of the
# volume) back over the primary one and rewrites checkedDate. In testing this
# was observed to restore a patched image to a byte-identical copy of the
# original. Running the checker over the corpus therefore *destroys* it.
#
# The check below consequently runs against a throwaway copy, and the canonical
# fixtures are never handed to fsck at all.
echo "verifying that each fixture still bites (checker run on a COPY, never on"
echo "the fixture, because fsck_hfs repairs the volume header in place):"
if [[ -x /usr/sbin/fsck.hfsplus ]]; then
  for img in "${OUT_DIR}"/*.img; do
    name=$(basename "${img}")
    probe=$(mktemp --suffix=.img)
    cp --sparse=always "${img}" "${probe}"
    if /usr/sbin/fsck.hfsplus "${probe}" >/dev/null 2>&1; then
      echo "  fsck accepted the copy: ${name}"
    else
      echo "  fsck rejected the copy: ${name}"
    fi
    rm -f "${probe}"
  done
fi

echo
echo "canonical fixtures left untouched in ${OUT_DIR}"
echo
echo "Do NOT run fsck.hfsplus on these files. tests/malformed_safety.rs asserts"
echo "our parser rejects each one; fsck would silently repair them."

