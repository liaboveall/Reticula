//! Deterministic pseudo-random number generation.
//!
//! The kernel never uses a platform or library generator. Seeded sample data
//! and the deterministic simulator must produce the same stream on every
//! target and in every release, so the algorithm is part of the kernel's
//! contract: [SplitMix64] expands a seed, and xoshiro256** produces the stream.
//! Both use nothing but 64-bit integer arithmetic.
//!
//! [SplitMix64]: https://prng.di.unimi.it/splitmix64.c

/// SplitMix64, used to expand a 64-bit seed into generator state.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Creates a generator from a seed. Every seed is valid.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Returns the next value of the sequence.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix64(self.state)
    }
}

/// The SplitMix64 output function: a bijective mixer of 64-bit values.
#[must_use]
pub const fn mix64(value: u64) -> u64 {
    let mut z = value;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The kernel's general-purpose generator: xoshiro256**.
#[derive(Clone, Debug)]
pub struct Rng {
    s0: u64,
    s1: u64,
    s2: u64,
    s3: u64,
}

impl Rng {
    /// Creates a generator whose state is derived from `seed` with SplitMix64.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        let mut expand = SplitMix64::new(seed);
        Self::from_state([
            expand.next_u64(),
            expand.next_u64(),
            expand.next_u64(),
            expand.next_u64(),
        ])
    }

    /// Creates a generator from raw state.
    ///
    /// The all-zero state is a fixed point of the algorithm; it is replaced by
    /// the state that seed 0 expands to.
    #[must_use]
    pub fn from_state(state: [u64; 4]) -> Self {
        let [s0, s1, s2, s3] = state;
        if s0 | s1 | s2 | s3 == 0 {
            return Self::from_seed(0);
        }
        Self { s0, s1, s2, s3 }
    }

    /// Returns the next 64 uniformly distributed bits.
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s1 << 17;
        self.s2 ^= self.s0;
        self.s3 ^= self.s1;
        self.s1 ^= self.s2;
        self.s0 ^= self.s3;
        self.s2 ^= t;
        self.s3 = self.s3.rotate_left(45);
        result
    }

    /// Returns a uniformly distributed value in `[0, 1)` with 53 random bits.
    pub fn next_f64(&mut self) -> f64 {
        // 2^-53. The top 53 bits convert to f64 exactly.
        const SCALE: f64 = 1.0 / 9_007_199_254_740_992.0;
        (self.next_u64() >> 11) as f64 * SCALE
    }

    /// Returns a uniformly distributed value in `[0, bound)`, or 0 when `bound` is 0.
    ///
    /// Unbiased: uses Lemire's multiply-and-reject method.
    pub fn next_below(&mut self, bound: u64) -> u64 {
        // (2^64 - bound) mod bound, the number of low products to reject.
        let Some(threshold) = bound.wrapping_neg().checked_rem(bound) else {
            return 0;
        };
        loop {
            let product = u128::from(self.next_u64()).wrapping_mul(u128::from(bound));
            if low_half(product) >= threshold {
                return high_half(product);
            }
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "keeps exactly the low 64 bits"
)]
const fn low_half(value: u128) -> u64 {
    value as u64
}

const fn high_half(value: u128) -> u64 {
    (value >> 64) as u64
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{Rng, SplitMix64};

    #[test]
    fn splitmix64_matches_reference_vector() {
        let mut generator = SplitMix64::new(1_234_567);
        let expected = [
            6_457_827_717_110_365_317,
            3_203_168_211_198_807_973,
            9_817_491_932_198_370_423,
            4_593_380_528_125_082_431,
            16_408_922_859_458_223_821,
        ];
        for value in expected {
            assert_eq!(generator.next_u64(), value);
        }
    }

    #[test]
    fn xoshiro256_star_star_matches_reference_vector() {
        let mut generator = Rng::from_state([1, 2, 3, 4]);
        let expected = [
            11_520,
            0,
            1_509_978_240,
            1_215_971_899_390_074_240,
            1_216_172_134_540_287_360,
            607_988_272_756_665_600,
            16_172_922_978_634_559_625,
            8_476_171_486_693_032_832,
            10_595_114_339_597_558_777,
            2_904_607_092_377_533_576,
        ];
        for value in expected {
            assert_eq!(generator.next_u64(), value);
        }
    }

    #[test]
    fn all_zero_state_is_replaced() {
        let mut generator = Rng::from_state([0; 4]);
        let mut from_seed = Rng::from_seed(0);
        assert_ne!(generator.clone().next_u64(), 0);
        assert_eq!(generator.next_u64(), from_seed.next_u64());
    }

    #[test]
    fn next_f64_stays_in_unit_interval() {
        let mut generator = Rng::from_seed(7);
        for _ in 0..10_000 {
            let value = generator.next_f64();
            assert!((0.0..1.0).contains(&value));
        }
    }

    #[test]
    fn next_below_respects_bound() {
        let mut generator = Rng::from_seed(42);
        assert_eq!(generator.next_below(0), 0);
        assert_eq!(generator.next_below(1), 0);
        let mut seen = [false; 6];
        for _ in 0..1_000 {
            let value = generator.next_below(6);
            assert!(value < 6);
            seen[usize::try_from(value).unwrap()] = true;
        }
        assert!(seen.iter().all(|hit| *hit));
        assert!(generator.next_below(u64::MAX) < u64::MAX);
    }
}
