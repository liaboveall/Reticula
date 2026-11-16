//! A small, stable 64-bit digest.
//!
//! It exists to compare engine output across targets and builds, where the
//! only requirement is that equal input sequences give equal digests and that
//! differences are overwhelmingly likely to show. It is not cryptographic and
//! is not the state digest used by synchronisation.

use crate::rng::mix64;

/// Order-sensitive digest over a sequence of values.
#[derive(Clone, Debug)]
pub struct Digest64 {
    state: u64,
}

impl Digest64 {
    /// Creates an empty digest.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: 0x243F_6A88_85A3_08D3,
        }
    }

    /// Adds one 64-bit value.
    pub fn write_u64(&mut self, value: u64) {
        self.state = mix64(self.state ^ value).wrapping_add(0x9E37_79B9_7F4A_7C15);
    }

    /// Adds one float by its bit pattern.
    ///
    /// All NaNs count as the same value: the sign and payload of a NaN produced
    /// by arithmetic are not specified consistently across targets.
    pub fn write_f64(&mut self, value: f64) {
        const CANONICAL_NAN: u64 = 0x7FF8_0000_0000_0000;
        self.write_u64(if value.is_nan() {
            CANONICAL_NAN
        } else {
            value.to_bits()
        });
    }

    /// Adds a byte string. The length is included, so `"ab", "c"` and
    /// `"a", "bc"` give different digests.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        for chunk in bytes.chunks(8) {
            self.write_u64(
                chunk
                    .iter()
                    .fold(0, |word, byte| (word << 8) | u64::from(*byte)),
            );
        }
    }

    /// Returns the digest of everything written so far.
    #[must_use]
    pub const fn finish(&self) -> u64 {
        mix64(self.state)
    }
}

impl Default for Digest64 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::Digest64;

    fn digest_of(values: &[u64]) -> u64 {
        let mut digest = Digest64::new();
        for value in values {
            digest.write_u64(*value);
        }
        digest.finish()
    }

    #[test]
    fn order_matters() {
        assert_ne!(digest_of(&[1, 2]), digest_of(&[2, 1]));
        assert_ne!(digest_of(&[]), digest_of(&[0]));
        assert_eq!(digest_of(&[1, 2, 3]), digest_of(&[1, 2, 3]));
    }

    #[test]
    fn nan_payloads_are_canonicalised() {
        let mut quiet = Digest64::new();
        quiet.write_f64(f64::NAN);
        let mut other = Digest64::new();
        other.write_f64(f64::from_bits(0xFFF8_0000_0000_1234));
        assert_eq!(quiet.finish(), other.finish());

        let mut zero = Digest64::new();
        zero.write_f64(0.0);
        let mut negative_zero = Digest64::new();
        negative_zero.write_f64(-0.0);
        assert_ne!(zero.finish(), negative_zero.finish());
    }

    #[test]
    fn byte_strings_are_length_prefixed() {
        let mut left = Digest64::new();
        left.write_bytes(b"ab");
        left.write_bytes(b"c");
        let mut right = Digest64::new();
        right.write_bytes(b"a");
        right.write_bytes(b"bc");
        assert_ne!(left.finish(), right.finish());

        let mut long = Digest64::new();
        long.write_bytes(b"exactly sixteen!");
        let mut longer = Digest64::new();
        longer.write_bytes(b"exactly sixteen!\0");
        assert_ne!(long.finish(), longer.finish());
    }
}
