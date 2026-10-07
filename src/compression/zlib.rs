//! A minimal inflate (RFC 1951) decoder with a zlib (RFC 1950) wrapper.
//!
//! This implements decompression of zlib-compressed decmpfs payloads. The
//! deflate format supports three block types: stored, fixed-Huffman, and
//! dynamic-Huffman. This decoder handles all three.
//!
//! Mining reference: Apple's decmpfs uses the standard C `zlib` library for
//! `CMP_TYPE_ZLIB`; the on-disk stream is a valid zlib stream (RFC 1950),
//! not raw deflate. See `bsd/sys/decmpfs.h`.

use crate::error::{Error, Result};
use std::sync::OnceLock;

/// A canonical Huffman decoding tree.
///
/// Built from code lengths per RFC 1951 Section 3.2.2. Each code is a
/// sequence of bits read MSB-first; the tree branches on each bit until
/// reaching a leaf node holding the symbol value.
#[derive(Debug, Clone)]
struct HuffmanTable {
    /// Binary tree of nodes. Index 0 is the root.
    nodes: Vec<Node>,
    max_len: usize,
}

impl HuffmanTable {
    fn build(lengths: &[u8]) -> Result<Self> {
        let max_len = lengths.iter().copied().max().unwrap_or(0) as usize;
        if max_len == 0 {
            return Err(Error::invalid(
                "huffman code lengths",
                "all lengths are zero",
            ));
        }

        // Compute bl_count[N] = number of codes with length N (excluding zero-length).
        // Per RFC 1951 Section 3.2.2, bl_count[0] must be zero.
        let mut bl_count = vec![0u32; max_len + 1];
        for &len in lengths {
            if len > 0 && (len as usize) <= max_len {
                bl_count[len as usize] += 1;
            }
        }

        let mut next_code = vec![0u32; max_len + 1];
        let mut code: u32 = 0;
        for bits in 1..=max_len {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Collect (code, code_length, symbol) in symbol order.
        let mut entries: Vec<(u32, usize, usize)> = Vec::new();
        for (sym, &len) in lengths.iter().enumerate() {
            if len == 0 || len as usize > max_len {
                continue;
            }
            let l = len as usize;
            entries.push((next_code[l], l, sym));
            next_code[l] += 1;
        }

        // Build the tree.
        let mut nodes: Vec<Node> = Vec::new();
        nodes.push(Node::Internal {
            zero: None,
            one: None,
        }); // root at index 0

        for (code_val, code_len, sym) in entries {
            let mut idx = 0usize;
            for bit_pos in (0..code_len).rev() {
                let bit = ((code_val >> bit_pos) & 1) as usize;

                // First, check if child exists (immutable borrow, released immediately).
                let existing = match &nodes[idx] {
                    Node::Internal { zero, one } => {
                        let slot = if bit == 0 { zero } else { one };
                        *slot
                    }
                    _ => unreachable!("expected internal node"),
                };

                if bit_pos == 0 {
                    // Leaf node.
                    let new_idx = nodes.len();
                    nodes.push(Node::Leaf(sym));
                    match &mut nodes[idx] {
                        Node::Internal { zero, one } => {
                            if bit == 0 {
                                *zero = Some(new_idx);
                            } else {
                                *one = Some(new_idx);
                            }
                        }
                        _ => unreachable!(),
                    }
                } else if let Some(child) = existing {
                    // Child already exists, descend.
                    idx = child;
                } else {
                    // Create new internal node and descend.
                    let new_idx = nodes.len();
                    nodes.push(Node::Internal {
                        zero: None,
                        one: None,
                    });
                    match &mut nodes[idx] {
                        Node::Internal { zero, one } => {
                            if bit == 0 {
                                *zero = Some(new_idx);
                            } else {
                                *one = Some(new_idx);
                            }
                        }
                        _ => unreachable!(),
                    }
                    idx = new_idx;
                }
            }
        }

        Ok(HuffmanTable { nodes, max_len })
    }

    fn decode_symbol(&self, reader: &mut BitReader) -> Result<usize> {
        let mut idx = 0usize;

        for _ in 0..self.max_len {
            match &self.nodes[idx] {
                Node::Leaf(sym) => return Ok(*sym),
                Node::Internal { zero, one } => {
                    let bit = reader.bit()?;
                    match (bit, zero, one) {
                        (0, Some(z), _) => idx = *z,
                        (1, _, Some(o)) => idx = *o,
                        _ => {
                            return Err(Error::invalid(
                                "huffman decode",
                                format!("no child for bit {bit} at node {idx}"),
                            ));
                        }
                    }
                }
            }
        }

        // After max_len bits, we should be at a leaf.
        match &self.nodes[idx] {
            Node::Leaf(sym) => Ok(*sym),
            Node::Internal { .. } => Err(Error::invalid(
                "huffman decode",
                format!("code exceeded max length {}", self.max_len),
            )),
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(usize),
    Internal {
        zero: Option<usize>,
        one: Option<usize>,
    },
}

// Length base values and extra bits (RFC 1951 Section 3.2.5).
const LENGTH_BASE: &[u16] = &[
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

const LENGTH_EXTRA: &[u8] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

// Distance base values and extra bits (RFC 1951 Section 3.2.5).
const DIST_BASE: &[u16] = &[
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

const DIST_EXTRA: &[u8] = &[
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

// Code length code order (RFC 1951 Section 3.2.7).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Decode a length value for a back-reference.
fn decode_length(reader: &mut BitReader, sym: usize) -> Result<usize> {
    let idx = sym
        .checked_sub(257)
        .ok_or_else(|| Error::invalid("length symbol", format!("symbol {sym} < 257")))?;

    if idx >= LENGTH_BASE.len() {
        return Err(Error::invalid(
            "length code",
            format!("symbol {sym} out of range"),
        ));
    }

    let base = LENGTH_BASE[idx] as usize;
    let extra_bits = LENGTH_EXTRA[idx] as usize;
    let extra = if extra_bits > 0 {
        reader.bits(extra_bits)? as usize
    } else {
        0
    };
    Ok(base + extra)
}

/// Decode a distance value for a back-reference.
fn decode_distance(reader: &mut BitReader, sym: usize) -> Result<usize> {
    if sym >= DIST_BASE.len() {
        return Err(Error::invalid(
            "distance code",
            format!("symbol {sym} out of range"),
        ));
    }

    let base = DIST_BASE[sym] as usize;
    let extra_bits = DIST_EXTRA[sym] as usize;
    let extra = if extra_bits > 0 {
        reader.bits(extra_bits)? as usize
    } else {
        0
    };
    Ok(base + extra)
}

/// Decode one deflate block using the given Huffman tables.
fn decode_block(
    reader: &mut BitReader,
    out: &mut Vec<u8>,
    lit_table: &HuffmanTable,
    dist_table: &HuffmanTable,
) -> Result<()> {
    loop {
        let sym = lit_table.decode_symbol(reader)?;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            // End of block marker.
            break;
        } else {
            // Length/distance pair: a back-reference.
            let length = decode_length(reader, sym)?;
            let dist_sym = dist_table.decode_symbol(reader)?;
            let dist = decode_distance(reader, dist_sym)?;

            if dist == 0 || dist > out.len() {
                return Err(Error::invalid(
                    "back-reference distance",
                    format!("distance {dist} exceeds output size {}", out.len()),
                ));
            }

            // DEFLATE back-references can overlap: a reference can point
            // into the region being written (RLE-style). Copy one byte
            // at a time in the overlap case.
            let start = out.len() - dist;
            let end = start + length;
            // Always copy byte-by-byte to handle overlap correctly.
            let mut i = start;
            while i < end {
                let byte = out[i];
                out.push(byte);
                i += 1;
            }
        }
    }
    Ok(())
}

/// Fixed Huffman literal/length code lengths (RFC 1951 Section 3.2.6).
fn fixed_literal_lengths() -> Vec<u8> {
    let mut lengths = vec![0u8; 288];
    for len in lengths[..144].iter_mut() {
        *len = 8;
    }
    for len in lengths[144..256].iter_mut() {
        *len = 9;
    }
    for len in lengths[256..280].iter_mut() {
        *len = 7;
    }
    for len in lengths[280..288].iter_mut() {
        *len = 8;
    }
    lengths
}

fn fixed_distance_lengths() -> Vec<u8> {
    vec![5u8; 30]
}

/// Cached fixed Huffman tables (built once on first use).
fn fixed_lit_table() -> &'static HuffmanTable {
    static TABLE: OnceLock<HuffmanTable> = OnceLock::new();
    TABLE.get_or_init(|| HuffmanTable::build(&fixed_literal_lengths()).expect("fixed table"))
}

fn fixed_dist_table() -> &'static HuffmanTable {
    static TABLE: OnceLock<HuffmanTable> = OnceLock::new();
    TABLE.get_or_init(|| HuffmanTable::build(&fixed_distance_lengths()).expect("fixed table"))
}

/// Decode a stored (uncompressed) deflate block (BTYPE=0).
fn inflate_stored(reader: &mut BitReader, out: &mut Vec<u8>) -> Result<()> {
    reader.align_byte();

    let len_lo = reader.byte()? as u16;
    let len_hi = reader.byte()? as u16;
    let len = len_lo | (len_hi << 8);

    let nlen_lo = reader.byte()? as u16;
    let nlen_hi = reader.byte()? as u16;
    let nlen = nlen_lo | (nlen_hi << 8);

    if nlen != !len {
        return Err(Error::invalid(
            "stored block length",
            format!("NLEN (0x{nlen:04x}) != complement of LEN (0x{len:04x})"),
        ));
    }

    for _ in 0..len {
        out.push(reader.byte()?);
    }

    Ok(())
}

/// Decode a fixed-Huffman deflate block (BTYPE=1).
fn inflate_fixed(reader: &mut BitReader, out: &mut Vec<u8>) -> Result<()> {
    let lit = fixed_lit_table().clone();
    let dist = fixed_dist_table().clone();
    decode_block(reader, out, &lit, &dist)
}

/// Decode a dynamic-Huffman deflate block (BTYPE=10).
fn inflate_dynamic(reader: &mut BitReader, out: &mut Vec<u8>) -> Result<()> {
    let hlit = 257 + reader.bits(5)? as usize;
    let hdist = 1 + reader.bits(5)? as usize;
    let hclen = 4 + reader.bits(4)? as usize;

    // Read code length code lengths in the CL_ORDER permutation.
    let mut cl_lengths = [0u8; 19];
    for &sym in CL_ORDER.iter().take(hclen) {
        cl_lengths[sym] = reader.bits(3)? as u8;
    }

    let cl_table = HuffmanTable::build(&cl_lengths)?;

    // Decode literal/length and distance code lengths using the CL table.
    let total = hlit + hdist;
    let mut all_lengths: Vec<u8> = Vec::with_capacity(total);

    while all_lengths.len() < total {
        let sym = cl_table.decode_symbol(reader)?;
        match sym {
            0..=15 => all_lengths.push(sym as u8),
            16 => {
                let repeat = 3 + reader.bits(2)? as usize;
                let prev = *all_lengths.last().ok_or(Error::invalid(
                    "code length repeat 16",
                    "no preceding length to repeat",
                ))?;
                let repeat = repeat.min(total.saturating_sub(all_lengths.len()));
                all_lengths.resize(all_lengths.len() + repeat, prev);
            }
            17 => {
                let repeat = 3 + reader.bits(3)? as usize;
                let repeat = repeat.min(total.saturating_sub(all_lengths.len()));
                all_lengths.resize(all_lengths.len() + repeat, 0);
            }
            18 => {
                let repeat = 11 + reader.bits(7)? as usize;
                let repeat = repeat.min(total.saturating_sub(all_lengths.len()));
                all_lengths.resize(all_lengths.len() + repeat, 0);
            }
            _ => {
                return Err(Error::invalid(
                    "code length symbol",
                    format!("invalid symbol {sym}"),
                ));
            }
        }
    }

    if all_lengths.len() != total {
        return Err(Error::Truncated {
            what: "dynamic huffman code lengths",
            needed: total,
            available: all_lengths.len(),
        });
    }

    let lit_table = HuffmanTable::build(&all_lengths[..hlit])?;
    let dist_table = HuffmanTable::build(&all_lengths[hlit..])?;

    decode_block(reader, out, &lit_table, &dist_table)
}

/// A bit reader for deflate data, reading LSB-first within each byte.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bit: 0,
        }
    }

    /// Read one bit, LSB-first within the current byte.
    fn bit(&mut self) -> Result<u32> {
        if self.pos >= self.data.len() {
            return Err(Error::Truncated {
                what: "deflate bitstream",
                needed: self.pos + 1,
                available: self.data.len(),
            });
        }
        let byte = self.data[self.pos];
        let val = ((byte >> self.bit) & 1) as u32;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(val)
    }

    /// Read `n` bits into a u32, LSB-first.
    fn bits(&mut self, n: usize) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 {
            return Err(Error::invalid("bit read", format!("{n} bits exceeds 32")));
        }
        let mut val: u32 = 0;
        for i in 0..n {
            let b = self.bit()?;
            val |= b << i;
        }
        Ok(val)
    }

    /// Read a full byte, first aligning to the next byte boundary.
    fn byte(&mut self) -> Result<u8> {
        self.align_byte();
        if self.pos >= self.data.len() {
            return Err(Error::Truncated {
                what: "zlib stream",
                needed: self.pos + 1,
                available: self.data.len(),
            });
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Discard remaining bits in the current partial byte.
    fn align_byte(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// Decode a zlib-wrapped deflate stream.
///
/// `data` must start with the 2-byte zlib header (CMF, FLG).
pub fn inflate(data: &[u8], expected_size: u64) -> Result<Vec<u8>> {
    if data.len() < 2 {
        return Err(Error::Truncated {
            what: "zlib stream",
            needed: 2,
            available: data.len(),
        });
    }

    let mut reader = BitReader::new(data);

    // Zlib header: 2 bytes.
    let cmf = reader.byte()?;
    let flg = reader.byte()?;

    // CMF low nibble must be 8 (deflate compression method).
    let cm = cmf & 0x0f;
    if cm != 8 {
        return Err(Error::invalid(
            "zlib compression method",
            format!("expected 8 (deflate), got {cm}"),
        ));
    }

    // Header check: (CMF*256 + FLG) % 31 == 0.
    if (u16::from_be_bytes([cmf, flg]) % 31) != 0 {
        return Err(Error::invalid(
            "zlib header",
            "header check failed: (CMF*256+FLG) % 31 != 0",
        ));
    }

    let _window_bits = (cmf >> 4) as usize;

    // Decode deflate blocks until BFINAL.
    let mut out = Vec::with_capacity(expected_size.min(1 << 20) as usize);

    loop {
        let bfinal = reader.bit()?;
        let btype = reader.bits(2)?;

        match btype {
            0 => inflate_stored(&mut reader, &mut out)?,
            1 => inflate_fixed(&mut reader, &mut out)?,
            2 => inflate_dynamic(&mut reader, &mut out)?,
            3 => {
                return Err(Error::invalid(
                    "deflate block type",
                    "reserved block type 3",
                ));
            }
            _ => unreachable!(),
        }

        if bfinal == 1 {
            break;
        }
    }

    // Adler32 checksum (4 bytes). Read to advance past it; not validated
    // since Apple's zlib always produces correct streams.
    for _ in 0..4 {
        let _ = reader.byte()?;
    }

    if out.len() != expected_size as usize {
        return Err(Error::invalid(
            "decompressed size",
            format!("expected {expected_size} bytes, got {}", out.len()),
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a zlib stream containing a single stored (uncompressed) block.
    fn zlib_stored(payload: &[u8]) -> Vec<u8> {
        // CMF=0x78 (deflate, window 32K), FLG=0x01 (check: 0x7801%31=0).
        let mut out = vec![0x78u8, 0x01];

        // One stored block, BFINAL=1, BTYPE=00: byte value 0x01.
        out.push(0x01);

        let len = payload.len() as u16;
        let nlen = !len;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(payload);

        // Adler32 of the payload.
        let adler = adler32(payload);
        out.extend_from_slice(&adler.to_be_bytes());
        out
    }

    fn adler32(data: &[u8]) -> u32 {
        let mut a: u32 = 1;
        let mut b: u32 = 0;
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    #[test]
    fn inflate_stored_block() {
        let payload = b"Hello, decmpfs!";
        let stream = zlib_stored(payload);
        let result = inflate(&stream, payload.len() as u64).unwrap();
        assert_eq!(result, payload);
    }

    #[test]
    fn inflate_stored_empty() {
        let stream = zlib_stored(&[]);
        let result = inflate(&stream, 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn inflate_stored_large() {
        let payload = vec![0xABu8; 10000];
        let stream = zlib_stored(&payload);
        let result = inflate(&stream, payload.len() as u64).unwrap();
        assert_eq!(result, payload);
    }

    #[test]
    fn inflate_stored_varied_pattern() {
        let payload: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        let stream = zlib_stored(&payload);
        let result = inflate(&stream, payload.len() as u64).unwrap();
        assert_eq!(result, payload);
    }

    #[test]
    fn inflate_rejects_bad_method() {
        let data = vec![0x75u8, 0x01]; // CM=5
        let err = inflate(&data, 0).unwrap_err();
        assert!(matches!(err, Error::InvalidField { .. }));
    }

    #[test]
    fn inflate_rejects_bad_header_check() {
        let data = vec![0x78u8, 0x00]; // 0x7800 % 31 != 0
        let err = inflate(&data, 0).unwrap_err();
        assert!(matches!(err, Error::InvalidField { .. }));
    }

    #[test]
    fn inflate_rejects_truncated() {
        let err = inflate(&[0x78], 0).unwrap_err();
        assert!(matches!(err, Error::Truncated { .. }));
    }

    #[test]
    fn huffman_table_rejects_all_zero() {
        let result = HuffmanTable::build(&[0, 0, 0]);
        assert!(result.is_err());
    }

    // --- Hand-encoded fixed-Huffman test cases ---

    /// Fixed Huffman: literal 'A' + end-of-block.
    /// Bit stream: BFINAL=1, BTYPE=01, sym 65 (8-bit code 0b01110001),
    /// sym 256 (7-bit code 0b0000000).
    #[test]
    fn inflate_fixed_huffman_single_literal() {
        let stream = vec![0x78, 0x01, 0x73, 0x04, 0x00, 0x00, 0x42, 0x00, 0x42];
        let result = inflate(&stream, 1).unwrap();
        assert_eq!(result, b"A");
    }

    // --- Real zlib streams from Python zlib.compress(level=9) ---
    // Exercises the dynamic Huffman path with back-references.

    const TEST_BACKREF: &[u8] = &[
        0x78, 0xda, 0x4b, 0x4c, 0x4a, 0x24, 0x01, 0x02, 0x00, 0x9b, 0xe7, 0x11, 0x86,
    ];
    const TEST_BACKREF_PAYLOAD: &[u8] = b"ababababababababababababababababababababababab";
    const TEST_BACKREF_LEN: u64 = 46;

    #[test]
    fn inflate_dynamic_backref() {
        let result = inflate(TEST_BACKREF, TEST_BACKREF_LEN).unwrap();
        assert_eq!(result, TEST_BACKREF_PAYLOAD);
    }

    const TEST_DYNAMIC: &[u8] = &[
        0x78, 0xda, 0x0b, 0xc9, 0x48, 0x55, 0x28, 0x2c, 0xcd, 0x4c, 0xce, 0x56, 0x48, 0x2a, 0xca,
        0x2f, 0xcf, 0x53, 0x48, 0xcb, 0xaf, 0x50, 0xc8, 0x2a, 0xcd, 0x2d, 0x28, 0x56, 0xc8, 0x2f,
        0x4b, 0x2d, 0x52, 0x28, 0x01, 0x4a, 0xe7, 0x24, 0x56, 0x55, 0x2a, 0xa4, 0xe4, 0xa7, 0xeb,
        0x29, 0x84, 0x8c, 0x2a, 0x1e, 0x7c, 0x8a, 0x01, 0xa4, 0xb3, 0xa1, 0x87,
    ];
    const TEST_DYNAMIC_PAYLOAD: &[u8] = b"The quick brown fox jumps over the lazy dog. ";
    const TEST_DYNAMIC_LEN: u64 = 450;

    #[test]
    fn inflate_dynamic_mixed_content() {
        let result = inflate(TEST_DYNAMIC, TEST_DYNAMIC_LEN).unwrap();
        assert_eq!(result.len(), 450);
        assert_eq!(&result[..45], TEST_DYNAMIC_PAYLOAD);
    }

    const TEST_RLE: &[u8] = &[
        0x78, 0xda, 0xed, 0xc1, 0x01, 0x0d, 0x00, 0x00, 0x00, 0xc2, 0xa0, 0x6c, 0xef, 0x5f, 0xca,
        0x1c, 0x6e, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0xbf, 0x01,
        0x87, 0xc1, 0xeb, 0x98,
    ];
    const TEST_RLE_LEN: u64 = 10000;

    #[test]
    fn inflate_dynamic_rle() {
        let result = inflate(TEST_RLE, TEST_RLE_LEN).unwrap();
        assert_eq!(result.len(), 10000);
        assert!(result.iter().all(|&b| b == b'A'));
    }
}
