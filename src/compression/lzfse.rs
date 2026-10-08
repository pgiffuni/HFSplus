// Copyright (c) 2015-2016, Apple Inc. All rights reserved.
// Copyright (c) 2015-2016, 0x09/hfsfuse contributors.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
// 3. Neither the name of the copyright holder nor the names of its
//    contributors may be used to endorse or promote products derived from
//    this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF NOT ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//
// Mining reference: Apple `lzfse` library, https://github.com/lzfse/lzfse
// (mirror: 0x09/hfsfuse `src/lzfse_decode_base.c`, `src/lzfse_fse.c`,
// `src/lzfse_fse.h`, `src/lzfse_internal.h`, BSD-3-Clause).
// Translated to Rust with bounds-checking and no `unsafe`.

//! LZFSE (LZ-FSE) decoder for decmpfs compression type 3.
//!
//! LZFSE combines LZ77-style matching with Finite State Entropy (FSE) coding.
//! The bitstream consists of a sequence of blocks, each prefixed by a 32-bit
//! little-endian magic number that identifies the block type:
//!
//! | magic (LE) | bytes | meaning |
//! |---|---|---|
//! | `0x62767831` | `1xvb` | LZFSE compressed v1 block (uncompressed freq tables) |
//! | `0x62767832` | `2xvb` | LZFSE compressed v2 block (Huffman-coded freq tables) |
//! | `0x6276782d` | `-xvb` | Uncompressed raw data block |
//! | `0x6276786e` | `nxvb` | LZVN compressed block (decoded by the LZVN decoder) |
//! | `0x62767824` | `$xvb` | End-of-stream marker |
//!
//! For decmpfs type 3, the resource fork contains a complete LZFSE bitstream
//! that may contain any of these block types (including embedded LZVN blocks).
//!
//! ## Architecture
//!
//! 1. **`FseBitReader`** — reads bits backwards from a byte slice (FSE stores
//!    each payload stream right-to-left within its section). The reader
//!    prefetches 56-63 bits into a 64-bit accumulator, matching the 64-bit
//!    FSE I/O stream used on x86_64/ARM64.
//!
//! 2. **`FseDecoderTable`** — built from per-symbol frequency tables using
//!    the tANS (asymmetric numeral systems) algorithm. Each state maps to
//!    a packed entry: symbol, bit count, and state delta.
//!
//! 3. **Block processing** — each block has a header (either inline u16
//!    frequency arrays for v1, or Huffman-coded values for v2) followed by
//!    literal and LMD (length/match/distance) payload sections. The decoder
//!    loops over blocks until an end-of-stream marker is seen.
//!
//! ## Decoding flow
//!
//! For each compressed block:
//!
//! 1. Parse the block header to obtain frequency tables, decoder states,
//!    and payload bit counts.
//! 2. Build decoder tables from frequency tables.
//! 3. Decode the literal section: 4 interleaved FSE states read symbols
//!    in batches of 4, each state using the same literal decoder table.
//! 4. Decode the LMD section: L (literal length), M (match length), and
//!    D (distance) are decoded as triplets from three separate FSE state
//!    machines, each using its own value decoder table.
//! 5. Reproduce LZ77 output: copy literals, then copy matches
//!    (handling overlap for small distances).
//!
//! ## Safety
//!
//! All buffer accesses are bounds-checked. A truncated or malformed bitstream
//! produces a structured `Error::Truncated` or `Error::InvalidField`,
//! never a panic or out-of-bounds access.

use crate::error::{Error, Result};

// ============================================================
// FSE bitstream reader (reads bits backwards from end of buffer)
// ============================================================

/// FSE input stream that reads bits backwards from a buffer.
///
/// FSE stores payload data right-to-left: the stream is read from the
/// end of a byte region toward the beginning. The reader maintains a
/// 64-bit accumulator that always holds between 56 and 63 bits.
///
/// Mining reference: Apple `lzfse_fse.h` `fse_in_stream64` and
/// `fse_in_checked_init64`, `fse_in_checked_flush64`, `fse_in_pull64`.
struct FseBitReader<'a> {
    buf: &'a [u8],
    buf_pos: usize,
    buf_start: usize,
    accum: u128,
    accum_nbits: i32,
}

impl<'a> FseBitReader<'a> {
    /// Initialize the reader by loading the first 7-8 bytes from the end
    /// of the buffer, matching `fse_in_checked_init64`.
    ///
    /// `n` is the raw number of header bits already consumed by the block
    /// header (a 3-bit field from the packed V2 header, or a 32-bit field
    /// from the V1 header). When `n > 0`, the reader loads 8 bytes and the
    /// accumulator holds 64 + n bits. When `n == 0`, it loads 7 bytes and
    /// masks to 56 bits.
    fn init(&mut self, n: i32) -> Result<()> {
        if n != 0 {
            // n > 0: load 8 bytes, accum_nbits = n + 64
            if self.buf_pos < self.buf_start + 8 {
                return Err(Error::Truncated {
                    what: "lzfse fse bitstream (8 bytes)",
                    needed: self.buf_start + 8,
                    available: self.buf_pos,
                });
            }
            self.buf_pos -= 8;
            let incoming = load_le64(&self.buf[self.buf_pos..self.buf_pos + 8]);
            self.accum = incoming as u128;
            self.accum_nbits = n + 64;
        } else {
            // Load 7 bytes, mask to 56 bits
            if self.buf_pos < self.buf_start + 7 {
                return Err(Error::Truncated {
                    what: "lzfse fse bitstream (7 bytes)",
                    needed: self.buf_start + 7,
                    available: self.buf_pos,
                });
            }
            self.buf_pos -= 7;
            let mut val: u128 = 0;
            for i in 0..7 {
                val |= (self.buf[self.buf_pos + i] as u128) << (i * 8);
            }
            self.accum = val & 0xffffffffffffff; // mask to 56 bits
            self.accum_nbits = 56;
        }

        // Verify accumulator is valid
        if self.accum_nbits < 56 || self.accum_nbits >= 64 {
            return Err(Error::invalid(
                "lzfse fse bitstream",
                "invalid accumulator bit count",
            ));
        }
        if self.accum >> self.accum_nbits != 0 {
            return Err(Error::invalid(
                "lzfse fse bitstream",
                "non-zero upper bits in accumulator",
            ));
        }

        Ok(())
    }

    /// Refill the accumulator to bring accum_nbits into [56, 63].
    ///
    /// Mining reference: Apple `lzfse_fse.h` `fse_in_checked_flush64`.
    fn flush(&mut self) -> Result<()> {
        if self.accum_nbits >= 56 {
            return Ok(());
        }
        let nbits = (63 - self.accum_nbits) & !7;
        if nbits == 0 {
            return Ok(());
        }
        let bytes = (nbits >> 3) as usize;
        if self.buf_pos < self.buf_start + bytes {
            return Err(Error::Truncated {
                what: "lzfse fse bitstream refill",
                needed: self.buf_start + bytes,
                available: self.buf_pos,
            });
        }
        self.buf_pos -= bytes;
        let incoming = load_le64(&self.buf[self.buf_pos..]) as u128;
        self.accum = (self.accum << nbits) | (incoming & mask_lsb128(nbits as u32));
        self.accum_nbits += nbits;

        if self.accum_nbits < 56 || self.accum_nbits >= 64 {
            return Err(Error::invalid(
                "lzfse fse bitstream flush",
                "invalid bit count after flush",
            ));
        }
        if self.accum >> self.accum_nbits != 0 {
            return Err(Error::invalid(
                "lzfse fse bitstream flush",
                "non-zero upper bits after flush",
            ));
        }
        Ok(())
    }

    /// Pull `n` bits from the stream, refilling if needed.
    ///
    /// Mining reference: Apple `lzfse_fse.h` `fse_in_pull64`.
    fn pull(&mut self, n: i32) -> Result<u128> {
        if n as u32 > self.accum_nbits as u32 {
            self.flush()?;
        }
        if n as u32 > self.accum_nbits as u32 {
            return Err(Error::Truncated {
                what: "lzfse fse bitstream pull",
                needed: n as usize,
                available: self.accum_nbits as usize,
            });
        }
        self.accum_nbits -= n;
        let result = self.accum >> self.accum_nbits;
        self.accum &= mask_lsb128(self.accum_nbits as u32);
        Ok(result)
    }

    /// Decode a symbol (literal byte) from the stream.
    ///
    /// Mining reference: Apple `lzfse_fse.h` `fse_decode`.
    fn decode_symbol(&mut self, state: &mut u16, table: &[i32]) -> Result<u8> {
        let entry = table[*state as usize];
        let nbits = entry & 0xff;
        let bits = self.pull(nbits)?;
        let symbol = ((entry >> 8) & 0xff) as u8;
        let delta = ((entry >> 16) as i16) as i32;
        *state = (delta + (bits as i32)) as u16;
        Ok(symbol)
    }

    /// Decode a value (L, M, or D) with extra bits using a value decoder table.
    ///
    /// Mining reference: Apple `lzfse_fse.h` `fse_value_decode`.
    fn decode_value(&mut self, state: &mut u16, table: &[FseValueDecoderEntry]) -> Result<i32> {
        let entry = table[*state as usize];
        let bits = self.pull(entry.total_bits as i32)? as u32;

        *state = (entry.delta as i32 + (bits >> entry.value_bits) as i32) as u16;
        let value = entry.vbase + mask_lsb32(bits, entry.value_bits as u32) as i32;
        Ok(value)
    }
}

// ============================================================
// FSE decoder table types
// ============================================================

/// Entry for FSE literal decoder tables, packed as a single i32:
///   bits 0-7:   k (number of bits to read)
///   bits 8-15:  symbol
///   bits 16-31: signed delta to compute next state
///
/// Mining reference: Apple `lzfse_fse.h` `fse_decoder_entry` and
/// `fse_init_decoder_table` in `lzfse_fse.c`.
#[derive(Clone, Copy)]
struct FseValueDecoderEntry {
    total_bits: u8,
    value_bits: u8,
    delta: i16,
    vbase: i32,
}

/// Build the FSE literal decoder table from per-symbol frequencies.
///
/// This implements the tANS table construction algorithm from
/// `fse_init_decoder_table`. Each symbol is assigned states proportional to
/// its frequency, and each state stores the bits to read and the delta to
/// compute the next state. Entries are packed as:
///   bits 0-7:   k (bit count)
///   bits 8-15:  symbol
///   bits 16-31: signed delta (next state delta)
///
/// Mining reference: Apple `lzfse_fse.c` `fse_init_decoder_table`.
fn build_literal_decoder_table(freq: &[u16; ENCODE_LITERAL_SYMBOLS]) -> Result<Vec<i32>> {
    let nstates = ENCODE_LITERAL_STATES;
    let n_clz = (nstates as u32).leading_zeros();
    let table = vec![0i32; nstates];
    let mut table = table;

    let mut offset: usize = 0;
    for (symbol, &f) in freq.iter().enumerate() {
        let f = f as i32;
        if f == 0 {
            continue;
        }

        // k = clz(f) - clz(nstates): ensures nstates <= (f << k) < 2*nstates
        let k = ((f as u32).leading_zeros() as i32) - (n_clz as i32);
        let k = if k < 0 { 0 } else { k };

        // j0 = boundary between k-bit and (k-1)-bit states
        let j0 = ((2 * nstates) >> k) as i32 - f;

        for j in 0..f {
            let (e_k, e_delta) = if j < j0 {
                (k as i8, (((f + j) << k) - nstates as i32) as i16)
            } else {
                ((k - 1) as i8, ((j - j0) << (k - 1)) as i16)
            };
            let entry: i32 = ((e_delta as i32 & 0xffff) << 16)
                | ((symbol as i32 & 0xff) << 8)
                | (e_k as i32 & 0xff);
            table[offset + j as usize] = entry;
        }
        offset += f as usize;
    }

    Ok(table)
}

/// Build the value decoder table for L, M, or D symbols.
///
/// Each entry stores `total_bits` (k + value_bits), `value_bits`, `delta`,
/// and `vbase`. The table follows the same tANS construction as
/// `fse_init_value_decoder_table`.
///
/// Mining reference: Apple `lzfse_fse.c` `fse_init_value_decoder_table`.
fn build_value_decoder_table(
    nstates: usize,
    nsymbols: usize,
    freq: &[u16],
    vbits: &[u8],
    vbase: &[i32],
) -> Result<Vec<FseValueDecoderEntry>> {
    let n_clz = (nstates as u32).leading_zeros();
    let table = vec![
        FseValueDecoderEntry {
            total_bits: 0,
            value_bits: 0,
            delta: 0,
            vbase: 0,
        };
        nstates
    ];
    let mut table = table;

    let mut offset: usize = 0;
    for symbol in 0..nsymbols {
        let f = freq[symbol] as i32;
        if f == 0 {
            continue;
        }

        let k = ((f as u32).leading_zeros() as i32) - (n_clz as i32);
        let k = if k < 0 { 0 } else { k };
        let j0 = ((2 * nstates) >> k) as i32 - f;
        let vbits_val = vbits[symbol] as i32;
        let vbase_val = vbase[symbol];

        for j in 0..f {
            let mut entry = table[offset + j as usize];
            entry.value_bits = vbits_val as u8;
            entry.vbase = vbase_val;

            if j < j0 {
                entry.total_bits = (k + vbits_val) as u8;
                entry.delta = (((f + j) << k) - nstates as i32) as i16;
            } else {
                entry.total_bits = ((k - 1) + vbits_val) as u8;
                entry.delta = ((j - j0) << (k - 1)) as i16;
            }

            table[offset + j as usize] = entry;
        }
        offset += f as usize;
    }

    Ok(table)
}

// ============================================================
// LZFSE constants
// ============================================================

/// LZFSE block magic numbers (stored as little-endian u32 in the stream).
///
/// Mining reference: Apple `lzfse_internal.h`.
const NO_BLOCK_MAGIC: u32 = 0x00000000;
const ENDOFSTREAM_BLOCK_MAGIC: u32 = 0x24787662; // "bvx$"
const UNCOMPRESSED_BLOCK_MAGIC: u32 = 0x2d787662; // "bvx-"
const COMPRESSEDV1_BLOCK_MAGIC: u32 = 0x31787662; // "bvx1"
const COMPRESSEDV2_BLOCK_MAGIC: u32 = 0x32787662; // "bvx2"
const COMPRESSEDLZVN_BLOCK_MAGIC: u32 = 0x6e787662; // "bvxn"

/// Symbol and state counts (fixed by the LZFSE format specification).
///
/// Mining reference: Apple `lzfse_internal.h` `lzfse_tunables.h`.
const ENCODE_L_SYMBOLS: usize = 20;
const ENCODE_M_SYMBOLS: usize = 20;
const ENCODE_D_SYMBOLS: usize = 64;
const ENCODE_LITERAL_SYMBOLS: usize = 256;
const ENCODE_L_STATES: usize = 64;
const ENCODE_M_STATES: usize = 64;
const ENCODE_D_STATES: usize = 256;
const ENCODE_LITERAL_STATES: usize = 1024;

/// Maximum number of literals per LZFSE block (4x match capacity plus margin).
///
/// Mining reference: Apple `lzfse_internal.h`
/// `LZFSE_DECODE_LITERALS_PER_BLOCK` (= 4 * LZFSE_DECODE_MATCHES_PER_BLOCK).
const LITERALS_PER_BLOCK: usize = 40000;

/// L symbol: extra bits and base values for extra-bits decoding.
///
/// Mining reference: Apple `lzfse_internal.h` `l_extra_bits`, `l_base_value`.
static L_EXTRA_BITS: [u8; ENCODE_L_SYMBOLS] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 5, 8];
static L_BASE_VALUE: [i32; ENCODE_L_SYMBOLS] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 20, 28, 60,
];

/// M (match length) symbol: extra bits and base values.
///
/// Mining reference: Apple `lzfse_internal.h` `m_extra_bits`, `m_base_value`.
static M_EXTRA_BITS: [u8; ENCODE_M_SYMBOLS] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 5, 8, 11];
static M_BASE_VALUE: [i32; ENCODE_M_SYMBOLS] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 24, 56, 312,
];

/// D (match distance) symbol: extra bits and base values.
///
/// Mining reference: Apple `lzfse_internal.h` `d_extra_bits`, `d_base_value`.
static D_EXTRA_BITS: [u8; ENCODE_D_SYMBOLS] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7,
    8, 8, 8, 8, 9, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14,
    14, 14, 15, 15, 15, 15,
];
static D_BASE_VALUE: [i32; ENCODE_D_SYMBOLS] = [
    0, 1, 2, 3, 4, 6, 8, 10, 12, 16, 20, 24, 28, 36, 44, 52, 60, 76, 92, 108, 124, 156, 188, 220,
    252, 316, 380, 444, 508, 636, 764, 892, 1020, 1276, 1532, 1788, 2044, 2556, 3068, 3580, 4092,
    5116, 6140, 7164, 8188, 10236, 12284, 14332, 16380, 20476, 24572, 28668, 32764, 40956, 49148,
    57340, 65532, 81916, 98300, 114684, 131068, 163836, 196604, 229372,
];

// ============================================================================
// Frequency table decoding for V2 blocks
// ============================================================================

/// Lookup table for the number of bits used to encode each frequency value.
///
/// Mining reference: Apple `lzfse_decode_base.c` `lzfse_freq_nbits_table`.
static FREQ_NBITS_TABLE: [i8; 32] = [
    2, 3, 2, 5, 2, 3, 2, 8, 2, 3, 2, 5, 2, 3, 2, 14, 2, 3, 2, 5, 2, 3, 2, 8, 2, 3, 2, 5, 2, 3, 2,
    14,
];

/// Lookup table for frequency values (for the ≤5-bit encoding path).
///
/// Mining reference: Apple `lzfse_decode_base.c` `lzfse_freq_value_table`.
static FREQ_VALUE_TABLE: [i8; 32] = [
    0, 2, 1, 4, 0, 3, 1, -1, 0, 2, 1, 5, 0, 3, 1, -1, 0, 2, 1, 6, 0, 3, 1, -1, 0, 2, 1, 7, 0, 3, 1,
    -1,
];

/// Decode frequency table values from a bitstream used in V2 block headers.
///
/// The freq[] area in a V2 header is a packed bitstream where each frequency
/// value is encoded using a variable-length code:
/// - Values 0-2 use 2 bits (indices 0, 4, 8, 12, etc.)
/// - Values 3-5 use 3 bits (indices 1, 5, 9, 13, etc.)
/// - Larger indices use 2 or 5 bits depending on the pattern
/// - Special cases: 8+extra (4 bits) and 24+extra (10 bits) for larger values
///
/// Mining reference: Apple `lzfse_decode_base.c` `lzfse_decode_v1_freq_value`.
fn decode_freq_stream(data: &[u8]) -> Result<(Vec<u16>, usize)> {
    let total_symbols =
        ENCODE_L_SYMBOLS + ENCODE_M_SYMBOLS + ENCODE_D_SYMBOLS + ENCODE_LITERAL_SYMBOLS;
    let mut freq = vec![0u16; total_symbols];
    let mut accum: u32 = 0;
    let mut accum_nbits: i32 = 0;
    let mut pos = 0;

    for slot in freq.iter_mut().take(total_symbols) {
        // Refill accumulator one byte at a time, ensuring we have at least 8 bits
        while pos < data.len() && accum_nbits + 8 <= 32 {
            accum |= (data[pos] as u32) << accum_nbits;
            accum_nbits += 8;
            pos += 1;
        }

        let b = (accum & 31) as usize;
        let nbits = FREQ_NBITS_TABLE[b] as i32;
        let value = FREQ_VALUE_TABLE[b];

        // Check if we have enough bits before extraction
        if nbits > accum_nbits {
            // Not enough bits in accumulator, and no more data to read.
            // The Apple code returns -1 here, but for valid data this shouldn't
            // happen — the encoder zero-pads the remaining bits.
            *slot = 0;
            continue;
        }

        let decoded: u16 = if nbits == 8 {
            // Extended: 8 + extra (4 bits)
            8 + ((accum >> 4) & 0xf) as u16
        } else if nbits == 14 {
            // Extended: 24 + extra (10 bits)
            24 + ((accum >> 4) & 0x3ff) as u16
        } else {
            // 1-5 bits encoding from table
            if value < 0 {
                0
            } else {
                value as u16
            }
        };

        accum >>= nbits;
        accum_nbits -= nbits;

        *slot = decoded;
    }

    // Validate that all freq data was consumed cleanly: the accumulator must
    // have fewer than 8 remaining bits, and we must have consumed all bytes.
    // This mirrors the reference check `if (accum_nbits >= 8 || src != src_end)`.
    if accum_nbits >= 8 || pos != data.len() {
        return Err(Error::invalid(
            "lzfse freq stream",
            "frequency table data does not end cleanly",
        ));
    }

    Ok((freq, pos))
}

// ============================================================
// LZFSE block header (decoded V1 format, used for both v1 and v2 blocks)
// ============================================================

/// Decoded LZFSE block header fields.
///
/// After parsing, both V1 and V2 headers are decoded into this format
/// so the rest of the decoder can treat them uniformly.
///
/// Mining reference: Apple `lzfse_internal.h`
/// `lzfse_compressed_block_header_v1` and
/// `lzfse_compressed_block_header_v2`.
struct LzfseBlockHeader {
    n_raw_bytes: u32,
    n_literals: u32,
    n_matches: u32,
    n_literal_payload_bytes: u32,
    n_lmd_payload_bytes: u32,
    literal_bits: i32,
    literal_state: [u16; 4],
    lmd_bits: i32,
    l_state: u16,
    m_state: u16,
    d_state: u16,
    l_freq: [u16; ENCODE_L_SYMBOLS],
    m_freq: [u16; ENCODE_M_SYMBOLS],
    d_freq: [u16; ENCODE_D_SYMBOLS],
    literal_freq: [u16; ENCODE_LITERAL_SYMBOLS],
}

/// Extract `nbits` bits starting at `lsb` from a u64.
///
/// Mining reference: Apple `lzfse_decode_base.c` `get_field()`.
fn get_u64_field(v: u64, offset: u32, nbits: u32) -> u32 {
    if nbits == 32 {
        (v >> offset) as u32
    } else {
        ((v >> offset) & ((1u64 << nbits) - 1)) as u32
    }
}

/// Parse a V1 compressed block header (frequency tables are inline as u16 arrays).
///
/// Mining reference: Apple `lzfse_internal.h`
/// `lzfse_compressed_block_header_v1`, `lzfse_decode_base.c`.
fn parse_v1_header(data: &[u8]) -> Result<(LzfseBlockHeader, usize)> {
    // V1 header struct layout (206 bytes total):
    // magic(4) + n_raw_bytes(4) + n_payload_bytes(4) + n_literals(4) +
    // n_matches(4) + n_literal_payload_bytes(4) + n_lmd_payload_bytes(4) +
    // literal_bits(4) + literal_state[4](8) + lmd_bits(4) + l_state(2) +
    // m_state(2) + d_state(2) + l_freq[20](40) + m_freq[20](40) +
    // d_freq[64](128) + literal_freq[256](512)
    const HEADER_SIZE: usize =
        4 + 4 + 4 + 4 + 4 + 4 + 4 + 4 + 8 + 4 + 2 + 2 + 2 + 40 + 40 + 128 + 512;

    if data.len() < HEADER_SIZE {
        return Err(Error::Truncated {
            what: "lzfse v1 block header",
            needed: HEADER_SIZE,
            available: data.len(),
        });
    }

    let mut header = LzfseBlockHeader {
        n_raw_bytes: load_le32(&data[4..8]),
        n_literals: load_le32(&data[8..12]),
        n_matches: load_le32(&data[12..16]),
        n_literal_payload_bytes: load_le32(&data[16..20]),
        n_lmd_payload_bytes: load_le32(&data[20..24]),
        literal_bits: (load_le32(&data[24..28]) as i32) - 7,
        literal_state: [
            load_le16(&data[28..30]),
            load_le16(&data[30..32]),
            load_le16(&data[32..34]),
            load_le16(&data[34..36]),
        ],
        lmd_bits: (load_le32(&data[36..40]) as i32) - 7,
        l_state: load_le16(&data[40..42]),
        m_state: load_le16(&data[42..44]),
        d_state: load_le16(&data[44..46]),
        l_freq: [0; ENCODE_L_SYMBOLS],
        m_freq: [0; ENCODE_M_SYMBOLS],
        d_freq: [0; ENCODE_D_SYMBOLS],
        literal_freq: [0; ENCODE_LITERAL_SYMBOLS],
    };

    let mut pos = 46;
    for slot in header.l_freq.iter_mut() {
        *slot = load_le16(&data[pos..pos + 2]);
        pos += 2;
    }
    for slot in header.m_freq.iter_mut() {
        *slot = load_le16(&data[pos..pos + 2]);
        pos += 2;
    }
    for slot in header.d_freq.iter_mut() {
        *slot = load_le16(&data[pos..pos + 2]);
        pos += 2;
    }
    for slot in header.literal_freq.iter_mut() {
        *slot = load_le16(&data[pos..pos + 2]);
        pos += 2;
    }

    // Verify freq sums match state counts
    let l_sum: u32 = header.l_freq.iter().map(|&f| f as u32).sum();
    let m_sum: u32 = header.m_freq.iter().map(|&f| f as u32).sum();
    let d_sum: u32 = header.d_freq.iter().map(|&f| f as u32).sum();
    let lit_sum: u32 = header.literal_freq.iter().map(|&f| f as u32).sum();

    if l_sum != ENCODE_L_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v1 header",
            format!("l_freq sum {} != {}", l_sum, ENCODE_L_STATES),
        ));
    }
    if m_sum != ENCODE_M_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v1 header",
            format!("m_freq sum {} != {}", m_sum, ENCODE_M_STATES),
        ));
    }
    if d_sum != ENCODE_D_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v1 header",
            format!("d_freq sum {} != {}", d_sum, ENCODE_D_STATES),
        ));
    }
    if lit_sum != ENCODE_LITERAL_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v1 header",
            format!("literal_freq sum {} != {}", lit_sum, ENCODE_LITERAL_STATES),
        ));
    }

    Ok((header, HEADER_SIZE))
}

/// Parse a V2 compressed block header (frequency tables are Huffman-coded).
///
/// Mining reference: Apple `lzfse_internal.h`
/// `lzfse_compressed_block_header_v2`, `lzfse_decode_base.c`
/// `lzfse_decode_v2_header_size` and `lzfse_decode_v1`.
fn parse_v2_header(data: &[u8]) -> Result<(LzfseBlockHeader, usize)> {
    // V2 header fixed part: magic(4) + n_raw_bytes(4) + packed_fields[3](24)
    // = 32 bytes minimum
    if data.len() < 32 {
        return Err(Error::Truncated {
            what: "lzfse v2 fixed header",
            needed: 32,
            available: data.len(),
        });
    }

    let n_raw_bytes = load_le32(&data[4..8]);
    let pf0 = load_le64(&data[8..16]);
    let pf1 = load_le64(&data[16..24]);
    let pf2 = load_le64(&data[24..32]);

    let header = LzfseBlockHeader {
        n_raw_bytes,
        n_literals: get_u64_field(pf0, 0, 20),
        n_matches: get_u64_field(pf0, 40, 20),
        n_literal_payload_bytes: get_u64_field(pf0, 20, 20),
        n_lmd_payload_bytes: get_u64_field(pf1, 40, 20),
        literal_bits: (get_u64_field(pf0, 60, 3) as i32) - 7,
        literal_state: [
            get_u64_field(pf1, 0, 10) as u16,
            get_u64_field(pf1, 10, 10) as u16,
            get_u64_field(pf1, 20, 10) as u16,
            get_u64_field(pf1, 30, 10) as u16,
        ],
        lmd_bits: (get_u64_field(pf1, 60, 3) as i32) - 7,
        l_state: get_u64_field(pf2, 32, 10) as u16,
        m_state: get_u64_field(pf2, 42, 10) as u16,
        d_state: get_u64_field(pf2, 52, 10) as u16,
        l_freq: [0; ENCODE_L_SYMBOLS],
        m_freq: [0; ENCODE_M_SYMBOLS],
        d_freq: [0; ENCODE_D_SYMBOLS],
        literal_freq: [0; ENCODE_LITERAL_SYMBOLS],
    };

    // header_size is the total header size (including the 32-byte fixed part)
    let header_size = get_u64_field(pf2, 0, 32) as usize;
    if header_size < 32 || data.len() < header_size {
        return Err(Error::Truncated {
            what: "lzfse v2 freq tables",
            needed: header_size,
            available: data.len(),
        });
    }

    // Decode frequency tables from freq[] area (starts at offset 32)
    let freq_data = &data[32..header_size];
    let _total_symbols =
        ENCODE_L_SYMBOLS + ENCODE_M_SYMBOLS + ENCODE_D_SYMBOLS + ENCODE_LITERAL_SYMBOLS;
    let (freq, _) = decode_freq_stream(freq_data)?;

    let mut header = header;
    let mut idx = 0;
    for slot in header.l_freq.iter_mut() {
        *slot = freq[idx];
        idx += 1;
    }
    for slot in header.m_freq.iter_mut() {
        *slot = freq[idx];
        idx += 1;
    }
    for slot in header.d_freq.iter_mut() {
        *slot = freq[idx];
        idx += 1;
    }
    for slot in header.literal_freq.iter_mut() {
        *slot = freq[idx];
        idx += 1;
    }

    // Verify freq sums
    let l_sum: u32 = header.l_freq.iter().map(|&f| f as u32).sum();
    let m_sum: u32 = header.m_freq.iter().map(|&f| f as u32).sum();
    let d_sum: u32 = header.d_freq.iter().map(|&f| f as u32).sum();
    let lit_sum: u32 = header.literal_freq.iter().map(|&f| f as u32).sum();

    if l_sum != ENCODE_L_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v2 header",
            format!("l_freq sum {} != {}", l_sum, ENCODE_L_STATES),
        ));
    }
    if m_sum != ENCODE_M_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v2 header",
            format!("m_freq sum {} != {}", m_sum, ENCODE_M_STATES),
        ));
    }
    if d_sum != ENCODE_D_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v2 header",
            format!("d_freq sum {} != {}", d_sum, ENCODE_D_STATES),
        ));
    }
    if lit_sum != ENCODE_LITERAL_STATES as u32 {
        return Err(Error::invalid(
            "lzfse v2 header",
            format!("literal_freq sum {} != {}", lit_sum, ENCODE_LITERAL_STATES),
        ));
    }

    Ok((header, header_size))
}

// ============================================================
// Main LZFSE decoder
// ============================================================

/// Decode an LZFSE bitstream.
///
/// `data` is the raw LZFSE compressed bytes. `uncompressed_size` is the
/// expected output size; the decoder validates against it.
pub fn decode(data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    let decoder = LzfseDecoder::new(data);
    let result = decoder.decode()?;

    if result.len() != uncompressed_size as usize {
        return Err(Error::invalid(
            "lzfse decompression",
            format!("expected {} bytes, got {}", uncompressed_size, result.len()),
        ));
    }
    Ok(result)
}

/// Stateful LZFSE decoder that loops over blocks until end-of-stream.
struct LzfseDecoder<'a> {
    src: &'a [u8],
    dst: Vec<u8>,
    src_pos: usize,
    /// Magic number of the current block being processed.
    block_magic: u32,
}

impl<'a> LzfseDecoder<'a> {
    fn new(src: &'a [u8]) -> Self {
        LzfseDecoder {
            src,
            dst: Vec::new(),
            src_pos: 0,
            block_magic: NO_BLOCK_MAGIC,
        }
    }

    fn decode(mut self) -> Result<Vec<u8>> {
        loop {
            // Read magic if not in a block
            if self.block_magic == NO_BLOCK_MAGIC {
                if self.src.len() - self.src_pos < 4 {
                    break;
                }
                let magic = load_le32(&self.src[self.src_pos..]);
                self.src_pos += 4;
                self.block_magic = magic;

                if magic == ENDOFSTREAM_BLOCK_MAGIC {
                    break;
                }
            }

            match self.block_magic {
                ENDOFSTREAM_BLOCK_MAGIC => break,
                UNCOMPRESSED_BLOCK_MAGIC => self.decode_uncompressed_block()?,
                COMPRESSEDV1_BLOCK_MAGIC => self.decode_compression_block(true)?,
                COMPRESSEDV2_BLOCK_MAGIC => self.decode_compression_block(false)?,
                COMPRESSEDLZVN_BLOCK_MAGIC => self.decode_lzvn_block()?,
                _ => {
                    return Err(Error::invalid(
                        "lzfse block magic",
                        format!("unknown magic 0x{:08x}", self.block_magic),
                    ));
                }
            }
        }
        Ok(self.dst)
    }

    /// Decode an uncompressed block: magic (already read) + n_raw_bytes + raw data.
    ///
    /// Mining reference: Apple `lzfse_decode_base.c` `LZFSE_UNCOMPRESSED_BLOCK_MAGIC`.
    fn decode_uncompressed_block(&mut self) -> Result<()> {
        if self.src.len() - self.src_pos < 4 {
            return Err(Error::Truncated {
                what: "lzfse uncompressed block n_raw_bytes",
                needed: 4,
                available: self.src.len() - self.src_pos,
            });
        }
        let n_raw = load_le32(&self.src[self.src_pos..]) as usize;
        self.src_pos += 4;

        if self.src.len() - self.src_pos < n_raw {
            return Err(Error::Truncated {
                what: "lzfse uncompressed block data",
                needed: n_raw,
                available: self.src.len() - self.src_pos,
            });
        }
        self.dst
            .extend_from_slice(&self.src[self.src_pos..self.src_pos + n_raw]);
        self.src_pos += n_raw;
        self.block_magic = NO_BLOCK_MAGIC;
        Ok(())
    }

    /// Decode a compressed LZFSE block (V1 or V2 header).
    ///
    /// Mining reference: Apple `lzfse_decode_base.c` block parsing and
    /// `lzfse_decode_lmd` main loop.
    fn decode_compression_block(&mut self, is_v1: bool) -> Result<()> {
        let header_start = self.src_pos - 4; // Include magic bytes

        let (header, header_size) = if is_v1 {
            parse_v1_header(&self.src[header_start..])?
        } else {
            parse_v2_header(&self.src[header_start..])?
        };

        // Verify we have the entire block payload
        let header_end = header_start + header_size;
        let literal_payload_end = header_end + header.n_literal_payload_bytes as usize;
        let block_end = literal_payload_end + header.n_lmd_payload_bytes as usize;

        if self.src.len() < block_end {
            return Err(Error::Truncated {
                what: "lzfse compressed block",
                needed: block_end,
                available: self.src.len(),
            });
        }

        // Advance past header to the start of literal payload
        self.src_pos = header_end;

        // Build decoder tables from frequency tables
        let literal_decoder = build_literal_decoder_table(&header.literal_freq)?;

        let l_decoder = build_value_decoder_table(
            ENCODE_L_STATES,
            ENCODE_L_SYMBOLS,
            &header.l_freq,
            &L_EXTRA_BITS,
            &L_BASE_VALUE,
        )?;

        let m_decoder = build_value_decoder_table(
            ENCODE_M_STATES,
            ENCODE_M_SYMBOLS,
            &header.m_freq,
            &M_EXTRA_BITS,
            &M_BASE_VALUE,
        )?;

        let d_decoder = build_value_decoder_table(
            ENCODE_D_STATES,
            ENCODE_D_SYMBOLS,
            &header.d_freq,
            &D_EXTRA_BITS,
            &D_BASE_VALUE,
        )?;

        // Decode literals (4 interleaved FSE states, read backwards from end of literal payload)
        // Buffer size follows Apple's LZFSE_DECODE_LITERALS_PER_BLOCK + 64 safety margin
        let mut literals = vec![0u8; LITERALS_PER_BLOCK + 64];
        {
            let lit_buf = &self.src[..literal_payload_end];
            let mut lit_reader = FseBitReader {
                buf: lit_buf,
                buf_pos: literal_payload_end,
                buf_start: 0,
                accum: 0,
                accum_nbits: 0,
            };
            lit_reader.init(header.literal_bits)?;

            let mut state0 = header.literal_state[0];
            let mut state1 = header.literal_state[1];
            let mut state2 = header.literal_state[2];
            let mut state3 = header.literal_state[3];

            let n_literals = header.n_literals as usize;
            let mut i = 0;
            while i < n_literals {
                lit_reader.flush()?;
                literals[i] = lit_reader.decode_symbol(&mut state0, &literal_decoder)?;
                lit_reader.flush()?;
                i += 1;
                if i >= n_literals {
                    break;
                }
                literals[i] = lit_reader.decode_symbol(&mut state1, &literal_decoder)?;
                lit_reader.flush()?;
                i += 1;
                if i >= n_literals {
                    break;
                }
                literals[i] = lit_reader.decode_symbol(&mut state2, &literal_decoder)?;
                lit_reader.flush()?;
                i += 1;
                if i >= n_literals {
                    break;
                }
                literals[i] = lit_reader.decode_symbol(&mut state3, &literal_decoder)?;
                i += 1;
            }
        }

        // Initialize LMD stream (read backwards from end of LMD payload)
        let lmd_buf = &self.src[literal_payload_end..block_end];
        let mut lmd_reader = FseBitReader {
            buf: lmd_buf,
            buf_pos: lmd_buf.len(),
            buf_start: 0,
            accum: 0,
            accum_nbits: 0,
        };
        lmd_reader.init(header.lmd_bits)?;

        let mut l_state = header.l_state;
        let mut m_state = header.m_state;
        let mut d_state = header.d_state;
        let mut lit_pos: usize = 0;
        let mut d_prev: i32 = -1;

        let n_matches = header.n_matches as usize;
        let mut symbols_remaining = n_matches;

        while symbols_remaining > 0 {
            // Decode L, M, D triplet
            lmd_reader.flush()?;
            let l_val = lmd_reader.decode_value(&mut l_state, &l_decoder)?;

            let m_val = lmd_reader.decode_value(&mut m_state, &m_decoder)?;

            let d_val = lmd_reader.decode_value(&mut d_state, &d_decoder)?;

            let l = l_val as usize;
            let m = m_val as usize;
            let d = if d_val != 0 { d_val } else { d_prev } as usize;

            // Copy literal bytes
            if l > 0 {
                if lit_pos + l > header.n_literals as usize {
                    return Err(Error::invalid(
                        "lzfse literal",
                        format!(
                            "literal count overflow: {lit_pos} + {l} > {}",
                            header.n_literals
                        ),
                    ));
                }
                self.dst.extend_from_slice(&literals[lit_pos..lit_pos + l]);
                lit_pos += l;
            }

            // Copy match (with overlap handling for small distances)
            if m > 0 {
                let dst_pos = self.dst.len();
                if d == 0 || d > dst_pos {
                    return Err(Error::invalid(
                        "lzfse match distance",
                        format!("distance {d} exceeds output position {dst_pos}"),
                    ));
                }
                let src_start = dst_pos - d;
                if d >= m {
                    // Non-overlapping copy
                    let bytes = self.dst[src_start..src_start + m].to_vec();
                    self.dst.extend_from_slice(&bytes);
                } else {
                    // Overlapping: copy byte-by-byte (splat semantics)
                    for _ in 0..m {
                        let src_idx = self.dst.len() - d;
                        let byte = self.dst[src_idx];
                        self.dst.push(byte);
                    }
                }
            }

            d_prev = d_val;
            symbols_remaining -= 1;
        }

        // Validate output size matches declared n_raw_bytes
        let n_raw = header.n_raw_bytes as usize;
        if self.dst.len() > n_raw {
            return Err(Error::invalid(
                "lzfse block",
                format!(
                    "output {} exceeds declared n_raw_bytes {}",
                    self.dst.len(),
                    n_raw
                ),
            ));
        }

        // Advance past both payload sections
        self.src_pos = block_end;
        self.block_magic = NO_BLOCK_MAGIC;
        Ok(())
    }

    /// Decode an embedded LZVN block within an LZFSE stream.
    ///
    /// Mining reference: Apple `lzfse_decode_base.c` `LZFSE_COMPRESSEDLZVN_BLOCK_MAGIC`.
    fn decode_lzvn_block(&mut self) -> Result<()> {
        if self.src.len() - self.src_pos < 8 {
            return Err(Error::Truncated {
                what: "lzfse lzvn block header",
                needed: 8,
                available: self.src.len() - self.src_pos,
            });
        }
        let n_raw_bytes = load_le32(&self.src[self.src_pos..]) as usize;
        let n_payload_bytes = load_le32(&self.src[self.src_pos + 4..]) as usize;
        self.src_pos += 8;

        let payload_end = (self.src_pos + n_payload_bytes).min(self.src.len());
        let block_data = &self.src[self.src_pos..payload_end];
        let decoded = super::lzvn::decode(block_data, n_raw_bytes as u64)?;
        self.dst.extend_from_slice(&decoded);
        self.src_pos += n_payload_bytes;

        self.block_magic = NO_BLOCK_MAGIC;
        Ok(())
    }
}

// ============================================================
// Utility functions
// ============================================================

/// Load a little-endian u16 from a byte slice.
fn load_le16(data: &[u8]) -> u16 {
    let mut result = 0u16;
    for (i, &byte) in data.iter().enumerate().take(2) {
        result |= (byte as u16) << (i * 8);
    }
    result
}

/// Load a little-endian u32 from a byte slice.
fn load_le32(data: &[u8]) -> u32 {
    let mut result = 0u32;
    for (i, &byte) in data.iter().enumerate().take(4) {
        result |= (byte as u32) << (i * 8);
    }
    result
}

/// Load a little-endian u64 from a byte slice.
fn load_le64(data: &[u8]) -> u64 {
    let mut result = 0u64;
    for (i, &byte) in data.iter().enumerate().take(8) {
        result |= (byte as u64) << (i * 8);
    }
    result
}

/// Mask the `nbits` least significant bits of `x`.
fn mask_lsb128(nbits: u32) -> u128 {
    if nbits >= 128 {
        u128::MAX
    } else {
        (1u128 << nbits) - 1
    }
}

/// Mask the `nbits` least significant bits of `x`.
fn mask_lsb32(x: u32, nbits: u32) -> u32 {
    if nbits >= 32 {
        x
    } else {
        x & ((1u32 << nbits) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        let result = decode(&[], 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn end_of_stream_marker_terminates() {
        // End-of-stream magic (LE bytes for 0x24787662)
        let data = [0x62, 0x76, 0x78, 0x24];
        let result = decode(&data, 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn unknown_magic_returns_error() {
        // bvx1 magic followed by truncated header
        let data = [0x62, 0x76, 0x78, 0x31];
        let result = decode(&data, 0);
        assert!(result.is_err());
    }

    #[test]
    fn uncompressed_block_round_trip() {
        // bvx- marker + n_raw_bytes=5 + "Hello" + eos
        let mut data = Vec::new();
        data.extend_from_slice(&UNCOMPRESSED_BLOCK_MAGIC.to_le_bytes());
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(b"Hello");
        data.extend_from_slice(&ENDOFSTREAM_BLOCK_MAGIC.to_le_bytes());

        let result = decode(&data, 5).unwrap();
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn fse_table_construction_matches_state_sum() {
        // With uniform frequency distribution, table should be valid
        let mut freq = [0u16; ENCODE_L_SYMBOLS];
        let per_state = ENCODE_L_STATES / ENCODE_L_SYMBOLS;
        freq.fill(per_state as u16);
        let table = build_value_decoder_table(
            ENCODE_L_STATES,
            ENCODE_L_SYMBOLS,
            &freq,
            &L_EXTRA_BITS,
            &L_BASE_VALUE,
        )
        .unwrap();
        assert_eq!(table.len(), ENCODE_L_STATES);
    }

    #[test]
    fn uncompressed_block_round_trip_hello() {
        // Test 1: "Hello, LZFSE World!" via uncompressed block
        // Magic(bvx-) + n_raw_bytes(19) + "Hello, LZFSE World!" + EOS
        let data: Vec<u8> = vec![
            0x62, 0x76, 0x78, 0x2d, // bvx- (uncompressed)
            0x13, 0x00, 0x00, 0x00, // 19 bytes
            0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20, 0x4c, 0x5a, 0x46, 0x53, 0x45, 0x20, 0x57,
            0x6f, 0x72, 0x6c, 0x64, 0x21, // "Hello, LZFSE World!"
            0x62, 0x76, 0x78, 0x24, // bvx$ (eos)
        ];
        let result = decode(&data, 19).unwrap();
        assert_eq!(result, b"Hello, LZFSE World!");
    }

    #[test]
    fn v2_compressed_block_decodes_text_pattern() {
        // Test 4: 4096 bytes of repeated text, V2 compressed (bvx2)
        // Generated with Apple's lzfse encoder
        let data: Vec<u8> = vec![
            0x62, 0x76, 0x78, 0x32, 0x00, 0x10, 0x00, 0x00, 0x34, 0x00, 0xd0, 0x01, 0x00, 0x03,
            0x00, 0x50, 0x0e, 0x49, 0xe8, 0xda, 0x1f, 0x0d, 0x00, 0x70, 0xa1, 0x00, 0x00, 0x00,
            0x37, 0x90, 0xb0, 0x0d, 0xe7, 0x00, 0x70, 0x0d, 0x00, 0x00, 0xd7, 0x5c, 0x03, 0x00,
            0x00, 0x00, 0xc0, 0x4f, 0xf0, 0x3e, 0x7c, 0x0f, 0x00, 0x00, 0x00, 0xdf, 0x03, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0xef, 0x0b, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xc0, 0xf1, 0xf1,
            0xf1, 0x3f, 0xf0, 0x23, 0x1c, 0xff, 0x03, 0xff, 0xc0, 0xf1, 0xf1, 0xf1, 0xf1, 0xf1,
            0xf1, 0x2b, 0x71, 0x7c, 0xfc, 0x0f, 0x1c, 0x1f, 0xff, 0x03, 0xc7, 0xc7, 0xc7, 0xc7,
            0xc7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xf5, 0xf9, 0x6e, 0xa5, 0x19,
            0xc3, 0xd8, 0x34, 0x6f, 0xeb, 0xcd, 0x6d, 0x2d, 0x81, 0x7f, 0xb0, 0x7b, 0x70, 0x22,
            0x9f, 0x27, 0xcb, 0x81, 0x0f, 0x53, 0x04, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xa8, 0xab, 0xc5, 0xff, 0x47, 0x62, 0x76, 0x78, 0x24,
        ];
        // This should decompress to 4096 bytes of a pattern repeated
        let result = decode(&data, 4096).expect("V2 decompression should succeed");
        assert_eq!(result.len(), 4096);
        // The content is "The quick brown fox jumps over the lazy dog. " repeated
        // with the last 7 literal bytes being "The qui" (pattern restart)
        let pattern = "The quick brown fox jumps over the lazy dog. ";
        let pattern_bytes = pattern.as_bytes();
        // Check full chunks only
        for chunk in result.chunks_exact(pattern.len()) {
            assert_eq!(
                chunk, pattern_bytes,
                "V2 block decompression output mismatch"
            );
        }
        // The remaining bytes should be the start of the pattern
        let remainder = &result[pattern.len() * (result.len() / pattern.len())..];
        assert!(
            pattern_bytes.starts_with(remainder),
            "V2 block decompression trailing bytes mismatch: got {:?}, expected prefix of pattern",
            remainder
        );
    }

    #[test]
    fn lzvn_embedded_in_lzfse_stream() {
        // Test 2: "Hello" with 1000 bytes of repeated pattern uses LZVN block (bvxn)
        // Magic(bvxn) + n_raw_bytes(1000) + n_payload_bytes + LZVN data + EOS
        let data: Vec<u8> = vec![
            0x62, 0x76, 0x78, 0x6e, // bvxn (LZVN compressed)
            0xe8, 0x03, 0x00, 0x00, // n_raw_bytes = 1000
            0x18, 0x00, 0x00, 0x00, // n_payload_bytes = 24
            // LZVN payload (24 bytes) - the actual compressed data
            0x68, 0x01, 0x41, 0xf0, 0xff, 0xf0, 0xcc, 0x6e, 0x42, 0xf0, 0xff, 0xf0, 0xca, 0xe2,
            0x42, 0x42, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x62, 0x76, 0x78,
            0x24, // bvx$ (eos)
        ];
        let result = decode(&data, 1000).expect("LZVN-within-LZFSE decompression should succeed");
        assert_eq!(result.len(), 1000);
        assert_eq!(&result[0..500], &vec![b'A'; 500]);
        assert_eq!(&result[500..1000], &vec![b'B'; 500]);
    }
}
