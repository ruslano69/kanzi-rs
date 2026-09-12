// Port of kanzi-go's internal/Magic.go header sniffing (the subset needed
// here): GetMagicType over the first 4 bytes. Numeric values match Go
// exactly; FSDCodec gates on BMP/RIFF/PBM/PGM/PPM/NO_MAGIC.

pub const NO_MAGIC: u32 = 0;
pub const JPG_MAGIC: u32 = 0xFFD8_FFE0;
pub const GIF_MAGIC: u32 = 0x4749_4638;
pub const PDF_MAGIC: u32 = 0x2550_4446;
pub const ZIP_MAGIC: u32 = 0x504B_0304;
pub const LZMA_MAGIC: u32 = 0x377A_BCAF;
pub const PNG_MAGIC: u32 = 0x8950_4E47;
pub const ELF_MAGIC: u32 = 0x7F45_4C46;
pub const MAC_MAGIC32: u32 = 0xFEED_FACE;
pub const MAC_CIGAM32: u32 = 0xCEFA_EDFE;
pub const MAC_MAGIC64: u32 = 0xFEED_FACF;
pub const MAC_CIGAM64: u32 = 0xCFFA_EDFE;
pub const ZSTD_MAGIC: u32 = 0x28B5_2FFD;
pub const BROTLI_MAGIC: u32 = 0x81CF_B2CE;
pub const RIFF_MAGIC: u32 = 0x5249_4646;
pub const CAB_MAGIC: u32 = 0x4D53_4346;
pub const FLAC_MAGIC: u32 = 0x664C_6143;
pub const XZ_MAGIC: u32 = 0xFD37_7A58;
pub const RAR_MAGIC: u32 = 0x5261_7221;
pub const KNZ_MAGIC: u32 = 0x4B41_4E5A;

pub const BZIP2_MAGIC: u32 = 0x425A68;
pub const MP3_ID3_MAGIC: u32 = 0x494433;

pub const GZIP_MAGIC: u32 = 0x1F8B;
pub const BMP_MAGIC: u32 = 0x424D;
pub const WIN_MAGIC: u32 = 0x4D5A;
pub const PBM_MAGIC: u32 = 0x5034;
pub const PGM_MAGIC: u32 = 0x5035;
pub const PPM_MAGIC: u32 = 0x5036;

const KEYS32: [u32; 18] = [
    GIF_MAGIC,
    PDF_MAGIC,
    ZIP_MAGIC,
    LZMA_MAGIC,
    PNG_MAGIC,
    ELF_MAGIC,
    MAC_MAGIC32,
    MAC_CIGAM32,
    MAC_MAGIC64,
    MAC_CIGAM64,
    ZSTD_MAGIC,
    BROTLI_MAGIC,
    CAB_MAGIC,
    RIFF_MAGIC,
    FLAC_MAGIC,
    XZ_MAGIC,
    KNZ_MAGIC,
    RAR_MAGIC,
];

const KEYS16: [u32; 3] = [GZIP_MAGIC, BMP_MAGIC, WIN_MAGIC];

/// Checks the first bytes of the slice against a list of common magic values.
pub fn get_magic_type(src: &[u8]) -> u32 {
    if src.len() < 4 {
        return NO_MAGIC;
    }

    let key = u32::from_be_bytes(src[0..4].try_into().unwrap());

    if (key & !0x0F) == JPG_MAGIC {
        return key;
    }

    if (key >> 8) == BZIP2_MAGIC || (key >> 8) == MP3_ID3_MAGIC {
        return key >> 8;
    }

    for &k in &KEYS32 {
        if key == k {
            return key;
        }
    }

    let key16 = key >> 16;

    for &k in &KEYS16 {
        if key16 == k {
            return key16;
        }
    }

    if (key16 == PBM_MAGIC) || (key16 == PGM_MAGIC) || (key16 == PPM_MAGIC) {
        let subkey = (key >> 8) & 0xFF;

        if (subkey == 0x07) || (subkey == 0x0A) || (subkey == 0x0D) || (subkey == 0x20) {
            return key16;
        }
    }

    NO_MAGIC
}
