//! The workbook's string table: cells hold an index, the text lives here once.
//!
//! This is the first cut: append and read. Looking a string up to reuse its
//! index, and dropping strings no cell refers to, come with editing.

use alloc::vec::Vec;

/// Strings stored end to end, with the offset at which each one ends.
#[derive(Clone, Debug, Default)]
pub struct StringTable {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl StringTable {
    /// Creates an empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            ends: Vec::new(),
        }
    }

    /// Number of strings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether the table holds no strings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Adds a string and returns its index, or `None` when the table is full.
    pub fn push(&mut self, text: &str) -> Option<u32> {
        let id = u32::try_from(self.ends.len()).ok()?;
        let end = u32::try_from(self.bytes.len().checked_add(text.len())?).ok()?;
        self.bytes.extend_from_slice(text.as_bytes());
        self.ends.push(end);
        Some(id)
    }

    /// The string with this index; empty for an index that was never issued.
    #[must_use]
    pub fn get(&self, id: u32) -> &str {
        let index = id as usize;
        let Some(end) = self.ends.get(index) else {
            return "";
        };
        let start = index
            .checked_sub(1)
            .and_then(|previous| self.ends.get(previous))
            .copied()
            .unwrap_or(0);
        self.bytes
            .get(start as usize..*end as usize)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::StringTable;

    #[test]
    fn strings_are_read_back_by_index() {
        let mut table = StringTable::new();
        assert!(table.is_empty());
        let ids: [Option<u32>; 4] = ["华东", "", "SKU-0042", "a"].map(|text| table.push(text));
        assert_eq!(ids, [Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(table.len(), 4);
        assert_eq!(table.get(0), "华东");
        assert_eq!(table.get(1), "");
        assert_eq!(table.get(2), "SKU-0042");
        assert_eq!(table.get(3), "a");
        assert_eq!(table.get(4), "");
    }
}
