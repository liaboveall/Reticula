//! CRC-32, which pages and gzip use.

use alloc::boxed::Box;

/// The tables for four bytes at a time, one after another. They are computed
/// when they are first needed, which is cheaper than shipping them.
#[derive(Debug)]
pub(super) struct Crc32 {
    tables: Box<[[u32; 256]; 4]>,
}

impl Crc32 {
    pub(super) fn new() -> Self {
        let mut tables = Box::new([[0_u32; 256]; 4]);
        for byte in 0..256_usize {
            let mut value = u32::try_from(byte).unwrap_or(0);
            for table in &mut *tables {
                for _ in 0..8 {
                    let low = value & 1;
                    value = (value >> 1) ^ (0xedb8_8320 & low.wrapping_neg());
                }
                if let Some(slot) = table.get_mut(byte) {
                    *slot = value;
                }
            }
        }
        Self { tables }
    }

    /// The checksum of the bytes.
    pub(super) fn of(&self, bytes: &[u8]) -> u32 {
        let [t0, t1, t2, t3] = &*self.tables;
        let mut state = u32::MAX;
        let (chunks, rest) = bytes.as_chunks::<4>();
        for chunk in chunks {
            state ^= u32::from_le_bytes(*chunk);
            state = look_up(t3, state)
                ^ look_up(t2, state >> 8)
                ^ look_up(t1, state >> 16)
                ^ look_up(t0, state >> 24);
        }
        for byte in rest {
            state = (state >> 8) ^ look_up(t0, state ^ u32::from(*byte));
        }
        !state
    }
}

/// The entry for the lowest byte of a number.
fn look_up(table: &[u32; 256], number: u32) -> u32 {
    table.get((number & 0xff) as usize).copied().unwrap_or(0)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::vec::Vec;
    use reticula_core::rng::Rng;

    fn naive(bytes: &[u8]) -> u32 {
        let mut state = u32::MAX;
        for byte in bytes {
            state ^= u32::from(*byte);
            for _ in 0..8 {
                state = if state & 1 == 1 {
                    (state >> 1) ^ 0xedb8_8320
                } else {
                    state >> 1
                };
            }
        }
        !state
    }

    #[test]
    fn checksums() {
        let crc = Crc32::new();
        assert_eq!(crc.of(b""), 0);
        assert_eq!(crc.of(b"123456789"), 0xcbf4_3926);
        let mut rng = Rng::from_seed(2);
        for len in 0..200 {
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64().to_le_bytes()[0]).collect();
            assert_eq!(crc.of(&bytes), naive(&bytes));
        }
    }
}
