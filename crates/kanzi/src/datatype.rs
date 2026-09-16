// Port of internal.DetectSimpleType (internal/Global.go) -- needed because
// AliasCodec (the DNA transform) sets ctx["dataType"] as a side effect even
// when it declines, and LZCodec.Forward reads that hint (DT_DNA -> longer
// min match, DT_SMALL_ALPHABET -> decline). Only the classification is
// ported; DT_MULTIMEDIA/DT_UTF8/DT_EXE/DT_BIN detection via magic bytes
// (internal.GetMagicType) is not implemented -- unreachable for the
// synthetic text/repeat/base64 test corpus (no file-format magic bytes).

// Numeric values match internal.DataType exactly (DT_UNDEFINED=0, DT_TEXT=1,
// DT_MULTIMEDIA=2, DT_EXE=3, DT_NUMERIC=4, DT_BASE64=5, DT_DNA=6, DT_BIN=7,
// DT_UTF8=8, DT_SMALL_ALPHABET=9) -- the low nibble of TextCodec's mode byte
// carries this code on the wire, so the mapping must be exact.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum DataType {
    Undefined = 0,
    Text = 1,
    Multimedia = 2,
    Exe = 3,
    Numeric = 4,
    Base64 = 5,
    Dna = 6,
    Bin = 7,
    Utf8 = 8,
    SmallAlphabet = 9,
}

const DNA_SYMBOLS: &[u8] = b"acgntuACGNTU"; // first 12 of "acgntuACGNTU\"" (Go's loop bound is 12)
const NUMERIC_SYMBOLS: &[u8] = b"0123456789+-*/=,.:; "; // all 20 bytes (note trailing space)
const BASE64_SYMBOLS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn detect_simple_type(count: usize, freqs: &[i32; 256]) -> DataType {
    if count == 0 {
        return DataType::Undefined;
    }

    let sum: i64 = DNA_SYMBOLS.iter().map(|&s| freqs[s as usize] as i64).sum();

    if sum > count as i64 - (count as i64) / 12 {
        return DataType::Dna;
    }

    let sum: i64 = NUMERIC_SYMBOLS
        .iter()
        .map(|&s| freqs[s as usize] as i64)
        .sum();

    if sum == count as i64 {
        return DataType::Numeric;
    }

    let sum: i64 = BASE64_SYMBOLS
        .iter()
        .map(|&s| freqs[s as usize] as i64)
        .sum();

    if sum + freqs[0x3D] as i64 == count as i64 {
        return DataType::Base64;
    }

    let distinct = freqs.iter().filter(|&&f| f > 0).count();

    if distinct == 256 {
        return DataType::Bin;
    }

    if distinct <= 4 {
        return DataType::SmallAlphabet;
    }

    DataType::Undefined
}

pub fn histogram(data: &[u8]) -> [i32; 256] {
    let mut freqs = [0i32; 256];

    for &b in data {
        freqs[b as usize] += 1;
    }

    freqs
}
