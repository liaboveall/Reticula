//! Checksums of pages, and hashes of texts.
//!
//! Both are fast and neither is cryptographic: they notice damage, not
//! malice. A file as a whole is identified by its SHA-256.

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

/// The checksum of one page. Part of the file format: it never changes.
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

/// A hash of a text, for finding it in a table in memory.
#[must_use]
pub fn of_text(text: &[u8]) -> u64 {
    let mut state = u64::try_from(text.len()).unwrap_or(0) ^ K1;
    let (chunks, rest) = text.as_chunks::<8>();
    for chunk in chunks {
        state = step(state, u64::from_le_bytes(*chunk), K0);
    }
    if !rest.is_empty() {
        // The last bytes, some of them for the second time: one read of eight
        // is faster than up to seven of one.
        let last = text
            .last_chunk::<8>()
            .map_or_else(|| few(text), |last| u64::from_le_bytes(*last));
        state = step(state, last, K0);
    }
    state ^= state.wrapping_shr(32);
    state.wrapping_mul(K2)
}

/// Fewer than eight bytes as one word.
fn few(bytes: &[u8]) -> u64 {
    if let (Some(first), Some(last)) = (bytes.first_chunk::<4>(), bytes.last_chunk::<4>()) {
        let last = u64::from(u32::from_le_bytes(*last));
        return u64::from(u32::from_le_bytes(*first)) | last.wrapping_shl(32);
    }
    let at = |index: usize| bytes.get(index).copied().map_or(0, u64::from);
    at(0) | at(1).wrapping_shl(8) | at(2).wrapping_shl(16)
}

/// Different texts of eight letters with the same [`of_text`] as far as its
/// upper half goes, which a dictionary cannot tell apart by it.
#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::arithmetic_side_effects, reason = "a test of small numbers")]
pub(crate) fn alike(count: usize) -> alloc::vec::Vec<alloc::string::String> {
    let inverse = |odd: u64| {
        (0..6).fold(odd, |x: u64, _| {
            x.wrapping_mul(2_u64.wrapping_sub(odd.wrapping_mul(x)))
        })
    };
    let (undo_last, undo_first) = (inverse(K2), inverse(K0));
    let start = (8_u64 ^ K1).rotate_left(27);
    let mut found = alloc::vec::Vec::new();
    for low in 0_u64.. {
        let mixed = (0x1234_5678_0000_0000 | low).wrapping_mul(undo_last);
        let state = mixed ^ mixed.wrapping_shr(32);
        let word = state.wrapping_mul(undo_first) ^ start;
        let bytes = word.to_le_bytes();
        if bytes.iter().all(|byte| (b' '..0x7f).contains(byte))
            && let Ok(text) = alloc::string::String::from_utf8(bytes.to_vec())
        {
            found.push(text);
            if found.len() == count {
                break;
            }
        }
        if low > 0xffff_fffe {
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn checksums_are_part_of_the_format() {
        assert_eq!(checksum(&[]), checksum(&[]));
        assert_ne!(checksum(&[0]), checksum(&[]));
        assert_ne!(checksum(&[0; 8]), checksum(&[0; 16]));
        assert_ne!(checksum(&[0; 4096]), checksum(&[1; 4096]));
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn texts_can_be_made_alike() {
        let texts = alike(50);
        let mut hashes: alloc::vec::Vec<u64> = texts
            .iter()
            .map(|text| of_text(text.as_bytes()).wrapping_shr(32))
            .collect();
        hashes.dedup();
        assert_eq!((texts.len(), hashes.len()), (50, 1));
    }

    #[test]
    fn texts_that_differ_in_one_byte_or_in_length_differ() {
        let text = [b'a'; 40];
        let mut seen = std::collections::BTreeSet::new();
        for len in 0..=text.len() {
            for at in 0..=len {
                let mut other = text;
                if let Some(byte) = other.get_mut(at) {
                    *byte = b'b';
                }
                let part = other.get(..len).unwrap_or(&[]);
                if at < len || at == 0 {
                    seen.insert(of_text(part));
                }
            }
        }
        // One for each place in each length, and the empty text.
        assert_eq!(seen.len(), 821);
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
