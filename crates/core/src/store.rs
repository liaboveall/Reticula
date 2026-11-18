//! Identity-slotted, block-adaptive column storage.
//!
//! Each row or column identity maps to a dense storage slot: base-run
//! identities map directly to `id as u32` with zero table lookup, while newly
//! inserted identities receive slots appended at the end. Within a column,
//! slots are partitioned into blocks of [`BLOCK_ROWS`] (`4,096`) entries, each
//! stored as empty, sorted sparse pairs, or a fixed 32 KiB dense array.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};

use crate::id::is_base_id;
use crate::value::Value;

/// Slots per column block.
pub const BLOCK_ROWS: u32 = 4096;

/// Sparse-to-dense promotion threshold (`256 * 10 = 2.5 KiB` vs `32 KiB`).
pub const SPARSE_LIMIT: usize = 256;

type ExtraSlotMap = HashMap<u64, u32, BuildHasherDefault<FxHasher>>;

/// Maps stable 64-bit identities to dense `u32` storage slots.
#[derive(Clone, Debug, Default)]
pub struct SlotMap {
    base_len: u32,
    next_slot: u32,
    extra: ExtraSlotMap,
}

impl SlotMap {
    /// Creates a slot map whose base identities `0..base_len` map directly to
    /// slots `0..base_len` without touching the hash table.
    #[must_use]
    pub fn with_base(base_len: u32) -> Self {
        Self {
            base_len,
            next_slot: base_len,
            extra: ExtraSlotMap::default(),
        }
    }

    /// Number of slots reserved for the base run.
    #[must_use]
    pub const fn base_len(&self) -> u32 {
        self.base_len
    }

    /// Total slots allocated so far (`>= base_len`).
    #[must_use]
    pub const fn slot_bound(&self) -> u32 {
        self.next_slot
    }

    /// Looks up the slot of `id` without allocating one if missing.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<u32> {
        if is_base_id(id) && id < u64::from(self.base_len) {
            return u32::try_from(id).ok();
        }
        self.extra.get(&id).copied()
    }

    /// Looks up the slot of `id`, or allocates a fresh slot if `id` has not
    /// been written to before.
    pub fn get_or_alloc(&mut self, id: u64) -> Option<u32> {
        if let Some(existing) = self.get(id) {
            return Some(existing);
        }
        let slot = self.next_slot;
        self.next_slot = self.next_slot.checked_add(1)?;
        self.extra.insert(id, slot);
        Some(slot)
    }

    /// Approximate heap bytes owned by the extra-slot hash table.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.extra
            .capacity()
            .saturating_mul(size_of::<(u64, u32)>())
    }
}

/// Representation of one `4,096`-row block within a [`Column`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Block {
    /// All `4,096` slots hold [`Value::Empty`].
    #[default]
    Empty,
    /// Sorted `(offset, packed_bits)` entries for non-empty cells.
    Sparse(Vec<(u16, u64)>),
    /// Fixed array of `4,096` packed cell words (`32 KiB`).
    Dense(Box<[u64]>),
}

impl Block {
    fn get(&self, offset: u16) -> u64 {
        match self {
            Self::Empty => Value::EMPTY_BITS,
            Self::Sparse(entries) => entries
                .binary_search_by_key(&offset, |&(off, _)| off)
                .ok()
                .and_then(|idx| entries.get(idx))
                .map_or(Value::EMPTY_BITS, |&(_, bits)| bits),
            Self::Dense(words) => words
                .get(offset as usize)
                .copied()
                .unwrap_or(Value::EMPTY_BITS),
        }
    }

    fn set(&mut self, offset: u16, bits: u64) {
        let is_empty = bits == Value::EMPTY_BITS;
        match self {
            Self::Empty => {
                if !is_empty {
                    *self = Self::Sparse(vec![(offset, bits)]);
                }
            }
            Self::Sparse(entries) => match entries.binary_search_by_key(&offset, |&(off, _)| off) {
                Ok(idx) => {
                    if is_empty {
                        entries.remove(idx);
                        if entries.is_empty() {
                            *self = Self::Empty;
                        }
                    } else if let Some(slot) = entries.get_mut(idx) {
                        slot.1 = bits;
                    }
                }
                Err(idx) if !is_empty => {
                    if entries.len() >= SPARSE_LIMIT {
                        let mut dense = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
                        for &(off, word) in entries.iter() {
                            if let Some(cell) = dense.get_mut(off as usize) {
                                *cell = word;
                            }
                        }
                        if let Some(cell) = dense.get_mut(offset as usize) {
                            *cell = bits;
                        }
                        *self = Self::Dense(dense.into_boxed_slice());
                    } else {
                        entries.insert(idx, (offset, bits));
                    }
                }
                Err(_) => {}
            },
            Self::Dense(words) => {
                if let Some(cell) = words.get_mut(offset as usize) {
                    *cell = bits;
                }
            }
        }
    }

    fn estimated_bytes(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Sparse(entries) => entries.capacity().saturating_mul(size_of::<(u16, u64)>()),
            Self::Dense(words) => words.len().saturating_mul(size_of::<u64>()),
        }
    }
}

/// One column of cells indexed by `u32` row slot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    blocks: Vec<Block>,
    len: u32,
}

impl Column {
    /// Creates an empty column.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            blocks: Vec::new(),
            len: 0,
        }
    }

    /// Creates an empty column pre-sized for `slots` row slots.
    #[must_use]
    pub fn with_slot_capacity(slots: u32) -> Self {
        let block_count = slots.div_ceil(BLOCK_ROWS) as usize;
        Self {
            blocks: Vec::with_capacity(block_count),
            len: 0,
        }
    }

    /// Highest slot index written + 1 (or appended length).
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether no non-empty or appended slots exist.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Underlying blocks, for snapshot encoding.
    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Reconstructs a column from decoded blocks and slot bound.
    #[must_use]
    pub fn from_blocks(blocks: Vec<Block>, len: u32) -> Self {
        Self { blocks, len }
    }

    /// Approximate heap bytes owned by this column.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let outer = self.blocks.capacity().saturating_mul(size_of::<Block>());
        let inner = self.blocks.iter().fold(0usize, |acc, block| {
            acc.saturating_add(block.estimated_bytes())
        });
        outer.saturating_add(inner)
    }

    /// Appends a slot at `self.len` (fast path used by dense sample generation
    /// when populating consecutive base slots).
    pub fn push(&mut self, value: Value) -> bool {
        let Some(new_len) = self.len.checked_add(1) else {
            return false;
        };
        let (block_idx, offset) = locate(self.len);
        if block_idx >= self.blocks.len() {
            self.blocks
                .resize_with(block_idx.saturating_add(1), || Block::Empty);
        }
        let bits = value.pack();
        if bits != Value::EMPTY_BITS
            && let Some(block) = self.blocks.get_mut(block_idx)
        {
            block.set(offset, bits);
        }
        self.len = new_len;
        true
    }

    /// Writes `value` into `slot`, growing the block table with [`Block::Empty`]
    /// entries as needed.
    pub fn set(&mut self, slot: u32, value: Value) -> bool {
        let Some(after) = slot.checked_add(1) else {
            return false;
        };
        let bits = value.pack();
        let (block_idx, offset) = locate(slot);
        if bits == Value::EMPTY_BITS && block_idx >= self.blocks.len() {
            return true;
        }
        if block_idx >= self.blocks.len() {
            self.blocks
                .resize_with(block_idx.saturating_add(1), || Block::Empty);
        }
        if let Some(block) = self.blocks.get_mut(block_idx) {
            block.set(offset, bits);
        }
        if after > self.len {
            self.len = after;
        }
        true
    }

    /// Reads the value at `slot`; [`Value::Empty`] past the end.
    #[must_use]
    pub fn get(&self, slot: u32) -> Value {
        Value::unpack(self.get_packed(slot))
    }

    /// Reads the raw 64-bit packed word at `slot`.
    #[must_use]
    pub fn get_packed(&self, slot: u32) -> u64 {
        let (block_idx, offset) = locate(slot);
        self.blocks
            .get(block_idx)
            .map_or(Value::EMPTY_BITS, |block| block.get(offset))
    }

    /// Packed words of slots `0..self.len`, in slot order.
    pub fn packed(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len).map(|slot| self.get_packed(slot))
    }
}

/// Block index and intra-block offset of a row slot.
const fn locate(slot: u32) -> (usize, u16) {
    ((slot / BLOCK_ROWS) as usize, (slot % BLOCK_ROWS) as u16)
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{BLOCK_ROWS, Block, Column, SPARSE_LIMIT, SlotMap};
    use crate::id::pack_id;
    use crate::value::Value;

    #[test]
    fn rows_are_read_back_across_blocks() {
        let mut column = Column::new();
        assert!(column.is_empty());
        let rows = BLOCK_ROWS * 2 + 3;
        for row in 0..rows {
            assert!(column.push(Value::Number(f64::from(row))));
        }
        assert_eq!(column.len(), rows);
        for row in [0, 1, BLOCK_ROWS - 1, BLOCK_ROWS, BLOCK_ROWS * 2, rows - 1] {
            assert_eq!(column.get(row), Value::Number(f64::from(row)));
        }
        assert_eq!(column.get(rows), Value::Empty);
        assert_eq!(column.get(u32::MAX), Value::Empty);
        assert_eq!(column.packed().count(), rows as usize);
    }

    #[test]
    fn sparse_blocks_stay_small_and_promote_cleanly() {
        let mut column = Column::new();
        assert!(column.set(999_999, Value::Bool(true)));
        assert_eq!(column.get(999_999), Value::Bool(true));
        assert_eq!(column.get(500_000), Value::Empty);
        // Only one tiny sparse block allocated even at slot 999,999.
        assert!(column.estimated_bytes() < 8192);

        // Clearing returns the block to Empty.
        assert!(column.set(999_999, Value::Empty));
        assert_eq!(column.get(999_999), Value::Empty);

        // Writing > SPARSE_LIMIT entries in block 0 promotes it to Dense.
        let limit = u32::try_from(SPARSE_LIMIT).unwrap_or(256);
        for offset in 0..=limit {
            assert!(column.set(offset, Value::Number(f64::from(offset))));
        }
        assert!(matches!(column.blocks().first(), Some(Block::Dense(_))));
        for offset in 0..=limit {
            assert_eq!(column.get(offset), Value::Number(f64::from(offset)));
        }
    }

    #[test]
    fn slot_map_uses_identity_arithmetic_for_base_and_table_for_inserted() {
        let mut map = SlotMap::with_base(1_048_576);
        assert_eq!(map.get(0), Some(0));
        assert_eq!(map.get(999_999), Some(999_999));
        let Some(inserted) = pack_id(0, 1, 1) else {
            panic!("id");
        };
        assert_eq!(map.get(inserted), None);
        assert_eq!(map.get_or_alloc(inserted), Some(1_048_576));
        assert_eq!(map.get(inserted), Some(1_048_576));
    }
}
