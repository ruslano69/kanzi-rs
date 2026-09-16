// Port of kanzi-go's rolzCodec1 (transform/ROLZCodec.go) -- the Reduced
// Offset Lempel-Ziv transform used at level 4 (TEXT+UTF+EXE+PACK+MM+ROLZ).
// Only the codec1 variant (ANS-coded literals/matches, logPosChecks=4) is
// ported: that is what NewROLZCodecWithCtx selects for the "ROLZ" transform
// name. rolzCodec2 ("ROLZX", CM-coded, levels 6+) is not ported.
//
// ctx["dataType"] handling mirrors Go: Forward detects the type when
// Undefined (side effect returned alongside Ok/Err like text_codec.rs and
// alias.rs do); Inverse only needs the v4+ flag layout (this project is
// v7-only, and Go takes the same branch for versions 4 through 7).
//
// BOUNDS FIDELITY NOTE: Go reads keys/hashes with two-index slices off the
// current chunk (`buf[srcIdx:srcIdx+8]` etc.). For slices Go permits the high
// index to reach the slice CAPACITY, so near the chunk end those reads spill
// into the following source bytes (or past len, where Go would panic only if
// past cap). Rust slices panic past len. This port therefore reads keys,
// hashes and match words from the FULL source at absolute positions with a
// zero-padded safe loader: byte-identical to Go whenever Go's reads land
// inside len(src) (all real inputs -- verified byte-exact against Go during
// porting), graceful instead of panicking otherwise.

use crate::ans::{AnsDecoder, AnsEncoder};
use crate::bitio::{BitReader, BitWriter};
use crate::datatype::{detect_simple_type, DataType};

const HASH_SIZE: usize = 1 << 16;
const MIN_MATCH3: i64 = 3;
const MIN_MATCH4: i64 = 4;
const MIN_MATCH7: i64 = 7;
const MAX_MATCH1: i64 = MIN_MATCH3 + 65535;
const LOG_POS_CHECKS: u32 = 4;
const CHUNK_SIZE: usize = 16 * 1024 * 1024;
const HASH_MASK: u32 = !(CHUNK_SIZE as u32 - 1);
const HASH_SEED: u64 = 200_002_979;
const MAX_BLOCK_SIZE: usize = 1 << 30;
const MIN_BLOCK_SIZE: usize = 64;

pub fn max_encoded_len(src_len: usize) -> usize {
    if src_len <= 512 {
        src_len + 64
    } else {
        src_len
    }
}

/// Zero-padded little-endian loaders (see module note). In-bounds reads are
/// exactly Go's bytes; out-of-bounds tails read as zeros.
#[inline]
fn le16_at(data: &[u8], pos: usize) -> u32 {
    let mut w = [0u8; 2];
    let n = data.len().saturating_sub(pos).min(2);
    w[..n].copy_from_slice(&data[pos..pos + n]);
    u16::from_le_bytes(w) as u32
}

#[inline]
fn le32_at(data: &[u8], pos: usize) -> u32 {
    let mut w = [0u8; 4];
    let n = data.len().saturating_sub(pos).min(4);
    w[..n].copy_from_slice(&data[pos..pos + n]);
    u32::from_le_bytes(w)
}

#[inline]
fn le64_at(data: &[u8], pos: usize) -> u64 {
    let mut w = [0u8; 8];
    let n = data.len().saturating_sub(pos).min(8);
    w[..n].copy_from_slice(&data[pos..pos + n]);
    u64::from_le_bytes(w)
}

#[inline]
fn get_key1_at(data: &[u8], pos: usize) -> u32 {
    le16_at(data, pos)
}

#[inline]
fn get_key2_at(data: &[u8], pos: usize) -> u32 {
    (((le64_at(data, pos) as u64).wrapping_mul(HASH_SEED)) >> 40) as u32 & 0xFFFF
}

#[inline]
fn rolz_hash_at(data: &[u8], pos: usize) -> u32 {
    (le32_at(data, pos)
        .wrapping_shl(8)
        .wrapping_mul(HASH_SEED as u32))
        & HASH_MASK
}

fn emit_length_rolz(block: &mut [u8], len: usize) -> usize {
    let mut idx = 0;

    if len >= 1 << 7 {
        if len >= 1 << 14 {
            if len >= 1 << 21 {
                block[idx] = (0x80 | (len >> 21)) as u8;
                idx += 1;
            }

            block[idx] = (0x80 | (len >> 14)) as u8;
            idx += 1;
        }

        block[idx] = (0x80 | (len >> 7)) as u8;
        idx += 1;
    }

    block[idx] = (len & 0x7F) as u8;
    idx + 1
}

// Returns (litLen, bytes consumed); lenBuf must hold 4 bytes (callers size
// mLenBuf with +4 padding exactly like Go).
fn read_length_rolz(len_buf: &[u8]) -> (usize, usize) {
    let mut next = len_buf[0];
    let mut idx = 1;
    let mut lit_len = (next & 0x7F) as usize;

    if next >= 128 {
        next = len_buf[idx];
        idx += 1;
        lit_len = (lit_len << 7) | (next & 0x7F) as usize;

        if next >= 128 {
            next = len_buf[idx];
            idx += 1;
            lit_len = (lit_len << 7) | (next & 0x7F) as usize;

            if next >= 128 {
                next = len_buf[idx];
                idx += 1;
                lit_len = (lit_len << 7) | (next & 0x7F) as usize;
            }
        }
    }

    (lit_len, idx)
}

pub struct RolzCodec {
    matches: Vec<u32>,
    counters: Vec<i32>,
    min_match: i64,
    lit_buf: Vec<u8>,
    len_buf: Vec<u8>,
    m_idx_buf: Vec<u8>,
    tk_buf: Vec<u8>,
}

impl RolzCodec {
    pub fn new() -> Self {
        RolzCodec {
            matches: Vec::new(),
            counters: vec![0i32; HASH_SIZE],
            min_match: MIN_MATCH3,
            lit_buf: Vec::new(),
            len_buf: Vec::new(),
            m_idx_buf: Vec::new(),
            tk_buf: Vec::new(),
        }
    }

    /// rolzCodec1.findMatch: returns (match position index, match length
    /// above minMatch) or (-1, -1). `buf` is the FULL source and `pos` /
    /// `ref` are absolute positions; match lengths stay capped at the chunk
    /// end (semantic), while byte fetches use the zero-padded safe loader
    /// (see module note).
    fn find_match(
        &self,
        buf: &[u8],
        chunk_end: usize,
        pos: usize,
        hash32: u32,
        counter: i32,
        matches: &[u32],
    ) -> (i64, i64) {
        let mut max_match = (MAX_MATCH1).min((chunk_end - pos) as i64);

        if max_match < self.min_match {
            return (-1, -1);
        }

        max_match -= 8;
        let mut best_len = 0i64;
        let mut best_idx = -1i64;
        let pos_checks = 1i32 << LOG_POS_CHECKS;
        let mask_checks = pos_checks - 1;

        // Check all recorded positions
        let mut i = counter;

        while i > counter - pos_checks {
            let r = matches[(i & mask_checks) as usize];

            // Hash check may save a memory access ...
            if r & HASH_MASK != hash32 {
                i -= 1;
                continue;
            }

            let ref_pos = (r & !HASH_MASK) as usize;

            if byte_at(buf, ref_pos + best_len as usize) != byte_at(buf, pos + best_len as usize) {
                i -= 1;
                continue;
            }

            let mut n = 0i64;

            while n < max_match {
                let diff = le64_at(buf, ref_pos + n as usize) ^ le64_at(buf, pos + n as usize);

                if diff != 0 {
                    n += (diff.trailing_zeros() >> 3) as i64;
                    break;
                }

                n += 8;
            }

            if n > best_len {
                best_idx = i as i64;
                best_len = n;
            }

            i -= 1;
        }

        if best_len < self.min_match {
            return (-1, -1);
        }

        (counter as i64 - best_idx, best_len - self.min_match)
    }

    /// Forward transform. `dt_in` is the pipeline's current data type.
    /// Returns (bytes_read, bytes_written, data_type) with the data type on
    /// both paths (Go's ctx["dataType"] side effect).
    pub fn forward(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        dt_in: DataType,
    ) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0, dt_in));
        }

        if src.len() < MIN_BLOCK_SIZE {
            return Err(("ROLZ codec forward transform skip: block too small", dt_in));
        }

        if src.len() > MAX_BLOCK_SIZE {
            return Err(("ROLZ codec forward transform skip: block too big", dt_in));
        }

        if dst.len() < max_encoded_len(src.len()) {
            return Err((
                "ROLZ codec forward transform failed: output buffer is too small",
                dt_in,
            ));
        }

        let src_end = src.len() - 4;
        dst[0..4].copy_from_slice(&(src.len() as u32).to_be_bytes());
        let mut size_chunk = src.len().min(CHUNK_SIZE);

        let mut lit_order = 1u32;

        if src.len() < 1 << 17 {
            lit_order = 0;
        }

        let mut flags = lit_order as u8;
        self.min_match = MIN_MATCH3;
        let mut delta = 2usize;
        let mut dt = dt_in;

        if dt == DataType::Undefined {
            let mut freqs0 = [0i32; 256];
            for &b in src {
                freqs0[b as usize] += 1;
            }
            let detected = detect_simple_type(src.len(), &freqs0);

            if detected != DataType::Undefined {
                dt = detected;
            }
        }

        if dt == DataType::Exe {
            delta = 3;
            flags |= 8;
        } else if dt == DataType::Dna {
            delta = 8;
            self.min_match = MIN_MATCH7;
            flags |= 4;
        } else if dt == DataType::Multimedia {
            delta = 8;
            self.min_match = MIN_MATCH4;
            flags |= 2;
        }

        flags |= (LOG_POS_CHECKS << 4) as u8;
        dst[4] = flags;
        let mut dst_idx = 5usize;

        if self.matches.is_empty() {
            self.matches = vec![0u32; HASH_SIZE << LOG_POS_CHECKS];
        }

        // Scratch buffers sized per chunk like Go (reused across chunks).
        let need_lit = max_encoded_len(size_chunk);
        if self.lit_buf.len() < need_lit {
            self.lit_buf = vec![0u8; need_lit];
        }
        if self.len_buf.len() < size_chunk / 5 + 8 {
            self.len_buf = vec![0u8; size_chunk / 5 + 8];
        }
        if self.m_idx_buf.len() < size_chunk / 4 + 8 {
            self.m_idx_buf = vec![0u8; size_chunk / 4 + 8];
        }
        if self.tk_buf.len() < size_chunk / 4 + 8 {
            self.tk_buf = vec![0u8; size_chunk / 4 + 8];
        }

        self.counters.fill(0);

        // Main loop
        let mut start_chunk = 0usize;
        let mut err: Option<&'static str> = None;

        while start_chunk < src_end && err.is_none() {
            let mut lit_idx = 0usize;
            let mut len_idx = 0usize;
            let mut m_idx = 0usize;
            let mut tk_idx = 0usize;

            self.matches.fill(0);
            let mut end_chunk = start_chunk + size_chunk;

            if end_chunk >= src_end {
                end_chunk = src_end;
                size_chunk = end_chunk - start_chunk;
            }

            let chunk_start = start_chunk;
            let chunk_end = end_chunk;
            let mut src_idx = 0usize;
            let n_first = (src_end - start_chunk).min(8);

            for _ in 0..n_first {
                self.lit_buf[lit_idx] = src[chunk_start + src_idx];
                lit_idx += 1;
                src_idx += 1;
            }

            let mut first_lit_idx = src_idx;
            let mut src_inc = 0i64;

            // Next chunk
            while src_idx < size_chunk {
                let abs = chunk_start + src_idx;
                let key = if self.min_match == MIN_MATCH3 {
                    get_key1_at(src, abs.wrapping_sub(delta))
                } else {
                    get_key2_at(src, abs.wrapping_sub(delta))
                };

                let m_start = (key as usize) << LOG_POS_CHECKS;
                let hash32 = rolz_hash_at(src, abs);
                let counter = self.counters[(key & 0xFFFF) as usize];
                let (mut match_idx, mut match_len) = self.find_match(
                    src,
                    chunk_end,
                    abs,
                    hash32,
                    counter,
                    &self.matches[m_start..m_start + (1 << LOG_POS_CHECKS)],
                );

                // Register current position
                let c = (counter + 1) & ((1 << LOG_POS_CHECKS) as i32 - 1);
                self.counters[(key & 0xFFFF) as usize] = c;
                self.matches[m_start + c as usize] = hash32 | abs as u32;

                if match_idx < 0 {
                    src_idx += 1 + ((src_inc >> 6) as usize);
                    src_inc += 1;
                    continue;
                }

                // Check if better match at next position
                let src_idx1 = src_idx + 1;
                let abs1 = chunk_start + src_idx1;
                let key1 = if self.min_match == MIN_MATCH3 {
                    get_key1_at(src, abs1.wrapping_sub(delta))
                } else {
                    get_key2_at(src, abs1.wrapping_sub(delta))
                };

                let m_start1 = (key1 as usize) << LOG_POS_CHECKS;
                let hash32_1 = rolz_hash_at(src, abs1);
                let counter1 = self.counters[(key1 & 0xFFFF) as usize];
                let (match_idx1, match_len1) = self.find_match(
                    src,
                    chunk_end,
                    abs1,
                    hash32_1,
                    counter1,
                    &self.matches[m_start1..m_start1 + (1 << LOG_POS_CHECKS)],
                );

                if match_idx1 >= 0 && match_len1 > match_len {
                    // New match is better
                    match_idx = match_idx1;
                    match_len = match_len1;
                    src_idx = src_idx1;

                    // Register current position
                    let c = (counter1 + 1) & ((1 << LOG_POS_CHECKS) as i32 - 1);
                    self.counters[(key1 & 0xFFFF) as usize] = c;
                    self.matches[m_start1 + c as usize] = hash32_1 | (chunk_start + src_idx) as u32;
                }

                // token LLLLLMMM -> L lit length, M match length
                let lit_len = src_idx - first_lit_idx;
                let mut token: u8;

                if match_len >= 7 {
                    token = 7;
                    len_idx +=
                        emit_length_rolz(&mut self.len_buf[len_idx..], (match_len - 7) as usize);
                } else {
                    token = match_len as u8;
                }

                // Emit literals
                if lit_len > 0 {
                    if lit_len >= 31 {
                        token |= 0xF8;
                        len_idx += emit_length_rolz(&mut self.len_buf[len_idx..], lit_len - 31);
                    } else {
                        token |= (lit_len << 3) as u8;
                    }

                    self.lit_buf[lit_idx..lit_idx + lit_len].copy_from_slice(
                        &src[chunk_start + first_lit_idx..chunk_start + first_lit_idx + lit_len],
                    );
                    lit_idx += lit_len;
                }

                self.tk_buf[tk_idx] = token;
                tk_idx += 1;

                // Emit match index
                self.m_idx_buf[m_idx] = (match_idx & 0xFF) as u8;
                m_idx += 1;
                src_idx += (match_len + self.min_match) as usize;
                first_lit_idx = src_idx;
                src_inc = 0;
            }

            // Emit last chunk literals
            src_idx = size_chunk;
            let lit_len = src_idx - first_lit_idx;

            if tk_idx != 0 {
                // At least one match to emit
                if lit_len >= 31 {
                    self.tk_buf[tk_idx] = 0xF8;
                } else {
                    self.tk_buf[tk_idx] = ((lit_len << 3) & 0xFF) as u8;
                }

                tk_idx += 1;
            }

            // Emit literals
            if lit_len > 0 {
                if lit_len >= 31 {
                    len_idx += emit_length_rolz(&mut self.len_buf[len_idx..], lit_len - 31);
                }

                self.lit_buf[lit_idx..lit_idx + lit_len].copy_from_slice(
                    &src[chunk_start + first_lit_idx..chunk_start + first_lit_idx + lit_len],
                );
                lit_idx += lit_len;
            }

            // Encode literal, length and match index buffers through ANS
            // over one shared bitstream (mirrors Go's BufferStream scoping).
            let mut bw = BitWriter::new();
            bw.write_bits(lit_idx as u64, 32);
            bw.write_bits(tk_idx as u64, 32);
            bw.write_bits(len_idx as u64, 32);
            bw.write_bits(m_idx as u64, 32);

            let mut lit_enc = match AnsEncoder::new(lit_order, None, None) {
                Ok(e) => e,
                Err(_) => {
                    err = Some("ROLZ codec forward transform failed: invalid litOrder");
                    break;
                }
            };
            lit_enc.write(&self.lit_buf[..lit_idx], &mut bw);

            let mut m_enc = match AnsEncoder::new(0, Some(32768), None) {
                Ok(e) => e,
                Err(_) => {
                    err = Some("ROLZ codec forward transform failed: invalid mEnc params");
                    break;
                }
            };
            m_enc.write(&self.tk_buf[..tk_idx], &mut bw);
            m_enc.write(&self.len_buf[..len_idx], &mut bw);
            m_enc.write(&self.m_idx_buf[..m_idx], &mut bw);
            let (chunk_bytes, _) = bw.finish_with_len();

            if dst_idx + chunk_bytes.len() > dst.len() {
                err = Some("ROLZ codec forward transform skip: destination buffer too small");
                break;
            }

            dst[dst_idx..dst_idx + chunk_bytes.len()].copy_from_slice(&chunk_bytes);
            dst_idx += chunk_bytes.len();
            start_chunk = end_chunk;
        }

        if err.is_none() {
            if dst_idx + 4 > dst.len() {
                err = Some("ROLZ codec forward transform skip: destination buffer too small");
            } else {
                // Emit last literals (the 4 bytes past srcEnd)
                let tail = src_end;
                dst[dst_idx] = src[tail];
                dst[dst_idx + 1] = src[tail + 1];
                dst[dst_idx + 2] = src[tail + 2];
                dst[dst_idx + 3] = src[tail + 3];
                let src_idx_done = tail + 4;
                dst_idx += 4;

                if src_idx_done != src.len() {
                    err = Some("ROLZ codec forward transform skip: destination buffer too small");
                } else if dst_idx >= src.len() {
                    err = Some("ROLZ codec forward transform skip: no compression");
                }

                if err.is_none() {
                    return Ok((src_idx_done, dst_idx, dt));
                }
            }
        }

        Err((err.unwrap_or("ROLZ codec forward transform skip"), dt))
    }

    /// Inverse transform.
    pub fn inverse(&mut self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        if src.len() < 5 {
            return Err(
                "ROLZ codec inverse transform failed: invalid input data (input array too small)",
            );
        }

        if src.len() > MAX_BLOCK_SIZE {
            return Err("ROLZ codec inverse transform failed: block too big");
        }

        let dst_end = u32::from_be_bytes(src[0..4].try_into().unwrap()) as usize;

        if dst_end <= 4 || dst_end > dst.len() + 4 {
            return Err("ROLZ codec inverse transform failed: invalid input data");
        }

        let dst_end = dst_end - 4;
        let mut start_chunk = 0usize;
        let mut src_idx = 5usize;
        let mut size_chunk = dst.len().min(CHUNK_SIZE);

        if self.lit_buf.len() < size_chunk {
            self.lit_buf = vec![0u8; size_chunk];
        }
        // Pad so readLengthROLZ can safely read up to 4 bytes past the
        // logical end once the first byte has been validated (mirrors Go).
        if self.len_buf.len() < size_chunk / 5 + 4 {
            self.len_buf = vec![0u8; size_chunk / 5 + 4];
        }
        if self.m_idx_buf.len() < size_chunk / 4 {
            self.m_idx_buf = vec![0u8; size_chunk / 4];
        }
        if self.tk_buf.len() < size_chunk / 4 {
            self.tk_buf = vec![0u8; size_chunk / 4];
        }

        self.counters.fill(0);
        let flags = src[4];
        let lit_order = (flags & 1) as u32;
        let mut delta = 2usize;
        self.min_match = MIN_MATCH3;

        // v7-only: Go decodes flags/minMatch from the >= v4 layout for
        // versions 4..7 identically; older layouts are rejected.
        if flags & 0x0E == 2 {
            self.min_match = MIN_MATCH4;
            delta = 8;
        } else if flags & 0x0E == 4 {
            self.min_match = MIN_MATCH7;
            delta = 8;
        } else if flags & 0x0E == 8 {
            delta = 3;
        }

        let log_pos_checks = (flags >> 4) as u32;

        if log_pos_checks < 2 || log_pos_checks > 8 {
            return Err(
                "ROLZ codec inverse transform failed: invalid 'logPosChecks' value in bitstream",
            );
        }

        if self.matches.len() < (1usize << log_pos_checks) {
            self.matches = vec![0u32; HASH_SIZE << log_pos_checks];
        }

        // Go allocates lenBuf fresh (zeroed) per Inverse call and reuses it
        // across chunks; read_length_rolz may peek up to 4 bytes at
        // [len_idx..len_idx+4), i.e. into the zero padding past the logical
        // end. Our struct buffers persist across calls (and forward() dirties
        // len_buf), so reset the whole buffer here: chunk-to-chunk staleness
        // then accumulates exactly like Go's, starting from zeros.
        self.len_buf.fill(0);

        let pos_checks = 1i32 << log_pos_checks;
        let mask_checks = pos_checks - 1;
        let mut err: Option<&'static str> = None;
        // Relative write cursor of the last fully-decoded chunk (hoisted for
        // the End-block recompute below).
        let mut last_dst_rel = 0usize;

        // Main loop
        while start_chunk < dst_end && err.is_none() {
            let mut m_idx = 0usize;
            let mut len_idx = 0usize;
            let mut lit_idx = 0usize;
            let mut tk_idx = 0usize;

            let mut end_chunk = start_chunk + size_chunk;

            if end_chunk > dst_end {
                end_chunk = dst_end;
            }

            size_chunk = end_chunk - start_chunk;
            let only_literals: bool;
            let lit_len_decoded: usize;
            let tk_len: usize;
            let m_len_len: usize;
            let m_idx_len: usize;

            // Decode literal, match length and match index buffers
            {
                let mut br = BitReader::new(&src[src_idx..]);
                let lit_len = br.read_bits(32) as usize;
                tk_len = br.read_bits(32) as usize;
                m_len_len = br.read_bits(32) as usize;
                m_idx_len = br.read_bits(32) as usize;
                let first_lit_len = size_chunk.min(8);

                if lit_len > self.lit_buf.len() {
                    err = Some("ROLZ codec: Invalid length for literals");
                    break;
                }

                if tk_len > self.tk_buf.len() {
                    err = Some("ROLZ codec: Invalid length for tokens");
                    break;
                }

                if m_len_len > self.len_buf.len() - 4 {
                    err = Some("ROLZ codec: Invalid length for match lengths");
                    break;
                }

                if m_idx_len > self.m_idx_buf.len() {
                    err = Some("ROLZ codec: Invalid length for match indexes");
                    break;
                }

                if lit_len < first_lit_len || lit_len > size_chunk {
                    err = Some("ROLZ codec inverse transform failed: invalid data");
                    break;
                }

                if (tk_len == 0 && m_idx_len != 0) || (tk_len > 0 && m_idx_len + 1 != tk_len) {
                    err = Some("ROLZ codec inverse transform failed: invalid data");
                    break;
                }

                lit_len_decoded = lit_len;

                let mut lit_dec = match AnsDecoder::new(lit_order, None) {
                    Ok(d) => d,
                    Err(_) => {
                        err = Some("ROLZ codec inverse transform failed: invalid litOrder");
                        break;
                    }
                };

                if lit_dec.read(&mut br, &mut self.lit_buf[..lit_len]).is_err() {
                    err = Some("ROLZ codec inverse transform failed: invalid literals");
                    break;
                }

                let mut m_dec = match AnsDecoder::new(0, Some(32768)) {
                    Ok(d) => d,
                    Err(_) => {
                        err = Some("ROLZ codec inverse transform failed: invalid mDec params");
                        break;
                    }
                };

                if m_dec.read(&mut br, &mut self.tk_buf[..tk_len]).is_err()
                    || m_dec.read(&mut br, &mut self.len_buf[..m_len_len]).is_err()
                    || m_dec
                        .read(&mut br, &mut self.m_idx_buf[..m_idx_len])
                        .is_err()
                {
                    err = Some("ROLZ codec inverse transform failed: invalid matches");
                    break;
                }

                only_literals = tk_len == 0;
                src_idx += ((br.bits_read() + 7) >> 3) as usize;
            }

            if only_literals {
                // Shortcut when no match
                if lit_len_decoded != size_chunk {
                    err = Some("ROLZ codec inverse transform failed: invalid data");
                    break;
                }

                dst[start_chunk..end_chunk].copy_from_slice(&self.lit_buf[..size_chunk]);
                // Go sets dstIdx = sizeChunk here; the End block recomputes
                // the absolute cursor from this, so track it the same way.
                last_dst_rel = size_chunk;
                start_chunk = end_chunk;
                continue;
            }

            self.matches.fill(0);

            let mut dst_idx = 0usize;
            let mm = 8usize;

            for _ in 0..mm {
                if dst_idx >= size_chunk {
                    break;
                }
                dst[start_chunk + dst_idx] = self.lit_buf[lit_idx];
                dst_idx += 1;
                lit_idx += 1;
            }

            // Next chunk
            while dst_idx < size_chunk && err.is_none() {
                // token LLLLLMMM -> L lit length, M match length
                let token = self.tk_buf[tk_idx];
                tk_idx += 1;
                let mut match_len = (token & 0x07) as usize;

                if match_len == 7 {
                    if len_idx >= m_len_len {
                        err = Some("ROLZ codec inverse transform failed: invalid data");
                        break;
                    }

                    let (ml, delta_idx) = read_length_rolz(&self.len_buf[len_idx..len_idx + 4]);
                    len_idx += delta_idx;
                    match_len = ml + 7;
                }

                let lit_len: usize;

                if token < 0xF8 {
                    lit_len = (token >> 3) as usize;
                } else {
                    if len_idx >= m_len_len {
                        err = Some("ROLZ codec inverse transform failed: invalid data");
                        break;
                    }

                    let (ll, delta_idx) = read_length_rolz(&self.len_buf[len_idx..len_idx + 4]);
                    len_idx += delta_idx;
                    lit_len = ll + 31;
                }

                if lit_len > 0 {
                    if dst_idx + lit_len > self.lit_buf.len() {
                        err = Some("ROLZ codec inverse transform failed: invalid data");
                        break;
                    }

                    let mut src_inc = 0i64;
                    let abs_dst = start_chunk + dst_idx;

                    // Whole 16-byte copies when both sides have the room:
                    // literal runs are short, and an exact-length copy costs a
                    // memcpy call each (same shape as lzx.rs's literals).
                    if lit_idx + lit_len + 16 <= self.lit_buf.len() && abs_dst + lit_len + 16 <= dst.len() {
                        let mut i = 0;

                        while i < lit_len {
                            dst[abs_dst + i..abs_dst + i + 16]
                                .copy_from_slice(&self.lit_buf[lit_idx + i..lit_idx + i + 16]);
                            i += 16;
                        }
                    } else {
                        dst[abs_dst..abs_dst + lit_len]
                            .copy_from_slice(&self.lit_buf[lit_idx..lit_idx + lit_len]);
                    }

                    if self.min_match == MIN_MATCH3 {
                        let mut n = 0usize;

                        while n < lit_len {
                            let key = get_key1_at(dst, abs_dst + n - delta);
                            let c = (self.counters[(key & 0xFFFF) as usize] + 1) & mask_checks;
                            self.matches[((key << log_pos_checks) as usize) + c as usize] =
                                (abs_dst + n) as u32;
                            self.counters[(key & 0xFFFF) as usize] = c;
                            n += 1 + ((src_inc >> 6) as usize);
                            src_inc += 1;
                        }
                    } else {
                        let mut n = 0usize;

                        while n < lit_len {
                            let key = get_key2_at(dst, abs_dst + n - delta);
                            let c = (self.counters[(key & 0xFFFF) as usize] + 1) & mask_checks;
                            self.matches[((key << log_pos_checks) as usize) + c as usize] =
                                (abs_dst + n) as u32;
                            self.counters[(key & 0xFFFF) as usize] = c;
                            n += 1 + ((src_inc >> 6) as usize);
                            src_inc += 1;
                        }
                    }

                    lit_idx += lit_len;
                    dst_idx += lit_len;

                    if dst_idx >= size_chunk {
                        // Last chunk literals not followed by match
                        if dst_idx == size_chunk {
                            break;
                        }

                        err = Some("ROLZ codec inverse transform failed: invalid data");
                        break;
                    }
                }

                // Sanity check
                if dst_idx + match_len + self.min_match as usize > dst_end {
                    err = Some("ROLZ codec inverse transform failed: invalid data");
                    break;
                }

                let match_idx = self.m_idx_buf[m_idx] as i32;
                m_idx += 1;
                let abs_dst = start_chunk + dst_idx;
                let key = if self.min_match == MIN_MATCH3 {
                    get_key1_at(dst, abs_dst.wrapping_sub(delta))
                } else {
                    get_key2_at(dst, abs_dst.wrapping_sub(delta))
                };

                let m_start = (key << log_pos_checks) as usize;
                let ck = (key & 0xFFFF) as usize;
                let r = self.matches
                    [m_start + ((self.counters[ck] - match_idx) & mask_checks) as usize]
                    as usize;
                self.counters[ck] = (self.counters[ck] + 1) & mask_checks;
                self.matches[m_start + self.counters[ck] as usize] = abs_dst as u32;
                // Mirrors Go's emitCopy exactly: disjoint ranges copy
                // directly, but OVERLAPPING ranges must expand byte by byte
                // (each step re-reads previously written bytes, e.g. run
                // expansion). copy_within (memmove) snapshots the source
                // first and yields different bytes on forward overlap.
                let mlen = match_len + self.min_match as usize;

                if abs_dst - r >= 8 && mlen <= 32 && abs_dst + mlen + 8 <= dst.len() {
                    // kanzi-cpp's emitCopy: whole 8-byte copies while the
                    // distance keeps them non-overlapping, overshooting up to
                    // 7 bytes past the match (overwritten by what follows).
                    let (mut d, mut m) = (abs_dst, r);
                    let mut left = mlen as i64;

                    while left > 0 {
                        dst.copy_within(m..m + 8, d);
                        d += 8;
                        m += 8;
                        left -= 8;
                    }
                } else if abs_dst >= r + mlen {
                    dst.copy_within(r..r + mlen, abs_dst);
                } else {
                    // Handle overlapping segments
                    for i in 0..mlen {
                        dst[abs_dst + i] = dst[r + i];
                    }
                }

                dst_idx += mlen;
            }

            if err.is_some() {
                break;
            }

            if tk_idx != tk_len
                || m_idx != m_idx_len
                || lit_idx != lit_len_decoded
                || len_idx != m_len_len
            {
                err = Some("ROLZ codec inverse transform failed: invalid data");
                break;
            }

            last_dst_rel = dst_idx;
            start_chunk = end_chunk;
        }

        if err.is_none() {
            // Emit last literals. Go recomputes the absolute write cursor as
            // dstIdx += (startChunk - sizeChunk); same thing here.
            let mut dst_idx = last_dst_rel + (start_chunk - size_chunk);

            if dst_idx + 4 > dst.len() || src.len() - src_idx != 4 {
                err = Some("ROLZ codec inverse transform failed: invalid input data");
            } else {
                dst[dst_idx] = src[src_idx];
                dst[dst_idx + 1] = src[src_idx + 1];
                dst[dst_idx + 2] = src[src_idx + 2];
                dst[dst_idx + 3] = src[src_idx + 3];
                src_idx += 4;
                dst_idx += 4;
            }

            if err.is_none() && src_idx != src.len() {
                err = Some("ROLZ codec inverse transform failed: invalid input data");
            }

            if err.is_none() {
                return Ok((src_idx, dst_idx));
            }
        }

        Err(err.unwrap_or("ROLZ codec inverse transform failed"))
    }
}

/// Single-byte fetch with zero padding past the buffer end (module note).
/// In-bounds reads are exactly Go's bytes.
#[inline]
fn byte_at(buf: &[u8], pos: usize) -> u8 {
    if pos < buf.len() {
        buf[pos]
    } else {
        0
    }
}
