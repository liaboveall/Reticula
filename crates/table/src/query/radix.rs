//! Sorting by the digits of keys, one slice after another.
//!
//! What is sorted are numbers of items; the keys stay where they are. Every
//! pass goes through the items in the order they are in and puts each where
//! the next one with its digit belongs, so that equal items keep their order.

use alloc::vec::Vec;

const DIGIT_BITS: u32 = 11;
const BUCKETS: usize = 2_048;

/// A key made of digits.
pub(super) trait Key: Copy {
    fn digit(self, shift: u32, mask: u32) -> usize;
}

impl Key for u32 {
    fn digit(self, shift: u32, mask: u32) -> usize {
        (self.wrapping_shr(shift) & mask) as usize
    }
}

impl Key for u64 {
    fn digit(self, shift: u32, mask: u32) -> usize {
        usize::try_from(self.wrapping_shr(shift) & u64::from(mask)).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug)]
struct Digit {
    shift: u32,
    mask: u32,
}

/// How often each digit occurs at each place.
#[derive(Debug)]
pub(super) struct Counts {
    digits: Vec<Digit>,
    counts: Vec<u32>,
}

impl Counts {
    /// For keys of so many bits.
    pub(super) fn new(bits: u32) -> Self {
        let places = bits.div_ceil(DIGIT_BITS);
        let width = bits
            .checked_div(places)
            .map_or(0, |_| bits.div_ceil(places));
        let digits: Vec<Digit> = (0..places)
            .map(|place| Digit {
                shift: place.saturating_mul(width),
                mask: 1_u32.wrapping_shl(width).wrapping_sub(1),
            })
            .collect();
        Self {
            counts: alloc::vec![0; digits.len().saturating_mul(BUCKETS)],
            digits,
        }
    }

    pub(super) fn add<K: Key>(&mut self, keys: &[K]) {
        let (places, _) = self.counts.as_chunks_mut::<BUCKETS>();
        for (digit, counts) in self.digits.iter().zip(places) {
            for key in keys {
                if let Some(count) = counts.get_mut(key.digit(digit.shift, digit.mask)) {
                    *count = count.wrapping_add(1);
                }
            }
        }
    }

    /// Forgets what was counted, and leaves counts of nothing.
    pub(super) const fn take(&mut self) -> Self {
        let nothing = Self {
            digits: Vec::new(),
            counts: Vec::new(),
        };
        core::mem::replace(self, nothing)
    }
}

/// A sort that is under way.
#[derive(Debug)]
pub(super) struct Radix {
    start: usize,
    len: usize,
    counts: Counts,
    /// The place of the digit that is sorted by, and whether that began.
    place: usize,
    begun: bool,
    at: usize,
    /// Where the next item with each digit goes.
    next: Vec<u32>,
    in_other: bool,
}

impl Radix {
    /// Sorts `len` items from `start` on, whose digits are counted. One slice
    /// after another is moved, so nothing depends on how long slices are.
    pub(super) fn new(start: usize, len: usize, counts: Counts) -> Self {
        Self {
            start,
            len,
            counts,
            place: 0,
            begun: false,
            at: 0,
            next: alloc::vec![0; BUCKETS],
            in_other: false,
        }
    }

    /// Whether the items have ended up in the other array.
    pub(super) const fn in_other(&self) -> bool {
        self.in_other
    }

    /// Skips the places where all items have the same digit, and finds out
    /// where the items go by the next one.
    fn begin(&mut self) {
        let (places, _) = self.counts.counts.as_chunks::<BUCKETS>();
        while let Some(counts) = places.get(self.place) {
            let all = u32::try_from(self.len).unwrap_or(u32::MAX);
            if counts.iter().all(|count| *count != all) {
                let mut sum = u32::try_from(self.start).unwrap_or(u32::MAX);
                for (next, count) in self.next.iter_mut().zip(counts) {
                    *next = sum;
                    sum = sum.wrapping_add(*count);
                }
                self.begun = true;
                return;
            }
            self.place = self.place.saturating_add(1);
        }
    }

    /// Moves the next slice of items. Gives how many, and whether all is done.
    #[inline(never)]
    pub(super) fn step<K: Key>(
        &mut self,
        keys: &[K],
        items: &mut [u32],
        other: &mut [u32],
        slice: usize,
    ) -> (usize, bool) {
        if !self.begun {
            self.begin();
        }
        let Some(digit) = self.counts.digits.get(self.place).copied() else {
            return (0, true);
        };
        let from = self.start.saturating_add(self.at);
        let len = self.len.saturating_sub(self.at).min(slice);
        let to = from.saturating_add(len);
        let (source, target) = if self.in_other {
            (&*other, items)
        } else {
            (&*items, other)
        };
        if let Some(next) = self.next.first_chunk_mut::<BUCKETS>() {
            for item in source.get(from..to).unwrap_or(&[]) {
                let key = keys.get(*item as usize).copied();
                let bucket = key.map_or(0, |key| key.digit(digit.shift, digit.mask));
                if let Some(next) = next.get_mut(bucket) {
                    if let Some(slot) = target.get_mut(*next as usize) {
                        *slot = *item;
                    }
                    *next = next.wrapping_add(1);
                }
            }
        }
        self.at = self.at.saturating_add(len);
        if self.at >= self.len {
            self.at = 0;
            self.begun = false;
            self.in_other = !self.in_other;
            self.place = self.place.saturating_add(1);
        }
        (len, self.place >= self.counts.digits.len())
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use reticula_core::rng::Rng;

    fn sorted<K: Key + Ord>(keys: &[K], bits: u32, start: usize, len: usize) {
        let mut items: Vec<u32> = (0..keys.len() as u32).collect();
        let mut other = items.clone();
        let mut counts = Counts::new(bits);
        counts.add(&keys[start..start + len]);
        let mut radix = Radix::new(start, len, counts);
        let slice = [1, 7, 1_000][bits as usize % 3];
        while !radix.step(keys, &mut items, &mut other, slice).1 {}
        let result = if radix.in_other() { &other } else { &items };
        let mut expected: Vec<u32> = (start as u32..(start + len) as u32).collect();
        expected.sort_by_key(|item| keys[*item as usize]);
        assert_eq!(result[start..start + len], expected);
    }

    #[test]
    fn keys_of_every_width_are_sorted_stably() {
        let mut rng = Rng::from_seed(6);
        for bits in 0..=64_u32 {
            let mask = crate::bits::mask(bits);
            // Few different keys, so that there are equal ones.
            let some: Vec<u64> = (0..8).map(|_| rng.next_u64() & mask).collect();
            let wide: Vec<u64> = (0..300)
                .map(|_| match rng.next_below(2) {
                    0 => some[rng.next_below(8) as usize],
                    _ => rng.next_u64() & mask,
                })
                .collect();
            sorted(&wide, bits, 0, 300);
            sorted(&wide, bits, 17, 200);
            sorted(&wide, bits, 300, 0);
            if bits <= 32 {
                let narrow: Vec<u32> = wide.iter().map(|key| *key as u32).collect();
                sorted(&narrow, bits, 3, 290);
            }
        }
    }

    #[test]
    fn places_that_do_not_matter_are_skipped() {
        let keys: Vec<u64> = (0..1_000).map(|key| (key % 7) << 30).collect();
        let mut counts = Counts::new(64);
        counts.add(&keys);
        let mut items: Vec<u32> = (0..1_000).collect();
        let mut other = items.clone();
        let mut radix = Radix::new(0, 1_000, counts);
        let mut moved = 0;
        loop {
            let (work, done) = radix.step(&keys, &mut items, &mut other, 300);
            moved += work;
            if done {
                break;
            }
        }
        assert!(moved <= 2_000, "{moved}");
    }
}
