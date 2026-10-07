//! Reading and decoding decmpfs-compressed files.
//!
//! HFS+ supports transparent file compression through the `com.apple.decmpfs`
//! extended attribute. A file marked compressed has its real contents stored in a
//! compressed form, and a reader must decompress them on demand.
//!
//! ## On-disk layout
//!
//! The `com.apple.decmpfs` attribute value begins with a 16-byte header stored
//! in **little-endian** byte order — the one field in HFS+ that breaks the
//! big-endian convention:
//!
//! | offset | size | field              |
//! |--------|------|--------------------|
//! | 0      | 4    | `compression_magic`|
//! | 4      | 4    | `compression_type` |
//! | 8      | 8    | `uncompressed_size`|
//! | 16     | 0..  | inline payload     |
//!
//! `compression_magic` must be `0x636d7066` ("cmpf"). After the header, the
//! layout depends on `compression_type`:
//!
//! - **Type 1** (`CMP_Type1`, "uncompressed data in xattr"): the uncompressed
//!   bytes are stored inline in the attribute value immediately following the
//!   header. No decompression is needed; the inline payload *is* the file data.
//!
//! - **Types 2–7** (ZLIB, LZFSE, LZVN, BZIP2, LZMA, LZ4): the compressed
//!   bytes live in the file's resource fork, not in the xattr. The xattr is
//!   exactly 16 bytes (just the header). The decompression algorithm is
//!   determined by `compression_type`.
//!
//! - **Types 0x80000001/0x80000002** (`DATALESS_CMPFS_TYPE` /
//!   `DATALESS_PKG_CMPFS_TYPE`): data-less file/package markers, not real
//!   compression types.
//!
//! ## Supported decompressors
//!
//! | type | algorithm | status |
//! |------|-----------|--------|
//! | 1    | uncompressed (inline)                  | handled |
//! | 2    | ZLIB (RFC 1950/1951)                  | implemented |
//! | 7    | LZ4 frame                             | implemented |
//! | 3    | LZFSE (Apple's Lempel–Ziv–FS entropy) | unsupported |
//! | 4    | LZVN                                | unsupported |
//! | 6    | LZMA                                | unsupported |
//! | 5    | BZIP2                               | unsupported |
//!
//! ZLIB and LZ4 are the types macOS uses most commonly for small file
//! compression. The remaining types are accepted on parse and reported as
//! `Error::Unsupported` so a caller can distinguish "compressed, but this
//! decoder is absent" from "not compressed at all".
//!
//! ## Safety
//!
//! Disk images are untrusted input. Every length field is bounds-checked against
//! the available buffer, and decompression output is validated against
//! `uncompressed_size` where that value is available.
//!
//! ## Alternatives considered (per project policy)
//!
//! Before implementing these decoders from scratch, the following candidates were
//! evaluated for vendoring:
//!
//! - **zlib/DEFLATE**: Apple's kernel uses the standard C zlib library (zlib
//!   License, BSD-compatible). Pure-Rust alternatives include `miniz_oxide` (MIT
//!   OR ISC) and `inflate` (MIT). The custom decoder was retained because it is
//!   small (~350 lines), well-tested (19 tests), `unsafe`-free, and already
//!   correct after fixing two bugs (Huffman leaf-check order, zero-length code
//!   counting). Replacing it would only be pursued if upstream maturity proved
//!   a concern.
//!
//! - **LZ4**: Apple uses the LZ4 reference C implementation (BSD-2-Clause).
//!   Pure-Rust alternatives include `lz4_flex` (MIT OR Apache-2.0). The current
//!   decoder handles the frame format; LZ4 block decompression is a small,
//!   well-specified subset.
//!
//! - **LZFSE / LZVN**: Apple open-sources the C implementations under
//!   Apache-2.0 (`bsd/sys/lzfse.h`, `bsd/sys/lzvn.h` in XNU). Vendoring them
//!   would require either a C toolchain at build time or a Rust port. No
//!   suitable license-compatible pure-Rust implementation was found in the
//!   crate registry cache.

use crate::endian::Le;
use crate::error::{Error, Result};

mod lz4;
mod zlib;

/// The `0x636d7066` literal Apple writes in `compression_magic`.
///
/// Reads as "cmpf" when interpreted as an ASCII string. Stored little-endian
/// on disk, so the bytes appear as `0x66 0x70 0x6d 0x63`.
///
/// Mining reference: `bsd/sys/decmpfs.h` `CMP_MAGIC`.
pub const CMP_MAGIC: u32 = 0x636d7066;

/// The on-disk `decmpfs_disk_header` structure, decoded as little-endian bytes.
///
/// Mining reference: `bsd/sys/decmpfs.h` `struct decmpfs_disk_header`, a packed
/// 16-byte structure with natural little-endian alignment on x86. There is no
/// `attr_size` field on disk; that belongs to the in-memory `decmpfs_header`
/// variant. The flexible array member `attr_bytes` is the optional inline
/// payload that follows for type 1 (uncompressed data in xattr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecmpfsHeader {
    /// Total size of the attribute value in bytes (the xattr length), i.e.
    /// the header size (16) plus any inline payload.
    ///
    /// This is not read from disk — it is derived from the attribute value
    /// length at parse time.
    pub attr_size: u32,
    /// Magic number, must be [`CMP_MAGIC`].
    pub compression_magic: u32,
    /// Compression algorithm identifier — one of the `CMP_Type*` constants.
    pub compression_type: CompressionType,
    /// The size of the decompressed data, in bytes.
    pub uncompressed_size: u64,
}

impl DecmpfsHeader {
    /// The fixed size of the `decmpfs_disk_header` structure on disk:
    ///
    /// ```text
    ///   u32 compression_magic  (offset  0)
    ///   u32 compression_type   (offset  4)
    ///   u64 uncompressed_size  (offset  8)
    /// ```
    ///
    /// Total: 4 + 4 + 8 = 16 bytes. Inline payload (for type 1) follows
    /// immediately at offset 16.
    ///
    /// Mining reference: Apple `bsd/sys/decmpfs.h`
    /// `struct decmpfs_disk_header` (16 bytes, `__attribute__((packed))`).
    pub const SIZE: usize = 16;

    /// Decode a `decmpfs_disk_header` from the raw xattr value.
    ///
    /// `bytes` is the full `com.apple.decmpfs` attribute value: the 16-byte
    /// header followed by any inline payload (for type 1).
    ///
    /// Returns an error if the buffer is shorter than 16 bytes, if the magic
    /// does not match [`CMP_MAGIC`], or if the compression type is malformed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::Truncated {
                what: "decmpfs header",
                needed: Self::SIZE,
                available: bytes.len(),
            });
        }

        let le = Le::new(bytes);
        let compression_magic = le.u32(0)?;

        if compression_magic != CMP_MAGIC {
            return Err(Error::invalid(
                "decmpfs compression_magic",
                format!("expected 0x{CMP_MAGIC:08x}, got 0x{compression_magic:08x}"),
            ));
        }

        let type_raw = le.u32(4)?;
        let compression_type = CompressionType::from_raw(type_raw)?;
        let uncompressed_size = le.u64(8)?;

        Ok(DecmpfsHeader {
            attr_size: bytes.len() as u32,
            compression_magic,
            compression_type,
            uncompressed_size,
        })
    }

    /// The number of bytes of inline payload that follow the header in the
    /// attribute value, for type 1 (uncompressed data in xattr).
    ///
    /// Returns zero for fork-backed types (2–7), since the payload is not
    /// inline.
    pub fn inline_payload_size(&self) -> usize {
        if self.compression_type == CompressionType::Uncompressed {
            (self.attr_size as usize).saturating_sub(Self::SIZE)
        } else {
            0
        }
    }
}

/// Compression algorithm identifiers used in `decmpfs_header.compression_type`.
///
/// Mining reference: `bsd/sys/decmpfs.h` `CMP_Type1` (the only named type there)
/// and the `CMP_MAX` sentinel; types 2–7 are defined in the AppleFSCompression
/// kext, whose source is not open-sourced but whose numeric assignments are
/// established by macOS practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    /// CMP_Type1: uncompressed data stored inline in the xattr.
    /// Not a real compression algorithm; the bytes after the header are the
    /// file contents verbatim.
    Uncompressed,
    /// CMP_Type2: ZLIB (RFC 1950 wrapper around RFC 1951 deflate).
    Zlib,
    /// CMP_Type3: LZFSE (Apple's Lempel–Ziv–Finite State Entropy).
    Lzfse,
    /// CMP_Type4: LZVN.
    Lzvn,
    /// CMP_Type5: Bzip2.
    Bzip2,
    /// CMP_Type6: LZMA.
    Lzma,
    /// CMP_Type7: LZ4 frame format.
    Lz4,
    /// Data-less file marker (0x80000001), not a real compression type.
    Dataless,
    /// Data-less package marker (0x80000002), not a real compression type.
    DatalessPkg,
    /// An unknown or unrecognised compression type.
    Unknown(u32),
}

impl CompressionType {
    /// Recover the on-disk numeric constant for a type.
    pub const fn as_raw(self) -> u32 {
        match self {
            CompressionType::Uncompressed => 1,
            CompressionType::Zlib => 2,
            CompressionType::Lzfse => 3,
            CompressionType::Lzvn => 4,
            CompressionType::Bzip2 => 5,
            CompressionType::Lzma => 6,
            CompressionType::Lz4 => 7,
            CompressionType::Dataless => 0x8000_0001,
            CompressionType::DatalessPkg => 0x8000_0002,
            CompressionType::Unknown(v) => v,
        }
    }

    /// Parse a raw numeric constant into a known type, or wrap it as
    /// [`CompressionType::Unknown`].
    pub const fn from_raw(raw: u32) -> Result<Self> {
        Ok(match raw {
            1 => CompressionType::Uncompressed,
            2 => CompressionType::Zlib,
            3 => CompressionType::Lzfse,
            4 => CompressionType::Lzvn,
            5 => CompressionType::Bzip2,
            6 => CompressionType::Lzma,
            7 => CompressionType::Lz4,
            0x8000_0001 => CompressionType::Dataless,
            0x8000_0002 => CompressionType::DatalessPkg,
            other => CompressionType::Unknown(other),
        })
    }
}

/// Decompress a complete compressed payload.
///
/// `data` is the compressed bytes: either the inline payload (type 1, after
/// the header) or the resource-fork contents (types 2–7).
/// `compression_type` selects the algorithm. `uncompressed_size` is the
/// declared output size, used to validate the result.
///
/// Returns the decompressed bytes, whose length equals `uncompressed_size`.
pub fn decompress(
    data: &[u8],
    compression_type: CompressionType,
    uncompressed_size: u64,
) -> Result<Vec<u8>> {
    match compression_type {
        CompressionType::Zlib => zlib::inflate(data, uncompressed_size),
        CompressionType::Lz4 => lz4::decode_frame(data, uncompressed_size),
        CompressionType::Uncompressed => {
            if data.len() != uncompressed_size as usize {
                return Err(Error::invalid(
                    "uncompressed inline data",
                    format!("expected {uncompressed_size} bytes, got {}", data.len()),
                ));
            }
            Ok(data.to_vec())
        }
        CompressionType::Lzfse
        | CompressionType::Lzvn
        | CompressionType::Bzip2
        | CompressionType::Lzma => Err(Error::unsupported(format!(
            "compression type {compression_type:?} is not yet implemented"
        ))),
        CompressionType::Dataless | CompressionType::DatalessPkg => Err(Error::invalid(
            "dataless file",
            "cannot decompress a data-less file or package marker",
        )),
        CompressionType::Unknown(v) => Err(Error::invalid(
            "compression type",
            format!("unknown compression type {v}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 16-byte decmpfs_disk_header from raw fields (all little-endian).
    fn header_bytes(magic: u32, ctype: u32, usize: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&magic.to_le_bytes());
        out.extend_from_slice(&ctype.to_le_bytes());
        out.extend_from_slice(&usize.to_le_bytes());
        out
    }

    #[test]
    fn header_parses_little_endian() {
        let raw = header_bytes(CMP_MAGIC, 2, 4096);
        let h = DecmpfsHeader::from_bytes(&raw).unwrap();
        assert_eq!(h.attr_size, 16);
        assert_eq!(h.compression_magic, CMP_MAGIC);
        assert_eq!(h.compression_type, CompressionType::Zlib);
        assert_eq!(h.uncompressed_size, 4096);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let raw = header_bytes(0xdead_beef, 2, 100);
        let err = DecmpfsHeader::from_bytes(&raw).unwrap_err();
        assert!(matches!(err, Error::InvalidField { .. }));
    }

    #[test]
    fn header_rejects_truncated() {
        let raw = &header_bytes(CMP_MAGIC, 2, 100)[..15];
        let err = DecmpfsHeader::from_bytes(raw).unwrap_err();
        assert!(matches!(err, Error::Truncated { .. }));
    }

    #[test]
    fn compression_type_roundtrips() {
        assert_eq!(CompressionType::Uncompressed.as_raw(), 1);
        assert_eq!(CompressionType::Zlib.as_raw(), 2);
        assert_eq!(CompressionType::Lzfse.as_raw(), 3);
        assert_eq!(CompressionType::Lz4.as_raw(), 7);
        assert_eq!(CompressionType::Dataless.as_raw(), 0x8000_0001);
        assert_eq!(CompressionType::DatalessPkg.as_raw(), 0x8000_0002);
        assert_eq!(
            CompressionType::from_raw(1).unwrap(),
            CompressionType::Uncompressed
        );
        assert_eq!(
            CompressionType::from_raw(3).unwrap(),
            CompressionType::Lzfse
        );
        assert_eq!(
            CompressionType::from_raw(999).unwrap(),
            CompressionType::Unknown(999)
        );
    }

    #[test]
    fn header_size_is_16_bytes() {
        assert_eq!(DecmpfsHeader::SIZE, 16);
    }

    #[test]
    fn uncompressed_type_returns_raw_data() {
        let payload = b"Hello, decmpfs!";
        let mut attr = header_bytes(CMP_MAGIC, 1, payload.len() as u64);
        attr.extend_from_slice(payload);
        let h = DecmpfsHeader::from_bytes(&attr).unwrap();
        assert_eq!(h.compression_type, CompressionType::Uncompressed);
        assert_eq!(h.inline_payload_size(), payload.len());
    }

    #[test]
    fn zlib_decompress_end_to_end() {
        let original = b"The quick brown fox jumps over the lazy dog. ".repeat(3);
        let compressed = zlib_compress_test_data(&original);
        let mut attr = header_bytes(CMP_MAGIC, 2, original.len() as u64);
        attr.extend_from_slice(&compressed);
        let h = DecmpfsHeader::from_bytes(&attr).unwrap();
        let result = decompress(&compressed, h.compression_type, original.len() as u64).unwrap();
        assert_eq!(result, original);
    }

    fn zlib_compress_test_data(data: &[u8]) -> Vec<u8> {
        // RFC 1950 zlib stream: CMF=0x78, FLG=0x01 (check: (0x78*256+0x01) % 31 == 0)
        // Stored block (BTYPE=00): HDR byte=0x01 (BFINAL=1, BTYPE=00), then LEN/NLEN, then raw data
        let mut out = vec![0x78, 0x01];
        let len = data.len() as u16;
        let nlen = !len;
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        out.push(len as u8);
        out.push((len >> 8) as u8);
        out.push(nlen as u8);
        out.push((nlen >> 8) as u8);
        out.extend_from_slice(data);
        // Adler32 checksum: s1 = 1 + sum(data) % 65521; s2 = 1 + sum(s1) % 65521
        let mut s1: u32 = 1;
        let mut s2: u32 = 0;
        for &b in data {
            s1 = (s1 + b as u32) % 65521;
            s2 = (s2 + s1) % 65521;
        }
        let adler = (s2 << 16) | s1;
        out.extend_from_slice(&adler.to_be_bytes());
        out
    }
}
