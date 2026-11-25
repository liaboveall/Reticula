//! Putting a few numbers of items in order.
//!
//! There is one sort and one heap for all of them, and what they compare is
//! asked through a pointer: each more would be as much code again. Items that
//! come in large numbers are sorted by their digits, see `radix`.

use alloc::vec::Vec;
use core::cmp::Ordering;

use super::memory::Large;

/// How two items compare.
pub(super) type Compare<'a> = &'a mut dyn FnMut(u32, u32) -> Ordering;

/// Items that are sorted by putting one after another in its place.
const FEW: usize = 12;

/// Sorts items stably.
#[inline(never)]
pub(super) fn sort_by(items: &mut [u32], scratch: &mut Vec<u32>, compare: Compare<'_>) {
    for part in items.chunks_mut(FEW) {
        insert_all(part, compare);
    }
    let mut width = FEW;
    while width < items.len() {
        for part in items.chunks_mut(width.saturating_mul(2)) {
            merge(part, width, scratch, compare);
        }
        width = width.saturating_mul(2);
    }
}

fn insert_all(items: &mut [u32], compare: Compare<'_>) {
    for end in 1..items.len() {
        let Some(item) = items.get(end).copied() else {
            return;
        };
        let mut at = end;
        while let Some(before) = at.checked_sub(1) {
            let Some(other) = items.get(before).copied() else {
                return;
            };
            if compare(other, item).is_le() {
                break;
            }
            put(items, at, other);
            at = before;
        }
        put(items, at, item);
    }
}

/// Merges the items before a place with those after it, both in order.
fn merge(items: &mut [u32], middle: usize, scratch: &mut Vec<u32>, compare: Compare<'_>) {
    let Some(left) = items.get(..middle) else {
        return;
    };
    scratch.clear();
    scratch.extend_from_slice(left);
    let (mut right, mut out) = (middle, 0_usize);
    for item in &*scratch {
        while let Some(other) = items.get(right).copied() {
            if compare(other, *item).is_ge() {
                break;
            }
            put(items, out, other);
            right = right.saturating_add(1);
            out = out.saturating_add(1);
        }
        put(items, out, *item);
        out = out.saturating_add(1);
    }
}

fn put(items: &mut [u32], at: usize, item: u32) {
    if let Some(slot) = items.get_mut(at) {
        *slot = item;
    }
}

/// Adds an item to the ones that come first, of which there are `limit` at
/// most, and drops the one that comes last when there are more. The items are
/// a heap with the last one on top. Gives whether one was dropped.
#[inline(never)]
pub(super) fn keep_first(
    heap: &mut Vec<u32>,
    limit: usize,
    item: u32,
    compare: Compare<'_>,
) -> bool {
    if heap.len() < limit {
        let mut at = heap.len();
        heap.push(item);
        while at > 0 {
            let parent = at.saturating_sub(1) >> 1;
            let Some(other) = heap.get(parent).copied() else {
                break;
            };
            if compare(other, item).is_ge() {
                break;
            }
            put(heap, at, other);
            at = parent;
        }
        put(heap, at, item);
        return false;
    }
    let Some(last) = heap.first().copied() else {
        return true;
    };
    if compare(item, last).is_ge() {
        return true;
    }
    let mut at = 0_usize;
    loop {
        let left = at.saturating_mul(2).saturating_add(1);
        let Some(mut child) = heap.get(left).copied() else {
            break;
        };
        let mut place = left;
        if let Some(right) = heap.get(left.saturating_add(1)).copied()
            && compare(right, child).is_gt()
        {
            child = right;
            place = left.saturating_add(1);
        }
        if compare(child, item).is_le() {
            break;
        }
        put(heap, at, child);
        at = place;
    }
    put(heap, at, item);
    true
}

/// Numbers in order, each once.
pub(super) fn in_order(numbers: &[u64]) -> Vec<u64> {
    let count = u32::try_from(numbers.len()).unwrap_or(u32::MAX);
    let mut places: Vec<u32> = (0..count).collect();
    let of = |place: u32| numbers.get(place as usize).copied().unwrap_or(0);
    sort_by(&mut places, &mut Vec::new(), &mut |a, b| of(a).cmp(&of(b)));
    let mut out: Vec<u64> = Vec::with_capacity(places.len());
    for place in places {
        if out.last() != Some(&of(place)) {
            out.push(of(place));
        }
    }
    out
}

/// How many slots are looked at for a thing. Where things are as many as there
/// may be, the longest runs of slots in use are far shorter, unless somebody
/// chose the things so that they are not.
const PROBES: usize = 1_024;

/// Places of things, found by a hash of them.
#[derive(Debug)]
pub(super) struct Index {
    /// One more than the place of a thing, or zero. At least half are zero.
    slots: Large<u32>,
    mask: usize,
}

impl Index {
    /// Number of slots for so many things.
    pub(super) const fn slots(things: usize) -> usize {
        things.saturating_mul(2).next_power_of_two()
    }

    /// An index whose slots are all zero. Their number is a power of two, or zero.
    pub(super) fn new(slots: Large<u32>) -> Self {
        Self {
            mask: slots.len().saturating_sub(1),
            slots,
        }
    }

    /// The place of a thing, or the slot for it when it is not there and there
    /// is one.
    pub(super) fn find(
        &self,
        hash: u64,
        is_it: &mut dyn FnMut(usize) -> bool,
    ) -> Result<usize, Option<usize>> {
        let mut slot = usize::try_from(hash & (u64::MAX >> 33)).unwrap_or(0) & self.mask;
        for _ in 0..PROBES {
            match self.slots.get(slot) {
                None => return Err(None),
                Some(0) => return Err(Some(slot)),
                Some(found) => {
                    let place = found.saturating_sub(1) as usize;
                    if is_it(place) {
                        return Ok(place);
                    }
                }
            }
            slot = slot.saturating_add(1) & self.mask;
        }
        Err(None)
    }

    /// Enters a thing at a slot that was found for it.
    pub(super) fn enter(&mut self, slot: usize, place: usize) {
        let place = u32::try_from(place).unwrap_or(u32::MAX);
        put(&mut self.slots, slot, place.saturating_add(1));
    }
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
    use crate::query::memory::{Large, Meter};
    use reticula_core::rng::Rng;

    #[test]
    fn items_are_sorted_stably() {
        let mut rng = Rng::from_seed(7);
        for len in (0..60).chain([100, 1_000, 5_000]) {
            let keys: Vec<u64> = (0..len).map(|_| rng.next_below(len / 2 + 1)).collect();
            let mut items: Vec<u32> = (0..len as u32).collect();
            let mut expected = items.clone();
            expected.sort_by_key(|item| keys[*item as usize]);
            sort_by(&mut items, &mut Vec::new(), &mut |a, b| {
                keys[a as usize].cmp(&keys[b as usize])
            });
            assert_eq!(items, expected);
            let mut each = keys.clone();
            each.sort_unstable();
            each.dedup();
            assert_eq!(in_order(&keys), each);
        }
    }

    #[test]
    fn the_first_ones_are_kept() {
        let mut rng = Rng::from_seed(8);
        for len in 0..80_u64 {
            for limit in [0, 1, 2, 7, 40, 100] {
                let keys: Vec<u64> = (0..len).map(|_| rng.next_below(1 << 40)).collect();
                let mut compare =
                    |a: u32, b: u32| keys[a as usize].cmp(&keys[b as usize]).then(a.cmp(&b));
                let mut heap = Vec::new();
                let mut dropped = false;
                for item in 0..len as u32 {
                    dropped |= keep_first(&mut heap, limit, item, &mut compare);
                }
                assert_eq!(dropped, len as usize > limit);
                let mut expected: Vec<u32> = (0..len as u32).collect();
                expected.sort_by(|a, b| compare(*a, *b));
                expected.truncate(limit);
                heap.sort_by(|a, b| compare(*a, *b));
                assert_eq!(heap, expected);
            }
        }
    }

    #[test]
    fn things_are_found_by_their_hash() {
        let meter = Meter::new();
        let mut slots = Large::empty(&meter);
        let things: Vec<u64> = (0..100).map(|thing| thing * 64).collect();
        slots.fill(Index::slots(100), 0, 1_000).unwrap();
        let mut index = Index::new(slots);
        for (place, thing) in things.iter().enumerate() {
            // Many of them have the same hash.
            let hash = thing % 320;
            let slot = index
                .find(hash, &mut |at| things[at] == *thing)
                .unwrap_err()
                .unwrap();
            index.enter(slot, place);
        }
        for (place, thing) in things.iter().enumerate() {
            let found = index.find(thing % 320, &mut |at| things[at] == *thing);
            assert_eq!(found, Ok(place));
        }
        assert!(index.find(5, &mut |_| false).is_err());
        // Nobody searches for ever.
        let mut slots = Large::empty(&meter);
        slots.fill(Index::slots(2_000), 0, 10_000).unwrap();
        let mut full = Index::new(slots);
        let mut entered = 0;
        while let Err(Some(slot)) = full.find(7, &mut |_| false) {
            full.enter(slot, entered);
            entered += 1;
        }
        assert_eq!(entered, PROBES);
        let none = Index::new(Large::empty(&meter));
        assert_eq!(none.find(5, &mut |_| true), Err(None));
    }
}
