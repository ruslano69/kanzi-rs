// Port of kanzi-go's SRT (transform/SRT.go) -- Sorted Ranks Transform,
// used as the SRT stage at level 6. Both Forward and Inverse are fully
// ported, including the Shell-sort (3x+1) symbol ordering, the SWAR run
// collapse in the encoder and the memmove rank shifts in the decoder.

pub const SRT_MAX_HEADER_SIZE: usize = 4 * 256;

pub struct Srt;

impl Srt {
    pub fn new() -> Self {
        Srt
    }

    pub fn max_encoded_len(src_len: usize) -> usize {
        src_len + SRT_MAX_HEADER_SIZE
    }

    /// Shell-sorts symbols by (frequency, symbol): ascending frequency is
    /// achieved by shifting smaller frequencies right (see Go's condition),
    /// ties by ascending symbol. Returns the present-symbol count.
    /// Port of Go's SRT.preprocess (h = 4, *3+1 / /=3, Shell insertion).
    fn preprocess(freqs: &[i32; 256], symbols: &mut [u8; 256]) -> usize {
        let mut nb_symbols = 0usize;

        for (i, &f) in freqs.iter().enumerate() {
            if f == 0 {
                continue;
            }

            symbols[nb_symbols] = i as u8;
            nb_symbols += 1;
        }

        let mut h = 4usize;

        while h < nb_symbols {
            h = h * 3 + 1;
        }

        loop {
            h /= 3;

            for i in h..nb_symbols {
                let t = symbols[i];
                let mut b = i as i64 - h as i64;

                while b >= 0
                    && (freqs[symbols[b as usize] as usize] < freqs[t as usize]
                        || (t < symbols[b as usize]
                            && freqs[t as usize] == freqs[symbols[b as usize] as usize]))
                {
                    symbols[(b + h as i64) as usize] = symbols[b as usize];
                    b -= h as i64;
                }

                symbols[(b + h as i64) as usize] = t;
            }

            if h == 1 {
                break;
            }
        }

        nb_symbols
    }

    pub fn forward(&self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        let count = src.len();

        if count == 0 || dst.is_empty() {
            return Ok((0, 0));
        }

        if dst.len() < Self::max_encoded_len(count) {
            return Err("Output buffer is too small");
        }

        let mut s2r = [0u8; 256];
        let mut r2s = [0u8; 256];
        let mut freqs = [0i32; 256];

        // find first symbols and count occurrences. Bounds-check-eliminated
        // like sbrt.rs's forward(): `c` is a byte value (<256), `b` counts
        // distinct byte values seen so far (<=256), and `i`/`j` are kept
        // strictly < count by both while-conditions below, so every access
        // is provably in range.
        let mut b = 0usize;
        let mut i = 0usize;

        while i < count {
            let ci = unsafe { *src.get_unchecked(i) };
            let c = ci as usize;

            if unsafe { *freqs.get_unchecked(c) } == 0 {
                unsafe {
                    *r2s.get_unchecked_mut(b) = ci;
                    *s2r.get_unchecked_mut(c) = b as u8;
                }
                b += 1;
            }

            let mut j = i + 1;

            while j < count && unsafe { *src.get_unchecked(j) } == ci {
                j += 1;
            }

            unsafe { *freqs.get_unchecked_mut(c) += (j - i) as i32 };
            i = j;
        }

        // init arrays
        let mut symbols = [0u8; 256];
        let nb_symbols = Self::preprocess(&freqs, &mut symbols);
        let mut buckets = [0usize; 256];
        let mut bucket_pos = 0usize;

        for i in 0..nb_symbols {
            let c = symbols[i] as usize;
            buckets[c] = bucket_pos;
            bucket_pos += freqs[c] as usize;
        }

        let header_size = Self::encode_header(&freqs, dst);

        // encoding (dst indices below are header-relative; Go resliced dst).
        // Bounds-check-eliminated like sbrt.rs's forward(): `c`/`t` are byte
        // values (<256), `r` is a rank slot (<256, and only decremented
        // while >0 below), and `i`/`p` stay within `count`/`dst.len()`
        // (checked via `dst.len() < max_encoded_len(count)` above, which
        // covers every byte of `src` plus the header) -- this is the same
        // tight serial dependency chain as SBRT's rank-list maintenance,
        // just against a bucketed/frequency-sorted rank list instead of a
        // move-to-front one.
        let mut i = 0usize;

        while i < count {
            let c = unsafe { *src.get_unchecked(i) };
            let mut r = unsafe { *s2r.get_unchecked(c as usize) } as usize;
            let mut p = unsafe { *buckets.get_unchecked(c as usize) };
            unsafe { *dst.get_unchecked_mut(header_size + p) = r as u8 };
            p += 1;

            if r > 0 {
                loop {
                    let t = unsafe { *r2s.get_unchecked(r - 1) };
                    unsafe {
                        *r2s.get_unchecked_mut(r) = t;
                        *s2r.get_unchecked_mut(t as usize) = r as u8;
                    }

                    if r == 1 {
                        break;
                    }

                    r -= 1;
                }

                unsafe {
                    *r2s.get_unchecked_mut(0) = c;
                    *s2r.get_unchecked_mut(c as usize) = 0;
                }
            }

            i += 1;

            // SWAR: collapse runs of c 8 bytes at a time
            if i + 8 <= count {
                let rep = 0x0101_0101_0101_0101u64.wrapping_mul(c as u64);

                while i + 8 <= count
                    && u64::from_le_bytes(src[i..i + 8].try_into().unwrap()) == rep
                {
                    dst[header_size + p..header_size + p + 8].fill(0);
                    p += 8;
                    i += 8;
                }
            }

            while i < count && unsafe { *src.get_unchecked(i) } == c {
                unsafe { *dst.get_unchecked_mut(header_size + p) = 0 };
                p += 1;
                i += 1;
            }

            unsafe { *buckets.get_unchecked_mut(c as usize) = p };
        }

        Ok((count, count + header_size))
    }

    fn encode_header(freqs: &[i32; 256], dst: &mut [u8]) -> usize {
        let mut n = 0usize;

        for &f in freqs.iter() {
            let mut f = f;

            while f >= 128 {
                dst[n] = (0x80 | (f & 0x7F)) as u8;
                n += 1;
                f >>= 7;
            }

            dst[n] = f as u8;
            n += 1;
        }

        n
    }

    pub fn inverse(&self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        // init arrays
        let mut freqs = [0i32; 256];
        let header_size = Self::decode_header(src, &mut freqs)?;

        let data_length = src.len() - header_size;
        let mut total_freq = 0u64;

        for &freq in freqs.iter() {
            if freq < 0 {
                return Err("SRT inverse transform failed: invalid data");
            }

            total_freq += freq as u64;
        }

        if total_freq != data_length as u64 {
            return Err("SRT inverse transform failed: invalid data");
        }

        let src_data = &src[header_size..];

        if src_data.len() > dst.len() {
            return Err("SRT inverse transform failed: invalid data");
        }

        let mut symbols = [0u8; 256];
        let mut nb_symbols = Self::preprocess(&freqs, &mut symbols);
        let mut buckets = [0usize; 256];
        let mut bucket_ends = [0usize; 256];
        let mut r2s = [0u8; 256];

        let mut bucket_pos = 0usize;

        for i in 0..nb_symbols {
            let c = symbols[i] as usize;

            if freqs[c] <= 0 || bucket_pos >= src_data.len() {
                return Err("SRT inverse transform failed: invalid data");
            }

            r2s[src_data[bucket_pos] as usize] = c as u8;
            buckets[c] = bucket_pos + 1;
            bucket_pos += freqs[c] as usize;

            if bucket_pos > src_data.len() {
                return Err("SRT inverse transform failed: invalid data");
            }

            bucket_ends[c] = bucket_pos;

            if i == nb_symbols - 1 && bucket_pos != src_data.len() {
                return Err("SRT inverse transform failed: invalid data");
            }
        }

        // decoding
        let mut c = r2s[0];

        for i in 0..dst.len() {
            // NOTE: Go iterates `range dst` (the FULL dst buffer, not just
            // dataLength) and the caller truncates to the returned length.
            // Our dst may be oversized scratch: decode exactly dataLength
            // symbols then stop (equivalent: extra iterations would only
            // append ignored garbage past the returned length... but they
            // also mutate r2s/buckets -- harmless post-return state).
            // To stay exact, iterate dataLength times like Go's effective
            // behavior on exactly-sized buffers.
            if i >= data_length {
                break;
            }

            dst[i] = c;

            if buckets[c as usize] < bucket_ends[c as usize] {
                let r = src_data[buckets[c as usize]];
                buckets[c as usize] += 1;

                if r == 0 {
                    continue;
                }

                // Shift ranks down by one. Port of kanzi-cpp's SRT::inverse:
                // small shifts (the common case -- recent symbols live at
                // small ranks) are unrolled inline instead of paying a
                // generic memmove call; large shifts use memmove.
                let r = r as usize;

                if r <= 8 {
                    if r >= 1 { r2s[0] = r2s[1]; }
                    if r >= 2 { r2s[1] = r2s[2]; }
                    if r >= 3 { r2s[2] = r2s[3]; }
                    if r >= 4 { r2s[3] = r2s[4]; }
                    if r >= 5 { r2s[4] = r2s[5]; }
                    if r >= 6 { r2s[5] = r2s[6]; }
                    if r >= 7 { r2s[6] = r2s[7]; }
                    if r >= 8 { r2s[7] = r2s[8]; }
                } else {
                    r2s.copy_within(1..r + 1, 0);
                }

                r2s[r] = c;
                c = r2s[0];
            } else {
                if nb_symbols == 1 {
                    continue;
                }

                nb_symbols -= 1;

                r2s.copy_within(1..nb_symbols + 1, 0);
                c = r2s[0];
            }
        }

        Ok((src.len(), data_length))
    }

    fn decode_header(src: &[u8], freqs: &mut [i32; 256]) -> Result<usize, &'static str> {
        let mut n = 0usize;

        for f in freqs.iter_mut() {
            if n >= src.len() {
                return Err("SRT inverse transform failed: truncated header");
            }

            let mut val = src[n] as i32;
            n += 1;

            if val < 128 {
                *f = val;
                continue;
            }

            let mut res = val & 0x7F;

            if n >= src.len() {
                return Err("SRT inverse transform failed: truncated header");
            }

            val = src[n] as i32;
            n += 1;
            res |= (val & 0x7F) << 7;

            if val >= 128 {
                if n >= src.len() {
                    return Err("SRT inverse transform failed: truncated header");
                }

                val = src[n] as i32;
                n += 1;
                res |= (val & 0x7F) << 14;

                if val >= 128 {
                    if n >= src.len() {
                        return Err("SRT inverse transform failed: truncated header");
                    }

                    val = src[n] as i32;
                    n += 1;
                    res |= (val & 0x7F) << 21;

                    if val >= 128 {
                        return Err("SRT inverse transform failed: invalid header");
                    }
                }
            }

            *f = res;
        }

        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let srt = Srt::new();
        let mut dst = vec![0u8; Srt::max_encoded_len(data.len())];
        let (read, written) = srt.forward(data, &mut dst).expect("forward failed");
        assert_eq!(read, data.len());
        dst.truncate(written);

        let mut back = vec![0u8; data.len()];
        let (_, back_len) = srt.inverse(&dst, &mut back).expect("inverse failed");
        back.truncate(back_len);
        assert_eq!(back, data, "SRT round-trip mismatch");
    }

    #[test]
    fn roundtrip_repetitive() {
        roundtrip(b"the quick brown fox jumps over the lazy dog. the quick brown fox.");
    }

    #[test]
    fn roundtrip_all_256_symbols() {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        roundtrip(&data);
    }

    #[test]
    fn roundtrip_single_symbol() {
        roundtrip(&[7u8; 500]);
    }

    #[test]
    fn inverse_rejects_truncated_header() {
        // Port of kanzi-go's Transforms_test.go "SRT truncated header" case:
        // a single byte >= 128 promises a multi-byte varint that never
        // arrives.
        let srt = Srt::new();
        let mut dst = vec![0u8; 8];
        assert!(srt.inverse(&[0x80], &mut dst).is_err());
    }

    #[test]
    fn inverse_rejects_inconsistent_frequency_table() {
        // A header whose frequencies don't sum to the payload length.
        let srt = Srt::new();
        let mut freqs = [0u8; 256];
        freqs[0] = 5; // claims 5 bytes of symbol 0, but no payload follows
        let mut dst = vec![0u8; 8];
        assert!(srt.inverse(&freqs, &mut dst).is_err());
    }
}

