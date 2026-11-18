//! The workbook's string table: cells hold an index, the text lives here once.
//!
//! Identical strings share a single `u32` index. Lookup uses an internal hash
//! index keyed by a deterministic 64-bit hash of the string's UTF-8 bytes; the
//! hash table is never iterated, so assigned indices depend solely on the order
//! in which distinct strings are interned.

use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::digest::Digest64;

type BucketMap = HashMap<u64, SmallVec<[u32; 1]>, BuildHasherDefault<FxHasher>>;

/// Deduplicated UTF-8 string pool stored end to end.
#[derive(Clone, Debug, Default)]
pub struct StringTable {
    bytes: Vec<u8>,
    ends: Vec<u32>,
    buckets: BucketMap,
}

impl StringTable {
    /// Creates an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty table with pre-reserved capacity for `strings` entries
    /// and `total_bytes` of UTF-8 text.
    #[must_use]
    pub fn with_capacity(strings: usize, total_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(total_bytes),
            ends: Vec::with_capacity(strings),
            buckets: BucketMap::with_capacity_and_hasher(strings, BuildHasherDefault::default()),
        }
    }

    /// Number of distinct strings stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether the table holds no strings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Approximate heap bytes owned by the string table.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(self.ends.capacity().saturating_mul(size_of::<u32>()))
            .saturating_add(
                self.buckets
                    .capacity()
                    .saturating_mul(size_of::<(u64, SmallVec<[u32; 1]>)>()),
            )
    }

    /// Interns `text`, returning its existing index if already stored or
    /// appending it otherwise. Returns `None` if `u32` capacity is exhausted.
    pub fn intern(&mut self, text: &str) -> Option<u32> {
        let hash = hash_str(text);
        if let Some(candidates) = self.buckets.get(&hash) {
            for candidate in candidates {
                if self.get(*candidate) == text {
                    return Some(*candidate);
                }
            }
        }
        let id = u32::try_from(self.ends.len()).ok()?;
        let end = u32::try_from(self.bytes.len().checked_add(text.len())?).ok()?;
        self.bytes.extend_from_slice(text.as_bytes());
        self.ends.push(end);
        self.buckets.entry(hash).or_default().push(id);
        Some(id)
    }

    /// Alias of [`Self::intern`] retained for callers that append strings.
    pub fn push(&mut self, text: &str) -> Option<u32> {
        self.intern(text)
    }

    /// Checks whether `id` refers to a valid entry in this table.
    #[must_use]
    pub fn contains_id(&self, id: u32) -> bool {
        (id as usize) < self.ends.len()
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

    /// Iterates all stored strings in deterministic index order `0..len`.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &str)> + '_ {
        let count = u32::try_from(self.ends.len()).unwrap_or(u32::MAX);
        (0..count).map(move |id| (id, self.get(id)))
    }
}

fn hash_str(text: &str) -> u64 {
    let mut digest = Digest64::new();
    digest.write_bytes(text.as_bytes());
    digest.finish()
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::StringTable;

    #[test]
    fn strings_are_read_back_and_deduplicated() {
        let mut table = StringTable::new();
        assert!(table.is_empty());
        let ids: [Option<u32>; 4] = ["华东", "", "SKU-0042", "a"].map(|text| table.intern(text));
        assert_eq!(ids, [Some(0), Some(1), Some(2), Some(3)]);
        // Interning duplicates reuses the existing indices without growing the table.
        assert_eq!(table.intern("华东"), Some(0));
        assert_eq!(table.intern(""), Some(1));
        assert_eq!(table.intern("SKU-0042"), Some(2));
        assert_eq!(table.len(), 4);
        assert_eq!(table.get(0), "华东");
        assert_eq!(table.get(1), "");
        assert_eq!(table.get(2), "SKU-0042");
        assert_eq!(table.get(3), "a");
        assert_eq!(table.get(4), "");
    }
}
