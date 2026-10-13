// Copyright (c) 2015-2016, Apple Inc. All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
// 3. Neither the name of the copyright holder(s) nor the names of any
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
// (mirror: 0x09/hfsfuse `src/lzvn_decode_base.c`, `src/lzvn_decode_base.h`,
// `src/lzvn_encode_base.c`, `src/lzvn_encode_base.h`, BSD-3-Clause).
// Translated to Rust with bounds-checking and no `unsafe`.
// SPDX-License-Identifier: BSD-3-Clause

//! LZVN bitstream decoder and encoder for decmpfs compression type 4.
//!
//! LZVN is Apple's LZ77-family compression format with a variable-length opcode
//! scheme. Each opcode byte indexes a 256-entry table that determines its
//! category: literal-only, match-only, literal+match (with small/medium/large
//! distance or previous distance), end-of-stream, no-op, or undefined.
//!
//! The decoder processes opcodes sequentially, maintaining a `d_prev` distance
//! that persists across opcodes so match-only opcodes can reuse it. For decmpfs
//! type 4, the resource fork contains a raw LZVN bitstream with no length
//! prefix — the `uncompressed_size` field tells the reader the expected output
//! length.
//!
//! ## Opcode structure
//!
//! Each opcode begins with a category-determining first byte, followed by
//! optional distance/length bytes and literal data. The full dispatch table
//! maps all 256 byte values to their categories.
//!
//! Distances can be up to 65535 (16-bit). Literals range 0-3 for most opcodes;
//! up to 271 for `lrg_l`. Match lengths range from 3 to 271 depending on the
//! opcode variant.

use crate::error::{Error, Result};

// ============================================================
// Decoder
// ============================================================

/// Size of an LZVN end-of-stream marker in bytes.
const EOS_SIZE: usize = 8;

/// Decode an LZVN bitstream.
///
/// `data` is the raw LZVN compressed bytes (no length prefix). Returns the
/// decompressed bytes, which must equal `uncompressed_size` on success.
pub fn decode(data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut output: Vec<u8> = Vec::with_capacity(uncompressed_size.min(65536) as usize);
    let mut decoder = Decoder::new(data, &mut output);
    decoder.decode()?;
    let result = output;
    if result.len() != uncompressed_size as usize {
        return Err(Error::invalid(
            "lzvn decompression",
            format!("expected {} bytes, got {}", uncompressed_size, result.len()),
        ));
    }
    Ok(result)
}

/// LZVN decoder state.
struct Decoder<'a> {
    src: &'a [u8],
    dst: &'a mut Vec<u8>,
    /// Previous match distance, carried across opcodes.
    d_prev: usize,
    /// Current read position in the source buffer.
    src_pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(src: &'a [u8], dst: &'a mut Vec<u8>) -> Self {
        Decoder {
            src,
            dst,
            d_prev: 0,
            src_pos: 0,
        }
    }

    fn decode(&mut self) -> Result<()> {
        while self.src_pos < self.src.len() {
            let opc = self.src[self.src_pos];
            if self.process_opcode(opc)? {
                break;
            }
        }
        Ok(())
    }

    /// Process one opcode. Returns `true` if end-of-stream was reached.
    fn process_opcode(&mut self, opc: u8) -> Result<bool> {
        match opc_category(opc) {
            Category::SmlD => {
                let l = extract_u8(opc, 6, 2) as usize;
                let m = extract_u8(opc, 3, 3) as usize + 3;
                self.src_pos += 1;
                if self.src_pos >= self.src.len() {
                    return Err(Error::Truncated {
                        what: "lzvn sml_d distance",
                        needed: self.src.len() + 1,
                        available: self.src.len(),
                    });
                }
                let d = (extract_u8(opc, 0, 3) as usize) << 8 | self.src[self.src_pos] as usize;
                self.src_pos += 1;
                self.copy_literal_and_match(l, m, d)?;
            }
            Category::MedD => {
                let l = extract_u8(opc, 3, 2) as usize;
                self.src_pos += 1;
                if self.remaining_src() < 2 + l {
                    return Err(Error::Truncated {
                        what: "lzvn med_d opcode",
                        needed: 2 + l,
                        available: self.remaining_src(),
                    });
                }
                let opc23 = load_le16(&self.src[self.src_pos..]);
                let m =
                    ((extract_u8(opc, 0, 3) as u16) << 2 | extract_u16(opc23, 0, 2)) as usize + 3;
                let d = extract_u16(opc23, 2, 14) as usize;
                self.src_pos += 2;
                self.copy_literal_and_match(l, m, d)?;
            }
            Category::LrgD => {
                let l = extract_u8(opc, 6, 2) as usize;
                let m = extract_u8(opc, 3, 3) as usize + 3;
                self.src_pos += 1;
                if self.remaining_src() < 2 + l {
                    return Err(Error::Truncated {
                        what: "lzvn lrg_d opcode",
                        needed: 2 + l,
                        available: self.remaining_src(),
                    });
                }
                let d = load_le16(&self.src[self.src_pos..]) as usize;
                self.src_pos += 2;
                self.copy_literal_and_match(l, m, d)?;
            }
            Category::PreD => {
                let l = extract_u8(opc, 6, 2) as usize;
                let m = extract_u8(opc, 3, 3) as usize + 3;
                let d = self.d_prev;
                self.src_pos += 1;
                self.copy_literal_and_match(l, m, d)?;
            }
            Category::SmlM => {
                let m = extract_u8(opc, 0, 4) as usize;
                self.src_pos += 1;
                self.copy_match_only(m)?;
            }
            Category::LrgM => {
                self.src_pos += 1;
                if self.src_pos >= self.src.len() {
                    return Err(Error::Truncated {
                        what: "lzvn lrg_m length",
                        needed: self.src.len() + 1,
                        available: self.src.len(),
                    });
                }
                let m = self.src[self.src_pos] as usize + 16;
                self.src_pos += 1;
                self.copy_match_only(m)?;
            }
            Category::SmlL => {
                let l = extract_u8(opc, 0, 4) as usize;
                self.src_pos += 1;
                self.copy_literal_only(l)?;
            }
            Category::LrgL => {
                self.src_pos += 1;
                if self.src_pos >= self.src.len() {
                    return Err(Error::Truncated {
                        what: "lzvn lrg_l length",
                        needed: self.src.len() + 1,
                        available: self.src.len(),
                    });
                }
                let l = self.src[self.src_pos] as usize + 16;
                self.src_pos += 1;
                self.copy_literal_only(l)?;
            }
            Category::Eos => {
                self.src_pos += EOS_SIZE;
                return Ok(true);
            }
            Category::Nop => {
                self.src_pos += 1;
            }
            Category::Undef => {
                return Err(Error::invalid(
                    "lzvn opcode",
                    format!("undefined opcode 0x{opc:02x}"),
                ));
            }
        }
        Ok(false)
    }

    fn remaining_src(&self) -> usize {
        self.src.len().saturating_sub(self.src_pos)
    }

    /// Copy literal bytes then a match, preserving `d_prev`.
    fn copy_literal_and_match(&mut self, l: usize, m: usize, d: usize) -> Result<()> {
        if self.remaining_src() < l {
            return Err(Error::Truncated {
                what: "lzvn literal bytes",
                needed: l,
                available: self.remaining_src(),
            });
        }
        if l > 0 {
            let start = self.src_pos;
            self.dst.extend_from_slice(&self.src[start..start + l]);
            self.src_pos += l;
        }
        self.copy_match(m, d)?;
        self.d_prev = d;
        Ok(())
    }

    /// Copy only a match (uses `d_prev`).
    fn copy_match_only(&mut self, m: usize) -> Result<()> {
        let d = self.d_prev;
        if d == 0 {
            return Err(Error::invalid(
                "lzvn match distance",
                "no previous distance set",
            ));
        }
        self.copy_match(m, d)?;
        Ok(())
    }

    /// Copy only literal bytes (no match).
    fn copy_literal_only(&mut self, l: usize) -> Result<()> {
        if self.remaining_src() < l {
            return Err(Error::Truncated {
                what: "lzvn literal bytes",
                needed: l,
                available: self.remaining_src(),
            });
        }
        if l > 0 {
            let start = self.src_pos;
            self.dst.extend_from_slice(&self.src[start..start + l]);
            self.src_pos += l;
        }
        Ok(())
    }

    /// Copy `m` bytes from `d` bytes back in the output.
    ///
    /// When `d < m`, the source and destination overlap — bytes are copied
    /// forward byte-by-byte (splat semantics), matching the C decoder.
    fn copy_match(&mut self, m: usize, d: usize) -> Result<()> {
        let dst_pos = self.dst.len();
        if d == 0 || d > dst_pos {
            return Err(Error::invalid(
                "lzvn match distance",
                format!("distance {d} exceeds output position {dst_pos}"),
            ));
        }
        let src_start = dst_pos - d;
        if d >= m {
            let bytes: Vec<u8> = self.dst[src_start..src_start + m].to_vec();
            self.dst.extend_from_slice(&bytes);
        } else {
            for _ in 0..m {
                let src_idx = self.dst.len() - d;
                let byte = self.dst[src_idx];
                self.dst.push(byte);
            }
        }
        Ok(())
    }
}

// ============================================================
// Opcode categories and helpers (shared by decoder and tests)
// ============================================================

/// Opcode categories for the LZVN decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    /// Literal + small distance match: `LLMMMDDD DDDDDDDD LITERAL`
    SmlD,
    /// Literal + medium distance match: `101LLMMM DDDDDDMM DDDDDDDD LITERAL`
    MedD,
    /// Literal + large distance match: `LLMMM111 DDDDDDDD DDDDDDDD LITERAL`
    LrgD,
    /// Literal + match with previous distance: `LLMMM110`
    PreD,
    /// Small match only (uses prev distance): `1111MMMM`
    SmlM,
    /// Large match only (uses prev distance): `11110000 MMMMMMMM`
    LrgM,
    /// Small literal only: `1110LLLL LITERAL`
    SmlL,
    /// Large literal only: `11100000 LLLLLLLL LITERAL`
    LrgL,
    /// End of stream marker (8 bytes)
    Eos,
    /// No-op (skip 1 byte)
    Nop,
    /// Undefined opcode (error)
    Undef,
}

/// Dispatch table mapping opcode byte to category.
///
/// Mining reference: Apple `lzvn_decode_base.c` `opc_tbl` array.
fn opc_category(opc: u8) -> Category {
    match opc {
        6 => Category::Eos,
        14 | 22 => Category::Nop,
        224 => Category::LrgL,
        225..=239 => Category::SmlL,
        240 => Category::LrgM,
        241..=255 => Category::SmlM,
        30 | 38 | 46 | 54 | 62 => Category::Undef,
        112..=127 => Category::Undef,
        208..=223 => Category::Undef,
        7 | 15 | 23 | 31 | 39 | 47 | 55 | 63 | 71 | 79 | 87 | 95 | 103 | 111 | 135 | 143 | 151
        | 159 | 199 | 207 => Category::LrgD,
        70 | 78 | 86 | 94 | 102 | 110 | 134 | 142 | 150 | 158 | 198 | 206 => Category::PreD,
        160..=191 => Category::MedD,
        _ => Category::SmlD,
    }
}

/// Extract `nbits` bits starting at `lsb` from a u8.
fn extract_u8(value: u8, lsb: u32, nbits: u32) -> u8 {
    (value >> lsb) & ((1u8 << nbits) - 1)
}

/// Extract `nbits` bits starting at `lsb` from a u16.
fn extract_u16(value: u16, lsb: u32, nbits: u32) -> u16 {
    (value >> lsb) & ((1u16 << nbits) - 1)
}

/// Load a little-endian u16 from a byte slice.
fn load_le16(data: &[u8]) -> u16 {
    (data[0] as u16) | ((data[1] as u16) << 8)
}

// ============================================================
// LZVN encoder
// ============================================================

/// Minimum source buffer size for LZVN compression.
/// Matches `LZVN_ENCODE_MIN_SRC_SIZE` from Apple's header.
const LZVN_ENCODE_MIN_SRC_SIZE: usize = 8;

/// Hash table parameters, matching Apple's `lzvn_encode_base.h`.
const LZVN_ENCODE_HASH_BITS: usize = 14;
const LZVN_ENCODE_HASH_VALUES: usize = 1 << LZVN_ENCODE_HASH_BITS;
const LZVN_ENCODE_MAX_DISTANCE: i32 = 0xffff;
const LZVN_ENCODE_MAX_LITERAL_BACKLOG: i32 = 400;
const LZVN_ENCODE_MIN_MARGIN: i32 = 8;

/// Hash table entry storing up to 4 candidate positions per hash.
///
/// Mining reference: Apple `lzvn_encode_base.h` `lzvn_encode_entry_type`.
#[derive(Clone, Copy)]
struct LzvnEncodeEntry {
    indices: [i32; 4],
    values: [u32; 4],
}

impl Default for LzvnEncodeEntry {
    fn default() -> Self {
        LzvnEncodeEntry {
            indices: [-LZVN_ENCODE_MAX_DISTANCE; 4],
            values: [0; 4],
        }
    }
}

/// A match found during LZVN encoding.
///
/// Mining reference: Apple `lzvn_encode_base.h` `lzvn_match_info`.
#[derive(Clone, Copy)]
struct LzvnMatchInfo {
    m_begin: i32,
    m_end: i32,
    m_len: i32,
    d: i32,
    k: i32,
}

impl LzvnMatchInfo {
    const fn new() -> Self {
        LzvnMatchInfo {
            m_begin: 0,
            m_end: 0,
            m_len: 0,
            d: 0,
            k: 0,
        }
    }
}

impl Default for LzvnMatchInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// LZVN encoder working state.
///
/// Mining reference: Apple `lzvn_encode_base.h` `lzvn_encoder_state`.
struct LzvnEncoderState<'a> {
    src: &'a [u8],
    src_begin: i32,
    src_end: i32,
    src_current: i32,
    src_current_end: i32,
    src_literal: i32,
    dst: Vec<u8>,
    pending: LzvnMatchInfo,
    d_prev: i32,
    table: Vec<LzvnEncodeEntry>,
}

/// Encode `src` into a raw LZVN bitstream.
///
/// This is the write-side counterpart to the `decode` function. The output
/// is a complete LZVN bitstream (with end-of-stream marker), suitable for
/// storage in a decmpfs resource fork with compression type 4.
///
/// Mining reference: Apple `lzvn_encode_base.c` `lzvn_encode_buffer`.
pub fn encode(src: &[u8]) -> Vec<u8> {
    if src.len() < LZVN_ENCODE_MIN_SRC_SIZE {
        let mut dst = Vec::new();
        emit_literal_block(&mut dst, src, 0, src.len() as i32);
        emit_end_of_stream(&mut dst);
        return dst;
    }

    let max_dst = src.len() + 8 + 8;
    let dst = Vec::with_capacity(max_dst);
    let table = vec![LzvnEncodeEntry::default(); LZVN_ENCODE_HASH_VALUES];

    let mut state = LzvnEncoderState {
        src,
        src_begin: 0,
        src_end: src.len() as i32,
        src_current: 0,
        src_current_end: src.len() as i32 - LZVN_ENCODE_MIN_MARGIN,
        src_literal: 0,
        dst,
        pending: LzvnMatchInfo::default(),
        d_prev: 0,
        table,
    };

    init_table(&mut state);
    encode_loop(&mut state);

    // Emit remaining literals.
    // NOTE: Any pending match is NOT emitted here, matching the C encoder's
    // behavior. The remaining literal bytes (which may overlap with the
    // pending match's range) are emitted uncompressed, preserving data
    // integrity at the cost of not compressing the final match.
    let remaining = state.src_end - state.src_literal;
    if remaining > 0 {
        emit_literal_block(&mut state.dst, state.src, state.src_literal, remaining);
        state.src_literal = state.src_end;
    }

    emit_end_of_stream(&mut state.dst);
    state.dst
}

/// Initialize the hash table with invalid positions and the first value.
///
/// Mining reference: Apple `lzvn_encode_base.c` `lzvn_init_table`.
fn init_table(state: &mut LzvnEncoderState) {
    let mut idx = -LZVN_ENCODE_MAX_DISTANCE;
    if idx < state.src_begin {
        idx = state.src_begin;
    }
    let value = if idx >= 0 && (idx as usize) < state.src.len() {
        load4_at(state.src, idx as usize)
    } else {
        0
    };
    let sidx = idx;
    for entry in state.table.iter_mut() {
        entry.indices = [sidx, sidx, sidx, sidx];
        entry.values = [value, value, value, value];
    }
}

/// Load 4 little-endian bytes from `src` at `offset`.
fn load4_at(src: &[u8], offset: usize) -> u32 {
    let offset = offset.min(src.len().saturating_sub(4));
    let b0 = src.get(offset).copied().unwrap_or(0) as u32;
    let b1 = src.get(offset + 1).copied().unwrap_or(0) as u32;
    let b2 = src.get(offset + 2).copied().unwrap_or(0) as u32;
    let b3 = src.get(offset + 3).copied().unwrap_or(0) as u32;
    b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
}

/// Hash function: takes a 32-bit value and produces a hash in range
/// `[0, LZVN_ENCODE_HASH_VALUES - 1]`.
///
/// Mining reference: Apple `lzvn_encode_base.c` `hash3i`.
fn lzvn_hash3i(i: u32) -> u32 {
    let i = i & 0x00ff_ffff;
    let h = i.wrapping_mul(65);
    (h >> 12) & (LZVN_ENCODE_HASH_VALUES as u32 - 1)
}

/// Return the number of zero bytes in `x`, starting from the least significant byte.
///
/// Mining reference: Apple `lzvn_encode_base.c` `trailing_zero_bytes`.
fn tzcnt_bytes(x: u32) -> i32 {
    if x == 0 {
        4
    } else {
        (x.trailing_zeros() >> 3) as i32
    }
}

/// Find a match at `m_begin` against a candidate at `m0_begin`.
///
/// Mining reference: Apple `lzvn_encode_base.c` `lzvn_find_match`
/// (and `lzvn_find_matchN` for the known-match variant).
fn find_match(
    src: &[u8],
    src_begin: i32,
    src_end: i32,
    l_begin: i32,
    m0_begin: i32,
    m_begin: i32,
    n_known: i32,
) -> Option<LzvnMatchInfo> {
    let src_len = src.len() as i32;
    if m_begin < 0 || m_begin >= src_len || m0_begin < 0 || m0_begin >= src_len {
        return None;
    }

    let mut n = n_known;
    if n < 3 {
        let vi = load4_at(src, m0_begin as usize);
        let vj = load4_at(src, m_begin as usize);
        n = tzcnt_bytes(vi ^ vj);
    }
    if n < 3 {
        return None;
    }

    let d = m_begin - m0_begin;
    if d <= 0 || d > LZVN_ENCODE_MAX_DISTANCE {
        return None;
    }

    // Expand forward
    let mut m_end = m_begin + n;
    while n == 4 && m_end + 4 < src_end {
        let vi = load4_at(src, m_end as usize);
        let vj = load4_at(src, (m_end - d) as usize);
        n = tzcnt_bytes(vi ^ vj);
        m_end += n;
    }

    // Expand backwards
    let mut m_b = m_begin;
    let mut m0_b = m0_begin;
    while m0_b > src_begin && m_b > l_begin {
        if src[(m0_b - 1) as usize] != src[(m_b - 1) as usize] {
            break;
        }
        m0_b -= 1;
        m_b -= 1;
    }

    let m_len = m_end - m_b;
    Some(LzvnMatchInfo {
        m_begin: m_b,
        m_end,
        m_len,
        d,
        k: m_len - if d < 0x600 { 2 } else { 3 },
    })
}

/// Main encoding loop: scan source, find matches, emit opcodes.
///
/// Mining reference: Apple `lzvn_encode_base.c` `lzvn_encode`.
fn encode_loop(state: &mut LzvnEncoderState) {
    const NO_MATCH: LzvnMatchInfo = LzvnMatchInfo::new();

    while state.src_current < state.src_current_end {
        let pos = state.src_current;
        let vi = load4_at(state.src, pos as usize);

        let h = lzvn_hash3i(vi) as usize;
        let e = state.table[h];

        // Rotate: insert new entry as most recent
        state.table[h].indices = [pos, e.indices[0], e.indices[1], e.indices[2]];
        state.table[h].values = [vi, e.values[0], e.values[1], e.values[2]];

        // Skip if still inside a previously emitted match
        if pos < state.src_literal {
            state.src_current += 1;
            continue;
        }

        let mut incoming = NO_MATCH;

        // Check 4 hash table candidates
        for k in 0..4 {
            let ik = e.indices[k];
            let nk = tzcnt_bytes(e.values[k] ^ vi);
            if let Some(m) = find_match(
                state.src,
                state.src_begin,
                state.src_end,
                state.src_literal,
                ik,
                pos,
                nk,
            ) {
                if m.k > incoming.k || (m.k == incoming.k && m.m_end > incoming.m_end + 1) {
                    incoming = m;
                }
            }
        }

        // Check previous distance
        if state.d_prev != 0 {
            if let Some(m1) = find_match(
                state.src,
                state.src_begin,
                state.src_end,
                state.src_literal,
                pos - state.d_prev,
                pos,
                0,
            ) {
                let mut m1 = m1;
                m1.k = m1.m_len - 1;
                if m1.k > incoming.k || (m1.k == incoming.k && m1.m_end > incoming.m_end + 1) {
                    incoming = m1;
                }
            }
        }

        // No match found: may need to flush literals if backlog is too high
        if incoming.m_len == 0 {
            if pos - state.src_literal >= LZVN_ENCODE_MAX_LITERAL_BACKLOG {
                if state.pending.m_len > 0 {
                    if emit_match(state, state.pending) == 0 {
                        break;
                    }
                    state.pending = NO_MATCH;
                } else {
                    if emit_literal(&mut state.dst, state.src, state.src_literal as usize, 271) == 0
                    {
                        break;
                    }
                    state.src_literal += 271;
                }
            }
            state.src_current += 1;
            continue;
        }

        // Decide between pending and incoming matches
        if state.pending.m_len == 0 {
            state.pending = incoming;
        } else if state.pending.m_end <= incoming.m_begin {
            // No overlap: emit pending, keep incoming
            if emit_match(state, state.pending) == 0 {
                break;
            }
            state.pending = incoming;
        } else {
            // Overlap: emit the better one
            if incoming.k > state.pending.k {
                state.pending = incoming;
            }
            if emit_match(state, state.pending) == 0 {
                break;
            }
            state.pending = NO_MATCH;
        }

        state.src_current += 1;
    }
}

/// Emit a match instruction, updating encoder state.
/// Returns bytes written, or 0 if output buffer is full.
///
/// Mining reference: Apple `lzvn_encode_base.c` `lzvn_emit_match`.
fn emit_match(state: &mut LzvnEncoderState, match_info: LzvnMatchInfo) -> i32 {
    let l = (match_info.m_begin - state.src_literal) as usize;
    let m = match_info.m_len as usize;
    let d = match_info.d as usize;
    let d_prev = state.d_prev as usize;

    let old_len = state.dst.len();
    emit(
        &mut state.dst,
        state.src,
        state.src_literal as usize,
        l,
        m,
        d,
        d_prev,
    );

    if state.dst.len() == old_len {
        return 0;
    }

    state.d_prev = match_info.d;
    state.src_literal = match_info.m_end;
    (state.dst.len() - old_len) as i32
}

/// Emit a literal-only block (no match).
/// Returns bytes written, or 0 if output buffer is full.
fn emit_literal(dst: &mut Vec<u8>, src: &[u8], src_literal: usize, l_in: i32) -> i32 {
    let mut l = l_in as usize;
    let mut p = src_literal;
    let start_len = dst.len();

    // Phase 1: Emit literals > 15 in chunks of up to 271 (lrg_l opcode)
    // Format: [0xE0, (x-16), literal bytes...]
    while l > 15 {
        let x = if l < 271 { l } else { 271 };
        dst.push(0xE0);
        dst.push((x - 16) as u8);
        let src_slice = &src[p..(p + x).min(src.len())];
        dst.extend_from_slice(src_slice);
        p += x;
        l -= x;
    }

    // Phase 2: Remaining literals 1-15 as sml_l (0xE0 | L)
    if l > 0 {
        dst.push(0xE0 + l as u8);
        let src_slice = &src[p..(p + l).min(src.len())];
        dst.extend_from_slice(src_slice);
    }

    (dst.len() - start_len) as i32
}

/// Emit a literal + match instruction.
///
/// Translates the LZVN `emit` function from `lzvn_encode_base.c`:
/// handles literal encoding (chunks of 16-271 via lrg_l, then 0-15 via sml_l
/// combined with the first match opcode), then encodes the match using the
/// appropriate distance category (sml_d, pre_d, lrg_d, or med_d).
///
/// Mining reference: Apple `lzvn_encode_base.c` `emit`.
fn emit(
    dst: &mut Vec<u8>,
    src: &[u8],
    src_literal: usize,
    l_in: usize,
    m_in: usize,
    d: usize,
    d_prev: usize,
) {
    let mut l = l_in;
    let mut m = m_in;
    let mut p = src_literal;

    // Phase 1: Emit literals > 15 in chunks of up to 271 (lrg_l opcode)
    while l > 15 {
        let x = if l < 271 { l } else { 271 };
        dst.push(0xE0);
        dst.push((x - 16) as u8);
        let src_slice = &src[p..(p + x).min(src.len())];
        dst.extend_from_slice(src_slice);
        p += x;
        l -= x;
    }

    // Phase 2: Handle remaining literals (0-15) combined with first match
    // If l > 3, emit a sml_l opcode first
    if l > 3 {
        dst.push(0xE0 + l as u8);
        let src_slice = &src[p..(p + l).min(src.len())];
        dst.extend_from_slice(src_slice);
        p += l;
        l = 0;
    }

    // Now l is 0-3: combine with first match
    // x = min(10 - 2*l, m), then m -= x, x -= 3
    let mut x = if m <= 10 - 2 * l { m } else { 10 - 2 * l };
    m -= x;
    x -= 3;

    // Load 4 bytes starting at the literal position (may read past end;
    // we only write `l` bytes)
    let literal = load4_at(src, p);

    // Emit the combined opcode based on distance
    if d == d_prev {
        if l == 0 {
            // sml_m: 1111MMMM where M = x + 3
            dst.push(0xF0 + (x + 3) as u8);
        } else {
            // pre_d: LLxxxy110
            dst.push((l << 6) as u8 | (x << 3) as u8 | 0x06);
        }
        for i in 0..l {
            dst.push(((literal >> (i * 8)) & 0xFF) as u8);
        }
    } else if d < 2048 - 2 * 256 {
        // Short distance: sml_d format LLxxxDDD DDDDDDDD
        dst.push((l << 6) as u8 | (x << 3) as u8 | ((d >> 8) & 0x07) as u8);
        dst.push((d & 0xFF) as u8);
        for i in 0..l {
            dst.push(((literal >> (i * 8)) & 0xFF) as u8);
        }
    } else if d >= (1 << 14) || m == 0 || (x + 3) + m > 34 {
        // Long distance or trivial: lrg_d format LLxxx111 DDDDDDDD DDDDDDDD
        dst.push((l << 6) as u8 | (x << 3) as u8 | 0x07);
        dst.push((d & 0xFF) as u8);
        dst.push((d >> 8) as u8);
        for i in 0..l {
            dst.push(((literal >> (i * 8)) & 0xFF) as u8);
        }
    } else {
        // Medium distance: med_d format 101LLMMM DDDDDDMM DDDDDDDD
        let x_total = x + m;
        m = 0;
        // First byte: 0xA0 + (x_total >> 2) + (L << 3)
        dst.push(0xA0 + (x_total >> 2) as u8 + (l << 3) as u8);
        // Next two bytes (little-endian, matching C store2): (D << 2) | (x_total & 3)
        let val = (d << 2) | (x_total & 3);
        dst.push((val & 0xFF) as u8);
        dst.push((val >> 8) as u8);
        for i in 0..l {
            dst.push(((literal >> (i * 8)) & 0xFF) as u8);
        }
    }

    // Phase 3: Emit remaining match length (M > 15 or leftover)
    while m > 15 {
        let x2 = if m < 271 { m } else { 271 };
        dst.push(0xF0);
        dst.push((x2 - 16) as u8);
        m -= x2;
    }
    if m > 0 {
        // sml_m: 0xF0 + m (where m is 0..15)
        dst.push(0xF0 + m as u8);
    }
}

/// Emit a literal-only block (wrapper for the final flush).
fn emit_literal_block(dst: &mut Vec<u8>, src: &[u8], src_literal: i32, l_in: i32) -> i32 {
    emit_literal(dst, src, src_literal as usize, l_in)
}

/// Emit end-of-stream marker (8 bytes: 0x06 followed by 7 zero bytes).
fn emit_end_of_stream(dst: &mut Vec<u8>) {
    dst.push(0x06);
    dst.extend_from_slice(&[0u8; 7]);
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_produces_empty_output() {
        let result = decode(&[], 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn undefined_opcode_returns_error() {
        let result = decode(&[0x1e], 0);
        assert!(result.is_err());
    }

    #[test]
    fn literal_then_eos_round_trip() {
        let data: Vec<u8> = vec![
            0xe5, b'H', b'e', b'l', b'l', b'o', 0x06, 0, 0, 0, 0, 0, 0, 0,
        ];
        let result = decode(&data, 5).unwrap();
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn undefined_opcode_error() {
        assert!(decode(&[0x1e], 0).is_err());
        assert!(decode(&[112], 0).is_err());
        assert!(decode(&[208], 0).is_err());
    }

    #[test]
    fn medd_opcodes_in_full_range() {
        // Verify opcodes 180-191 are classified as MedD, not SmlD
        for opc in 160u8..=191 {
            assert_eq!(
                opc_category(opc),
                Category::MedD,
                "opcode 0x{opc:02x} should be MedD"
            );
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let inputs: Vec<Vec<u8>> = vec![
            vec![],
            b"hello".to_vec(),
            b"hello world this is a test of compression".to_vec(),
            vec![b'a'; 100],
            vec![b'a'; 1000],
            b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc".to_vec(),
            (0..1000).map(|i| (i % 256) as u8).collect(),
            (0..1000).map(|i| (i % 17) as u8).collect(),
            vec![0xABu8; 1000],
            vec![0u8; 500],
            b"The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog.".to_vec(),
        ];

        for (i, input) in inputs.iter().enumerate() {
            let encoded = encode(input);
            let decoded = decode(&encoded, input.len() as u64).unwrap_or_else(|e| {
                panic!(
                    "round-trip failed for input {i}: decode error: {e:?}\nencoded: {:02x?}",
                    encoded
                )
            });
            assert_eq!(decoded, *input, "round-trip mismatch for input {i}");
        }
    }

    #[test]
    fn encode_preserves_data() {
        let input = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let encoded = encode(input);
        assert!(
            encoded.len() < input.len(),
            "compressed {} should be smaller than input ({})",
            encoded.len(),
            input.len()
        );
        let decoded = decode(&encoded, input.len() as u64).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn encode_empty_input() {
        let encoded = encode(b"");
        let decoded = decode(&encoded, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn encode_small_literal_then_match() {
        // "abc" + repeat -> should find matches at distance 3
        let input = b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabca";
        let encoded = encode(input);
        let decoded = decode(&encoded, input.len() as u64).unwrap();
        assert_eq!(decoded, input);
    }
}
