//! Heap reservation helpers for predictable linear-memory growth.
//!
//! Growing WebAssembly linear memory one 64 KiB page at a time during a large
//! operation (such as generating a million-row sheet or decoding a snapshot)
//! invokes the host's `memory.grow` thousands of times. Asking the global
//! allocator for one contiguous block up front and releasing it immediately
//! merges those calls into a single growth step while leaving the pages in the
//! allocator's free list.

use alloc::vec::Vec;

/// Pre-warms the global allocator so at least `bytes` of contiguous heap space
/// has been requested from the underlying memory backing.
///
/// Returns `false` without panicking or trapping if the allocator refuses the
/// reservation.
#[must_use]
pub fn try_reserve_heap(bytes: usize) -> bool {
    if bytes == 0 {
        return true;
    }
    let mut scratch = Vec::<u8>::new();
    if scratch.try_reserve_exact(bytes).is_err() {
        return false;
    }
    drop(scratch);
    true
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::try_reserve_heap;

    #[test]
    fn small_reservation_succeeds() {
        assert!(try_reserve_heap(0));
        assert!(try_reserve_heap(1024 * 1024));
    }
}
