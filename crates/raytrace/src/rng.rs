// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! A tiny, dependency-free deterministic RNG. Seeded per (pixel, sample, global
//! seed) so a render is byte-for-byte reproducible regardless of thread
//! scheduling — the op-log replay invariant the workspace relies on.
//!
//! We hash the three seed components with SplitMix64, then stream with a
//! PCG-style xorshift. No global state, no atomics: each pixel/sample owns its
//! stream.

/// A per-sample deterministic random stream.
#[derive(Clone)]
pub struct Rng {
    state: u64,
}

/// SplitMix64 finaliser — mixes a counter into a well-distributed 64-bit value.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// Seed from three components (e.g. pixel index, sample index, global seed).
    /// Order-independent-of-scheduling: the same triple always yields the same
    /// stream.
    pub fn new(a: u64, b: u64, c: u64) -> Self {
        let mut s = splitmix64(a);
        s = splitmix64(s ^ b.wrapping_mul(0xD1B5_4A32_D192_ED03));
        s = splitmix64(s ^ c.wrapping_mul(0xCA5A_8265_4A0B_1D5B));
        // Avoid the all-zero state.
        Self { state: s | 1 }
    }

    /// Next raw 64-bit value (xorshift64*).
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform f64 in `[0, 1)`.
    #[inline]
    pub fn f01(&mut self) -> f64 {
        // 53-bit mantissa of precision.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(10, 20, 30);
        let mut b = Rng::new(10, 20, 30);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(10, 20, 30);
        let mut b = Rng::new(10, 20, 31);
        let mut diff = 0;
        for _ in 0..100 {
            if a.next_u64() != b.next_u64() {
                diff += 1;
            }
        }
        assert!(diff > 90, "streams should differ: {diff}/100");
    }

    #[test]
    fn f01_stays_in_range() {
        let mut r = Rng::new(1, 2, 3);
        for _ in 0..100_000 {
            let x = r.f01();
            assert!((0.0..1.0).contains(&x), "x={x}");
        }
    }

    #[test]
    fn f01_mean_is_about_half() {
        let mut r = Rng::new(42, 7, 99);
        let n = 200_000;
        let sum: f64 = (0..n).map(|_| r.f01()).sum();
        let mean = sum / n as f64;
        assert!((mean - 0.5).abs() < 0.01, "mean={mean}");
    }
}
