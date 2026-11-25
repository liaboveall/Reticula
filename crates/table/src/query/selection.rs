//! Sets of rows.

use alloc::rc::Rc;

use super::memory::Large;
use crate::bits;
use crate::layout::Directory;

/// Where a bit lies: its word, and the bit in it.
pub(super) fn place(index: usize) -> (usize, u32) {
    (index >> 6, u32::try_from(index & 63).unwrap_or(0))
}

pub(super) fn get(bits: &[u64], index: usize) -> bool {
    let (word, bit) = place(index);
    bits.get(word)
        .is_some_and(|word| word.wrapping_shr(bit) & 1 == 1)
}

pub(super) fn set(bits: &mut [u64], index: usize) {
    let (word, bit) = place(index);
    if let Some(word) = bits.get_mut(word) {
        *word |= 1_u64.wrapping_shl(bit);
    }
}

pub(super) fn clear(bits: &mut [u64], index: usize) {
    let (word, bit) = place(index);
    if let Some(word) = bits.get_mut(word) {
        *word &= !1_u64.wrapping_shl(bit);
    }
}

/// The first bit that is set, from one on and before another, or that other.
#[inline(never)]
pub(super) fn next_set(bits: &[u64], from: usize, end: usize) -> usize {
    let (mut at, bit) = place(from);
    let mut keep = u64::MAX.wrapping_shl(bit);
    while at.saturating_mul(64) < end {
        let word = bits.get(at).copied().unwrap_or(u64::MAX) & keep;
        if word != 0 {
            let found = at
                .saturating_mul(64)
                .saturating_add(word.trailing_zeros() as usize);
            return found.min(end);
        }
        keep = u64::MAX;
        at = at.saturating_add(1);
    }
    end
}

/// The bits of a word that stand for something, when there are `len` in all.
pub(super) fn tail(len: usize, word: usize) -> u64 {
    let rest = len.saturating_sub(word.saturating_mul(64)).min(64);
    bits::mask(u32::try_from(rest).unwrap_or(64))
}

pub(super) fn count(bits: &[u64]) -> usize {
    bits.iter()
        .map(|word| word.count_ones() as usize)
        .fold(0, usize::wrapping_add)
}

/// Some rows of a table, or all of them.
#[derive(Clone, Debug)]
pub(super) struct Selection {
    bits: Option<Rc<Large<u64>>>,
    count: u32,
}

impl Selection {
    pub(super) const fn all(rows: u32) -> Self {
        Self {
            bits: None,
            count: rows,
        }
    }

    pub(super) fn some(bits: Large<u64>, count: u32) -> Self {
        Self {
            bits: Some(Rc::new(bits)),
            count,
        }
    }

    /// Number of rows.
    pub(super) const fn len(&self) -> u32 {
        self.count
    }

    /// The bits of the rows, unless it is all of them.
    pub(super) fn bits(&self) -> Option<&[u64]> {
        self.bits.as_deref().map(|bits| &**bits)
    }

    /// The bits of the rows of a group, unless it is all of them.
    pub(super) fn of_group(&self, directory: &Directory, group: u32) -> Option<&[u64]> {
        self.bits().map(|bits| group_words(bits, directory, group))
    }

    /// How many rows of a group are in it.
    pub(super) fn count_in(&self, directory: &Directory, group: u32) -> usize {
        self.of_group(directory, group)
            .map_or_else(|| directory.rows_in(group) as usize, count)
    }

    /// Whether every row of this is also in another.
    pub(super) fn is_within(&self, other: &Self) -> bool {
        match (self.bits(), other.bits()) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => {
                self.count <= other.count && a.iter().zip(b).all(|(a, b)| a & !b == 0)
            }
        }
    }

    /// Whether they are the same rows.
    pub(super) fn is_same(&self, other: &Self) -> bool {
        self.count == other.count && self.is_within(other)
    }
}

/// The words of the rows of a group. Groups have a multiple of 64 rows.
pub(super) fn group_words<'a>(bits: &'a [u64], directory: &Directory, group: u32) -> &'a [u64] {
    let (from, to) = group_range(directory, group);
    bits.get(from..to.min(bits.len())).unwrap_or(&[])
}

/// From where to where the words of a group lie.
pub(super) fn group_range(directory: &Directory, group: u32) -> (usize, usize) {
    let per = (directory.group_rows >> 6) as usize;
    let from = (group as usize).saturating_mul(per);
    let words = (directory.rows_in(group) as usize).div_ceil(64);
    (from, from.saturating_add(words))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::vec;

    #[test]
    fn bits_are_set_found_and_cleared() {
        let mut bits = vec![0_u64; 3];
        for index in [0, 63, 64, 130, 191, 192] {
            set(&mut bits, index);
        }
        assert_eq!(count(&bits), 5);
        assert!(get(&bits, 63) && !get(&bits, 62) && !get(&bits, 192));
        assert_eq!(next_set(&bits, 0, 192), 0);
        assert_eq!(next_set(&bits, 1, 192), 63);
        assert_eq!(next_set(&bits, 64, 192), 64);
        assert_eq!(next_set(&bits, 65, 192), 130);
        assert_eq!(next_set(&bits, 131, 191), 191);
        assert_eq!(next_set(&bits, 131, 150), 150);
        clear(&mut bits, 63);
        assert_eq!(next_set(&bits, 1, 64), 64);
        assert_eq!(tail(130, 2), 3);
        assert_eq!(tail(130, 1), u64::MAX);
        assert_eq!(tail(128, 2), 0);
    }
}
