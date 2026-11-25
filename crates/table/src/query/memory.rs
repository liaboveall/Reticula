//! Counting the memory that queries take.
//!
//! Every buffer that grows with the table is counted, so that a limit can be
//! set and the same things happen at that limit that happen when a device has
//! no more memory. Buffers are reserved before they are used, and asking for
//! one that cannot be had is an error, not the end of the program.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ops::{Deref, DerefMut};

use super::QueryError;

#[derive(Debug)]
struct Counts {
    limit: Cell<usize>,
    used: Cell<usize>,
    outside: Cell<usize>,
}

/// What may be used, and what is.
#[derive(Clone, Debug)]
pub(super) struct Meter(Rc<Counts>);

impl Meter {
    pub(super) fn new() -> Self {
        Self(Rc::new(Counts {
            limit: Cell::new(usize::MAX),
            used: Cell::new(0),
            outside: Cell::new(0),
        }))
    }

    pub(super) fn set_limit(&self, bytes: usize) {
        self.0.limit.set(bytes);
    }

    /// Says how much is used by what does not count itself: the reader.
    pub(super) fn set_outside(&self, bytes: usize) {
        self.0.outside.set(bytes);
    }

    /// Bytes in use.
    pub(super) fn used(&self) -> usize {
        self.0.used.get().saturating_add(self.0.outside.get())
    }

    /// Whether that many bytes more are allowed.
    pub(super) fn fits(&self, bytes: usize) -> bool {
        self.used().saturating_add(bytes) <= self.0.limit.get()
    }

    /// Nothing, to be resized.
    pub(super) fn no_charge(&self) -> Charge {
        Charge {
            meter: self.clone(),
            bytes: 0,
        }
    }
}

/// A number of bytes that are counted as used while this exists.
#[derive(Debug)]
pub(super) struct Charge {
    meter: Meter,
    bytes: usize,
}

impl Charge {
    pub(super) const fn bytes(&self) -> usize {
        self.bytes
    }

    #[inline(never)]
    pub(super) fn resize(&mut self, bytes: usize) -> Result<(), QueryError> {
        let used = &self.meter.0.used;
        if bytes > self.bytes && !self.meter.fits(bytes.saturating_sub(self.bytes)) {
            return Err(QueryError::OutOfMemory);
        }
        used.set(used.get().saturating_sub(self.bytes).saturating_add(bytes));
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for Charge {
    #[inline(never)]
    fn drop(&mut self) {
        let used = &self.meter.0.used;
        used.set(used.get().saturating_sub(self.bytes));
    }
}

/// A buffer that grows with the table. It is counted.
#[derive(Debug)]
pub(super) struct Large<T> {
    items: Vec<T>,
    charge: Charge,
}

impl<T: Copy> Large<T> {
    /// An empty buffer.
    pub(super) fn empty(meter: &Meter) -> Self {
        Self {
            items: Vec::new(),
            charge: meter.no_charge(),
        }
    }

    /// Makes room for so many items more.
    #[inline(never)]
    pub(super) fn reserve(&mut self, more: usize) -> Result<(), QueryError> {
        let wanted = self
            .items
            .len()
            .checked_add(more)
            .ok_or(QueryError::OutOfMemory)?;
        if wanted <= self.items.capacity() {
            return Ok(());
        }
        let bytes = wanted
            .checked_mul(size_of::<T>())
            .ok_or(QueryError::OutOfMemory)?;
        let before = self.charge.bytes();
        self.charge.resize(bytes)?;
        if self.items.try_reserve_exact(more).is_err() {
            self.charge.resize(before)?;
            return Err(QueryError::OutOfMemory);
        }
        Ok(())
    }

    /// Makes room for so many items if that can be had, so that they need not
    /// be moved when there are more than there was room for. It may be more
    /// than there will be.
    pub(super) fn expect(&mut self, items: u64) {
        if let Ok(items) = usize::try_from(items) {
            let _ = self.reserve(items);
        }
    }

    /// Makes room for so many items more, and for as many again as there are,
    /// so that adding items one by one does not move them all every time.
    #[inline(never)]
    fn reserve_amortized(&mut self, more: usize) -> Result<(), QueryError> {
        let room = self.items.capacity().saturating_sub(self.items.len());
        if room >= more {
            return Ok(());
        }
        match self.reserve(more.max(self.items.len())) {
            Ok(()) => Ok(()),
            Err(_) => self.reserve(more),
        }
    }

    /// Adds an item. There is room for it unless it was not asked for.
    #[inline(never)]
    pub(super) fn push(&mut self, item: T) -> Result<(), QueryError> {
        if self.items.len() >= self.items.capacity() {
            self.reserve_amortized(1)?;
        }
        self.items.push(item);
        Ok(())
    }

    /// Adds items.
    pub(super) fn extend_from_slice(&mut self, items: &[T]) -> Result<(), QueryError> {
        self.reserve_amortized(items.len())?;
        self.items.extend_from_slice(items);
        Ok(())
    }

    /// Adds copies of an item until there are `len`, but no more than
    /// `at_most` of them. Gives whether there are.
    #[inline(never)]
    pub(super) fn fill(&mut self, len: usize, item: T, at_most: usize) -> Result<bool, QueryError> {
        let to = self.items.len().saturating_add(at_most).min(len);
        if to > self.items.len() {
            self.reserve(len.saturating_sub(self.items.len()))?;
            self.items.resize(to, item);
        }
        Ok(self.items.len() >= len)
    }

    /// Forgets the items. The room stays.
    pub(super) fn clear(&mut self) {
        self.items.clear();
    }

    /// Forgets all items but the first ones. The room stays.
    pub(super) fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    /// Takes the buffer away, and leaves one that takes no memory.
    pub(super) fn take(&mut self) -> Self {
        let empty = Self {
            items: Vec::new(),
            charge: self.charge.meter.no_charge(),
        };
        core::mem::replace(self, empty)
    }
}

impl Large<u32> {
    /// Adds the numbers that come next until there are `len`, but no more than
    /// `at_most` of them. Gives whether there are.
    pub(super) fn fill_counting(&mut self, len: usize, at_most: usize) -> Result<bool, QueryError> {
        let from = self.items.len();
        let to = from.saturating_add(at_most).min(len);
        if to > from {
            self.reserve(len.saturating_sub(from))?;
            let first = u32::try_from(from).map_err(|_| QueryError::OutOfMemory)?;
            let last = u32::try_from(to).map_err(|_| QueryError::OutOfMemory)?;
            self.items.extend(first..last);
        }
        Ok(self.items.len() >= len)
    }
}

impl<T> Deref for Large<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<T> DerefMut for Large<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn what_is_used_is_counted_until_it_is_dropped() {
        let meter = Meter::new();
        meter.set_limit(1_000);
        let mut a = Large::<u32>::empty(&meter);
        a.reserve(100).unwrap();
        assert_eq!(meter.used(), 400);
        let mut b = Large::<u64>::empty(&meter);
        assert!(b.reserve(100).is_err());
        assert_eq!(meter.used(), 400);
        b.reserve(75).unwrap();
        assert_eq!(meter.used(), 1_000);
        while !a.fill(100, 7, 33).unwrap() {}
        assert_eq!(a.len(), 100);
        assert_eq!(a.push(1), Err(QueryError::OutOfMemory));
        drop(b.take());
        assert_eq!(b.len(), 0);
        a.push(1).unwrap();
        // Room for as many again.
        assert_eq!(meter.used(), 800);
        meter.set_outside(500);
        assert!(!meter.fits(1));
        drop(a);
        assert_eq!(meter.used(), 500);
    }

    #[test]
    fn numbers_are_filled_in_pieces() {
        let meter = Meter::new();
        let mut a = Large::<u32>::empty(&meter);
        while !a.fill_counting(10, 3).unwrap() {}
        assert_eq!(&*a, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert!(a.fill_counting(0, 3).unwrap());
    }

    #[test]
    fn what_cannot_exist_is_refused() {
        let meter = Meter::new();
        assert!(Large::<u64>::empty(&meter).reserve(usize::MAX).is_err());
        assert!(Large::<u8>::empty(&meter).reserve(usize::MAX).is_err());
        assert_eq!(meter.used(), 0);
    }
}
