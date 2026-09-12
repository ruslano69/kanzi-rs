// Port of kanzi-go's XXHash32/XXHash64 (hash/XXHash32.go, hash/XXHash64.go)
// -- Yann Collet's xxHash, used for the optional per-block checksum
// (-x/-x32/-x64 in the real CLI, ckSize field in the stream header).
//
// Fidelity note -- a real deviation from the public xxHash64 spec, kept
// exactly as-is for wire compatibility: the merge step's four rotations
// (`(v1<<1)|(v1>>31)` etc.) use the 32-bit rotation complements (31, 25,
// 20, 14) instead of the correct 64-bit ones (63, 57, 52, 46). This is a
// genuine bug in kanzi-go's XXHash64 relative to the reference algorithm
// at github.com/Cyan4973/xxHash (every other rotate in the function -- the
// per-stripe round, the 8/4/1-byte tail mixing, the final avalanche -- is
// the correct 64-bit complement). Since this only needs to be a stable,
// deterministic function for corruption *detection*, not standards
// compliance, and the wire format is whatever kanzi actually emits, this
// port replicates the bug rather than "fixing" it. XXHash32 has no such
// bug (its merge step already uses the correct 32-bit complements).

const PRIME32_1: u32 = 2654435761;
const PRIME32_2: u32 = 2246822519;
const PRIME32_3: u32 = 3266489917;
const PRIME32_4: u32 = 668265263;
const PRIME32_5: u32 = 374761393;

const PRIME64_1: u64 = 0x9E3779B185EBCA87;
const PRIME64_2: u64 = 0xC2B2AE3D27D4EB4F;
const PRIME64_3: u64 = 0x165667B19E3779F9;
const PRIME64_4: u64 = 0x85EBCA77C2B2AE63;
const PRIME64_5: u64 = 0x27D4EB2F165667C5;

#[inline]
fn round32(acc: u32, val: u32) -> u32 {
    let acc = acc.wrapping_add(val.wrapping_mul(PRIME32_2));
    acc.rotate_left(13).wrapping_mul(PRIME32_1)
}

pub struct XxHash32 {
    seed: u32,
}

impl XxHash32 {
    pub fn new(seed: u32) -> Self {
        XxHash32 { seed }
    }

    pub fn hash(&self, data: &[u8]) -> u32 {
        let end = data.len();
        let mut n = 0usize;
        let mut h32: u32;

        if end >= 16 {
            let end16 = end - 16;
            let mut v1 = self.seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
            let mut v2 = self.seed.wrapping_add(PRIME32_2);
            let mut v3 = self.seed;
            let mut v4 = self.seed.wrapping_sub(PRIME32_1);

            while n <= end16 {
                let buf = &data[n..n + 16];
                v1 = round32(v1, u32::from_le_bytes(buf[0..4].try_into().unwrap()));
                v2 = round32(v2, u32::from_le_bytes(buf[4..8].try_into().unwrap()));
                v3 = round32(v3, u32::from_le_bytes(buf[8..12].try_into().unwrap()));
                v4 = round32(v4, u32::from_le_bytes(buf[12..16].try_into().unwrap()));
                n += 16;
            }

            h32 = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
        } else {
            h32 = self.seed.wrapping_add(PRIME32_5);
        }

        h32 = h32.wrapping_add(end as u32);

        while n + 4 <= end {
            h32 = h32.wrapping_add(u32::from_le_bytes(data[n..n + 4].try_into().unwrap()).wrapping_mul(PRIME32_3));
            h32 = h32.rotate_left(17).wrapping_mul(PRIME32_4);
            n += 4;
        }

        while n < end {
            h32 = h32.wrapping_add((data[n] as u32).wrapping_mul(PRIME32_5));
            h32 = h32.rotate_left(11).wrapping_mul(PRIME32_1);
            n += 1;
        }

        h32 ^= h32 >> 15;
        h32 = h32.wrapping_mul(PRIME32_2);
        h32 ^= h32 >> 13;
        h32 = h32.wrapping_mul(PRIME32_3);
        h32 ^ (h32 >> 16)
    }
}

#[inline]
fn round64(acc: u64, val: u64) -> u64 {
    let acc = acc.wrapping_add(val.wrapping_mul(PRIME64_2));
    acc.rotate_left(31).wrapping_mul(PRIME64_1)
}

#[inline]
fn merge_round64(acc: u64, val: u64) -> u64 {
    let acc = acc ^ round64(0, val);
    acc.wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

pub struct XxHash64 {
    seed: u64,
}

impl XxHash64 {
    pub fn new(seed: u64) -> Self {
        XxHash64 { seed }
    }

    pub fn hash(&self, data: &[u8]) -> u64 {
        let end = data.len();
        let mut n = 0usize;
        let mut h64: u64;

        if end >= 32 {
            let end32 = end - 32;
            let mut v1 = self.seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
            let mut v2 = self.seed.wrapping_add(PRIME64_2);
            let mut v3 = self.seed;
            let mut v4 = self.seed.wrapping_sub(PRIME64_1);

            while n <= end32 {
                let buf = &data[n..n + 32];
                v1 = round64(v1, u64::from_le_bytes(buf[0..8].try_into().unwrap()));
                v2 = round64(v2, u64::from_le_bytes(buf[8..16].try_into().unwrap()));
                v3 = round64(v3, u64::from_le_bytes(buf[16..24].try_into().unwrap()));
                v4 = round64(v4, u64::from_le_bytes(buf[24..32].try_into().unwrap()));
                n += 32;
            }

            // Deliberately NOT `rotate_left` -- see module doc: Go uses the
            // 32-bit rotation complements (31/25/20/14) here by mistake,
            // not the correct 64-bit ones (63/57/52/46), and that bug is
            // part of the wire format now.
            h64 = ((v1 << 1) | (v1 >> 31))
                .wrapping_add((v2 << 7) | (v2 >> 25))
                .wrapping_add((v3 << 12) | (v3 >> 20))
                .wrapping_add((v4 << 18) | (v4 >> 14));

            h64 = merge_round64(h64, v1);
            h64 = merge_round64(h64, v2);
            h64 = merge_round64(h64, v3);
            h64 = merge_round64(h64, v4);
        } else {
            h64 = self.seed.wrapping_add(PRIME64_5);
        }

        h64 = h64.wrapping_add(end as u64);

        while n + 8 <= end {
            h64 ^= round64(0, u64::from_le_bytes(data[n..n + 8].try_into().unwrap()));
            h64 = h64.rotate_left(27).wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4);
            n += 8;
        }

        while n + 4 <= end {
            h64 ^= (u32::from_le_bytes(data[n..n + 4].try_into().unwrap()) as u64).wrapping_mul(PRIME64_1);
            h64 = h64.rotate_left(23).wrapping_mul(PRIME64_2).wrapping_add(PRIME64_3);
            n += 4;
        }

        while n < end {
            h64 ^= (data[n] as u64).wrapping_mul(PRIME64_5);
            h64 = h64.rotate_left(11).wrapping_mul(PRIME64_1);
            n += 1;
        }

        h64 ^= h64 >> 33;
        h64 = h64.wrapping_mul(PRIME64_2);
        h64 ^= h64 >> 29;
        h64 = h64.wrapping_mul(PRIME64_3);
        h64 ^ (h64 >> 32)
    }
}
