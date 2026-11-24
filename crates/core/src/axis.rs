//! Order-statistic B+ tree axis of stable row or column identities.
//!
//! A fresh worksheet begins with one implicit run of `1,048,576` rows (or
//! `16,384` columns), costing under 200 bytes. Insertions split at most one
//! leaf run and update B+ tree subtree aggregates (`total_entries`,
//! `live_count`, `pixel_sum`) along the root path in `O(log R)` without
//! full-axis index rebuilds. Deletions increment per-entry tombstone counters
//! rather than shifting cell coordinates, so range anchors and concurrent undo
//! behave deterministically.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ops::Bound::{Excluded, Unbounded};

use serde::{Deserialize, Serialize};

use crate::id::{IdRun, id_counter, id_stream};

/// Default initial visible row count of a new sheet (same as Excel).
pub const DEFAULT_BASE_ROWS: u32 = 1_048_576;

/// Default initial visible column count of a new sheet (same as Excel).
pub const DEFAULT_BASE_COLS: u32 = 16_384;

/// Hard upper bound on total entries (live plus tombstones) on a row axis.
pub const MAX_ROW_AXIS_LEN: u32 = 2_000_000;

/// Hard upper bound on total entries (live plus tombstones) on a column axis.
pub const MAX_COL_AXIS_LEN: u32 = 16_384;

/// Default per-entry pixel size used when constructing a standalone `Axis`
/// without explicit sheet defaults (`DEFAULT_ROW_HEIGHT`).
const DEFAULT_AXIS_ENTRY_PX: u16 = 24;

/// Order-statistic B+ tree node branching limits (`B = 16`).
const BTREE_MIN_DEGREE: usize = 16;
const LEAF_MAX: usize = 32;
const LEAF_MIN: usize = BTREE_MIN_DEGREE;
const INTERNAL_MAX: usize = 32;
const INTERNAL_MIN: usize = BTREE_MIN_DEGREE;

/// Why an axis mutation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisError {
    /// A run had length zero or wrapped across replica streams.
    InvalidRun,
    /// Total entries on the axis would exceed `max_entries`.
    LimitExceeded,
    /// `after` named an identity that does not exist on the axis.
    AnchorNotFound,
    /// One or more inserted identities already exist on the axis.
    DuplicateId,
    /// Target identity for deletion, undeletion or removal was not found.
    TargetNotFound,
    /// Undeleting an entry whose deletion count is already zero, or removing an
    /// entry that does not match an exact inserted run.
    InvalidInverse,
    /// Moving entries to a position anchored inside the moved set itself.
    InvalidMove,
}

/// One contiguous run of identities in axis order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisRun {
    /// Contiguous identity range `[first_id, first_id + len)`.
    pub ids: IdRun,
    /// Number of entries in this run whose tombstone count is zero.
    pub live_count: u32,
    /// Per-entry deletion counts (`0` = live). `None` when every entry in the
    /// run is live, avoiding heap allocation on untouched base runs.
    pub tombstones: Option<Box<[u32]>>,
}

impl AxisRun {
    /// Creates a fully live run.
    #[must_use]
    pub const fn live(ids: IdRun) -> Self {
        Self {
            live_count: ids.len,
            ids,
            tombstones: None,
        }
    }

    /// Deletion count of `offset` (`0` if live or out of bounds).
    #[must_use]
    pub fn tombstone_at(&self, offset: u32) -> u32 {
        self.tombstones
            .as_ref()
            .and_then(|counts| counts.get(offset as usize))
            .copied()
            .unwrap_or(0)
    }

    /// Splits this run at `left_len` (`0 < left_len < self.ids.len`).
    #[must_use]
    pub fn split_at(&self, left_len: u32) -> Option<(Self, Self)> {
        let (left_ids, right_ids) = self.ids.split_at(left_len)?;
        match &self.tombstones {
            None => Some((Self::live(left_ids), Self::live(right_ids))),
            Some(counts) => {
                let mid = left_len as usize;
                let left_slice = counts.get(..mid)?;
                let right_slice = counts.get(mid..)?;
                Some((
                    Self::from_tombstones(left_ids, left_slice),
                    Self::from_tombstones(right_ids, right_slice),
                ))
            }
        }
    }

    /// Slices `[start_offset, end_offset)` (`0 <= start_offset < end_offset <= len`)
    /// out of this run, preserving tombstone counts.
    #[must_use]
    pub fn slice(&self, start_offset: u32, end_offset: u32) -> Option<Self> {
        if start_offset >= end_offset || end_offset > self.ids.len {
            return None;
        }
        let len = end_offset.checked_sub(start_offset)?;
        let first_id = self.ids.id_at(start_offset)?;
        let sub_ids = IdRun::new(first_id, len)?;
        match &self.tombstones {
            None => Some(Self::live(sub_ids)),
            Some(counts) => {
                let slice = counts.get(start_offset as usize..end_offset as usize)?;
                Some(Self::from_tombstones(sub_ids, slice))
            }
        }
    }

    fn from_tombstones(ids: IdRun, counts: &[u32]) -> Self {
        let mut live_count: u32 = 0;
        let mut any_deleted = false;
        for count in counts {
            if *count == 0 {
                live_count = live_count.saturating_add(1);
            } else {
                any_deleted = true;
            }
        }
        let tombstones = if any_deleted {
            Some(counts.to_vec().into_boxed_slice())
        } else {
            None
        };
        Self {
            ids,
            live_count,
            tombstones,
        }
    }

    fn live_before(&self, offset: u32) -> u32 {
        match &self.tombstones {
            None => offset.min(self.ids.len),
            Some(counts) => {
                let limit = (offset as usize).min(counts.len());
                let Some(prefix) = counts.get(..limit) else {
                    return 0;
                };
                let mut live: u32 = 0;
                for count in prefix {
                    if *count == 0 {
                        live = live.saturating_add(1);
                    }
                }
                live
            }
        }
    }

    fn live_in_range(&self, start_offset: u32, end_offset: u32) -> u32 {
        if start_offset >= end_offset {
            return 0;
        }
        let start = start_offset.min(self.ids.len);
        let end = end_offset.min(self.ids.len);
        if start >= end {
            return 0;
        }
        match &self.tombstones {
            None => end.saturating_sub(start),
            Some(counts) => {
                let Some(slice) = counts.get(start as usize..end as usize) else {
                    return 0;
                };
                let mut live: u32 = 0;
                for count in slice {
                    if *count == 0 {
                        live = live.saturating_add(1);
                    }
                }
                live
            }
        }
    }

    fn offset_of_live(&self, nth_live: u32) -> Option<u32> {
        if nth_live >= self.live_count {
            return None;
        }
        match &self.tombstones {
            None => Some(nth_live),
            Some(counts) => {
                let mut seen: u32 = 0;
                for (idx, count) in counts.iter().enumerate() {
                    if *count == 0 {
                        if seen == nth_live {
                            return u32::try_from(idx).ok();
                        }
                        seen = seen.saturating_add(1);
                    }
                }
                None
            }
        }
    }

    fn estimated_bytes(&self) -> usize {
        self.tombstones
            .as_ref()
            .map_or(0, |counts| counts.len().saturating_mul(size_of::<u32>()))
    }
}

/// Internal wrapper augmenting [`AxisRun`] with sparse custom pixel overrides
/// and cached live pixel total without altering persisted [`AxisRun`] layout.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LeafRun {
    run: AxisRun,
    /// Sorted by 0-based `offset` in `0..run.ids.len`; only entries differing
    /// from `Axis::default_px` are stored.
    custom_px: Option<Vec<(u32, u16)>>,
    pixel_sum: u64,
}

impl LeafRun {
    fn from_run(run: AxisRun, default_px: u16) -> Self {
        let pixel_sum = u64::from(run.live_count).saturating_mul(u64::from(default_px));
        Self {
            run,
            custom_px: None,
            pixel_sum,
        }
    }

    fn with_custom(run: AxisRun, custom_px: Option<Vec<(u32, u16)>>, default_px: u16) -> Self {
        let mut lr = Self {
            run,
            custom_px: custom_px.filter(|v| !v.is_empty()),
            pixel_sum: 0,
        };
        lr.recompute_pixel_sum(default_px);
        lr
    }

    fn entry_px(&self, offset: u32, default_px: u16) -> u16 {
        let Some(entries) = &self.custom_px else {
            return default_px;
        };
        match entries.binary_search_by_key(&offset, |&(off, _)| off) {
            Ok(idx) => entries.get(idx).map_or(default_px, |&(_, px)| px),
            Err(_) => default_px,
        }
    }

    fn set_entry_px(&mut self, offset: u32, px: Option<u16>, default_px: u16) {
        let desired = px.filter(|&v| v != default_px);
        let old_px = self.entry_px(offset, default_px);
        let is_live = self.run.tombstone_at(offset) == 0;
        let new_px = desired.unwrap_or(default_px);
        if is_live && old_px != new_px {
            self.pixel_sum = self
                .pixel_sum
                .saturating_sub(u64::from(old_px))
                .saturating_add(u64::from(new_px));
        }

        match desired {
            None => {
                if let Some(entries) = &mut self.custom_px {
                    if let Ok(idx) = entries.binary_search_by_key(&offset, |&(o, _)| o) {
                        entries.remove(idx);
                    }
                    if entries.is_empty() {
                        self.custom_px = None;
                    }
                }
            }
            Some(val) => {
                let entries = self.custom_px.get_or_insert_with(Vec::new);
                match entries.binary_search_by_key(&offset, |&(o, _)| o) {
                    Ok(idx) => {
                        if let Some(slot) = entries.get_mut(idx) {
                            slot.1 = val;
                        }
                    }
                    Err(idx) => entries.insert(idx, (offset, val)),
                }
            }
        }
    }

    fn recompute_pixel_sum(&mut self, default_px: u16) {
        let base = i128::from(u64::from(self.run.live_count).saturating_mul(u64::from(default_px)));
        let mut sum = base;
        if let Some(entries) = &self.custom_px {
            for &(off, px) in entries {
                if self.run.tombstone_at(off) == 0 {
                    sum = sum
                        .saturating_sub(i128::from(default_px))
                        .saturating_add(i128::from(px));
                }
            }
        }
        self.pixel_sum = u64::try_from(sum.max(0)).unwrap_or(u64::MAX);
    }

    fn split_at(&self, left_len: u32, default_px: u16) -> Option<(Self, Self)> {
        let (left_run, right_run) = self.run.split_at(left_len)?;
        match &self.custom_px {
            None => Some((
                Self::from_run(left_run, default_px),
                Self::from_run(right_run, default_px),
            )),
            Some(entries) => {
                let mid = entries.partition_point(|&(off, _)| off < left_len);
                let left_vec = entries.get(..mid).map(<[(u32, u16)]>::to_vec);
                let right_vec = entries.get(mid..).map(|tail| {
                    tail.iter()
                        .map(|&(off, px)| (off.saturating_sub(left_len), px))
                        .collect()
                });
                Some((
                    Self::with_custom(left_run, left_vec, default_px),
                    Self::with_custom(right_run, right_vec, default_px),
                ))
            }
        }
    }

    fn slice(&self, start_offset: u32, end_offset: u32, default_px: u16) -> Option<Self> {
        let sub_run = self.run.slice(start_offset, end_offset)?;
        let custom = self.custom_px.as_ref().map(|entries| {
            let lo = entries.partition_point(|&(off, _)| off < start_offset);
            let hi = entries.partition_point(|&(off, _)| off < end_offset);
            entries
                .get(lo..hi)
                .unwrap_or(&[])
                .iter()
                .map(|&(off, px)| (off.saturating_sub(start_offset), px))
                .collect::<Vec<_>>()
        });
        Some(Self::with_custom(sub_run, custom, default_px))
    }

    fn can_coalesce_with(&self, right: &Self) -> bool {
        self.run.tombstones.is_none()
            && right.run.tombstones.is_none()
            && id_stream(self.run.ids.first_id) == id_stream(right.run.ids.first_id)
            && id_counter(self.run.ids.first_id).checked_add(self.run.ids.len)
                == Some(id_counter(right.run.ids.first_id))
    }

    fn absorb_right(&mut self, right: Self) {
        let left_len = self.run.ids.len;
        let combined_len = left_len.saturating_add(right.run.ids.len);
        if let Some(merged) = IdRun::new(self.run.ids.first_id, combined_len) {
            self.run = AxisRun::live(merged);
        }
        self.pixel_sum = self.pixel_sum.saturating_add(right.pixel_sum);
        if let Some(right_custom) = right.custom_px {
            let entries = self.custom_px.get_or_insert_with(Vec::new);
            entries.reserve(right_custom.len());
            for (off, px) in right_custom {
                entries.push((left_len.saturating_add(off), px));
            }
        }
    }

    fn estimated_bytes(&self) -> usize {
        self.run.estimated_bytes().saturating_add(
            self.custom_px
                .as_ref()
                .map_or(0, |v| v.capacity().saturating_mul(size_of::<(u32, u16)>())),
        )
    }

    /// Resolves 0-based live offset `within_live` to its pixel start offset inside this run.
    fn pixel_offset_of_live(&self, within_live: u32, default_px: u16) -> Option<u64> {
        if within_live >= self.run.live_count {
            return None;
        }
        let base = u64::from(within_live).saturating_mul(u64::from(default_px));
        let Some(entries) = &self.custom_px else {
            return Some(base);
        };
        let target_off = self.run.offset_of_live(within_live)?;
        let mut sum = i128::from(base);
        for &(off, px) in entries {
            if off >= target_off {
                break;
            }
            if self.run.tombstone_at(off) == 0 {
                sum = sum
                    .saturating_sub(i128::from(default_px))
                    .saturating_add(i128::from(px));
            }
        }
        u64::try_from(sum.max(0)).ok()
    }

    /// Resolves pixel coordinate `px_in_run < self.pixel_sum` to `(within_live, start_px, entry_px)`.
    fn live_at_pixel(&self, mut px_in_run: u64, default_px: u16) -> Option<(u32, u64, u16)> {
        if px_in_run >= self.pixel_sum || default_px == 0 {
            return None;
        }
        let Some(entries) = &self.custom_px else {
            let def_u64 = u64::from(default_px);
            let within_live = u32::try_from(px_in_run.checked_div(def_u64)?).ok()?;
            let start_px = u64::from(within_live).saturating_mul(def_u64);
            return Some((within_live, start_px, default_px));
        };

        let def_u64 = u64::from(default_px);
        let mut cursor_off: u32 = 0;
        let mut live_acc: u32 = 0;
        let mut px_acc: u64 = 0;

        for &(off, px) in entries {
            if off > cursor_off {
                let gap_live = self.run.live_in_range(cursor_off, off);
                let gap_px = u64::from(gap_live).saturating_mul(def_u64);
                if px_in_run < gap_px {
                    let step = u32::try_from(px_in_run.checked_div(def_u64)?).ok()?;
                    let start_px = px_acc.saturating_add(u64::from(step).saturating_mul(def_u64));
                    return Some((live_acc.saturating_add(step), start_px, default_px));
                }
                px_in_run = px_in_run.saturating_sub(gap_px);
                px_acc = px_acc.saturating_add(gap_px);
                live_acc = live_acc.saturating_add(gap_live);
            }
            if self.run.tombstone_at(off) == 0 {
                let entry_u64 = u64::from(px);
                if px_in_run < entry_u64 {
                    return Some((live_acc, px_acc, px));
                }
                px_in_run = px_in_run.saturating_sub(entry_u64);
                px_acc = px_acc.saturating_add(entry_u64);
                live_acc = live_acc.saturating_add(1);
            }
            cursor_off = off.saturating_add(1);
        }

        let tail_live = self.run.live_in_range(cursor_off, self.run.ids.len);
        let tail_px = u64::from(tail_live).saturating_mul(def_u64);
        if px_in_run < tail_px {
            let step = u32::try_from(px_in_run.checked_div(def_u64)?).ok()?;
            let start_px = px_acc.saturating_add(u64::from(step).saturating_mul(def_u64));
            return Some((live_acc.saturating_add(step), start_px, default_px));
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChildSlot {
    child: u32,
    total_entries: u32,
    live_count: u32,
    pixel_sum: u64,
}

#[derive(Clone, Debug)]
enum NodeKind {
    Internal(Vec<ChildSlot>),
    Leaf {
        prev: Option<u32>,
        next: Option<u32>,
        runs: Vec<LeafRun>,
    },
}

#[derive(Clone, Debug)]
struct BNode {
    parent: Option<u32>,
    kind: NodeKind,
}

impl BNode {
    fn summary(&self, self_id: u32) -> ChildSlot {
        let mut total_entries = 0_u32;
        let mut live_count = 0_u32;
        let mut pixel_sum = 0_u64;
        match &self.kind {
            NodeKind::Internal(children) => {
                for ch in children {
                    total_entries = total_entries.saturating_add(ch.total_entries);
                    live_count = live_count.saturating_add(ch.live_count);
                    pixel_sum = pixel_sum.saturating_add(ch.pixel_sum);
                }
            }
            NodeKind::Leaf { runs, .. } => {
                for lr in runs {
                    total_entries = total_entries.saturating_add(lr.run.ids.len);
                    live_count = live_count.saturating_add(lr.run.live_count);
                    pixel_sum = pixel_sum.saturating_add(lr.pixel_sum);
                }
            }
        }
        ChildSlot {
            child: self_id,
            total_entries,
            live_count,
            pixel_sum,
        }
    }

    fn child_len(&self) -> usize {
        match &self.kind {
            NodeKind::Internal(children) => children.len(),
            NodeKind::Leaf { runs, .. } => runs.len(),
        }
    }

    fn estimated_bytes(&self) -> usize {
        match &self.kind {
            NodeKind::Internal(children) => {
                children.capacity().saturating_mul(size_of::<ChildSlot>())
            }
            NodeKind::Leaf { runs, .. } => runs
                .capacity()
                .saturating_mul(size_of::<LeafRun>())
                .saturating_add(
                    runs.iter()
                        .fold(0usize, |acc, r| acc.saturating_add(r.estimated_bytes())),
                ),
        }
    }
}

/// Ordered axis of rows or columns backed by an order-statistic B+ tree.
#[derive(Clone, Debug)]
pub struct Axis {
    nodes: Vec<Option<BNode>>,
    free_nodes: Vec<u32>,
    root: Option<u32>,
    first_leaf: Option<u32>,
    last_leaf: Option<u32>,
    /// Sorted map from each run's `first_id` to the `leaf_id` holding it.
    id_map: BTreeMap<u64, u32>,
    run_count: usize,
    total_live: u32,
    total_entries: u32,
    total_pixels: u64,
    default_px: u16,
    max_entries: u32,
    generation: u64,
}

#[derive(Clone, Copy)]
enum CoalesceStep {
    IntraLeaf(usize),
    CrossLeaf { next_lid: u32, right_first_id: u64 },
}

impl PartialEq for Axis {
    fn eq(&self, other: &Self) -> bool {
        self.total_live == other.total_live
            && self.total_entries == other.total_entries
            && self.total_pixels == other.total_pixels
            && self.default_px == other.default_px
            && self.max_entries == other.max_entries
            && self.run_count == other.run_count
            && self.runs().eq(other.runs())
    }
}

impl Eq for Axis {}

impl Default for Axis {
    fn default() -> Self {
        Self::empty(MAX_ROW_AXIS_LEN)
    }
}

/// Ordered double-ended iterator over [`AxisRun`]s in axis order.
#[derive(Clone, Debug)]
pub struct AxisRunsIter<'a> {
    axis: &'a Axis,
    front_leaf: Option<u32>,
    front_idx: usize,
    back_leaf: Option<u32>,
    back_idx: usize,
    remaining: usize,
}

impl<'a> AxisRunsIter<'a> {
    /// Returns a clone of this iterator (compatibility shim for callers invoking
    /// `.runs().iter()`).
    #[must_use]
    pub fn iter(&self) -> Self {
        self.clone()
    }
}

impl<'a> Iterator for AxisRunsIter<'a> {
    type Item = &'a AxisRun;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let leaf_id = self.front_leaf?;
            let Some(Some(BNode {
                kind: NodeKind::Leaf { next, runs, .. },
                ..
            })) = self.axis.nodes.get(leaf_id as usize)
            else {
                self.remaining = 0;
                return None;
            };
            if let Some(lr) = runs.get(self.front_idx) {
                self.front_idx = self.front_idx.saturating_add(1);
                self.remaining = self.remaining.saturating_sub(1);
                return Some(&lr.run);
            }
            self.front_leaf = *next;
            self.front_idx = 0;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for AxisRunsIter<'_> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl DoubleEndedIterator for AxisRunsIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let leaf_id = self.back_leaf?;
            let Some(Some(BNode {
                kind: NodeKind::Leaf { prev, runs, .. },
                ..
            })) = self.axis.nodes.get(leaf_id as usize)
            else {
                self.remaining = 0;
                return None;
            };
            if self.back_idx < runs.len()
                && let Some(lr) = runs.get(self.back_idx)
            {
                self.remaining = self.remaining.saturating_sub(1);
                if self.back_idx > 0 {
                    self.back_idx = self.back_idx.saturating_sub(1);
                } else {
                    self.back_leaf = *prev;
                    self.back_idx = self
                        .back_leaf
                        .map_or(0, |pid| self.axis.leaf_runs_len(pid).saturating_sub(1));
                }
                return Some(&lr.run);
            }
            self.back_leaf = *prev;
            self.back_idx = self
                .back_leaf
                .map_or(0, |pid| self.axis.leaf_runs_len(pid).saturating_sub(1));
        }
    }
}

impl Axis {
    /// Creates an empty axis with an upper bound on total entries.
    #[must_use]
    pub const fn empty(max_entries: u32) -> Self {
        Self::empty_with_px(max_entries, DEFAULT_AXIS_ENTRY_PX)
    }

    /// Creates an empty axis specifying the default entry pixel size (`height` or `width`).
    #[must_use]
    pub const fn empty_with_px(max_entries: u32, default_px: u16) -> Self {
        let safe_px = if default_px == 0 { 1 } else { default_px };
        Self {
            nodes: Vec::new(),
            free_nodes: Vec::new(),
            root: None,
            first_leaf: None,
            last_leaf: None,
            id_map: BTreeMap::new(),
            run_count: 0,
            total_live: 0,
            total_entries: 0,
            total_pixels: 0,
            default_px: safe_px,
            max_entries,
            generation: 1,
        }
    }

    /// Creates an axis holding a single base run `(0, 0, 0..base_len)`.
    #[must_use]
    pub fn with_base(base_len: u32, max_entries: u32) -> Self {
        Self::with_base_px(base_len, max_entries, DEFAULT_AXIS_ENTRY_PX)
    }

    /// Creates a base-run axis with explicit default entry pixel size.
    #[must_use]
    pub fn with_base_px(base_len: u32, max_entries: u32, default_px: u16) -> Self {
        let clamped = base_len.min(max_entries);
        let Some(ids) = IdRun::new(0, clamped) else {
            return Self::empty_with_px(max_entries, default_px);
        };
        let safe_px = default_px.max(1);
        let leaf_run = LeafRun::from_run(AxisRun::live(ids), safe_px);
        let total_pixels = leaf_run.pixel_sum;
        let leaf = BNode {
            parent: None,
            kind: NodeKind::Leaf {
                prev: None,
                next: None,
                runs: vec![leaf_run],
            },
        };
        let mut id_map = BTreeMap::new();
        id_map.insert(0, 0);
        Self {
            nodes: vec![Some(leaf)],
            free_nodes: Vec::new(),
            root: Some(0),
            first_leaf: Some(0),
            last_leaf: Some(0),
            id_map,
            run_count: 1,
            total_live: clamped,
            total_entries: clamped,
            total_pixels,
            default_px: safe_px,
            max_entries,
            generation: 1,
        }
    }

    /// Reconstructs an axis from persisted runs after validating invariants.
    pub fn from_runs(runs: Vec<AxisRun>, max_entries: u32) -> Result<Self, AxisError> {
        Self::from_runs_with_px(runs, max_entries, DEFAULT_AXIS_ENTRY_PX)
    }

    /// Reconstructs an axis from persisted runs with explicit default pixel size.
    pub fn from_runs_with_px(
        runs: Vec<AxisRun>,
        max_entries: u32,
        default_px: u16,
    ) -> Result<Self, AxisError> {
        let safe_px = default_px.max(1);
        let mut total: u32 = 0;
        let mut sorted_spans: Vec<IdRun> = Vec::with_capacity(runs.len());
        let mut leaf_runs: Vec<LeafRun> = Vec::with_capacity(runs.len());
        for run in runs {
            if !run.ids.is_valid() {
                return Err(AxisError::InvalidRun);
            }
            if let Some(counts) = &run.tombstones
                && counts.len() != run.ids.len as usize
            {
                return Err(AxisError::InvalidRun);
            }
            total = total
                .checked_add(run.ids.len)
                .ok_or(AxisError::LimitExceeded)?;
            if total > max_entries {
                return Err(AxisError::LimitExceeded);
            }
            sorted_spans.push(run.ids);
            leaf_runs.push(LeafRun::from_run(run, safe_px));
        }

        sorted_spans.sort_unstable_by_key(|r| r.first_id);
        for pair in sorted_spans.windows(2) {
            if let [left, right] = pair
                && id_stream(left.first_id) == id_stream(right.first_id)
            {
                let left_end = id_counter(left.first_id).saturating_add(left.len);
                if left_end > id_counter(right.first_id) {
                    return Err(AxisError::DuplicateId);
                }
            }
        }

        let mut axis = Self::empty_with_px(max_entries, safe_px);
        axis.bulk_load(leaf_runs);
        Ok(axis)
    }

    /// Updates default entry pixel size (`DEFAULT_ROW_HEIGHT` or `DEFAULT_COL_WIDTH`)
    /// and refreshes pixel sums (`O(R + custom_entries)`).
    pub fn set_default_px(&mut self, default_px: u16) {
        let safe_px = default_px.max(1);
        if self.default_px == safe_px {
            return;
        }
        self.default_px = safe_px;
        let leaves = self.collect_leaf_runs();
        let rebuilt: Vec<LeafRun> = leaves
            .into_iter()
            .map(|lr| {
                let custom = lr.custom_px.map(|v| {
                    v.into_iter()
                        .filter(|&(_, px)| px != safe_px)
                        .collect::<Vec<_>>()
                });
                LeafRun::with_custom(lr.run, custom, safe_px)
            })
            .collect();
        self.bulk_load(rebuilt);
    }

    /// Monotonic structural generation counter, bumped on every mutation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns an iterator over [`AxisRun`]s in axis order.
    #[must_use]
    pub fn runs(&self) -> AxisRunsIter<'_> {
        let back_idx = self
            .last_leaf
            .map_or(0, |lid| self.leaf_runs_len(lid).saturating_sub(1));
        AxisRunsIter {
            axis: self,
            front_leaf: self.first_leaf,
            front_idx: 0,
            back_leaf: self.last_leaf,
            back_idx,
            remaining: self.run_count,
        }
    }

    /// Collects all [`AxisRun`]s into a vector in axis order, for snapshot
    /// serialisation.
    #[must_use]
    pub fn to_runs_vec(&self) -> Vec<AxisRun> {
        let mut out = Vec::with_capacity(self.run_count);
        for run in self.runs() {
            out.push(run.clone());
        }
        out
    }

    /// First run in axis order, if non-empty.
    #[must_use]
    pub fn first_run(&self) -> Option<&AxisRun> {
        let lid = self.first_leaf?;
        self.leaf_run_at(lid, 0).map(|lr| &lr.run)
    }

    /// Last run in axis order, if non-empty.
    #[must_use]
    pub fn last_run(&self) -> Option<&AxisRun> {
        let lid = self.last_leaf?;
        let len = self.leaf_runs_len(lid);
        let idx = len.checked_sub(1)?;
        self.leaf_run_at(lid, idx).map(|lr| &lr.run)
    }

    /// Number of contiguous runs currently held.
    #[must_use]
    pub const fn runs_len(&self) -> usize {
        self.run_count
    }

    /// Maximum allowed total entries (live plus tombstones).
    #[must_use]
    pub const fn max_entries(&self) -> u32 {
        self.max_entries
    }

    /// Number of currently visible (non-deleted) entries.
    #[must_use]
    pub const fn live_len(&self) -> u32 {
        self.total_live
    }

    /// Total entries on the axis, tombstones included.
    #[must_use]
    pub const fn total_len(&self) -> u32 {
        self.total_entries
    }

    /// Total CSS pixels summed over all live entries along this axis.
    #[must_use]
    pub const fn total_pixels(&self) -> u64 {
        self.total_pixels
    }

    /// Approximate heap memory owned by this axis, in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let nodes_outer = self
            .nodes
            .capacity()
            .saturating_mul(size_of::<Option<BNode>>());
        let nodes_inner = self.nodes.iter().fold(0usize, |acc, opt| {
            acc.saturating_add(opt.as_ref().map_or(0, BNode::estimated_bytes))
        });
        let free_mem = self.free_nodes.capacity().saturating_mul(size_of::<u32>());
        let id_map_mem = self.id_map.len().saturating_mul(24);
        nodes_outer
            .saturating_add(nodes_inner)
            .saturating_add(free_mem)
            .saturating_add(id_map_mem)
    }

    /// Sets or clears custom pixel size for `id` in `O(log R)`.
    pub fn set_entry_px(&mut self, id: u64, px: Option<u16>) -> Result<(), AxisError> {
        let loc = self.locate_id(id).ok_or(AxisError::TargetNotFound)?;
        let default_px = self.default_px;
        let old_sum = self
            .leaf_run_at(loc.leaf_id, loc.run_idx)
            .map_or(0, |lr| lr.pixel_sum);
        if let Some(lr) = self.leaf_run_at_mut(loc.leaf_id, loc.run_idx) {
            lr.set_entry_px(loc.offset, px, default_px);
            let new_sum = lr.pixel_sum;
            self.total_pixels = self
                .total_pixels
                .saturating_sub(old_sum)
                .saturating_add(new_sum);
        }
        self.recompute_ancestors(loc.leaf_id);
        self.bump_generation();
        Ok(())
    }

    /// Computes cumulative pixel offset (top/left edge) of 0-based visible
    /// position `pos` in `O(log R)`.
    #[must_use]
    pub fn pixel_offset_of_pos(&self, pos: u32) -> Option<u64> {
        if pos >= self.total_live {
            return None;
        }
        let mut cur = self.root?;
        let mut rem_pos = pos;
        let mut px_acc = 0_u64;
        loop {
            let node = self.nodes.get(cur as usize)?.as_ref()?;
            match &node.kind {
                NodeKind::Internal(children) => {
                    let mut next_child = None;
                    for slot in children {
                        if rem_pos < slot.live_count {
                            next_child = Some(slot.child);
                            break;
                        }
                        rem_pos = rem_pos.saturating_sub(slot.live_count);
                        px_acc = px_acc.saturating_add(slot.pixel_sum);
                    }
                    cur = next_child?;
                }
                NodeKind::Leaf { runs, .. } => {
                    for lr in runs {
                        if rem_pos < lr.run.live_count {
                            let within = lr.pixel_offset_of_live(rem_pos, self.default_px)?;
                            return Some(px_acc.saturating_add(within));
                        }
                        rem_pos = rem_pos.saturating_sub(lr.run.live_count);
                        px_acc = px_acc.saturating_add(lr.pixel_sum);
                    }
                    return None;
                }
            }
        }
    }

    /// Resolves pixel offset `px` (`0 <= px < total_pixels()`) to
    /// `(visible_pos, item_start_px, item_px)` in `O(log R)`.
    #[must_use]
    pub fn pos_at_pixel(&self, px: u64) -> Option<(u32, u64, u16)> {
        if px >= self.total_pixels {
            return None;
        }
        let mut cur = self.root?;
        let mut rem_px = px;
        let mut live_acc = 0_u32;
        let mut px_acc = 0_u64;
        loop {
            let node = self.nodes.get(cur as usize)?.as_ref()?;
            match &node.kind {
                NodeKind::Internal(children) => {
                    let mut next_child = None;
                    for slot in children {
                        if rem_px < slot.pixel_sum {
                            next_child = Some(slot.child);
                            break;
                        }
                        rem_px = rem_px.saturating_sub(slot.pixel_sum);
                        live_acc = live_acc.saturating_add(slot.live_count);
                        px_acc = px_acc.saturating_add(slot.pixel_sum);
                    }
                    cur = next_child?;
                }
                NodeKind::Leaf { runs, .. } => {
                    for lr in runs {
                        if rem_px < lr.pixel_sum {
                            let (within_live, start_in_run, item_px) =
                                lr.live_at_pixel(rem_px, self.default_px)?;
                            return Some((
                                live_acc.saturating_add(within_live),
                                px_acc.saturating_add(start_in_run),
                                item_px,
                            ));
                        }
                        rem_px = rem_px.saturating_sub(lr.pixel_sum);
                        live_acc = live_acc.saturating_add(lr.run.live_count);
                        px_acc = px_acc.saturating_add(lr.pixel_sum);
                    }
                    return None;
                }
            }
        }
    }

    /// Looks up the 0-based visible position of `id`, returning `None` if `id`
    /// does not exist or is currently deleted (`O(log R)`).
    #[must_use]
    pub fn id_to_pos(&self, id: u64) -> Option<u32> {
        let loc = self.locate_id(id)?;
        let lr = self.leaf_run_at(loc.leaf_id, loc.run_idx)?;
        if lr.run.tombstone_at(loc.offset) != 0 {
            return None;
        }
        Some(
            loc.live_before
                .saturating_add(lr.run.live_before(loc.offset)),
        )
    }

    /// Whether `id` exists on the axis (live or deleted).
    #[must_use]
    pub fn contains_id(&self, id: u64) -> bool {
        self.locate_id(id).is_some()
    }

    /// Looks up the identity at 0-based visible position `pos` in `O(log R)`.
    #[must_use]
    pub fn pos_to_id(&self, pos: u32) -> Option<u64> {
        if pos >= self.total_live {
            return None;
        }
        let mut cur = self.root?;
        let mut rem = pos;
        loop {
            let node = self.nodes.get(cur as usize)?.as_ref()?;
            match &node.kind {
                NodeKind::Internal(children) => {
                    let mut next_child = None;
                    for slot in children {
                        if rem < slot.live_count {
                            next_child = Some(slot.child);
                            break;
                        }
                        rem = rem.saturating_sub(slot.live_count);
                    }
                    cur = next_child?;
                }
                NodeKind::Leaf { runs, .. } => {
                    for lr in runs {
                        if rem < lr.run.live_count {
                            let offset = lr.run.offset_of_live(rem)?;
                            return lr.run.ids.id_at(offset);
                        }
                        rem = rem.saturating_sub(lr.run.live_count);
                    }
                    return None;
                }
            }
        }
    }

    /// Compares two anchor identities in axis order (including tombstones).
    #[must_use]
    pub fn compare_anchors(&self, a: u64, b: u64) -> Option<Ordering> {
        let loc_a = self.locate_id(a)?;
        let loc_b = self.locate_id(b)?;
        Some(loc_a.axis_order.cmp(&loc_b.axis_order))
    }

    /// Returns the 0-based axis order index (tombstones included) of `id`.
    #[must_use]
    pub fn axis_order_of(&self, id: u64) -> Option<u32> {
        self.locate_id(id).map(|loc| loc.axis_order)
    }

    /// Whether `id` exists on the axis and has tombstone count `0`.
    #[must_use]
    pub fn is_live(&self, id: u64) -> bool {
        let Some(loc) = self.locate_id(id) else {
            return false;
        };
        self.leaf_run_at(loc.leaf_id, loc.run_idx)
            .is_some_and(|lr| lr.run.tombstone_at(loc.offset) == 0)
    }

    /// Resolves a 0-based axis-order index (`0..total_len()`, tombstones
    /// included) to `(id, is_live)` in `O(log R)`.
    #[must_use]
    pub fn id_at_axis_order(&self, order: u32) -> Option<(u64, bool)> {
        if order >= self.total_entries {
            return None;
        }
        let mut cur = self.root?;
        let mut rem = order;
        loop {
            let node = self.nodes.get(cur as usize)?.as_ref()?;
            match &node.kind {
                NodeKind::Internal(children) => {
                    let mut next_child = None;
                    for slot in children {
                        if rem < slot.total_entries {
                            next_child = Some(slot.child);
                            break;
                        }
                        rem = rem.saturating_sub(slot.total_entries);
                    }
                    cur = next_child?;
                }
                NodeKind::Leaf { runs, .. } => {
                    for lr in runs {
                        if rem < lr.run.ids.len {
                            let id = lr.run.ids.id_at(rem)?;
                            return Some((id, lr.run.tombstone_at(rem) == 0));
                        }
                        rem = rem.saturating_sub(lr.run.ids.len);
                    }
                    return None;
                }
            }
        }
    }

    /// Calls `visit` with contiguous live [`IdRun`] segments between
    /// `first_anchor` and `last_anchor` (inclusive, regardless of which anchor
    /// comes first on the axis). Returns `false` if either anchor does not
    /// exist on the axis.
    pub fn for_each_live_in_range(
        &self,
        first_anchor: u64,
        last_anchor: u64,
        mut visit: impl FnMut(IdRun),
    ) -> bool {
        let (Some(a), Some(b)) = (self.locate_id(first_anchor), self.locate_id(last_anchor)) else {
            return false;
        };
        let (start, end) = if a.axis_order <= b.axis_order {
            (a, b)
        } else {
            (b, a)
        };
        let mut cur_leaf = Some(start.leaf_id);
        let mut cur_idx = start.run_idx;
        while let Some(lid) = cur_leaf {
            let Some(Some(BNode {
                kind: NodeKind::Leaf { next, runs, .. },
                ..
            })) = self.nodes.get(lid as usize)
            else {
                break;
            };
            while let Some(lr) = runs.get(cur_idx) {
                let is_start = lid == start.leaf_id && cur_idx == start.run_idx;
                let is_end = lid == end.leaf_id && cur_idx == end.run_idx;
                if lr.run.live_count > 0 {
                    let from = if is_start { start.offset } else { 0 };
                    let to_inclusive = if is_end {
                        end.offset
                    } else {
                        lr.run.ids.len.saturating_sub(1)
                    };
                    Self::emit_live_subruns(&lr.run, from, to_inclusive, &mut visit);
                }
                if is_end {
                    return true;
                }
                cur_idx = cur_idx.saturating_add(1);
            }
            cur_leaf = *next;
            cur_idx = 0;
        }
        true
    }

    /// Calls `visit` with contiguous live [`IdRun`] segments covering `count`
    /// visible positions starting at 0-based visible position `first_pos`.
    pub fn for_each_live_positions(
        &self,
        first_pos: u32,
        count: u32,
        mut visit: impl FnMut(IdRun),
    ) {
        if count == 0 || first_pos >= self.total_live {
            return;
        }
        let end_pos = first_pos.saturating_add(count).min(self.total_live);
        let mut remaining = end_pos.saturating_sub(first_pos);
        let Some((mut cur_leaf, mut cur_idx, mut skip_in_run)) = self.find_leaf_for_pos(first_pos)
        else {
            return;
        };

        while remaining > 0 {
            let Some(Some(BNode {
                kind: NodeKind::Leaf { next, runs, .. },
                ..
            })) = self.nodes.get(cur_leaf as usize)
            else {
                break;
            };
            while remaining > 0 {
                let Some(lr) = runs.get(cur_idx) else {
                    break;
                };
                let available = lr.run.live_count.saturating_sub(skip_in_run);
                if available > 0 {
                    let take = available.min(remaining);
                    if let (Some(from), Some(to)) = (
                        lr.run.offset_of_live(skip_in_run),
                        lr.run
                            .offset_of_live(skip_in_run.saturating_add(take.saturating_sub(1))),
                    ) {
                        Self::emit_live_subruns(&lr.run, from, to, &mut visit);
                    }
                    remaining = remaining.saturating_sub(take);
                }
                skip_in_run = 0;
                cur_idx = cur_idx.saturating_add(1);
            }
            let Some(nxt) = *next else {
                break;
            };
            cur_leaf = nxt;
            cur_idx = 0;
        }
    }

    /// Inserts `ids` directly after `after` (`None` inserts at the very start
    /// of the axis) in `O(log R)`. Validated atomically before mutating state.
    pub fn insert_after(&mut self, after: Option<u64>, ids: IdRun) -> Result<(), AxisError> {
        if !ids.is_valid() {
            return Err(AxisError::InvalidRun);
        }
        let new_total = self
            .total_entries
            .checked_add(ids.len)
            .ok_or(AxisError::LimitExceeded)?;
        if new_total > self.max_entries {
            return Err(AxisError::LimitExceeded);
        }
        if self.overlaps_existing(ids) {
            return Err(AxisError::DuplicateId);
        }

        let new_lr = LeafRun::from_run(AxisRun::live(ids), self.default_px);
        match after {
            None => match self.first_leaf {
                None => {
                    self.bulk_load(vec![new_lr]);
                }
                Some(lid) => {
                    let final_leaf = self.insert_runs_into_leaf(lid, 0, vec![new_lr]);
                    self.try_coalesce_around(final_leaf);
                }
            },
            Some(anchor) => {
                let loc = self.locate_id(anchor).ok_or(AxisError::AnchorNotFound)?;
                let existing = self
                    .leaf_run_at(loc.leaf_id, loc.run_idx)
                    .cloned()
                    .ok_or(AxisError::AnchorNotFound)?;
                let left_len = loc.offset.saturating_add(1);
                let replacements = if left_len == existing.run.ids.len {
                    vec![existing, new_lr]
                } else {
                    let (left, right) = existing
                        .split_at(left_len, self.default_px)
                        .ok_or(AxisError::InvalidRun)?;
                    vec![left, new_lr, right]
                };
                let final_leaf = self.replace_leaf_run(loc.leaf_id, loc.run_idx, replacements);
                if let Some(lid) = final_leaf {
                    self.try_coalesce_around(lid);
                }
            }
        }
        self.bump_generation();
        Ok(())
    }

    /// Physically removes an inserted `IdRun` (the exact inverse of
    /// [`Self::insert_after`]) in `O(log R)`. All entries in `ids` must exist
    /// consecutively without tombstones.
    pub fn remove_inserted(&mut self, ids: IdRun) -> Result<(), AxisError> {
        if !ids.is_valid() {
            return Err(AxisError::InvalidRun);
        }
        let first_loc = self
            .locate_id(ids.first_id)
            .ok_or(AxisError::TargetNotFound)?;
        let existing = self
            .leaf_run_at(first_loc.leaf_id, first_loc.run_idx)
            .cloned()
            .ok_or(AxisError::TargetNotFound)?;
        let end_offset = first_loc
            .offset
            .checked_add(ids.len)
            .ok_or(AxisError::InvalidInverse)?;
        if end_offset > existing.run.ids.len {
            return Err(AxisError::InvalidInverse);
        }
        for offset in first_loc.offset..end_offset {
            if existing.run.tombstone_at(offset) != 0 {
                return Err(AxisError::InvalidInverse);
            }
        }

        let mut replacements = Vec::with_capacity(2);
        if first_loc.offset > 0 {
            let (left, _) = existing
                .split_at(first_loc.offset, self.default_px)
                .ok_or(AxisError::InvalidInverse)?;
            replacements.push(left);
        }
        if end_offset < existing.run.ids.len {
            let (_, right) = existing
                .split_at(end_offset, self.default_px)
                .ok_or(AxisError::InvalidInverse)?;
            replacements.push(right);
        }

        let neighbor_leaf =
            self.replace_leaf_run(first_loc.leaf_id, first_loc.run_idx, replacements);
        if let Some(lid) = neighbor_leaf {
            self.try_coalesce_around(lid);
        }
        self.bump_generation();
        Ok(())
    }

    /// Validates that `targets` are non-empty, valid, pairwise non-overlapping
    /// runs whose identities all exist on this axis (live or tombstoned).
    pub fn validate_existing_runs(&self, targets: &[IdRun]) -> Result<(), AxisError> {
        if targets.is_empty() {
            return Err(AxisError::InvalidRun);
        }
        for (i, target) in targets.iter().enumerate() {
            if !target.is_valid() {
                return Err(AxisError::InvalidRun);
            }
            for earlier in targets.get(..i).unwrap_or(&[]) {
                if Self::runs_overlap(*earlier, *target) {
                    return Err(AxisError::DuplicateId);
                }
            }
            for id in target.iter() {
                if self.locate_id(id).is_none() {
                    return Err(AxisError::TargetNotFound);
                }
            }
        }
        Ok(())
    }

    /// Increments the deletion count of every identity in `targets`.
    /// Validates all runs before mutating so an invalid input leaves the axis
    /// untouched.
    pub fn delete_runs(&mut self, targets: &[IdRun]) -> Result<(), AxisError> {
        self.validate_target_runs(targets, false)?;
        for target in targets {
            let mut cursor_id = target.first_id;
            let mut rem = target.len;
            while rem > 0 {
                let loc = self.locate_id(cursor_id).ok_or(AxisError::TargetNotFound)?;
                let existing = self
                    .leaf_run_at(loc.leaf_id, loc.run_idx)
                    .cloned()
                    .ok_or(AxisError::TargetNotFound)?;
                let take = rem.min(existing.run.ids.len.saturating_sub(loc.offset));
                let iv_end = loc.offset.saturating_add(take);
                if existing.run.tombstones.is_none() && existing.run.ids.len > 64 {
                    let mut pieces = Vec::with_capacity(3);
                    if loc.offset > 0
                        && let Some(left) = existing.slice(0, loc.offset, self.default_px)
                    {
                        pieces.push(left);
                    }
                    let del_first = existing
                        .run
                        .ids
                        .id_at(loc.offset)
                        .ok_or(AxisError::InvalidRun)?;
                    let del_ids = IdRun::new(del_first, take).ok_or(AxisError::InvalidRun)?;
                    let del_usize = usize::try_from(take).map_err(|_| AxisError::InvalidRun)?;
                    let del_custom = existing.custom_px.as_ref().map(|entries| {
                        let lo = entries.partition_point(|&(o, _)| o < loc.offset);
                        let hi = entries.partition_point(|&(o, _)| o < iv_end);
                        entries
                            .get(lo..hi)
                            .unwrap_or(&[])
                            .iter()
                            .map(|&(o, px)| (o.saturating_sub(loc.offset), px))
                            .collect::<Vec<_>>()
                    });
                    pieces.push(LeafRun::with_custom(
                        AxisRun {
                            ids: del_ids,
                            live_count: 0,
                            tombstones: Some(vec![1_u32; del_usize].into_boxed_slice()),
                        },
                        del_custom,
                        self.default_px,
                    ));
                    if iv_end < existing.run.ids.len
                        && let Some(right) =
                            existing.slice(iv_end, existing.run.ids.len, self.default_px)
                    {
                        pieces.push(right);
                    }
                    let _ = self.replace_leaf_run(loc.leaf_id, loc.run_idx, pieces);
                } else {
                    self.increment_tombstone_range(loc.leaf_id, loc.run_idx, loc.offset, iv_end)?;
                }
                cursor_id = cursor_id.saturating_add(u64::from(take));
                rem = rem.saturating_sub(take);
            }
        }
        self.bump_generation();
        Ok(())
    }

    /// Decrements the deletion count of every identity in `targets` (inverse of
    /// [`Self::delete_runs`]). Fails atomically if any target has count `0`.
    pub fn undelete_runs(&mut self, targets: &[IdRun]) -> Result<(), AxisError> {
        self.validate_target_runs(targets, true)?;
        let mut touched_leaves: Vec<u32> = Vec::with_capacity(targets.len());
        for target in targets {
            let mut cursor_id = target.first_id;
            let mut rem = target.len;
            while rem > 0 {
                let loc = self.locate_id(cursor_id).ok_or(AxisError::TargetNotFound)?;
                let run_len = self
                    .leaf_run_at(loc.leaf_id, loc.run_idx)
                    .map_or(0, |lr| lr.run.ids.len);
                let take = rem.min(run_len.saturating_sub(loc.offset));
                let iv_end = loc.offset.saturating_add(take);
                self.decrement_tombstone_range(loc.leaf_id, loc.run_idx, loc.offset, iv_end)?;
                if !touched_leaves.contains(&loc.leaf_id) {
                    touched_leaves.push(loc.leaf_id);
                }
                cursor_id = cursor_id.saturating_add(u64::from(take));
                rem = rem.saturating_sub(take);
            }
        }
        for lid in touched_leaves {
            if self.node_is_live(lid) {
                self.try_coalesce_around(lid);
            }
        }
        self.bump_generation();
        Ok(())
    }

    /// Moves all entries in `targets` (preserving their current relative axis
    /// order and tombstone counts) to sit immediately after `after` (`None`
    /// places them at the very start of the axis). Returns the prior `runs`
    /// slice so [`Self::restore_runs`] can invert the move.
    pub fn move_runs(
        &mut self,
        targets: &[IdRun],
        after: Option<u64>,
    ) -> Result<Vec<AxisRun>, AxisError> {
        self.validate_existing_runs(targets)?;
        if let Some(anchor) = after {
            let _ = self.locate_id(anchor).ok_or(AxisError::AnchorNotFound)?;
            if targets.iter().any(|t| t.contains(anchor)) {
                return Err(AxisError::InvalidMove);
            }
        }

        let current = self.collect_leaf_runs();
        let prior: Vec<AxisRun> = current.iter().map(|lr| lr.run.clone()).collect();
        let mut staying_runs: Vec<LeafRun> = Vec::with_capacity(current.len().saturating_add(4));
        let mut moved_runs: Vec<LeafRun> = Vec::with_capacity(targets.len().saturating_add(2));

        for lr in current {
            let intervals = run_target_intervals(&lr.run, targets);
            if intervals.is_empty() {
                staying_runs.push(lr);
                continue;
            }
            let mut cursor: u32 = 0;
            for (iv_start, iv_end) in intervals {
                if cursor < iv_start
                    && let Some(stay_piece) = lr.slice(cursor, iv_start, self.default_px)
                {
                    staying_runs.push(stay_piece);
                }
                if let Some(moved_piece) = lr.slice(iv_start, iv_end, self.default_px) {
                    moved_runs.push(moved_piece);
                }
                cursor = iv_end;
            }
            if cursor < lr.run.ids.len
                && let Some(tail_piece) = lr.slice(cursor, lr.run.ids.len, self.default_px)
            {
                staying_runs.push(tail_piece);
            }
        }

        let combined = match after {
            None => {
                let mut combined =
                    Vec::with_capacity(moved_runs.len().saturating_add(staying_runs.len()));
                combined.extend(moved_runs);
                combined.extend(staying_runs);
                combined
            }
            Some(anchor) => {
                let mut anchor_pos: Option<(usize, u32)> = None;
                for (idx, lr) in staying_runs.iter().enumerate() {
                    if lr.run.ids.contains(anchor) {
                        let off =
                            id_counter(anchor).saturating_sub(id_counter(lr.run.ids.first_id));
                        anchor_pos = Some((idx, off));
                        break;
                    }
                }
                let (idx, off) = anchor_pos.ok_or(AxisError::AnchorNotFound)?;
                let target_run = staying_runs
                    .get(idx)
                    .cloned()
                    .ok_or(AxisError::AnchorNotFound)?;
                let left_len = off.saturating_add(1);
                if left_len == target_run.run.ids.len {
                    let insert_at = idx.saturating_add(1);
                    staying_runs.splice(insert_at..insert_at, moved_runs);
                } else {
                    let (left, right) = target_run
                        .split_at(left_len, self.default_px)
                        .ok_or(AxisError::InvalidRun)?;
                    if let Some(slot) = staying_runs.get_mut(idx) {
                        *slot = left;
                    }
                    let insert_at = idx.saturating_add(1);
                    moved_runs.push(right);
                    staying_runs.splice(insert_at..insert_at, moved_runs);
                }
                staying_runs
            }
        };

        let coalesced = Self::coalesce_vec(combined);
        self.bulk_load(coalesced);
        self.bump_generation();
        Ok(prior)
    }

    /// Restores `runs` saved prior to [`Self::move_runs`].
    pub fn restore_runs(&mut self, runs: Vec<AxisRun>) -> Result<(), AxisError> {
        let prev_gen = self.generation;
        let mut restored = Self::from_runs_with_px(runs, self.max_entries, self.default_px)?;
        restored.generation = prev_gen.wrapping_add(1);
        *self = restored;
        Ok(())
    }

    /// Physically removes all tombstoned entries (`tombstone > 0`) from this
    /// axis during local snapshot garbage collection (`14-m1b.md` 5.5),
    /// returning the purged identities in deterministic order.
    pub fn purge_tombstones(&mut self) -> Vec<u64> {
        let current = self.collect_leaf_runs();
        let mut purged = Vec::new();
        let mut live_runs: Vec<LeafRun> = Vec::with_capacity(current.len());
        for lr in current {
            match &lr.run.tombstones {
                None => live_runs.push(lr),
                Some(counts) => {
                    for (idx, &cnt) in counts.iter().enumerate() {
                        if cnt > 0
                            && let Ok(off) = u32::try_from(idx)
                            && let Some(id) = lr.run.ids.id_at(off)
                        {
                            purged.push(id);
                        }
                    }
                    if lr.run.live_count > 0 {
                        let to = lr.run.ids.len.saturating_sub(1);
                        Self::emit_live_subruns(&lr.run, 0, to, &mut |sub| {
                            let base_off = id_counter(sub.first_id)
                                .saturating_sub(id_counter(lr.run.ids.first_id));
                            let custom = lr.custom_px.as_ref().map(|entries| {
                                let lo = entries.partition_point(|&(o, _)| o < base_off);
                                let hi = entries.partition_point(|&(o, _)| {
                                    o < base_off.saturating_add(sub.len)
                                });
                                entries
                                    .get(lo..hi)
                                    .unwrap_or(&[])
                                    .iter()
                                    .map(|&(o, px)| (o.saturating_sub(base_off), px))
                                    .collect::<Vec<_>>()
                            });
                            live_runs.push(LeafRun::with_custom(
                                AxisRun::live(sub),
                                custom,
                                self.default_px,
                            ));
                        });
                    }
                }
            }
        }
        let coalesced = Self::coalesce_vec(live_runs);
        self.bulk_load(coalesced);
        self.bump_generation();
        purged
    }

    // --------------------------------------------------------- B+ tree helpers

    const fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn node_is_live(&self, id: u32) -> bool {
        self.nodes
            .get(id as usize)
            .and_then(Option::as_ref)
            .is_some()
    }

    fn alloc_node(&mut self, node: BNode) -> u32 {
        if let Some(idx) = self.free_nodes.pop()
            && let Some(slot) = self.nodes.get_mut(idx as usize)
        {
            *slot = Some(node);
            return idx;
        }
        let idx = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(Some(node));
        idx
    }

    fn free_node(&mut self, id: u32) {
        if let Some(slot) = self.nodes.get_mut(id as usize) {
            *slot = None;
            self.free_nodes.push(id);
        }
    }

    fn set_parent(&mut self, child_id: u32, parent: Option<u32>) {
        if let Some(Some(node)) = self.nodes.get_mut(child_id as usize) {
            node.parent = parent;
        }
    }

    fn leaf_runs_len(&self, leaf_id: u32) -> usize {
        let Some(Some(BNode {
            kind: NodeKind::Leaf { runs, .. },
            ..
        })) = self.nodes.get(leaf_id as usize)
        else {
            return 0;
        };
        runs.len()
    }

    fn leaf_run_at(&self, leaf_id: u32, run_idx: usize) -> Option<&LeafRun> {
        let Some(Some(BNode {
            kind: NodeKind::Leaf { runs, .. },
            ..
        })) = self.nodes.get(leaf_id as usize)
        else {
            return None;
        };
        runs.get(run_idx)
    }

    fn leaf_run_at_mut(&mut self, leaf_id: u32, run_idx: usize) -> Option<&mut LeafRun> {
        let Some(Some(BNode {
            kind: NodeKind::Leaf { runs, .. },
            ..
        })) = self.nodes.get_mut(leaf_id as usize)
        else {
            return None;
        };
        runs.get_mut(run_idx)
    }

    fn collect_leaf_runs(&self) -> Vec<LeafRun> {
        let mut out = Vec::with_capacity(self.run_count);
        let mut cur = self.first_leaf;
        while let Some(lid) = cur {
            let Some(Some(BNode {
                kind: NodeKind::Leaf { next, runs, .. },
                ..
            })) = self.nodes.get(lid as usize)
            else {
                break;
            };
            out.extend(runs.iter().cloned());
            cur = *next;
        }
        out
    }

    fn coalesce_vec(mut runs: Vec<LeafRun>) -> Vec<LeafRun> {
        let mut out: Vec<LeafRun> = Vec::with_capacity(runs.len());
        for lr in runs.drain(..) {
            if let Some(prev) = out.last_mut()
                && prev.can_coalesce_with(&lr)
            {
                prev.absorb_right(lr);
            } else {
                out.push(lr);
            }
        }
        out
    }

    fn bulk_load(&mut self, runs: Vec<LeafRun>) {
        self.nodes.clear();
        self.free_nodes.clear();
        self.root = None;
        self.first_leaf = None;
        self.last_leaf = None;
        self.id_map.clear();
        self.run_count = runs.len();
        self.total_live = 0;
        self.total_entries = 0;
        self.total_pixels = 0;

        if runs.is_empty() {
            return;
        }

        for lr in &runs {
            self.total_live = self.total_live.saturating_add(lr.run.live_count);
            self.total_entries = self.total_entries.saturating_add(lr.run.ids.len);
            self.total_pixels = self.total_pixels.saturating_add(lr.pixel_sum);
        }

        // Single-run common case: allocate capacity 1 to preserve tiny footprint.
        if runs.len() == 1 {
            let first_id = runs.first().map_or(0, |lr| lr.run.ids.first_id);
            let leaf_id = self.alloc_node(BNode {
                parent: None,
                kind: NodeKind::Leaf {
                    prev: None,
                    next: None,
                    runs,
                },
            });
            self.id_map.insert(first_id, leaf_id);
            self.root = Some(leaf_id);
            self.first_leaf = Some(leaf_id);
            self.last_leaf = Some(leaf_id);
            return;
        }

        let chunk_size = 24usize.clamp(INTERNAL_MIN, LEAF_MAX);
        let mut level: Vec<u32> = Vec::with_capacity(runs.len().div_ceil(chunk_size));
        let mut it = runs.into_iter().peekable();
        let mut prev_leaf: Option<u32> = None;
        while it.peek().is_some() {
            let mut chunk = Vec::with_capacity(chunk_size);
            for _ in 0..chunk_size {
                let Some(item) = it.next() else {
                    break;
                };
                chunk.push(item);
            }
            let lid = self.alloc_node(BNode {
                parent: None,
                kind: NodeKind::Leaf {
                    prev: prev_leaf,
                    next: None,
                    runs: chunk,
                },
            });
            if let Some(pl) = prev_leaf
                && let Some(Some(BNode {
                    kind: NodeKind::Leaf { next, .. },
                    ..
                })) = self.nodes.get_mut(pl as usize)
            {
                *next = Some(lid);
            } else {
                self.first_leaf = Some(lid);
            }
            if let Some(Some(BNode {
                kind: NodeKind::Leaf { runs: rlist, .. },
                ..
            })) = self.nodes.get(lid as usize)
            {
                for lr in rlist {
                    self.id_map.insert(lr.run.ids.first_id, lid);
                }
            }
            prev_leaf = Some(lid);
            self.last_leaf = Some(lid);
            level.push(lid);
        }

        while level.len() > 1 {
            let mut next_level = Vec::with_capacity(level.len().div_ceil(chunk_size));
            for slice in level.chunks(chunk_size) {
                let mut slots = Vec::with_capacity(slice.len());
                for &cid in slice {
                    if let Some(Some(cnode)) = self.nodes.get(cid as usize) {
                        slots.push(cnode.summary(cid));
                    }
                }
                let pid = self.alloc_node(BNode {
                    parent: None,
                    kind: NodeKind::Internal(slots),
                });
                for &cid in slice {
                    self.set_parent(cid, Some(pid));
                }
                next_level.push(pid);
            }
            level = next_level;
        }
        self.root = level.first().copied();
    }

    fn recompute_ancestors(&mut self, mut node_id: u32) {
        while let Some(Some(BNode {
            parent: Some(pid), ..
        })) = self.nodes.get(node_id as usize)
        {
            let pid = *pid;
            let Some(summary) = self
                .nodes
                .get(node_id as usize)
                .and_then(Option::as_ref)
                .map(|n| n.summary(node_id))
            else {
                break;
            };
            if let Some(Some(BNode {
                kind: NodeKind::Internal(children),
                ..
            })) = self.nodes.get_mut(pid as usize)
                && let Some(slot) = children.iter_mut().find(|s| s.child == node_id)
            {
                *slot = summary;
            }
            node_id = pid;
        }
    }

    fn insert_runs_into_leaf(
        &mut self,
        leaf_id: u32,
        at_idx: usize,
        inserted: Vec<LeafRun>,
    ) -> u32 {
        let mut added_entries = 0_u32;
        let mut added_live = 0_u32;
        let mut added_px = 0_u64;
        for lr in &inserted {
            added_entries = added_entries.saturating_add(lr.run.ids.len);
            added_live = added_live.saturating_add(lr.run.live_count);
            added_px = added_px.saturating_add(lr.pixel_sum);
            self.id_map.insert(lr.run.ids.first_id, leaf_id);
        }
        let added_count = inserted.len();
        if let Some(Some(BNode {
            kind: NodeKind::Leaf { runs, .. },
            ..
        })) = self.nodes.get_mut(leaf_id as usize)
        {
            runs.splice(at_idx..at_idx, inserted);
        }
        self.run_count = self.run_count.saturating_add(added_count);
        self.total_entries = self.total_entries.saturating_add(added_entries);
        self.total_live = self.total_live.saturating_add(added_live);
        self.total_pixels = self.total_pixels.saturating_add(added_px);
        self.recompute_ancestors(leaf_id);
        self.ensure_leaf_max(leaf_id);
        leaf_id
    }

    fn replace_leaf_run(
        &mut self,
        leaf_id: u32,
        run_idx: usize,
        replacements: Vec<LeafRun>,
    ) -> Option<u32> {
        let prev_leaf;
        let next_leaf;
        let old_first_id;
        let old_entries;
        let old_live;
        let old_px;
        {
            let Some(Some(BNode {
                kind: NodeKind::Leaf { prev, next, runs },
                ..
            })) = self.nodes.get_mut(leaf_id as usize)
            else {
                return None;
            };
            prev_leaf = *prev;
            next_leaf = *next;
            let existing = runs.get(run_idx)?;
            old_first_id = existing.run.ids.first_id;
            old_entries = existing.run.ids.len;
            old_live = existing.run.live_count;
            old_px = existing.pixel_sum;
            self.id_map.remove(&old_first_id);
            let mut new_entries = 0_u32;
            let mut new_live = 0_u32;
            let mut new_px = 0_u64;
            for lr in &replacements {
                new_entries = new_entries.saturating_add(lr.run.ids.len);
                new_live = new_live.saturating_add(lr.run.live_count);
                new_px = new_px.saturating_add(lr.pixel_sum);
                self.id_map.insert(lr.run.ids.first_id, leaf_id);
            }
            let new_count = replacements.len();
            runs.splice(run_idx..=run_idx, replacements);
            self.run_count = self.run_count.saturating_sub(1).saturating_add(new_count);
            self.total_entries = self
                .total_entries
                .saturating_sub(old_entries)
                .saturating_add(new_entries);
            self.total_live = self
                .total_live
                .saturating_sub(old_live)
                .saturating_add(new_live);
            self.total_pixels = self
                .total_pixels
                .saturating_sub(old_px)
                .saturating_add(new_px);
        }

        self.recompute_ancestors(leaf_id);
        let len = self.leaf_runs_len(leaf_id);
        if len > LEAF_MAX {
            self.ensure_leaf_max(leaf_id);
            return Some(leaf_id);
        }
        if len < LEAF_MIN {
            let survivor = self.rebalance_underflow(leaf_id);
            if let Some(s) = survivor {
                return Some(s);
            }
            return prev_leaf
                .filter(|&p| self.node_is_live(p))
                .or_else(|| next_leaf.filter(|&n| self.node_is_live(n)));
        }
        Some(leaf_id)
    }

    fn ensure_leaf_max(&mut self, leaf_id: u32) {
        let len = self.leaf_runs_len(leaf_id);
        if len <= LEAF_MAX {
            return;
        }
        let split_at = len / 2;
        let (parent, old_next, right_runs) = {
            let Some(Some(BNode {
                parent,
                kind: NodeKind::Leaf { next, runs, .. },
            })) = self.nodes.get_mut(leaf_id as usize)
            else {
                return;
            };
            let right_runs = runs.split_off(split_at);
            (*parent, *next, right_runs)
        };
        let new_lid = self.alloc_node(BNode {
            parent,
            kind: NodeKind::Leaf {
                prev: Some(leaf_id),
                next: old_next,
                runs: right_runs,
            },
        });
        if let Some(Some(BNode {
            kind: NodeKind::Leaf { runs, .. },
            ..
        })) = self.nodes.get(new_lid as usize)
        {
            for lr in runs {
                self.id_map.insert(lr.run.ids.first_id, new_lid);
            }
        }
        if let Some(Some(BNode {
            kind: NodeKind::Leaf { next, .. },
            ..
        })) = self.nodes.get_mut(leaf_id as usize)
        {
            *next = Some(new_lid);
        }
        if let Some(nx) = old_next {
            if let Some(Some(BNode {
                kind: NodeKind::Leaf { prev, .. },
                ..
            })) = self.nodes.get_mut(nx as usize)
            {
                *prev = Some(new_lid);
            }
        } else {
            self.last_leaf = Some(new_lid);
        }
        self.insert_sibling_after(leaf_id, new_lid);
    }

    fn insert_sibling_after(&mut self, left_id: u32, right_id: u32) {
        let left_parent = self
            .nodes
            .get(left_id as usize)
            .and_then(Option::as_ref)
            .and_then(|n| n.parent);
        let Some(left_summary) = self
            .nodes
            .get(left_id as usize)
            .and_then(Option::as_ref)
            .map(|n| n.summary(left_id))
        else {
            return;
        };
        let Some(right_summary) = self
            .nodes
            .get(right_id as usize)
            .and_then(Option::as_ref)
            .map(|n| n.summary(right_id))
        else {
            return;
        };

        let Some(pid) = left_parent else {
            let new_root = self.alloc_node(BNode {
                parent: None,
                kind: NodeKind::Internal(vec![left_summary, right_summary]),
            });
            self.set_parent(left_id, Some(new_root));
            self.set_parent(right_id, Some(new_root));
            self.root = Some(new_root);
            return;
        };

        self.set_parent(right_id, Some(pid));
        let children_len = if let Some(Some(BNode {
            kind: NodeKind::Internal(children),
            ..
        })) = self.nodes.get_mut(pid as usize)
            && let Some(pos) = children.iter().position(|s| s.child == left_id)
        {
            if let Some(slot) = children.get_mut(pos) {
                *slot = left_summary;
            }
            children.insert(pos.saturating_add(1), right_summary);
            children.len()
        } else {
            return;
        };
        self.recompute_ancestors(pid);

        if children_len > INTERNAL_MAX {
            self.split_internal(pid);
        }
    }

    fn split_internal(&mut self, node_id: u32) {
        let len = self
            .nodes
            .get(node_id as usize)
            .and_then(Option::as_ref)
            .map_or(0, BNode::child_len);
        if len <= INTERNAL_MAX {
            return;
        }
        let mid = len / 2;
        let (parent, right_children) = {
            let Some(Some(BNode {
                parent,
                kind: NodeKind::Internal(children),
            })) = self.nodes.get_mut(node_id as usize)
            else {
                return;
            };
            let right_children = children.split_off(mid);
            (*parent, right_children)
        };
        let child_ids: Vec<u32> = right_children.iter().map(|s| s.child).collect();
        let right_id = self.alloc_node(BNode {
            parent,
            kind: NodeKind::Internal(right_children),
        });
        for cid in child_ids {
            self.set_parent(cid, Some(right_id));
        }
        self.insert_sibling_after(node_id, right_id);
    }

    fn rebalance_underflow(&mut self, node_id: u32) -> Option<u32> {
        let node = self.nodes.get(node_id as usize).and_then(Option::as_ref)?;
        let parent = node.parent;
        let len = node.child_len();

        let Some(pid) = parent else {
            // Root node adjustments.
            match &node.kind {
                NodeKind::Leaf { .. } => {
                    if len == 0 {
                        self.root = None;
                        self.first_leaf = None;
                        self.last_leaf = None;
                        self.free_node(node_id);
                        return None;
                    }
                    return Some(node_id);
                }
                NodeKind::Internal(children) => {
                    if children.len() == 1
                        && let Some(only) = children.first().map(|s| s.child)
                    {
                        self.set_parent(only, None);
                        self.root = Some(only);
                        self.free_node(node_id);
                        return Some(only);
                    }
                    return Some(node_id);
                }
            }
        };

        let is_leaf = matches!(node.kind, NodeKind::Leaf { .. });
        let min_len = if is_leaf { LEAF_MIN } else { INTERNAL_MIN };
        if len >= min_len && len > 0 {
            return Some(node_id);
        }

        // Identify sibling indices in parent `pid`.
        let (left_sib, right_sib) = {
            let Some(Some(BNode {
                kind: NodeKind::Internal(children),
                ..
            })) = self.nodes.get(pid as usize)
            else {
                return Some(node_id);
            };
            let Some(pos) = children.iter().position(|s| s.child == node_id) else {
                return Some(node_id);
            };
            let ls = pos
                .checked_sub(1)
                .and_then(|i| children.get(i))
                .map(|s| s.child);
            let rs = children.get(pos.saturating_add(1)).map(|s| s.child);
            (ls, rs)
        };

        let max_len = if is_leaf { LEAF_MAX } else { INTERNAL_MAX };

        // 1. Try merging into left sibling, or borrowing from left sibling.
        if let Some(ls_id) = left_sib {
            let ls_len = self
                .nodes
                .get(ls_id as usize)
                .and_then(Option::as_ref)
                .map_or(0, BNode::child_len);
            if ls_len.saturating_add(len) <= max_len {
                self.merge_into_left(pid, ls_id, node_id);
                self.rebalance_underflow(pid);
                return is_leaf.then_some(ls_id);
            }
            if ls_len > min_len {
                self.borrow_from_left(ls_id, node_id);
                self.recompute_ancestors(ls_id);
                self.recompute_ancestors(node_id);
                return Some(node_id);
            }
        }

        // 2. Try merging right sibling into self, or borrowing from right sibling.
        if let Some(rs_id) = right_sib {
            let rs_len = self
                .nodes
                .get(rs_id as usize)
                .and_then(Option::as_ref)
                .map_or(0, BNode::child_len);
            if len.saturating_add(rs_len) <= max_len {
                self.merge_into_left(pid, node_id, rs_id);
                self.rebalance_underflow(pid);
                return Some(node_id);
            }
            if rs_len > min_len {
                self.borrow_from_right(node_id, rs_id);
                self.recompute_ancestors(rs_id);
                self.recompute_ancestors(node_id);
                return Some(node_id);
            }
        }

        Some(node_id)
    }

    fn merge_into_left(&mut self, pid: u32, left_id: u32, right_id: u32) {
        let Some(right_node) = self.nodes.get_mut(right_id as usize).and_then(Option::take) else {
            return;
        };
        match right_node.kind {
            NodeKind::Leaf {
                next: right_next,
                runs: mut right_runs,
                ..
            } => {
                for lr in &right_runs {
                    self.id_map.insert(lr.run.ids.first_id, left_id);
                }
                if let Some(Some(BNode {
                    kind: NodeKind::Leaf { next, runs, .. },
                    ..
                })) = self.nodes.get_mut(left_id as usize)
                {
                    runs.append(&mut right_runs);
                    *next = right_next;
                }
                if let Some(nx) = right_next {
                    if let Some(Some(BNode {
                        kind: NodeKind::Leaf { prev, .. },
                        ..
                    })) = self.nodes.get_mut(nx as usize)
                    {
                        *prev = Some(left_id);
                    }
                } else {
                    self.last_leaf = Some(left_id);
                }
            }
            NodeKind::Internal(mut right_children) => {
                for s in &right_children {
                    self.set_parent(s.child, Some(left_id));
                }
                if let Some(Some(BNode {
                    kind: NodeKind::Internal(children),
                    ..
                })) = self.nodes.get_mut(left_id as usize)
                {
                    children.append(&mut right_children);
                }
            }
        }
        self.free_nodes.push(right_id);

        let left_summary = self
            .nodes
            .get(left_id as usize)
            .and_then(Option::as_ref)
            .map(|n| n.summary(left_id));
        if let Some(Some(BNode {
            kind: NodeKind::Internal(children),
            ..
        })) = self.nodes.get_mut(pid as usize)
        {
            if let Some(rpos) = children.iter().position(|s| s.child == right_id) {
                children.remove(rpos);
            }
            if let Some(sum) = left_summary
                && let Some(slot) = children.iter_mut().find(|s| s.child == left_id)
            {
                *slot = sum;
            }
        }
        self.recompute_ancestors(pid);
    }

    fn borrow_from_left(&mut self, left_id: u32, right_id: u32) {
        let Some(Some(left_node)) = self.nodes.get_mut(left_id as usize) else {
            return;
        };
        match &mut left_node.kind {
            NodeKind::Leaf { runs, .. } => {
                let Some(moved) = runs.pop() else {
                    return;
                };
                self.id_map.insert(moved.run.ids.first_id, right_id);
                if let Some(Some(BNode {
                    kind: NodeKind::Leaf { runs: rruns, .. },
                    ..
                })) = self.nodes.get_mut(right_id as usize)
                {
                    rruns.insert(0, moved);
                }
            }
            NodeKind::Internal(children) => {
                let Some(moved) = children.pop() else {
                    return;
                };
                self.set_parent(moved.child, Some(right_id));
                if let Some(Some(BNode {
                    kind: NodeKind::Internal(rchildren),
                    ..
                })) = self.nodes.get_mut(right_id as usize)
                {
                    rchildren.insert(0, moved);
                }
            }
        }
    }

    fn borrow_from_right(&mut self, left_id: u32, right_id: u32) {
        let Some(Some(right_node)) = self.nodes.get_mut(right_id as usize) else {
            return;
        };
        match &mut right_node.kind {
            NodeKind::Leaf { runs, .. } => {
                if runs.is_empty() {
                    return;
                }
                let moved = runs.remove(0);
                self.id_map.insert(moved.run.ids.first_id, left_id);
                if let Some(Some(BNode {
                    kind: NodeKind::Leaf { runs: lruns, .. },
                    ..
                })) = self.nodes.get_mut(left_id as usize)
                {
                    lruns.push(moved);
                }
            }
            NodeKind::Internal(children) => {
                if children.is_empty() {
                    return;
                }
                let moved = children.remove(0);
                self.set_parent(moved.child, Some(left_id));
                if let Some(Some(BNode {
                    kind: NodeKind::Internal(lchildren),
                    ..
                })) = self.nodes.get_mut(left_id as usize)
                {
                    lchildren.push(moved);
                }
            }
        }
    }

    fn find_coalesce_step(&self, cursor_leaf: u32) -> Option<CoalesceStep> {
        let Some(BNode {
            kind: NodeKind::Leaf { next, runs, .. },
            ..
        }) = self
            .nodes
            .get(cursor_leaf as usize)
            .and_then(Option::as_ref)
        else {
            return None;
        };
        for i in 0..runs.len().saturating_sub(1) {
            if let (Some(a), Some(b)) = (runs.get(i), runs.get(i.saturating_add(1)))
                && a.can_coalesce_with(b)
            {
                return Some(CoalesceStep::IntraLeaf(i));
            }
        }
        let next_lid = (*next)?;
        let last_left = runs.last()?;
        let first_right = self.leaf_run_at(next_lid, 0)?;
        if last_left.can_coalesce_with(first_right) {
            return Some(CoalesceStep::CrossLeaf {
                next_lid,
                right_first_id: first_right.run.ids.first_id,
            });
        }
        None
    }

    fn try_coalesce_around(&mut self, mut start_leaf: u32) {
        // Merge across adjacent pairs on `start_leaf` and its immediate leaf borders.
        let mut cursor_leaf = self
            .nodes
            .get(start_leaf as usize)
            .and_then(Option::as_ref)
            .and_then(|n| match &n.kind {
                NodeKind::Leaf { prev, .. } => *prev,
                NodeKind::Internal(_) => None,
            })
            .unwrap_or(start_leaf);

        let mut steps = 0_u8;
        while steps < 4 {
            steps = steps.saturating_add(1);
            let mut merged_any = false;
            while let Some(step) = self.find_coalesce_step(cursor_leaf) {
                match step {
                    CoalesceStep::IntraLeaf(i) => {
                        if let Some(Some(BNode {
                            kind: NodeKind::Leaf { runs, .. },
                            ..
                        })) = self.nodes.get_mut(cursor_leaf as usize)
                        {
                            let right = runs.remove(i.saturating_add(1));
                            self.id_map.remove(&right.run.ids.first_id);
                            if let Some(left) = runs.get_mut(i) {
                                left.absorb_right(right);
                            }
                            self.run_count = self.run_count.saturating_sub(1);
                        }
                        self.recompute_ancestors(cursor_leaf);
                        merged_any = true;
                    }
                    CoalesceStep::CrossLeaf {
                        next_lid,
                        right_first_id,
                    } => {
                        let removed_right = if let Some(Some(BNode {
                            kind: NodeKind::Leaf { runs: nr, .. },
                            ..
                        })) = self.nodes.get_mut(next_lid as usize)
                        {
                            Some(nr.remove(0))
                        } else {
                            None
                        };
                        if let Some(r) = removed_right {
                            self.id_map.remove(&right_first_id);
                            if let Some(Some(BNode {
                                kind: NodeKind::Leaf { runs: lr_vec, .. },
                                ..
                            })) = self.nodes.get_mut(cursor_leaf as usize)
                                && let Some(left) = lr_vec.last_mut()
                            {
                                left.absorb_right(r);
                            }
                            self.run_count = self.run_count.saturating_sub(1);
                            self.recompute_ancestors(cursor_leaf);
                            self.recompute_ancestors(next_lid);
                            if self.leaf_runs_len(next_lid) < LEAF_MIN {
                                let _ = self.rebalance_underflow(next_lid);
                            }
                            merged_any = true;
                        } else {
                            break;
                        }
                    }
                }
            }
            if !merged_any {
                start_leaf = match self
                    .nodes
                    .get(cursor_leaf as usize)
                    .and_then(Option::as_ref)
                {
                    Some(BNode {
                        kind: NodeKind::Leaf { next: Some(nx), .. },
                        ..
                    }) => *nx,
                    _ => break,
                };
                cursor_leaf = start_leaf;
            }
        }
        if self.node_is_live(cursor_leaf) && self.leaf_runs_len(cursor_leaf) < LEAF_MIN {
            let _ = self.rebalance_underflow(cursor_leaf);
        }
    }

    fn increment_tombstone_range(
        &mut self,
        leaf_id: u32,
        run_idx: usize,
        iv_start: u32,
        iv_end: u32,
    ) -> Result<(), AxisError> {
        let default_px = self.default_px;
        let mut live_delta = 0_u32;
        let mut px_delta = 0_u64;
        {
            let lr = self
                .leaf_run_at_mut(leaf_id, run_idx)
                .ok_or(AxisError::TargetNotFound)?;
            if lr.run.tombstones.is_none() {
                let len_usize =
                    usize::try_from(lr.run.ids.len).map_err(|_| AxisError::InvalidRun)?;
                lr.run.tombstones = Some(vec![0_u32; len_usize].into_boxed_slice());
            }
            let Some(counts) = lr.run.tombstones.as_mut() else {
                return Err(AxisError::TargetNotFound);
            };
            for off in iv_start..iv_end {
                let Some(entry) = counts.get_mut(off as usize) else {
                    return Err(AxisError::TargetNotFound);
                };
                if *entry == 0 {
                    live_delta = live_delta.saturating_add(1);
                    let epx = u64::from(match &lr.custom_px {
                        Some(v) => match v.binary_search_by_key(&off, |&(o, _)| o) {
                            Ok(i) => v.get(i).map_or(default_px, |&(_, p)| p),
                            Err(_) => default_px,
                        },
                        None => default_px,
                    });
                    px_delta = px_delta.saturating_add(epx);
                }
                *entry = entry.saturating_add(1);
            }
            lr.run.live_count = lr.run.live_count.saturating_sub(live_delta);
            lr.pixel_sum = lr.pixel_sum.saturating_sub(px_delta);
        }
        self.total_live = self.total_live.saturating_sub(live_delta);
        self.total_pixels = self.total_pixels.saturating_sub(px_delta);
        self.recompute_ancestors(leaf_id);
        Ok(())
    }

    fn decrement_tombstone_range(
        &mut self,
        leaf_id: u32,
        run_idx: usize,
        iv_start: u32,
        iv_end: u32,
    ) -> Result<(), AxisError> {
        let default_px = self.default_px;
        let mut live_delta = 0_u32;
        let mut px_delta = 0_u64;
        {
            let lr = self
                .leaf_run_at_mut(leaf_id, run_idx)
                .ok_or(AxisError::TargetNotFound)?;
            let Some(counts) = lr.run.tombstones.as_mut() else {
                return Err(AxisError::InvalidInverse);
            };
            for off in iv_start..iv_end {
                let Some(entry) = counts.get_mut(off as usize) else {
                    return Err(AxisError::TargetNotFound);
                };
                if *entry == 0 {
                    return Err(AxisError::InvalidInverse);
                }
                *entry = entry.saturating_sub(1);
                if *entry == 0 {
                    live_delta = live_delta.saturating_add(1);
                    let epx = u64::from(match &lr.custom_px {
                        Some(v) => match v.binary_search_by_key(&off, |&(o, _)| o) {
                            Ok(i) => v.get(i).map_or(default_px, |&(_, p)| p),
                            Err(_) => default_px,
                        },
                        None => default_px,
                    });
                    px_delta = px_delta.saturating_add(epx);
                }
            }
            lr.run.live_count = lr.run.live_count.saturating_add(live_delta);
            lr.pixel_sum = lr.pixel_sum.saturating_add(px_delta);
            if lr.run.live_count == lr.run.ids.len {
                lr.run.tombstones = None;
            }
        }
        self.total_live = self.total_live.saturating_add(live_delta);
        self.total_pixels = self.total_pixels.saturating_add(px_delta);
        self.recompute_ancestors(leaf_id);
        Ok(())
    }

    fn find_leaf_for_pos(&self, pos: u32) -> Option<(u32, usize, u32)> {
        if pos >= self.total_live {
            return None;
        }
        let mut cur = self.root?;
        let mut rem = pos;
        loop {
            let node = self.nodes.get(cur as usize)?.as_ref()?;
            match &node.kind {
                NodeKind::Internal(children) => {
                    let mut next_child = None;
                    for slot in children {
                        if rem < slot.live_count {
                            next_child = Some(slot.child);
                            break;
                        }
                        rem = rem.saturating_sub(slot.live_count);
                    }
                    cur = next_child?;
                }
                NodeKind::Leaf { runs, .. } => {
                    for (idx, lr) in runs.iter().enumerate() {
                        if rem < lr.run.live_count {
                            return Some((cur, idx, rem));
                        }
                        rem = rem.saturating_sub(lr.run.live_count);
                    }
                    return None;
                }
            }
        }
    }

    fn locate_id(&self, id: u64) -> Option<LocatedId> {
        let (&first_id, &leaf_id) = self.id_map.range(..=id).next_back()?;
        if id_stream(id) != id_stream(first_id) {
            return None;
        }
        let offset = id_counter(id).checked_sub(id_counter(first_id))?;
        let node = self.nodes.get(leaf_id as usize)?.as_ref()?;
        let NodeKind::Leaf { runs, .. } = &node.kind else {
            return None;
        };
        let run_idx = runs.iter().position(|lr| lr.run.ids.first_id == first_id)?;
        let lr = runs.get(run_idx)?;
        if offset >= lr.run.ids.len {
            return None;
        }

        let mut live_before = 0_u32;
        let mut order_before = 0_u32;
        for prev_lr in runs.get(..run_idx)? {
            live_before = live_before.saturating_add(prev_lr.run.live_count);
            order_before = order_before.saturating_add(prev_lr.run.ids.len);
        }

        let mut cur_id = leaf_id;
        let mut cur_parent = node.parent;
        while let Some(pid) = cur_parent {
            let pnode = self.nodes.get(pid as usize)?.as_ref()?;
            if let NodeKind::Internal(children) = &pnode.kind {
                for slot in children {
                    if slot.child == cur_id {
                        break;
                    }
                    live_before = live_before.saturating_add(slot.live_count);
                    order_before = order_before.saturating_add(slot.total_entries);
                }
            }
            cur_id = pid;
            cur_parent = pnode.parent;
        }

        Some(LocatedId {
            leaf_id,
            run_idx,
            offset,
            live_before,
            axis_order: order_before.saturating_add(offset),
        })
    }

    fn overlaps_existing(&self, candidate: IdRun) -> bool {
        if let Some((&prev_first, &lid)) = self.id_map.range(..=candidate.first_id).next_back()
            && id_stream(prev_first) == id_stream(candidate.first_id)
            && let Some(node) = self.nodes.get(lid as usize).and_then(Option::as_ref)
            && let NodeKind::Leaf { runs, .. } = &node.kind
            && let Some(lr) = runs.iter().find(|lr| lr.run.ids.first_id == prev_first)
            && Self::runs_overlap(lr.run.ids, candidate)
        {
            return true;
        }
        if let Some((&next_first, &lid)) = self
            .id_map
            .range((Excluded(candidate.first_id), Unbounded))
            .next()
            && id_stream(next_first) == id_stream(candidate.first_id)
            && let Some(node) = self.nodes.get(lid as usize).and_then(Option::as_ref)
            && let NodeKind::Leaf { runs, .. } = &node.kind
            && let Some(lr) = runs.iter().find(|lr| lr.run.ids.first_id == next_first)
            && Self::runs_overlap(lr.run.ids, candidate)
        {
            return true;
        }
        false
    }

    fn validate_target_runs(
        &self,
        targets: &[IdRun],
        require_deleted: bool,
    ) -> Result<(), AxisError> {
        for (i, target) in targets.iter().enumerate() {
            if !target.is_valid() {
                return Err(AxisError::InvalidRun);
            }
            for earlier in targets.get(..i).unwrap_or(&[]) {
                if Self::runs_overlap(*earlier, *target) {
                    return Err(AxisError::DuplicateId);
                }
            }
            let mut cursor = target.first_id;
            let mut rem = target.len;
            while rem > 0 {
                let loc = self.locate_id(cursor).ok_or(AxisError::TargetNotFound)?;
                let lr = self
                    .leaf_run_at(loc.leaf_id, loc.run_idx)
                    .ok_or(AxisError::TargetNotFound)?;
                let take = rem.min(lr.run.ids.len.saturating_sub(loc.offset));
                for off in loc.offset..loc.offset.saturating_add(take) {
                    let count = lr.run.tombstone_at(off);
                    if require_deleted && count == 0 {
                        return Err(AxisError::InvalidInverse);
                    }
                    if !require_deleted && count == u32::MAX {
                        return Err(AxisError::LimitExceeded);
                    }
                }
                cursor = cursor.saturating_add(u64::from(take));
                rem = rem.saturating_sub(take);
            }
        }
        Ok(())
    }

    fn emit_live_subruns(
        run: &AxisRun,
        from: u32,
        to_inclusive: u32,
        visit: &mut impl FnMut(IdRun),
    ) {
        if from > to_inclusive || to_inclusive >= run.ids.len {
            return;
        }
        match &run.tombstones {
            None => {
                let len = to_inclusive.saturating_sub(from).saturating_add(1);
                if let Some(first_id) = run.ids.id_at(from)
                    && let Some(sub) = IdRun::new(first_id, len)
                {
                    visit(sub);
                }
            }
            Some(counts) => {
                let mut seg_start: Option<u32> = None;
                for offset in from..=to_inclusive {
                    let deleted = counts.get(offset as usize).copied().unwrap_or(1) != 0;
                    match (seg_start, deleted) {
                        (None, false) => seg_start = Some(offset),
                        (Some(start), true) => {
                            let len = offset.saturating_sub(start);
                            if let Some(first_id) = run.ids.id_at(start)
                                && let Some(sub) = IdRun::new(first_id, len)
                            {
                                visit(sub);
                            }
                            seg_start = None;
                        }
                        _ => {}
                    }
                }
                if let Some(start) = seg_start {
                    let len = to_inclusive.saturating_sub(start).saturating_add(1);
                    if let Some(first_id) = run.ids.id_at(start)
                        && let Some(sub) = IdRun::new(first_id, len)
                    {
                        visit(sub);
                    }
                }
            }
        }
    }

    fn runs_overlap(a: IdRun, b: IdRun) -> bool {
        if id_stream(a.first_id) != id_stream(b.first_id) {
            return false;
        }
        let a_start = u64::from(id_counter(a.first_id));
        let a_end = a_start.saturating_add(u64::from(a.len));
        let b_start = u64::from(id_counter(b.first_id));
        let b_end = b_start.saturating_add(u64::from(b.len));
        a_start < b_end && b_start < a_end
    }
}

#[derive(Clone, Copy)]
struct LocatedId {
    leaf_id: u32,
    run_idx: usize,
    offset: u32,
    live_before: u32,
    axis_order: u32,
}

fn run_target_intervals(run: &AxisRun, targets: &[IdRun]) -> Vec<(u32, u32)> {
    let mut intervals: Vec<(u32, u32)> = Vec::new();
    let run_stream = id_stream(run.ids.first_id);
    let run_c0 = id_counter(run.ids.first_id);
    let run_c1 = run_c0.saturating_add(run.ids.len);
    for target in targets {
        if id_stream(target.first_id) != run_stream {
            continue;
        }
        let t_c0 = id_counter(target.first_id);
        let t_c1 = t_c0.saturating_add(target.len);
        let start_c = run_c0.max(t_c0);
        let end_c = run_c1.min(t_c1);
        if start_c < end_c {
            intervals.push((start_c.saturating_sub(run_c0), end_c.saturating_sub(run_c0)));
        }
    }
    intervals.sort_unstable_by_key(|&(s, _)| s);
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(intervals.len());
    for (s, e) in intervals {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use core::cmp::Ordering;

    use super::{Axis, AxisError, DEFAULT_BASE_ROWS, MAX_ROW_AXIS_LEN};
    use crate::id::{IdAllocator, IdRun};
    use crate::rng::Rng;

    #[test]
    fn fresh_base_axis_maps_positions_and_uses_minimal_memory() {
        let axis = Axis::with_base(DEFAULT_BASE_ROWS, MAX_ROW_AXIS_LEN);
        assert_eq!(axis.live_len(), DEFAULT_BASE_ROWS);
        assert_eq!(axis.runs().len(), 1);
        assert!(axis.estimated_bytes() < 256);
        assert_eq!(axis.pos_to_id(0), Some(0));
        assert_eq!(
            axis.pos_to_id(DEFAULT_BASE_ROWS - 1),
            Some(u64::from(DEFAULT_BASE_ROWS - 1))
        );
        assert_eq!(axis.pos_to_id(DEFAULT_BASE_ROWS), None);
        assert_eq!(axis.id_to_pos(500_000), Some(500_000));
        assert_eq!(axis.total_pixels(), u64::from(DEFAULT_BASE_ROWS) * 24);
        assert_eq!(axis.pixel_offset_of_pos(10), Some(240));
        assert_eq!(axis.pos_at_pixel(240), Some((10, 240, 24)));
        assert_eq!(axis.pos_at_pixel(263), Some((10, 240, 24)));
        assert_eq!(axis.pos_at_pixel(264), Some((11, 264, 24)));
    }

    #[test]
    fn insertions_deletions_and_inverses_match_naive_list() {
        let mut axis = Axis::with_base(10, 100);
        let mut alloc = IdAllocator::local();

        let Some(top) = alloc.alloc_run(2) else {
            panic!("alloc");
        };
        let Some(mid) = alloc.alloc_run(3) else {
            panic!("alloc");
        };
        assert_eq!(axis.insert_after(None, top), Ok(()));
        assert_eq!(axis.insert_after(Some(4), mid), Ok(()));
        assert_eq!(axis.live_len(), 15);

        let expected_ids: Vec<u64> = [
            top.first_id,
            top.first_id.saturating_add(1),
            0,
            1,
            2,
            3,
            4,
            mid.first_id,
            mid.first_id.saturating_add(1),
            mid.first_id.saturating_add(2),
            5,
            6,
            7,
            8,
            9,
        ]
        .to_vec();
        for (pos, expected) in expected_ids.iter().enumerate() {
            let pos_u32 = u32::try_from(pos).unwrap_or(0);
            assert_eq!(axis.pos_to_id(pos_u32), Some(*expected));
            assert_eq!(axis.id_to_pos(*expected), Some(pos_u32));
        }
        assert_eq!(axis.compare_anchors(top.first_id, 0), Some(Ordering::Less));
        assert_eq!(axis.compare_anchors(4, mid.first_id), Some(Ordering::Less));
        assert_eq!(
            axis.compare_anchors(5, mid.first_id),
            Some(Ordering::Greater)
        );

        let Some(del_four) = IdRun::new(4, 1) else {
            panic!("run");
        };
        assert_eq!(axis.delete_runs(&[del_four]), Ok(()));
        assert_eq!(axis.delete_runs(&[del_four]), Ok(()));
        assert_eq!(axis.id_to_pos(4), None);
        assert_eq!(axis.live_len(), 14);
        assert_eq!(axis.undelete_runs(&[del_four]), Ok(()));
        assert_eq!(axis.id_to_pos(4), None);
        assert_eq!(axis.undelete_runs(&[del_four]), Ok(()));
        assert_eq!(axis.id_to_pos(4), Some(6));
        assert_eq!(axis.live_len(), 15);

        assert_eq!(axis.remove_inserted(mid), Ok(()));
        assert_eq!(axis.remove_inserted(top), Ok(()));
        assert_eq!(axis.runs().len(), 1);
        assert_eq!(axis.live_len(), 10);
        assert_eq!(
            axis.undelete_runs(&[del_four]),
            Err(AxisError::InvalidInverse)
        );
    }

    #[test]
    fn range_iteration_skips_tombstones_and_handles_reversed_anchors() {
        let mut axis = Axis::with_base(8, 100);
        let Some(del) = IdRun::new(3, 2) else {
            panic!("run");
        };
        assert_eq!(axis.delete_runs(&[del]), Ok(()));

        let mut forward = Vec::new();
        assert!(axis.for_each_live_in_range(1, 6, |run| forward.extend(run.iter())));
        assert_eq!(forward, [1, 2, 5, 6]);

        let mut backward = Vec::new();
        assert!(axis.for_each_live_in_range(6, 1, |run| backward.extend(run.iter())));
        assert_eq!(backward, forward);
    }

    #[test]
    fn deleting_row_on_1m_base_run_avoids_megabyte_tombstone_allocation() {
        let mut axis = Axis::with_base(1_048_576, 2_000_000);
        let Some(del) = IdRun::new(3, 1) else {
            panic!("run");
        };
        assert_eq!(axis.delete_runs(&[del]), Ok(()));
        assert_eq!(axis.live_len(), 1_048_575);
        let allocated_tombstones: usize = axis
            .runs()
            .filter_map(|r| r.tombstones.as_ref().map(|t| t.len()))
            .sum();
        assert_eq!(allocated_tombstones, 1);
        assert_eq!(axis.undelete_runs(&[del]), Ok(()));
        assert_eq!(axis.runs().len(), 1);
        assert!(
            axis.first_run()
                .is_some_and(|r| r.tombstones.is_none() && r.live_count == 1_048_576)
        );
    }

    #[test]
    fn pixel_lookup_and_btree_splits_match_reference_model() {
        fn pick_idx(rng: &mut Rng, len: usize) -> usize {
            let m = u64::try_from(len).unwrap_or(1).max(1);
            usize::try_from(rng.next_u64().checked_rem(m).unwrap_or(0)).unwrap_or(0)
        }

        let mut axis = Axis::with_base_px(40, 20_000, 24);
        let mut alloc = IdAllocator::local();
        let mut rng = Rng::from_seed(0x9E37_79B9_7F4A_7C15);

        // Naive reference: (id, tombstones, px)
        let mut model: Vec<(u64, u32, u16)> = (0..40_u64).map(|id| (id, 0, 24)).collect();
        let mut inserted_stack: Vec<IdRun> = Vec::new();

        for step in 0..1_500_u32 {
            let op = rng.next_u64().checked_rem(6).unwrap_or(0);
            if op == 0 || op == 1 {
                let len = u32::try_from(rng.next_u64().checked_rem(3).unwrap_or(0))
                    .unwrap_or(0)
                    .saturating_add(1);
                if let Some(run) = alloc.alloc_run(len) {
                    let anchor_idx = if model.is_empty() || rng.next_u64().is_multiple_of(5) {
                        None
                    } else {
                        let pick = pick_idx(&mut rng, model.len());
                        model.get(pick).map(|&(id, _, _)| id)
                    };
                    assert_eq!(axis.insert_after(anchor_idx, run), Ok(()));
                    let insert_pos = match anchor_idx {
                        None => 0,
                        Some(aid) => model
                            .iter()
                            .position(|&(id, _, _)| id == aid)
                            .map_or(model.len(), |p| p.saturating_add(1)),
                    };
                    for offset in 0..len {
                        let id = run.id_at(offset).unwrap_or(0);
                        model.insert(insert_pos.saturating_add(offset as usize), (id, 0, 24));
                    }
                    inserted_stack.push(run);
                }
            } else if op == 2 && !model.is_empty() {
                let idx = pick_idx(&mut rng, model.len());
                let (id, _, _) = model[idx];
                if let Some(del_run) = IdRun::new(id, 1) {
                    assert_eq!(axis.delete_runs(&[del_run]), Ok(()));
                    model[idx].1 = model[idx].1.saturating_add(1);
                }
            } else if op == 3 && !model.is_empty() {
                let idx = pick_idx(&mut rng, model.len());
                let (id, tomb, _) = model[idx];
                if tomb > 0
                    && let Some(del_run) = IdRun::new(id, 1)
                {
                    assert_eq!(axis.undelete_runs(&[del_run]), Ok(()));
                    model[idx].1 = model[idx].1.saturating_sub(1);
                }
            } else if op == 4
                && let Some(run) = inserted_stack.pop()
            {
                let start_pos = model
                    .iter()
                    .position(|&(id, tomb, _)| id == run.first_id && tomb == 0);
                let intact = start_pos.is_some_and(|p| {
                    let len = run.len as usize;
                    p.saturating_add(len) <= model.len()
                        && (0..run.len).all(|off| {
                            model[p.saturating_add(off as usize)].0
                                == run.id_at(off).unwrap_or(u64::MAX)
                                && model[p.saturating_add(off as usize)].1 == 0
                        })
                });
                if intact && let Some(p) = start_pos {
                    assert_eq!(axis.remove_inserted(run), Ok(()));
                    model.drain(p..p.saturating_add(run.len as usize));
                }
            } else if !model.is_empty() {
                let idx = pick_idx(&mut rng, model.len());
                let (id, _, _) = model[idx];
                let px = if rng.next_u64().is_multiple_of(3) {
                    None
                } else {
                    Some(16_u16.saturating_add(
                        u16::try_from(rng.next_u64().checked_rem(60).unwrap_or(0)).unwrap_or(0),
                    ))
                };
                assert_eq!(axis.set_entry_px(id, px), Ok(()));
                model[idx].2 = px.unwrap_or(24);
            }

            if step.is_multiple_of(50) || step == 1_499 {
                let live_model: Vec<(u64, u16)> = model
                    .iter()
                    .filter(|&&(_, tomb, _)| tomb == 0)
                    .map(|&(id, _, px)| (id, px))
                    .collect();
                assert_eq!(axis.live_len() as usize, live_model.len());
                let expected_total_px: u64 = live_model.iter().map(|&(_, px)| u64::from(px)).sum();
                assert_eq!(axis.total_pixels(), expected_total_px);

                let mut cum_px = 0_u64;
                let mut seen_ids = BTreeMap::new();
                for (pos, &(id, px)) in live_model.iter().enumerate() {
                    let pos_u32 = u32::try_from(pos).unwrap_or(0);
                    assert_eq!(axis.pos_to_id(pos_u32), Some(id));
                    assert_eq!(axis.id_to_pos(id), Some(pos_u32));
                    assert_eq!(axis.pixel_offset_of_pos(pos_u32), Some(cum_px));
                    assert_eq!(axis.pos_at_pixel(cum_px), Some((pos_u32, cum_px, px)));
                    assert_eq!(
                        axis.pos_at_pixel(cum_px.saturating_add(u64::from(px)).saturating_sub(1)),
                        Some((pos_u32, cum_px, px))
                    );
                    cum_px = cum_px.saturating_add(u64::from(px));
                    seen_ids.insert(id, pos_u32);
                }
                assert_eq!(axis.pos_at_pixel(expected_total_px), None);
                assert_eq!(seen_ids.len(), live_model.len());
            }
        }
    }

    #[test]
    fn btree_multilevel_splits_and_remove_inserted_roundtrip() {
        let mut axis = Axis::with_base_px(1, 20_000, 24);
        let mut inserted = Vec::new();
        // Use gaps between inserted IDs so none coalesce, forcing >1,500 distinct
        // leaves/runs and multi-level internal node splits and underflows.
        for i in 1..=1_500_u64 {
            let first = 10_000_u64.saturating_add(i.saturating_mul(4));
            let run = IdRun::new(first, 1).unwrap();
            let anchor = inserted.last().map(|r: &IdRun| r.first_id).or(Some(0));
            assert_eq!(axis.insert_after(anchor, run), Ok(()));
            if i.is_multiple_of(7) {
                assert_eq!(axis.set_entry_px(run.first_id, Some(40)), Ok(()));
            }
            inserted.push(run);
        }
        assert_eq!(axis.live_len(), 1_501);
        assert!(axis.runs().len() > 1_024);

        // Remove 1,200 runs from alternating ends to exercise internal merges/borrows.
        for idx in (0..1_200_u32).rev() {
            let run = inserted.remove(if idx.is_multiple_of(2) {
                0
            } else {
                inserted.len().saturating_sub(1)
            });
            assert_eq!(axis.remove_inserted(run), Ok(()));
        }
        assert_eq!(axis.live_len(), 301);
        assert_eq!(axis.pos_to_id(0), Some(0));
        for (pos, run) in inserted.iter().enumerate() {
            let pos_u32 = u32::try_from(pos.saturating_add(1)).unwrap_or(0);
            assert_eq!(axis.pos_to_id(pos_u32), Some(run.first_id));
            assert_eq!(axis.id_to_pos(run.first_id), Some(pos_u32));
        }
    }
}
