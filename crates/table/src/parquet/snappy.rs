//! The Snappy format.

/// A copy that lies at least this far back can be done in pieces of that
/// many bytes.
const PIECE: usize = 16;

/// Unpacks `input`, which has to fill `out` exactly.
pub(super) fn decompress(input: &[u8], out: &mut [u8]) -> Option<()> {
    let mut rest = input;
    let mut declared = 0_u64;
    for shift in (0..35).step_by(7) {
        let (byte, after) = rest.split_first()?;
        rest = after;
        declared |= u64::from(byte & 0x7f).wrapping_shl(shift);
        if byte & 0x80 == 0 {
            break;
        }
    }
    if u64::try_from(out.len()).ok() != Some(declared) {
        return None;
    }
    let mut at = 0_usize;
    while let Some((tag, after)) = rest.split_first() {
        rest = after;
        let code = usize::from(tag >> 2);
        match tag & 3 {
            0 => {
                let len = if code < 60 {
                    code.saturating_add(1)
                } else {
                    let (bytes, after) = rest.split_at_checked(code.saturating_sub(59))?;
                    rest = after;
                    little_endian(bytes).checked_add(1)?
                };
                let end = at.checked_add(len)?;
                // Most literals are short. A whole piece is copied for one when
                // there is room: what is too much is written over later.
                if len <= PIECE
                    && let Some(from) = rest.first_chunk::<PIECE>()
                    && let Some(to) = out.get_mut(at..).and_then(<[u8]>::first_chunk_mut::<PIECE>)
                {
                    *to = *from;
                    rest = rest.get(len..)?;
                } else {
                    let (from, after) = rest.split_at_checked(len)?;
                    rest = after;
                    out.get_mut(at..end)?.copy_from_slice(from);
                }
                at = end;
            }
            1 => {
                let (low, after) = rest.split_first()?;
                rest = after;
                let offset = (usize::from(tag >> 5) << 8) | usize::from(*low);
                at = copy(out, at, offset, (code & 7).saturating_add(4))?;
            }
            2 => {
                let (bytes, after) = rest.split_first_chunk::<2>()?;
                rest = after;
                let offset = usize::from(u16::from_le_bytes(*bytes));
                at = copy(out, at, offset, code.saturating_add(1))?;
            }
            _ => {
                let (bytes, after) = rest.split_first_chunk::<4>()?;
                rest = after;
                at = copy(out, at, little_endian(bytes), code.saturating_add(1))?;
            }
        }
    }
    (at == out.len()).then_some(())
}

/// A number of at most four bytes.
fn little_endian(bytes: &[u8]) -> usize {
    let mut all = [0_u8; 4];
    for (slot, byte) in all.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    usize::try_from(u32::from_le_bytes(all)).unwrap_or(usize::MAX)
}

/// Repeats `len` bytes, at most 64, from `offset` bytes before `at` on.
/// Gives where the copy ends.
fn copy(out: &mut [u8], at: usize, offset: usize, len: usize) -> Option<usize> {
    let from = at.checked_sub(offset)?;
    let end = at.checked_add(len)?;
    if offset == 0 || end > out.len() {
        return None;
    }
    if offset >= PIECE && at.saturating_add(64) <= out.len() {
        let mut done = 0_usize;
        while done < len {
            let piece = *out
                .get(from.wrapping_add(done)..)
                .and_then(<[u8]>::first_chunk::<PIECE>)?;
            *out.get_mut(at.wrapping_add(done)..)
                .and_then(<[u8]>::first_chunk_mut::<PIECE>)? = piece;
            done = done.wrapping_add(PIECE);
        }
    } else {
        // What was copied already repeats with the same period, so each piece
        // can be longer than the one before it.
        let mut done = 0_usize;
        while done < len {
            let most = offset.wrapping_add(done);
            let piece = most.min(len.wrapping_sub(done));
            out.copy_within(from..from.wrapping_add(piece), at.wrapping_add(done));
            done = done.wrapping_add(piece);
        }
    }
    Some(end)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "tests"
)]
pub(super) mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use reticula_core::rng::Rng;

    #[derive(Clone, Copy, Debug)]
    enum Element {
        Literal(usize),
        Copy { offset: usize, len: usize },
    }

    fn preamble(len: usize, out: &mut Vec<u8>) {
        let mut rest = len;
        while rest >= 0x80 {
            out.push((rest & 0x7f) as u8 | 0x80);
            rest >>= 7;
        }
        out.push(rest as u8);
    }

    fn literal(bytes: &[u8], out: &mut Vec<u8>) {
        let last = bytes.len() - 1;
        if last < 60 {
            out.push((last as u8) << 2);
        } else {
            let count = match last {
                0..=0xff => 1,
                0x100..=0xffff => 2,
                0x1_0000..=0xff_ffff => 3,
                _ => 4,
            };
            out.push((59 + count as u8) << 2);
            out.extend_from_slice(&(last as u32).to_le_bytes()[..count]);
        }
        out.extend_from_slice(bytes);
    }

    /// Nothing but literals.
    pub(in crate::parquet) fn compress(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        preamble(bytes.len(), &mut out);
        for piece in bytes.chunks(70_000) {
            literal(piece, &mut out);
        }
        out
    }

    /// Bytes made of those elements, and the naive result of unpacking them.
    fn build(elements: &[Element], rng: &mut Rng) -> (Vec<u8>, Vec<u8>) {
        let mut plain: Vec<u8> = Vec::new();
        let mut body = Vec::new();
        for element in elements {
            match *element {
                Element::Literal(len) => {
                    let bytes: Vec<u8> =
                        (0..len).map(|_| rng.next_u64().to_le_bytes()[0]).collect();
                    literal(&bytes, &mut body);
                    plain.extend_from_slice(&bytes);
                }
                Element::Copy { offset, len } => {
                    for _ in 0..len {
                        plain.push(plain[plain.len() - offset]);
                    }
                    let short = (4..=11).contains(&len) && offset < 2_048;
                    if short && rng.next_below(2) == 0 {
                        body.push(1 | ((len - 4) << 2) as u8 | ((offset >> 8) << 5) as u8);
                        body.push(offset as u8);
                    } else if offset < 65_536 && rng.next_below(4) > 0 {
                        body.push(2 | ((len - 1) << 2) as u8);
                        body.extend_from_slice(&(offset as u16).to_le_bytes());
                    } else {
                        body.push(3 | ((len - 1) << 2) as u8);
                        body.extend_from_slice(&(offset as u32).to_le_bytes());
                    }
                }
            }
        }
        let mut packed = Vec::new();
        preamble(plain.len(), &mut packed);
        packed.extend_from_slice(&body);
        (packed, plain)
    }

    fn elements(rng: &mut Rng) -> Vec<Element> {
        let mut out = Vec::new();
        let mut total = 0_usize;
        for _ in 0..rng.next_below(40) {
            let element = if total == 0 || rng.next_below(3) == 0 {
                Element::Literal(match rng.next_below(8) {
                    0 => 61 + rng.next_below(400) as usize,
                    1 => 60,
                    _ => 1 + rng.next_below(20) as usize,
                })
            } else {
                let offset = match rng.next_below(3) {
                    0 => 1 + rng.next_below(total.min(20) as u64),
                    _ => 1 + rng.next_below(total as u64),
                };
                Element::Copy {
                    offset: offset as usize,
                    len: 1 + rng.next_below(64) as usize,
                }
            };
            total += match element {
                Element::Literal(len) | Element::Copy { len, .. } => len,
            };
            out.push(element);
        }
        out
    }

    #[test]
    fn every_element_is_unpacked() {
        let mut rng = Rng::from_seed(3);
        for _ in 0..3_000 {
            let elements = elements(&mut rng);
            let (packed, plain) = build(&elements, &mut rng);
            let mut out = vec![0xee; plain.len()];
            assert_eq!(decompress(&packed, &mut out), Some(()), "{elements:?}");
            assert_eq!(out, plain, "{elements:?}");
            // A buffer of another length is refused.
            let mut longer = vec![0; plain.len() + 1];
            assert_eq!(decompress(&packed, &mut longer), None);
        }
        for len in [0, 1, 60, 61, 256, 257, 65_536, 65_537, 200_000] {
            let plain: Vec<u8> = (0..len).map(|index| index as u8).collect();
            let mut out = vec![0; len];
            assert_eq!(decompress(&compress(&plain), &mut out), Some(()));
            assert_eq!(out, plain);
        }
    }

    #[test]
    fn what_is_wrong_is_refused() {
        // A copy from before the start, one from nowhere, one past the end,
        // a literal that ends early, a length that does not end.
        for packed in [
            &[4, 0x00, b'a', 0x01 | (1 << 2), 2][..],
            &[4, 0x00, b'a', 0x01, 0],
            &[4, 0x00, b'a', 0x01 | (2 << 2), 1],
            &[4, 3 << 2, b'a'],
            &[0x80, 0x80],
            &[4, 0x00, b'a', 0x02],
            &[4, 0x00, b'a', 0x03, 1, 0],
            &[4, 63 << 2, 0xff, 0xff, 0xff, 0xff],
            &[4, 0x00, b'a'],
            &[],
        ] {
            let mut out = [0; 4];
            assert_eq!(decompress(packed, &mut out), None, "{packed:?}");
        }
    }

    #[test]
    fn damage_is_survived() {
        let mut rng = Rng::from_seed(4);
        for _ in 0..20_000 {
            let elements = elements(&mut rng);
            let (mut packed, plain) = build(&elements, &mut rng);
            for _ in 0..=rng.next_below(3) {
                let at = rng.next_below(packed.len() as u64) as usize;
                packed[at] = rng.next_u64().to_le_bytes()[0];
            }
            let mut out = vec![0; plain.len()];
            let _ = decompress(&packed, &mut out);
        }
    }
}
