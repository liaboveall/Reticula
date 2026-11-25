//! Checksums of pages. Part of the file format: they never change.
//!
//! They are fast and not cryptographic: they notice damage, not malice. A file
//! as a whole is identified by its SHA-256.

const K0: u64 = 0x9e37_79b9_7f4a_7c15;
const K1: u64 = 0xc2b2_ae3d_27d4_eb4f;
const K2: u64 = 0x1656_67b1_9e37_79f9;
const K3: u64 = 0x27d4_eb2f_1656_67c5;

const fn step(state: u64, word: u64, by: u64) -> u64 {
    (state.rotate_left(27) ^ word).wrapping_mul(by)
}

fn words(bytes: &[u8]) -> impl Iterator<Item = u64> + '_ {
    let (chunks, rest) = bytes.as_chunks::<8>();
    let mut last = [0_u8; 8];
    for (slot, byte) in last.iter_mut().zip(rest) {
        *slot = *byte;
    }
    chunks
        .iter()
        .map(|chunk| u64::from_le_bytes(*chunk))
        .chain((!rest.is_empty()).then(|| u64::from_le_bytes(last)))
}

/// The checksum of one page.
#[must_use]
pub fn checksum(page: &[u8]) -> u32 {
    let mut lanes = [K0, K1, K2, K3];
    let (blocks, rest) = page.as_chunks::<32>();
    for block in blocks {
        let (parts, _) = block.as_chunks::<8>();
        for ((lane, by), part) in lanes.iter_mut().zip([K3, K2, K1, K0]).zip(parts) {
            *lane = step(*lane, u64::from_le_bytes(*part), by);
        }
    }
    let mut state = u64::try_from(page.len()).unwrap_or(0);
    for lane in lanes {
        state = step(state, lane, K0);
    }
    for word in words(rest) {
        state = step(state, word, K1);
    }
    state ^= state.wrapping_shr(29);
    state = state.wrapping_mul(K2);
    state ^= state.wrapping_shr(32);
    u32::try_from(state & 0xffff_ffff).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn checksums_are_part_of_the_format() {
        assert_eq!(checksum(&[]), 1_474_474_137);
        assert_eq!(
            checksum(b"a page of a table, not a whole one, in words"),
            2_064_827_197
        );
        assert_ne!(checksum(&[0]), checksum(&[]));
        assert_ne!(checksum(&[0; 8]), checksum(&[0; 16]));
        assert_ne!(checksum(&[0; 4096]), checksum(&[1; 4096]));
    }

    #[test]
    fn every_flipped_bit_of_a_page_is_noticed() {
        let mut page = [0x5a_u8; 4096];
        let sum = checksum(&page);
        for index in 0..page.len() {
            for bit in 0..8_u32 {
                let flip = 1_u8.wrapping_shl(bit);
                if let Some(byte) = page.get_mut(index) {
                    *byte ^= flip;
                }
                assert_ne!(checksum(&page), sum);
                if let Some(byte) = page.get_mut(index) {
                    *byte ^= flip;
                }
            }
        }
    }
}
