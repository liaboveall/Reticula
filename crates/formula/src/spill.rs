//! Dynamic array spill materialization, conflict detection, and viewport
//! overlay tracking (E22).
//!
//! The registry records which cells each spill owns and which anchors show
//! `#SPILL!`; it does not decide between spills that want the same cells.
//! That decision depends on every spill involved, so the engine makes it for
//! all of them at once (`engine/settle.rs`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{Workbook, WorkbookCellKey};
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
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

impl SpillEntry {
    /// The value at `(row, col)` of the array, provided it lands on `cell`.
    fn value_at(&self, row: usize, col: usize, cell: WorkbookCellKey) -> Option<&EvalScalar> {
        if self.rows.get(row) != Some(&cell.1) || self.cols.get(col) != Some(&cell.2) {
            return None;
        }
        array_value(&self.array, row, col)
    }
}

/// Outcome of releasing an anchor's active or blocked spill state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleasedSpill {
    /// Shadow cells (`!= anchor`) previously owned by the anchor.
    pub cleared_shadows: Vec<WorkbookCellKey>,
    /// Whether the anchor previously held an active spill or blocked registration.
    pub had_state: bool,
}

/// Where an array of a given shape would land on the sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpillFit {
    /// Every covered cell besides the anchor is free of static content.
    Free {
        /// Ordered live row IDs covered by the array.
        rows: Vec<RowId>,
        /// Ordered live column IDs covered by the array.
        cols: Vec<ColId>,
    },
    /// A value, a formula or the edge of the sheet is in the way.
    Obstructed {
        /// Cells whose clearing may free the spill (empty when the array does
        /// not fit on the sheet).
        footprint: Vec<WorkbookCellKey>,
    },
}

impl SpillFit {
    /// The covered cells without the anchor, or those that may free the spill.
    #[must_use]
    pub fn footprint(&self, anchor: WorkbookCellKey) -> Vec<WorkbookCellKey> {
        match self {
            Self::Free { rows, cols } => footprint_cells(anchor, rows, cols),
            Self::Obstructed { footprint } => footprint.clone(),
        }
    }
}

/// Registry tracking active dynamic-array spills, blocked anchors, and
/// per-column shadow values for fast viewport and formula lookups.
#[derive(Clone, Debug, Default)]
pub struct SpillRegistry {
    anchors: BTreeMap<WorkbookCellKey, SpillEntry>,
    cell_to_anchor: BTreeMap<WorkbookCellKey, WorkbookCellKey>,
    sheet_col_shadows: BTreeMap<(SheetId, ColId, RowId), EvalScalar>,
    blocked_anchors: BTreeMap<WorkbookCellKey, Vec<WorkbookCellKey>>,
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
    #[must_use]
    pub fn watchers_of_cell(&self, key: WorkbookCellKey) -> Vec<WorkbookCellKey> {
        self.blocked_watch
            .get(&key)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Lists every active and blocked anchor in key order.
    #[must_use]
    pub fn all_anchors(&self) -> BTreeSet<WorkbookCellKey> {
        self.anchors
            .keys()
            .chain(self.blocked_anchors.keys())
            .copied()
            .collect()
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

    /// Whether any active or blocked anchor sits in `col` between `first` and
    /// `last` (inclusive, in row ID order).
    #[must_use]
    pub fn has_anchor_in_col_run(
        &self,
        sheet_id: SheetId,
        col: ColId,
        first: RowId,
        last: RowId,
    ) -> bool {
        let lo = (sheet_id, first, ColId(0));
        let hi = (sheet_id, last, ColId(u64::MAX));
        lo <= hi
            && (self.anchors.range(lo..=hi).any(|(k, _)| k.2 == col)
                || self.blocked_anchors.range(lo..=hi).any(|(k, _)| k.2 == col))
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

    /// Resolves where a `height × width` array anchored at `anchor` would land and
    /// whether static content (a value, a formula, the sheet edge) is in the way.
    #[must_use]
    pub fn fit(
        workbook: &Workbook<BoundFormula>,
        anchor: WorkbookCellKey,
        height: u32,
        width: u32,
    ) -> SpillFit {
        let (sheet_id, anchor_row, anchor_col) = anchor;
        let Some((rows, cols)) =
            resolve_spill_axes(workbook, sheet_id, anchor_row, anchor_col, height, width)
        else {
            return SpillFit::Obstructed {
                footprint: Vec::new(),
            };
        };
        let Some(sheet) = workbook.sheet(sheet_id) else {
            return SpillFit::Obstructed {
                footprint: Vec::new(),
            };
        };
        let obstructed = rows.iter().any(|&row| {
            cols.iter().any(|&col| {
                (sheet_id, row, col) != anchor
                    && (sheet.value_by_id(row, col) != Value::Empty
                        || workbook.has_formula(sheet_id, row, col))
            })
        });
        if obstructed {
            SpillFit::Obstructed {
                footprint: footprint_cells(anchor, &rows, &cols),
            }
        } else {
            SpillFit::Free { rows, cols }
        }
    }

    /// Whether another spill owns any of the cells `anchor` would cover.
    #[must_use]
    pub fn is_contested(&self, anchor: WorkbookCellKey, rows: &[RowId], cols: &[ColId]) -> bool {
        if self.cell_to_anchor.is_empty() {
            return false;
        }
        rows.iter().any(|&row| {
            cols.iter().any(|&col| {
                self.cell_to_anchor
                    .get(&(anchor.0, row, col))
                    .is_some_and(|&owner| owner != anchor)
            })
        })
    }

    /// Removes any active or blocked spill state owned by `anchor`.
    pub fn release(&mut self, anchor: WorkbookCellKey) -> ReleasedSpill {
        let was_blocked = self.remove_blocked(anchor);
        let active = self.remove_active(anchor);
        ReleasedSpill {
            had_state: was_blocked || active.is_some(),
            cleared_shadows: active.unwrap_or_default(),
        }
    }

    /// Records that `anchor` shows `#SPILL!` and watches `footprint`, returning
    /// the shadow cells it gave up.
    pub fn block(
        &mut self,
        anchor: WorkbookCellKey,
        footprint: Vec<WorkbookCellKey>,
    ) -> Vec<WorkbookCellKey> {
        let released = self.release(anchor);
        for &cell in &footprint {
            self.blocked_watch.entry(cell).or_default().insert(anchor);
        }
        self.blocked_anchors.insert(anchor, footprint);
        released.cleared_shadows
    }

    /// Materializes `arr` over `rows × cols`, which no other spill may own, and
    /// returns the shadow cells whose value or owner changed.
    pub fn materialize(
        &mut self,
        anchor: WorkbookCellKey,
        rows: Vec<RowId>,
        cols: Vec<ColId>,
        arr: Array2D,
    ) -> Vec<WorkbookCellKey> {
        let sheet_id = anchor.0;
        let _ = self.remove_blocked(anchor);
        let old = self.anchors.remove(&anchor);
        let mut changed = Vec::new();
        let mut values = arr.values.iter();
        for (ri, &row) in rows.iter().enumerate() {
            for (ci, &col) in cols.iter().enumerate() {
                let value = values.next();
                let cell = (sheet_id, row, col);
                if cell == anchor {
                    continue;
                }
                let before = old.as_ref().and_then(|entry| entry.value_at(ri, ci, cell));
                if before.is_some() && before == value {
                    continue;
                }
                changed.push(cell);
                self.cell_to_anchor.insert(cell, anchor);
                self.sheet_col_shadows.insert(
                    (sheet_id, col, row),
                    value.cloned().unwrap_or(EvalScalar::Empty),
                );
            }
        }
        let entry = SpillEntry {
            rows,
            cols,
            array: arr,
        };
        match old {
            Some(old) => {
                for (ri, &row) in old.rows.iter().enumerate() {
                    for (ci, &col) in old.cols.iter().enumerate() {
                        let cell = (sheet_id, row, col);
                        if cell != anchor && entry.value_at(ri, ci, cell).is_none() {
                            self.cell_to_anchor.remove(&cell);
                            self.sheet_col_shadows.remove(&(sheet_id, col, row));
                            changed.push(cell);
                        }
                    }
                }
            }
            None => {
                let count = self.sheet_spill_counts.entry(sheet_id).or_insert(0);
                *count = count.saturating_add(1);
            }
        }
        self.anchors.insert(anchor, entry);
        changed
    }

    /// Replaces the values of an active spill without moving its footprint: cells
    /// outside `arr` (all of them for `None`) become empty. Returns the shadow
    /// cells whose value changed.
    pub fn refill(
        &mut self,
        anchor: WorkbookCellKey,
        arr: Option<&Array2D>,
    ) -> Vec<WorkbookCellKey> {
        let sheet_id = anchor.0;
        let Some(entry) = self.anchors.get_mut(&anchor) else {
            return Vec::new();
        };
        let mut changed = Vec::new();
        let mut slots = entry.array.values.iter_mut();
        for (ri, &row) in entry.rows.iter().enumerate() {
            for (ci, &col) in entry.cols.iter().enumerate() {
                let Some(slot) = slots.next() else {
                    return changed;
                };
                let value = arr
                    .and_then(|a| array_value(a, ri, ci))
                    .cloned()
                    .unwrap_or(EvalScalar::Empty);
                if *slot == value {
                    continue;
                }
                *slot = value.clone();
                let cell = (sheet_id, row, col);
                if cell != anchor {
                    self.sheet_col_shadows.insert((sheet_id, col, row), value);
                    changed.push(cell);
                }
            }
        }
        changed
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

    fn remove_blocked(&mut self, anchor: WorkbookCellKey) -> bool {
        let Some(footprint) = self.blocked_anchors.remove(&anchor) else {
            return false;
        };
        for cell in &footprint {
            if let Some(set) = self.blocked_watch.get_mut(cell) {
                set.remove(&anchor);
                if set.is_empty() {
                    self.blocked_watch.remove(cell);
                }
            }
        }
        true
    }

    fn remove_active(&mut self, anchor: WorkbookCellKey) -> Option<Vec<WorkbookCellKey>> {
        let entry = self.anchors.remove(&anchor)?;
        if let Some(count) = self.sheet_spill_counts.get_mut(&anchor.0) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.sheet_spill_counts.remove(&anchor.0);
            }
        }
        let shadows = footprint_cells(anchor, &entry.rows, &entry.cols);
        for cell in &shadows {
            self.cell_to_anchor.remove(cell);
            self.sheet_col_shadows.remove(&(cell.0, cell.2, cell.1));
        }
        Some(shadows)
    }
}

/// Lists the cells `rows × cols` on the anchor's sheet, without the anchor.
#[must_use]
pub fn footprint_cells(
    anchor: WorkbookCellKey,
    rows: &[RowId],
    cols: &[ColId],
) -> Vec<WorkbookCellKey> {
    let mut cells = Vec::with_capacity(rows.len().saturating_mul(cols.len()).saturating_sub(1));
    for &row in rows {
        for &col in cols {
            let cell = (anchor.0, row, col);
            if cell != anchor {
                cells.push(cell);
            }
        }
    }
    cells
}

fn array_value(arr: &Array2D, row: usize, col: usize) -> Option<&EvalScalar> {
    let width = arr.cols as usize;
    if row >= arr.rows as usize || col >= width {
        return None;
    }
    arr.values
        .get(row.saturating_mul(width).saturating_add(col))
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
