//! Big-endian decoding primitives and a bounds-checked cursor.
//!
//! Every multi-byte field in an HFS family on-disk structure is stored
//! big-endian ("network byte order"). Apple reaches for a set of `SWAP_BE16` /
//! `SWAP_BE32` / `SWAP_BE64` macros rather than `ntohs`/`htonl`, because the
//! HFS code has to run on both big- and little-endian machines without
//! conditional compilation.
//!
//! Mining reference: Apple `core/hfs_endian.h` defines the `SWAP_BE*` macro
//! family and `core/hfs_endian.c` implements the corresponding field-by-field
//! swap routines (`hfs_swap_HFSPlusVolumeHeader`,
//! `hfs_swap_HFSPlusForkData`, and so on). Those routines are the authoritative
//! statement of which fields are byte-swapped and of their widths; this module
//! provides the same widths as safe Rust accessors.
//!
//! This crate deliberately does **not** use `unsafe` pointer casts over image
//! buffers. Apple casts `char *` straight to `struct HFSPlusVolumeHeader *`,
//! which is safe for the kernel's own trusted buffers but unacceptable for a
//! userspace filesystem reading an untrusted image file. [`Be`] and [`Cursor`]
//! give the same field widths with an explicit, checked read for each one.

use crate::error::{Error, Result};

/// A big-endian byte slice that can be decoded field by field.
///
/// ```
/// use hfsplus::endian::Be;
/// let be = Be::new(&[0x48, 0x2b, 0x00, 0x04]);
/// assert_eq!(be.u16(0).unwrap(), 0x482b);
/// assert_eq!(be.u16(2).unwrap(), 0x0004);
/// assert!(be.u16(4).is_err());
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Be<'a> {
    bytes: &'a [u8],
}

/// The subset of `Be` that also supports 64-bit and slice reads.
impl<'a> Be<'a> {
    /// Wrap a byte slice.
    #[inline]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Be { bytes }
    }

    /// The underlying bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Total length in bytes.
    #[inline]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the slice is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Decode a big-endian `u16` at `off`.
    #[inline]
    pub fn u16(&self, off: usize) -> Result<u16> {
        let s = self.slice(off, 2, "be16")?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    /// Decode a big-endian `u32` at `off`.
    #[inline]
    pub fn u32(&self, off: usize) -> Result<u32> {
        let s = self.slice(off, 4, "be32")?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// Decode a big-endian `u64` at `off`.
    #[inline]
    pub fn u64(&self, off: usize) -> Result<u64> {
        let s = self.slice(off, 8, "be64")?;
        Ok(u64::from_be_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    /// Decode a big-endian `i16` at `off`.
    ///
    /// `iNodeNum`-style unions and mode fields are signed, so the signed
    /// accessors exist alongside the unsigned ones.
    #[inline]
    pub fn i16(&self, off: usize) -> Result<i16> {
        Ok(self.u16(off)? as i16)
    }

    /// Borrow `len` bytes at `off`.
    #[inline]
    pub fn slice(&self, off: usize, len: usize, what: &'static str) -> Result<&'a [u8]> {
        let end = off.checked_add(len).ok_or(Error::overflow(what))?;
        self.bytes.get(off..end).ok_or(Error::Truncated {
            what,
            needed: end,
            available: self.bytes.len(),
        })
    }

    /// Decode a `u16`-length-prefixed HFS Pascal string at `off`.
    ///
    /// Apple calls this family `HFSPlusStr*` and stores the length in the
    /// *first byte* of a two-byte field, so the on-disk encoding is one
    /// length byte followed by at most 255 content bytes.
    ///
    /// Mining reference: Apple `core/hfs_format.h`, `struct HFSPlusStr27` and
    /// friends, together with `core/hfs_endian.c` where `length8` is the high
    /// byte of a 16-bit big-endian field.
    pub fn pascal_string(&self, off: usize) -> Result<PascalString<'_>> {
        let len = self.u8(off)? as usize;
        let raw = self.slice(off + 1, len, "pascal")?;
        Ok(PascalString { bytes: raw })
    }

    /// Decode a single byte at `off`.
    #[inline]
    pub fn u8(&self, off: usize) -> Result<u8> {
        self.slice(off, 1, "be8").map(|s| s[0])
    }

    /// Decode an `i32` at `off`.
    #[inline]
    pub fn i32(&self, off: usize) -> Result<i32> {
        Ok(self.u32(off)? as i32)
    }
}

/// A borrowed HFS Pascal string: a length byte followed by raw bytes.
///
/// The bytes are *not* required to be UTF-8. HFS volumes legitimately contain
/// legacy Mac OS encodings, and the string must be preserved verbatim so that
/// a catalog record can be written back byte-for-byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PascalString<'a> {
    bytes: &'a [u8],
}

impl<'a> PascalString<'a> {
    /// Wrap already-unpacked bytes.
    pub const fn from_bytes(bytes: &'a [u8]) -> Self {
        PascalString { bytes }
    }

    /// The raw bytes, excluding the length byte.
    #[inline]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Byte length of the string.
    #[inline]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the string is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Lossy UTF-8 view, for diagnostics and display only.
    ///
    /// Never use this for catalog comparison: HFS name ordering is defined by
    /// the Unicode comparison rules in `core/UnicodeWrappers.c`, not by UTF-8
    /// byte order or `String::cmp`.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(self.bytes).into_owned()
    }
}

impl std::fmt::Display for PascalString<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string_lossy())
    }
}

/// A forward cursor with bounds checking, for sequential field decoding.
///
/// Using a cursor keeps the decoded field order in the source identical to the
/// on-disk order, which is what makes a structure auditable against Apple's
/// `hfs_endian.c` swap routine for the same structure.
///
/// Mining reference: Apple `core/hfs_endian.c` decodes each structure by
/// walking its fields in declaration order with `bswapU16_inc` /
/// `bswapU32_inc`-style helpers; [`Cursor`] mirrors that pattern.
#[derive(Clone, Debug)]
pub struct Cursor<'a> {
    be: Be<'a>,
    pos: usize,
    what: &'static str,
}

impl<'a> Cursor<'a> {
    /// Start a cursor at `off` within `bytes`.
    pub fn at(bytes: &'a [u8], off: usize, what: &'static str) -> Self {
        Cursor {
            be: Be::new(bytes),
            pos: off,
            what,
        }
    }

    /// Start a cursor at the beginning of `bytes`.
    pub fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Cursor::at(bytes, 0, what)
    }

    /// Current byte offset of the cursor.
    #[inline]
    pub const fn pos(&self) -> usize {
        self.pos
    }

    /// Number of bytes remaining from the cursor to the end of the buffer.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.be.len().saturating_sub(self.pos)
    }

    /// Advance without reading.
    #[inline]
    pub fn skip(&mut self, len: usize) -> Result<&'a [u8]> {
        let out = self.be.slice(self.pos, len, self.what)?;
        self.pos += len;
        Ok(out)
    }

    /// Read a `u8` and advance.
    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        let v = self.be.u8(self.pos)?;
        self.pos += 1;
        Ok(v)
    }

    /// Read a big-endian `u16` and advance.
    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        let v = self.be.u16(self.pos)?;
        self.pos += 2;
        Ok(v)
    }

    /// Read a big-endian `i16` and advance.
    #[inline]
    pub fn i16(&mut self) -> Result<i16> {
        let v = self.be.i16(self.pos)?;
        self.pos += 2;
        Ok(v)
    }

    /// Read a big-endian `u32` and advance.
    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        let v = self.be.u32(self.pos)?;
        self.pos += 4;
        Ok(v)
    }

    /// Read a big-endian `i32` and advance.
    #[inline]
    pub fn i32(&mut self) -> Result<i32> {
        let v = self.be.i32(self.pos)?;
        self.pos += 4;
        Ok(v)
    }

    /// Read a big-endian `u64` and advance.
    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        let v = self.be.u64(self.pos)?;
        self.pos += 8;
        Ok(v)
    }

    /// Read a fixed-size array and advance.
    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let s = self.skip(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        Ok(out)
    }

    /// Read an HFS Pascal string and advance.
    ///
    /// The on-disk form is a two-byte big-endian length split as
    /// `length8`/`length16` in Apple's `union HFSUniStr255`-adjacent types,
    /// but the `HFSPlusStr*` family stores the length in the *first* byte.
    pub fn pascal_string(&mut self) -> Result<PascalString<'a>> {
        let len = self.u8()? as usize;
        let bytes = self.skip(len)?;
        Ok(PascalString { bytes })
    }

    /// Borrow the next `len` bytes without decoding them.
    #[inline]
    pub fn peek_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        self.be.slice(self.pos, len, self.what)
    }
}

/// Read a big-endian `u16` from a slice with an explicit range check.
///
/// A convenience for tests and one-off field reads.
#[inline]
pub fn read_u16(bytes: &[u8], off: usize) -> Result<u16> {
    Be::new(bytes).u16(off)
}

/// Read a big-endian `u32` from a slice with an explicit range check.
#[inline]
pub fn read_u32(bytes: &[u8], off: usize) -> Result<u32> {
    Be::new(bytes).u32(off)
}

/// Read a big-endian `u64` from a slice with an explicit range check.
#[inline]
pub fn read_u64(bytes: &[u8], off: usize) -> Result<u64> {
    Be::new(bytes).u64(off)
}

/// A little-endian byte slice that can be decoded field by field.
///
/// This exists for the decmpfs compression header, which Apple stores in
/// **little-endian** byte order even though the surrounding HFS+ structures
/// (volume header, B-tree nodes, catalog records) are big-endian. Apple's
/// decmpfs lives in `bsd/sys/decmpfs.h`: `struct decmpfs_header` is defined
/// with natural alignment on x86, so on-disk it is little-endian.
///
/// The same safety rules apply as for [`Be`]: no `unsafe` pointer casts over
/// image bytes, no trusted length fields.
///
/// ```
/// use hfsplus::endian::Le;
/// let le = Le::new(&[0x78, 0x56, 0x34, 0x12]);
/// assert_eq!(le.u32(0).unwrap(), 0x12345678);
/// assert!(le.u32(4).is_err());
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Le<'a> {
    bytes: &'a [u8],
}

impl<'a> Le<'a> {
    /// Wrap a byte slice.
    #[inline]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Le { bytes }
    }

    /// The underlying bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Total length in bytes.
    #[inline]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the slice is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Decode a little-endian `u16` at `off`.
    #[inline]
    pub fn u16(&self, off: usize) -> Result<u16> {
        let s = self.slice(off, 2, "le16")?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    /// Decode a little-endian `u32` at `off`.
    #[inline]
    pub fn u32(&self, off: usize) -> Result<u32> {
        let s = self.slice(off, 4, "le32")?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// Decode a little-endian `u64` at `off`.
    #[inline]
    pub fn u64(&self, off: usize) -> Result<u64> {
        let s = self.slice(off, 8, "le64")?;
        Ok(u64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    /// Borrow `len` bytes at `off`.
    #[inline]
    pub fn slice(&self, off: usize, len: usize, what: &'static str) -> Result<&'a [u8]> {
        let end = off.checked_add(len).ok_or(Error::overflow(what))?;
        self.bytes.get(off..end).ok_or(Error::Truncated {
            what,
            needed: end,
            available: self.bytes.len(),
        })
    }

    /// Decode a single byte at `off`.
    #[inline]
    pub fn u8(&self, off: usize) -> Result<u8> {
        self.slice(off, 1, "le8").map(|s| s[0])
    }

    /// Decode a little-endian `i32` at `off`.
    #[inline]
    pub fn i32(&self, off: usize) -> Result<i32> {
        Ok(self.u32(off)? as i32)
    }
}

/// A forward cursor with bounds checking for sequential little-endian decoding.
#[derive(Clone, Debug)]
pub struct LeCursor<'a> {
    le: Le<'a>,
    pos: usize,
    what: &'static str,
}

impl<'a> LeCursor<'a> {
    /// Start a cursor at `off` within `bytes`.
    pub fn at(bytes: &'a [u8], off: usize, what: &'static str) -> Self {
        LeCursor {
            le: Le::new(bytes),
            pos: off,
            what,
        }
    }

    /// Start a cursor at the beginning of `bytes`.
    pub fn new(bytes: &'a [u8], what: &'static str) -> Self {
        LeCursor::at(bytes, 0, what)
    }

    /// Current byte offset of the cursor.
    #[inline]
    pub const fn pos(&self) -> usize {
        self.pos
    }

    /// Number of bytes remaining from the cursor to the end of the buffer.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.le.len().saturating_sub(self.pos)
    }

    /// Read a little-endian `u16` and advance.
    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        let v = self.le.u16(self.pos)?;
        self.pos += 2;
        Ok(v)
    }

    /// Read a little-endian `u32` and advance.
    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        let v = self.le.u32(self.pos)?;
        self.pos += 4;
        Ok(v)
    }

    /// Read a little-endian `i32` and advance.
    #[inline]
    pub fn i32(&mut self) -> Result<i32> {
        let v = self.le.i32(self.pos)?;
        self.pos += 4;
        Ok(v)
    }

    /// Read a little-endian `u64` and advance.
    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        let v = self.le.u64(self.pos)?;
        self.pos += 8;
        Ok(v)
    }

    /// Advance without reading.
    #[inline]
    pub fn skip(&mut self, len: usize) -> Result<&'a [u8]> {
        let out = self.le.slice(self.pos, len, self.what)?;
        self.pos += len;
        Ok(out)
    }

    /// Borrow the next `len` bytes without decoding them.
    #[inline]
    pub fn peek_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        self.le.slice(self.pos, len, self.what)
    }
}
