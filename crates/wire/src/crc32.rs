//! Pure `no_std` IEEE 802.3 CRC-32 (`0xEDB8_8320` reflected polynomial),
//! byte-for-byte compatible with standard ZIP/PNG/Ethernet and the JavaScript
//! framing verified in `S4` (`14-m1b.md` 1 & 5.5).

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "compile-time constant 256-entry CRC-32 table bounded by `while i < 256`"
)]
const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut i = 0_u16;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Incremental IEEE CRC-32 hasher.
#[derive(Clone, Copy, Debug)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// Creates a new IEEE CRC-32 state (`0xFFFF_FFFF` initial value).
    #[must_use]
    pub const fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Feeds `bytes` into the running checksum.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        for &b in bytes {
            let low = (crc & 0xFF) ^ u32::from(b);
            if let Some(&entry) = CRC32_TABLE.get(low as usize) {
                crc = (crc >> 8) ^ entry;
            }
        }
        self.state = crc;
    }

    /// Finalises and returns the 32-bit IEEE CRC-32 digest.
    #[must_use]
    pub const fn finish(self) -> u32 {
        !self.state
    }
}

/// Computes the IEEE CRC-32 checksum of `bytes` in one shot.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut h = Crc32::new();
    h.update(bytes);
    h.finish()
}

/// Computes the IEEE CRC-32 checksum over `a` followed by `b` without allocating.
#[must_use]
pub fn crc32_pair(a: &[u8], b: &[u8]) -> u32 {
    let mut h = Crc32::new();
    h.update(a);
    h.update(b);
    h.finish()
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{crc32, crc32_pair};

    #[test]
    fn ieee_check_vector_123456789_matches_cbf43926() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_pair(b"1234", b"56789"), 0xCBF4_3926);
    }
}
