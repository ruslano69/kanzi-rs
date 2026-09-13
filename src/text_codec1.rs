// Port of kanzi-go's textCodec1 (transform/TextCodec.go), the variant used
// with FPAQ/CM/TPAQ entropy (textcodec=1). Shares the static dictionary and
// small helpers with text_codec.rs (textCodec2); the word-index encoding,
// escape handling and dictionary-reset differ.
//
// Fidelity notes:
// - Like textCodec2, the static dictionary is a per-call leaked copy to keep
//   the DictEntry lifetimes simple (see text_codec.rs module doc).
// - The two special static entries (escape tokens, indices base and base+1)
//   are appended exactly as Go's reset does.
// - `pe == pe1` pointer identity is modeled as an index comparison.

use crate::datatype::DataType;
use crate::text_codec::{
    compute_text_stats, create_dictionary, init_delimiter_chars, is_delimiter, is_text,
    same_words, DictEntry, TC_DICT_EN_1024, TC_ESCAPE_TOKEN1, TC_ESCAPE_TOKEN2, TC_HASH1,
    TC_HASH2, TC_MASK_CRLF, TC_MASK_DT, TC_MASK_LENGTH, TC_MASK_NOT_TEXT, TC_MAX_DICT_SIZE,
    TC_MAX_WORD_LENGTH, TC_THRESHOLD2, CR, LF,
};

const TC1_MIN_BLOCK_SIZE: usize = 1024;
const TC1_THRESHOLD1: i32 = 128;
const EMPTY: u32 = u32::MAX;

fn log2_no_check(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// Mirrors newTextCodec1WithCtx's logHashSize computation (blockSize/8).
fn log_hash_size(block_size: u32, entropy_tpaqx: bool) -> u32 {
    let mut log = 13u32;

    if block_size >= 8 {
        log = log2_no_check(block_size / 8).clamp(13, 26);
    }

    if entropy_tpaqx {
        log += 1;
    }

    log
}

/// Mirrors reset(count)'s dictSize computation (count/128).
fn dict_size_for(count: usize) -> usize {
    if count >= 1024 {
        1usize << log2_no_check((count / 128) as u32).clamp(13, 18)
    } else {
        1usize << 13
    }
}

struct TextState1<'a> {
    dict_list: Vec<DictEntry<'a>>,
    dict_map: Vec<u32>,
    hash_mask: i32,
    dict_size: usize,
    static_dict_size: usize,
    delim: [bool; 256],
    is_crlf: bool,
}

impl<'a> TextState1<'a> {
    fn new(count: usize, block_size: u32, entropy_tpaqx: bool) -> Self {
        let log = log_hash_size(block_size, entropy_tpaqx);
        let dict_size = dict_size_for(count);
        let hash_mask = (1i32 << log) - 1;

        // Fresh copy of the static dictionary, leaked so entries can hold
        // 'static slices while dynamic entries hold slices of src.
        let static_dict: Vec<DictEntry<'static>> = {
            let mut dict_bytes = TC_DICT_EN_1024.to_vec();
            let leaked: &'static mut [u8] =
                Box::leak(dict_bytes.drain(..).collect::<Vec<u8>>().into_boxed_slice());
            create_dictionary(leaked, 1024)
        };
        let base = static_dict.len();

        let mut dict_list: Vec<DictEntry<'a>> = Vec::with_capacity(dict_size);

        for e in static_dict.into_iter() {
            dict_list.push(DictEntry {
                hash: e.hash,
                data: e.data,
                ptr: e.ptr,
            });
        }

        // Add special entries at end of static dictionary (Go indices base,
        // base+1: token2 then token1).
        dict_list.push(DictEntry {
            hash: 0,
            data: ((1i32 << 24) | base as i32),
            ptr: &[TC_ESCAPE_TOKEN2],
        });
        dict_list.push(DictEntry {
            hash: 0,
            data: ((1i32 << 24) | (base as i32 + 1)),
            ptr: &[TC_ESCAPE_TOKEN1],
        });
        let static_dict_size = base + 2;

        for i in dict_list.len()..dict_size {
            dict_list.push(DictEntry {
                hash: 0,
                data: i as i32,
                ptr: &[],
            });
        }

        let mut dict_map = vec![EMPTY; 1usize << log];

        for i in 0..static_dict_size {
            dict_map[(dict_list[i].hash & hash_mask) as usize] = i as u32;
        }

        TextState1 {
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

/// emitWordIndex1 (varint 5/7/7 bits), used by textCodec1.
fn emit_word_index1(dst: &mut [u8], val: i32) -> usize {
    if val < TC1_THRESHOLD1 {
        dst[0] = val as u8;
        return 1;
    }

    if val < TC_THRESHOLD2 {
        dst[0] = (0x80 | (val >> 7)) as u8;
        dst[1] = (0x7F & val) as u8;
        return 2;
    }

    dst[0] = (0xE0 | (val >> 14)) as u8;
    dst[1] = (0x80 | (val >> 7)) as u8;
    dst[2] = (0x7F & val) as u8;
    3
}

/// textCodec1.emitSymbols.
fn emit_symbols1(src: &[u8], dst: &mut [u8], is_crlf: bool, static_dict_size: usize) -> usize {
    let dst_end = dst.len();
    let mut dst_idx = 0usize;

    for &cur in src {
        if dst_idx >= dst_end {
            return dst_end + 1;
        }

        if cur == TC_ESCAPE_TOKEN1 || cur == TC_ESCAPE_TOKEN2 {
            dst[dst_idx] = TC_ESCAPE_TOKEN1;
            dst_idx += 1;

            let idx = if cur == TC_ESCAPE_TOKEN1 {
                static_dict_size as i32 - 1
            } else {
                static_dict_size as i32 - 2
            };

            let len_idx = if idx >= TC_THRESHOLD2 {
                3
            } else if idx < TC1_THRESHOLD1 {
                1
            } else {
                2
            };

            if dst_idx + len_idx >= dst_end {
                return dst_end + 1;
            }

            dst_idx += emit_word_index1(&mut dst[dst_idx..dst_idx + len_idx], idx);
        } else if cur == CR {
            if !is_crlf {
                dst[dst_idx] = cur;
                dst_idx += 1;
            }
        } else {
            dst[dst_idx] = cur;
            dst_idx += 1;
        }
    }

    dst_idx
}

/// textCodec1.Forward (wrapped by TextCodec.Forward: bit4 cleared on success
/// for encodingType=1, which never affects these stats bytes). Returns Err
/// on decline; the DataType accompanies both paths.
pub fn forward(
    src: &[u8],
    dst: &mut [u8],
    block_size: u32,
    entropy_tpaqx: bool,
) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    let count = src.len();

    if count < TC1_MIN_BLOCK_SIZE {
        return Err(("block too small", DataType::Undefined));
    }

    if dst.len() < count {
        return Err(("Output buffer is too small", DataType::Undefined));
    }

    let mode = compute_text_stats(src, true);

    if mode & TC_MASK_NOT_TEXT != 0 {
        let dt = dt_from_mode(mode);
        return Err(("Input is not text, skip", dt));
    }

    let mut st = TextState1::new(count, block_size, entropy_tpaqx);
    st.is_crlf = mode & TC_MASK_CRLF != 0;

    let src_end = count;
    let dst_end = dst.len();
    let dst_end4 = dst_end.saturating_sub(4);
    let mut emit_anchor = 0usize;
    let mut words = st.static_dict_size;

    dst[0] = mode;
    let mut dst_idx = 1usize;
    let mut src_idx = 0usize;

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
            // At least 2 letters
            let length = (src_idx as i64 - delim_anchor - 1) as i32;

            if length <= TC_MAX_WORD_LENGTH {
                // Compute hashes: h1 word chars, h2 first char case flipped.
                let val = src[(delim_anchor + 1) as usize];
                let mut h1 = TC_HASH1;
                h1 = h1.wrapping_mul(TC_HASH1) ^ (val as i32).wrapping_mul(TC_HASH2);
                let mut h2 = TC_HASH1;
                h2 = h2.wrapping_mul(TC_HASH1) ^ ((val ^ 0x20) as i32).wrapping_mul(TC_HASH2);

                for i in ((delim_anchor + 2) as usize)..src_idx {
                    let h = (src[i] as i32).wrapping_mul(TC_HASH2);
                    h1 = h1.wrapping_mul(TC_HASH1) ^ h;
                    h2 = h2.wrapping_mul(TC_HASH1) ^ h;
                }

                let mut pe_idx = EMPTY;
                let pe1_idx = st.dict_map[(h1 & st.hash_mask) as usize];

                if pe1_idx != EMPTY
                    && st.dict_list[pe1_idx as usize].hash == h1
                    && (st.dict_list[pe1_idx as usize].data >> 24) == length
                {
                    pe_idx = pe1_idx;
                } else {
                    let pe2_idx = st.dict_map[(h2 & st.hash_mask) as usize];

                    if pe2_idx != EMPTY
                        && st.dict_list[pe2_idx as usize].hash == h2
                        && (st.dict_list[pe2_idx as usize].data >> 24) == length
                    {
                        pe_idx = pe2_idx;
                    }
                }

                // Check for hash collisions
                if pe_idx != EMPTY {
                    let pe = st.dict_list[pe_idx as usize];

                    if !same_words(
                        &pe.ptr[1..length as usize],
                        &src[(delim_anchor + 2) as usize..],
                    ) {
                        pe_idx = EMPTY;
                    }
                }

                if pe_idx == EMPTY {
                    // Word not found: replace entry if not in static dict.
                    if (length > 3 || (length == 3 && (words as i32) < TC_THRESHOLD2))
                        && pe1_idx == EMPTY
                    {
                        let pe = &mut st.dict_list[words];

                        if (pe.data & TC_MASK_LENGTH) as usize >= st.static_dict_size {
                            st.dict_map[(pe.hash & st.hash_mask) as usize] = EMPTY;
                            pe.ptr = &src[(delim_anchor + 1) as usize..];
                            pe.hash = h1;
                            pe.data = ((length as i32) << 24) | words as i32;
                        }

                        st.dict_map[(h1 & st.hash_mask) as usize] = words as u32;
                        words += 1;

                        if words >= st.dict_size && !st.expand_dictionary() {
                            words = st.static_dict_size;
                        }
                    }
                } else {
                    // Word found in the dictionary
                    let pe = st.dict_list[pe_idx as usize];

                    if emit_anchor as i64 != delim_anchor
                        || src[delim_anchor as usize] != b' '
                    {
                        dst_idx += emit_symbols1(
                            &src[emit_anchor..(delim_anchor + 1) as usize],
                            &mut dst[dst_idx..dst_end],
                            st.is_crlf,
                            st.static_dict_size,
                        );
                    }

                    if dst_idx >= dst_end4 {
                        err = Some("Text transform failed. Output buffer too small");
                        break;
                    }

                    if pe_idx == pe1_idx {
                        dst[dst_idx] = TC_ESCAPE_TOKEN1;
                    } else {
                        dst[dst_idx] = TC_ESCAPE_TOKEN2;
                    }

                    dst_idx += 1;
                    dst_idx += emit_word_index1(
                        &mut dst[dst_idx..dst_idx + 3],
                        pe.data & TC_MASK_LENGTH,
                    );
                    emit_anchor = (delim_anchor + 1) as usize + (pe.data >> 24) as usize;
                }
            }
        }

        // Reset delimiter position
        delim_anchor = src_idx as i64;
        src_idx += 1;
    }

    if err.is_none() {
        dst_idx += emit_symbols1(
            &src[emit_anchor..src_end],
            &mut dst[dst_idx..dst_end],
            st.is_crlf,
            st.static_dict_size,
        );

        if dst_idx > dst_end {
            err = Some("Text transform failed. Output buffer too small");
        }
    }

    if err.is_none() && src_idx != src_end {
        err = Some("Text transform failed. Source index mismatch");
    }

    if let Some(e) = err {
        return Err((e, DataType::Text));
    }

    Ok((src_idx, dst_idx, DataType::Text))
}

/// textCodec1.Inverse.
pub fn inverse(
    src: &[u8],
    dst: &mut [u8],
    block_size: u32,
    entropy_tpaqx: bool,
) -> Result<(usize, usize), &'static str> {
    let mut st = TextState1::new(dst.len(), block_size, entropy_tpaqx);

    let src_end = src.len();
    let dst_end = dst.len();
    let mut words = st.static_dict_size;
    let mut word_run = false;
    let mut err: Option<&'static str> = None;
    st.is_crlf = src[0] & TC_MASK_CRLF != 0;

    let mut src_idx = 1usize;
    let mut dst_idx = 0usize;
    let mut delim_anchor: i64 = 0;

    if is_text(src[src_idx]) {
        delim_anchor = src_idx as i64 - 1;
    }

    while src_idx < src_end && dst_idx < dst_end {
        let cur = src[src_idx];

        if is_text(cur) {
            dst[dst_idx] = cur;
            src_idx += 1;
            dst_idx += 1;
            continue;
        }

        if (src_idx as i64) > delim_anchor + 3 && is_delimiter(cur, &st.delim) {
            let length = (src_idx as i64 - delim_anchor - 1) as i32;

            if length <= TC_MAX_WORD_LENGTH {
                let mut h1 = TC_HASH1;
                h1 = h1.wrapping_mul(TC_HASH1)
                    ^ (src[(delim_anchor + 1) as usize] as i32).wrapping_mul(TC_HASH2);
                h1 = h1.wrapping_mul(TC_HASH1)
                    ^ (src[(delim_anchor + 2) as usize] as i32).wrapping_mul(TC_HASH2);

                for i in ((delim_anchor + 3) as usize)..src_idx {
                    h1 = h1.wrapping_mul(TC_HASH1)
                        ^ (src[i] as i32).wrapping_mul(TC_HASH2);
                }

                let pe1_idx = st.dict_map[(h1 & st.hash_mask) as usize];
                let mut found = false;

                if pe1_idx != EMPTY
                    && st.dict_list[pe1_idx as usize].hash == h1
                    && (st.dict_list[pe1_idx as usize].data >> 24) == length
                {
                    found = true;
                }

                if !found {
                    if (length > 3 || (words as i32) < TC_THRESHOLD2) && pe1_idx == EMPTY {
                        let pe = &mut st.dict_list[words];

                        if (pe.data & TC_MASK_LENGTH) as usize >= st.static_dict_size {
                            st.dict_map[(pe.hash & st.hash_mask) as usize] = EMPTY;
                            pe.ptr = &src[(delim_anchor + 1) as usize..];
                            pe.hash = h1;
                            pe.data = (length << 24) | words as i32;
                        }

                        st.dict_map[(h1 & st.hash_mask) as usize] = words as u32;
                        words += 1;

                        if words >= st.dict_size && !st.expand_dictionary() {
                            words = st.static_dict_size;
                        }
                    }
                }
            }
        }

        src_idx += 1;

        if cur == TC_ESCAPE_TOKEN1 || cur == TC_ESCAPE_TOKEN2 {
            // Word in dictionary => read word index (varint 5/7/7 bits)
            if src_idx >= src_end {
                err = Some("Text transform failed. Truncated word index");
                break;
            }

            let mut idx = src[src_idx] as i32;
            src_idx += 1;

            if idx >= 128 {
                idx &= 0x7F;

                if src_idx >= src_end {
                    err = Some("Text transform failed. Truncated word index");
                    break;
                }

                let mut idx2 = src[src_idx] as i32;
                src_idx += 1;

                if idx2 >= 0x80 {
                    idx = ((idx & 0x1F) << 7) | (idx2 & 0x7F);

                    if src_idx >= src_end {
                        err = Some("Text transform failed. Truncated word index");
                        break;
                    }

                    idx2 = src[src_idx] as i32;
                    src_idx += 1;
                }

                idx = (idx << 7) | idx2;

                if idx >= st.dict_size as i32 {
                    err = Some("Text transform failed. Invalid index");
                    break;
                }
            }

            let pe = st.dict_list[idx as usize];
            let length = ((pe.data >> 24) & 0xFF) as usize;

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

            if pe.ptr.is_empty() || dst_idx + length >= dst_end {
                err = Some("Text transform failed. Invalid input data");
                break;
            }

            dst[dst_idx..dst_idx + length].copy_from_slice(&pe.ptr[0..length]);

            if cur == TC_ESCAPE_TOKEN2 {
                dst[dst_idx] ^= 0x20;
            }

            dst_idx += length;
        } else {
            word_run = false;
            delim_anchor = src_idx as i64 - 1;

            if st.is_crlf && cur == LF {
                dst[dst_idx] = CR;
                dst_idx += 1;

                if dst_idx >= dst_end {
                    err = Some("Text transform failed. Invalid input data");
                    break;
                }
            }

            dst[dst_idx] = cur;
            dst_idx += 1;
        }
    }

    if err.is_none() && src_idx != src_end {
        err = Some("Text transform failed. Source index mismatch");
    }

    if let Some(e) = err {
        return Err(e);
    }

    Ok((src_idx, dst_idx))
}

fn dt_from_mode(mode: u8) -> DataType {
    match mode & TC_MASK_DT {
        0 => DataType::Undefined,
        1 => DataType::Text,
        4 => DataType::Numeric,
        5 => DataType::Base64,
        6 => DataType::Dna,
        7 => DataType::Bin,
        8 => DataType::Utf8,
        9 => DataType::SmallAlphabet,
        _ => DataType::Undefined,
    }
}