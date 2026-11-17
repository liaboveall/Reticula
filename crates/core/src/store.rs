//! Column storage.
//!
//! A column is a list of fixed-size blocks of packed values. Nothing moves
//! when a column grows, and a block can later be compressed, loaded or sent
//! on its own.
//!
//! This is the first cut: dense blocks, appended to and read by row.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::value::Value;

/// Rows per block.
pub const BLOCK_ROWS: u32 = 4096;

/// One column of cells.
#[derive(Clone, Debug, Default)]
pub struct Column {
    blocks: Vec<Box<[u64]>>,
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

    /// Number of rows.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether the column has no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends a row. Returns `false`, leaving the column unchanged, once it
    /// holds as many rows as can be numbered.
    pub fn push(&mut self, value: Value) -> bool {
        let Some(new_len) = self.len.checked_add(1) else {
            return false;
        };
        let (block, offset) = locate(self.len);
        if offset == 0 {
            self.blocks
                .push(vec![Value::EMPTY_BITS; BLOCK_ROWS as usize].into_boxed_slice());
        }
        let Some(cell) = self
            .blocks
            .get_mut(block)
            .and_then(|block| block.get_mut(offset))
        else {
            return false;
        };
        *cell = value.pack();
        self.len = new_len;
        true
    }

    /// The value in a row; empty past the end.
    #[must_use]
    pub fn get(&self, row: u32) -> Value {
        if row >= self.len {
            return Value::Empty;
        }
        let (block, offset) = locate(row);
        self.blocks
            .get(block)
            .and_then(|block| block.get(offset))
            .map_or(Value::Empty, |bits| Value::unpack(*bits))
    }

    /// The packed words of every row, in order, for digests and snapshots.
    pub fn packed(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len).map(|row| self.get(row).pack())
    }
}

/// The block a row is in, and its place in that block.
const fn locate(row: u32) -> (usize, usize) {
    ((row / BLOCK_ROWS) as usize, (row % BLOCK_ROWS) as usize)
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{BLOCK_ROWS, Column};
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
}
