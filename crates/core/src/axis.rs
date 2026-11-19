//! Run-encoded axis of stable row or column identities.
//!
//! A fresh worksheet begins with one implicit run of `1,048,576` rows (or
//! `16,384` columns), costing a few dozen bytes. Insertions split at most one
//! run and insert a new run; deletions increment a per-entry tombstone counter
//! inside the affected run rather than removing the entry, so range anchors and
//! concurrent undo behave deterministically.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

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

/// Entry in the sorted identity lookup index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdIndexEntry {
    first_id: u64,
    len: u32,
    run_idx: u32,
    axis_order_start: u32,
}

/// Ordered axis of rows or columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Axis {
    runs: Vec<AxisRun>,
    /// Cumulative live count strictly before each run (`len == runs.len()`).
    live_prefix: Vec<u32>,
    /// Cumulative total entry count (live + tombstones) strictly before each run.
    order_prefix: Vec<u32>,
    total_live: u32,
    total_entries: u32,
    max_entries: u32,
    /// Sorted by `first_id` for `O(log R)` identity lookup with minimal RAM.
    id_index: Vec<IdIndexEntry>,
}

impl Default for Axis {
    fn default() -> Self {
        Self::empty(MAX_ROW_AXIS_LEN)
    }
}

impl Axis {
    /// Creates an empty axis with an upper bound on total entries.
    #[must_use]
    pub const fn empty(max_entries: u32) -> Self {
        Self {
            runs: Vec::new(),
            live_prefix: Vec::new(),
            order_prefix: Vec::new(),
            total_live: 0,
            total_entries: 0,
            max_entries,
            id_index: Vec::new(),
        }
    }

    /// Creates an axis holding a single base run `(0, 0, 0..base_len)`.
    #[must_use]
    pub fn with_base(base_len: u32, max_entries: u32) -> Self {
        let clamped = base_len.min(max_entries);
        let Some(ids) = IdRun::new(0, clamped) else {
            return Self::empty(max_entries);
        };
        let mut axis = Self {
            runs: vec![AxisRun::live(ids)],
            live_prefix: Vec::new(),
            order_prefix: Vec::new(),
            total_live: 0,
            total_entries: 0,
            max_entries,
            id_index: Vec::new(),
        };
        axis.rebuild_index();
        axis
    }

    /// Reconstructs an axis from persisted runs after validating invariants.
    pub fn from_runs(runs: Vec<AxisRun>, max_entries: u32) -> Result<Self, AxisError> {
        let mut total: u32 = 0;
        for run in &runs {
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
        }
        let mut axis = Self {
            runs,
            live_prefix: Vec::new(),
            order_prefix: Vec::new(),
            total_live: 0,
            total_entries: 0,
            max_entries,
            id_index: Vec::new(),
        };
        axis.rebuild_index();
        // Check that sorted runs do not overlap in ID space.
        for pair in axis.id_index.windows(2) {
            if let [left, right] = pair
                && id_stream(left.first_id) == id_stream(right.first_id)
            {
                let left_end = id_counter(left.first_id).saturating_add(left.len);
                if left_end > id_counter(right.first_id) {
                    return Err(AxisError::DuplicateId);
                }
            }
        }
        Ok(axis)
    }

    /// Runs in axis order, for snapshot serialisation and digests.
    #[must_use]
    pub fn runs(&self) -> &[AxisRun] {
        &self.runs
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

    /// Approximate heap memory owned by this axis, in bytes.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let runs_mem = self.runs.capacity().saturating_mul(size_of::<AxisRun>());
        let prefix_mem = self
            .live_prefix
            .capacity()
            .saturating_add(self.order_prefix.capacity())
            .saturating_mul(size_of::<u32>());
        let index_mem = self
            .id_index
            .capacity()
            .saturating_mul(size_of::<IdIndexEntry>());
        let tombstones_mem = self
            .runs
            .iter()
            .fold(0usize, |acc, run| acc.saturating_add(run.estimated_bytes()));
        runs_mem
            .saturating_add(prefix_mem)
            .saturating_add(index_mem)
            .saturating_add(tombstones_mem)
    }

    /// Looks up the 0-based visible position of `id`, returning `None` if `id`
    /// does not exist or is currently deleted.
    #[must_use]
    pub fn id_to_pos(&self, id: u64) -> Option<u32> {
        let loc = self.locate_id(id)?;
        let run = self.runs.get(loc.run_idx as usize)?;
        if run.tombstone_at(loc.offset) != 0 {
            return None;
        }
        let before = self.live_prefix.get(loc.run_idx as usize).copied()?;
        before.checked_add(run.live_before(loc.offset))
    }

    /// Whether `id` exists on the axis (live or deleted).
    #[must_use]
    pub fn contains_id(&self, id: u64) -> bool {
        self.locate_id(id).is_some()
    }

    /// Looks up the identity at 0-based visible position `pos`.
    #[must_use]
    pub fn pos_to_id(&self, pos: u32) -> Option<u64> {
        if pos >= self.total_live {
            return None;
        }
        // Last run whose `live_prefix <= pos` and that has at least one live item
        // covering `pos`: `partition_point(|&p| p <= pos) - 1` can land on a
        // trailing zero-live run, so walk back if `live_count == 0` (or better:
        // search on `live_prefix[i] + runs[i].live_count <= pos`!).
        let run_idx = self.runs.partition_point(|_| false);
        let _ = run_idx;
        let idx = self.find_run_for_pos(pos)?;
        let run = self.runs.get(idx)?;
        let prefix = self.live_prefix.get(idx).copied()?;
        let within = pos.checked_sub(prefix)?;
        let offset = run.offset_of_live(within)?;
        run.ids.id_at(offset)
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
        self.runs
            .get(loc.run_idx as usize)
            .is_some_and(|run| run.tombstone_at(loc.offset) == 0)
    }

    /// Resolves a 0-based axis-order index (`0..total_len()`, tombstones
    /// included) to `(id, is_live)` in `O(log R)`.
    #[must_use]
    pub fn id_at_axis_order(&self, order: u32) -> Option<(u64, bool)> {
        if order >= self.total_entries {
            return None;
        }
        let run_idx = self
            .order_prefix
            .partition_point(|&start| start <= order)
            .checked_sub(1)?;
        let run = self.runs.get(run_idx)?;
        let start = self.order_prefix.get(run_idx).copied()?;
        let offset = order.checked_sub(start)?;
        let id = run.ids.id_at(offset)?;
        Some((id, run.tombstone_at(offset) == 0))
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
        for run_idx in start.run_idx..=end.run_idx {
            let Some(run) = self.runs.get(run_idx as usize) else {
                continue;
            };
            if run.live_count == 0 {
                continue;
            }
            let from = if run_idx == start.run_idx {
                start.offset
            } else {
                0
            };
            let to_inclusive = if run_idx == end.run_idx {
                end.offset
            } else {
                run.ids.len.saturating_sub(1)
            };
            Self::emit_live_subruns(run, from, to_inclusive, &mut visit);
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
        let Some(mut run_idx) = self.find_run_for_pos(first_pos) else {
            return;
        };
        let mut skip_in_run =
            first_pos.saturating_sub(self.live_prefix.get(run_idx).copied().unwrap_or(0));

        while remaining > 0 {
            let Some(run) = self.runs.get(run_idx) else {
                break;
            };
            let available = run.live_count.saturating_sub(skip_in_run);
            if available > 0 {
                let take = available.min(remaining);
                if let (Some(from), Some(to)) = (
                    run.offset_of_live(skip_in_run),
                    run.offset_of_live(skip_in_run.saturating_add(take.saturating_sub(1))),
                ) {
                    Self::emit_live_subruns(run, from, to, &mut visit);
                }
                remaining = remaining.saturating_sub(take);
            }
            skip_in_run = 0;
            let Some(next) = run_idx.checked_add(1) else {
                break;
            };
            run_idx = next;
        }
    }

    /// Inserts `ids` directly after `after` (`None` inserts at the very start
    /// of the axis). Validated atomically before mutating state.
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

        match after {
            None => {
                self.runs.insert(0, AxisRun::live(ids));
            }
            Some(anchor) => {
                let loc = self.locate_id(anchor).ok_or(AxisError::AnchorNotFound)?;
                let idx = loc.run_idx as usize;
                let Some(existing) = self.runs.get(idx).cloned() else {
                    return Err(AxisError::AnchorNotFound);
                };
                let left_len = loc.offset.saturating_add(1);
                if left_len == existing.ids.len {
                    let insert_at = idx.saturating_add(1);
                    self.runs.insert(insert_at, AxisRun::live(ids));
                } else {
                    let (left, right) = existing.split_at(left_len).ok_or(AxisError::InvalidRun)?;
                    if let Some(slot) = self.runs.get_mut(idx) {
                        *slot = left;
                    }
                    let insert_at = idx.saturating_add(1);
                    self.runs.insert(insert_at, AxisRun::live(ids));
                    self.runs.insert(insert_at.saturating_add(1), right);
                }
            }
        }

        self.coalesce_adjacent_runs();
        self.rebuild_index();
        Ok(())
    }

    /// Physically removes an inserted `IdRun` (the exact inverse of
    /// [`Self::insert_after`]). All entries in `ids` must exist consecutively.
    pub fn remove_inserted(&mut self, ids: IdRun) -> Result<(), AxisError> {
        if !ids.is_valid() {
            return Err(AxisError::InvalidRun);
        }
        // Every ID in `ids` must currently exist on the axis.
        let first_loc = self
            .locate_id(ids.first_id)
            .ok_or(AxisError::TargetNotFound)?;
        let idx = first_loc.run_idx as usize;
        let Some(existing) = self.runs.get(idx).cloned() else {
            return Err(AxisError::TargetNotFound);
        };
        let end_offset = first_loc
            .offset
            .checked_add(ids.len)
            .ok_or(AxisError::InvalidInverse)?;
        if end_offset > existing.ids.len {
            return Err(AxisError::InvalidInverse);
        }
        // Removed entries must not carry tombstones from other operations.
        for offset in first_loc.offset..end_offset {
            if existing.tombstone_at(offset) != 0 {
                return Err(AxisError::InvalidInverse);
            }
        }

        if first_loc.offset == 0 && end_offset == existing.ids.len {
            self.runs.remove(idx);
        } else if first_loc.offset == 0 {
            let (_, right) = existing
                .split_at(end_offset)
                .ok_or(AxisError::InvalidInverse)?;
            if let Some(slot) = self.runs.get_mut(idx) {
                *slot = right;
            }
        } else if end_offset == existing.ids.len {
            let (left, _) = existing
                .split_at(first_loc.offset)
                .ok_or(AxisError::InvalidInverse)?;
            if let Some(slot) = self.runs.get_mut(idx) {
                *slot = left;
            }
        } else {
            let (left, rest) = existing
                .split_at(first_loc.offset)
                .ok_or(AxisError::InvalidInverse)?;
            let (_, right) = rest.split_at(ids.len).ok_or(AxisError::InvalidInverse)?;
            if let Some(slot) = self.runs.get_mut(idx) {
                *slot = left;
            }
            self.runs.insert(idx.saturating_add(1), right);
        }

        self.coalesce_adjacent_runs();
        self.rebuild_index();
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
            for id in target.iter() {
                self.adjust_tombstone(id, true)?;
            }
        }
        self.rebuild_index();
        Ok(())
    }

    /// Decrements the deletion count of every identity in `targets` (inverse of
    /// [`Self::delete_runs`]). Fails atomically if any target has count `0`.
    pub fn undelete_runs(&mut self, targets: &[IdRun]) -> Result<(), AxisError> {
        self.validate_target_runs(targets, true)?;
        for target in targets {
            for id in target.iter() {
                self.adjust_tombstone(id, false)?;
            }
        }
        self.rebuild_index();
        Ok(())
    }

    /// Moves all entries in `targets` (preserving their current relative axis
    /// order and tombstone counts) to sit immediately after `after` (`None`
    /// places them at the very start of the axis). Returns the prior `runs`
    /// slice so [`Self::restore_runs`] can invert the move in `O(runs)`.
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

        let prior = self.runs.clone();
        let mut staying_runs: Vec<AxisRun> = Vec::with_capacity(self.runs.len().saturating_add(4));
        let mut moved_runs: Vec<AxisRun> = Vec::with_capacity(targets.len().saturating_add(2));

        for run in &self.runs {
            // Fast path: untouched runs (e.g. a 1,000,000-row base segment not
            // intersecting `targets`) are copied in O(1) without per-item work.
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
            if intervals.is_empty() {
                staying_runs.push(run.clone());
                continue;
            }
            intervals.sort_unstable_by_key(|&(s, _)| s);
            let mut cursor: u32 = 0;
            for (iv_start, iv_end) in intervals {
                if cursor < iv_start
                    && let Some(stay_piece) = run.slice(cursor, iv_start)
                {
                    staying_runs.push(stay_piece);
                }
                if let Some(moved_piece) = run.slice(iv_start, iv_end) {
                    moved_runs.push(moved_piece);
                }
                cursor = iv_end;
            }
            if cursor < run.ids.len
                && let Some(tail_piece) = run.slice(cursor, run.ids.len)
            {
                staying_runs.push(tail_piece);
            }
        }

        match after {
            None => {
                let mut combined =
                    Vec::with_capacity(moved_runs.len().saturating_add(staying_runs.len()));
                combined.extend(moved_runs);
                combined.extend(staying_runs);
                self.runs = combined;
            }
            Some(anchor) => {
                let mut anchor_pos: Option<(usize, u32)> = None;
                for (idx, run) in staying_runs.iter().enumerate() {
                    if run.ids.contains(anchor) {
                        let off = id_counter(anchor).saturating_sub(id_counter(run.ids.first_id));
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
                if left_len == target_run.ids.len {
                    let insert_at = idx.saturating_add(1);
                    staying_runs.splice(insert_at..insert_at, moved_runs);
                } else {
                    let (left, right) =
                        target_run.split_at(left_len).ok_or(AxisError::InvalidRun)?;
                    if let Some(slot) = staying_runs.get_mut(idx) {
                        *slot = left;
                    }
                    let insert_at = idx.saturating_add(1);
                    moved_runs.push(right);
                    staying_runs.splice(insert_at..insert_at, moved_runs);
                }
                self.runs = staying_runs;
            }
        }

        self.coalesce_adjacent_runs();
        self.rebuild_index();
        Ok(prior)
    }

    /// Restores `runs` saved prior to [`Self::move_runs`].
    pub fn restore_runs(&mut self, runs: Vec<AxisRun>) -> Result<(), AxisError> {
        let restored = Self::from_runs(runs, self.max_entries)?;
        *self = restored;
        Ok(())
    }

    /// Physically removes all tombstoned entries (`tombstone > 0`) from this
    /// axis during local snapshot garbage collection (`14-m1b.md` 5.5),
    /// returning the purged identities in deterministic order.
    pub fn purge_tombstones(&mut self) -> Vec<u64> {
        let mut purged = Vec::new();
        let mut live_runs: Vec<AxisRun> = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            match &run.tombstones {
                None => live_runs.push(run.clone()),
                Some(counts) => {
                    for (idx, &cnt) in counts.iter().enumerate() {
                        if cnt > 0
                            && let Ok(off) = u32::try_from(idx)
                            && let Some(id) = run.ids.id_at(off)
                        {
                            purged.push(id);
                        }
                    }
                    if run.live_count > 0 {
                        let to = run.ids.len.saturating_sub(1);
                        Self::emit_live_subruns(run, 0, to, &mut |sub| {
                            live_runs.push(AxisRun::live(sub));
                        });
                    }
                }
            }
        }
        self.runs = live_runs;
        self.coalesce_adjacent_runs();
        self.rebuild_index();
        purged
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
            // Disallow overlapping runs within the same operation so deletion
            // counts reflect one logical action per target.
            for earlier in targets.get(..i).unwrap_or(&[]) {
                if Self::runs_overlap(*earlier, *target) {
                    return Err(AxisError::DuplicateId);
                }
            }
            for id in target.iter() {
                let loc = self.locate_id(id).ok_or(AxisError::TargetNotFound)?;
                let run = self
                    .runs
                    .get(loc.run_idx as usize)
                    .ok_or(AxisError::TargetNotFound)?;
                let count = run.tombstone_at(loc.offset);
                if require_deleted && count == 0 {
                    return Err(AxisError::InvalidInverse);
                }
                if !require_deleted && count == u32::MAX {
                    return Err(AxisError::LimitExceeded);
                }
            }
        }
        Ok(())
    }

    fn adjust_tombstone(&mut self, id: u64, increment: bool) -> Result<(), AxisError> {
        let loc = self.locate_id(id).ok_or(AxisError::TargetNotFound)?;
        let run = self
            .runs
            .get_mut(loc.run_idx as usize)
            .ok_or(AxisError::TargetNotFound)?;
        if run.tombstones.is_none() {
            if !increment {
                return Err(AxisError::InvalidInverse);
            }
            run.tombstones = Some(vec![0_u32; run.ids.len as usize].into_boxed_slice());
        }
        let Some(counts) = run.tombstones.as_mut() else {
            return Err(AxisError::TargetNotFound);
        };
        let Some(entry) = counts.get_mut(loc.offset as usize) else {
            return Err(AxisError::TargetNotFound);
        };
        if increment {
            if *entry == 0 {
                run.live_count = run.live_count.saturating_sub(1);
            }
            *entry = entry.saturating_add(1);
        } else {
            if *entry == 0 {
                return Err(AxisError::InvalidInverse);
            }
            *entry = entry.saturating_sub(1);
            if *entry == 0 {
                run.live_count = run.live_count.saturating_add(1);
            }
        }
        if run.live_count == run.ids.len {
            run.tombstones = None;
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

    /// Merges adjacent runs that form one contiguous ID stream and are both
    /// completely live (so undo of an insertion restores the exact single run).
    fn coalesce_adjacent_runs(&mut self) {
        let mut i = 0usize;
        while i.saturating_add(1) < self.runs.len() {
            let can_merge = match (self.runs.get(i), self.runs.get(i.saturating_add(1))) {
                (Some(left), Some(right)) => {
                    left.tombstones.is_none()
                        && right.tombstones.is_none()
                        && id_stream(left.ids.first_id) == id_stream(right.ids.first_id)
                        && id_counter(left.ids.first_id).checked_add(left.ids.len)
                            == Some(id_counter(right.ids.first_id))
                }
                _ => false,
            };
            if can_merge {
                let removed = self.runs.remove(i.saturating_add(1));
                if let Some(left) = self.runs.get_mut(i) {
                    let combined_len = left.ids.len.saturating_add(removed.ids.len);
                    if let Some(merged) = IdRun::new(left.ids.first_id, combined_len) {
                        *left = AxisRun::live(merged);
                    }
                }
            } else {
                i = i.saturating_add(1);
            }
        }
    }

    fn find_run_for_pos(&self, pos: u32) -> Option<usize> {
        if pos >= self.total_live {
            return None;
        }
        // Find the first run whose cumulative end `live_prefix[i] + live_count > pos`.
        let mut low = 0usize;
        let mut high = self.runs.len();
        while low < high {
            let mid = low.saturating_add(high.saturating_sub(low) / 2);
            let prefix = self.live_prefix.get(mid).copied().unwrap_or(0);
            let live = self.runs.get(mid).map_or(0, |r| r.live_count);
            if prefix.saturating_add(live) <= pos {
                low = mid.saturating_add(1);
            } else {
                high = mid;
            }
        }
        if low < self.runs.len() {
            Some(low)
        } else {
            None
        }
    }

    fn locate_id(&self, id: u64) -> Option<LocatedId> {
        let candidate = self
            .id_index
            .partition_point(|entry| entry.first_id <= id)
            .checked_sub(1)?;
        let entry = self.id_index.get(candidate)?;
        if id_stream(id) != id_stream(entry.first_id) {
            return None;
        }
        let offset = id_counter(id).checked_sub(id_counter(entry.first_id))?;
        if offset >= entry.len {
            return None;
        }
        Some(LocatedId {
            run_idx: entry.run_idx,
            offset,
            axis_order: entry.axis_order_start.saturating_add(offset),
        })
    }

    fn overlaps_existing(&self, candidate: IdRun) -> bool {
        self.id_index.iter().any(|entry| {
            IdRun::new(entry.first_id, entry.len)
                .is_some_and(|existing| Self::runs_overlap(existing, candidate))
        })
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

    fn rebuild_index(&mut self) {
        self.live_prefix.clear();
        self.order_prefix.clear();
        self.id_index.clear();
        self.live_prefix.reserve(self.runs.len());
        self.order_prefix.reserve(self.runs.len());
        self.id_index.reserve(self.runs.len());

        let mut live_acc: u32 = 0;
        let mut order_acc: u32 = 0;
        for (idx, run) in self.runs.iter().enumerate() {
            self.live_prefix.push(live_acc);
            self.order_prefix.push(order_acc);
            let run_idx = u32::try_from(idx).unwrap_or(u32::MAX);
            self.id_index.push(IdIndexEntry {
                first_id: run.ids.first_id,
                len: run.ids.len,
                run_idx,
                axis_order_start: order_acc,
            });
            live_acc = live_acc.saturating_add(run.live_count);
            order_acc = order_acc.saturating_add(run.ids.len);
        }
        self.total_live = live_acc;
        self.total_entries = order_acc;
        self.id_index.sort_unstable_by_key(|entry| entry.first_id);
    }
}

#[derive(Clone, Copy)]
struct LocatedId {
    run_idx: u32,
    offset: u32,
    axis_order: u32,
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::vec::Vec;
    use core::cmp::Ordering;

    use super::{Axis, AxisError, DEFAULT_BASE_ROWS, MAX_ROW_AXIS_LEN};
    use crate::id::{IdAllocator, IdRun};

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
    }

    #[test]
    fn insertions_deletions_and_inverses_match_naive_list() {
        let mut axis = Axis::with_base(10, 100);
        let mut alloc = IdAllocator::local();

        // Insert 2 rows at the top, and 3 rows after base row 4 (which is 5th base row).
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

        // Double delete row 4 (E18): one undelete keeps it deleted; second restores it.
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

        // Inverse of insertions restores the exact single base run!
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
}
