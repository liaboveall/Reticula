//! Lazy generation-stamped hash index cache for exact column lookups (`19-m3b.md` §6).
//!
//! Speeds up `VLOOKUP(..., FALSE)`, `MATCH(..., 0)`, `XLOOKUP`, and `XMATCH`
//! exact vertical column lookups (`>= 64` rows) from `O(R)` linear scans to
//! `O(1)` probes while preserving Excel first-match, type-strict, blank-skipping,
//! wildcard-fallback, and case-insensitive Unicode equivalence semantics.

use alloc::vec::Vec;
use core::mem::size_of;

use reticula_core::id::{ColId, IdRun, SheetId};
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block, Column};
use reticula_core::value::Value;

use crate::value_eval::EvalScalar;

/// Minimum live row span required before activating a hash index.
pub(crate) const MIN_INDEX_ROWS: u32 = 64;

/// Maximum resident memory budget across all cached column lookup indexes (`32 MiB`).
pub(crate) const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Maximum tracked cold lookup ranges awaiting a second hit.
const MAX_COLD_TRACKER_ENTRIES: usize = 1024;

const HASH_TAG_NUM: u64 = 0x1111_1111_1111_1111;
const HASH_TAG_BOOL: u64 = 0x2222_2222_2222_2222;
const HASH_TAG_TEXT: u64 = 0x3333_3333_3333_3333;

/// Identifies a contiguous slice of live positions within a single worksheet column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LookupRangeKey {
    /// Target worksheet identifier.
    pub sheet: SheetId,
    /// Lookup column identifier.
    pub col: ColId,
    /// Starting 0-based live row position within `sheet`.
    pub first_row_pos: u32,
    /// Number of contiguous live rows indexed from `first_row_pos`.
    pub row_count: u32,
}

/// Generation stamp invalidating an indexed column whenever its axis layout or
/// column cells change (`O(1)` verification).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LookupStamp {
    row_axis_gen: u64,
    col_axis_gen: u64,
    col_gen: u64,
}

impl LookupStamp {
    #[inline]
    fn from_sheet(sheet: &Sheet, col: ColId) -> Self {
        Self {
            row_axis_gen: sheet.row_axis_generation(),
            col_axis_gen: sheet.col_axis_generation(),
            col_gen: sheet.col_generation(col),
        }
    }
}

/// Probe result from querying a [`ColumnHashIndex`] with a dynamic needle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LookupProbeResult {
    /// Index answered definitively (`Some(row_offset)` = 0-based first match).
    Hit(Option<u32>),
    /// Needle contains wildcard/escape tokens (`*`, `?`, `~`), so the caller
    /// should fall back to wildcard pattern scanning.
    Bypass,
}

/// Compact `16`-byte open-addressed slot storing the 31-bit non-zero hash tag,
/// earliest 0-based live row offset, and packed 64-bit [`Value`] word.
#[derive(Clone, Copy, Debug)]
struct HashSlot {
    tag_hash: u32,
    row_offset: u32,
    packed_val: u64,
}

impl HashSlot {
    const EMPTY: Self = Self {
        tag_hash: 0,
        row_offset: 0,
        packed_val: 0,
    };
}

/// Deterministic SplitMix64 open-addressed column lookup index.
#[derive(Clone, Debug)]
pub(crate) struct ColumnHashIndex {
    slots: Vec<HashSlot>,
    mask: usize,
}

impl ColumnHashIndex {
    fn empty() -> Self {
        Self {
            slots: Vec::new(),
            mask: 0,
        }
    }

    #[inline]
    const fn byte_size(&self) -> usize {
        self.slots.len().saturating_mul(size_of::<HashSlot>())
    }

    fn build(sheet: &Sheet, col: ColId, first_row_pos: u32, row_count: u32) -> Self {
        let Some(col_slot) = sheet.col_slots().get(col.0) else {
            return Self::empty();
        };
        let Some(column) = sheet.raw_columns().get(col_slot as usize) else {
            return Self::empty();
        };

        let mut row_runs: Vec<IdRun> = Vec::new();
        sheet
            .row_axis()
            .for_each_live_positions(first_row_pos, row_count, |r| row_runs.push(r));

        let mut non_empty: usize = 0;
        for_each_lookup_word(sheet, column, &row_runs, |_, bits| {
            if is_indexable_bits(bits) {
                non_empty = non_empty.saturating_add(1);
            }
        });
        if non_empty == 0 {
            return Self::empty();
        }

        let target = non_empty.saturating_mul(10) / 7;
        let cap = target.max(64).next_power_of_two();
        let mut slots = Vec::new();
        if slots.try_reserve_exact(cap).is_err() {
            return Self::empty();
        }
        slots.resize(cap, HashSlot::EMPTY);
        let mask = cap.saturating_sub(1);

        for_each_lookup_word(sheet, column, &row_runs, |row_offset, bits| {
            let Some((canon_bits, hash)) = hash_cell_bits(sheet, bits) else {
                return;
            };
            let tag = tag_from_hash(hash);
            let mut idx = slot_index(hash, mask);
            let mut probes = 0_usize;
            while probes < cap {
                let Some(slot) = slots.get_mut(idx) else {
                    break;
                };
                if slot.tag_hash == 0 {
                    *slot = HashSlot {
                        tag_hash: tag,
                        row_offset,
                        packed_val: canon_bits,
                    };
                    break;
                }
                if slot.tag_hash == tag && cell_bits_equal(sheet, slot.packed_val, canon_bits) {
                    // Preserve earliest occurrence in column order.
                    break;
                }
                idx = idx.saturating_add(1) & mask;
                probes = probes.saturating_add(1);
            }
        });

        Self { slots, mask }
    }

    /// Probes the column index for finite number `n` (`-0.0` canonicalized to `+0.0`).
    #[inline]
    pub(crate) fn probe_number(&self, n: f64) -> Option<u32> {
        if self.slots.is_empty() || !n.is_finite() {
            return None;
        }
        let canon = if n == 0.0 { 0.0 } else { n };
        let target_bits = Value::number(canon).pack();
        let hash = hash_number_bits(canon.to_bits());
        let tag = tag_from_hash(hash);
        let cap = self.slots.len();
        let mut idx = slot_index(hash, self.mask);
        let mut probes = 0_usize;
        while probes < cap {
            let slot = self.slots.get(idx)?;
            if slot.tag_hash == 0 {
                return None;
            }
            if slot.tag_hash == tag && slot.packed_val == target_bits {
                return Some(slot.row_offset);
            }
            idx = idx.saturating_add(1) & self.mask;
            probes = probes.saturating_add(1);
        }
        None
    }

    /// Probes the column index for boolean `b`.
    #[inline]
    pub(crate) fn probe_bool(&self, b: bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let target_bits = Value::Bool(b).pack();
        let hash = hash_bool(b);
        let tag = tag_from_hash(hash);
        let cap = self.slots.len();
        let mut idx = slot_index(hash, self.mask);
        let mut probes = 0_usize;
        while probes < cap {
            let slot = self.slots.get(idx)?;
            if slot.tag_hash == 0 {
                return None;
            }
            if slot.tag_hash == tag && slot.packed_val == target_bits {
                return Some(slot.row_offset);
            }
            idx = idx.saturating_add(1) & self.mask;
            probes = probes.saturating_add(1);
        }
        None
    }

    /// Probes the column index for plain case-insensitive text `needle`.
    pub(crate) fn probe_text(&self, sheet: &Sheet, needle: &str) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let hash = hash_lower_text(needle);
        let tag = tag_from_hash(hash);
        let cap = self.slots.len();
        let mut idx = slot_index(hash, self.mask);
        let mut probes = 0_usize;
        while probes < cap {
            let slot = self.slots.get(idx)?;
            if slot.tag_hash == 0 {
                return None;
            }
            if slot.tag_hash == tag
                && let Value::Text(id) = Value::unpack(slot.packed_val)
                && text_eq_ci(sheet.strings().get(id), needle)
            {
                return Some(slot.row_offset);
            }
            idx = idx.saturating_add(1) & self.mask;
            probes = probes.saturating_add(1);
        }
        None
    }

    /// Probes the index for an arbitrary scalar needle, bypassing when wildcards apply.
    pub(crate) fn probe_scalar(
        &self,
        sheet: &Sheet,
        needle: &EvalScalar,
        allow_wildcards: bool,
    ) -> LookupProbeResult {
        match needle {
            EvalScalar::Empty => LookupProbeResult::Hit(self.probe_number(0.0)),
            EvalScalar::Number(n) => LookupProbeResult::Hit(self.probe_number(*n)),
            EvalScalar::Bool(b) => LookupProbeResult::Hit(self.probe_bool(*b)),
            EvalScalar::Text(s) => {
                if allow_wildcards && has_wildcard_tokens(s) {
                    LookupProbeResult::Bypass
                } else {
                    LookupProbeResult::Hit(self.probe_text(sheet, s))
                }
            }
            EvalScalar::Error(_) => LookupProbeResult::Hit(None),
        }
    }
}

#[derive(Clone, Debug)]
struct CacheEntry {
    key: LookupRangeKey,
    stamp: LookupStamp,
    index: ColumnHashIndex,
    byte_size: usize,
    last_used: u64,
}

#[derive(Clone, Copy, Debug)]
struct ColdTouchEntry {
    key: LookupRangeKey,
    stamp: LookupStamp,
    hits: u32,
    last_seen: u64,
}

/// Workbook-level lazy LRU cache of exact-match column hash indexes (`32 MiB` cap).
#[derive(Clone, Debug, Default)]
pub struct LookupIndexCache {
    entries: Vec<CacheEntry>,
    cold_touches: Vec<ColdTouchEntry>,
    total_bytes: usize,
    tick: u64,
}

impl LookupIndexCache {
    /// Creates an empty lazy lookup index cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            cold_touches: Vec::new(),
            total_bytes: 0,
            tick: 0,
        }
    }

    /// Clears all cached indexes and cold-hit counters.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cold_touches.clear();
        self.total_bytes = 0;
    }

    /// Invalidates any cached index or tracker entry belonging to `sheet`.
    pub fn invalidate_sheet(&mut self, sheet: SheetId) {
        self.cold_touches.retain(|c| c.key.sheet != sheet);
        self.entries.retain(|e| e.key.sheet != sheet);
        self.total_bytes = self
            .entries
            .iter()
            .fold(0_usize, |acc, e| acc.saturating_add(e.byte_size));
    }

    /// Returns a valid [`ColumnHashIndex`] for `(sheet, col, first_row_pos, row_count)`
    /// once `row_count >= MIN_INDEX_ROWS` and cumulative touches reach `2`.
    pub(crate) fn get_or_build(
        &mut self,
        sheet: &Sheet,
        col: ColId,
        first_row_pos: u32,
        row_count: u32,
        touch_count: u32,
    ) -> Option<&ColumnHashIndex> {
        if row_count < MIN_INDEX_ROWS {
            return None;
        }
        let key = LookupRangeKey {
            sheet: sheet.id(),
            col,
            first_row_pos,
            row_count,
        };
        let stamp = LookupStamp::from_sheet(sheet, col);
        self.tick = self.tick.saturating_add(1);
        let now = self.tick;

        if let Some(pos) = self.entries.iter().position(|e| e.key == key) {
            let stale = self.entries.get(pos).is_some_and(|e| e.stamp != stamp);
            if stale {
                let new_idx = ColumnHashIndex::build(sheet, col, first_row_pos, row_count);
                let new_bytes = new_idx.byte_size();
                if new_bytes > MAX_CACHE_BYTES {
                    let removed = self.entries.swap_remove(pos);
                    self.total_bytes = self.total_bytes.saturating_sub(removed.byte_size);
                    return None;
                }
                if let Some(entry) = self.entries.get_mut(pos) {
                    let old_bytes = entry.byte_size;
                    entry.stamp = stamp;
                    entry.index = new_idx;
                    entry.byte_size = new_bytes;
                    entry.last_used = now;
                    self.total_bytes = self
                        .total_bytes
                        .saturating_sub(old_bytes)
                        .saturating_add(new_bytes);
                }
                while self.total_bytes > MAX_CACHE_BYTES && self.entries.len() > 1 {
                    let Some(victim_idx) = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.key != key)
                        .min_by_key(|(_, e)| e.last_used)
                        .map(|(i, _)| i)
                    else {
                        break;
                    };
                    let victim = self.entries.swap_remove(victim_idx);
                    self.total_bytes = self.total_bytes.saturating_sub(victim.byte_size);
                }
                return self.entries.iter().find(|e| e.key == key).map(|e| &e.index);
            }

            if let Some(entry) = self.entries.get_mut(pos) {
                entry.last_used = now;
            }
            return self.entries.get(pos).map(|e| &e.index);
        }

        let cold_pos = self.cold_touches.iter().position(|c| c.key == key);
        let prev_hits = cold_pos.map_or(0, |p| {
            self.cold_touches
                .get(p)
                .filter(|c| c.stamp == stamp)
                .map_or(0, |c| c.hits)
        });
        let new_hits = prev_hits.saturating_add(touch_count);
        if new_hits < 2 {
            if let Some(p) = cold_pos {
                if let Some(slot) = self.cold_touches.get_mut(p) {
                    slot.stamp = stamp;
                    slot.hits = new_hits;
                    slot.last_seen = now;
                }
            } else {
                if self.cold_touches.len() >= MAX_COLD_TRACKER_ENTRIES
                    && let Some(victim) = self
                        .cold_touches
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, c)| c.last_seen)
                        .map(|(i, _)| i)
                {
                    let _ = self.cold_touches.swap_remove(victim);
                }
                self.cold_touches.push(ColdTouchEntry {
                    key,
                    stamp,
                    hits: new_hits,
                    last_seen: now,
                });
            }
            return None;
        }

        if let Some(p) = cold_pos {
            let _ = self.cold_touches.swap_remove(p);
        }

        let index = ColumnHashIndex::build(sheet, col, first_row_pos, row_count);
        let byte_size = index.byte_size();
        if byte_size > MAX_CACHE_BYTES {
            return None;
        }
        while self.total_bytes.saturating_add(byte_size) > MAX_CACHE_BYTES
            && !self.entries.is_empty()
        {
            let Some(victim_idx) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(i, _)| i)
            else {
                break;
            };
            let victim = self.entries.swap_remove(victim_idx);
            self.total_bytes = self.total_bytes.saturating_sub(victim.byte_size);
        }
        self.total_bytes = self.total_bytes.saturating_add(byte_size);
        self.entries.push(CacheEntry {
            key,
            stamp,
            index,
            byte_size,
            last_used: now,
        });
        self.entries.last().map(|e| &e.index)
    }

    #[cfg(test)]
    pub(crate) const fn hot_entry_count(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    pub(crate) fn insert_synthetic_for_test(
        &mut self,
        sheet: SheetId,
        col: ColId,
        slot_count: usize,
    ) {
        let index = ColumnHashIndex {
            slots: alloc::vec![HashSlot::EMPTY; slot_count],
            mask: slot_count.saturating_sub(1),
        };
        let byte_size = index.byte_size();
        if byte_size > MAX_CACHE_BYTES {
            return;
        }
        self.tick = self.tick.saturating_add(1);
        let now = self.tick;
        while self.total_bytes.saturating_add(byte_size) > MAX_CACHE_BYTES
            && !self.entries.is_empty()
        {
            let Some(victim_idx) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(i, _)| i)
            else {
                break;
            };
            let victim = self.entries.swap_remove(victim_idx);
            self.total_bytes = self.total_bytes.saturating_sub(victim.byte_size);
        }
        self.total_bytes = self.total_bytes.saturating_add(byte_size);
        self.entries.push(CacheEntry {
            key: LookupRangeKey {
                sheet,
                col,
                first_row_pos: 0,
                row_count: MIN_INDEX_ROWS,
            },
            stamp: LookupStamp {
                row_axis_gen: 1,
                col_axis_gen: 1,
                col_gen: 1,
            },
            index,
            byte_size,
            last_used: now,
        });
    }
}

/// Returns `true` iff `s` contains any Excel wildcard or escape token (`*`, `?`, `~`).
#[inline]
pub(crate) fn has_wildcard_tokens(s: &str) -> bool {
    s.as_bytes()
        .iter()
        .any(|&b| matches!(b, b'*' | b'?' | b'~'))
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[inline]
fn finalize_hash(h: u64) -> u64 {
    let mixed = splitmix64(h);
    if mixed == 0 { 1 } else { mixed }
}

#[inline]
fn tag_from_hash(hash: u64) -> u32 {
    let hi = u32::try_from(hash >> 32).unwrap_or(0);
    hi | 1
}

#[inline]
fn slot_index(hash: u64, mask: usize) -> usize {
    let low32 = u32::try_from(hash & u64::from(u32::MAX)).unwrap_or(0);
    (low32 as usize) & mask
}

#[inline]
fn hash_number_bits(canon_bits: u64) -> u64 {
    finalize_hash(HASH_TAG_NUM ^ canon_bits)
}

#[inline]
fn hash_bool(b: bool) -> u64 {
    finalize_hash(HASH_TAG_BOOL ^ u64::from(b))
}

fn hash_lower_text(s: &str) -> u64 {
    let mut h = HASH_TAG_TEXT;
    if s.is_ascii() {
        for b in s.bytes() {
            h = splitmix64(h ^ u64::from(b.to_ascii_lowercase()));
        }
    } else {
        let lower = s.to_lowercase();
        for ch in lower.chars() {
            h = splitmix64(h ^ u64::from(u32::from(ch)));
        }
    }
    finalize_hash(h)
}

#[inline]
fn text_eq_ci(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

#[inline]
fn is_indexable_bits(bits: u64) -> bool {
    if bits == Value::EMPTY_BITS {
        return false;
    }
    matches!(
        Value::unpack(bits),
        Value::Number(_) | Value::Bool(_) | Value::Text(_)
    )
}

#[inline]
fn hash_cell_bits(sheet: &Sheet, bits: u64) -> Option<(u64, u64)> {
    if bits == Value::EMPTY_BITS {
        return None;
    }
    match Value::unpack(bits) {
        Value::Number(n) => {
            if !n.is_finite() {
                return None;
            }
            let canon = if n == 0.0 { 0.0 } else { n };
            let canon_bits = Value::number(canon).pack();
            Some((canon_bits, hash_number_bits(canon.to_bits())))
        }
        Value::Bool(b) => Some((bits, hash_bool(b))),
        Value::Text(id) => {
            let s = sheet.strings().get(id);
            Some((bits, hash_lower_text(s)))
        }
        Value::Empty | Value::Error(_) | Value::Array(_) => None,
    }
}

#[inline]
fn cell_bits_equal(sheet: &Sheet, a_bits: u64, b_bits: u64) -> bool {
    if a_bits == b_bits {
        return true;
    }
    match (Value::unpack(a_bits), Value::unpack(b_bits)) {
        (Value::Text(a_id), Value::Text(b_id)) => {
            text_eq_ci(sheet.strings().get(a_id), sheet.strings().get(b_id))
        }
        _ => false,
    }
}

fn for_each_lookup_word(
    sheet: &Sheet,
    column: &Column,
    row_runs: &[IdRun],
    mut visit: impl FnMut(u32, u64),
) {
    let base_bound = u64::from(sheet.row_slots().base_len());
    let blocks = column.blocks();
    let mut offset_idx: u32 = 0;

    for run in row_runs {
        let end_excl = run.first_id.saturating_add(u64::from(run.len));
        if end_excl <= base_bound
            && let Ok(mut cur) = u32::try_from(run.first_id)
        {
            let end = cur.saturating_add(run.len);
            while cur < end {
                let block_idx = (cur / BLOCK_ROWS) as usize;
                let off = (cur % BLOCK_ROWS) as usize;
                let rem = (BLOCK_ROWS as usize).saturating_sub(off);
                let take = rem.min((end.saturating_sub(cur)) as usize);
                let take_u32 = u32::try_from(take).unwrap_or(0);
                match blocks.get(block_idx) {
                    None | Some(Block::Empty) => {}
                    Some(Block::Dense(words)) => {
                        if let Some(slice) = words.get(off..off.saturating_add(take)) {
                            for (k, &bits) in slice.iter().enumerate() {
                                if bits != Value::EMPTY_BITS {
                                    visit(
                                        offset_idx.saturating_add(u32::try_from(k).unwrap_or(0)),
                                        bits,
                                    );
                                }
                            }
                        }
                    }
                    Some(Block::Sparse(_)) => {
                        for k in 0..take_u32 {
                            let bits = column.get_packed(cur.saturating_add(k));
                            if bits != Value::EMPTY_BITS {
                                visit(offset_idx.saturating_add(k), bits);
                            }
                        }
                    }
                }
                cur = cur.saturating_add(take_u32.max(1));
                offset_idx = offset_idx.saturating_add(take_u32);
            }
        } else {
            for row_id in run.iter() {
                if let Some(rs) = sheet.row_slots().get(row_id) {
                    let bits = column.get_packed(rs);
                    if bits != Value::EMPTY_BITS {
                        visit(offset_idx, bits);
                    }
                }
                offset_idx = offset_idx.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{Op, Workbook};
    use reticula_core::sheet::Sheet;
    use reticula_core::value::Value;

    use super::*;

    #[test]
    fn lazy_activation_skips_below_min_rows_and_activates_on_second_hit() {
        let mut sheet = Sheet::standard(SheetId(0), alloc::string::String::from("Sheet1"));
        for r in 0_u64..100 {
            sheet.set_value_by_id(RowId(r), ColId(0), Value::Number((r * 10) as f64));
        }

        let mut cache = LookupIndexCache::default();
        // `< MIN_INDEX_ROWS` (64) never allocates an index.
        assert!(cache.get_or_build(&sheet, ColId(0), 0, 63, 1).is_none());
        assert!(cache.get_or_build(&sheet, ColId(0), 0, 63, 1).is_none());
        assert_eq!(cache.hot_entry_count(), 0);

        // Cold 1st hit for `>= 64` rows returns `None`; 2nd hit builds and caches.
        assert!(cache.get_or_build(&sheet, ColId(0), 0, 100, 1).is_none());
        assert_eq!(cache.hot_entry_count(), 0);
        let idx = cache
            .get_or_build(&sheet, ColId(0), 0, 100, 1)
            .expect("second hit builds hash index");
        assert_eq!(idx.probe_number(500.0), Some(50));
        assert_eq!(idx.probe_number(501.0), None);
        assert_eq!(cache.hot_entry_count(), 1);
    }

    #[test]
    fn duplicate_keys_unicode_case_blanks_and_negative_zero_match_excel() {
        let mut sheet = Sheet::standard(SheetId(0), alloc::string::String::from("Sheet1"));
        for r in 0_u64..80 {
            if r == 5 {
                let id = sheet.add_string("Apple").unwrap();
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Text(id));
            } else if r == 6 {
                let id = sheet.add_string("APPLE").unwrap();
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Text(id));
            } else if r == 7 {
                let id = sheet.add_string("a*c").unwrap();
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Text(id));
            } else if r == 8 {
                let id = sheet.add_string("\u{212A}").unwrap();
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Text(id));
            } else if r == 9 {
                let id = sheet.add_string("ΚΟΣΜΟΣ").unwrap();
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Text(id));
            } else if r == 10 {
                // Leave Row 10 explicitly Empty!
            } else if r == 11 {
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Number(0.0));
            } else if r == 12 {
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Bool(true));
            } else {
                sheet.set_value_by_id(RowId(r), ColId(0), Value::Number((r + 100) as f64));
            }
        }

        let mut cache = LookupIndexCache::default();
        let idx = cache.get_or_build(&sheet, ColId(0), 0, 80, 2).unwrap();

        // First match preserved for case-insensitive duplicates.
        assert_eq!(idx.probe_text(&sheet, "apple"), Some(5));
        assert_eq!(idx.probe_text(&sheet, "APPLE"), Some(5));
        // Unicode Kelvin `\u{212A}` and Greek sigma match case-insensitively.
        assert_eq!(idx.probe_text(&sheet, "k"), Some(8));
        assert_eq!(idx.probe_text(&sheet, "κοσμος"), Some(9));
        // Wildcards bypass when allowed, and match literally when disallowed (`XLOOKUP(..., 0)`).
        assert_eq!(
            idx.probe_scalar(
                &sheet,
                &EvalScalar::Text(alloc::string::String::from("a*c")),
                true
            ),
            LookupProbeResult::Bypass
        );
        assert_eq!(
            idx.probe_scalar(
                &sheet,
                &EvalScalar::Text(alloc::string::String::from("a*c")),
                false
            ),
            LookupProbeResult::Hit(Some(7))
        );
        // Empty cells in the column never match `0.0`, while Empty needle normalizes to `0.0`.
        assert_eq!(
            idx.probe_scalar(&sheet, &EvalScalar::Empty, true),
            LookupProbeResult::Hit(Some(11))
        );
        assert_eq!(idx.probe_number(-0.0), Some(11));
        assert_eq!(idx.probe_bool(true), Some(12));
    }

    #[test]
    fn key_and_axis_mutations_invalidate_while_value_edits_keep_index_warm() {
        let mut wb = Workbook::<crate::binder::BoundFormula>::new();
        for r in 0_u64..80 {
            wb.sheet_mut(SheetId(0)).unwrap().set_value_by_id(
                RowId(r),
                ColId(0),
                Value::Number((r + 100) as f64),
            );
            wb.sheet_mut(SheetId(0)).unwrap().set_value_by_id(
                RowId(r),
                ColId(1),
                Value::Number((r * 2) as f64),
            );
        }

        let mut cache = LookupIndexCache::default();
        let _ = cache
            .get_or_build(wb.sheet(SheetId(0)).unwrap(), ColId(0), 0, 80, 2)
            .unwrap();
        let stamp0 = LookupStamp::from_sheet(wb.sheet(SheetId(0)).unwrap(), ColId(0));

        // Editing value column B leaves key column A's generation stamp intact.
        wb.sheet_mut(SheetId(0)).unwrap().set_value_by_id(
            RowId(10),
            ColId(1),
            Value::Number(9999.0),
        );
        let stamp1 = LookupStamp::from_sheet(wb.sheet(SheetId(0)).unwrap(), ColId(0));
        assert_eq!(stamp0, stamp1);

        // Editing key column A updates the index stamp and refreshes lookup results.
        wb.sheet_mut(SheetId(0)).unwrap().set_value_by_id(
            RowId(20),
            ColId(0),
            Value::Number(77_777.0),
        );
        let idx = cache
            .get_or_build(wb.sheet(SheetId(0)).unwrap(), ColId(0), 0, 80, 1)
            .unwrap();
        assert_eq!(idx.probe_number(77_777.0), Some(20));

        // Structural row insertion invalidates `row_axis_gen` and shifts offsets accurately.
        let ins = IdRun::new(0x1000_0000_0000_0000, 1).unwrap();
        let _ = wb
            .apply(Op::InsertRows {
                sheet: SheetId(0),
                after: Some(RowId(0)),
                ids: ins,
            })
            .unwrap();
        let idx_after_insert = cache
            .get_or_build(wb.sheet(SheetId(0)).unwrap(), ColId(0), 0, 80, 1)
            .unwrap();
        assert_eq!(idx_after_insert.probe_number(77_777.0), Some(21));
    }

    #[test]
    fn cache_enforces_32_mib_lru_budget() {
        let mut cache = LookupIndexCache::default();
        // 1,048,576 slots * 16 B = 16 MiB each -> 3 insertions must evict the oldest.
        let half_budget_slots = (MAX_CACHE_BYTES / 2) / size_of::<HashSlot>();
        cache.insert_synthetic_for_test(SheetId(0), ColId(0), half_budget_slots);
        cache.insert_synthetic_for_test(SheetId(0), ColId(1), half_budget_slots);
        assert_eq!(cache.hot_entry_count(), 2);
        assert_eq!(cache.total_bytes(), MAX_CACHE_BYTES);

        cache.insert_synthetic_for_test(SheetId(0), ColId(2), half_budget_slots);
        assert_eq!(cache.hot_entry_count(), 2);
        assert!(cache.total_bytes() <= MAX_CACHE_BYTES);
    }
}
