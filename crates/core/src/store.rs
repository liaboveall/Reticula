//! Identity-slotted, block-adaptive Copy-on-Write column storage (`14-m1b.md` 2.3).
//!
//! Each row or column identity maps to a dense storage slot: base-run
//! identities map directly to `id as u32` with zero table lookup, while newly
//! inserted identities receive slots appended at the end. Within a column,
//! slots are partitioned into blocks of [`BLOCK_ROWS`] (`4,096`) entries, each
//! stored as empty, sorted sparse pairs (`Arc<Vec<(u16, u64)>>`), or a fixed
//! 32 KiB dense array (`Arc<Vec<u64>>`). Wrapping non-empty blocks in [`Arc`]
//! allows background snapshot encoding (and future draft branches) to fork a
//! 50,000,000-cell workbook in `< 0.2 ms` with zero block copying while active
//! edits copy-on-write only touched blocks via [`Arc::make_mut`].

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use rustc_hash::FxHasher;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::axis::Axis;
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
    extra_ids: Vec<u64>,
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
            extra_ids: Vec::new(),
        }
    }

    /// Reconstructs a slot map from `base_len` and ordered extra identities,
    /// returning `None` if an extra identity duplicates an earlier entry or a
    /// base identity, or if slot indices overflow `u32`.
    #[must_use]
    pub fn from_parts(base_len: u32, extra_ids: Vec<u64>) -> Option<Self> {
        let extra_len = u32::try_from(extra_ids.len()).ok()?;
        let next_slot = base_len.checked_add(extra_len)?;
        let mut extra =
            ExtraSlotMap::with_capacity_and_hasher(extra_ids.len(), BuildHasherDefault::default());
        for (idx, &id) in extra_ids.iter().enumerate() {
            if is_base_id(id) && id < u64::from(base_len) {
                return None;
            }
            let offset = u32::try_from(idx).ok()?;
            let slot = base_len.checked_add(offset)?;
            if extra.insert(id, slot).is_some() {
                return None;
            }
        }
        Some(Self {
            base_len,
            next_slot,
            extra,
            extra_ids,
        })
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

    /// Non-base identities in deterministic slot allocation order
    /// (`slot = base_len + index`).
    #[must_use]
    pub fn extra_ids(&self) -> &[u64] {
        &self.extra_ids
    }

    /// Reverse lookup from dense `slot` back to its 64-bit identity in O(1).
    #[must_use]
    pub fn slot_to_id(&self, slot: u32) -> Option<u64> {
        if slot < self.base_len {
            return Some(u64::from(slot));
        }
        let idx = slot.checked_sub(self.base_len)? as usize;
        self.extra_ids.get(idx).copied()
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
        self.extra_ids.push(id);
        Some(slot)
    }

    /// Approximate heap bytes owned by the extra-slot tables.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.extra
            .capacity()
            .saturating_mul(size_of::<(u64, u32)>())
            .saturating_add(self.extra_ids.capacity().saturating_mul(size_of::<u64>()))
    }
}

/// Representation of one `4,096`-row block within a [`Column`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Block {
    /// All `4,096` slots hold [`Value::Empty`].
    #[default]
    Empty,
    /// Copy-on-write sorted `(offset, packed_bits)` entries for non-empty cells.
    Sparse(Arc<Vec<(u16, u64)>>),
    /// Copy-on-write fixed array of `4,096` packed cell words (`32 KiB`).
    Dense(Arc<Vec<u64>>),
}

#[derive(Serialize)]
enum BlockWireRef<'a> {
    Empty,
    Sparse(&'a [(u16, u64)]),
    Dense(&'a [u64]),
}

#[derive(Deserialize)]
enum BlockWireOwned {
    Empty,
    Sparse(Vec<(u16, u64)>),
    Dense(Vec<u64>),
}

impl Serialize for Block {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::Empty => BlockWireRef::Empty,
            Self::Sparse(entries) => BlockWireRef::Sparse(entries.as_slice()),
            Self::Dense(words) => BlockWireRef::Dense(words.as_slice()),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Block {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = BlockWireOwned::deserialize(deserializer)?;
        match wire {
            BlockWireOwned::Empty => Ok(Self::Empty),
            BlockWireOwned::Sparse(entries) => Ok(Self::Sparse(Arc::new(entries))),
            BlockWireOwned::Dense(words) => {
                if words.len() != BLOCK_ROWS as usize {
                    return Err(D::Error::custom("invalid dense block length"));
                }
                Ok(Self::Dense(Arc::new(words)))
            }
        }
    }
}

impl Block {
    /// Constructs a [`Block::Sparse`] from owned pairs.
    #[must_use]
    pub fn sparse(entries: Vec<(u16, u64)>) -> Self {
        Self::Sparse(Arc::new(entries))
    }

    /// Constructs a [`Block::Dense`] from a `4,096`-element word vector.
    #[must_use]
    pub fn dense(words: Vec<u64>) -> Option<Self> {
        if words.len() == BLOCK_ROWS as usize {
            Some(Self::Dense(Arc::new(words)))
        } else {
            None
        }
    }

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

    fn read_span(&self, offset: u16, out: &mut [u64]) {
        let len = out.len();
        if len == 0 {
            return;
        }
        let off_usize = usize::from(offset);
        let end_usize = off_usize.saturating_add(len);
        match self {
            Self::Empty => out.fill(Value::EMPTY_BITS),
            Self::Dense(words) => {
                if let Some(src) = words.get(off_usize..end_usize) {
                    out.copy_from_slice(src);
                } else {
                    out.fill(Value::EMPTY_BITS);
                }
            }
            Self::Sparse(entries) => {
                out.fill(Value::EMPTY_BITS);
                let start_idx =
                    entries.partition_point(|&(e_off, _)| usize::from(e_off) < off_usize);
                for &(e_off, bits) in entries.get(start_idx..).unwrap_or(&[]) {
                    let e_u = usize::from(e_off);
                    if e_u >= end_usize {
                        break;
                    }
                    if let Some(dst) = out.get_mut(e_u.saturating_sub(off_usize)) {
                        *dst = bits;
                    }
                }
            }
        }
    }

    fn write_span(
        &mut self,
        offset: u16,
        span_base_off: u32,
        src: &[u64],
        on_changed: &mut impl FnMut(u32),
    ) -> u32 {
        let len = src.len();
        if len == 0 {
            return 0;
        }
        let off_usize = usize::from(offset);
        let end_usize = off_usize.saturating_add(len);
        let mut changed = 0_u32;
        match self {
            Self::Empty => {
                let mut non_empty = 0usize;
                for (i, &w) in src.iter().enumerate() {
                    if w != Value::EMPTY_BITS {
                        non_empty = non_empty.saturating_add(1);
                        let rel = u32::try_from(i).unwrap_or(u32::MAX);
                        changed = changed.saturating_add(1);
                        on_changed(span_base_off.saturating_add(rel));
                    }
                }
                if non_empty == 0 {
                    return 0;
                }
                if non_empty > SPARSE_LIMIT {
                    let mut dense = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
                    if let Some(dst) = dense.get_mut(off_usize..end_usize) {
                        dst.copy_from_slice(src);
                    }
                    *self = Self::Dense(Arc::new(dense));
                } else {
                    let mut entries = Vec::with_capacity(non_empty);
                    for (i, &w) in src.iter().enumerate() {
                        if w != Value::EMPTY_BITS
                            && let Ok(rel_u16) = u16::try_from(off_usize.saturating_add(i))
                        {
                            entries.push((rel_u16, w));
                        }
                    }
                    *self = Self::Sparse(Arc::new(entries));
                }
            }
            Self::Dense(words_arc) => {
                let any_diff = words_arc
                    .get(off_usize..end_usize)
                    .is_some_and(|existing| existing != src);
                if !any_diff {
                    return 0;
                }
                let words = Arc::make_mut(words_arc);
                if let Some(dst) = words.get_mut(off_usize..end_usize) {
                    for (i, (old_slot, &new_w)) in dst.iter_mut().zip(src.iter()).enumerate() {
                        if *old_slot != new_w {
                            *old_slot = new_w;
                            changed = changed.saturating_add(1);
                            let rel = u32::try_from(i).unwrap_or(u32::MAX);
                            on_changed(span_base_off.saturating_add(rel));
                        }
                    }
                }
            }
            Self::Sparse(entries_arc) => {
                // Fast check for whether any entry inside or outside changes.
                let mut merged: Vec<(u16, u64)> = Vec::with_capacity(
                    entries_arc
                        .len()
                        .saturating_add(len)
                        .min(BLOCK_ROWS as usize),
                );
                let mut e_idx = 0usize;
                let e_slice = entries_arc.as_slice();
                while let Some(&(e_off, bits)) = e_slice.get(e_idx) {
                    if usize::from(e_off) >= off_usize {
                        break;
                    }
                    merged.push((e_off, bits));
                    e_idx = e_idx.saturating_add(1);
                }
                for (i, &new_w) in src.iter().enumerate() {
                    let cur_off_usize = off_usize.saturating_add(i);
                    let cur_off = u16::try_from(cur_off_usize).unwrap_or(u16::MAX);
                    let mut old_w = Value::EMPTY_BITS;
                    if let Some(&(e_off, e_bits)) = e_slice.get(e_idx)
                        && e_off == cur_off
                    {
                        old_w = e_bits;
                        e_idx = e_idx.saturating_add(1);
                    }
                    if old_w != new_w {
                        changed = changed.saturating_add(1);
                        let rel = u32::try_from(i).unwrap_or(u32::MAX);
                        on_changed(span_base_off.saturating_add(rel));
                    }
                    if new_w != Value::EMPTY_BITS {
                        merged.push((cur_off, new_w));
                    }
                }
                for &(e_off, bits) in e_slice.get(e_idx..).unwrap_or(&[]) {
                    merged.push((e_off, bits));
                }
                if changed == 0 {
                    return 0;
                }
                if merged.is_empty() {
                    *self = Self::Empty;
                } else if merged.len() > SPARSE_LIMIT {
                    let mut dense = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
                    for (e_off, bits) in merged {
                        if let Some(cell) = dense.get_mut(usize::from(e_off)) {
                            *cell = bits;
                        }
                    }
                    *self = Self::Dense(Arc::new(dense));
                } else {
                    *entries_arc = Arc::new(merged);
                }
            }
        }
        changed
    }

    fn set(&mut self, offset: u16, bits: u64) {
        let is_empty = bits == Value::EMPTY_BITS;
        match self {
            Self::Empty => {
                if !is_empty {
                    *self = Self::Sparse(Arc::new(vec![(offset, bits)]));
                }
            }
            Self::Sparse(entries_arc) => {
                let entries = Arc::make_mut(entries_arc);
                match entries.binary_search_by_key(&offset, |&(off, _)| off) {
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
                            *self = Self::Dense(Arc::new(dense));
                        } else {
                            entries.insert(idx, (offset, bits));
                        }
                    }
                    Err(_) => {}
                }
            }
            Self::Dense(words_arc) => {
                let words = Arc::make_mut(words_arc);
                if let Some(cell) = words.get_mut(offset as usize) {
                    *cell = bits;
                }
            }
        }
    }

    fn remap_strings(&mut self, remap: &[u32]) {
        match self {
            Self::Empty => {}
            Self::Sparse(entries_arc) => {
                let has_text = entries_arc
                    .iter()
                    .any(|&(_, bits)| matches!(Value::unpack(bits), Value::Text(_)));
                if !has_text {
                    return;
                }
                let entries = Arc::make_mut(entries_arc);
                for (_, bits) in entries.iter_mut() {
                    if let Value::Text(old_id) = Value::unpack(*bits)
                        && let Some(&new_id) = remap.get(old_id as usize)
                    {
                        *bits = Value::Text(new_id).pack();
                    }
                }
            }
            Self::Dense(words_arc) => {
                let has_text = words_arc
                    .iter()
                    .any(|&bits| matches!(Value::unpack(bits), Value::Text(_)));
                if !has_text {
                    return;
                }
                let words = Arc::make_mut(words_arc);
                for bits in words.iter_mut() {
                    if let Value::Text(old_id) = Value::unpack(*bits)
                        && let Some(&new_id) = remap.get(old_id as usize)
                    {
                        *bits = Value::Text(new_id).pack();
                    }
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

    /// Clears `slot` back to [`Value::Empty`] (used during tombstone GC).
    pub fn clear_slot(&mut self, slot: u32) {
        let _ = self.set(slot, Value::Empty);
    }

    /// Remaps [`Value::Text`] indices across all blocks using `remap[old_id]`.
    pub fn remap_strings(&mut self, remap: &[u32]) {
        for block in &mut self.blocks {
            block.remap_strings(remap);
        }
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

    /// Reads contiguous packed 64-bit words for slots `[start_slot .. start_slot + out.len())`
    /// block-by-block into `out`.
    pub fn read_slot_span_packed(&self, start_slot: u32, out: &mut [u64]) {
        let total = out.len();
        if total == 0 {
            return;
        }
        let mut done = 0usize;
        while done < total {
            let Some(rel_u32) = u32::try_from(done).ok() else {
                break;
            };
            let Some(cur_slot) = start_slot.checked_add(rel_u32) else {
                if let Some(tail) = out.get_mut(done..) {
                    tail.fill(Value::EMPTY_BITS);
                }
                break;
            };
            let (block_idx, offset) = locate(cur_slot);
            let rem_in_block = (BLOCK_ROWS as usize).saturating_sub(usize::from(offset));
            let take = rem_in_block.min(total.saturating_sub(done));
            if let Some(dst) = out.get_mut(done..done.saturating_add(take)) {
                match self.blocks.get(block_idx) {
                    None => dst.fill(Value::EMPTY_BITS),
                    Some(block) => block.read_span(offset, dst),
                }
            }
            done = done.saturating_add(take);
        }
    }

    /// Writes contiguous packed 64-bit words from `src` into slots
    /// `[start_slot .. start_slot + src.len())`, returning changed slot count
    /// and invoking `on_changed(offset_in_src)` for each modified cell.
    pub fn write_slot_span_packed(
        &mut self,
        start_slot: u32,
        src: &[u64],
        mut on_changed: impl FnMut(u32),
    ) -> u32 {
        let total = src.len();
        if total == 0 {
            return 0;
        }
        let mut changed_total = 0_u32;
        let mut done = 0usize;
        let mut max_after = self.len;
        while done < total {
            let Ok(rel_u32) = u32::try_from(done) else {
                break;
            };
            let Some(cur_slot) = start_slot.checked_add(rel_u32) else {
                break;
            };
            let (block_idx, offset) = locate(cur_slot);
            let rem_in_block = (BLOCK_ROWS as usize).saturating_sub(usize::from(offset));
            let take = rem_in_block.min(total.saturating_sub(done));
            let Some(src_slice) = src.get(done..done.saturating_add(take)) else {
                break;
            };
            if block_idx >= self.blocks.len() {
                if src_slice.iter().any(|&w| w != Value::EMPTY_BITS) {
                    self.blocks
                        .resize_with(block_idx.saturating_add(1), || Block::Empty);
                    if let Some(block) = self.blocks.get_mut(block_idx) {
                        let c = block.write_span(offset, rel_u32, src_slice, &mut on_changed);
                        changed_total = changed_total.saturating_add(c);
                    }
                    if let Some(take_u32) = u32::try_from(take).ok()
                        && let Some(after) = cur_slot.checked_add(take_u32)
                    {
                        max_after = max_after.max(after);
                    }
                }
            } else if let Some(block) = self.blocks.get_mut(block_idx) {
                let c = block.write_span(offset, rel_u32, src_slice, &mut on_changed);
                changed_total = changed_total.saturating_add(c);
                if src_slice.iter().any(|&w| w != Value::EMPTY_BITS)
                    && let Some(take_u32) = u32::try_from(take).ok()
                    && let Some(after) = cur_slot.checked_add(take_u32)
                {
                    max_after = max_after.max(after);
                }
            }
            done = done.saturating_add(take);
        }
        self.len = max_after;
        changed_total
    }

    /// Reconstructs a column from decoded blocks and slot bound, validating
    /// every block's internal invariants and every stored word via `check_word`.
    pub fn try_from_blocks(
        blocks: Vec<Block>,
        len: u32,
        mut check_word: impl FnMut(u64) -> bool,
    ) -> Option<Self> {
        let max_blocks = len.div_ceil(BLOCK_ROWS) as usize;
        if blocks.len() > max_blocks {
            return None;
        }
        for block in &blocks {
            match block {
                Block::Empty => {}
                Block::Sparse(entries) => {
                    if entries.is_empty() || entries.len() > SPARSE_LIMIT {
                        return None;
                    }
                    let mut prev_offset: Option<u16> = None;
                    for &(offset, bits) in entries.iter() {
                        if u32::from(offset) >= BLOCK_ROWS || bits == Value::EMPTY_BITS {
                            return None;
                        }
                        if let Some(prev) = prev_offset
                            && offset <= prev
                        {
                            return None;
                        }
                        if !check_word(bits) {
                            return None;
                        }
                        prev_offset = Some(offset);
                    }
                }
                Block::Dense(words) => {
                    if words.len() != BLOCK_ROWS as usize {
                        return None;
                    }
                    for &bits in words.iter() {
                        if !check_word(bits) {
                            return None;
                        }
                    }
                }
            }
        }
        Some(Self { blocks, len })
    }

    /// Visits every non-empty `(slot, Value)` pair in ascending slot order.
    pub fn for_each_non_empty(&self, mut visit: impl FnMut(u32, Value)) {
        for (block_idx, block) in self.blocks.iter().enumerate() {
            let Some(block_idx_u32) = u32::try_from(block_idx).ok() else {
                break;
            };
            let Some(base_slot) = block_idx_u32.checked_mul(BLOCK_ROWS) else {
                break;
            };
            match block {
                Block::Empty => {}
                Block::Sparse(entries) => {
                    for &(offset, bits) in entries.iter() {
                        if let Some(slot) = base_slot.checked_add(u32::from(offset)) {
                            visit(slot, Value::unpack(bits));
                        }
                    }
                }
                Block::Dense(words) => {
                    for (offset, &bits) in words.iter().enumerate() {
                        if bits != Value::EMPTY_BITS
                            && let Some(off_u32) = u32::try_from(offset).ok()
                            && let Some(slot) = base_slot.checked_add(off_u32)
                        {
                            visit(slot, Value::unpack(bits));
                        }
                    }
                }
            }
        }
    }

    /// Computes Excel-compatible `Ctrl+Down` (`forward = true`) or `Ctrl+Up`
    /// (`forward = false`) target visible row position along `row_axis`.
    #[must_use]
    pub fn jump_vertical(
        &self,
        row_axis: &Axis,
        row_slots: &SlotMap,
        start_pos: u32,
        forward: bool,
    ) -> u32 {
        let total = row_axis.live_len();
        if total == 0 {
            return 0;
        }
        let cur = start_pos.min(total.saturating_sub(1));
        let last = total.saturating_sub(1);
        if (forward && cur >= last) || (!forward && cur == 0) {
            return cur;
        }

        let is_non_empty = |pos: u32| -> bool {
            row_axis
                .pos_to_id(pos)
                .and_then(|id| row_slots.get(id))
                .is_some_and(|slot| self.get_packed(slot) != Value::EMPTY_BITS)
        };

        let next_pos = if forward {
            cur.saturating_add(1)
        } else {
            cur.saturating_sub(1)
        };
        let cur_filled = is_non_empty(cur);
        let next_filled = is_non_empty(next_pos);

        if cur_filled && next_filled {
            // Walk to the last consecutive non-empty cell in the current run.
            if forward {
                let mut p = next_pos;
                while p < last && is_non_empty(p.saturating_add(1)) {
                    p = p.saturating_add(1);
                }
                p
            } else {
                let mut p = next_pos;
                while p > 0 && is_non_empty(p.saturating_sub(1)) {
                    p = p.saturating_sub(1);
                }
                p
            }
        } else {
            // Walk across empty cells to the first non-empty cell, or sheet edge.
            // Fast-skip when the column has no non-empty blocks at all!
            if self.is_empty() {
                return if forward { last } else { 0 };
            }
            if forward {
                let mut p = next_pos;
                while p <= last {
                    if is_non_empty(p) {
                        return p;
                    }
                    if p == last {
                        break;
                    }
                    p = p.saturating_add(1);
                }
                last
            } else {
                let mut p = next_pos;
                loop {
                    if is_non_empty(p) {
                        return p;
                    }
                    if p == 0 {
                        return 0;
                    }
                    p = p.saturating_sub(1);
                }
            }
        }
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

    use alloc::vec;
    use alloc::vec::Vec;

    use super::{BLOCK_ROWS, Block, Column, SPARSE_LIMIT, SlotMap};
    use crate::axis::Axis;
    use crate::id::pack_id;
    use crate::value::Value;

    #[test]
    fn rows_are_read_back_across_blocks_and_cow_isolates_clones() {
        let mut column = Column::new();
        assert!(column.is_empty());
        let rows = BLOCK_ROWS * 2 + 3;
        for row in 0..rows {
            assert!(column.push(Value::Number(f64::from(row))));
        }
        assert_eq!(column.len(), rows);

        // Clone shares Arc blocks; mutating `column` leaves `forked` untouched!
        let forked = column.clone();
        assert!(column.set(5, Value::Number(9999.0)));
        assert_eq!(column.get(5), Value::Number(9999.0));
        assert_eq!(forked.get(5), Value::Number(5.0));

        for row in [0, 1, BLOCK_ROWS - 1, BLOCK_ROWS, BLOCK_ROWS * 2, rows - 1] {
            assert_eq!(forked.get(row), Value::Number(f64::from(row)));
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
        assert!(column.estimated_bytes() < 8192);

        assert!(column.set(999_999, Value::Empty));
        assert_eq!(column.get(999_999), Value::Empty);

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
    fn slot_map_and_ctrl_arrow_jump_behave_like_excel() {
        let mut map = SlotMap::with_base(1_048_576);
        assert_eq!(map.get(0), Some(0));
        assert_eq!(map.get(999_999), Some(999_999));
        let Some(inserted) = pack_id(0, 1, 1) else {
            panic!("id");
        };
        assert_eq!(map.get(inserted), None);
        assert_eq!(map.get_or_alloc(inserted), Some(1_048_576));
        assert_eq!(map.get(inserted), Some(1_048_576));

        let axis = Axis::with_base(20, 100);
        let slots = SlotMap::with_base(100);
        let mut col = Column::new();
        for r in [2_u32, 3, 4, 10, 11] {
            assert!(col.set(r, Value::Number(1.0)));
        }
        assert_eq!(col.jump_vertical(&axis, &slots, 0, true), 2);
        assert_eq!(col.jump_vertical(&axis, &slots, 2, true), 4);
        assert_eq!(col.jump_vertical(&axis, &slots, 4, true), 10);
        assert_eq!(col.jump_vertical(&axis, &slots, 10, true), 11);
        assert_eq!(col.jump_vertical(&axis, &slots, 11, true), 19);
        assert_eq!(col.jump_vertical(&axis, &slots, 19, false), 11);
        assert_eq!(col.jump_vertical(&axis, &slots, 11, false), 10);
        assert_eq!(col.jump_vertical(&axis, &slots, 10, false), 4);
        assert_eq!(col.jump_vertical(&axis, &slots, 4, false), 2);
        assert_eq!(col.jump_vertical(&axis, &slots, 2, false), 0);
    }

    #[test]
    fn span_read_write_preserves_cow_and_matches_scalar_access() {
        let mut col = Column::new();
        let mut words = Vec::with_capacity(5000);
        for i in 0_u32..5000 {
            words.push(if i % 10 == 0 {
                Value::EMPTY_BITS
            } else {
                Value::Number(f64::from(i) * 1.25).pack()
            });
        }
        let mut changed_slots = Vec::new();
        let changed = col.write_slot_span_packed(10, &words, |off| changed_slots.push(off));
        assert_eq!(changed as usize, changed_slots.len());
        assert_eq!(changed, 4500);
        assert_eq!(col.len(), 5010);

        let mut forked = col.clone();
        // Writing identical words reports 0 changes (preserving CoW sharing).
        let noop_changed = forked.write_slot_span_packed(10, &words, |_| {
            panic!("identical span write should not report changes")
        });
        assert_eq!(noop_changed, 0);

        // Sparse patch across empty/sparse block.
        let mut sparse_col = Column::new();
        let patch = [
            Value::Number(42.0).pack(),
            Value::EMPTY_BITS,
            Value::Bool(true).pack(),
        ];
        assert_eq!(sparse_col.write_slot_span_packed(4095, &patch, |_| {}), 2);
        let mut round_trip = vec![0_u64; 5];
        sparse_col.read_slot_span_packed(4094, &mut round_trip);
        assert_eq!(
            round_trip,
            vec![
                Value::EMPTY_BITS,
                Value::Number(42.0).pack(),
                Value::EMPTY_BITS,
                Value::Bool(true).pack(),
                Value::EMPTY_BITS,
            ]
        );

        let mut out = vec![0_u64; 5000];
        col.read_slot_span_packed(10, &mut out);
        assert_eq!(out, words);
        for (i, &w) in words.iter().enumerate() {
            assert_eq!(
                col.get_packed(10_u32.saturating_add(u32::try_from(i).unwrap_or(0))),
                w
            );
        }
    }
}
