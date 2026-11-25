//! Hashes of texts, for finding them in memory. Nothing that is stored depends
//! on them.

const K0: u64 = 0x9e37_79b9_7f4a_7c15;
const K1: u64 = 0xc2b2_ae3d_27d4_eb4f;
const K2: u64 = 0x1656_67b1_9e37_79f9;

const fn step(state: u64, word: u64, by: u64) -> u64 {
    (state.rotate_left(27) ^ word).wrapping_mul(by)
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
}
