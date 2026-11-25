//! Texts without regard to case, and as numbers that sort like them.

use alloc::vec::Vec;

use crate::reader::ReadError;

const NOT_TEXT: ReadError = ReadError::Corrupt("text that is not UTF-8");

/// The text in lower case, as `str::to_lowercase` has it. It is the text
/// itself when that is in lower case already, and is in `scratch` otherwise.
pub(super) fn fold<'a>(text: &'a [u8], scratch: &'a mut Vec<u8>) -> Result<&'a [u8], ReadError> {
    let mut wide = 0_u8;
    let mut upper = false;
    for byte in text {
        wide |= *byte;
        upper |= byte.wrapping_sub(b'A') < 26;
    }
    if wide < 0x80 {
        if !upper {
            return Ok(text);
        }
        scratch.clear();
        scratch.extend(text.iter().map(u8::to_ascii_lowercase));
        return Ok(scratch);
    }
    let text = core::str::from_utf8(text).map_err(|_| NOT_TEXT)?;
    scratch.clear();
    if text.contains('Σ') {
        // How it ends depends on what is around it.
        scratch.extend_from_slice(text.to_lowercase().as_bytes());
        return Ok(scratch);
    }
    let mut buffer = [0_u8; 4];
    for char in text.chars() {
        if char.is_ascii() {
            let mut small = [0_u8; 4];
            let encoded = char.to_ascii_lowercase().encode_utf8(&mut small);
            scratch.extend_from_slice(encoded.as_bytes());
        } else if is_caseless(char) {
            scratch.extend_from_slice(char.encode_utf8(&mut buffer).as_bytes());
        } else {
            for lower in char.to_lowercase() {
                scratch.extend_from_slice(lower.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    Ok(scratch)
}

/// Characters known to have no case without looking them up: most of those
/// of Chinese, Japanese and Korean.
const fn is_caseless(char: char) -> bool {
    matches!(char, '\u{2e80}'..='\u{a63f}' | '\u{ac00}'..='\u{d7ff}')
}

/// Eight bytes of a text from `depth` on as a number. Such numbers compare
/// like the texts, where those are equal before.
///
/// Every byte is one more than in the text, which is possible because UTF-8
/// has no bytes above 0xf4, and zero where the text has ended. So a text ends
/// before another that it begins, and equal numbers say whether the texts may
/// differ later, see [`goes_on`].
pub(super) fn chunk(text: &[u8], depth: usize) -> u64 {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let rest = text.get(depth..).unwrap_or(&[]);
    if let Some(bytes) = rest.first_chunk::<8>() {
        return u64::from_be_bytes(*bytes).wrapping_add(ONES);
    }
    let mut bytes = [0_u8; 8];
    for (slot, byte) in bytes.iter_mut().zip(rest) {
        *slot = byte.wrapping_add(1);
    }
    u64::from_be_bytes(bytes)
}

/// Whether the text may have more bytes than those in the number.
pub(super) const fn goes_on(chunk: u64) -> bool {
    chunk & 0xff != 0
}

/// Whether one text contains another.
pub(super) fn contains(text: &[u8], part: &[u8]) -> bool {
    let Some((first, rest)) = part.split_first() else {
        return true;
    };
    if text.len() < part.len() {
        return false;
    }
    if text.len() > 512 {
        // This takes time to begin, and then no more for text that nearly
        // contains it everywhere.
        if let (Ok(text), Ok(part)) = (core::str::from_utf8(text), core::str::from_utf8(part)) {
            return text.contains(part);
        }
    }
    let starts = text.len().saturating_sub(rest.len());
    let mut from = 0;
    while let Some(at) = text
        .get(from..starts)
        .and_then(|left| left.iter().position(|byte| byte == first))
    {
        from = from.saturating_add(at).saturating_add(1);
        if text.get(from..).is_some_and(|left| left.starts_with(rest)) {
            return true;
        }
    }
    false
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

    use alloc::string::String;

    use super::*;
    use reticula_core::rng::Rng;

    fn folded(text: &str) -> String {
        let mut scratch = Vec::new();
        String::from_utf8(fold(text.as_bytes(), &mut scratch).unwrap().to_vec()).unwrap()
    }

    /// Characters that are hard in one way or another.
    const HARD: &[char] = &[
        'a',
        'Z',
        '0',
        ' ',
        '\0',
        '\u{1}',
        'é',
        'É',
        'ß',
        'ẞ',
        'İ',
        'ı',
        'Σ',
        'σ',
        'ς',
        'Ⱥ',
        'ⱥ',
        'Å',
        '\u{212a}',
        'Ω',
        'ǅ',
        '漢',
        '字',
        'カ',
        '한',
        '\u{a640}',
        '\u{a63f}',
        '\u{d7ff}',
        '\u{e000}',
        '\u{10400}',
        '\u{1e900}',
        '\u{10ffff}',
        '.',
        '-',
        '\u{301}',
    ];

    fn text(rng: &mut Rng, longest: u64) -> String {
        (0..rng.next_below(longest + 1))
            .map(|_| HARD[rng.next_below(HARD.len() as u64) as usize])
            .collect()
    }

    #[test]
    fn every_character_is_folded_like_the_standard_library_does() {
        for char in '\0'..=char::MAX {
            let mut one = String::new();
            one.push(char);
            assert_eq!(folded(&one), one.to_lowercase(), "{char:?}");
        }
    }

    #[test]
    fn texts_are_folded_like_the_standard_library_does() {
        let mut rng = Rng::from_seed(1);
        for _ in 0..20_000 {
            let text = text(&mut rng, 12);
            assert_eq!(folded(&text), text.to_lowercase(), "{text:?}");
        }
        assert_eq!(folded("ΟΔΥΣΣΕΥΣ"), "οδυσσευς");
        assert!(fold(&[b'a', 0xff], &mut Vec::new()).is_err());
    }

    #[test]
    fn chunks_compare_like_texts() {
        let mut rng = Rng::from_seed(2);
        for _ in 0..20_000 {
            let shared = text(&mut rng, 8);
            let (mut a, mut b) = (shared.clone(), shared);
            a.push_str(&text(&mut rng, 4));
            b.push_str(&text(&mut rng, 4));
            let (mut depth, mut order) = (0, core::cmp::Ordering::Equal);
            loop {
                let (x, y) = (chunk(a.as_bytes(), depth), chunk(b.as_bytes(), depth));
                order = order.then(x.cmp(&y));
                if x != y || !goes_on(x) {
                    break;
                }
                depth += 8;
            }
            assert_eq!(order, a.as_bytes().cmp(b.as_bytes()), "{a:?} {b:?}");
        }
    }

    #[test]
    fn parts_are_found() {
        let mut rng = Rng::from_seed(3);
        for round in 0..20_000 {
            let longest = if round % 50 == 0 { 800 } else { 8 };
            let letters = |rng: &mut Rng, longest| -> Vec<u8> {
                (0..rng.next_below(longest))
                    .map(|_| b'a' + rng.next_below(2) as u8)
                    .collect()
            };
            let (text, part) = (letters(&mut rng, longest), letters(&mut rng, 6));
            let expected = part.is_empty() || text.windows(part.len()).any(|w| w == part);
            assert_eq!(contains(&text, &part), expected, "{text:?} {part:?}");
        }
    }
}
