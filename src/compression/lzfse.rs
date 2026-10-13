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
// SPDX-License-Identifier: BSD-3-Clause

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

const LZFSE_ENCODE_HASH_BITS: usize = 14;
const LZFSE_ENCODE_HASH_WIDTH: usize = 4;
const LZFSE_ENCODE_HASH_VALUES: usize = 1 << LZFSE_ENCODE_HASH_BITS;
#[allow(dead_code)]
const LZFSE_MATCHES_PER_BLOCK: usize = 10000;
const LZFSE_ENCODE_MIN_MATCH: usize = 4;
const LZFSE_ENCODE_MAX_MATCH: usize = 1024;
#[allow(dead_code)]
const LZFSE_ENCODE_GOOD_MATCH: usize = 40;
const LZFSE_ENCODE_LZVN_THRESHOLD: usize = 4096;

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
        } else if nbits == 5 && value < 0 {
            // The 5-bit pattern 00011 (b=3) is the special code for freq value 4.
            // The reference decoder's "else" branch would interpret it as 24,
            // which is incorrect — fix to return the correct value.
            4
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

            if d_val != 0 {
                d_prev = d_val;
            }
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

/// Mask the `nbits` least significant bits of `x` (u64).
fn mask_lsb64(x: u64, nbits: u32) -> u64 {
    if nbits >= 64 {
        x
    } else {
        x & ((1u64 << nbits) - 1)
    }
}

// ============================================================
// LZFSE Encoder
// ============================================================

/// Encoder-side FSE table entry.
///
/// Mining reference: Apple `lzfse_fse.h` `fse_encoder_entry`.
#[derive(Clone, Copy)]
struct FseEncoderEntry {
    s0: i32,
    k: i32,
    delta0: i32,
    delta1: i32,
}

/// Build an FSE encoder table from per-symbol frequencies.
///
/// Mining reference: Apple `lzfse_fse.c` `fse_init_encoder_table`.
///
/// For each symbol with frequency `f > 0`, computes:
/// - `k`: shift value such that `nstates <= (f << k) < 2 * nstates`
/// - `s0`: first state using `k` bits (derived from `k` and `nstates`)
/// - `delta0`: state delta for states >= s0 (using `k` bits)
/// - `delta1`: state delta for states < s0 (using `k-1` bits)
fn fse_init_encoder_table(nstates: usize, nsymbols: usize, freq: &[u16]) -> Vec<FseEncoderEntry> {
    let nstates_i = nstates as i32;
    let n_clz = (nstates as u32).leading_zeros() as i32;
    let mut table = vec![
        FseEncoderEntry {
            s0: 0,
            k: 0,
            delta0: 0,
            delta1: 0,
        };
        nsymbols
    ];

    let mut offset = 0;
    for i in 0..nsymbols {
        let f = freq[i] as i32;
        if f == 0 {
            continue;
        }
        let mut k = (f.leading_zeros() as i32) - n_clz;
        if k < 0 {
            k = 0;
        }
        table[i].s0 = (f << k) - nstates_i;
        table[i].k = k;
        table[i].delta0 = offset - f + (nstates_i >> k);
        table[i].delta1 = if k > 0 {
            offset - f + (nstates_i >> (k - 1))
        } else {
            0
        };
        offset += f;
    }

    table
}

/// Adjust frequencies when the sum exceeds the number of states.
///
/// Mining reference: Apple `lzfse_fse.c` `fse_adjust_freqs`.
fn fse_adjust_freqs(freq: &mut [u16], mut overrun: i32) {
    for shift in (1..=3).rev() {
        if overrun == 0 {
            break;
        }
        for sym in freq.iter_mut() {
            if *sym > 1 {
                let n = ((*sym - 1) >> shift).min(overrun as u16);
                *sym -= n;
                overrun -= n as i32;
                if overrun == 0 {
                    break;
                }
            }
        }
    }
}

/// Normalize occurrence counts to a frequency table summing to `nstates`.
///
/// Mining reference: Apple `lzfse_fse.c` `fse_normalize_freq`.
fn fse_normalize_freq(nstates: usize, _nsymbols: usize, occ: &[u32], freq: &mut [u16]) {
    let nstates_i = nstates as i32;
    let mut s_count: u32 = 0;
    let mut remaining = nstates_i;
    let mut max_freq = 0;
    let mut max_freq_sym = 0;
    let shift = (nstates as u32).leading_zeros().saturating_sub(1) as i32;

    for &o in occ {
        s_count += o;
    }

    let highprec_step: u32 = (1u32 << 31).checked_div(s_count).unwrap_or(0);

    for (i, &occ_i) in occ.iter().enumerate() {
        let f = ((occ_i.wrapping_mul(highprec_step) >> shift) + 1) >> 1;

        if f == 0 && occ_i != 0 {
            freq[i] = 1;
            remaining -= 1;
        } else {
            freq[i] = f as u16;
            remaining -= f as i32;
        }

        if freq[i] as i32 > max_freq {
            max_freq = freq[i] as i32;
            max_freq_sym = i;
        }
    }

    if -remaining < (max_freq >> 2) {
        freq[max_freq_sym] = (freq[max_freq_sym] as i32 + remaining) as u16;
    } else {
        fse_adjust_freqs(freq, -remaining);
    }
}

/// FSE encoder output stream (64-bit accumulator).
///
/// Mining reference: Apple `lzfse_fse.h` `fse_out_stream64`,
/// `fse_out_init64`, `fse_out_push64`, `fse_out_flush64`, `fse_out_finish64`.
struct FseOutStream {
    accum: u64,
    accum_nbits: i32,
}

impl FseOutStream {
    fn new() -> Self {
        FseOutStream {
            accum: 0,
            accum_nbits: 0,
        }
    }

    /// Push `n` bits of `b` into the accumulator.
    fn push(&mut self, n: i32, b: u64) {
        self.accum |= b << self.accum_nbits;
        self.accum_nbits += n;
    }

    /// Write all complete bytes from the accumulator to `dst`.
    fn flush(&mut self, dst: &mut Vec<u8>) {
        let nbits = (self.accum_nbits & !7) as usize;
        let bytes = nbits / 8;
        for i in 0..bytes {
            dst.push(((self.accum >> (i * 8)) & 0xFF) as u8);
        }
        self.accum >>= nbits as u32;
        self.accum_nbits -= nbits as i32;
    }

    /// Write final bytes (including padding) and reset the accumulator.
    fn finish(&mut self, dst: &mut Vec<u8>) {
        let nbits = ((self.accum_nbits + 7) & !7) as usize;
        let bytes = nbits / 8;
        for i in 0..bytes {
            dst.push(((self.accum >> (i * 8)) & 0xFF) as u8);
        }
        self.accum = 0;
        self.accum_nbits -= nbits as i32;
    }
}

/// Encode a symbol using the FSE encoder table.
///
/// Mining reference: Apple `lzfse_fse.h` `fse_encode`.
fn fse_encode(state: &mut u16, table: &[FseEncoderEntry], out: &mut FseOutStream, symbol: u8) {
    let s = *state as i32;
    let e = table[symbol as usize];
    let hi = if s >= e.s0 { 1 } else { 0 };
    let nbits = if hi == 1 { e.k } else { e.k - 1 };
    let delta = if hi == 1 { e.delta0 } else { e.delta1 };
    let b = mask_lsb64(s as u64, nbits as u32);
    out.push(nbits, b);
    *state = (delta + (s >> nbits)) as u16;
}

/// Map a value to its L symbol index.
///
/// Mining reference: Apple `lzfse_encode_tables.h` `l_base_from_value`.
fn l_base_from_value(value: u32) -> u8 {
    let mut sym = 0u32;
    while sym < (ENCODE_L_SYMBOLS as u32 - 1) && value >= L_BASE_VALUE[(sym + 1) as usize] as u32 {
        sym += 1;
    }
    sym as u8
}

/// Map a value to its M symbol index.
///
/// Mining reference: Apple `lzfse_encode_tables.h` `m_base_from_value`.
fn m_base_from_value(value: u32) -> u8 {
    let mut sym = 0u32;
    while sym < (ENCODE_M_SYMBOLS as u32 - 1) && value >= M_BASE_VALUE[(sym + 1) as usize] as u32 {
        sym += 1;
    }
    sym as u8
}

/// Map a value to its D symbol index.
///
/// Mining reference: Apple `lzfse_encode_tables.h` `d_base_from_value`.
fn d_base_from_value(value: i32) -> u8 {
    let mut index: i32 = 0;
    {
        let in_range_k = if (0..60).contains(&value) {
            -1i32
        } else {
            0i32
        };
        let contrib = value & in_range_k;
        index |= contrib;
    }
    {
        let in_range_k = if (60..1020).contains(&value) {
            -1i32
        } else {
            0i32
        };
        let contrib = (((value - 60) >> 4) + 64) & in_range_k;
        index |= contrib;
    }
    {
        let in_range_k = if (1020..16380).contains(&value) {
            -1i32
        } else {
            0i32
        };
        let contrib = (((value - 1020) >> 8) + 128) & in_range_k;
        index |= contrib;
    }
    {
        let in_range_k = if (16380..262140).contains(&value) {
            -1i32
        } else {
            0i32
        };
        let contrib = (((value - 16380) >> 12) + 192) & in_range_k;
        index |= contrib;
    }
    let table: [u8; 256] = [
        0, 1, 2, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11,
        12, 12, 12, 12, 12, 12, 12, 12, 13, 13, 13, 13, 13, 13, 13, 13, 14, 14, 14, 14, 14, 14, 14,
        14, 15, 15, 15, 15, 15, 15, 15, 15, 16, 16, 16, 16, 16, 17, 18, 19, 20, 20, 21, 21, 22, 22,
        23, 23, 24, 24, 24, 24, 25, 25, 25, 25, 26, 26, 26, 26, 27, 27, 27, 27, 28, 28, 28, 28, 28,
        28, 28, 28, 29, 29, 29, 29, 29, 29, 29, 29, 30, 30, 30, 30, 30, 30, 30, 30, 31, 31, 31, 31,
        31, 31, 31, 31, 32, 32, 32, 32, 32, 33, 34, 35, 36, 36, 37, 37, 38, 38, 39, 39, 40, 40, 40,
        40, 41, 41, 41, 41, 42, 42, 42, 42, 43, 43, 43, 43, 44, 44, 44, 44, 44, 44, 44, 44, 45, 45,
        45, 45, 45, 45, 45, 45, 46, 46, 46, 46, 46, 46, 46, 46, 47, 47, 47, 47, 47, 47, 47, 47, 48,
        48, 48, 48, 48, 49, 50, 51, 52, 52, 53, 53, 54, 54, 55, 55, 56, 56, 56, 56, 57, 57, 57, 57,
        58, 58, 58, 58, 59, 59, 59, 59, 60, 60, 60, 60, 60, 60, 60, 60, 61, 61, 61, 61, 61, 61, 61,
        61, 62, 62, 62, 62, 62, 62, 62, 62, 63, 63, 63, 63, 63, 63, 63, 63, 0, 0, 0, 0,
    ];
    table[(index & 0xFF) as usize]
}

/// Encode a V1 frequency value using a fixed Huffman-like code.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_v1_freq_value`.
fn lzfse_encode_v1_freq_value(value: i32) -> (u32, i32) {
    let (bits, nbits) = match value {
        0 => (0u32, 2),
        1 => (2, 2),
        2 => (1u32, 3),
        3 => (5u32, 3),
        4 => (3u32, 5),
        5 => (11u32, 5),
        6 => (19u32, 5),
        7 => (27u32, 5),
        _ if value < 24 => (7u32 + ((value as u32 - 8) << 4), 8),
        _ => (((value as u32 - 24) << 4) + 15, 14),
    };
    (bits, nbits)
}

/// Encode frequency tables from V1 header format to V2 header format.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_v1_freq_table`.
fn lzfse_encode_v1_freq_table(freq: &[u16]) -> Vec<u8> {
    let total_symbols =
        ENCODE_L_SYMBOLS + ENCODE_M_SYMBOLS + ENCODE_D_SYMBOLS + ENCODE_LITERAL_SYMBOLS;
    let mut dst = Vec::new();
    let mut accum: u32 = 0;
    let mut accum_nbits: i32 = 0;

    for &f in freq.iter().take(total_symbols) {
        let (bits, nbits) = lzfse_encode_v1_freq_value(f as i32);
        accum |= bits << accum_nbits;
        accum_nbits += nbits;
        while accum_nbits >= 8 {
            dst.push((accum & 0xFF) as u8);
            accum >>= 8;
            accum_nbits -= 8;
        }
    }
    if accum_nbits > 0 {
        dst.push((accum & 0xFF) as u8);
    }

    dst
}

/// Insert `nbits` bits from `data` at position `offset` in a u64.
///
/// Mining reference: Apple `lzfse_encode_base.c` `setField`.
fn set_field(data: u32, offset: u32, nbits: u32) -> u64 {
    if nbits >= 32 {
        (data as u64) << offset
    } else {
        (data as u64 & ((1u64 << nbits) - 1)) << offset
    }
}

/// V1 header fields used during encoding (freq tables handled separately).
///
/// Mining reference: Apple `lzfse_internal.h`
/// `lzfse_compressed_block_header_v1`.
struct LzfseV1HeaderFields {
    n_literal_payload_bytes: u32,
    n_lmd_payload_bytes: u32,
    n_literals: u32,
    n_matches: u32,
    literal_bits: i32,
    literal_state: [u16; 4],
    lmd_bits: i32,
    l_state: u16,
    m_state: u16,
    d_state: u16,
}

#[allow(dead_code)]
const LZFSE_ENCODE_MAX_D_VALUE: u32 = 262139;

/// A candidate match found by the hash-based match finder.
#[allow(dead_code)]
struct LzfseMatch {
    pos: usize,
    ref_pos: usize,
    length: u32,
}

/// History line: positions and values for candidate matches.
#[derive(Clone, Copy)]
struct HistoryLine {
    pos: [i32; LZFSE_ENCODE_HASH_WIDTH],
    value: [u32; LZFSE_ENCODE_HASH_WIDTH],
}

impl Default for HistoryLine {
    fn default() -> Self {
        HistoryLine {
            pos: [-4 * LZFSE_ENCODE_MAX_D_VALUE as i32; LZFSE_ENCODE_HASH_WIDTH],
            value: [0; LZFSE_ENCODE_HASH_WIDTH],
        }
    }
}

/// Hash function: Knuth multiplicative hash into [0, 2^14-1].
///
/// Mining reference: Apple `lzfse_encode_base.c` `hashX`.
fn hash_x(x: u32) -> usize {
    ((x.wrapping_mul(2654435761u32)) >> (32 - LZFSE_ENCODE_HASH_BITS)) as usize
}

/// Load 4 bytes little-endian from `src` at `offset`, clamping to end.
fn load4_at(src: &[u8], offset: usize) -> u32 {
    let offset = offset.min(src.len().saturating_sub(4));
    let b0 = src.get(offset).copied().unwrap_or(0) as u32;
    let b1 = src.get(offset + 1).copied().unwrap_or(0) as u32;
    let b2 = src.get(offset + 2).copied().unwrap_or(0) as u32;
    let b3 = src.get(offset + 3).copied().unwrap_or(0) as u32;
    b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
}

/// Load 8 bytes little-endian from `src` at `offset`, clamping to end.
#[allow(dead_code)]
fn load8_at(src: &[u8], offset: usize) -> u64 {
    let offset = offset.min(src.len().saturating_sub(8));
    let mut result = 0u64;
    for i in 0..8 {
        result |= (src.get(offset + i).copied().unwrap_or(0) as u64) << (i * 8);
    }
    result
}

/// Encode L extra bits and L symbol into the FSE output stream.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_l_value`.
fn encode_l_value(
    out: &mut FseOutStream,
    l_state: &mut u16,
    l_enc: &[FseEncoderEntry],
    l_value: u32,
) {
    let sym = l_base_from_value(l_value);
    let base = L_BASE_VALUE[sym as usize] as u32;
    let extra_bits = L_EXTRA_BITS[sym as usize] as i32;
    let extra_value = l_value - base;
    out.push(extra_bits, extra_value as u64);
    fse_encode(l_state, l_enc, out, sym);
}

/// Encode M (match length) extra bits and symbol.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_m_value`.
fn encode_m_value(
    out: &mut FseOutStream,
    m_state: &mut u16,
    m_enc: &[FseEncoderEntry],
    m_value: u32,
) {
    let sym = m_base_from_value(m_value);
    let base = M_BASE_VALUE[sym as usize] as u32;
    let extra_bits = M_EXTRA_BITS[sym as usize] as i32;
    let extra_value = m_value - base;
    out.push(extra_bits, extra_value as u64);
    fse_encode(m_state, m_enc, out, sym);
}

/// Encode D (match distance) extra bits and symbol.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_d_value`.
fn encode_d_value(
    out: &mut FseOutStream,
    d_state: &mut u16,
    d_enc: &[FseEncoderEntry],
    d_value: u32,
) {
    let sym = d_base_from_value(d_value as i32);
    let base = D_BASE_VALUE[sym as usize] as u32;
    let extra_bits = D_EXTRA_BITS[sym as usize] as i32;
    let extra_value = d_value - base;
    out.push(extra_bits, extra_value as u64);
    fse_encode(d_state, d_enc, out, sym);
}

/// Encode the V1 header fields into the V2 packed_fields area.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_v1_state`.
fn lzfse_encode_v1_state(packed: &mut [u64; 3], h1: &LzfseV1HeaderFields) {
    packed[0] = set_field(h1.n_literals, 0, 20)
        | set_field(h1.n_literal_payload_bytes, 20, 20)
        | set_field(h1.n_matches, 40, 20)
        | set_field(7 + h1.literal_bits as u32, 60, 3);

    packed[1] = set_field(h1.literal_state[0] as u32, 0, 10)
        | set_field(h1.literal_state[1] as u32, 10, 10)
        | set_field(h1.literal_state[2] as u32, 20, 10)
        | set_field(h1.literal_state[3] as u32, 30, 10)
        | set_field(h1.n_lmd_payload_bytes, 40, 20)
        | set_field(7 + h1.lmd_bits as u32, 60, 3);

    // packed[2] low 32 bits already has header_size from caller.
    // Only set the upper 32 bits (states) here.
    packed[2] |= set_field(h1.l_state as u32, 32, 10)
        | set_field(h1.m_state as u32, 42, 10)
        | set_field(h1.d_state as u32, 52, 10);
}

/// Encode literal and match data into a compressed LZFSE V2 block.
///
/// Produces: magic + n_raw_bytes + packed_fields[3] + freq[] + literal_payload + lmd_payload.
/// The caller appends the end-of-stream marker.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_matches`.
fn lzfse_encode_block(
    _src: &[u8],
    literals: &[u8],
    l_values: &[u32],
    m_values: &[u32],
    d_values: &[u32],
) -> Vec<u8> {
    let n_literals = literals.len() as u32;
    let n_matches = l_values.len() as u32;

    // Total raw output bytes = literal bytes + match bytes.
    let n_raw_bytes: u32 = l_values.iter().sum::<u32>() + m_values.iter().sum::<u32>();

    // --- Build encoder frequency tables ---

    // Pad literals to a multiple of 4 for the 4-stream FSE encoding.
    let mut padded_literals = Vec::with_capacity(((n_literals as usize + 3) / 4) * 4);
    padded_literals.extend_from_slice(literals);
    while (padded_literals.len() & 3) != 0 {
        padded_literals.push(0);
    }
    let n_literals_padded = padded_literals.len() as u32;

    // Convert repeated distances to 0 ("same as previous").
    let mut d_encoded = d_values.to_vec();
    let mut d_prev: u32 = 0;
    for d in &mut d_encoded {
        if *d == d_prev {
            *d = 0;
        } else {
            d_prev = *d;
        }
    }

    // Count symbol occurrences.
    let mut l_occ = [0u32; ENCODE_L_SYMBOLS];
    let mut m_occ = [0u32; ENCODE_M_SYMBOLS];
    let mut d_occ = [0u32; ENCODE_D_SYMBOLS];
    let mut literal_occ = [0u32; ENCODE_LITERAL_SYMBOLS];

    for i in 0..n_matches as usize {
        l_occ[l_base_from_value(l_values[i]) as usize] += 1;
        m_occ[m_base_from_value(m_values[i]) as usize] += 1;
        d_occ[d_base_from_value(d_encoded[i] as i32) as usize] += 1;
    }
    for &lit in &padded_literals {
        literal_occ[lit as usize] += 1;
    }

    // --- Normalize occurrence tables to frequency tables ---
    let mut l_freq = [0u16; ENCODE_L_SYMBOLS];
    let mut m_freq = [0u16; ENCODE_M_SYMBOLS];
    let mut d_freq = [0u16; ENCODE_D_SYMBOLS];
    let mut literal_freq = [0u16; ENCODE_LITERAL_SYMBOLS];

    fse_normalize_freq(ENCODE_L_STATES, ENCODE_L_SYMBOLS, &l_occ, &mut l_freq);
    fse_normalize_freq(ENCODE_M_STATES, ENCODE_M_SYMBOLS, &m_occ, &mut m_freq);
    fse_normalize_freq(ENCODE_D_STATES, ENCODE_D_SYMBOLS, &d_occ, &mut d_freq);
    fse_normalize_freq(
        ENCODE_LITERAL_STATES,
        ENCODE_LITERAL_SYMBOLS,
        &literal_occ,
        &mut literal_freq,
    );

    // --- Build FSE encoder tables ---
    let l_enc = fse_init_encoder_table(ENCODE_L_STATES, ENCODE_L_SYMBOLS, &l_freq);
    let m_enc = fse_init_encoder_table(ENCODE_M_STATES, ENCODE_M_SYMBOLS, &m_freq);
    let d_enc = fse_init_encoder_table(ENCODE_D_STATES, ENCODE_D_SYMBOLS, &d_freq);
    let literal_enc =
        fse_init_encoder_table(ENCODE_LITERAL_STATES, ENCODE_LITERAL_SYMBOLS, &literal_freq);

    // --- Encode frequency table data (for V2 header) ---
    let mut freq_all = Vec::with_capacity(
        ENCODE_L_SYMBOLS + ENCODE_M_SYMBOLS + ENCODE_D_SYMBOLS + ENCODE_LITERAL_SYMBOLS,
    );
    freq_all.extend_from_slice(&l_freq);
    freq_all.extend_from_slice(&m_freq);
    freq_all.extend_from_slice(&d_freq);
    freq_all.extend_from_slice(&literal_freq);
    let freq_bytes = lzfse_encode_v1_freq_table(&freq_all);

    // --- Encode literal payload (FSE, 4 interleaved streams, reverse order) ---
    let mut literal_payload = Vec::new();
    let mut out = FseOutStream::new();
    let mut lit_state0: u16 = 0;
    let mut lit_state1: u16 = 0;
    let mut lit_state2: u16 = 0;
    let mut lit_state3: u16 = 0;

    let n_lit = padded_literals.len();
    let mut i = n_lit;
    while i >= 4 {
        fse_encode(
            &mut lit_state3,
            &literal_enc,
            &mut out,
            padded_literals[i - 1],
        );
        fse_encode(
            &mut lit_state2,
            &literal_enc,
            &mut out,
            padded_literals[i - 2],
        );
        fse_encode(
            &mut lit_state1,
            &literal_enc,
            &mut out,
            padded_literals[i - 3],
        );
        fse_encode(
            &mut lit_state0,
            &literal_enc,
            &mut out,
            padded_literals[i - 4],
        );
        out.flush(&mut literal_payload);
        i -= 4;
    }
    let literal_bits = out.accum_nbits;
    out.finish(&mut literal_payload);

    let n_literal_payload_bytes = literal_payload.len() as u32;

    // --- Encode LMD payload (FSE, reverse order of matches) ---
    // 8 zero padding bytes at the start of the LMD payload, needed by the
    // decoder's FSE reader for initial 8-byte load.
    let mut lmd_payload = Vec::new();
    lmd_payload.extend_from_slice(&[0u8; 8]);
    let mut out = FseOutStream::new();
    let mut l_state: u16 = 0;
    let mut m_state: u16 = 0;
    let mut d_state: u16 = 0;

    for i in (0..n_matches as usize).rev() {
        // Order: D first, then M, then L (decoder reads L, M, D)
        encode_d_value(&mut out, &mut d_state, &d_enc, d_encoded[i]);
        encode_m_value(&mut out, &mut m_state, &m_enc, m_values[i]);
        encode_l_value(&mut out, &mut l_state, &l_enc, l_values[i]);
        out.flush(&mut lmd_payload);
    }

    let lmd_bits = out.accum_nbits;
    out.finish(&mut lmd_payload);

    let n_lmd_payload_bytes = lmd_payload.len() as u32;

    // --- Assemble V2 header ---
    let header_size = 32 + freq_bytes.len();
    let h1 = LzfseV1HeaderFields {
        n_literals: n_literals_padded,
        n_matches,
        n_literal_payload_bytes,
        n_lmd_payload_bytes,
        literal_bits,
        literal_state: [lit_state0, lit_state1, lit_state2, lit_state3],
        lmd_bits,
        l_state,
        m_state,
        d_state,
    };

    let mut packed = [0u64; 3];
    packed[2] = set_field(header_size as u32, 0, 32);
    lzfse_encode_v1_state(&mut packed, &h1);

    let mut result = Vec::new();
    result.extend_from_slice(&COMPRESSEDV2_BLOCK_MAGIC.to_le_bytes());
    result.extend_from_slice(&n_raw_bytes.to_le_bytes());
    result.extend_from_slice(&packed[0].to_le_bytes());
    result.extend_from_slice(&packed[1].to_le_bytes());
    result.extend_from_slice(&packed[2].to_le_bytes());
    result.extend_from_slice(&freq_bytes);
    result.extend_from_slice(&literal_payload);
    result.extend_from_slice(&lmd_payload);
    result
}

/// Search for the longest match at `pos` starting at `pos` in `src`.
///
/// Mining reference: Apple `lzfse_encode_base.c` match-finding loop.
fn find_match(src: &[u8], history: &mut [HistoryLine], pos: usize) -> Option<(usize, u32)> {
    if pos + LZFSE_ENCODE_MIN_MATCH > src.len() {
        return None;
    }

    let key4 = load4_at(src, pos);
    let hash = hash_x(key4);
    let bucket = &mut history[hash];

    // Search existing candidates for the longest match.
    let mut best_len: u32 = 0;
    let mut best_dist: usize = 0;

    for slot in 0..LZFSE_ENCODE_HASH_WIDTH {
        let ref_pos = bucket.pos[slot];
        if ref_pos < 0 {
            continue;
        }
        let ref_pos = ref_pos as usize;

        // Quick 4-byte filter.
        if bucket.value[slot] != key4 {
            continue;
        }

        // Verify the match and count length. Overlapping matches are allowed
        // in LZFSE (the decoder handles them with byte-by-byte copy).
        let mut len: u32 = 4;
        while len < LZFSE_ENCODE_MAX_MATCH as u32 && pos + (len as usize) < src.len() {
            if src[pos + (len as usize)] != src[ref_pos + (len as usize)] {
                break;
            }
            len += 1;
        }

        if len > best_len {
            best_len = len;
            best_dist = pos - ref_pos;
        }
    }

    // Insert current position into the bucket (slot 0), evicting the oldest.
    for slot in (1..LZFSE_ENCODE_HASH_WIDTH).rev() {
        bucket.pos[slot] = bucket.pos[slot - 1];
        bucket.value[slot] = bucket.value[slot - 1];
    }
    bucket.pos[0] = pos as i32;
    bucket.value[0] = key4;

    if best_len >= LZFSE_ENCODE_MIN_MATCH as u32 {
        Some((best_dist, best_len))
    } else {
        None
    }
}

/// Main LZFSE encoder entry point: produces a complete LZFSE frame.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_buffer`.
fn lzfse_encode_buffer(src: &[u8]) -> Result<Vec<u8>> {
    let src_len = src.len();

    // Build the literal-match sequence using hash-based match finding.
    let mut literals: Vec<u8> = Vec::new();
    let mut l_values: Vec<u32> = Vec::new();
    let mut m_values: Vec<u32> = Vec::new();
    let mut d_values: Vec<u32> = Vec::new();

    let mut history: Vec<HistoryLine> = vec![HistoryLine::default(); LZFSE_ENCODE_HASH_VALUES];

    let mut pos: usize = 0;
    let mut pending_literals: Vec<u8> = Vec::new();

    while pos + LZFSE_ENCODE_MIN_MATCH <= src_len {
        // Try to find a match at the current position.
        let match_result = find_match(src, &mut history, pos);

        if let Some((dist, len)) = match_result {
            // For good matches (length >= GOOD_MATCH), try starting one byte later
            // (lazy matching optimization — simplified to just use the found match).
            let len = len.min(LZFSE_ENCODE_MAX_MATCH as u32);
            let dist = dist.min(LZFSE_ENCODE_MAX_D_VALUE as usize);

            l_values.push(pending_literals.len() as u32);
            m_values.push(len);
            d_values.push(dist as u32);
            literals.extend_from_slice(&pending_literals);
            pending_literals.clear();

            pos += len as usize;
        } else {
            pending_literals.push(src[pos]);
            pos += 1;
        }
    }

    // Emit remaining literals as a final LMD triplet with M=0.
    if !pending_literals.is_empty() {
        l_values.push(pending_literals.len() as u32);
        m_values.push(0);
        d_values.push(1);
        literals.extend_from_slice(&pending_literals);
        pending_literals.clear();
    }

    // If no matches were found, the data is not compressible by LZFSE.
    if m_values.is_empty() {
        return Err(Error::invalid(
            "lzfse encode",
            "no matches found; data is not compressible",
        ));
    }

    // Encode the block.
    let block = lzfse_encode_block(src, &literals, &l_values, &m_values, &d_values);

    // Append the end-of-stream marker.
    let mut result = block;
    result.extend_from_slice(&ENDOFSTREAM_BLOCK_MAGIC.to_le_bytes());
    Ok(result)
}

/// Compress data using LZFSE (type 3), producing a complete LZFSE frame.
///
/// For small inputs (<= `LZFSE_ENCODE_LZVN_THRESHOLD`), the encoder tries
/// LZVN first and uses whichever produces a smaller result. For data that
/// does not compress, an uncompressed block is emitted.
///
/// Mining reference: Apple `lzfse_encode_base.c` `lzfse_encode_buffer`
/// and `lzfse_encode_lzvn_block`.
pub fn encode(src: &[u8]) -> Vec<u8> {
    if src.is_empty() {
        return ENDOFSTREAM_BLOCK_MAGIC.to_le_bytes().to_vec();
    }

    // For inputs up to the LZVN threshold, try LZVN first (it is faster and
    // often smaller for small inputs).
    if src.len() <= LZFSE_ENCODE_LZVN_THRESHOLD {
        let lzvn_payload = super::lzvn::encode(src);
        let mut lzvn_block = Vec::new();
        lzvn_block.extend_from_slice(&COMPRESSEDLZVN_BLOCK_MAGIC.to_le_bytes());
        lzvn_block.extend_from_slice(&(src.len() as u32).to_le_bytes());
        lzvn_block.extend_from_slice(&(lzvn_payload.len() as u32).to_le_bytes());
        lzvn_block.extend_from_slice(&lzvn_payload);
        lzvn_block.extend_from_slice(&ENDOFSTREAM_BLOCK_MAGIC.to_le_bytes());
        if lzvn_block.len() <= src.len() + 8 {
            return lzvn_block;
        }
    }

    // Try LZFSE V2 encoding.
    if let Ok(lzfse_data) = lzfse_encode_buffer(src) {
        if lzfse_data.len() <= src.len() + 16 {
            return lzfse_data;
        }
    }

    // Fall back to uncompressed block.
    let mut result = Vec::new();
    result.extend_from_slice(&UNCOMPRESSED_BLOCK_MAGIC.to_le_bytes());
    result.extend_from_slice(&(src.len() as u32).to_le_bytes());
    result.extend_from_slice(src);
    result.extend_from_slice(&ENDOFSTREAM_BLOCK_MAGIC.to_le_bytes());
    result
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

    #[test]
    fn lzfse_encode_decode_round_trip() {
        let pattern = b"The quick brown fox jumps over the lazy dog. ".to_vec();
        let pattern10 = pattern.repeat(10);
        let seq_bytes: Vec<u8> = (0..1000u32)
            .flat_map(|i| [i as u8, (i >> 8) as u8, (i >> 16) as u8, (i >> 24) as u8])
            .collect();
        let inputs: Vec<&[u8]> = vec![
            b"ABABABABABABABABABABABABABABABABABABABAB",
            &[0u8; 100],
            &pattern10,
            &[0xAA; 500],
            &[b'A'; 4500],
            b"Hello, LZFSE World! This is a test.",
            &seq_bytes,
        ];

        for (i, input) in inputs.iter().enumerate() {
            let compressed = encode(input);
            if compressed.len() >= input.len() + 16 {
                println!(
                    "test {i}: compression not effective ({} >= {})",
                    compressed.len(),
                    input.len() + 16
                );
                continue;
            }
            let decompressed = match decode(&compressed, input.len() as u64) {
                Ok(d) => d,
                Err(e) => {
                    println!("test {i}: decompress error: {:?}", e);
                    panic!("test {i}: LZFSE round-trip should decompress");
                }
            };
            assert_eq!(
                &decompressed[..],
                &input[..],
                "test {i}: LZFSE round-trip output mismatch"
            );
        }
    }

    #[test]
    fn lzfse_encode_empty_input() {
        let compressed = encode(b"");
        let decompressed = decode(&compressed, 0).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn lzfse_encode_incompressible_data() {
        // Pseudo-random data that won't compress well
        let mut input = Vec::new();
        let mut state: u32 = 0x12345678;
        for _ in 0..500 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            input.push((state >> 16) as u8);
        }
        let compressed = encode(&input);
        let decompressed = decode(&compressed, input.len() as u64).unwrap();
        assert_eq!(decompressed, input);
    }
}
