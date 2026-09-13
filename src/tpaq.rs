// Port of kanzi-go's TPAQPredictor (entropy/TPAQPredictor.go) -- the
// context-mixing predictor used by levels 8 (TPAQ) and 9 (TPAQX, `extra`
// mode: a second SSE stage plus a 7th mixer input from an extra hashed
// context). Feeds binary_entropy.rs's generic BinaryEntropyEncoder/Decoder,
// exactly like CmPredictor does for level 7.
//
// Fidelity notes (see fpaq.rs/cm.rs for the general pattern):
// - All int32 arithmetic (hash mixing, context hashing, mixer dot product)
//   uses wrapping ops, matching Go's defined two's-complement wraparound.
// - Go's `cp0`..`cp6` are raw `*uint8` pointers into one of three fixed
//   backing slices (smallStatesMap0, smallStatesMap1, bigStatesMap); each
//   pointer only ever targets its OWN slice, never reassigned across
//   slices, so they're ported as plain `usize` indices into those slices
//   instead (no unsafe/raw pointers needed).
// - This project is bitstream-version-7-only, so `useLogicalShift` (Go:
//   `bsVersion >= 7`) is always true -- the non-logical-shift branches
//   (arithmetic shifts of `c4`/`c8`/hash mixing where Go picks logical
//   shifts for bsVersion>=7) are simply not implemented; only the logical
//   form is ported.
// - `ctx["blockSize"]` (the stream's configured block size) and
//   `ctx["size"]` (the current block's actual pre-entropy byte length,
//   i.e. this project's `pre_transform_length`/`post_transform_len`) drive
//   the states/mixers/hash/buffer table sizing -- both must match exactly
//   between encode and decode for a given block, which they do since both
//   sides read/write the same header field for the latter and the same
//   stream header field for the former.

use std::sync::OnceLock;

use crate::binary_entropy::Predictor;

const MAX_LENGTH: i32 = 88;
const BUFFER_SIZE: u32 = 64 * 1024 * 1024;
const HASH_SIZE: u32 = 16 * 1024 * 1024;
const MASK_80808080: i32 = -2139062144; // 0x80808080
const MASK_F0F0F000: i32 = -252645376; // 0xF0F0F000
const MASK_4F4FFFFF: i32 = 1330642943; // 0x4F4FFFFF
const MASK_FFFF0000: i32 = -65536; // 0xFFFF0000
const TPAQ_HASH: i32 = 0x7FEB352D;
const BEGIN_LEARN_RATE: i32 = 60 << 7;
const END_LEARN_RATE: i32 = 11 << 7;

include!("tpaq_tables.rs");

const INV_EXP: [i32; 33] = [
    0, 8, 22, 47, 88, 160, 283, 492, 848, 1451, 2459, 4117, 6766, 10819, 16608, 24127, 32768, 41409, 48928, 54717,
    58770, 61419, 63077, 64085, 64688, 65044, 65253, 65376, 65448, 65489, 65514, 65528, 65536,
];

struct SquashTables {
    squash: [i32; 4096],
    stretch: [i32; 4096],
}

fn tables() -> &'static SquashTables {
    static TABLES: OnceLock<SquashTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut squash = [0i32; 4096];

        for x in -2047..=2047i32 {
            let w = x & 127;
            let y = (x >> 7) + 16;
            squash[(x + 2047) as usize] = (INV_EXP[y as usize] * (128 - w) + INV_EXP[(y + 1) as usize] * w) >> 11;
        }

        squash[4095] = 4095;

        let mut stretch = [0i32; 4096];
        let mut pi: usize = 0;

        for x in -2047..=2047i32 {
            let i = squash_raw(x, &squash);

            while pi as i32 <= i {
                stretch[pi] = x;
                pi += 1;
            }
        }

        stretch[4095] = 2047;

        SquashTables { squash, stretch }
    })
}

fn squash_raw(d: i32, squash: &[i32; 4096]) -> i32 {
    if d >= 2048 {
        4095
    } else if d <= -2048 {
        0
    } else {
        squash[(d + 2047) as usize]
    }
}

#[inline]
fn idx_mask(x: i32, mask: i32) -> usize {
    (x as u32 & mask as u32) as usize
}

fn hash_tpaq_logical(x: i32, y: i32) -> i32 {
    let h = x.wrapping_mul(TPAQ_HASH) ^ y.wrapping_mul(TPAQ_HASH);
    (h >> 1) ^ (h >> 9) ^ ((x as u32 >> 2) as i32) ^ ((y as u32 >> 3) as i32) ^ TPAQ_HASH
}

fn create_context(ctx_id: i32, cx: i32) -> i32 {
    let c = (cx.wrapping_mul(987654323).wrapping_add(ctx_id)) as u32;
    (c.rotate_left(16).wrapping_mul(123456791) as i32).wrapping_add(ctx_id)
}

/// Port of Go's LogisticAdaptiveProbMap (the only APM variant TPAQ uses).
struct LogisticApm {
    data: Vec<u16>,
    rate: i32,
    index: usize,
    gradient: [i32; 2],
}

impl LogisticApm {
    fn new(n: usize, rate: u32) -> Self {
        let size = (n * 33).max(33);
        let mut data = vec![0u16; size];
        let squash = &tables().squash;

        for j in 0..=32i32 {
            data[j as usize] = (squash_raw((j - 16) << 7, squash) << 4) as u16;
        }

        if n > 1 {
            let first33: [u16; 33] = data[0..33].try_into().unwrap();

            for i in 1..n {
                data[i * 33..i * 33 + 33].copy_from_slice(&first33);
            }
        }

        LogisticApm { data, rate: rate as i32, index: 0, gradient: [0, 65528 + (1 << rate)] }
    }

    fn get(&mut self, bit: usize, pr: i32, ctx: i32) -> i32 {
        let rate = self.rate;
        let idx = self.index;
        let g = self.gradient[bit];
        let mut v1 = self.data[idx + 1] as i32;
        v1 += (g - v1) >> rate;
        self.data[idx + 1] = v1 as u16;
        let mut v0 = self.data[idx] as i32;
        v0 += (g - v0) >> rate;
        self.data[idx] = v0 as u16;

        let pr = tables().stretch[pr as usize];
        let idx = (((pr + 2048) >> 7) + (ctx << 5) + ctx) as usize;
        self.index = idx;
        let w = pr & 127;
        (self.data[idx + 1] as i32 * w + self.data[idx] as i32 * (128 - w)) >> 11
    }
}

/// Port of Go's TPAQMixer -- an 8-input single-layer neural mixer.
struct TpaqMixer {
    pr: i32,
    skew: i32,
    w: [i32; 8],
    p: [i32; 8],
    learn_rate: i32,
}

impl TpaqMixer {
    fn new() -> Self {
        TpaqMixer { pr: 2048, skew: 0, w: [32768; 8], p: [0; 8], learn_rate: BEGIN_LEARN_RATE }
    }

    fn update(&mut self, bit: i32) {
        let pr = self.pr;
        let lr = self.learn_rate;
        let err = (((bit << 12) - pr).wrapping_mul(lr)) >> 10;

        if err == 0 {
            return;
        }

        // Quickly decaying learn rate: `(END - lr) >> 31` is 0 while
        // lr > END (arithmetic shift of a non-negative i32) and -1 (all
        // ones) once lr <= END would make (END-lr) go negative... mirrors
        // Go's int32 arithmetic shift exactly.
        let lr = lr.wrapping_add((END_LEARN_RATE.wrapping_sub(lr)) >> 31);
        self.learn_rate = lr;
        self.skew = self.skew.wrapping_add(err);

        for i in 0..8 {
            self.w[i] = self.w[i].wrapping_add((self.p[i].wrapping_mul(err)) >> 12);
        }
    }

    fn get(&mut self, p: [i32; 8]) -> i32 {
        self.p = p;

        // Go computes this whole dot product in int32 (each `*` and `+` on
        // Go's defined-wraparound int32), then shifts and widens to `int`
        // only at the very end -- NOT widened to 64 bits first. Wrapping
        // add is commutative/associative mod 2^32 regardless of order, so
        // accumulating skew+products in any order still matches Go's
        // left-to-right int32 sum bit-for-bit, including when weights have
        // drifted far enough for w[i]*p[i] (or the running sum) to
        // overflow i32 -- which does happen over a long enough block, and
        // silently diverging from Go's wraparound there was compounding
        // into a measurable compression-ratio gap on TPAQ/TPAQX.
        let mut dot: i32 = self.skew;

        for i in 0..8 {
            dot = dot.wrapping_add(self.w[i].wrapping_mul(p[i]));
        }

        let d = dot.wrapping_add(65536) >> 17;
        let pr = squash_raw(d, &tables().squash);
        self.pr = pr;
        pr
    }
}

pub struct TpaqPredictor {
    pr: i32,
    c0: i32,
    c4: i32,
    c8: i32,
    bpos: u32,
    pos: i32,
    bin_count: i32,
    match_len: i32,
    match_pos: i32,
    match_val: i32,
    hash: i32,
    states_mask: i32,
    mixers_mask: i32,
    hash_mask: i32,
    buffer_mask: i32,
    sse0: LogisticApm,
    sse1: Option<LogisticApm>,
    mixers: Vec<TpaqMixer>,
    mixer_idx: usize,
    buffer: Vec<u8>,
    hashes: Vec<i32>,
    big_states_map: Vec<u8>,
    small_states_map0: Vec<u8>,
    small_states_map1: Vec<u8>,
    cp0: usize, // index into small_states_map0
    cp1: usize, // index into small_states_map1
    cp2: usize, // indices into big_states_map
    cp3: usize,
    cp4: usize,
    cp5: usize,
    cp6: usize,
    ctx0: i32,
    ctx1: i32,
    ctx2: i32,
    ctx3: i32,
    ctx4: i32,
    ctx5: i32,
    ctx6: i32,
    extra: bool,
}

fn log2_no_check(x: u32) -> u32 {
    31 - x.leading_zeros()
}

impl TpaqPredictor {
    /// `stream_block_size` is the stream's configured block size
    /// (ctx["blockSize"] in Go); `cur_block_len` is this specific block's
    /// actual pre-entropy byte length (ctx["size"]). `extra` selects TPAQX
    /// (true) vs TPAQ (false).
    pub fn new(stream_block_size: u32, cur_block_len: u32, extra: bool) -> Self {
        let rbsz = stream_block_size;
        let absz = cur_block_len;

        let mut states_size: u32 = match rbsz {
            s if s >= 64 * 1024 * 1024 => 1 << 28,
            s if s >= 16 * 1024 * 1024 => 1 << 27,
            s if s >= 4 * 1024 * 1024 => 1 << 26,
            s if s >= 1024 * 1024 => 1 << 24,
            _ => 1 << 22,
        };

        let mut mixers_size: u32 = match absz {
            s if s >= 32 * 1024 * 1024 => 1 << 16,
            s if s >= 16 * 1024 * 1024 => 1 << 15,
            s if s >= 8 * 1024 * 1024 => 1 << 14,
            s if s >= 4 * 1024 * 1024 => 1 << 13,
            s if s >= 1024 * 1024 => 1 << 11,
            _ => 1 << 8,
        };

        let mut buffer_size: u32 = BUFFER_SIZE.min(rbsz);
        let mxsz: u64 = if absz < (1 << 26) { absz as u64 * 16 } else { 1 << 30 };
        let mut hash_size: u32 = (HASH_SIZE as u64).min(mxsz) as u32;

        // bsVersion(7) > 6: normalize buffer/hash sizes to powers of two.
        buffer_size = 1 << log2_no_check(buffer_size);
        hash_size = 1 << log2_no_check(hash_size);

        let extra_mem: u32 = if extra { 1 } else { 0 };
        mixers_size <<= 2 * extra_mem;
        states_size <<= 2 * extra_mem;
        hash_size <<= 2 * extra_mem;

        // bsVersion(7) > 5: cap hash size for Java compatibility.
        hash_size = hash_size.min(1024 * 1024 * 1024);

        let mut mixers = Vec::with_capacity(mixers_size as usize);

        for _ in 0..mixers_size {
            mixers.push(TpaqMixer::new());
        }

        let big_states_map = vec![0u8; states_size as usize];
        let small_states_map0 = vec![0u8; 1 << 16];
        let small_states_map1 = vec![0u8; 1 << 24];
        let hashes = vec![0i32; hash_size as usize];
        let buffer = vec![0u8; buffer_size as usize];

        let sse0 = if extra { LogisticApm::new(256, 6) } else { LogisticApm::new(256, 7) };
        let sse1 = if extra { Some(LogisticApm::new(65536, 7)) } else { None };

        TpaqPredictor {
            pr: 2048,
            c0: 1,
            c4: 0,
            c8: 0,
            bpos: 8,
            pos: 0,
            bin_count: 0,
            match_len: 0,
            match_pos: 0,
            match_val: 0,
            hash: 0,
            states_mask: (states_size - 1) as i32,
            mixers_mask: (mixers_size - 1) as i32 & !1,
            hash_mask: (hash_size - 1) as i32,
            buffer_mask: (buffer_size - 1) as i32,
            sse0,
            sse1,
            mixers,
            mixer_idx: 0,
            buffer,
            hashes,
            big_states_map,
            small_states_map0,
            small_states_map1,
            cp0: 0,
            cp1: 0,
            cp2: 0,
            cp3: 0,
            cp4: 0,
            cp5: 0,
            cp6: 0,
            ctx0: 0,
            ctx1: 0,
            ctx2: 0,
            ctx3: 0,
            ctx4: 0,
            ctx5: 0,
            ctx6: 0,
            extra,
        }
    }

    fn find_match(&mut self) {
        if self.match_len > 0 {
            if self.match_len < MAX_LENGTH {
                self.match_len += 1;
            }

            self.match_pos = self.match_pos.wrapping_add(1);
        } else {
            self.match_pos = self.hashes[self.hash as usize];

            if self.match_pos != 0 && self.pos.wrapping_sub(self.match_pos) <= self.buffer_mask {
                let mut r = self.match_len + 2;
                let mut s = self.pos.wrapping_sub(r);
                let mut t = self.match_pos.wrapping_sub(r);

                while r <= MAX_LENGTH {
                    if self.buffer[idx_mask(s.wrapping_sub(1), self.buffer_mask)]
                        != self.buffer[idx_mask(t.wrapping_sub(1), self.buffer_mask)]
                    {
                        break;
                    }

                    if self.buffer[idx_mask(s, self.buffer_mask)] != self.buffer[idx_mask(t, self.buffer_mask)] {
                        break;
                    }

                    r += 2;
                    s = s.wrapping_sub(2);
                    t = t.wrapping_sub(2);
                }

                self.match_len = r - 2;
            }
        }
    }

    fn get_match_context_pred(&mut self) -> i32 {
        let m = self.match_val >> (self.bpos - 1);

        if self.c0 == m >> 1 {
            let p = MATCH_PRED[(self.match_len - 1) as usize];

            if m & 1 == 0 {
                -p
            } else {
                p
            }
        } else {
            self.match_len = 0;
            0
        }
    }
}

impl Predictor for TpaqPredictor {
    fn get(&mut self) -> i32 {
        self.pr
    }

    fn update(&mut self, bit: u8) {
        let y = bit as i32;
        self.mixers[self.mixer_idx].update(y);
        self.c0 = self.c0.wrapping_add(self.c0).wrapping_add(y);
        self.bpos -= 1;

        if self.bpos == 0 {
            self.buffer[idx_mask(self.pos, self.buffer_mask)] = self.c0 as u8;
            self.pos = self.pos.wrapping_add(1);
            self.c8 = (self.c8.wrapping_shl(8)) | ((self.c4 >> 24) & 0xFF);
            self.c4 = (self.c4.wrapping_shl(8)) | (self.c0 & 0xFF);
            self.hash = ((self.hash.wrapping_mul(TPAQ_HASH).wrapping_shl(4)).wrapping_add(self.c4)) & self.hash_mask;
            self.c0 = 1;
            self.bpos = 8;
            self.bin_count = self.bin_count.wrapping_add((self.c4 >> 7) & 1);

            self.mixer_idx = if self.match_len != 0 {
                idx_mask(self.c4, self.mixers_mask) + 1
            } else {
                idx_mask(self.c4, self.mixers_mask)
            };

            self.ctx0 = (self.c4 & 0xFF) << 8;
            self.ctx1 = (self.c4 & 0xFFFF) << 8;
            self.ctx2 = create_context(2, self.c4 & 0x00FF_FFFF);
            self.ctx3 = create_context(3, self.c4);

            if self.bin_count < self.pos >> 2 {
                // Mostly text or mixed.
                self.ctx4 = create_context(self.ctx1, self.c4 ^ (self.c8 & 0xFFFF));
                self.ctx5 = (self.c8 & MASK_F0F0F000) | (((self.c4 & MASK_F0F0F000) as u32 >> 4) as i32);

                if self.extra {
                    let h1 = if self.c4 & MASK_80808080 == 0 { self.c4 & MASK_4F4FFFFF } else { self.c4 & MASK_80808080 };
                    let h2 = if self.c8 & MASK_80808080 == 0 { self.c8 & MASK_4F4FFFFF } else { self.c8 & MASK_80808080 };
                    self.ctx6 = hash_tpaq_logical(h1.wrapping_shl(2), (h2 as u32 >> 2) as i32);
                }
            } else {
                // Mostly binary.
                self.ctx4 = create_context(TPAQ_HASH.wrapping_add(self.match_len), self.c4 ^ (self.c4 & 0x000F_FFFF));
                self.ctx5 = self.ctx0 | (self.c8.wrapping_shl(16));

                if self.extra {
                    self.ctx6 = hash_tpaq_logical(self.c4 & MASK_FFFF0000, (self.c8 as u32 >> 16) as i32);
                }
            }

            self.find_match();
            self.match_val = (self.buffer[idx_mask(self.match_pos, self.buffer_mask)] as i32) | 0x100;
            self.hashes[self.hash as usize] = self.pos;
        }

        let bit_idx = y as usize;
        let table = &STATE_TRANSITIONS[bit_idx];
        self.small_states_map0[self.cp0] = table[self.small_states_map0[self.cp0] as usize];
        self.small_states_map1[self.cp1] = table[self.small_states_map1[self.cp1] as usize];
        self.big_states_map[self.cp2] = table[self.big_states_map[self.cp2] as usize];
        self.big_states_map[self.cp3] = table[self.big_states_map[self.cp3] as usize];
        self.big_states_map[self.cp4] = table[self.big_states_map[self.cp4] as usize];
        self.big_states_map[self.cp5] = table[self.big_states_map[self.cp5] as usize];

        let c = self.c0;
        let c0 = self.c0;
        let ctx0_plus_c = self.ctx0.wrapping_add(c);
        let (ctx0, ctx1, ctx2, ctx3, ctx4, ctx5) = (self.ctx0, self.ctx1, self.ctx2, self.ctx3, self.ctx4, self.ctx5);
        let s_mask = self.states_mask;

        self.cp0 = (ctx0 + c) as usize;
        let p0 = STATE_MAP[self.small_states_map0[self.cp0] as usize];
        self.cp1 = (ctx1 + c) as usize;
        let p1 = STATE_MAP[self.small_states_map1[self.cp1] as usize];
        self.cp2 = idx_mask(ctx2.wrapping_add(c), s_mask);
        let p2 = STATE_MAP[self.big_states_map[self.cp2] as usize];
        self.cp3 = idx_mask(ctx3.wrapping_add(c), s_mask);
        let p3 = STATE_MAP[self.big_states_map[self.cp3] as usize];
        self.cp4 = idx_mask(ctx4.wrapping_add(c), s_mask);
        let p4 = STATE_MAP[self.big_states_map[self.cp4] as usize];
        self.cp5 = idx_mask(ctx5 ^ c, s_mask);
        let p5 = STATE_MAP[self.big_states_map[self.cp5] as usize];

        let p7 = if self.match_len != 0 { self.get_match_context_pred() } else { 0 };

        let mut p: i32;

        if !self.extra {
            p = self.mixers[self.mixer_idx].get([p0, p1, p2, p3, p4, p5, p7, p7]);

            if self.bin_count < (self.pos >> 3) {
                p = (3 * self.sse0.get(bit_idx, p, c0) + p) >> 2;
            }
        } else {
            self.big_states_map[self.cp6] = table[self.big_states_map[self.cp6] as usize];
            self.cp6 = idx_mask(self.ctx6.wrapping_add(c), self.states_mask);
            let p6 = STATE_MAP[self.big_states_map[self.cp6] as usize];

            p = self.mixers[self.mixer_idx].get([p0, p1, p2, p3, p4, p5, p6, p7]);

            let bin_count = self.bin_count;
            let pos = self.pos;
            let sse1 = self.sse1.as_mut().unwrap();

            if bin_count < (pos >> 3) {
                p = sse1.get(bit_idx, p, ctx0_plus_c);
            } else {
                if bin_count >= (pos >> 2) {
                    p = (3 * self.sse0.get(bit_idx, p, c0) + p) >> 2;
                }

                let sse1 = self.sse1.as_mut().unwrap();
                p = (3 * sse1.get(bit_idx, p, ctx0_plus_c) + p) >> 2;
            }
        }

        self.pr = p.wrapping_add(((p.wrapping_sub(2048)) as u32 >> 31) as i32);
    }
}
