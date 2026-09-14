// Port of kanzi-go's textCodec2 (transform/TextCodec.go), the variant used
// with HUFFMAN/ANS0/NONE/RANGE entropy (textcodec=2). textCodec1 (used with
// FPAQ/CM/TPAQ entropy) lives in text_codec1.rs and shares this module's
// static dictionary and small helpers (see the pub(crate) items below).
//
// Go's `*dictEntry` can alias either the embedded static dictionary or the
// current call's src buffer; in Rust this is modeled as DictEntry<'a>
// borrowing from whichever backing array, with 'a tied to one Forward/
// Inverse call (this.reset() rebuilds the whole dictionary unconditionally
// on every call in the original code too -- no cross-call persistence to
// replicate).

use crate::datatype::{detect_simple_type, DataType};
use crate::magic;

pub(crate) const TC_HASH1: i32 = 2146121005;
pub(crate) const TC_HASH2: i32 = -2073254261;
pub(crate) const TC_THRESHOLD2: i32 = 128 * 128;
const TC_V7_INDEX_BASE2: i32 = 63;
const TC_V7_INDEX_BASE3: i32 = 8255;
pub(crate) const TC_MAX_DICT_SIZE: usize = 1 << 19;
pub(crate) const TC_MAX_WORD_LENGTH: i32 = 31;
const TC_MIN_BLOCK_SIZE: usize = 1024;
pub(crate) const TC_ESCAPE_TOKEN1: u8 = 0x0F;
pub(crate) const TC_ESCAPE_TOKEN2: u8 = 0x0E;
pub(crate) const TC_MASK_FLIP_CASE: u8 = 0x80;
pub(crate) const TC_MASK_NOT_TEXT: u8 = 0x80;
pub(crate) const TC_MASK_CRLF: u8 = 0x40;
pub(crate) const TC_MASK_XML_HTML: u8 = 0x20;
pub(crate) const TC_MASK_TEXT_CODEC: u8 = 0x10;
pub(crate) const TC_MASK_DT: u8 = 0x0F;
pub(crate) const TC_MASK_LENGTH: i32 = 0x0007FFFF;
pub(crate) const CR: u8 = 0x0D;
pub(crate) const LF: u8 = 0x0A;

// Pre-filtered (letters only, exactly matching createDictionary's own
// isText() filter applied once at extraction time) copy of _TC_DICT_EN_1024.
pub(crate) const TC_DICT_EN_1024: &[u8] = include_bytes!("tc_dict_en_1024.txt");

#[inline]
pub(crate) fn is_lower_case(v: u8) -> bool {
    (b'a'..=b'z').contains(&v)
}

#[inline]
pub(crate) fn is_upper_case(v: u8) -> bool {
    (b'A'..=b'Z').contains(&v)
}

#[inline]
pub(crate) fn is_text(v: u8) -> bool {
    is_lower_case(v | 0x20)
}

#[inline]
pub(crate) fn is_delimiter(v: u8, table: &[bool; 256]) -> bool {
    table[v as usize]
}

pub(crate) fn init_delimiter_chars() -> [bool; 256] {
    let mut res = [false; 256];

    for i in (b' ' as usize)..=(b'/' as usize) {
        res[i] = true;
    }

    for i in (b':' as usize)..=(b'?' as usize) {
        res[i] = true;
    }

    for &c in &[b'\n', b'\r', b'\t', b'_', b'|', b'{', b'}', b'[', b']'] {
        res[c as usize] = true;
    }

    res
}

#[derive(Clone, Copy)]
pub(crate) struct DictEntry<'a> {
    pub(crate) hash: i32,
    pub(crate) data: i32,
    pub(crate) ptr: &'a [u8],
}

/// Shared static dictionary entries, built once (the `create_dictionary`
/// fold is deterministic over the fixed `TC_DICT_EN_1024` bytes).
/// Replaces the previous per-call `Box::leak` copy -- which leaked ~20KB
/// per TEXT call (unbounded growth for long-running processes) -- with a
/// single `OnceLock`-cached build; per-call setup now just memcpys the
/// entries (they are `Copy`). Same pattern as `tpaq.rs`'s static tables.
pub(crate) fn static_dict_entries() -> &'static [DictEntry<'static>] {
    use std::sync::OnceLock;
    static ENTRIES: OnceLock<&'static [DictEntry<'static>]> = OnceLock::new();
    *ENTRIES.get_or_init(|| {
        let bytes = TC_DICT_EN_1024.to_vec();
        let leaked: &'static mut [u8] = Box::leak(bytes.into_boxed_slice());
        let entries: Vec<DictEntry<'static>> = create_dictionary(leaked, 1024);
        Box::leak(entries.into_boxed_slice())
    })
}

#[inline]
pub(crate) fn same_words(a: &[u8], b: &[u8]) -> bool {
    a.len() <= b.len() && a == &b[..a.len()]
}

/// Builds the static 1024-entry dictionary. Two-pass: first fold each
/// word's leading uppercase letter to lowercase in place (matching Go's
/// `words[i] ^= 0x20` mutation-during-scan), then take immutable slices --
/// Rust can't alias a `Vec<DictEntry>` borrowing `words` while still
/// mutating `words` in the same pass the way Go's GC'd slices allow.
pub(crate) fn create_dictionary(
    words: &mut [u8],
    max_words: usize,
) -> Vec<DictEntry<'_>> {
    let n = words.len();
    let mut bounds: Vec<(usize, usize, i32)> = Vec::with_capacity(max_words);
    let mut anchor = 0usize;
    let mut h: i32 = TC_HASH1;
    let mut nb_words = 0usize;
    let mut i = 0usize;

    while i < n && nb_words < max_words {
        if is_upper_case(words[i]) {
            if i > anchor {
                bounds.push((anchor, i, h));
                nb_words += 1;
                anchor = i;
                h = TC_HASH1;
            }
            words[i] ^= 0x20;
        }
        h = h.wrapping_mul(TC_HASH1) ^ (words[i] as i32).wrapping_mul(TC_HASH2);
        i += 1;
    }

    if nb_words < max_words {
        bounds.push((anchor, n, h));
    }

    let words_ro: &[u8] = words;
    bounds
        .into_iter()
        .enumerate()
        .map(|(idx, (a, e, h))| DictEntry {
            hash: h,
            data: (((e - a) as i32) << 24) | idx as i32,
            ptr: &words_ro[a..],
        })
        .collect()
}

/// Port of computeTextStats + detectTextType combined. `strict` selects Go's
/// strict text-detection heuristic (true for textCodec1, false for
/// textCodec2). Returns the mode byte written as dst[0].
pub(crate) fn compute_text_stats(block: &[u8], strict: bool) -> u8 {
    if !strict && magic::get_magic_type(block) != magic::NO_MAGIC {
        // This is going to fail if the block is not the first of the file.
        // But this is a cheap test, good enough for fast mode.
        return TC_MASK_NOT_TEXT;
    }

    let count = block.len();
    let mut freqs0 = [0i64; 256];
    let mut freqs1 = vec![0i64; 65536]; // [256][256], flattened

    let end4 = count & !3;
    let mut prv: u8 = 0;
    let mut i = 0usize;

    while i < end4 {
        let c0 = block[i];
        let c1 = block[i + 1];
        let c2 = block[i + 2];
        let c3 = block[i + 3];
        freqs0[c0 as usize] += 1;
        freqs0[c1 as usize] += 1;
        freqs0[c2 as usize] += 1;
        freqs0[c3 as usize] += 1;
        freqs1[(prv as usize) * 256 + c0 as usize] += 1;
        freqs1[(c0 as usize) * 256 + c1 as usize] += 1;
        freqs1[(c1 as usize) * 256 + c2 as usize] += 1;
        freqs1[(c2 as usize) * 256 + c3 as usize] += 1;
        prv = c3;
        i += 4;
    }

    while i < count {
        let c = block[i];
        freqs0[c as usize] += 1;
        freqs1[(prv as usize) * 256 + c as usize] += 1;
        prv = c;
        i += 1;
    }

    let mut nb_text_chars = freqs0[CR as usize] + freqs0[LF as usize];
    let mut nb_ascii = 0i64;

    for i in 0..128 {
        if is_text(i as u8) {
            nb_text_chars += freqs0[i];
        }
        nb_ascii += freqs0[i];
    }

    let nb_bin_chars = count as i64 - nb_ascii;
    let not_text = if nb_bin_chars > (count as i64 >> 2) {
        true
    } else {
        let mut nt = nb_text_chars < (count as i64 / 4);

        if strict {
            nt = nt
                || (freqs0[0] >= (count as i64 / 100))
                || ((nb_ascii / 95) < (count as i64 / 100));
        } else {
            nt = nt || (freqs0[32] < (count as i64 / 50));
        }

        nt
    };

    if not_text {
        return detect_text_type(&freqs0, &freqs1, count);
    }

    let mut res: u8 = 0;

    if nb_bin_chars <= count as i64 - count as i64 / 10 {
        let f1 = freqs0[b'<' as usize];
        let f2 = freqs0[b'>' as usize];
        let f3 = freqs1[(b'&' as usize) * 256 + b'a' as usize]
            + freqs1[(b'&' as usize) * 256 + b'g' as usize]
            + freqs1[(b'&' as usize) * 256 + b'l' as usize]
            + freqs1[(b'&' as usize) * 256 + b'q' as usize];
        let min_freq = ((count as i64 - nb_bin_chars) >> 9).max(2);

        if f1 >= min_freq && f2 >= min_freq && f3 > 0 {
            if f1 < f2 {
                if f1 >= f2 - f2 / 100 {
                    res |= TC_MASK_XML_HTML;
                }
            } else if f2 < f1 {
                if f2 >= f1 - f1 / 100 {
                    res |= TC_MASK_XML_HTML;
                }
            } else {
                res |= TC_MASK_XML_HTML;
            }
        }
    }

    if freqs0[CR as usize] != 0 && freqs0[CR as usize] == freqs0[LF as usize] {
        let mut is_crlf = true;

        for i in 0..256 {
            if i != LF as usize && freqs1[(CR as usize) * 256 + i] != 0 {
                is_crlf = false;
                break;
            }

            if i != CR as usize && freqs1[i * 256 + LF as usize] != 0 {
                is_crlf = false;
                break;
            }
        }

        if is_crlf {
            res |= TC_MASK_CRLF;
        }
    }

    res
}

fn detect_text_type(freqs0: &[i64; 256], freqs: &[i64], count: usize) -> u8 {
    let mut freqs0_i32 = [0i32; 256];

    for i in 0..256 {
        freqs0_i32[i] = freqs0[i] as i32;
    }

    let dt = detect_simple_type(count, &freqs0_i32);

    if dt != DataType::Undefined {
        return TC_MASK_NOT_TEXT | data_type_code(dt);
    }

    let mut sum: i64 = freqs0[0xC0] + freqs0[0xC1];

    for f in &freqs0[0xF5..256] {
        sum += f;
    }

    if sum != 0 {
        return TC_MASK_NOT_TEXT;
    }

    let mut sum2: i64 = 0;

    for i in 0..256usize {
        if i < 0xA0 || i > 0xBF {
            sum += freqs[0xE0 * 256 + i];
        }

        if i < 0x80 || i > 0x9F {
            sum += freqs[0xED * 256 + i];
        }

        if i < 0x90 || i > 0xBF {
            sum += freqs[0xF0 * 256 + i];
        }

        if i < 0x80 || i > 0x8F {
            sum += freqs[0xF4 * 256 + i];
        }

        if i < 0x80 || i > 0xBF {
            for j in 0xC2..=0xDF {
                sum += freqs[j * 256 + i];
            }

            for j in 0xE1..=0xEC {
                sum += freqs[j * 256 + i];
            }

            sum += freqs[0xF1 * 256 + i];
            sum += freqs[0xF2 * 256 + i];
            sum += freqs[0xF3 * 256 + i];
            sum += freqs[0xEE * 256 + i];
            sum += freqs[0xEF * 256 + i];
        } else {
            sum2 += freqs0[i];
        }

        if sum != 0 {
            return TC_MASK_NOT_TEXT;
        }
    }

    if sum2 >= (count as i64) / 8 {
        TC_MASK_NOT_TEXT | data_type_code(DataType::Utf8)
    } else {
        TC_MASK_NOT_TEXT
    }
}

fn data_type_code(dt: DataType) -> u8 {
    dt as u8
}

fn log2_clamped(x: u32, lo: u32, hi: u32) -> u32 {
    if x == 0 {
        return lo;
    }

    (31 - x.leading_zeros()).clamp(lo, hi)
}

struct TextState<'a> {
    dict_list: Vec<DictEntry<'a>>,
    dict_map: Vec<u32>, // index into dict_list, u32::MAX = empty
    hash_mask: i32,
    dict_size: usize,
    static_dict_size: usize,
    delim: [bool; 256],
    is_crlf: bool,
}

const EMPTY: u32 = u32::MAX;

impl<'a> TextState<'a> {
    fn new(count: usize, block_size: u32, entropy_tpaqx: bool) -> Self {
        let mut log = if block_size >= 32 {
            let l = log2_clamped(block_size / 32, 13, 24);
            l
        } else {
            13
        };

        if entropy_tpaqx {
            log += 1;
        }

        let mut dict_size = 1usize << 13;

        if count >= 1024 {
            let l = log2_clamped((count / 128) as u32, 13, 18);
            dict_size = 1usize << l;
        }

        let hash_mask = (1i32 << log) - 1;

        // Shared static dictionary (built once, see `static_dict_entries`).
        let static_entries = static_dict_entries();
        let static_dict_size = static_entries.len();

        let mut dict_list: Vec<DictEntry<'a>> = Vec::with_capacity(dict_size);

        for e in static_entries.iter().take(dict_size.min(1024)) {
            dict_list.push(DictEntry {
                hash: e.hash,
                data: e.data,
                ptr: e.ptr,
            });
        }

        for i in dict_list.len()..dict_size {
            dict_list.push(DictEntry {
                hash: 0,
                data: i as i32,
                ptr: &[],
            });
        }

        let mut dict_map = vec![EMPTY; 1usize << log];

        for i in 0..static_dict_size {
            let slot = (dict_list[i].hash & hash_mask) as usize;
            dict_map[slot] = i as u32;
        }

        TextState {
            dict_list,
            dict_map,
            hash_mask,
            dict_size,
            static_dict_size,
            delim: init_delimiter_chars(),
            is_crlf: false,
        }
    }

    fn expand_dictionary(&mut self) -> bool {
        if self.dict_size >= TC_MAX_DICT_SIZE {
            return false;
        }

        for i in self.dict_size..self.dict_size * 2 {
            self.dict_list.push(DictEntry {
                hash: 0,
                data: i as i32,
                ptr: &[],
            });
        }

        self.dict_size <<= 1;
        true
    }
}

fn emit_word_index(dst: &mut [u8], w_idx0: i32) -> usize {
    let mut w_idx = w_idx0;

    if w_idx < TC_V7_INDEX_BASE2 {
        dst[0] = 0x80 | ((w_idx + 1) as u8);
        return 1;
    }

    if w_idx < TC_V7_INDEX_BASE3 {
        w_idx -= TC_V7_INDEX_BASE2;
        dst[0] = 0xC0 | ((w_idx >> 8) as u8);
        dst[1] = w_idx as u8;
        return 2;
    }

    w_idx -= TC_V7_INDEX_BASE3;
    dst[0] = 0xF0 | ((w_idx >> 16) as u8);
    dst[1] = (w_idx >> 8) as u8;
    dst[2] = w_idx as u8;
    3
}

fn emit_symbols(src: &[u8], dst: &mut [u8], is_crlf: bool) -> usize {
    let mut dst_idx = 0usize;

    for &cur in src {
        match cur {
            TC_ESCAPE_TOKEN1 => {
                if dst_idx + 1 >= dst.len() {
                    return dst.len() + 1;
                }
                dst[dst_idx] = TC_ESCAPE_TOKEN1;
                dst_idx += 1;
                dst[dst_idx] = TC_ESCAPE_TOKEN1;
                dst_idx += 1;
            }
            CR => {
                if !is_crlf {
                    if dst_idx >= dst.len() {
                        return dst.len() + 1;
                    }
                    dst[dst_idx] = cur;
                    dst_idx += 1;
                }
            }
            _ => {
                if cur >= 0x80 {
                    if dst_idx >= dst.len() {
                        return dst.len() + 1;
                    }
                    dst[dst_idx] = TC_ESCAPE_TOKEN1;
                    dst_idx += 1;
                }
                if dst_idx >= dst.len() {
                    return dst.len() + 1;
                }
                dst[dst_idx] = cur;
                dst_idx += 1;
            }
        }
    }

    dst_idx
}

pub fn max_encoded_len(src_len: usize) -> usize {
    src_len
}

/// textCodec2.Forward, wrapped with TextCodec's outer bit4 marker (always
/// set -- we only ever use encodingType=2). Returns Err on decline (mirrors
/// every "not text" / "too small" / "no room" condition in Go). The
/// DataType accompanies both Ok and Err exactly like Go's ctx["dataType"]
/// side effect, which is set even when this stage declines.
pub fn forward(
    src: &[u8],
    dst: &mut [u8],
    block_size: u32,
    entropy_tpaqx: bool,
) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    let count = src.len();

    if count < TC_MIN_BLOCK_SIZE {
        return Err(("block too small", DataType::Undefined));
    }

    if dst.len() < max_encoded_len(count) {
        return Err(("output buffer too small", DataType::Undefined));
    }

    let mode = compute_text_stats(src, false);

    if mode & TC_MASK_NOT_TEXT != 0 {
        let dt = match mode & TC_MASK_DT {
            0 => DataType::Undefined,
            1 => DataType::Text,
            4 => DataType::Numeric,
            5 => DataType::Base64,
            6 => DataType::Dna,
            7 => DataType::Bin,
            8 => DataType::Utf8,
            9 => DataType::SmallAlphabet,
            _ => DataType::Undefined,
        };
        return Err(("not text", dt));
    }

    let mut st = TextState::new(count, block_size, entropy_tpaqx);
    st.is_crlf = mode & TC_MASK_CRLF != 0;

    let src_end = count;
    let dst_end = max_encoded_len(count);
    let dst_end3 = dst_end - 3;
    let mut emit_anchor = 0usize;
    let mut words = st.static_dict_size;

    dst[0] = mode;
    let mut src_idx = 0usize;
    let mut dst_idx = 1usize;

    while src_idx < src_end && src[src_idx] == b' ' {
        dst[dst_idx] = b' ';
        src_idx += 1;
        dst_idx += 1;
        emit_anchor += 1;
    }

    let mut err: Option<&'static str> = None;
    let mut delim_anchor: i64 = src_idx as i64;

    if is_text(src[src_idx]) {
        delim_anchor = src_idx as i64 - 1;
    }

    while src_idx < src_end {
        if is_text(src[src_idx]) {
            src_idx += 1;
            continue;
        }

        if (src_idx as i64) > delim_anchor + 2 && is_delimiter(src[src_idx], &st.delim) {
            let length = src_idx as i64 - delim_anchor - 1;

            if length <= TC_MAX_WORD_LENGTH as i64 {
                // Go works with a signed delimiter anchor (da may be -1 at
                // the start of the block, so da+1 == 0 is valid); keep it
                // signed here and cast only after adding -- `as usize` on -1
                // would wrap and the +1 would panic.
                let da = delim_anchor;
                let val = src[(da + 1) as usize];
                let mut h1 = TC_HASH1;
                h1 = h1.wrapping_mul(TC_HASH1) ^ (val as i32).wrapping_mul(TC_HASH2);
                let mut h2 = TC_HASH1;
                h2 = h2.wrapping_mul(TC_HASH1) ^ ((val ^ 0x20) as i32).wrapping_mul(TC_HASH2);

                for i in ((da + 2) as usize)..src_idx {
                    let h = (src[i] as i32).wrapping_mul(TC_HASH2);
                    h1 = h1.wrapping_mul(TC_HASH1) ^ h;
                    h2 = h2.wrapping_mul(TC_HASH1) ^ h;
                }

                let slot1 = (h1 & st.hash_mask) as usize;
                let pe1_idx = st.dict_map[slot1];
                let mut pe_idx: u32 = EMPTY;

                if pe1_idx != EMPTY
                    && st.dict_list[pe1_idx as usize].hash == h1
                    && (st.dict_list[pe1_idx as usize].data >> 24) == length as i32
                {
                    pe_idx = pe1_idx;
                } else {
                    let slot2 = (h2 & st.hash_mask) as usize;
                    let pe2_idx = st.dict_map[slot2];

                    if pe2_idx != EMPTY
                        && st.dict_list[pe2_idx as usize].hash == h2
                        && (st.dict_list[pe2_idx as usize].data >> 24) == length as i32
                    {
                        pe_idx = pe2_idx;
                    }
                }

                if pe_idx != EMPTY {
                    let pe = st.dict_list[pe_idx as usize];

                    if !same_words(&pe.ptr[1..length as usize], &src[(da + 2) as usize..]) {
                        pe_idx = EMPTY;
                    }
                }

                if pe_idx == EMPTY {
                    if (length > 3 || (length == 3 && (words as i32) < TC_THRESHOLD2))
                        && pe1_idx == EMPTY
                    {
                        let pe = &mut st.dict_list[words];

                        if (pe.data & TC_MASK_LENGTH) as usize >= st.static_dict_size {
                            st.dict_map[(pe.hash & st.hash_mask) as usize] = EMPTY;
                            pe.ptr = &src[(da + 1) as usize..];
                            pe.hash = h1;
                            pe.data = ((length as i32) << 24) | words as i32;
                        }

                        st.dict_map[slot1] = words as u32;
                        words += 1;

                        if words >= st.dict_size {
                            if !st.expand_dictionary() {
                                words = st.static_dict_size;
                            }
                        }
                    }
                } else {
                    let pe = st.dict_list[pe_idx as usize];

                    // Go: `emitAnchor != da || src[da] != ' '` with signed da
                    // (short-circuits before src[-1] when da == -1).
                    if emit_anchor as i64 != da || src[da as usize] != b' ' {
                        dst_idx += emit_symbols(
                            &src[emit_anchor..(da + 1) as usize],
                            &mut dst[dst_idx..dst_end],
                            st.is_crlf,
                        );
                    }

                    if dst_idx >= dst_end3 {
                        err = Some("output buffer too small");
                        break;
                    }

                    if pe_idx != pe1_idx {
                        dst[dst_idx] = TC_MASK_FLIP_CASE;
                        dst_idx += 1;
                    }

                    dst_idx +=
                        emit_word_index(&mut dst[dst_idx..dst_idx + 3], pe.data & TC_MASK_LENGTH);
                    emit_anchor = (da + 1) as usize + (pe.data >> 24) as usize;
                }
            }
        }

        delim_anchor = src_idx as i64;
        src_idx += 1;
    }

    if err.is_none() {
        dst_idx += emit_symbols(
            &src[emit_anchor..src_end],
            &mut dst[dst_idx..dst_end],
            st.is_crlf,
        );

        if dst_idx > dst_end {
            err = Some("output buffer too small");
        }
    }

    if err.is_none() && src_idx != src_end {
        err = Some("did not consume all input");
    }

    if let Some(e) = err {
        return Err((e, DataType::Text));
    }

    dst[0] |= TC_MASK_TEXT_CODEC;
    Ok((src_idx, dst_idx, DataType::Text))
}

pub fn inverse(
    src: &[u8],
    dst: &mut [u8],
    block_size: u32,
    entropy_tpaqx: bool,
) -> Result<(usize, usize), &'static str> {
    let mut st = TextState::new(dst.len(), block_size, entropy_tpaqx);
    let mut words = st.static_dict_size;
    let mut word_run = false;
    st.is_crlf = src[0] & TC_MASK_CRLF != 0;

    let mut src_idx = 1usize;
    let mut dst_idx = 0usize;
    let src_end = src.len();
    let dst_end = dst.len();
    let mut delim_anchor: i64 = src_idx as i64;

    if is_text(src[src_idx]) {
        delim_anchor = src_idx as i64 - 1;
    }

    let mut err: Option<&'static str> = None;

    while src_idx < src_end && dst_idx < dst_end {
        let mut cur = src[src_idx];

        if is_text(cur) {
            dst[dst_idx] = cur;
            src_idx += 1;
            dst_idx += 1;
            continue;
        }

        if (src_idx as i64) > delim_anchor + 3 && is_delimiter(cur, &st.delim) {
            let length = src_idx as i64 - delim_anchor - 1;

            if length <= TC_MAX_WORD_LENGTH as i64 {
                let da = delim_anchor as usize;
                let mut h1 = TC_HASH1;
                h1 = h1.wrapping_mul(TC_HASH1) ^ (src[da + 1] as i32).wrapping_mul(TC_HASH2);
                h1 = h1.wrapping_mul(TC_HASH1) ^ (src[da + 2] as i32).wrapping_mul(TC_HASH2);

                for i in (da + 3)..src_idx {
                    h1 = h1.wrapping_mul(TC_HASH1) ^ (src[i] as i32).wrapping_mul(TC_HASH2);
                }

                let slot1 = (h1 & st.hash_mask) as usize;
                let pe1_idx = st.dict_map[slot1];
                let mut pe_idx = EMPTY;

                if pe1_idx != EMPTY
                    && st.dict_list[pe1_idx as usize].hash == h1
                    && (st.dict_list[pe1_idx as usize].data >> 24) == length as i32
                {
                    pe_idx = pe1_idx;
                }

                if pe_idx == EMPTY
                    && (length > 3 || (words as i32) < TC_THRESHOLD2)
                    && pe1_idx == EMPTY
                {
                    let pe = &mut st.dict_list[words];

                    if (pe.data & TC_MASK_LENGTH) as usize >= st.static_dict_size {
                        st.dict_map[(pe.hash & st.hash_mask) as usize] = EMPTY;
                        pe.ptr = &src[da + 1..];
                        pe.hash = h1;
                        pe.data = ((length as i32) << 24) | words as i32;
                    }

                    st.dict_map[slot1] = words as u32;
                    words += 1;

                    if words >= st.dict_size {
                        if !st.expand_dictionary() {
                            words = st.static_dict_size;
                        }
                    }
                }
            }
        }

        src_idx += 1;
        let mut flip_mask: u8 = 0;

        if cur >= 128 {
            let rank_encoding = true; // bsVersion >= 7, always true for us

            if cur == TC_MASK_FLIP_CASE {
                flip_mask = 0x20;

                if src_idx >= src_end {
                    err = Some("truncated word index");
                    break;
                }

                cur = src[src_idx];
                src_idx += 1;
            }

            let mut idx = (cur as i32) & 0x7F;
            let one_byte = idx < 64;

            if idx >= 64 {
                let three_bytes = idx >= 112;

                if three_bytes {
                    if src_end - src_idx < 2 {
                        err = Some("truncated word index");
                        break;
                    }
                    idx = ((idx & 0x0F) << 16)
                        | ((src[src_idx] as i32) << 8)
                        | src[src_idx + 1] as i32;
                    src_idx += 2;
                } else {
                    if src_idx >= src_end {
                        err = Some("truncated word index");
                        break;
                    }
                    idx = ((idx & 0x1F) << 8) | src[src_idx] as i32;
                    src_idx += 1;
                }

                if rank_encoding {
                    if three_bytes {
                        idx += TC_V7_INDEX_BASE3;
                    } else {
                        idx += TC_V7_INDEX_BASE2;
                    }

                    if idx as usize >= st.dict_size {
                        err = Some("invalid index");
                        break;
                    }
                }
            }

            if idx == 0 {
                err = Some("invalid index");
                break;
            }

            if one_byte {
                idx -= 1;
            }

            let pe = st.dict_list[idx as usize];
            let length = (pe.data >> 24) & 0xFF;

            if length > 1 {
                if word_run {
                    dst[dst_idx] = b' ';
                    dst_idx += 1;
                }
                word_run = true;
                delim_anchor = src_idx as i64;
            } else {
                word_run = false;
                delim_anchor = src_idx as i64 - 1;
            }

            if pe.ptr.is_empty() || dst_idx + length as usize >= dst_end {
                err = Some("invalid input data");
                break;
            }

            dst[dst_idx..dst_idx + length as usize].copy_from_slice(&pe.ptr[0..length as usize]);
            dst[dst_idx] ^= flip_mask;
            dst_idx += length as usize;
        } else if cur == TC_ESCAPE_TOKEN1 {
            if src_idx >= src_end {
                err = Some("truncated escaped literal");
                break;
            }

            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;
            word_run = false;
            delim_anchor = src_idx as i64 - 1;
        } else {
            if st.is_crlf && cur == LF {
                dst[dst_idx] = CR;
                dst_idx += 1;

                if dst_idx >= dst_end {
                    err = Some("invalid input data");
                    break;
                }
            }

            dst[dst_idx] = cur;
            dst_idx += 1;
            word_run = false;
            delim_anchor = src_idx as i64 - 1;
        }
    }

    if let Some(e) = err {
        return Err(e);
    }

    if src_idx != src_end {
        return Err("did not consume all input");
    }

    Ok((src_idx, dst_idx))
}
