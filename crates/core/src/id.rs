//! Stable 64-bit identities for rows, columns and sheets.
//!
//! Every row and column identity packs three fields into 64 bits:
//!
//! - bits 56..64: the 8-bit log epoch (`0..=255`), incremented whenever a
//!   server restores from an older backup so that replica numbers issued after
//!   the backup point can never collide with newly issued ones;
//! - bits 32..56: the 24-bit replica number within that epoch
//!   (`0..=16_777_215`); `(epoch = 0, replica = 0)` is reserved for the
//!   implicit base run of a fresh sheet, so base IDs equal their 0-based
//!   positions;
//! - bits 0..32: the 32-bit monotonic counter within that replica.

use serde::{Deserialize, Serialize};

/// Largest 24-bit replica number within one log epoch.
pub const MAX_REPLICA: u32 = 0x00FF_FFFF;

/// Stable identifier of a worksheet within a workbook.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct SheetId(pub u32);

/// Stable identifier of a row.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct RowId(pub u64);

/// Stable identifier of a column.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ColId(pub u64);

/// Address of one cell by stable identity.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct CellKey {
    /// Worksheet that owns the cell.
    pub sheet: SheetId,
    /// Stable row identity.
    pub row: RowId,
    /// Stable column identity.
    pub col: ColId,
}

/// Packs `(epoch, replica, counter)` into a 64-bit identity, or returns `None`
/// if `replica` exceeds [`MAX_REPLICA`].
#[must_use]
pub const fn pack_id(epoch: u8, replica: u32, counter: u32) -> Option<u64> {
    if replica > MAX_REPLICA {
        return None;
    }
    let high = ((epoch as u32) << 24) | replica;
    Some(((high as u64) << 32) | (counter as u64))
}

/// Extracts `(epoch, replica, counter)` from a packed 64-bit identity.
#[must_use]
pub const fn unpack_id(id: u64) -> (u8, u32, u32) {
    let high = upper_u32(id);
    let low = lower_u32(id);
    let epoch = top_byte(high);
    let replica = high & MAX_REPLICA;
    (epoch, replica, low)
}

/// Whether `id` belongs to the implicit base run `(epoch = 0, replica = 0)`.
#[must_use]
pub const fn is_base_id(id: u64) -> bool {
    (id >> 32) == 0
}

/// Upper 32 bits `(epoch << 24 | replica)` shared by IDs from the same stream.
#[must_use]
pub const fn id_stream(id: u64) -> u32 {
    upper_u32(id)
}

/// Lower 32-bit counter of an identity.
#[must_use]
pub const fn id_counter(id: u64) -> u32 {
    lower_u32(id)
}

/// A contiguous run of `len` identities from the same `(epoch, replica)`
/// stream: `first_id, first_id + 1, ..., first_id + len - 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRun {
    /// First identity in the run.
    pub first_id: u64,
    /// Number of identities in the run (`> 0` for non-empty runs).
    pub len: u32,
}

impl IdRun {
    /// Constructs a validated contiguous run, returning `None` if `len == 0`
    /// or if `counter + len - 1` would overflow the 32-bit counter field into
    /// another replica stream.
    #[must_use]
    pub const fn new(first_id: u64, len: u32) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let start = id_counter(first_id);
        if start.checked_add(len.saturating_sub(1)).is_none() {
            return None;
        }
        Some(Self { first_id, len })
    }

    /// Whether this run is internally valid (non-empty and stays within one
    /// `(epoch, replica)` stream).
    #[must_use]
    pub const fn is_valid(self) -> bool {
        Self::new(self.first_id, self.len).is_some()
    }

    /// Whether `id` lies inside this run.
    #[must_use]
    pub const fn contains(self, id: u64) -> bool {
        if !self.is_valid() || id_stream(id) != id_stream(self.first_id) {
            return false;
        }
        let start = id_counter(self.first_id);
        let counter = id_counter(id);
        let Some(offset) = counter.checked_sub(start) else {
            return false;
        };
        offset < self.len
    }

    /// Identity at `offset` within the run, if `offset < self.len`.
    #[must_use]
    pub const fn id_at(self, offset: u32) -> Option<u64> {
        if offset >= self.len || !self.is_valid() {
            return None;
        }
        self.first_id.checked_add(offset as u64)
    }

    /// Splits the run into `(left, right)` where `left` has `left_len` items.
    #[must_use]
    pub const fn split_at(self, left_len: u32) -> Option<(Self, Self)> {
        if left_len == 0 || left_len >= self.len || !self.is_valid() {
            return None;
        }
        let Some(right_first) = self.first_id.checked_add(left_len as u64) else {
            return None;
        };
        let right_len = self.len.saturating_sub(left_len);
        let (Some(left), Some(right)) = (
            Self::new(self.first_id, left_len),
            Self::new(right_first, right_len),
        ) else {
            return None;
        };
        Some((left, right))
    }

    /// Iterates all packed `u64` identities in the run in order.
    pub fn iter(self) -> impl Iterator<Item = u64> {
        let count = if self.is_valid() { self.len } else { 0 };
        (0..count).map(move |offset| self.first_id.wrapping_add(u64::from(offset)))
    }
}

/// Monotonic allocator of identities for one `(epoch, replica)` stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdAllocator {
    epoch: u8,
    replica: u32,
    next_counter: u32,
}

impl IdAllocator {
    /// Creates an allocator starting at `next_counter` (clamped to at least 1
    /// for non-base streams so `(0, 0, 0)` stays unambiguous).
    #[must_use]
    pub const fn new(epoch: u8, replica: u32, next_counter: u32) -> Option<Self> {
        if replica > MAX_REPLICA {
            return None;
        }
        let start = if epoch == 0 && replica == 0 {
            next_counter
        } else if next_counter == 0 {
            1
        } else {
            next_counter
        };
        Some(Self {
            epoch,
            replica,
            next_counter: start,
        })
    }

    /// Creates the default local-document allocator `(epoch = 0, replica = 1)`.
    #[must_use]
    pub const fn local() -> Self {
        Self {
            epoch: 0,
            replica: 1,
            next_counter: 1,
        }
    }

    /// Epoch of identities issued by this allocator.
    #[must_use]
    pub const fn epoch(&self) -> u8 {
        self.epoch
    }

    /// Replica number of identities issued by this allocator.
    #[must_use]
    pub const fn replica(&self) -> u32 {
        self.replica
    }

    /// Next counter that will be issued.
    #[must_use]
    pub const fn next_counter(&self) -> u32 {
        self.next_counter
    }

    /// Allocates a contiguous run of `len` fresh identities.
    pub fn alloc_run(&mut self, len: u32) -> Option<IdRun> {
        if len == 0 {
            return None;
        }
        let after_last = self.next_counter.checked_add(len.saturating_sub(1))?;
        let first_id = pack_id(self.epoch, self.replica, self.next_counter)?;
        let run = IdRun::new(first_id, len)?;
        self.next_counter = after_last.saturating_add(1);
        // If `after_last == u32::MAX`, `next_counter` stays `u32::MAX` and any
        // subsequent `alloc_run` with `len >= 1` will fail if `next_counter`
        // was already used; guard against reusing `u32::MAX` by checking here.
        if after_last == u32::MAX {
            // Mark exhausted by setting replica past MAX_REPLICA internally.
            self.replica = MAX_REPLICA.saturating_add(1);
        }
        Some(run)
    }
}

const fn upper_u32(word: u64) -> u32 {
    (word >> 32) as u32
}

const fn lower_u32(word: u64) -> u32 {
    (word & 0xFFFF_FFFF) as u32
}

const fn top_byte(word: u32) -> u8 {
    (word >> 24) as u8
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{IdAllocator, IdRun, MAX_REPLICA, is_base_id, pack_id, unpack_id};

    #[test]
    fn ids_pack_and_unpack_without_loss() {
        for (epoch, replica, counter) in [
            (0, 0, 0),
            (0, 0, 1_048_575),
            (0, 1, 1),
            (7, MAX_REPLICA, u32::MAX),
            (255, 42, 999_999),
        ] {
            let Some(id) = pack_id(epoch, replica, counter) else {
                panic!("valid parts rejected");
            };
            assert_eq!(unpack_id(id), (epoch, replica, counter));
            assert_eq!(is_base_id(id), epoch == 0 && replica == 0);
        }
        assert_eq!(pack_id(0, MAX_REPLICA + 1, 0), None);
    }

    #[test]
    fn runs_stay_within_one_stream() {
        let Some(base) = pack_id(1, 5, u32::MAX - 2) else {
            panic!("valid id");
        };
        assert!(IdRun::new(base, 3).is_some());
        assert!(IdRun::new(base, 4).is_none());

        let mut alloc = IdAllocator::local();
        let Some(run) = alloc.alloc_run(4) else {
            panic!("allocation succeeds");
        };
        assert_eq!(run.len, 4);
        assert!(run.contains(run.first_id));
        assert!(run.contains(run.first_id.saturating_add(3)));
        assert!(!run.contains(run.first_id.saturating_add(4)));
    }
}
