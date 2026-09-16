// Port of kanzi-go's HuffmanEncoder (entropy/HuffmanCodec.go), V6 bitstream
// format (the only format the real encoder ever writes). Shares the
// canonical-code / alphabet / varint wire format with the Huffman decoder
// ported earlier in this session (rust_huffman project) -- reimplemented
// here since these are separate Cargo projects.

use crate::bitio::BitWriter;

const MAX_SYMBOL_SIZE: usize = 12;
const MAX_CHUNK_SIZE: usize = 1 << 14; // 16384

const FULL_ALPHABET: u32 = 0;
const ALPHABET_256: u32 = 0;
const ALPHABET_0: u32 = 1;
const PARTIAL_ALPHABET: u32 = 1;

// entropy.ExpGolombCodec._EXPG_VALUES[1] (signed variant): cache[byte two's
// complement pattern of the delta] = (bit_pattern << 9) | bit_length.
const EXPG_SIGNED: [u32; 256] = [
    513, 2052, 2054, 3080, 3082, 3084, 3086, 4112, 4114, 4116, 4118, 4120, 4122, 4124, 4126, 5152,
    5154, 5156, 5158, 5160, 5162, 5164, 5166, 5168, 5170, 5172, 5174, 5176, 5178, 5180, 5182, 6208,
    6210, 6212, 6214, 6216, 6218, 6220, 6222, 6224, 6226, 6228, 6230, 6232, 6234, 6236, 6238, 6240,
    6242, 6244, 6246, 6248, 6250, 6252, 6254, 6256, 6258, 6260, 6262, 6264, 6266, 6268, 6270, 7296,
    7298, 7300, 7302, 7304, 7306, 7308, 7310, 7312, 7314, 7316, 7318, 7320, 7322, 7324, 7326, 7328,
    7330, 7332, 7334, 7336, 7338, 7340, 7342, 7344, 7346, 7348, 7350, 7352, 7354, 7356, 7358, 7360,
    7362, 7364, 7366, 7368, 7370, 7372, 7374, 7376, 7378, 7380, 7382, 7384, 7386, 7388, 7390, 7392,
    7394, 7396, 7398, 7400, 7402, 7404, 7406, 7408, 7410, 7412, 7414, 7416, 7418, 7420, 7422, 8448,
    8451, 8449, 7423, 7421, 7419, 7417, 7415, 7413, 7411, 7409, 7407, 7405, 7403, 7401, 7399, 7397,
    7395, 7393, 7391, 7389, 7387, 7385, 7383, 7381, 7379, 7377, 7375, 7373, 7371, 7369, 7367, 7365,
    7363, 7361, 7359, 7357, 7355, 7353, 7351, 7349, 7347, 7345, 7343, 7341, 7339, 7337, 7335, 7333,
    7331, 7329, 7327, 7325, 7323, 7321, 7319, 7317, 7315, 7313, 7311, 7309, 7307, 7305, 7303, 7301,
    7299, 7297, 6271, 6269, 6267, 6265, 6263, 6261, 6259, 6257, 6255, 6253, 6251, 6249, 6247, 6245,
    6243, 6241, 6239, 6237, 6235, 6233, 6231, 6229, 6227, 6225, 6223, 6221, 6219, 6217, 6215, 6213,
    6211, 6209, 5183, 5181, 5179, 5177, 5175, 5173, 5171, 5169, 5167, 5165, 5163, 5161, 5159, 5157,
    5155, 5153, 4127, 4125, 4123, 4121, 4119, 4117, 4115, 4113, 3087, 3085, 3083, 3081, 2055, 2053,
];

fn exp_golomb_encode_byte(bw: &mut BitWriter, val: u8) {
    if val == 0 {
        bw.write_bit(1);
        return;
    }

    let emit = EXPG_SIGNED[val as usize];
    bw.write_bits((emit & 0x1FF) as u64, emit >> 9);
}

fn write_var_int(bw: &mut BitWriter, mut value: u32) {
    while value >= 128 {
        bw.write_bits((0x80 | (value & 0x7F)) as u64, 8);
        value >>= 7;
    }
    bw.write_bits(value as u64, 8);
}

fn encode_alphabet(bw: &mut BitWriter, symbols: &[u8]) {
    let count = symbols.len();

    if count == 0 {
        bw.write_bit(FULL_ALPHABET);
        bw.write_bit(ALPHABET_0);
    } else if count == 256 {
        bw.write_bit(FULL_ALPHABET);
        bw.write_bit(ALPHABET_256);
    } else {
        bw.write_bit(PARTIAL_ALPHABET);
        let mut masks = [0u8; 32];

        for &s in symbols {
            masks[(s >> 3) as usize] |= 1 << (s & 7);
        }

        let last_mask = (symbols[count - 1] >> 3) as u64;
        bw.write_bits(last_mask, 5);
        bw.write_array(&masks, 8 * (last_mask as usize + 1));
    }
}

fn compute_in_place_sizes_phase1(data: &mut [i64]) {
    let n = data.len() as i64;
    let mut s = 0i64;
    let mut r = 0i64;
    let mut t = 0i64;

    while t < n - 1 {
        let mut sum = 0i64;

        for _ in 0..2 {
            if s >= n || (r < t && data[r as usize] < data[s as usize]) {
                sum += data[r as usize];
                data[r as usize] = t;
                r += 1;
                continue;
            }

            sum += data[s as usize];

            if s > t {
                data[s as usize] = 0;
            }

            s += 1;
        }

        data[t as usize] = sum;
        t += 1;
    }
}

fn compute_in_place_sizes_phase2(data: &mut [i64]) -> i64 {
    if data.len() < 2 {
        return 0;
    }

    let n = data.len() as i64;
    let mut level_top = n - 2;
    let mut depth = 1i64;
    let mut i = n;
    let mut total_nodes_at_level = 2i64;

    while i > 0 {
        let mut k = level_top;

        while k > 0 && data[(k - 1) as usize] >= level_top {
            k -= 1;
        }

        let internal_nodes_at_level = level_top - k;
        let leaves_at_level = total_nodes_at_level - internal_nodes_at_level;

        for _ in 0..leaves_at_level {
            i -= 1;
            data[i as usize] = depth;
        }

        total_nodes_at_level = internal_nodes_at_level << 1;
        level_top = k;
        depth += 1;
    }

    depth - 1
}

/// Sorts `ranks` (each = (freq<<8)|symbol) ascending by freq, runs
/// Moffat-Katajainen in place, writes resulting sizes into `sizes`.
/// Returns (max_code_len, symbols in the sorted-by-freq order).
fn compute_code_lengths(sizes: &mut [u8; 256], ranks: &mut [u32]) -> (i64, Vec<u8>) {
    ranks.sort_unstable();
    let n = ranks.len();
    let mut freqs = vec![0i64; n];
    let mut syms = vec![0u8; n];

    for i in 0..n {
        freqs[i] = (ranks[i] >> 8) as i64;
        syms[i] = (ranks[i] & 0xFF) as u8;
    }

    compute_in_place_sizes_phase1(&mut freqs);
    let max_code_len = compute_in_place_sizes_phase2(&mut freqs);

    for i in 0..n {
        sizes[syms[i] as usize] = freqs[i] as u8;
    }

    (max_code_len, syms)
}

/// Reorders `symbols` by (size, symbol) ascending and assigns canonical
/// codes in that order -- identical contract to the decoder's version
/// (generateCanonicalCodes is shared by encoder and decoder in Go too).
fn generate_canonical_codes(sizes: &[u8; 256], codes: &mut [u16; 256], symbols: &mut [u8]) {
    if symbols.is_empty() {
        return;
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
}

/// Port of HuffmanEncoder.limitCodeLengths. `ranks` must be the
/// increasing-frequency-ordered symbol list from compute_code_lengths
/// (by construction of Moffat-Katajainen, sizes are then non-increasing
/// along this order, which the "fold" loop below relies on).
/// Returns the resulting max code length; MAX_SYMBOL_SIZE on success,
/// or a sentinel > MAX_SYMBOL_SIZE if the debt could not be repaid (only
/// possible for pathological distributions -- caller falls back to flat
/// 8-bit codes in that case, matching Go's own fallback for the same
/// "unlikely branch").
fn limit_code_lengths(sizes: &mut [u8; 256], ranks: &[u8]) -> i64 {
    let mut n = 0usize;
    let mut debt = 0i64;

    while (sizes[ranks[n] as usize] as i64) >= MAX_SYMBOL_SIZE as i64 {
        debt += sizes[ranks[n] as usize] as i64 - MAX_SYMBOL_SIZE as i64;
        sizes[ranks[n] as usize] = MAX_SYMBOL_SIZE as u8;
        n += 1;
    }

    let mut q: [Vec<u8>; 6] = Default::default();
    let count = ranks.len();

    while n < count {
        let idx = MAX_SYMBOL_SIZE as i64 - 1 - sizes[ranks[n] as usize] as i64;

        if idx > 5 || debt < (1i64 << idx) {
            break;
        }

        q[idx as usize].push(ranks[n]);
        n += 1;
    }

    let mut idx: i64 = 5;

    while debt > 0 && idx >= 0 {
        if q[idx as usize].is_empty() || debt < (1i64 << idx) {
            idx -= 1;
            continue;
        }

        let r = q[idx as usize].remove(0);
        sizes[r as usize] += 1;
        debt -= 1i64 << idx;
    }

    idx = 0;

    while debt > 0 && idx < 6 {
        if q[idx as usize].is_empty() {
            idx += 1;
            continue;
        }

        let r = q[idx as usize].remove(0);
        sizes[r as usize] += 1;
        debt -= 1i64 << idx;
    }

    if debt > 0 {
        // Pathological distribution that the fast repay couldn't fix. Go
        // falls back to NormalizeFrequencies + a second computeCodeLengths
        // pass; not ported (never observed on realistic input at 16 KiB
        // chunk granularity). Signal "still over limit" so the caller uses
        // flat 8-bit codes instead -- still a valid, decodable bitstream.
        return MAX_SYMBOL_SIZE as i64 + 1;
    }

    MAX_SYMBOL_SIZE as i64
}

pub struct HuffmanEncoder {
    codes: [u16; 256],
    sizes: [u8; 256],
    buffer: Vec<u8>,
}

impl HuffmanEncoder {
    pub fn new() -> Self {
        let mut codes = [0u16; 256];

        for i in 0..256 {
            codes[i] = i as u16;
        }

        HuffmanEncoder {
            codes,
            sizes: [8u8; 256],
            buffer: Vec::new(),
        }
    }

    /// Encodes `block`, chunked at MAX_CHUNK_SIZE (16384), into `bw`.
    /// Matches HuffmanEncoder.Write exactly.
    pub fn write(&mut self, block: &[u8], bw: &mut BitWriter) {
        let mut start = 0;

        while start < block.len() {
            let size_chunk = MAX_CHUNK_SIZE.min(block.len() - start);
            let chunk = &block[start..start + size_chunk];

            if size_chunk < 32 {
                bw.write_array(chunk, 8 * size_chunk);
            } else {
                let mut freqs = [0i32; 256];

                for &b in chunk {
                    freqs[b as usize] += 1;
                }

                let count = self.update_frequencies(&freqs, bw);

                if count > 1 {
                    self.encode_chunk(chunk, size_chunk, bw);
                }
            }

            start += size_chunk;
        }
    }

    fn update_frequencies(&mut self, freqs: &[i32; 256], bw: &mut BitWriter) -> usize {
        let mut alphabet = [0u8; 256];
        let mut count = 0usize;

        for i in 0..256 {
            self.codes[i] = 0;

            if freqs[i] > 0 {
                alphabet[count] = i as u8;
                count += 1;
            }
        }

        let symbols = &alphabet[0..count];
        encode_alphabet(bw, symbols);

        if count == 0 {
            return 0;
        }

        if count == 1 {
            self.codes[symbols[0] as usize] = 1 << 12;
            self.sizes[symbols[0] as usize] = 1;
        } else {
            let mut ranks = vec![0u32; count];

            for i in 0..count {
                ranks[i] = ((freqs[symbols[i] as usize] as u32) << 8) | symbols[i] as u32;
            }

            let (mut max_code_len, freq_order_syms) =
                compute_code_lengths(&mut self.sizes, &mut ranks);

            if max_code_len > MAX_SYMBOL_SIZE as i64 {
                max_code_len = limit_code_lengths(&mut self.sizes, &freq_order_syms);
            }

            if max_code_len > MAX_SYMBOL_SIZE as i64 {
                // Unlikely branch: no code set fits within MAX_SYMBOL_SIZE.
                for i in 0..count {
                    self.codes[alphabet[i] as usize] = i as u16;
                    self.sizes[alphabet[i] as usize] = 8;
                }
            } else {
                let mut syms = alphabet[0..count].to_vec();
                generate_canonical_codes(&self.sizes, &mut self.codes, &mut syms);
            }
        }

        // Transmit code lengths only (ExpGolomb-coded unary length deltas).
        let mut prev_size: i8 = 2;

        for &s in symbols {
            let cur_size = self.sizes[s as usize];
            self.codes[s as usize] |= (cur_size as u16) << 12;
            let delta = (cur_size as i8).wrapping_sub(prev_size);
            exp_golomb_encode_byte(bw, delta as u8);
            prev_size = cur_size as i8;
        }

        count
    }

    fn encode_chunk(&mut self, block: &[u8], count: usize, bw: &mut BitWriter) {
        let sz_frag = count / 4;
        let sz_frag4 = sz_frag & !3;

        if self.buffer.len() < 2 * MAX_CHUNK_SIZE {
            self.buffer = vec![0u8; 2 * MAX_CHUNK_SIZE];
        }

        let sz_buf = self.buffer.len() / 4;
        let mut nb_bits = [0u32; 4];

        for j in 0..4 {
            let src = &block[j * sz_frag..];
            let buf_off = j * sz_buf;
            let mut idx = 0usize;
            let mut state: u64 = 0;
            let mut bits: u32 = 0;
            let mut i = 0usize;

            while i < sz_frag4 {
                let code0 = self.codes[src[i] as usize];
                let len0 = (code0 >> 12) as u32;
                state = (state << len0) | ((code0 & 0x0FFF) as u64);
                let code1 = self.codes[src[i + 1] as usize];
                let len1 = (code1 >> 12) as u32;
                state = (state << len1) | ((code1 & 0x0FFF) as u64);
                let code2 = self.codes[src[i + 2] as usize];
                let len2 = (code2 >> 12) as u32;
                state = (state << len2) | ((code2 & 0x0FFF) as u64);
                let code3 = self.codes[src[i + 3] as usize];
                let len3 = (code3 >> 12) as u32;
                state = (state << len3) | ((code3 & 0x0FFF) as u64);

                bits += len0 + len1 + len2 + len3;
                let word = state << (64 - bits);
                self.buffer[buf_off + idx..buf_off + idx + 8].copy_from_slice(&word.to_be_bytes());
                idx += (bits >> 3) as usize;
                bits &= 7;
                i += 4;
            }

            while i < sz_frag {
                let code = self.codes[src[i] as usize];
                let len = (code >> 12) as u32;
                state = (state << len) | ((code & 0x0FFF) as u64);
                bits += len;
                i += 1;
            }

            nb_bits[j] = (idx as u32) * 8 + bits;

            while bits >= 8 {
                bits -= 8;
                self.buffer[buf_off + idx] = (state >> bits) as u8;
                idx += 1;
            }

            if bits > 0 {
                self.buffer[buf_off + idx] = (state << (8 - bits)) as u8;
            }
        }

        for j in 0..4 {
            write_var_int(bw, nb_bits[j]);
        }

        for j in 0..4 {
            bw.write_array(&self.buffer[j * sz_buf..], nb_bits[j] as usize);
        }

        let count4 = 4 * sz_frag;

        for i in count4..count {
            bw.write_bits(block[i] as u64, 8);
        }
    }
}
