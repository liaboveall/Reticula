//! Dynamic array spill materialization, conflict detection, and viewport
//! overlay tracking (`04-engine.md` E22, `17-m3a.md`).
//!
//! Uses order-independent symmetric `#SPILL!` conflict resolution: when two
//! dynamic array formulas' footprints overlap with no static obstacle, both
//! anchors transition to `#SPILL!` and watch the overlapping coordinates so
//! clearing either blocker pops the remaining spill open automatically.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{Workbook, WorkbookCellKey};
use reticula_core::sheet::Sheet;
use reticula_core::value::{CalcError, Value};
use reticula_core::viewport::{FLAG_SPILL, OverlayValue};

use crate::binder::BoundFormula;
use crate::value_eval::{Array2D, EvalScalar};

/// Materialized dynamic-array spill anchored at one formula cell.
#[derive(Clone, Debug, PartialEq)]
pub struct SpillEntry {
    /// Ordered live row IDs covered by the spill (`len == array.rows`).
    pub rows: Vec<RowId>,
    /// Ordered live column IDs covered by the spill (`len == array.cols`).
    pub cols: Vec<ColId>,
    /// Row-major evaluated array (`rows.len() * cols.len() == array.values.len()`).
    pub array: Array2D,
}

/// Blocked spill state tracked so clearing any static or dynamic obstacle can
/// immediately wake waiting formulas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedSpill {
    /// Watched shadow coordinates (empty when overflowed sheet bounds).
    pub footprint: Vec<WorkbookCellKey>,
    /// `true` when blocked by a static non-empty cell, formula cell, or grid
    /// bounds; `false` when blocked solely by dynamic spill collision(s).
    pub blocked_by_static: bool,
}

/// Outcome of clearing an anchor's active or blocked spill state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearSpillOutcome {
    /// Shadow cells (`!= anchor`) previously materialized by `anchor` that are now empty.
    pub cleared_shadows: Vec<WorkbookCellKey>,
    /// Blocked anchors waiting on any cell vacated by `anchor`.
    pub unblocked_watchers: Vec<WorkbookCellKey>,
    /// Whether `anchor` previously held an active spill or blocked registration.
    pub had_state: bool,
}

/// Relative dependency ordering between two colliding dynamic spill anchors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillCollisionOrder {
    /// Colliding anchor is strictly upstream of the committing anchor (`other ->* anchor`),
    /// so `anchor` yields cleanly (`#SPILL!`) without revoking `other`.
    OtherUpstream,
    /// Committing anchor is strictly upstream of the colliding anchor (`anchor ->* other`),
    /// so `anchor` evicts/wakes `other`.
    AnchorUpstream,
    /// Independent or mutually dependent anchors; symmetric `#SPILL!` applies.
    Symmetric,
}

/// Outcome of committing an [`Array2D`] at `anchor`.
#[derive(Clone, Debug, PartialEq)]
pub enum CommitSpillOutcome {
    /// Array fit without obstacles (including `1×1` dynamic array outputs).
    Materialized {
        /// Scalar value at `(0, 0)` written to the anchor cell on `Sheet`.
        anchor_scalar: EvalScalar,
        /// Shadow coordinates (`!= anchor`) whose materialized value changed or
        /// was removed compared to the previous spill of `anchor`.
        changed_shadows: Vec<WorkbookCellKey>,
        /// Previously blocked anchors watching any shadow cell vacated by shrinkage.
        unblocked_watchers: Vec<WorkbookCellKey>,
        /// Strictly downstream active anchors evicted so `anchor` could materialize.
        displaced_anchors: Vec<WorkbookCellKey>,
    },
    /// Array failed (`#SPILL!`) due to bounds overflow, static obstacle, or
    /// symmetric dynamic spill collision.
    Blocked {
        /// Active anchors revoked to `#SPILL!` due to symmetric collision.
        revoked_anchors: Vec<WorkbookCellKey>,
        /// Shadow cells vacated across `anchor` and any revoked colliding anchors.
        freed_shadows: Vec<WorkbookCellKey>,
        /// Blocked anchors watching vacated cells that may now pop open.
        unblocked_watchers: Vec<WorkbookCellKey>,
    },
}

/// Registry tracking active dynamic-array spills, symmetric blockers, and
/// per-column shadow values for fast viewport and formula lookups.
#[derive(Clone, Debug, Default)]
pub struct SpillRegistry {
    anchors: BTreeMap<WorkbookCellKey, SpillEntry>,
    cell_to_anchor: BTreeMap<WorkbookCellKey, WorkbookCellKey>,
    sheet_col_shadows: BTreeMap<(SheetId, ColId, RowId), EvalScalar>,
    blocked_anchors: BTreeMap<WorkbookCellKey, BlockedSpill>,
    blocked_watch: BTreeMap<WorkbookCellKey, BTreeSet<WorkbookCellKey>>,
    sheet_spill_counts: BTreeMap<SheetId, u32>,
}

impl SpillRegistry {
    /// Creates an empty spill registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the registry contains zero active and blocked spills.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty() && self.blocked_anchors.is_empty()
    }

    /// O(1) when empty, else O(log S) check for whether `sheet_id` has any active materialized spills.
    #[must_use]
    pub fn has_spills_on_sheet(&self, sheet_id: SheetId) -> bool {
        if self.sheet_spill_counts.is_empty() {
            return false;
        }
        self.sheet_spill_counts
            .get(&sheet_id)
            .is_some_and(|&count| count > 0)
    }

    /// O(log N) check for whether `(sheet_id, col)` holds any spilled shadow cells.
    #[must_use]
    pub fn has_shadows_in_col(&self, sheet_id: SheetId, col: ColId) -> bool {
        if !self.has_spills_on_sheet(sheet_id) {
            return false;
        }
        let lo = (sheet_id, col, RowId(0));
        let hi = (sheet_id, col, RowId(u64::MAX));
        self.sheet_col_shadows.range(lo..=hi).next().is_some()
    }

    /// Returns the active [`SpillEntry`] anchored at `anchor`, if materialized.
    #[must_use]
    pub fn active_spill(&self, anchor: WorkbookCellKey) -> Option<&SpillEntry> {
        self.anchors.get(&anchor)
    }

    /// Returns whether `anchor` is currently blocked (`#SPILL!`).
    #[must_use]
    pub fn is_blocked_anchor(&self, anchor: WorkbookCellKey) -> bool {
        self.blocked_anchors.contains_key(&anchor)
    }

    /// Returns the watched shadow footprint of `anchor` when blocked solely
    /// by dynamic spill collisions (`blocked_by_static == false`).
    #[must_use]
    pub fn dynamic_blocked_footprint(&self, anchor: WorkbookCellKey) -> Option<&[WorkbookCellKey]> {
        self.blocked_anchors
            .get(&anchor)
            .filter(|b| !b.blocked_by_static)
            .map(|b| b.footprint.as_slice())
    }

    /// Returns the `(rows, cols)` dimensions of an active spill anchored at `anchor`.
    #[must_use]
    pub fn spill_shape(&self, anchor: WorkbookCellKey) -> Option<(u32, u32)> {
        self.anchors
            .get(&anchor)
            .map(|e| (e.array.rows, e.array.cols))
    }

    /// Looks up a spilled shadow scalar at `(sheet_id, row, col)` (returns `None`
    /// for spill anchor cells, whose values live directly on `Sheet`).
    #[must_use]
    pub fn shadow_value(&self, sheet_id: SheetId, row: RowId, col: ColId) -> Option<&EvalScalar> {
        if !self.has_spills_on_sheet(sheet_id) {
            return None;
        }
        self.sheet_col_shadows.get(&(sheet_id, col, row))
    }

    /// Returns the owning spill anchor when `key` holds a materialized shadow cell.
    #[must_use]
    pub fn anchor_of_cell(&self, key: WorkbookCellKey) -> Option<WorkbookCellKey> {
        self.cell_to_anchor.get(&key).copied()
    }

    /// Returns all blocked anchors currently watching `key`.
    pub fn watchers_of_cell(&self, key: WorkbookCellKey) -> Vec<WorkbookCellKey> {
        self.blocked_watch
            .get(&key)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Lists all active and blocked spill anchors residing on `sheet_id`.
    #[must_use]
    pub fn anchors_on_sheet(&self, sheet_id: SheetId) -> Vec<WorkbookCellKey> {
        let lo = (sheet_id, RowId(0), ColId(0));
        let hi = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
        let mut out: BTreeSet<WorkbookCellKey> = BTreeSet::new();
        for &k in self.anchors.range(lo..=hi).map(|(k, _)| k) {
            out.insert(k);
        }
        for &k in self.blocked_anchors.range(lo..=hi).map(|(k, _)| k) {
            out.insert(k);
        }
        out.into_iter().collect()
    }

    /// Visits every spilled shadow cell in column `col` of `sheet` whose live row
    /// position lies within `r1_pos..=r2_pos`. Fast no-op when `sheet` has no spills.
    pub fn for_each_shadow_in_col_span(
        &self,
        sheet: &Sheet,
        col: ColId,
        r1_pos: u32,
        r2_pos: u32,
        mut emit: impl FnMut(RowId, &EvalScalar),
    ) {
        let sheet_id = sheet.id();
        if !self.has_spills_on_sheet(sheet_id) {
            return;
        }
        let lo = (sheet_id, col, RowId(0));
        let hi = (sheet_id, col, RowId(u64::MAX));
        let row_axis = sheet.row_axis();
        for (&(_, _, row_id), scalar) in self.sheet_col_shadows.range(lo..=hi) {
            if let Some(pos) = row_axis.id_to_pos(row_id.0)
                && (r1_pos..=r2_pos).contains(&pos)
            {
                emit(row_id, scalar);
            }
        }
    }

    /// Removes any active or blocked spill owned by `anchor`, returning freed
    /// shadow cells and any blocked anchors watching them.
    pub fn clear_anchor(&mut self, anchor: WorkbookCellKey) -> ClearSpillOutcome {
        let mut outcome = ClearSpillOutcome::default();
        if let Some(old_blocked) = self.remove_blocked_only(anchor) {
            outcome.had_state = true;
            if !old_blocked.blocked_by_static {
                for cell in old_blocked.footprint {
                    self.collect_watchers_into(cell, &mut outcome.unblocked_watchers, Some(anchor));
                }
            }
        }
        if let Some(old_shadows) = self.remove_active_only(anchor) {
            outcome.had_state = true;
            for cell in old_shadows {
                self.collect_watchers_into(cell, &mut outcome.unblocked_watchers, Some(anchor));
                outcome.cleared_shadows.push(cell);
            }
        }
        outcome
    }

    /// Commits a dynamic-array formula result at `anchor`, applying symmetric
    /// E22 `#SPILL!` conflict detection.
    pub fn commit_array(
        &mut self,
        anchor: WorkbookCellKey,
        arr: Array2D,
        workbook: &Workbook<BoundFormula>,
    ) -> CommitSpillOutcome {
        self.commit_array_with_order(anchor, arr, workbook, |_, _, _, _| {
            SpillCollisionOrder::Symmetric
        })
    }

    /// Commits a dynamic-array formula result at `anchor`, classifying each
    /// colliding spill anchor via `classify(registry, anchor, footprint, other)`
    /// so strictly downstream spills yield cleanly to upstream spills while
    /// independent or mutually dependent spills collide symmetrically.
    pub fn commit_array_with_order<F>(
        &mut self,
        anchor: WorkbookCellKey,
        arr: Array2D,
        workbook: &Workbook<BoundFormula>,
        mut classify: F,
    ) -> CommitSpillOutcome
    where
        F: FnMut(
            &Self,
            WorkbookCellKey,
            &[WorkbookCellKey],
            WorkbookCellKey,
        ) -> SpillCollisionOrder,
    {
        let (sheet_id, anchor_row, anchor_col) = anchor;
        let old_blocked = self.remove_blocked_only(anchor);

        let anchor_scalar = arr
            .values
            .first()
            .cloned()
            .unwrap_or(EvalScalar::Error(CalcError::Calc));

        let Some((rows, cols)) = resolve_spill_axes(
            workbook, sheet_id, anchor_row, anchor_col, arr.rows, arr.cols,
        ) else {
            // Exceeded worksheet bounds -> static `#SPILL!`, vacate any prior active spill.
            let freed_shadows = self.remove_active_only(anchor).unwrap_or_default();
            let mut unblocked_watchers = Vec::new();
            for &cell in &freed_shadows {
                self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
            }
            if let Some(ref b) = old_blocked
                && !b.blocked_by_static
            {
                for &cell in &b.footprint {
                    self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
                }
            }
            self.record_blocked(anchor, Vec::new(), true);
            return CommitSpillOutcome::Blocked {
                revoked_anchors: Vec::new(),
                freed_shadows,
                unblocked_watchers,
            };
        };

        let mut footprint =
            Vec::with_capacity(rows.len().saturating_mul(cols.len()).saturating_sub(1));
        for &r in &rows {
            for &c in &cols {
                if (sheet_id, r, c) != anchor {
                    footprint.push((sheet_id, r, c));
                }
            }
        }

        // Check static obstacles first (non-empty Sheet value or formula cell).
        let Some(sheet) = workbook.sheet(sheet_id) else {
            let freed_shadows = self.remove_active_only(anchor).unwrap_or_default();
            let mut unblocked_watchers = Vec::new();
            if let Some(ref b) = old_blocked
                && !b.blocked_by_static
            {
                for &cell in &b.footprint {
                    self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
                }
            }
            self.record_blocked(anchor, Vec::new(), true);
            return CommitSpillOutcome::Blocked {
                revoked_anchors: Vec::new(),
                freed_shadows,
                unblocked_watchers,
            };
        };

        let mut has_static_blocker = false;
        let mut active_collisions = BTreeSet::new();
        let mut dynamic_blocked_collisions = BTreeSet::new();

        for &cell in &footprint {
            if sheet.value_by_id(cell.1, cell.2) != Value::Empty
                || workbook.formula(cell.0, cell.1, cell.2).is_some()
            {
                has_static_blocker = true;
                break;
            }
            if let Some(&owner) = self.cell_to_anchor.get(&cell)
                && owner != anchor
            {
                active_collisions.insert(owner);
            }
            if let Some(watchers) = self.blocked_watch.get(&cell) {
                for &watcher in watchers {
                    if watcher != anchor
                        && self
                            .blocked_anchors
                            .get(&watcher)
                            .is_some_and(|b| !b.blocked_by_static)
                    {
                        dynamic_blocked_collisions.insert(watcher);
                    }
                }
            }
        }

        let mut has_upstream_blocker = false;
        let mut symmetric_active = Vec::new();
        let mut downstream_active = Vec::new();
        let mut symmetric_blocked = Vec::new();
        let mut downstream_blocked = Vec::new();

        if !has_static_blocker
            && (!active_collisions.is_empty() || !dynamic_blocked_collisions.is_empty())
        {
            for &other in &active_collisions {
                match classify(self, anchor, &footprint, other) {
                    SpillCollisionOrder::OtherUpstream => {
                        has_upstream_blocker = true;
                        break;
                    }
                    SpillCollisionOrder::AnchorUpstream => {
                        downstream_active.push(other);
                    }
                    SpillCollisionOrder::Symmetric => {
                        symmetric_active.push(other);
                    }
                }
            }
            if !has_upstream_blocker {
                for &other in &dynamic_blocked_collisions {
                    match classify(self, anchor, &footprint, other) {
                        SpillCollisionOrder::OtherUpstream => {
                            has_upstream_blocker = true;
                            break;
                        }
                        SpillCollisionOrder::AnchorUpstream => {
                            downstream_blocked.push(other);
                        }
                        SpillCollisionOrder::Symmetric => {
                            symmetric_blocked.push(other);
                        }
                    }
                }
            }
        }

        if has_static_blocker || has_upstream_blocker {
            let freed_shadows = self.remove_active_only(anchor).unwrap_or_default();
            let mut unblocked_watchers = Vec::new();
            for &cell in &freed_shadows {
                self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
            }
            if let Some(ref b) = old_blocked
                && !b.blocked_by_static
            {
                for &cell in &b.footprint {
                    self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
                }
            }
            self.record_blocked(anchor, footprint, true);
            return CommitSpillOutcome::Blocked {
                revoked_anchors: Vec::new(),
                freed_shadows,
                unblocked_watchers,
            };
        }

        if !symmetric_active.is_empty() || !symmetric_blocked.is_empty() {
            let mut freed_shadows = self.remove_active_only(anchor).unwrap_or_default();
            let mut revoked_anchors = Vec::with_capacity(
                symmetric_active
                    .len()
                    .saturating_add(downstream_active.len()),
            );
            for &other in &symmetric_active {
                let other_footprint = self.remove_active_only(other).unwrap_or_default();
                freed_shadows.extend(other_footprint.iter().copied());
                self.record_blocked(other, other_footprint, false);
                revoked_anchors.push(other);
            }
            for &other in &downstream_active {
                let other_footprint = self.remove_active_only(other).unwrap_or_default();
                freed_shadows.extend(other_footprint.iter().copied());
                self.record_blocked(other, other_footprint, true);
                revoked_anchors.push(other);
            }
            let mut unblocked_watchers = Vec::new();
            for &cell in &freed_shadows {
                self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
            }
            if let Some(ref b) = old_blocked
                && !b.blocked_by_static
            {
                for &cell in &b.footprint {
                    if !footprint.contains(&cell) {
                        self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
                    }
                }
            }
            // Note: do not wake symmetric blockers we just revoked into blocked state,
            // while waking strictly downstream anchors so they re-evaluate cleanly.
            unblocked_watchers.retain(|w| !symmetric_active.contains(w));
            for &other in downstream_active.iter().chain(downstream_blocked.iter()) {
                if !unblocked_watchers.contains(&other) {
                    unblocked_watchers.push(other);
                }
            }
            self.record_blocked(anchor, footprint, false);
            return CommitSpillOutcome::Blocked {
                revoked_anchors,
                freed_shadows,
                unblocked_watchers,
            };
        }

        // Clean fit: first evict any strictly downstream active/blocked anchors cleanly.
        let mut displaced_shadows = Vec::new();
        let mut displaced_anchors = Vec::new();
        let mut unblocked_watchers = Vec::new();
        for &other in &downstream_active {
            let other_shadows = self.remove_active_only(other).unwrap_or_default();
            for &cell in &other_shadows {
                self.collect_watchers_into(cell, &mut unblocked_watchers, Some(anchor));
            }
            displaced_shadows.extend(other_shadows);
            displaced_anchors.push(other);
            if !unblocked_watchers.contains(&other) {
                unblocked_watchers.push(other);
            }
        }
        for &other in &downstream_blocked {
            if !unblocked_watchers.contains(&other) {
                unblocked_watchers.push(other);
            }
        }

        // Compare against `anchor`'s prior active spill (if any) to track exact diffs.
        let mut old_shadow_vals = BTreeMap::new();
        let old_shadows = self.remove_active_only_with_values(anchor, &mut old_shadow_vals);

        let mut new_shadows_map = BTreeMap::new();
        let mut changed_shadows = Vec::new();
        let col_count = cols.len();
        for (ri, &r) in rows.iter().enumerate() {
            for (ci, &c) in cols.iter().enumerate() {
                let cell = (sheet_id, r, c);
                if cell == anchor {
                    continue;
                }
                let val_idx = ri.saturating_mul(col_count).saturating_add(ci);
                let scalar = arr
                    .values
                    .get(val_idx)
                    .cloned()
                    .unwrap_or(EvalScalar::Error(CalcError::Calc));
                self.cell_to_anchor.insert(cell, anchor);
                self.sheet_col_shadows
                    .insert((sheet_id, c, r), scalar.clone());
                if old_shadow_vals.get(&cell) != Some(&scalar) {
                    changed_shadows.push(cell);
                }
                new_shadows_map.insert(cell, ());
            }
        }

        for old_cell in old_shadows {
            if !new_shadows_map.contains_key(&old_cell) {
                changed_shadows.push(old_cell);
                self.collect_watchers_into(old_cell, &mut unblocked_watchers, Some(anchor));
            }
        }
        if let Some(ref b) = old_blocked
            && !b.blocked_by_static
        {
            for &old_cell in &b.footprint {
                if !new_shadows_map.contains_key(&old_cell) {
                    self.collect_watchers_into(old_cell, &mut unblocked_watchers, Some(anchor));
                }
            }
        }
        for cell in displaced_shadows {
            if !new_shadows_map.contains_key(&cell) && !changed_shadows.contains(&cell) {
                changed_shadows.push(cell);
            }
        }

        if let Some(count) = self.sheet_spill_counts.get_mut(&sheet_id) {
            *count = count.saturating_add(1);
        } else {
            self.sheet_spill_counts.insert(sheet_id, 1);
        }
        self.anchors.insert(
            anchor,
            SpillEntry {
                rows,
                cols,
                array: arr,
            },
        );

        CommitSpillOutcome::Materialized {
            anchor_scalar,
            changed_shadows,
            unblocked_watchers,
            displaced_anchors,
        }
    }

    /// Builds a viewport overlay descriptor for `(sheet_id, row, col)` when
    /// it holds a spilled shadow value.
    #[must_use]
    pub fn viewport_overlay(
        &self,
        sheet_id: SheetId,
        row: RowId,
        col: ColId,
    ) -> Option<(OverlayValue<'_>, u16)> {
        let scalar = self.shadow_value(sheet_id, row, col)?;
        let val = match scalar {
            EvalScalar::Empty => OverlayValue::Empty,
            EvalScalar::Number(n) => OverlayValue::Number(*n),
            EvalScalar::Bool(b) => OverlayValue::Bool(*b),
            EvalScalar::Error(e) => OverlayValue::Error(*e),
            EvalScalar::Text(s) => OverlayValue::Text(s.as_str()),
        };
        Some((val, FLAG_SPILL))
    }

    fn record_blocked(
        &mut self,
        anchor: WorkbookCellKey,
        footprint: Vec<WorkbookCellKey>,
        blocked_by_static: bool,
    ) {
        let _ = self.remove_blocked_only(anchor);
        for &cell in &footprint {
            self.blocked_watch.entry(cell).or_default().insert(anchor);
        }
        self.blocked_anchors.insert(
            anchor,
            BlockedSpill {
                footprint,
                blocked_by_static,
            },
        );
    }

    fn remove_blocked_only(&mut self, anchor: WorkbookCellKey) -> Option<BlockedSpill> {
        let blocked = self.blocked_anchors.remove(&anchor)?;
        for &cell in &blocked.footprint {
            if let Some(set) = self.blocked_watch.get_mut(&cell) {
                set.remove(&anchor);
                if set.is_empty() {
                    self.blocked_watch.remove(&cell);
                }
            }
        }
        Some(blocked)
    }

    fn remove_active_only(&mut self, anchor: WorkbookCellKey) -> Option<Vec<WorkbookCellKey>> {
        let entry = self.anchors.remove(&anchor)?;
        let sheet_id = anchor.0;
        if let Some(count) = self.sheet_spill_counts.get_mut(&sheet_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.sheet_spill_counts.remove(&sheet_id);
            }
        }
        let mut shadows = Vec::with_capacity(
            entry
                .rows
                .len()
                .saturating_mul(entry.cols.len())
                .saturating_sub(1),
        );
        for &r in &entry.rows {
            for &c in &entry.cols {
                let cell = (sheet_id, r, c);
                if cell == anchor {
                    continue;
                }
                self.cell_to_anchor.remove(&cell);
                self.sheet_col_shadows.remove(&(sheet_id, c, r));
                shadows.push(cell);
            }
        }
        Some(shadows)
    }

    fn remove_active_only_with_values(
        &mut self,
        anchor: WorkbookCellKey,
        out_vals: &mut BTreeMap<WorkbookCellKey, EvalScalar>,
    ) -> Vec<WorkbookCellKey> {
        let Some(entry) = self.anchors.remove(&anchor) else {
            return Vec::new();
        };
        let sheet_id = anchor.0;
        if let Some(count) = self.sheet_spill_counts.get_mut(&sheet_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.sheet_spill_counts.remove(&sheet_id);
            }
        }
        let mut shadows = Vec::with_capacity(
            entry
                .rows
                .len()
                .saturating_mul(entry.cols.len())
                .saturating_sub(1),
        );
        for &r in &entry.rows {
            for &c in &entry.cols {
                let cell = (sheet_id, r, c);
                if cell == anchor {
                    continue;
                }
                self.cell_to_anchor.remove(&cell);
                if let Some(old_val) = self.sheet_col_shadows.remove(&(sheet_id, c, r)) {
                    out_vals.insert(cell, old_val);
                }
                shadows.push(cell);
            }
        }
        shadows
    }

    fn collect_watchers_into(
        &self,
        cell: WorkbookCellKey,
        out: &mut Vec<WorkbookCellKey>,
        exclude: Option<WorkbookCellKey>,
    ) {
        if let Some(set) = self.blocked_watch.get(&cell) {
            for &watcher in set {
                if Some(watcher) != exclude && !out.contains(&watcher) {
                    out.push(watcher);
                }
            }
        }
    }
}

fn resolve_spill_axes(
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    anchor_row: RowId,
    anchor_col: ColId,
    height: u32,
    width: u32,
) -> Option<(Vec<RowId>, Vec<ColId>)> {
    if height == 0 || width == 0 {
        return None;
    }
    let sheet = workbook.live_sheet(sheet_id)?;
    let r_pos = sheet.row_axis().id_to_pos(anchor_row.0)?;
    let c_pos = sheet.col_axis().id_to_pos(anchor_col.0)?;
    let r_end = r_pos.checked_add(height)?;
    let c_end = c_pos.checked_add(width)?;
    if r_end > sheet.row_axis().live_len() || c_end > sheet.col_axis().live_len() {
        return None;
    }

    let mut rows = Vec::with_capacity(height as usize);
    sheet
        .row_axis()
        .for_each_live_positions(r_pos, height, |run| {
            rows.extend(run.iter().map(RowId));
        });
    let mut cols = Vec::with_capacity(width as usize);
    sheet
        .col_axis()
        .for_each_live_positions(c_pos, width, |run| {
            cols.extend(run.iter().map(ColId));
        });
    if rows.len() == height as usize && cols.len() == width as usize {
        Some((rows, cols))
    } else {
        None
    }
}
