// Port of kanzi-go's CMPredictor (entropy/CMPredictor.go) -- the Context
// Model predictor used by the CM entropy codec (level 7's entropy stage,
// via binary_entropy.rs's generic BinaryEntropyEncoder/Decoder).
//
// This project is bitstream-version-7-only, so `isBsVersion3` is always
// false (the `bsVersion < 4` branch in Go's constructor never triggers
// here) -- ported anyway for documentation parity with Go, but the V3
// branch is dead code in this project.

use crate::binary_entropy::Predictor;

const CM_FAST_RATE: i32 = 2;
const CM_MEDIUM_RATE: i32 = 4;
const CM_SLOW_RATE: i32 = 6;
const CM_PSCALE: i32 = 65536;
const CM_COUNTER1_STRIDE: usize = 257;
const CM_COUNTER2_STRIDE: usize = 17;

pub struct CmPredictor {
    c1: u8,
    c2: u8,
    ctx: i32,
    run_mask: i32,
    counter1: Vec<i32>,
    counter2: Vec<i32>,
    idx: usize,
}

impl CmPredictor {
    pub fn new() -> Self {
        let mut counter1 = vec![0i32; 256 * CM_COUNTER1_STRIDE];
        let mut counter2 = vec![0i32; 512 * CM_COUNTER2_STRIDE];

        for i in 0..256usize {
            let c1_idx = i * CM_COUNTER1_STRIDE;
            let c2_idx = (i + i) * CM_COUNTER2_STRIDE;

            for j in 0..=256usize {
                counter1[c1_idx + j] = CM_PSCALE >> 1;
            }

            for j in 0..16i32 {
                counter2[c2_idx + j as usize] = j << 12;
                counter2[c2_idx + CM_COUNTER2_STRIDE + j as usize] = j << 12;
            }

            // bsVersion is always 7 in this project -> isBsVersion3 is
            // always false -> always the 65535 branch (see module doc).
            counter2[c2_idx + 16] = 65535;
            counter2[c2_idx + CM_COUNTER2_STRIDE + 16] = 65535;
        }

        CmPredictor { c1: 0, c2: 0, ctx: 1, run_mask: 0, counter1, counter2, idx: 0 }
    }
}

impl Predictor for CmPredictor {
    fn get(&mut self) -> i32 {
        let ctx = self.ctx;
        let pc2_base = (ctx | self.run_mask) as usize;
        let pc2 = (pc2_base << 4) + pc2_base;
        let pc1_base = ctx as usize;
        let pc1 = (pc1_base << 8) + pc1_base;
        let c1tab = &self.counter1;
        let c2tab = &self.counter2;
        let p = (13i64 * (c1tab[pc1 + 256] as i64 + c1tab[pc1 + self.c1 as usize] as i64)
            + 6 * c1tab[pc1 + self.c2 as usize] as i64)
            >> 5;
        let idx = (p >> 12) as usize;
        // Go mutates `this.idx` inside Get() -- it's a read-side-effect
        // consumed by the *following* Update() call (must be captured from
        // THIS Get() call, before any Update() runs). `&mut self` here
        // makes that direct, same as Go.
        self.idx = idx;
        let x2 = c2tab[pc2 + idx + 1] as i64;
        let x1 = c2tab[pc2 + idx] as i64;
        ((p + p + 3 * (x1 + x2) + 64) >> 7) as i32
    }

    fn update(&mut self, bit: u8) {
        let ctx = self.ctx;
        let pc2_base = (ctx | self.run_mask) as usize;
        let pc2 = (pc2_base << 4) + pc2_base;
        let pc1_base = ctx as usize;
        let pc1 = (pc1_base << 8) + pc1_base;
        let c1 = pc1 + self.c1 as usize;
        let idx = self.idx;
        let mut new_ctx = ctx;

        if bit == 0 {
            self.counter1[pc1 + 256] -= self.counter1[pc1 + 256] >> CM_FAST_RATE;
            self.counter1[c1] -= self.counter1[c1] >> CM_MEDIUM_RATE;
            self.counter2[pc2 + idx] -= self.counter2[pc2 + idx] >> CM_SLOW_RATE;
            self.counter2[pc2 + idx + 1] -= self.counter2[pc2 + idx + 1] >> CM_SLOW_RATE;
            new_ctx = new_ctx.wrapping_add(new_ctx);
        } else {
            self.counter1[pc1 + 256] -= (self.counter1[pc1 + 256] - CM_PSCALE + 16) >> CM_FAST_RATE;
            self.counter1[c1] -= (self.counter1[c1] - CM_PSCALE + 16) >> CM_MEDIUM_RATE;
            self.counter2[pc2 + idx] -= (self.counter2[pc2 + idx] - CM_PSCALE + 16) >> CM_SLOW_RATE;
            self.counter2[pc2 + idx + 1] -= (self.counter2[pc2 + idx + 1] - CM_PSCALE + 16) >> CM_SLOW_RATE;
            new_ctx = new_ctx.wrapping_add(new_ctx).wrapping_add(1);
        }

        self.ctx = new_ctx;

        if new_ctx > 255 {
            self.c2 = self.c1;
            self.c1 = new_ctx as u8;
            self.ctx = 1;
            self.run_mask = if self.c1 == self.c2 { 0x100 } else { 0 };
        }
    }
}
