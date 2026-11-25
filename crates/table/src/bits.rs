//! Numbers of a fixed width in bits, one after another.
//!
//! The first number is in the lowest bits of the first byte. A run ends with
//! padding up to a whole number of 64-bit words.

use alloc::vec::Vec;

/// How many bits the largest of the numbers takes.
#[must_use]
pub const fn width_of(largest: u64) -> u32 {
    u64::BITS.wrapping_sub(largest.leading_zeros())
}

/// Bytes that `count` numbers of `width` bits take.
#[must_use]
pub const fn packed_len(count: usize, width: u32) -> u64 {
    let bits = (count as u64).saturating_mul(width as u64);
    bits.div_ceil(64).saturating_mul(8)
}

/// The `width` lowest bits.
#[must_use]
pub const fn mask(width: u32) -> u64 {
    if width >= u64::BITS {
        u64::MAX
    } else {
        1_u64.wrapping_shl(width).wrapping_sub(1)
    }
}

/// Appends numbers, each of at most `width` bits.
pub fn pack<I: IntoIterator<Item = u64>>(numbers: I, width: u32, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let keep = mask(width);
    let mut word = 0_u64;
    // Always below 64.
    let mut filled = 0_u32;
    for number in numbers {
        let number = number & keep;
        word |= number.wrapping_shl(filled);
        let next = filled.wrapping_add(width);
        if next >= u64::BITS {
            out.extend_from_slice(&word.to_le_bytes());
            word = number
                .checked_shr(u64::BITS.wrapping_sub(filled))
                .unwrap_or(0);
            filled = next.wrapping_sub(u64::BITS);
        } else {
            filled = next;
        }
    }
    if filled > 0 {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Reads as many numbers as `out` has room for. Where `bytes` ends early,
/// the rest are zero.
pub fn unpack(bytes: &[u8], width: u32, out: &mut [u64]) {
    if width == 0 {
        out.fill(0);
        return;
    }
    let (chunks, _) = bytes.as_chunks::<8>();
    let mut words = chunks.iter().map(|chunk| u64::from_le_bytes(*chunk));
    if width >= u64::BITS {
        for slot in out {
            *slot = words.next().unwrap_or(0);
        }
        return;
    }
    let keep = mask(width);
    let mut rest = 0_u64;
    // How many bits of `rest` are left. Always below 64.
    let mut have = 0_u32;
    for slot in out {
        if have >= width {
            *slot = rest & keep;
            rest = rest.wrapping_shr(width);
            have = have.wrapping_sub(width);
        } else {
            let word = words.next().unwrap_or(0);
            *slot = (rest | word.wrapping_shl(have)) & keep;
            let used = width.wrapping_sub(have);
            rest = word.wrapping_shr(used);
            have = u64::BITS.wrapping_sub(used);
        }
    }
}

/// Where the number at `index` lies: the first byte of it, and by how many
/// bits it is shifted there. It spans at most nine bytes.
#[must_use]
pub const fn place(index: u64, width: u32) -> (u64, u32) {
    let bit = index.saturating_mul(width as u64);
    (bit >> 3, (bit & 7) as u32)
}

/// Takes a number out of the bytes it spans, see [`place`].
#[must_use]
pub fn extract(bytes: &[u8], shift: u32, width: u32) -> u64 {
    let mut all = [0_u8; 16];
    for (slot, byte) in all.iter_mut().zip(bytes.iter().take(9)) {
        *slot = *byte;
    }
    let shifted = u128::from_le_bytes(all).wrapping_shr(shift);
    u64::try_from(shifted & u128::from(mask(width))).unwrap_or(0)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use reticula_core::rng::Rng;

    #[test]
    fn every_width_comes_back() {
        let mut rng = Rng::from_seed(1);
        for width in 0..=64_u32 {
            for count in [0_usize, 1, 2, 7, 63, 64, 65, 200] {
                let numbers: Vec<u64> = (0..count).map(|_| rng.next_u64() & mask(width)).collect();
                let mut bytes = Vec::new();
                pack(numbers.iter().copied(), width, &mut bytes);
                assert_eq!(bytes.len() as u64, packed_len(count, width));
                let mut back = alloc::vec![u64::MAX; count];
                unpack(&bytes, width, &mut back);
                assert_eq!(back, numbers, "width {width}, {count} numbers");
                for (index, number) in numbers.iter().enumerate() {
                    let (at, shift) = place(index as u64, width);
                    let from = (at as usize).min(bytes.len());
                    let to = (from + 9).min(bytes.len());
                    assert_eq!(extract(&bytes[from..to], shift, width), *number);
                }
            }
        }
    }

    #[test]
    fn bytes_that_end_early_are_zeros() {
        let mut out = [7_u64; 4];
        unpack(&[0xff; 8], 48, &mut out);
        assert_eq!(out[0], mask(48));
        assert_eq!(out[3], 0);
        unpack(&[], 64, &mut out);
        assert_eq!(out, [0; 4]);
    }

    #[test]
    fn widths() {
        assert_eq!(width_of(0), 0);
        assert_eq!(width_of(1), 1);
        assert_eq!(width_of(255), 8);
        assert_eq!(width_of(256), 9);
        assert_eq!(width_of(u64::MAX), 64);
    }
}
