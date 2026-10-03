//! HFS name comparison.
//!
//! This is the compatibility heart of the filesystem. Getting it wrong does not
//! produce a visible error: files simply become unreachable, because the name
//! that is stored cannot be found by the name that is asked for.
//!
//! # Do not use `String::cmp`
//!
//! Rust's byte-wise UTF-8 comparison is wrong for HFS+ in three separate ways,
//! all of which matter:
//!
//! 1. **UTF-8 byte order is not UTF-16 code unit order.** For most text they
//!    agree, but they diverge above the BMP, because UTF-8 sorts by code point
//!    while HFS+ stores UTF-16. Two names made of supplementary characters order
//!    differently under the two.
//! 2. **HFS+ case folding is neither Unicode case folding nor Unicode
//!    normalisation.** Apple's comparator folds a fixed set of code points and
//!    skips a fixed set of sixteen characters that fold to zero. It does **not**
//!    normalise, so a composed name and its decomposed form compare
//!    *differently*. It also does not fold most accented Latin: the Latin-1
//!    supplement is identity, so `À` (U+00C0) does **not** match `à` (U+00E0).
//!    See [`FOLDED_BLOCKS`] and [`IGNORABLE_CHARACTERS`] for the exact sets.
//! 3. **Which comparator runs is decided by more than the volume signature.**
//!    See below.
//!
//! # Which comparator a volume uses
//!
//! This is the part that is easy to get wrong. Apple opens the catalog B-tree
//! with the *folding* comparator by default, and only switches to the binary one
//! under a conjunction of conditions:
//!
//! ```c
//! retval = BTOpenPath(catalog_vp, (KeyCompareProcPtr) CompareExtendedCatalogKeys);
//! ...
//! if ((hfsmp->hfs_flags & HFS_X) && BTGetInformation(...) == 0) {
//!     if (btinfo.keyCompareType == kHFSBinaryCompare) {
//!         hfsmp->hfs_flags |= HFS_CASE_SENSITIVE;
//!         BTOpenPath(catalog_vp, (KeyCompareProcPtr) cat_binarykeycompare);
//!     }
//! }
//! ```
//!
//! Mining reference: Apple `core/hfs_vfsutils.c` `hfs_MountHFSPlusVolume`.
//!
//! So a volume is case-sensitive only when it is **both** HFSX **and** its
//! catalog `keyCompareType` is `kHFSBinaryCompare`. An HFSX volume that
//! recorded `kHFSCaseFolding` still folds. And on a plain HFS+ volume the
//! `keyCompareType` field is not consulted at all.
//!
//! This is why `hfsx-case-insensitive` in the corpus has an HFS+ signature
//! (`mkfs_hfsplus` only emits `kHFSXSigWord` for `-s`) yet is still handled by
//! the folding path, and why an HFSX signature alone is never sufficient to
//! conclude case sensitivity.

use super::tables::{G_LATIN_CASE_FOLD, G_LOWER_CASE_TABLE};

/// Ordering of two HFS+ catalog names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ordering {
    /// The first name sorts before the second.
    Less,
    /// The two names are equal under this comparator.
    Equal,
    /// The first name sorts after the second.
    Greater,
}

impl Ordering {
    /// Merge two orderings, keeping the first non-equal result.
    ///
    /// Mirrors Apple's `CompareExtendedCatalogKeys`, which compares `parentID`
    /// first and only reaches the name when they are equal.
    pub fn then(self, other: Ordering) -> Ordering {
        if self == Ordering::Equal {
            other
        } else {
            self
        }
    }

    /// The ordering as a three-way integer, for B-tree search code.
    pub fn as_i32(self) -> i32 {
        match self {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }
}

impl From<Ordering> for i32 {
    fn from(o: Ordering) -> i32 {
        o.as_i32()
    }
}

/// The comparison rule a catalog uses.
///
/// Mining reference: Apple `core/hfs_catalog.c` defines two comparators and
/// `core/hfs_vfsutils.c` chooses between them as described in the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparator {
    /// `CompareExtendedCatalogKeys`: fold case and skip ignorable characters.
    CaseFolding,
    /// `cat_binarykeycompare` plus `UnicodeBinaryCompare`: plain code unit order.
    Binary,
}

impl Comparator {
    /// Choose the comparator for a volume, given how it was opened.
    ///
    /// `is_hfsx` is the volume signature test; `key_compare_type` is the raw
    /// byte from the catalog B-tree header. Both must agree before names compare
    /// case-sensitively.
    ///
    /// Mining reference: Apple `core/hfs_vfsutils.c` `hfs_mounthfsplus` tests
    /// `(hfsmp->hfs_flags & HFS_X)` *and* `btinfo.keyCompareType ==
    /// kHFSBinaryCompare` before setting `HFS_CASE_SENSITIVE`, so the two
    /// conditions are conjunctive there as they are here.
    ///
    /// The signature test is not redundant with the byte. A volume is HFSX
    /// exactly when the signature word is `kHFSXSigWord`, and a plain HFS+ volume
    /// is permitted to carry `kHFSBinaryCompare` in its catalog header — the
    /// byte describes how *that* tree was built, not what the filesystem means by
    /// a name. Trusting it alone would make an HFS+ volume report
    /// case-sensitivity that does not exist, and every lookup by a folded name
    /// would miss.
    ///
    /// `kHFSBinaryCompare` from `core/hfs_format.h`. Note that
    /// `core/hfs_btreeio.c` writes exactly this byte into the trees it
    /// initialises, but that is `hfs_create_attr_btree` and concerns the
    /// attributes B-tree, whose keys are attribute names rather than file names.
    pub const fn for_volume(is_hfsx: bool, key_compare_type: u8) -> Self {
        if is_hfsx && key_compare_type == crate::catalog::key::K_HFS_BINARY_COMPARE {
            Comparator::Binary
        } else {
            Comparator::CaseFolding
        }
    }

    /// Compare two UTF-16 names under this rule.
    pub fn compare(self, a: &[u16], b: &[u16]) -> Ordering {
        match self {
            Comparator::CaseFolding => fast_unicode_compare(a, b),
            Comparator::Binary => unicode_binary_compare(a, b),
        }
    }

    /// Whether this comparator distinguishes case.
    pub const fn is_case_sensitive(self) -> bool {
        matches!(self, Comparator::Binary)
    }
}

/// The code-point blocks that HFS+ case folding actually touches.
///
/// Mining reference: derived from Apple `core/UCStringCompareData.h` by walking
/// `gLowerCaseTable` and collecting the high bytes with a non-zero index, then
/// removing those whose entries are all zero.
///
/// Two absences here are as important as the presences:
///
/// - **The Latin-1 supplement is almost entirely absent.** `gLatinCaseFold`
///   spans U+0000-U+00FF but changes only 34 entries: ASCII A-Z, and the four
///   letters with no precomposed upper/lower pair — AE, Eth, O-with-stroke and
///   Thorn. Every other accented capital is identity, so `À` does **not** match
///   `à`. This surprises anyone expecting Unicode semantics, and it is a real
///   interoperability hazard: the same HFS+ directory can be listed under
///   different names on systems with different folding rules.
/// - **There is no normalisation.** Composed and decomposed spellings are
///   different names.
pub const FOLDED_BLOCKS: &str =
    "U+0100-U+01FF, U+0300-U+03FF, U+0400-U+04FF, U+0500-U+05FF, \
     U+1000-U+10FF, U+2000-U+20FF, U+2100-U+21FF, U+FE00-U+FEFF, U+FF00-U+FFFF";

/// The sixteen code units that HFS+ case folding skips.
///
/// Mining reference: derived from Apple `core/UCStringCompareData.h`
/// `gLowerCaseTable`, whose sub-table entries of zero mark the ignorable
/// characters. Verified by enumerating every zero entry: there are exactly
/// sixteen, and they are all bidi, zero-width or deprecated formatting
/// characters. None of them is a combining mark.
///
/// This is worth stating explicitly because it is a common misconception. HFS
/// (classic Mac OS) compared names through a decomposition table, so composed and
/// decomposed spellings were equal there. HFS+ does not carry that table: the
/// only folding data in `UCStringCompareData.h` is `gLatinCaseFold`,
/// `gLowerCaseTable` and `gCompareTable`, the last being for legacy 8-bit Mac
/// Script names and not used by HFS+ at all.
pub const IGNORABLE_CHARACTERS: [u16; 16] = [
    0x200C, // ZERO WIDTH NON-JOINER
    0x200D, // ZERO WIDTH JOINER
    0x200E, // LEFT-TO-RIGHT MARK
    0x200F, // RIGHT-TO-LEFT MARK
    0x202A, // LEFT-TO-RIGHT EMBEDDING
    0x202B, // RIGHT-TO-LEFT EMBEDDING
    0x202C, // POP DIRECTIONAL FORMATTING
    0x202D, // LEFT-TO-RIGHT OVERRIDE
    0x202E, // RIGHT-TO-LEFT OVERRIDE
    0x206A, // INHIBIT SYMMETRIC SWAPPING
    0x206B, // ACTIVATE SYMMETRIC SWAPPING
    0x206C, // INHIBIT ARABIC FORM SHAPING
    0x206D, // ACTIVATE ARABIC FORM SHAPING
    0x206E, // NATIONAL DIGIT SHAPES
    0x206F, // NOMINAL DIGIT SHAPES
    0xFEFF, // ZERO WIDTH NO-BREAK SPACE / byte order mark
];

/// Fold one code unit, or return `None` if it is ignorable.
///
/// Mining reference: Apple `core/UnicodeWrappers.c` `FastUnicodeCompare`,
/// which inlines exactly this:
///
/// ```c
/// if (c1 < 0x0100) {
///     c1 = gLatinCaseFold[c1];
/// } else if ((temp = lowerCaseTable[c1 >> 8]) != 0) {
///     c1 = lowerCaseTable[temp + (c1 & 0x00FF)];
/// }
/// ```
#[inline]
fn fold(c: u16) -> Option<u16> {
    let folded = if c < 0x0100 {
        G_LATIN_CASE_FOLD[c as usize]
    } else {
        let high = (c >> 8) as usize;
        let sub = G_LOWER_CASE_TABLE[high];
        if sub == 0 {
            c
        } else {
            G_LOWER_CASE_TABLE[sub as usize + (c & 0x00FF) as usize]
        }
    };
    // Zero means "ignore this character". The end of a string folds to the same
    // sentinel, which is how the two cases are told apart by position alone.
    if folded == 0 {
        None
    } else {
        Some(folded)
    }
}

/// Next non-ignorable code unit, or `None` at end of string.
#[inline]
fn next_valid(s: &[u16], i: &mut usize) -> Option<u16> {
    while *i < s.len() {
        let c = s[*i];
        *i += 1;
        // Basic Latin is checked first and breaks immediately, exactly as Apple
        // does, which matters because a Latin code unit never folds to zero
        // (U+0000 maps to 0xFFFF) so the fast path cannot skip anything.
        let folded = if c < 0x0100 {
            G_LATIN_CASE_FOLD[c as usize]
        } else {
            match fold(c) {
                Some(f) => f,
                None => continue,
            }
        };
        if folded != 0 {
            return Some(folded);
        }
    }
    None
}

/// HFS+ case-insensitive, decomposition-tolerant name comparison.
///
/// Mining reference: Apple `core/UnicodeWrappers.c` `FastUnicodeCompare`:
///
/// ```c
/// while (1) {
///     c1 = 0; c2 = 0;
///     while (length1 && c1 == 0) { c1 = *(str1++); --length1; ... }
///     while (length2 && c2 == 0) { c2 = *(str2++); --length2; ... }
///     if (c1 != c2) break;
///     if (c1 == 0) return 0;
/// }
/// if (c1 < c2) return -1; else return 1;
/// ```
///
/// Three properties that a hand-written `to_lowercase` comparison does not have:
///
/// - The sixteen characters in [`IGNORABLE_CHARACTERS`] are **skipped
///   entirely**, so a name containing one compares equal to the same name
///   without it.
/// - A shorter string is not automatically "less": `ab` and `ab` plus an
///   ignorable character are equal.
/// - Equality is decided by reaching the end of both strings at the same time,
///   not by comparing lengths.
///
/// What it does **not** do is normalise. `e` + U+0301 and U+00E9 are two
/// different names here, because U+0301 is not in the ignorable set.
pub fn fast_unicode_compare(a: &[u16], b: &[u16]) -> Ordering {
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        match (next_valid(a, &mut i), next_valid(b, &mut j)) {
            (Some(x), Some(y)) => {
                if x != y {
                    return if x < y { Ordering::Less } else { Ordering::Greater };
                }
            }
            (None, None) => return Ordering::Equal,
            // One string ran out and the other still has a character, so the
            // shorter one sorts first.
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// HFS+ case-sensitive, binary name comparison.
///
/// Mining reference: Apple `core/UnicodeWrappers.c` `UnicodeBinaryCompare`.
///
/// Note the ordering of Apple's test: the length comparison is computed first
/// and used only if every shared code unit matched, so a difference in the
/// prefix wins over the length. The result is ordinary lexicographic order over
/// code units.
pub fn unicode_binary_compare(a: &[u16], b: &[u16]) -> Ordering {
    let shared = a.len().min(b.len());
    for i in 0..shared {
        match a[i].cmp(&b[i]) {
            std::cmp::Ordering::Less => return Ordering::Less,
            std::cmp::Ordering::Greater => return Ordering::Greater,
            std::cmp::Ordering::Equal => {}
        }
    }
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Less => Ordering::Less,
        std::cmp::Ordering::Equal => Ordering::Equal,
        std::cmp::Ordering::Greater => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn folding_comparator_ignores_case() {
        let c = Comparator::CaseFolding;
        assert_eq!(c.compare(&units("TestVol"), &units("TestVol")), Ordering::Equal);
        assert_eq!(c.compare(&units("TestVol"), &units("testvol")), Ordering::Equal);
        assert_eq!(c.compare(&units("TESTVOL"), &units("testvol")), Ordering::Equal);
        assert_eq!(c.compare(&units("ReadMe"), &units("readme")), Ordering::Equal);
    }

    #[test]
    fn binary_comparator_preserves_case() {
        let c = Comparator::Binary;
        // Code unit order: 'T' (0x54) sorts before 't' (0x74).
        assert_eq!(c.compare(&units("TestVol"), &units("testvol")), Ordering::Less);
        assert_eq!(c.compare(&units("testvol"), &units("TestVol")), Ordering::Greater);
        assert_eq!(c.compare(&units("TestVol"), &units("TestVol")), Ordering::Equal);
    }

    #[test]
    fn folding_skips_the_sixteen_ignorable_characters() {
        // Every entry of IGNORABLE_CHARACTERS must genuinely fold to zero,
        // otherwise the documented set and the table disagree.
        for c in IGNORABLE_CHARACTERS {
            assert_eq!(fold(c), None, "U+{c:04X} must fold to zero");
        }
        // And they are skipped wherever they appear.
        for c in IGNORABLE_CHARACTERS {
            let mut trailing = units("cafe");
            trailing.push(c);
            assert_eq!(
                fast_unicode_compare(&trailing, &units("cafe")),
                Ordering::Equal,
                "U+{c:04X} must be skipped"
            );
            let mut interior = units("cafe");
            interior.insert(3, c);
            assert_eq!(
                fast_unicode_compare(&interior, &units("cafe")),
                Ordering::Equal,
                "U+{c:04X} must be skipped mid-string"
            );
        }
    }

    #[test]
    fn there_are_exactly_sixteen_ignorable_characters() {
        // Enumerated from Apple's gLowerCaseTable by finding every zero entry.
        // If this test ever fails, the generated table changed.
        let mut zeros = Vec::new();
        for (hb, sub) in G_LOWER_CASE_TABLE.iter().take(256).enumerate() {
            if *sub == 0 {
                continue;
            }
            let base = *sub as usize;
            for lb in 0..256usize {
                let idx = base + lb;
                if idx < G_LOWER_CASE_TABLE.len() && G_LOWER_CASE_TABLE[idx] == 0 {
                    zeros.push((hb * 256 + lb) as u16);
                }
            }
        }
        zeros.sort_unstable();
        assert_eq!(zeros, IGNORABLE_CHARACTERS.to_vec());
    }

    #[test]
    fn combining_marks_are_not_ignored_because_hfsplus_does_not_decompose() {
        // This is the correction that matters: HFS+ does not normalise names.
        // U+0301 COMBINING ACUTE ACCENT is not in the ignorable set, so the
        // decomposed spelling is a *different name* from the composed one.
        let mut decomposed = units("cafe");
        decomposed.push(0x0301);
        let composed = units("caf\u{e9}");
        assert_eq!(decomposed.len(), 5);
        assert_eq!(composed.len(), 4);
        assert_ne!(
            fast_unicode_compare(&decomposed, &composed),
            Ordering::Equal,
            "HFS+ must not treat a decomposed spelling as equal to the composed one"
        );
        assert!(fold(0x0301).is_some(), "U+0301 must fold to itself");
    }

    #[test]
    fn binary_compare_sees_ignorable_characters_too() {
        let mut with_zero_width = units("cafe");
        with_zero_width.push(0x200D);
        assert_ne!(
            unicode_binary_compare(&with_zero_width, &units("cafe")),
            Ordering::Equal,
            "binary comparison must see the extra code unit"
        );
        assert_eq!(unicode_binary_compare(&with_zero_width, &with_zero_width), Ordering::Equal);
    }

    #[test]
    fn empty_names_sort_first_and_compare_equal_to_each_other() {
        for c in [Comparator::CaseFolding, Comparator::Binary] {
            assert_eq!(c.compare(&[], &[]), Ordering::Equal);
            assert_eq!(c.compare(&[], &units("a")), Ordering::Less);
            assert_eq!(c.compare(&units("a"), &[]), Ordering::Greater);
            // A name made only of ignorable characters is equal to empty, but
            // only for the folding comparator.
            let only_ignorable = [0x200Du16];
            if c == Comparator::CaseFolding {
                assert_eq!(c.compare(&only_ignorable, &[]), Ordering::Equal);
            } else {
                assert_eq!(c.compare(&only_ignorable, &[]), Ordering::Greater);
            }
        }
    }

    #[test]
    fn ordering_is_lexicographic_not_length_first() {
        // "ab" vs "abc": the shorter is less even though "b" > "c" is false.
        assert_eq!(unicode_binary_compare(&units("ab"), &units("abc")), Ordering::Less);
        // A difference in the shared prefix wins over the length difference.
        assert_eq!(unicode_binary_compare(&units("az"), &units("ba")), Ordering::Less);
        assert_eq!(unicode_binary_compare(&units("b"), &units("ab")), Ordering::Greater);
    }

    #[test]
    fn folding_compares_lexicographically_after_folding() {
        let c = Comparator::CaseFolding;
        assert_eq!(c.compare(&units("apple"), &units("apples")), Ordering::Less);
        assert_eq!(c.compare(&units("apples"), &units("apple")), Ordering::Greater);
        assert_eq!(c.compare(&units("az"), &units("ba")), Ordering::Less);
    }

    #[test]
    fn ascii_folding_covers_the_basic_letters_only() {
        for (upper, lower) in [(0x0041u16, 0x0061u16), (0x005A, 0x007A), (0x0047, 0x0067)] {
            assert_eq!(fold(upper), Some(lower));
            assert_eq!(fast_unicode_compare(&[upper], &[lower]), Ordering::Equal);
        }
        // Digits and punctuation are untouched.
        for c in [0x0030u16, 0x0039, 0x0020, 0x002D, 0x007E] {
            assert_eq!(fold(c), Some(c));
        }
    }

    #[test]
    fn the_latin1_supplement_folds_only_four_letters() {
        // Enumerated from the generated table. gLatinCaseFold spans 0x00-0xFF but
        // only changes 34 entries: ASCII A-Z, plus the four Latin-1 letters that
        // have no precomposed upper/lower pair.
        for (upper, lower) in [
            (0x00C6u16, 0x00E6), // AE -> ae
            (0x00D0, 0x00F0),    // Eth -> eth
            (0x00D8, 0x00F8),    // O with stroke -> o with stroke
            (0x00DE, 0x00FE),    // Thorn -> thorn
        ] {
            assert_eq!(fold(upper), Some(lower), "U+{upper:04X} -> U+{lower:04X}");
        }

        // Every other accented Latin capital is identity. This is the finding that
        // most surprises people coming from Unicode expectations.
        for upper in [
            0x00C0u16, 0x00C1, 0x00C9, 0x00D1, 0x00DD, // A-grave, A-acute, E-grave,
            0x00DC, 0x00C7, 0x00D1, 0x00DA, 0x00DB,     // N-tilde, C-cedilla, U-ring, U-diaeresis
        ] {
            assert_eq!(fold(upper), Some(upper), "U+{upper:04X} must be identity");
        }
        assert_ne!(
            fast_unicode_compare(&[0x00C0], &[0x00E0]),
            Ordering::Equal,
            "A-grave must not match a-grave on an HFS+ volume"
        );
    }

    #[test]
    fn the_latin_table_has_no_ignorable_entries() {
        // Every ignorable character lives in the two-level table, not in
        // gLatinCaseFold. Asserted because it is the reason the Latin fast path
        // can `break` out of its loop without checking for a zero result.
        assert_eq!(G_LATIN_CASE_FOLD[0], 0xFFFF, "the sentinel mapping of U+0000");
        for (i, v) in G_LATIN_CASE_FOLD.iter().enumerate().skip(1) {
            assert_ne!(*v, 0, "U+{i:04X} must not fold to the ignore sentinel");
        }
    }

    #[test]
    fn greek_and_cyrillic_capitals_are_folded() {
        // Verified against the generated table.
        for (upper, lower) in [
            (0x0391u16, 0x03B1), // Greek capital alpha -> alpha
            (0x0392, 0x03B2),    // Beta
            (0x03A9, 0x03C9),    // Omega
            (0x0410, 0x0430),    // Cyrillic A -> a
            (0x042F, 0x044F),    // Cyrillic Ya -> ya
            (0x0531, 0x0561),    // Armenian A -> a
            (0x2160, 0x2170),    // Roman numeral I -> small i
            (0xFF21, 0xFF41),    // Fullwidth A -> fullwidth a
            (0x10A0, 0x10D0),    // Georgian capital -> small
            (0x0110, 0x0111),    // D with stroke -> d with stroke
        ] {
            assert_eq!(fold(upper), Some(lower), "U+{upper:04X} -> U+{lower:04X}");
            assert_eq!(fast_unicode_compare(&[upper], &[lower]), Ordering::Equal);
        }
    }

    #[test]
    fn characters_in_unmapped_blocks_pass_through() {
        // High byte 0x30 has a zero index in gLowerCaseTable, so nothing in
        // U+3000-U+30FF is folded or ignored.
        for c in [0x3000u16, 0x3042, 0x30AB] {
            assert_eq!(fold(c), Some(c), "U+{c:04X} must be identity");
        }
        // And they compare as themselves, so Japanese kana are order-sensitive.
        assert_ne!(
            fast_unicode_compare(&[0x30A2], &[0x30A1]),
            Ordering::Equal,
            "kana must not fold to each other"
        );
    }

    #[test]
    fn nul_is_not_treated_as_end_of_string() {
        // Apple maps U+0000 to 0xFFFF precisely so that zero can be the
        // sentinel. A NUL inside a name must compare as an ordinary character.
        assert_eq!(fold(0x0000), Some(0xFFFF));
        assert_eq!(fast_unicode_compare(&[0x0000], &[0xFFFF]), Ordering::Equal);
        assert_eq!(fast_unicode_compare(&[0x0000], &[]), Ordering::Greater);
    }

    #[test]
    fn supplementary_characters_compare_by_code_unit_not_code_point() {
        // U+1D400 (mathematical bold capital A) is the surrogate pair D835 DC00.
        let bold_a = [0xD835u16, 0xDC00];
        let plain_a = [0x0041u16];
        // Binary order is by code unit, so the surrogate pair is greater.
        assert_eq!(unicode_binary_compare(&bold_a, &plain_a), Ordering::Greater);
        // UTF-8 byte order would agree here, which is why the divergence needs a
        // specific example: U+E000 (private use) vs U+10000 (surrogate pair).
        let bmp = [0xE000u16];
        let supplementary = [0xD800u16, 0xDC00];
        assert_eq!(unicode_binary_compare(&bmp, &supplementary), Ordering::Greater);
        assert_eq!(
            unicode_binary_compare(&supplementary, &bmp),
            Ordering::Less,
            "code unit order puts D800 before E000, while code point order does not"
        );
    }

    #[test]
    fn comparator_selection_needs_both_hfsx_and_binary_key_compare_type() {
        // Plain HFS+: the keyCompareType is not consulted at all.
        assert_eq!(
            Comparator::for_volume(false, crate::catalog::key::K_HFS_BINARY_COMPARE),
            Comparator::CaseFolding
        );
        // HFSX with binary comparison: case sensitive.
        assert_eq!(
            Comparator::for_volume(true, crate::catalog::key::K_HFS_BINARY_COMPARE),
            Comparator::Binary
        );
        // HFSX that recorded case folding: still folds.
        assert_eq!(
            Comparator::for_volume(true, crate::catalog::key::K_HFS_CASE_FOLDING),
            Comparator::CaseFolding
        );
        // HFSX with an unrecognised value: folds, because it is not the
        // case-sensitive marker.
        assert_eq!(Comparator::for_volume(true, 0x00), Comparator::CaseFolding);
        assert_eq!(Comparator::for_volume(true, 0xFF), Comparator::CaseFolding);
    }

    #[test]
    fn ordering_then_composes_like_apples_comparator() {
        assert_eq!(Ordering::Equal.then(Ordering::Less), Ordering::Less);
        assert_eq!(Ordering::Less.then(Ordering::Equal), Ordering::Less);
        assert_eq!(Ordering::Greater.then(Ordering::Less), Ordering::Greater);
        assert_eq!(Ordering::Less.as_i32(), -1);
        assert_eq!(Ordering::Equal.as_i32(), 0);
        assert_eq!(Ordering::Greater.as_i32(), 1);
    }

    #[test]
    fn comparison_is_reflexive_over_a_range_of_names() {
        // Whatever the comparator, a name must equal itself. A wrong table entry
        // breaks this, which is why it is worth sweeping a range.
        let names = [
            "a", "A", "z", "Z", "0", "9", " ", "-", "~", "_", ".", "cafe", "café",
            "cafe\u{301}", "\u{00c0}", "\u{00e0}", "\u{0178}", "\u{00ff}",
            "\u{1d400}", "\u{65e5}\u{672c}", "\u{0301}",
        ];
        for c in [Comparator::CaseFolding, Comparator::Binary] {
            for n in names {
                let u = units(n);
                assert_eq!(c.compare(&u, &u), Ordering::Equal, "{c:?} {n:?}");
            }
        }
    }
}