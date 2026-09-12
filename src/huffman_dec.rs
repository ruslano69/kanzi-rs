// Port of kanzi-go's HuffmanDecoder, V6 bitstream format only. Ported
// earlier this session into a standalone project (rust_huffman) and
// verified byte-exact there against the real Go decoder on real Go-encoder
// output; re-hosted here on top of this project's own bitio::BitReader
// (scalar path only -- the SIMD experiment from that project isn't needed
// for a correctness-first decoder).

use crate::bitio::BitReader;

const MAX_SYMBOL_SIZE: u32 = 12;
const DECODING_MASK: usize = (1usize << MAX_SYMBOL_SIZE) - 1;
const TABLE_SIZE: usize = 1usize << MAX_SYMBOL_SIZE;
const HUF_CHUNK_SIZE: usize = 1 << 14;
const BUFFER_SIZE: usize = 2 * HUF_CHUNK_SIZE;

const FULL_ALPHABET: u32 = 0;
const ALPHABET_0: u32 = 1;

fn read_var_int(br: &mut BitReader) -> u32 {
    let mut res: u32 = 0;
    let mut shift: u32 = 0;

    for _ in 0..4 {
        let value = br.read_bits(8) as u32;
        res |= (value & 0x7F) << shift;

        if value < 128 {
            return res;
        }

        shift += 7;
    }

    let value = br.read_bits(8) as u32;
    res | ((value & 0x0F) << 28)
}

fn decode_alphabet(br: &mut BitReader) -> Vec<u8> {
    if br.read_bit() == FULL_ALPHABET {
        if br.read_bit() == ALPHABET_0 {
            return Vec::new();
        }
        return (0u32..=255).map(|i| i as u8).collect();
    }

    let last_mask = br.read_bits(5) as usize;
    let mut masks = [0u8; 32];
    br.read_array(&mut masks, 8 * (last_mask + 1));

    let mut symbols = Vec::with_capacity(64);

    for i in 0..=last_mask {
        for j in 0..8u32 {
            if (masks[i] >> j) & 1 == 1 {
                symbols.push((i * 8) as u8 + j as u8);
            }
        }
    }

    symbols
}

fn exp_golomb_decode_signed_byte(br: &mut BitReader) -> i8 {
    if br.read_bit() == 1 {
        return 0;
    }

    let mut log2 = 1u32;

    while br.read_bit() != 1 {
        log2 += 1;
    }

    let max_log2 = 7u32;

    if log2 > max_log2 {
        log2 = max_log2;
    }

    let val = br.read_bits(log2 + 1);
    let mut res: u64 = (val >> 1).wrapping_add(1u64 << log2).wrapping_sub(1);

    if val & 1 == 1 {
        res = (!res).wrapping_add(1);
    }

    res as u8 as i8
}

fn generate_canonical_codes(sizes: &[u8; 256], symbols: &mut [u8]) -> [u16; 256] {
    let mut codes = [0u16; 256];

    if symbols.is_empty() {
        return codes;
    }

    if symbols.len() > 1 {
        symbols.sort_by_key(|&s| (sizes[s as usize], s));
    }

    let mut code: u16 = 0;
    let mut cur_len = sizes[symbols[0] as usize];

    for &s in symbols.iter() {
        code <<= sizes[s as usize] - cur_len;
        cur_len = sizes[s as usize];
        codes[s as usize] = code;
        code += 1;
    }

    codes
}

/// Port of Go's buildDecodingTable, including its bounds check (Go:
/// `if int(end) > len(this.table) { return false }`) -- dropping that
/// check let a corrupted code length turn into an out-of-bounds `table`
/// write instead of a clean decode error. `sizes[s]` is guaranteed
/// in (0, MAX_SYMBOL_SIZE] by the caller's own validation (mirroring
/// Go's readLengths check) before this ever runs, but the bounds check
/// is kept anyway, exactly like Go keeps both -- belt and suspenders.
fn build_decoding_table(sizes: &[u8; 256], codes: &[u16; 256], symbols: &[u8], table: &mut [u16]) -> bool {
    for v in table.iter_mut() {
        *v = 7;
    }

    let shift = MAX_SYMBOL_SIZE;

    for &s in symbols {
        let len = sizes[s as usize] as u32;

        if len == 0 || len > shift {
            return false;
        }

        let idx = (codes[s as usize] as u32) << (shift - len);
        let end = idx + (1u32 << (shift - len));

        if end as usize > table.len() {
            return false;
        }

        let val = ((s as u16) << 8) | (sizes[s as usize] as u16);

        for j in idx..end {
            table[j as usize] = val;
        }
    }

    true
}

/// Loads one 8-byte big-endian refill word and folds it into `state`,
/// exactly like Go's HuffmanDecoder.readState. All arithmetic is Go-uint8
/// wrapping on purpose (see the wrapping note on the decode loop below).
#[inline]
fn read_state(buffer: &[u8], state: &mut u64, idx: &mut usize, bits: u8) -> u8 {
    let shift: u8 = (56u8.wrapping_sub(bits)) & !7u8;
    // Zero-padded load: valid streams always have the bytes (stride layout
    // + guard clearing guarantee it); corrupt input must fail safely via the
    // end-of-chunk size validation, never via undefined behavior (Go would
    // panic on slice bounds here).
    let mut w = [0u8; 8];
    let avail = buffer.len().saturating_sub(*idx).min(8);
    w[..avail].copy_from_slice(&buffer[*idx..*idx + avail]);
    let word = u64::from_be_bytes(w);
    // Go: (*state << shift) | (word >> (64-shift)). Go shifts with count>=64
    // yield 0 (no panic); Rust panics in debug on overshift, so guard both
    // shift==0 (>>64) and shift>=64 explicitly.
    let (shifted_state, new_bits) = if shift == 0 {
        (*state, 0)
    } else if shift >= 64 {
        (0, 0)
    } else {
        ((*state << shift), word >> (64 - shift as u32))
    };
    *state = shifted_state | new_bits;
    *idx += (shift >> 3) as usize;
    bits.wrapping_add(shift).wrapping_sub(MAX_SYMBOL_SIZE as u8)
}

/// Single-symbol table lookup: Go's `(state>>bs)&MASK`. Go shifts with
/// count>=64 yield 0; Rust would panic (debug) or mask the count (release),
/// so guard explicitly for exactness on every input, valid or corrupt.
#[inline]
fn lookup(table: &[u16], state: u64, bs: u8) -> u16 {
    let idx = if bs >= 64 {
        0
    } else {
        ((state >> bs) as usize) & DECODING_MASK
    };
    table[idx]
}

// NOTE on bit alignment: unlike Go's concurrent reader (which copies each
// block's bits into a fresh byte-aligned local buffer before constructing a
// per-block bitstream), this decoder reads directly off one continuous
// BitReader spanning the whole file. That's equivalent -- Go's copy step
// re-bases a block's bits to local offset 0 but preserves their exact
// sequence, and our BitReader already supports arbitrary (non-byte-aligned)
// starting positions -- so the Huffman payload here begins wherever the
// shared reader's cursor happens to be, not necessarily byte-aligned.
pub struct HuffmanDecoderV6 {
    buffer: Vec<u8>,
    table: Vec<u16>,
}

impl HuffmanDecoderV6 {
    pub fn new() -> Self {
        HuffmanDecoderV6 {
            buffer: vec![0u8; BUFFER_SIZE],
            table: vec![0u16; TABLE_SIZE],
        }
    }

    pub fn decode(&mut self, br: &mut BitReader, out: &mut [u8]) -> Result<(), String> {
        let end = out.len();
        let mut start_chunk = 0;

        while start_chunk < end {
            let size_chunk = HUF_CHUNK_SIZE.min(end - start_chunk);

            if size_chunk < 32 {
                br.read_array(
                    &mut out[start_chunk..start_chunk + size_chunk],
                    8 * size_chunk,
                );
            } else {
                let mut symbols = decode_alphabet(br);
                let count = symbols.len();

                if count == 0 {
                    break;
                }

                let mut sizes = [0u8; 256];
                let mut cur_size: i8 = 2;

                for &s in &symbols {
                    let delta = exp_golomb_decode_signed_byte(br);
                    cur_size = cur_size.wrapping_add(delta);

                    // Port of Go's readLengths check. Without this, a
                    // corrupted delta can leave `cur_size` <= 0 or above
                    // MAX_SYMBOL_SIZE, which build_decoding_table would
                    // otherwise turn into a bogus shift amount and an
                    // out-of-bounds table write instead of a clean error.
                    if cur_size <= 0 || cur_size as u32 > MAX_SYMBOL_SIZE {
                        return Err(format!(
                            "Invalid bitstream: incorrect size {} for Huffman symbol {}",
                            cur_size, s
                        ));
                    }

                    sizes[s as usize] = cur_size as u8;
                }

                if count == 1 {
                    let val = symbols[0];

                    for b in out[start_chunk..start_chunk + size_chunk].iter_mut() {
                        *b = val;
                    }
                } else {
                    let codes = generate_canonical_codes(&sizes, &mut symbols);

                    if !build_decoding_table(&sizes, &codes, &symbols, &mut self.table) {
                        return Err("Invalid bitstream: incorrect symbol size".to_string());
                    }

                    self.decode_chunk_v6(
                        br,
                        &mut out[start_chunk..start_chunk + size_chunk],
                        size_chunk,
                    )?;
                }
            }

            start_chunk += size_chunk;
        }

        Ok(())
    }

    fn decode_chunk_v6(
        &mut self,
        br: &mut BitReader,
        block: &mut [u8],
        count: usize,
    ) -> Result<(), String> {
        let sz_bits0 = read_var_int(br) as usize;
        let sz_bits1 = read_var_int(br) as usize;
        let sz_bits2 = read_var_int(br) as usize;
        let sz_bits3 = read_var_int(br) as usize;

        let stride = self.buffer.len() / 4;
        let (base0, base1, base2, base3) = (0usize, stride, 2 * stride, 3 * stride);
        let (mut idx0, mut idx1, mut idx2, mut idx3) = (base0, base1, base2, base3);

        br.read_array(&mut self.buffer[idx0..], sz_bits0);
        br.read_array(&mut self.buffer[idx1..], sz_bits1);
        br.read_array(&mut self.buffer[idx2..], sz_bits2);
        br.read_array(&mut self.buffer[idx3..], sz_bits3);

        // Match Go's decodeChunkV6 guard clearing: read_state always loads a
        // full 8-byte big-endian word, so bytes past each stream's payload
        // end must read as deterministic zeros, not stale data from a
        // previous chunk reusing this buffer.
        for (idx, sz_bits) in [
            (idx0, sz_bits0),
            (idx1, sz_bits1),
            (idx2, sz_bits2),
            (idx3, sz_bits3),
        ] {
            let sz = idx + ((sz_bits + 7) >> 3);
            let end = (sz + 8).min(idx + stride);
            if sz < end {
                self.buffer[sz..end].fill(0);
            }
        }

        let mut state0 = 0u64;
        let mut state1 = 0u64;
        let mut state2 = 0u64;
        let mut state3 = 0u64;
        let mut bits0 = 0u8;
        let mut bits1 = 0u8;
        let mut bits2 = 0u8;
        let mut bits3 = 0u8;
        let mut bs0;
        let mut bs1;
        let mut bs2;
        let mut bs3;

        let sz_frag = count / 4;
        let (b0, b1, b2, b3) = (0usize, sz_frag, 2 * sz_frag, 3 * sz_frag);
        let mut n = 0usize;

        let buffer = &self.buffer;
        let table = &self.table;

        // NOTE on wrapping arithmetic: every `bs`/`bits` update below uses
        // wrapping (mod-256) semantics to match Go's uint8 arithmetic exactly.
        // A symbol length may transiently exceed the currently refilled bit
        // index (bs goes "negative", i.e. wraps); the deficit is reconciled
        // by the next read_state refill. This happens on ordinary valid
        // streams (long codes) -- checked/panicking arithmetic breaks
        // decoding there, so wrapping is required, not just tolerated.
        // Corrupt streams are still rejected by the end-of-chunk size check.
        if sz_frag >= 4 {
            while n < sz_frag - 4 {
                bs0 = read_state(buffer, &mut state0, &mut idx0, bits0);
                bs1 = read_state(buffer, &mut state1, &mut idx1, bits1);
                bs2 = read_state(buffer, &mut state2, &mut idx2, bits2);
                bs3 = read_state(buffer, &mut state3, &mut idx3, bits3);

                let val00 = lookup(table, state0, bs0);
                bs0 = bs0.wrapping_sub(val00 as u8);
                let val10 = lookup(table, state1, bs1);
                bs1 = bs1.wrapping_sub(val10 as u8);
                let val20 = lookup(table, state2, bs2);
                bs2 = bs2.wrapping_sub(val20 as u8);
                let val30 = lookup(table, state3, bs3);
                bs3 = bs3.wrapping_sub(val30 as u8);
                let val01 = lookup(table, state0, bs0);
                bs0 = bs0.wrapping_sub(val01 as u8);
                let val11 = lookup(table, state1, bs1);
                bs1 = bs1.wrapping_sub(val11 as u8);
                let val21 = lookup(table, state2, bs2);
                bs2 = bs2.wrapping_sub(val21 as u8);
                let val31 = lookup(table, state3, bs3);
                bs3 = bs3.wrapping_sub(val31 as u8);
                let val02 = lookup(table, state0, bs0);
                bs0 = bs0.wrapping_sub(val02 as u8);
                let val12 = lookup(table, state1, bs1);
                bs1 = bs1.wrapping_sub(val12 as u8);
                let val22 = lookup(table, state2, bs2);
                bs2 = bs2.wrapping_sub(val22 as u8);
                let val32 = lookup(table, state3, bs3);
                bs3 = bs3.wrapping_sub(val32 as u8);
                let val03 = lookup(table, state0, bs0);
                bs0 = bs0.wrapping_sub(val03 as u8);
                let val13 = lookup(table, state1, bs1);
                bs1 = bs1.wrapping_sub(val13 as u8);
                let val23 = lookup(table, state2, bs2);
                bs2 = bs2.wrapping_sub(val23 as u8);
                let val33 = lookup(table, state3, bs3);
                bs3 = bs3.wrapping_sub(val33 as u8);

                bits0 = bs0.wrapping_add(MAX_SYMBOL_SIZE as u8);
                bits1 = bs1.wrapping_add(MAX_SYMBOL_SIZE as u8);
                bits2 = bs2.wrapping_add(MAX_SYMBOL_SIZE as u8);
                bits3 = bs3.wrapping_add(MAX_SYMBOL_SIZE as u8);

                block[b0 + n] = (val00 >> 8) as u8;
                block[b1 + n] = (val10 >> 8) as u8;
                block[b2 + n] = (val20 >> 8) as u8;
                block[b3 + n] = (val30 >> 8) as u8;
                block[b0 + n + 1] = (val01 >> 8) as u8;
                block[b1 + n + 1] = (val11 >> 8) as u8;
                block[b2 + n + 1] = (val21 >> 8) as u8;
                block[b3 + n + 1] = (val31 >> 8) as u8;
                block[b0 + n + 2] = (val02 >> 8) as u8;
                block[b1 + n + 2] = (val12 >> 8) as u8;
                block[b2 + n + 2] = (val22 >> 8) as u8;
                block[b3 + n + 2] = (val32 >> 8) as u8;
                block[b0 + n + 3] = (val03 >> 8) as u8;
                block[b1 + n + 3] = (val13 >> 8) as u8;
                block[b2 + n + 3] = (val23 >> 8) as u8;
                block[b3 + n + 3] = (val33 >> 8) as u8;

                n += 4;
            }
        }

        bs0 = read_state(buffer, &mut state0, &mut idx0, bits0);
        bs1 = read_state(buffer, &mut state1, &mut idx1, bits1);
        bs2 = read_state(buffer, &mut state2, &mut idx2, bits2);
        bs3 = read_state(buffer, &mut state3, &mut idx3, bits3);

        while n < sz_frag {
            let val0 = lookup(table, state0, bs0);
            bs0 = bs0.wrapping_sub(val0 as u8);
            let val1 = lookup(table, state1, bs1);
            bs1 = bs1.wrapping_sub(val1 as u8);
            let val2 = lookup(table, state2, bs2);
            bs2 = bs2.wrapping_sub(val2 as u8);
            let val3 = lookup(table, state3, bs3);
            bs3 = bs3.wrapping_sub(val3 as u8);

            block[b0 + n] = (val0 >> 8) as u8;
            block[b1 + n] = (val1 >> 8) as u8;
            block[b2 + n] = (val2 >> 8) as u8;
            block[b3 + n] = (val3 >> 8) as u8;
            n += 1;
        }

        let count4 = 4 * sz_frag;

        for i in count4..count {
            block[i] = br.read_bits(8) as u8;
        }

        // Same end-of-chunk accounting as Go's decodeChunkV6: the bytes
        // consumed from each stream buffer minus the leftover bit index must
        // equal the transmitted stream size. (bs+12 is Go-uint8 wrapping add,
        // hence wrapping_add here too.)
        let consumed0 =
            ((idx0 - base0) << 3) as i64 - bs0.wrapping_add(MAX_SYMBOL_SIZE as u8) as i64;
        let consumed1 =
            ((idx1 - base1) << 3) as i64 - bs1.wrapping_add(MAX_SYMBOL_SIZE as u8) as i64;
        let consumed2 =
            ((idx2 - base2) << 3) as i64 - bs2.wrapping_add(MAX_SYMBOL_SIZE as u8) as i64;
        let consumed3 =
            ((idx3 - base3) << 3) as i64 - bs3.wrapping_add(MAX_SYMBOL_SIZE as u8) as i64;

        if consumed0 != sz_bits0 as i64
            || consumed1 != sz_bits1 as i64
            || consumed2 != sz_bits2 as i64
            || consumed3 != sz_bits3 as i64
        {
            return Err("Invalid bitstream: incorrect Huffman stream size".to_string());
        }

        Ok(())
    }
}
