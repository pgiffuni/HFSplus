#!/usr/bin/env python3
"""Generate src/unicode/tables.rs from Apple's UCStringCompareData.h.

The HFS case-folding comparison depends on two tables of several hundred hex
constants. They are transcribed mechanically rather than by hand: a single
mistyped entry produces a name that fails to match itself, which is very hard to
diagnose from behaviour and impossible to spot by eye.

Provenance: the data is derived from Apple `core/UCStringCompareData.h`
(APSL-1.2) at commit d1bac2f062e6e9c0dfcce302d9aacb10173d0eea. The generated Rust
file keeps that licence; see LICENSE-README.md.

Usage:
    tools/genucdtables.py /path/to/apple-hfs/core/UCStringCompareData.h
"""
import re
import sys
from pathlib import Path

LATIN_ENTRIES = 256

PREAMBLE = """// GENERATED FILE -- do not edit by hand.
//
// Regenerate with:
//     tools/genucdtables.py /path/to/apple-hfs/core/UCStringCompareData.h
//
// Mining reference: Apple `core/UCStringCompareData.h`, the `gLatinCaseFold` and
// `gLowerCaseTable` arrays. This data is derived from Apple's implementation and
// stays under APSL-1.2; see LICENSE-README.md.
//
// Transcribed mechanically rather than by hand. These are hundreds of hex
// constants and a single mistyped entry makes a name that fails to match itself,
// which is close to impossible to spot from behaviour alone.
"""

LATIN_DOC = """
/// Case folding for the 256 code units below U+0100.
///
/// Mining reference: `gLatinCaseFold`. Entry 0 maps U+0000 to 0xFFFF rather than
/// to zero, which is what lets `FastUnicodeCompare` use zero as its
/// end-of-string and ignore-this-character sentinel.
///
/// Despite spanning the whole Latin-1 supplement this table changes only 34
/// entries: ASCII A-Z, and the four letters with no precomposed upper/lower pair
/// -- AE, Eth, O-with-stroke and Thorn. Every other accented Latin capital is
/// identity, so `A` with grave does **not** match `a` with grave on an HFS+
///
/// volume.
"""

LOWER_DOC = """
/// Two-level case folding for code units at or above U+0100.
///
/// Mining reference: `gLowerCaseTable`, one flat array. The first 256 entries are
/// indices selected by the *high byte* of a code unit. An index of zero means the
/// whole high-byte block needs no mapping; a non-zero index is the position of
/// that block's 256-entry sub-table, indexed by the low byte.
///
/// A sub-table entry of zero means "this character is ignorable", which is how
/// sixteen formatting characters are skipped during comparison. Apple's own
/// comment records the algorithm:
///
/// ```text
/// lower = table[highbyte(c)]
/// if (lower == 0)
///     lower = c
/// else
///     lower = table[lower + lowbyte(c)]
/// if (lower == 0)
///     ignore this character
/// ```
"""


def extract(text: str, name: str) -> list[int]:
    """Pull `u_int16_t NAME[] = { ... };` out of a C header."""
    m = re.search(r"u_int16_t\s+" + name + r"\s*\[\s*\]\s*=\s*\{(.*?)\n\}", text, re.S)
    if not m:
        sys.exit(f"error: table {name} not found in the header")
    body = re.sub(r"/\*.*?\*/", " ", m.group(1), flags=re.S)
    values = [int(v, 16) for v in re.findall(r"0x([0-9A-Fa-f]{4})", body)]
    if not values:
        sys.exit(f"error: no values found in {name}")
    return values


def check(values: list[int], name: str) -> None:
    """Sanity-check the structural invariants the C code relies on."""
    if name == "gLatinCaseFold":
        if len(values) != LATIN_ENTRIES:
            sys.exit(f"error: {name} has {len(values)} entries, expected {LATIN_ENTRIES}")
        if values[0] == 0:
            sys.exit(
                f"error: {name}[0] is 0; Apple maps U+0000 to 0xFFFF so that zero can "
                f"act as the sentinel in FastUnicodeCompare"
            )
        return

    if len(values) < 256:
        sys.exit(f"error: {name} is shorter than its own 256-entry index block")
    for hb, off in enumerate(values[:256]):
        if off != 0 and off >= len(values):
            sys.exit(f"error: {name} high byte {hb} points at {off}, past the end")


def rows(values: list[int], per_row: int = 8, indent: str = "    ") -> str:
    out, row = [], []
    for v in values:
        row.append(v)
        if len(row) == per_row:
            out.append(indent + " ".join(f"0x{v:04X}," for v in row))
            row = []
    if row:
        out.append(indent + " ".join(f"0x{v:04X}," for v in row))
    return "\n".join(out)


def main() -> None:
    path = Path(sys.argv[1] if len(sys.argv) > 1 else "core/UCStringCompareData.h")
    text = path.read_text()
    latin = extract(text, "gLatinCaseFold")
    lower = extract(text, "gLowerCaseTable")
    check(latin, "gLatinCaseFold")
    check(lower, "gLowerCaseTable")

    parts = [
        PREAMBLE,
        LATIN_DOC,
        f"pub(crate) const G_LATIN_CASE_FOLD: [u16; {len(latin)}] = [\n",
        rows(latin),
        "\n];\n",
        LOWER_DOC,
        f"pub(crate) const G_LOWER_CASE_TABLE: [u16; {len(lower)}] = [\n",
        rows(lower),
        "\n];\n",
    ]

    dest = Path("src/unicode/tables.rs")
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text("".join(parts))
    print(f"{dest}: {len(latin)} latin entries, {len(lower)} case-fold entries")


if __name__ == "__main__":
    main()