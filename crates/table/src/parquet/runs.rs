//! Numbers packed into bits: in runs, or as differences.
//!
//! The decoders hold places in a page, not the page itself, which is given to
//! every call. They read as many numbers as they are asked for and go on
//! where they stopped.

use crate::bits;

/// Most numbers of a block of differences.
const MAX_DELTA_BLOCK: u64 = 1 << 20;

/// Reads numbers of `width` bits from the bit `from` on, the lowest bits of a
/// byte first. Nothing when there are not that many.
pub(super) fn unpack(bytes: &[u8], from: u64, width: u32, out: &mut [u64]) -> Option<()> {
    if width == 0 {
        out.fill(0);
        return Some(());
    }
    let count = u64::try_from(out.len()).ok()?;
    let until = from.checked_add(count.checked_mul(u64::from(width))?)?;
    if until > u64::try_from(bytes.len()).ok()?.saturating_mul(8) || width > 64 {
        return None;
    }
    let bytes = bytes.get(usize::try_from(from >> 3).ok()?..)?;
    let (chunks, tail) = bytes.as_chunks::<8>();
    let mut last = [0_u8; 8];
    for (slot, byte) in last.iter_mut().zip(tail) {
        *slot = *byte;
    }
    let mut words = chunks
        .iter()
        .map(|chunk| u64::from_le_bytes(*chunk))
        .chain(core::iter::once(u64::from_le_bytes(last)));
    let keep = bits::mask(width);
    let skipped = u32::try_from(from & 7).unwrap_or(0);
    // How many bits of `rest` are left.
    let (mut rest, mut have) = if skipped == 0 {
        (0, 0)
    } else {
        (
            words.next().unwrap_or(0).wrapping_shr(skipped),
            64_u32.wrapping_sub(skipped),
        )
    };
    for slot in out {
        if have >= width {
            *slot = rest & keep;
            rest = rest.wrapping_shr(width);
            have = have.wrapping_sub(width);
        } else {
            let word = words.next().unwrap_or(0);
            *slot = (rest | word.wrapping_shl(have)) & keep;
            let used = width.wrapping_sub(have);
            rest = word.checked_shr(used).unwrap_or(0);
            have = 64_u32.wrapping_sub(used);
        }
    }
    Some(())
}

fn varint(bytes: &[u8], at: &mut usize, end: usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        if *at >= end {
            return None;
        }
        let byte = *bytes.get(*at)?;
        *at = at.wrapping_add(1);
        value |= u64::from(byte & 0x7f).wrapping_shl(shift);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn zigzag(value: u64) -> u64 {
    (value >> 1) ^ (value & 1).wrapping_neg()
}

impl Default for Run {
    fn default() -> Self {
        Self::Same { value: 0, left: 0 }
    }
}

#[derive(Clone, Copy, Debug)]
enum Run {
    /// One number again and again.
    Same { value: u64, left: usize },
    /// Packed numbers: the bit the next one starts at.
    Packed { bit: u64, left: usize },
}

/// Runs of one number, and of packed ones.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Hybrid {
    at: usize,
    end: usize,
    width: u32,
    run: Run,
}

impl Hybrid {
    /// Starts at `at`; the runs end before `end`. Numbers have `width` bits.
    pub(super) const fn new(at: usize, end: usize, width: u32) -> Self {
        Self {
            at,
            end,
            width,
            run: Run::Same { value: 0, left: 0 },
        }
    }

    /// Goes to the next run when the current one is at its end.
    fn load(&mut self, bytes: &[u8]) -> Option<()> {
        while let Run::Same { left: 0, .. } | Run::Packed { left: 0, .. } = self.run {
            let header = varint(bytes, &mut self.at, self.end)?;
            let count = header >> 1;
            if header & 1 == 0 {
                let len = self.width.div_ceil(8) as usize;
                let next = self.at.checked_add(len)?;
                if next > self.end {
                    return None;
                }
                let mut all = [0_u8; 8];
                for (slot, byte) in all.iter_mut().zip(bytes.get(self.at..next)?) {
                    *slot = *byte;
                }
                self.at = next;
                let value = u64::from_le_bytes(all);
                if value > bits::mask(self.width) {
                    return None;
                }
                self.run = Run::Same {
                    value,
                    left: usize::try_from(count).unwrap_or(usize::MAX),
                };
            } else {
                let start = u64::try_from(self.at).ok()?;
                let wanted = count.saturating_mul(u64::from(self.width));
                // Some writers leave out the end of the last run.
                let here = u64::try_from(self.end.checked_sub(self.at)?).ok()?;
                let len = wanted.min(here);
                let whole = if len == wanted {
                    count.saturating_mul(8)
                } else {
                    len.saturating_mul(8)
                        .checked_div(u64::from(self.width))
                        .unwrap_or(0)
                };
                if whole == 0 && count > 0 {
                    return None;
                }
                self.run = Run::Packed {
                    bit: start.saturating_mul(8),
                    left: usize::try_from(whole).unwrap_or(usize::MAX),
                };
                self.at = self.at.checked_add(usize::try_from(len).ok()?)?;
            }
        }
        Some(())
    }

    /// When one number comes at least `least` times, or `most` times, takes
    /// up to `most` of it. Gives the number and how many there were.
    pub(super) fn same(
        &mut self,
        bytes: &[u8],
        least: usize,
        most: usize,
    ) -> Option<Option<(u64, usize)>> {
        self.load(bytes)?;
        if let Run::Same { value, left } = &mut self.run
            && *left >= least.min(most)
        {
            let count = (*left).min(most);
            *left = left.wrapping_sub(count);
            return Some(Some((*value, count)));
        }
        Some(None)
    }

    /// Reads as many numbers as `out` has room for.
    pub(super) fn read(&mut self, bytes: &[u8], out: &mut [u64]) -> Option<()> {
        let mut rest = out;
        while !rest.is_empty() {
            self.load(bytes)?;
            let count = match &mut self.run {
                Run::Same { value, left } => {
                    let count = (*left).min(rest.len());
                    rest.get_mut(..count)?.fill(*value);
                    *left = left.wrapping_sub(count);
                    count
                }
                Run::Packed { bit, left } => {
                    let count = (*left).min(rest.len());
                    let end = bytes.get(..self.end)?;
                    unpack(end, *bit, self.width, rest.get_mut(..count)?)?;
                    let used = u64::try_from(count)
                        .ok()?
                        .saturating_mul(u64::from(self.width));
                    *bit = bit.saturating_add(used);
                    *left = left.wrapping_sub(count);
                    count
                }
            };
            rest = rest.get_mut(count..)?;
        }
        Some(())
    }
}

/// Whole numbers, each as its difference from the one before it.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Delta {
    at: usize,
    end: usize,
    /// Numbers of a small block, of which `parts` make a block.
    part_len: usize,
    parts: usize,
    /// Numbers that were not read yet.
    left: u64,
    /// The first number is still to come.
    first: bool,
    value: u64,
    smallest: u64,
    /// Where the widths of the parts are, and which part comes next.
    widths: usize,
    part: usize,
    /// The part that is being read: numbers left, the bit of the next one.
    part_left: usize,
    bit: u64,
    width: u32,
}

impl Delta {
    /// Starts at `at`; the numbers end before `end`. At most `most` are expected.
    pub(super) fn new(bytes: &[u8], at: usize, end: usize, most: usize) -> Option<Self> {
        let mut at = at;
        let block = varint(bytes, &mut at, end)?;
        let parts = varint(bytes, &mut at, end)?;
        let total = varint(bytes, &mut at, end)?;
        let value = zigzag(varint(bytes, &mut at, end)?);
        let part_len = block.checked_div(parts)?;
        if block > MAX_DELTA_BLOCK
            || block.checked_rem(parts)? != 0
            || part_len == 0
            || part_len.checked_rem(8)? != 0
            || total > u64::try_from(most).ok()?
        {
            return None;
        }
        let parts = usize::try_from(parts).ok()?;
        Some(Self {
            at,
            end,
            part_len: usize::try_from(part_len).ok()?,
            parts,
            left: total,
            first: true,
            value,
            smallest: 0,
            widths: at,
            part: parts,
            part_left: 0,
            bit: 0,
            width: 0,
        })
    }

    /// Goes to the next small block.
    fn load(&mut self, bytes: &[u8]) -> Option<()> {
        if self.part >= self.parts {
            self.smallest = zigzag(varint(bytes, &mut self.at, self.end)?);
            self.widths = self.at;
            self.at = self.at.checked_add(self.parts)?;
            self.part = 0;
        }
        let place = self.widths.checked_add(self.part)?;
        if place >= self.end {
            return None;
        }
        self.width = u32::from(*bytes.get(place)?);
        if self.width > 64 {
            return None;
        }
        self.bit = u64::try_from(self.at).ok()?.checked_mul(8)?;
        let len = (self.part_len >> 3).checked_mul(self.width as usize)?;
        self.at = self.at.checked_add(len)?;
        self.part = self.part.wrapping_add(1);
        self.part_left = self.part_len;
        Some(())
    }

    /// Reads as many numbers as `out` has room for.
    pub(super) fn read(&mut self, bytes: &[u8], out: &mut [u64]) -> Option<()> {
        let mut rest = out;
        if self.first && !rest.is_empty() {
            let (slot, after) = rest.split_first_mut()?;
            self.left = self.left.checked_sub(1)?;
            *slot = self.value;
            self.first = false;
            rest = after;
        }
        while !rest.is_empty() {
            if self.part_left == 0 {
                if self.left == 0 {
                    return None;
                }
                self.load(bytes)?;
            }
            let count = self.part_left.min(rest.len());
            self.left = self.left.checked_sub(u64::try_from(count).ok()?)?;
            let (slots, after) = rest.split_at_mut_checked(count)?;
            unpack(bytes.get(..self.end)?, self.bit, self.width, slots)?;
            let mut value = self.value;
            for slot in slots {
                value = value.wrapping_add(self.smallest).wrapping_add(*slot);
                *slot = value;
            }
            self.value = value;
            let used = u64::try_from(count)
                .ok()?
                .saturating_mul(u64::from(self.width));
            self.bit = self.bit.saturating_add(used);
            self.part_left = self.part_left.wrapping_sub(count);
            rest = after;
        }
        Some(())
    }

    /// Where the numbers end, when none was read yet.
    pub(super) fn after(mut self, bytes: &[u8]) -> Option<usize> {
        let mut left = self.left.saturating_sub(1);
        let part_len = u64::try_from(self.part_len).ok()?;
        while left > 0 {
            self.load(bytes)?;
            left = left.saturating_sub(part_len);
        }
        (self.at <= self.end).then_some(self.at)
    }
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

    fn naive(bytes: &[u8], from: u64, width: u32) -> u64 {
        let mut value = 0_u64;
        for bit in 0..u64::from(width) {
            let at = from + bit;
            let set = bytes[(at / 8) as usize] >> (at % 8) & 1;
            value |= u64::from(set) << bit;
        }
        value
    }

    pub(in crate::parquet) fn put_varint(value: u64, out: &mut Vec<u8>) {
        let mut rest = value;
        while rest >= 0x80 {
            out.push((rest & 0x7f) as u8 | 0x80);
            rest >>= 7;
        }
        out.push(rest as u8);
    }

    pub(in crate::parquet) fn put_zigzag(value: i64, out: &mut Vec<u8>) {
        put_varint(((value << 1) ^ (value >> 63)).cast_unsigned(), out);
    }

    /// Numbers one after another, the last byte filled up with zeros.
    pub(in crate::parquet) fn pack(numbers: &[u64], width: u32) -> Vec<u8> {
        let mut out = vec![0_u8; (numbers.len() * width as usize).div_ceil(8)];
        for (index, number) in numbers.iter().enumerate() {
            for bit in 0..width as usize {
                if number >> bit & 1 == 1 {
                    let at = index * width as usize + bit;
                    out[at / 8] |= 1 << (at % 8);
                }
            }
        }
        out
    }

    /// Runs of one number where there are many, packed ones elsewhere.
    pub(in crate::parquet) fn hybrid(
        numbers: &[u64],
        width: u32,
        rng: &mut Rng,
        out: &mut Vec<u8>,
    ) {
        let mut at = 0;
        while at < numbers.len() {
            let same = numbers[at..]
                .iter()
                .take_while(|number| **number == numbers[at])
                .count();
            let left = numbers.len() - at;
            if same >= 8 || rng.next_below(5) == 0 {
                let count = if rng.next_below(3) == 0 {
                    1 + rng.next_below(same as u64) as usize
                } else {
                    same
                };
                put_varint((count as u64) << 1, out);
                out.extend_from_slice(&numbers[at].to_le_bytes()[..width.div_ceil(8) as usize]);
                at += count;
            } else {
                let groups = 1 + rng.next_below(4) as usize;
                let groups = groups.min(left.div_ceil(8));
                let mut piece = numbers[at..numbers.len().min(at + groups * 8)].to_vec();
                piece.resize(groups * 8, 0);
                put_varint((groups as u64) << 1 | 1, out);
                out.extend(pack(&piece, width));
                at += groups * 8;
            }
        }
    }

    pub(in crate::parquet) fn delta(numbers: &[u64], block: usize, parts: usize) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(block as u64, &mut out);
        put_varint(parts as u64, &mut out);
        put_varint(numbers.len() as u64, &mut out);
        let first = numbers.first().copied().unwrap_or(0);
        put_zigzag(first.cast_signed(), &mut out);
        let differences: Vec<i64> = numbers
            .windows(2)
            .map(|pair| pair[1].wrapping_sub(pair[0]).cast_signed())
            .collect();
        for piece in differences.chunks(block) {
            let smallest = piece.iter().copied().min().unwrap_or(0);
            put_zigzag(smallest, &mut out);
            let part_len = block / parts;
            let small: Vec<Vec<u64>> = piece
                .chunks(part_len)
                .map(|part| {
                    let mut part: Vec<u64> = part
                        .iter()
                        .map(|value| value.wrapping_sub(smallest).cast_unsigned())
                        .collect();
                    part.resize(part_len, 0);
                    part
                })
                .collect();
            let widths: Vec<u32> = small
                .iter()
                .map(|part| part.iter().copied().map(bits::width_of).max().unwrap_or(0))
                .collect();
            for part in 0..parts {
                out.push(widths.get(part).copied().unwrap_or(0) as u8);
            }
            for (part, width) in small.iter().zip(&widths) {
                out.extend(pack(part, *width));
            }
        }
        out
    }

    #[test]
    fn packed_numbers_of_every_width() {
        let mut rng = Rng::from_seed(7);
        for width in 0..=64_u32 {
            for _ in 0..20 {
                let len = rng.next_below(90) as usize;
                let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64().to_le_bytes()[0]).collect();
                let from = rng.next_below(len as u64 * 8 + 1);
                let fits = (len as u64 * 8 - from)
                    .checked_div(u64::from(width))
                    .unwrap_or(30);
                let count = rng.next_below(fits + 1) as usize;
                let mut out = vec![u64::MAX; count];
                assert_eq!(unpack(&bytes, from, width, &mut out), Some(()));
                for (index, value) in out.iter().enumerate() {
                    let at = from + index as u64 * u64::from(width);
                    assert_eq!(*value, naive(&bytes, at, width), "width {width}");
                }
                if width > 0 {
                    let mut more = vec![0; fits as usize + 1];
                    assert_eq!(unpack(&bytes, from, width, &mut more), None);
                }
            }
        }
        assert_eq!(unpack(&[], u64::MAX, 64, &mut [0]), None);
        assert_eq!(unpack(&[0; 100], 0, 65, &mut [0]), None);
    }

    fn numbers(rng: &mut Rng, width: u32) -> Vec<u64> {
        let mut out = Vec::new();
        for _ in 0..rng.next_below(12) {
            let value = rng.next_u64() & bits::mask(width);
            if rng.next_below(2) == 0 {
                out.extend(vec![value; rng.next_below(300) as usize]);
            } else {
                out.extend((0..rng.next_below(70)).map(|_| rng.next_u64() & bits::mask(width)));
            }
        }
        out
    }

    #[test]
    fn runs_are_read_in_any_pieces() {
        let mut rng = Rng::from_seed(8);
        for round in 0..3_000_u32 {
            let width = round % 33;
            let numbers = numbers(&mut rng, width);
            let mut bytes = vec![0xaa; rng.next_below(5) as usize];
            let start = bytes.len();
            hybrid(&numbers, width, &mut rng, &mut bytes);
            let end = bytes.len();
            bytes.extend([0xaa; 9]);
            let mut decoder = Hybrid::new(start, end, width);
            let mut back = Vec::new();
            while back.len() < numbers.len() {
                let left = numbers.len() - back.len();
                let most = 1 + rng.next_below(left.min(200) as u64) as usize;
                if rng.next_below(2) == 0 {
                    if let Some((value, count)) = decoder.same(&bytes, 64, most).unwrap() {
                        assert!(count >= most.min(64) && count <= most);
                        back.extend(vec![value; count]);
                    }
                } else {
                    let mut out = vec![0; most];
                    decoder.read(&bytes, &mut out).unwrap();
                    back.extend(out);
                }
            }
            assert_eq!(back, numbers, "width {width}");
        }
    }

    #[test]
    fn runs_that_end_early_are_refused() {
        // A packed run without its end: what is there can be read.
        let bytes = [0x03, 0xff, 0x01];
        let mut decoder = Hybrid::new(0, 3, 3);
        let mut out = [0; 5];
        assert_eq!(decoder.read(&bytes, &mut out), Some(()));
        assert_eq!(out, [7, 7, 7, 0, 0]);
        assert_eq!(decoder.read(&bytes, &mut [0]), None);
        for bytes in [&[][..], &[0x80], &[0x04], &[0x03], &[0xff; 11], &[0x04, 9]] {
            let mut decoder = Hybrid::new(0, bytes.len(), 3);
            assert_eq!(decoder.read(bytes, &mut [0; 2]), None, "{bytes:?}");
        }
        // Runs of no length are left out.
        let bytes = [0x00, 0x05, 0x01, 0x04, 0x02];
        let mut out = [0; 2];
        assert_eq!(Hybrid::new(0, 5, 3).read(&bytes, &mut out), Some(()));
        assert_eq!(out, [2, 2]);
    }

    #[test]
    fn differences_are_read_in_any_pieces() {
        let mut rng = Rng::from_seed(9);
        for round in 0..2_000 {
            let width = rng.next_below(65) as u32;
            let count = [0, 1, 2, 31, 32, 33, 128, 129, 500][round % 9];
            let mut value = rng.next_u64();
            let numbers: Vec<u64> = (0..count)
                .map(|_| {
                    value = value.wrapping_add(rng.next_u64() & bits::mask(width));
                    value
                })
                .collect();
            let (block, parts) = [(128, 4), (256, 1), (8, 1), (512, 16)][round % 4];
            let mut bytes = vec![0x55; rng.next_below(4) as usize];
            let start = bytes.len();
            bytes.extend(delta(&numbers, block, parts));
            let end = bytes.len();
            bytes.extend([0x55; 3]);
            let mut decoder = Delta::new(&bytes, start, end, count).unwrap();
            assert_eq!(decoder.after(&bytes), Some(end));
            let mut back = Vec::new();
            while back.len() < numbers.len() {
                let left = (numbers.len() - back.len()).min(100);
                let mut out = vec![0; 1 + rng.next_below(left as u64) as usize];
                decoder.read(&bytes, &mut out).unwrap();
                back.extend(out);
            }
            assert_eq!(back, numbers);
            assert_eq!(decoder.read(&bytes, &mut [0]), None);
            if count > 0 {
                assert!(Delta::new(&bytes, start, end, count - 1).is_none());
            }
        }
    }

    #[test]
    fn differences_that_cannot_be_are_refused() {
        for header in [
            &[128, 1, 0, 2, 2][..], // No small blocks.
            &[128, 1, 3, 2, 2],     // Not a whole number of them.
            &[4, 1, 2, 2],          // Too few numbers in one.
            &[0, 1, 2, 2],          // None at all.
            &[0x80, 0x80, 0x80, 0x80, 0x01, 1, 2, 2],
            &[128, 1, 4],
            &[],
        ] {
            assert!(
                Delta::new(header, 0, header.len(), 100).is_none(),
                "{header:?}"
            );
        }
        // A width of more than 64 bits, and a block that is not there.
        let bytes = [128, 1, 4, 2, 2, 0, 65, 0, 0, 0];
        let mut decoder = Delta::new(&bytes, 0, bytes.len(), 100).unwrap();
        assert_eq!(decoder.after(&bytes), None);
        assert_eq!(decoder.read(&bytes, &mut [0; 2]), None);
        let bytes = [128, 1, 4, 2, 2];
        let mut decoder = Delta::new(&bytes, 0, bytes.len(), 100).unwrap();
        assert_eq!(decoder.read(&bytes, &mut [0; 2]), None);
    }

    #[test]
    fn any_bytes_are_survived() {
        let mut rng = Rng::from_seed(10);
        for _ in 0..30_000 {
            let len = rng.next_below(60) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| match rng.next_below(3) {
                    0 => rng.next_below(4) as u8,
                    _ => rng.next_u64().to_le_bytes()[0],
                })
                .collect();
            let end = rng.next_below(len as u64 + 1) as usize;
            let start = rng.next_below(end as u64 + 1) as usize;
            let mut out = [0; 50];
            let mut runs = Hybrid::new(start, end, rng.next_below(33) as u32);
            let _ = runs.same(&bytes, 4, 40);
            let _ = runs.read(&bytes, &mut out);
            if let Some(mut delta) = Delta::new(&bytes, start, end, 1 << 20) {
                let _ = delta.after(&bytes);
                let _ = delta.read(&bytes, &mut out);
            }
        }
    }
}
