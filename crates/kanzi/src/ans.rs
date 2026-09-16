// Port of kanzi-go's ANSRangeCodec (entropy/ANSRangeCodec.go) -- the
// Asymmetric Numeral System range codec used internally by ROLZ (level 4+).
// Both orders (0 and 1) and both roles (encoder, decoder) are ported.
// DecodeChunkV1 (bitstream version 1, predating v4) is intentionally not
// ported: this project only produces and supports v7 streams, for which Go
// itself always takes the V2 path.
//
// Two fidelity notes (both verified against Go during porting):
// - All ANS state arithmetic uses wrapping semantics: Go's int/uint64
//   arithmetic wraps silently, Rust panics in debug, so every hot-path op is
//   an explicit wrapping_*.
// - The order-1 statistics pass histograms four disjoint quarter slices with
//   separate predecessor chains (each restarting at context 0, so the three
//   quarter-boundary digrams differ from a true sequential pass) and drops
//   the trailing len%4 bytes. Replicated exactly (byte-exact headers depend
//   on it).

use crate::bitio::{BitReader, BitWriter};

const ANS_TOP: i64 = 1 << 15;
const DEFAULT_CHUNK_SIZE: usize = 16384;
const MIN_CHUNK_SIZE: usize = 1024;
const MAX_CHUNK_SIZE: usize = 1 << 27;
const DEFAULT_LOG_RANGE: u32 = 12;

// --- wire helpers (same formats as huffman_*'s private copies) ---

fn write_var_int(bw: &mut BitWriter, mut value: u32) {
    while value >= 128 {
        bw.write_bits((0x80 | (value & 0x7F)) as u64, 8);
        value >>= 7;
    }
    bw.write_bits(value as u64, 8);
}

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

fn encode_alphabet(bw: &mut BitWriter, alphabet: &[usize]) {
    let count = alphabet.len();

    if count == 0 {
        bw.write_bit(0);
        bw.write_bit(1);
    } else if count == 256 {
        bw.write_bit(0);
        bw.write_bit(0);
    } else {
        bw.write_bit(1);
        let mut masks = [0u8; 32];

        for &s in alphabet {
            masks[s >> 3] |= 1 << (s & 7);
        }

        let last_mask = (alphabet[count - 1] >> 3) as u64;
        bw.write_bits(last_mask, 5);
        bw.write_array(&masks, 8 * (last_mask as usize + 1));
    }
}

fn decode_alphabet(br: &mut BitReader, alphabet: &mut [usize]) -> usize {
    if br.read_bit() == 0 {
        if br.read_bit() == 1 {
            return 0;
        }
        for (i, a) in alphabet.iter_mut().enumerate().take(256) {
            *a = i;
        }
        return 256;
    }

    let last_mask = br.read_bits(5) as usize;
    let mut masks = [0u8; 32];
    br.read_array(&mut masks, 8 * (last_mask + 1));
    let mut count = 0;

    for i in 0..=last_mask {
        for j in 0..8 {
            if (masks[i] >> j) & 1 == 1 {
                if count >= alphabet.len() {
                    return count;
                }
                alphabet[count] = i * 8 + j;
                count += 1;
            }
        }
    }

    count
}

// --- NormalizeFrequencies (port of entropy.NormalizeFrequencies) ---

/// Scales the frequencies so that their sum equals `scale`. Updates `freqs`
/// (scaled) and `alphabet` (present symbols), returns the alphabet size.
fn normalize_frequencies(
    freqs: &mut [i64],
    alphabet: &mut [usize],
    total_freq: i64,
    scale: i64,
) -> usize {
    if scale < 256 || scale > 65536 {
        return 0;
    }

    if alphabet.is_empty() || total_freq == 0 {
        return 0;
    }

    let mut alphabet_size = 0;

    // Shortcut
    if total_freq == scale {
        for i in 0..256 {
            if freqs[i] != 0 {
                alphabet[alphabet_size] = i;
                alphabet_size += 1;
            }
        }

        return alphabet_size;
    }

    let mut sum_scaled_freq = 0i64;
    let mut sum_freq = 0i64;
    let mut idx_max = 0usize;

    // Scale frequencies by squeezing/stretching distribution over complete range
    for i in 0..alphabet.len() {
        alphabet[i] = 0;
        let f = freqs[i];

        if f == 0 {
            continue;
        }

        let sf = f * scale;
        let scaled_freq = if sf <= total_freq {
            // Quantum of frequency
            1
        } else {
            (sf + (total_freq >> 1)) / total_freq
        };

        alphabet[alphabet_size] = i;
        alphabet_size += 1;
        sum_scaled_freq += scaled_freq;
        freqs[i] = scaled_freq;
        sum_freq += f;

        if scaled_freq > freqs[idx_max] {
            idx_max = i;
        }

        if sum_freq >= total_freq {
            break;
        }
    }

    if alphabet_size == 0 {
        return 0;
    }

    if alphabet_size == 1 {
        freqs[alphabet[0]] = scale;
        return 1;
    }

    if sum_scaled_freq == scale {
        return alphabet_size;
    }

    let mut delta = sum_scaled_freq - scale;
    let err_thr = freqs[idx_max] >> 4;

    if (delta.unsigned_abs() as i64) <= err_thr {
        // Fast path (small error): just adjust the max frequency
        freqs[idx_max] -= delta;
        return alphabet_size;
    }

    let inc: i64;

    if delta < 0 {
        delta += err_thr;
        freqs[idx_max] += err_thr;
        inc = 1;
        delta = -delta;
    } else {
        delta -= err_thr;
        freqs[idx_max] -= err_thr;
        inc = -1;
    }

    // Slow path: spread error across frequencies
    let mut round = 1;

    // Create queue of present symbols
    while round < 6 && delta > 0 {
        let mut adjustments = 0;
        round += 1;

        for k in 0..alphabet_size {
            let idx = alphabet[k];

            // Skip small frequencies to avoid big distortion
            // Do not zero out frequencies
            if freqs[idx] <= 2 {
                continue;
            }

            // Adjust frequency
            freqs[idx] += inc;
            adjustments += 1;
            delta -= 1;

            if delta == 0 {
                break;
            }
        }

        if adjustments == 0 {
            break;
        }
    }

    freqs[idx_max] = (freqs[idx_max] - delta).max(1);
    alphabet_size
}

// --- encoder ---

struct EncSymbol {
    x_max: i64,     // (Exclusive) upper bound of pre-normalization interval
    bias: i64,      // Bias
    cmpl_freq: i64, // Complement of frequency: (1 << scale_bits) - freq
    inv_shift: u32, // Reciprocal shift
    inv_freq: u64,  // Fixed-point reciprocal frequency
}

impl EncSymbol {
    fn reset(&mut self, cum_freq: i64, mut freq: i64, log_range: u32) {
        // Make sure xMax is a positive int32. Compatibility with Java implementation
        freq = freq.min((1 << log_range) - 1);
        self.x_max = ((ANS_TOP >> log_range) << 16) * freq;
        self.cmpl_freq = (1 << log_range) - freq;

        if freq < 2 {
            self.inv_freq = 0xFFFF_FFFF;
            self.inv_shift = 32;
            self.bias = cum_freq + (1 << log_range) - 1;
        } else {
            let mut shift = 0u32;

            while freq > 1 << shift {
                shift += 1;
            }

            // Alverson, "Integer Division using reciprocals"
            self.inv_freq =
                (((1u64 << (shift + 31)) + (freq as u64 - 1)) / freq as u64) & 0xFFFF_FFFF;
            self.inv_shift = 32 + shift - 1;
            self.bias = cum_freq;
        }
    }
}

/// Computes cumulated frequencies and encodes the header обся stock.
/// Free function so the table borrows stay disjoint from the bitstream.
fn update_frequencies(
    freqs: &mut [i64],
    symbols: &mut [EncSymbol],
    order: u32,
    lr: u32,
    bw: &mut BitWriter,
) -> usize {
    let mut res = 0;
    let endk = (255 * order + 1) as usize;
    bw.write_bits((lr - 8) as u64, 3); // logRange
    let mut alphabet = [0usize; 256];

    for k in 0..endk {
        let f = &mut freqs[257 * k..257 * (k + 1)];
        let symb = &mut symbols[k << 8..(k + 1) << 8];
        let total = f[256];
        let alphabet_size = normalize_frequencies(&mut f[0..256], &mut alphabet, total, 1 << lr);

        if alphabet_size > 0 {
            let mut sum = 0i64;
            let mut count = 0;

            for i in 0..256 {
                if f[i] == 0 {
                    continue;
                }

                symb[i].reset(sum, f[i], lr);
                sum += f[i];
                count += 1;

                if count >= alphabet_size {
                    break;
                }
            }
        }

        encode_header(&alphabet[0..alphabet_size], f, lr, bw);
        res += alphabet_size;
    }

    res
}

/// Encodes alphabet and frequencies into the bitstream.
fn encode_header(alphabet: &[usize], frequencies: &[i64], lr: u32, bw: &mut BitWriter) {
    encode_alphabet(bw, alphabet);
    let alphabet_size = alphabet.len();

    if alphabet_size <= 1 {
        return;
    }

    let mut chk_size = 8u32;

    if alphabet_size < 64 {
        chk_size = 6;
    }

    let mut llr = 3u32;

    while (1 << llr) <= lr {
        llr += 1;
    }

    // Encode all frequencies (but the first one) by chunks
    let mut i = 1usize;

    while i < alphabet_size {
        let mut max = frequencies[alphabet[i]] - 1;
        let mut log_max = 0u32;
        let endj = (i + chk_size as usize).min(alphabet_size);

        // Search for max frequency log size in next chunk
        for j in i + 1..endj {
            if frequencies[alphabet[j]] - 1 > max {
                max = frequencies[alphabet[j]] - 1;
            }
        }

        while (1 << log_max) <= max {
            log_max += 1;
        }

        bw.write_bits(log_max as u64, llr);

        if log_max == 0 {
            // all frequencies equal one in this chunk
            i += chk_size as usize;
            continue;
        }

        // Write frequencies
        for j in i..endj {
            bw.write_bits((frequencies[alphabet[j]] - 1) as u64, log_max);
        }

        i += chk_size as usize;
    }
}

fn encode_symbol(buffer: &mut [u8], mut n: usize, mut st: i64, sym: &EncSymbol) -> (usize, i64) {
    let x = if st >= sym.x_max { 1 } else { 0 };

    buffer[n] = st as u8;
    n -= x;
    buffer[n] = (st >> 8) as u8;
    n -= x;
    st >>= if x == 1 { 16 } else { 0 };

    // Go: st + bias + ((uint64(st)*invFreq)>>invShift)*cmplFreq (wrapping)
    let q = ((st as u64).wrapping_mul(sym.inv_freq) >> sym.inv_shift) as i64;
    let nst = st
        .wrapping_add(sym.bias)
        .wrapping_add(q.wrapping_mul(sym.cmpl_freq));
    (n, nst)
}

fn encode_chunk(
    buffer: &mut [u8],
    symbols: &[EncSymbol],
    order: u32,
    block: &[u8],
    bw: &mut BitWriter,
) {
    let mut st0 = ANS_TOP;
    let mut st1 = ANS_TOP;
    let mut st2 = ANS_TOP;
    let mut st3 = ANS_TOP;
    let mut n = buffer.len() - 1;
    let end4 = block.len() & !3;

    for i in (end4..block.len()).rev() {
        buffer[n] = block[i];
        n -= 1;
    }

    if order == 0 {
        let symb = &symbols[0..256];
        let mut i = end4 as i64 - 1;

        while i > 0 {
            let r = encode_symbol(buffer, n, st0, &symb[block[i as usize] as usize]);
            n = r.0;
            st0 = r.1;
            let r = encode_symbol(buffer, n, st1, &symb[block[i as usize - 1] as usize]);
            n = r.0;
            st1 = r.1;
            let r = encode_symbol(buffer, n, st2, &symb[block[i as usize - 2] as usize]);
            n = r.0;
            st2 = r.1;
            let r = encode_symbol(buffer, n, st3, &symb[block[i as usize - 3] as usize]);
            n = r.0;
            st3 = r.1;
            i -= 4;
        }
    } else if block.len() > 1 {
        // order 1
        let quarter = (end4 >> 2) as i64;
        let mut i0 = quarter - 2;
        let mut i1 = 2 * quarter - 2;
        let mut i2 = 3 * quarter - 2;
        let mut i3 = end4 as i64 - 2;
        let mut prv0 = block[(i0 + 1) as usize] as usize;
        let mut prv1 = block[(i1 + 1) as usize] as usize;
        let mut prv2 = block[(i2 + 1) as usize] as usize;
        let mut prv3 = block[(i3 + 1) as usize] as usize;

        while i0 >= 0 {
            let cur0 = block[i0 as usize] as usize;
            let r = encode_symbol(buffer, n, st0, &symbols[(cur0 << 8) | prv0]);
            n = r.0;
            st0 = r.1;
            let cur1 = block[i1 as usize] as usize;
            let r = encode_symbol(buffer, n, st1, &symbols[(cur1 << 8) | prv1]);
            n = r.0;
            st1 = r.1;
            let cur2 = block[i2 as usize] as usize;
            let r = encode_symbol(buffer, n, st2, &symbols[(cur2 << 8) | prv2]);
            n = r.0;
            st2 = r.1;
            let cur3 = block[i3 as usize] as usize;
            let r = encode_symbol(buffer, n, st3, &symbols[(cur3 << 8) | prv3]);
            n = r.0;
            st3 = r.1;
            prv0 = cur0;
            prv1 = cur1;
            prv2 = cur2;
            prv3 = cur3;
            i0 -= 1;
            i1 -= 1;
            i2 -= 1;
            i3 -= 1;
        }

        // Last symbols
        let r = encode_symbol(buffer, n, st0, &symbols[prv0]);
        n = r.0;
        st0 = r.1;
        let r = encode_symbol(buffer, n, st1, &symbols[prv1]);
        n = r.0;
        st1 = r.1;
        let r = encode_symbol(buffer, n, st2, &symbols[prv2]);
        n = r.0;
        st2 = r.1;
        let r = encode_symbol(buffer, n, st3, &symbols[prv3]);
        n = r.0;
        st3 = r.1;
    }

    n += 1;

    // Write chunk size
    write_var_int(bw, (buffer.len() - n) as u32);

    // Write final ANS state
    bw.write_bits(st0 as u64, 32);
    bw.write_bits(st1 as u64, 32);
    bw.write_bits(st2 as u64, 32);
    bw.write_bits(st3 as u64, 32);

    if buffer.len() != n {
        // Write encoded data to bitstream
        bw.write_array(&buffer[n..], 8 * (buffer.len() - n));
    }
}

pub struct AnsEncoder {
    freqs: Vec<i64>,
    symbols: Vec<EncSymbol>,
    buffer: Vec<u8>,
    chunk_size: usize,
    order: u32,
    log_range: u32,
}

impl AnsEncoder {
    /// Mirrors NewANSRangeEncoder(bs, order?, chunkSize?, logRange?):
    /// order defaults to 0, chunk size to 16384 (x256 when order==1),
    /// log range to 12 (minus order, floored at 8).
    pub fn new(
        order: u32,
        chunk_size: Option<usize>,
        log_range: Option<u32>,
    ) -> Result<Self, String> {
        if order != 0 && order != 1 {
            return Err("ANS codec: The order must be 0 or 1".to_string());
        }

        let mut chk = chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);

        if chunk_size.is_some() && (chk < MIN_CHUNK_SIZE || chk > MAX_CHUNK_SIZE) {
            return Err("ANS codec: invalid chunk size".to_string());
        }

        let lr = log_range.unwrap_or(DEFAULT_LOG_RANGE);

        if log_range.is_some() && (lr < 8 || lr > 15) {
            return Err("ANS codec: Invalid range (must be in [8..15])".to_string());
        }

        if order == 1 {
            chk = ((chk as u64) << 8).min(MAX_CHUNK_SIZE as u64) as usize;
        }

        let dim = (255 * order + 1) as usize;
        Ok(AnsEncoder {
            freqs: vec![0i64; dim * 257], // freqs[x][256] = total(freqs[x][0..255])
            symbols: (0..dim * 256)
                .map(|_| EncSymbol {
                    x_max: 0,
                    bias: 0,
                    cmpl_freq: 0,
                    inv_shift: 0,
                    inv_freq: 0,
                })
                .collect(),
            buffer: Vec::new(),
            chunk_size: chk,
            order,
            log_range: lr.saturating_sub(order).max(8),
        })
    }

    /// Dynamically computes the frequencies for every chunk and encodes the
    /// block. Mirrors ANSRangeEncoder.Write (returns bytes written).
    pub fn write(&mut self, block: &[u8], bw: &mut BitWriter) -> usize {
        if block.len() <= 32 {
            bw.write_array(block, 8 * block.len());
            return block.len();
        }

        let size = self.chunk_size.min(block.len());
        let extra = (size >> 3).max(size.min(1 << 16));
        let buf_size = size + extra;

        // Add some padding
        if self.buffer.len() < buf_size {
            self.buffer = vec![0u8; buf_size];
        }

        let end = block.len();
        let mut start_chunk = 0;

        while start_chunk < end {
            let end_chunk = (start_chunk + self.chunk_size).min(end);
            let alphabet_size = self.rebuild_statistics(&block[start_chunk..end_chunk], bw);

            if self.order == 1 || alphabet_size > 1 {
                encode_chunk(
                    &mut self.buffer,
                    &self.symbols,
                    self.order,
                    &block[start_chunk..end_chunk],
                    bw,
                );
            }

            start_chunk = end_chunk;
        }

        end
    }

    /// Computes chunk frequencies, cumulated frequencies and encodes the
    /// chunk header. Returns the total alphabet size (summed over contexts).
    fn rebuild_statistics(&mut self, block: &[u8], bw: &mut BitWriter) -> usize {
        self.freqs.fill(0);

        if self.order == 0 {
            let mut total = 0i64;
            for &b in block {
                self.freqs[b as usize] += 1;
                total += 1;
            }
            self.freqs[256] = total;
        } else {
            // Order-1 with totals. Go histograms four disjoint quarter
            // slices with SEPARATE ComputeHistogram calls, each restarting
            // its predecessor chain at 0 (so the three quarter-boundary
            // digrams use context 0, unlike a true sequential pass), and
            // drops the trailing len%4 bytes. Replicated exactly: any
            // "smarter" chaining changes header bytes.
            let quarter = block.len() >> 2;

            if quarter == 0 {
                let mut prv = 0usize;
                for &b in block {
                    self.freqs[prv * 257 + b as usize] += 1;
                    self.freqs[prv * 257 + 256] += 1;
                    prv = b as usize;
                }
            } else {
                for k in 0..4 {
                    let mut prv = 0usize;
                    for &b in &block[k * quarter..(k + 1) * quarter] {
                        self.freqs[prv * 257 + b as usize] += 1;
                        self.freqs[prv * 257 + 256] += 1;
                        prv = b as usize;
                    }
                }
            }
        }

        let lr = self.log_range;
        let order = self.order;
        update_frequencies(&mut self.freqs, &mut self.symbols, order, lr, bw)
    }
}

// --- decoder ---

/// 16-bit fields, like kanzi-cpp's ANSDecSymbol: both quantities are bounded by
/// the frequency scale (2^log_range <= 32768), and the small entry keeps the
/// order-1 table (65536 entries) at 256 KiB instead of 1 MiB.
#[derive(Clone, Copy)]
struct DecSymbol {
    cum_freq: u16,
    freq: u16,
}

impl DecSymbol {
    fn reset(&mut self, cum_freq: i64, mut freq: i64, log_range: u32) {
        // Mirror encoder
        freq = freq.min((1 << log_range) - 1);
        self.cum_freq = cum_freq as u16;
        self.freq = freq as u16;
    }
}

pub struct AnsDecoder {
    freqs: Vec<i64>,
    symbols: Vec<DecSymbol>,
    f2s: Vec<u8>,
    buffer: Vec<u8>,
    chunk_size: usize,
    log_range: u32,
    order: u32,
}

/// The eight buffer bytes a group of four interleaved states may consume.
#[inline(always)]
fn window8(buffer: &[u8], n: usize) -> Option<&[u8; 8]> {
    buffer.get(n..n + 8)?.try_into().ok()
}

/// One ANS decoding step, D(x) = (s, q_s (x/M) + mod(x,M) - b_s), followed by a
/// branchless renormalization: kanzi-cpp always reads the next two bytes and
/// folds them in under a 0/-1 mask rather than branching on the state, which
/// the state-dependent `if` here used to mispredict on every other symbol.
/// `k` is the offset into the group's byte window; it stays <= 6 at every read
/// (four symbols, two bytes each at most), so masking it keeps both indices in
/// range without a bounds check.
#[inline(always)]
fn decode_symbol(
    st: u32,
    sym: DecSymbol,
    log_range: u32,
    mask: u32,
    w: &[u8; 8],
    k: usize,
) -> (u32, usize) {
    let st = (sym.freq as u32)
        .wrapping_mul(st >> log_range)
        .wrapping_add(st & mask)
        .wrapping_sub(sym.cum_freq as u32);
    debug_assert!(k <= 6, "the fourth symbol of a group reads at most w[6..8]");
    let x = if st < ANS_TOP as u32 { u32::MAX } else { 0 };
    let next = ((w[k & 7] as u32) << 8) | w[(k + 1) & 7] as u32;

    ((st << (x & 16)) | (x & next), k + (x & 2) as usize)
}

impl AnsDecoder {
    /// Mirrors NewANSRangeDecoderWithCtx(bs, ctx, order?, chunkSize?) with a
    /// v4+ bitstream (always the V2 chunk path, like Go for bsVersion >= 2).
    pub fn new(order: u32, chunk_size: Option<usize>) -> Result<Self, String> {
        if order != 0 && order != 1 {
            return Err("ANS codec: The order must be 0 or 1".to_string());
        }

        let mut chk = chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);

        if chunk_size.is_some() && (chk < MIN_CHUNK_SIZE || chk > MAX_CHUNK_SIZE) {
            return Err("ANS codec: invalid chunk size".to_string());
        }

        if order == 1 {
            chk = ((chk as u64) << 8).min(MAX_CHUNK_SIZE as u64) as usize;
        }

        let dim = (255 * order + 1) as usize;
        Ok(AnsDecoder {
            freqs: vec![0i64; dim * 256],
            symbols: vec![DecSymbol { cum_freq: 0, freq: 0 }; dim * 256],
            f2s: Vec::new(),
            buffer: Vec::new(),
            chunk_size: chk,
            log_range: DEFAULT_LOG_RANGE,
            order,
        })
    }

    /// Decodes alphabet and frequencies from the bitstream.
    fn decode_header(&mut self, br: &mut BitReader) -> Result<usize, String> {
        self.log_range = 8 + br.read_bits(3) as u32;

        if self.log_range < 8 || self.log_range > 15 {
            return Err(format!(
                "Invalid bitstream: range = {} (must be in [8..15])",
                self.log_range
            ));
        }

        let mut res = 0;
        let dim = (255 * self.order + 1) as usize;
        let scale = 1i64 << self.log_range;

        if self.f2s.len() < dim * scale as usize {
            self.f2s = vec![0u8; dim * scale as usize];
        }

        let mut llr = 3u32;

        while (1 << llr) <= self.log_range {
            llr += 1;
        }

        let mut alphabet = [0usize; 256];

        for k in 0..dim {
            let alphabet_size = decode_alphabet(br, &mut alphabet);

            if alphabet_size == 0 {
                if self.order == 1 && k == 0 {
                    return Err("Invalid bitstream: missing ANS1 context 0".to_string());
                }

                continue;
            }

            let f = &mut self.freqs[k << 8..(k + 1) << 8];

            if alphabet_size != 256 {
                f.fill(0);
            }

            let mut chk_size = 8usize;

            if alphabet_size < 64 {
                chk_size = 6;
            }

            let mut sum = 0i64;

            // Decode all frequencies (but the first one) by chunks
            let mut i = 1usize;

            while i < alphabet_size {
                // Read frequencies size for current chunk
                let log_max = br.read_bits(llr) as u32;

                if (1i64 << log_max) > scale {
                    return Err(format!(
                        "Invalid bitstream: incorrect frequency size {} in ANS range decoder",
                        log_max
                    ));
                }

                let endj = (i + chk_size).min(alphabet_size);

                // Read frequencies
                for j in i..endj {
                    let mut freq = 1i64;

                    if log_max > 0 {
                        freq = 1 + br.read_bits(log_max) as i64;

                        if freq <= 0 || freq >= scale {
                            return Err(format!(
                                "Invalid bitstream: incorrect frequency {} for symbol '{}' in ANS range decoder",
                                freq, alphabet[j]
                            ));
                        }
                    }

                    f[alphabet[j]] = freq;
                    sum += freq;
                }

                i += chk_size;
            }

            // Infer first frequency
            if scale <= sum {
                return Err(format!(
                    "Invalid bitstream: incorrect frequency {} for symbol '{}' in ANS range decoder",
                    f[alphabet[0]], alphabet[0]
                ));
            }

            f[alphabet[0]] = scale - sum;
            sum = 0;

            // Split borrows: symbols/f2s for context k, f already reborrowed.
            // (Reborrow f's slice end: f covers k<<8..(k+1)<<8; release it.)
            let lr = self.log_range;
            let (symbols, f2s) = (&mut self.symbols, &mut self.f2s);
            let symb = &mut symbols[k << 8..(k + 1) << 8];
            let freq2sym = &mut f2s[k * scale as usize..(k + 1) * scale as usize];

            // Create reverse mapping
            for i in 0..256 {
                if f[i] == 0 {
                    continue;
                }

                let mut j = f[i] - 1;

                while j >= 0 {
                    freq2sym[(sum + j) as usize] = i as u8;
                    j -= 1;
                }

                symb[i].reset(sum, f[i], lr);
                sum += f[i];
            }

            res += alphabet_size;
        }

        Ok(res)
    }

    /// Decodes data from the bitstream into the block. Mirrors
    /// ANSRangeDecoder.Read (returns bytes decoded).
    pub fn read(&mut self, br: &mut BitReader, block: &mut [u8]) -> Result<usize, String> {
        if block.len() <= 32 {
            br.read_array(block, 8 * block.len());
            return Ok(block.len());
        }

        let end = block.len();
        let mut start_chunk = 0;

        while start_chunk < end {
            let end_chunk = (start_chunk + self.chunk_size).min(end);
            let alphabet_size = self.decode_header(br)?;

            if alphabet_size == 0 {
                return Err("Invalid bitstream: empty ANS header".to_string());
            }

            if self.order == 0 && alphabet_size == 1 {
                // Shortcut for chunks with only one symbol
                // (Go reads alphabet[0] from its own array; ours was decoded
                // into the local array inside decode_header -- re-derive it:
                // a single-symbol alphabet holds that symbol. decode_header
                // does not return it, so peek it back from freqs.)
                let mut sym = 0u8;

                for (i, &fq) in self.freqs[0..256].iter().enumerate() {
                    if fq != 0 {
                        sym = i as u8;
                        break;
                    }
                }

                for b in block[start_chunk..end_chunk].iter_mut() {
                    *b = sym;
                }
            } else if !self.decode_chunk_v2(br, &mut block[start_chunk..end_chunk]) {
                return Err("Invalid bitstream: incorrect chunk size".to_string());
            }

            start_chunk = end_chunk;
        }

        Ok(start_chunk)
    }

    fn decode_chunk_v2(&mut self, br: &mut BitReader, block: &mut [u8]) -> bool {
        // Read chunk size
        let sz = read_var_int(br) as usize;

        if sz >= MAX_CHUNK_SIZE {
            return false;
        }

        // Read initial ANS state
        let mut st0 = br.read_bits(32) as u32;
        let mut st1 = br.read_bits(32) as u32;
        let mut st2 = br.read_bits(32) as u32;
        let mut st3 = br.read_bits(32) as u32;

        if block.is_empty() {
            return true;
        }

        let size = self.chunk_size.min(block.len());
        let extra = (size >> 3).max(size.min(1 << 16));
        let min_buf_size = size + extra + 2; // protect against corrupted bitstream

        // Add some padding
        if self.buffer.len() < min_buf_size {
            self.buffer = vec![0u8; min_buf_size];
        }

        // Read compressed data
        br.read_array(&mut self.buffer, 8 * sz);

        // Ensure deterministic renormalization reads past payload end without
        // clearing the entire reusable buffer (mirrors Go).
        if sz < self.buffer.len() {
            let guard_end = (sz + 64).min(self.buffer.len());
            self.buffer[sz..guard_end].fill(0);
        }

        let buffer = &self.buffer[..];
        let symbols = &self.symbols[..];
        let f2s = &self.f2s[..];
        let mut n = 0usize;
        let lr = self.log_range;
        let mask = (1u32 << lr) - 1;
        let end4 = block.len() & !3;

        if self.order == 0 {
            let mut i = 0;

            while i < end4 {
                // A group of four symbols reads at most buffer[n + 7] and
                // advances n by at most 8, so one bounds check covers it.
                // Reading unconditionally looks up to six bytes further ahead
                // than the old branching form did, so this is what keeps the
                // read inside the buffer; a valid chunk never runs out, since
                // the cursor advances with the consumed bits (at most eight per
                // symbol) while the buffer holds the payload plus padding.
                let Some(w) = window8(buffer, n) else { return false };
                let mut k = 0usize;
                let cur3 = f2s[(st3 & mask) as usize];
                block[i] = cur3;
                (st3, k) = decode_symbol(st3, symbols[cur3 as usize], lr, mask, w, k);
                let cur2 = f2s[(st2 & mask) as usize];
                block[i + 1] = cur2;
                (st2, k) = decode_symbol(st2, symbols[cur2 as usize], lr, mask, w, k);
                let cur1 = f2s[(st1 & mask) as usize];
                block[i + 2] = cur1;
                (st1, k) = decode_symbol(st1, symbols[cur1 as usize], lr, mask, w, k);
                let cur0 = f2s[(st0 & mask) as usize];
                block[i + 3] = cur0;
                (st0, k) = decode_symbol(st0, symbols[cur0 as usize], lr, mask, w, k);
                n += k;
                i += 4;
            }
        } else {
            // order 1
            let quarter = end4 >> 2;
            let (mut i0, mut i1, mut i2, mut i3) = (0usize, quarter, 2 * quarter, 3 * quarter);
            let (mut prv0, mut prv1, mut prv2, mut prv3) = (0usize, 0usize, 0usize, 0usize);

            while i0 < quarter {
                let Some(w) = window8(buffer, n) else { return false };
                let mut k = 0usize;
                let cur3 = f2s[(prv3 << lr) + (st3 & mask) as usize];
                block[i3] = cur3;
                (st3, k) = decode_symbol(st3, symbols[(prv3 << 8) | cur3 as usize], lr, mask, w, k);
                let cur2 = f2s[(prv2 << lr) + (st2 & mask) as usize];
                block[i2] = cur2;
                (st2, k) = decode_symbol(st2, symbols[(prv2 << 8) | cur2 as usize], lr, mask, w, k);
                let cur1 = f2s[(prv1 << lr) + (st1 & mask) as usize];
                block[i1] = cur1;
                (st1, k) = decode_symbol(st1, symbols[(prv1 << 8) | cur1 as usize], lr, mask, w, k);
                let cur0 = f2s[(prv0 << lr) + (st0 & mask) as usize];
                block[i0] = cur0;
                (st0, k) = decode_symbol(st0, symbols[(prv0 << 8) | cur0 as usize], lr, mask, w, k);
                n += k;
                prv3 = cur3 as usize;
                prv2 = cur2 as usize;
                prv1 = cur1 as usize;
                prv0 = cur0 as usize;
                i0 += 1;
                i1 += 1;
                i2 += 1;
                i3 += 1;
            }
        }

        for i in end4..block.len() {
            let Some(&b) = buffer.get(n) else { return false };
            block[i] = b;
            n += 1;
        }

        n == sz
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitio::{BitReader, BitWriter};

    /// Bytes with a skewed, context-dependent distribution, so both the order-0
    /// and the order-1 coder have something to model.
    fn skewed(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut prev = 0u8;

        while v.len() < n {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let r = (seed >> 33) as u32;
            let b = match r % 8 {
                0..=3 => prev.wrapping_add(1),
                4 | 5 => b'a' + (r % 26) as u8,
                6 => (r >> 8) as u8,
                _ => 0,
            };
            v.push(b);
            prev = b;
        }

        v
    }

    fn encoded(data: &[u8], order: u32, chunk: Option<usize>) -> Vec<u8> {
        let mut bw = BitWriter::new();
        let mut enc = AnsEncoder::new(order, chunk, None).unwrap();
        assert_eq!(enc.write(data, &mut bw), data.len());
        bw.finish()
    }

    fn roundtrip(data: &[u8], order: u32, chunk: Option<usize>) {
        let payload = encoded(data, order, chunk);
        let mut br = BitReader::new(&payload);
        let mut out = vec![0u8; data.len()];
        let mut dec = AnsDecoder::new(order, chunk).unwrap();

        assert_eq!(dec.read(&mut br, &mut out).unwrap(), data.len());
        assert_eq!(out, data, "order {order}, {} bytes", data.len());
    }

    #[test]
    fn roundtrip_orders_sizes_and_chunk_sizes() {
        for &order in &[0u32, 1] {
            // Sizes around the raw-copy cutoff (32) and around the group of
            // four the decoder reads at a time, so the odd tail is covered.
            for &n in &[0usize, 1, 17, 32, 33, 35, 100, 4095, 70_001] {
                roundtrip(&skewed(n), order, None);
            }

            // Several chunks, and a chunk boundary that is not a multiple of 4.
            roundtrip(&skewed(40_000), order, Some(1024));
            roundtrip(&skewed(40_000), order, Some(3999));

            // Single-symbol chunks take a separate path in the decoder.
            roundtrip(&vec![0xA7u8; 50_000], order, None);
        }
    }

    fn decodes_without_panicking(bad: &[u8], order: u32, len: usize) -> Option<Vec<u8>> {
        let mut br = BitReader::new(bad);
        let mut out = vec![0u8; len];
        let mut dec = AnsDecoder::new(order, None).unwrap();

        dec.read(&mut br, &mut out).ok().map(|_| out)
    }

    #[test]
    fn corrupt_payload_is_rejected_without_panicking() {
        // A length that is not a multiple of four, so the odd tail runs too.
        let data = skewed(20_003);

        for &order in &[0u32, 1] {
            let payload = encoded(&data, order, None);
            let mut wrong = 0;

            // Walk the payload rather than a few spots: the decoder must stay
            // inside its buffer whatever the bits say.
            for pos in (0..payload.len()).step_by(37) {
                let mut bad = payload.clone();
                bad[pos] ^= 0x5A;

                if decodes_without_panicking(&bad, order, data.len()).as_deref() != Some(&data[..]) {
                    wrong += 1;
                }
            }

            assert!(wrong > 0, "order {order}: corruption went unnoticed everywhere");

            // Zeroing the tail of the payload drives the decoder into
            // renormalizing on every symbol, which is what walks the read
            // cursor off the end of its buffer fastest.
            for pos in (payload.len() / 2..payload.len()).step_by(101) {
                let mut bad = payload.clone();
                bad[pos..].fill(0);
                decodes_without_panicking(&bad, order, data.len());
            }
        }
    }
}
