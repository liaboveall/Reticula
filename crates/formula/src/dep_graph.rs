//! Reverse dependency graph (`13-m1a.md` 5.6).
//!
//! Splits precedent tracking into three structures tailored to identity-based
//! spreadsheets:
//! 1. **Single-cell reverse map** (`(SheetId, RowId, ColId) -> Vec<u32>`): O(1)
//!    lookup when a cell is edited; zero updates when rows or columns are
//!    inserted or deleted.
//! 2. **Anchor-ordered range index**: single-column, multi-column, full-column
//!    and full-row range subscriptions stored by anchor identities so row/col
//!    insertions never rewrite the index (making B7a top-row insertion O(1)).
//! 3. **Structure-sensitive set** (`ROW`, `COLUMN` callers): deterministic
//!    `BTreeSet<u32>` per sheet marked dirty on structural mutations.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use reticula_core::axis::Axis;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{Workbook, WorkbookCellKey};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::binder::{BoundExpr, BoundFormula, BoundRef};

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// Single-column row-span subscription (`first_row..=last_row` on one column).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowSpanSub {
    /// Dependent formula slot ID.
    pub fid: u32,
    /// Anchor A on the row axis.
    pub first_row: RowId,
    /// Anchor B on the row axis.
    pub last_row: RowId,
}

/// Multi-column rectangular range subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AreaSub {
    /// Dependent formula slot ID.
    pub fid: u32,
    /// Row anchor A.
    pub first_row: RowId,
    /// Row anchor B.
    pub last_row: RowId,
    /// Column anchor A.
    pub first_col: ColId,
    /// Column anchor B.
    pub last_col: ColId,
}

/// Column-span subscription (`first_col..=last_col` across all rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColSpanSub {
    /// Dependent formula slot ID.
    pub fid: u32,
    /// Column anchor A.
    pub first_col: ColId,
    /// Column anchor B.
    pub last_col: ColId,
}

/// Reverse edges registered by one formula, kept so removal is fast without
/// scanning the entire graph.
#[derive(Clone, Debug, Default)]
pub struct FormulaRegistration {
    single_cells: SmallVec<[WorkbookCellKey; 4]>,
    col_spans: SmallVec<[(SheetId, ColId, RowId, RowId); 1]>,
    areas: SmallVec<[(SheetId, AreaSub); 1]>,
    full_cols: SmallVec<[(SheetId, ColSpanSub); 1]>,
    full_rows: SmallVec<[(SheetId, RowSpanSub); 1]>,
    structural_sheet: Option<SheetId>,
}

impl FormulaRegistration {
    /// Whether this formula statically subscribes to any cell or range on `target`.
    #[must_use]
    pub fn references_sheet(&self, target: SheetId) -> bool {
        self.single_cells.iter().any(|&(s, _, _)| s == target)
            || self.col_spans.iter().any(|&(s, _, _, _)| s == target)
            || self.areas.iter().any(|&(s, _)| s == target)
            || self.full_cols.iter().any(|&(s, _)| s == target)
            || self.full_rows.iter().any(|&(s, _)| s == target)
    }
}

/// Reverse dependency graph keyed by dense `FormulaId` (`u32`).
#[derive(Clone, Debug, Default)]
pub struct DepGraph {
    single_deps: FxMap<WorkbookCellKey, Vec<u32>>,
    col_ranges: FxMap<(SheetId, ColId), Vec<RowSpanSub>>,
    sheet_col_range_keys: BTreeMap<SheetId, BTreeSet<ColId>>,
    multi_col_ranges: BTreeMap<SheetId, Vec<AreaSub>>,
    full_col_deps: BTreeMap<SheetId, Vec<ColSpanSub>>,
    full_row_deps: BTreeMap<SheetId, Vec<RowSpanSub>>,
    structural_set: BTreeMap<SheetId, BTreeSet<u32>>,
}

impl DepGraph {
    /// Creates an empty dependency graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-reserves capacity in the single-cell reverse map for a known hot
    /// precedent cell (used by `open_sample` for `$V$1` with 100k dependents).
    pub fn reserve_single_dependents(&mut self, cell: WorkbookCellKey, capacity: usize) {
        self.single_deps.entry(cell).or_default().reserve(capacity);
    }

    /// Extracts and registers all static precedents of `formula` (located on
    /// `host_sheet`) under `fid`.
    pub fn register(
        &mut self,
        fid: u32,
        host_sheet: SheetId,
        formula: &BoundFormula,
    ) -> FormulaRegistration {
        let mut reg = FormulaRegistration::default();
        collect_refs(&formula.root, host_sheet, fid, &mut reg);

        for &cell_key in &reg.single_cells {
            self.single_deps.entry(cell_key).or_default().push(fid);
        }
        for &(sheet, col, r1, r2) in &reg.col_spans {
            self.col_ranges
                .entry((sheet, col))
                .or_default()
                .push(RowSpanSub {
                    fid,
                    first_row: r1,
                    last_row: r2,
                });
            self.sheet_col_range_keys
                .entry(sheet)
                .or_default()
                .insert(col);
        }
        for &(sheet, sub) in &reg.areas {
            self.multi_col_ranges.entry(sheet).or_default().push(sub);
        }
        for &(sheet, sub) in &reg.full_cols {
            self.full_col_deps.entry(sheet).or_default().push(sub);
        }
        for &(sheet, sub) in &reg.full_rows {
            self.full_row_deps.entry(sheet).or_default().push(sub);
        }
        if formula.is_structure_sensitive() {
            reg.structural_sheet = Some(host_sheet);
            self.structural_set
                .entry(host_sheet)
                .or_default()
                .insert(fid);
        }
        reg
    }

    /// Removes all reverse edges previously recorded in `reg` for `fid`.
    pub fn unregister(&mut self, fid: u32, reg: &FormulaRegistration) {
        for cell_key in &reg.single_cells {
            if let Some(list) = self.single_deps.get_mut(cell_key) {
                remove_first_u32(list, fid);
                if list.is_empty() {
                    self.single_deps.remove(cell_key);
                }
            }
        }
        for &(sheet, col, r1, r2) in &reg.col_spans {
            if let Some(list) = self.col_ranges.get_mut(&(sheet, col)) {
                let target = RowSpanSub {
                    fid,
                    first_row: r1,
                    last_row: r2,
                };
                if let Some(idx) = list.iter().position(|x| *x == target) {
                    list.swap_remove(idx);
                }
                if list.is_empty() {
                    self.col_ranges.remove(&(sheet, col));
                    if let Some(cols) = self.sheet_col_range_keys.get_mut(&sheet) {
                        cols.remove(&col);
                    }
                }
            }
        }
        for &(sheet, sub) in &reg.areas {
            if let Some(list) = self.multi_col_ranges.get_mut(&sheet)
                && let Some(idx) = list.iter().position(|x| *x == sub)
            {
                list.swap_remove(idx);
            }
        }
        for &(sheet, sub) in &reg.full_cols {
            if let Some(list) = self.full_col_deps.get_mut(&sheet)
                && let Some(idx) = list.iter().position(|x| *x == sub)
            {
                list.swap_remove(idx);
            }
        }
        for &(sheet, sub) in &reg.full_rows {
            if let Some(list) = self.full_row_deps.get_mut(&sheet)
                && let Some(idx) = list.iter().position(|x| *x == sub)
            {
                list.swap_remove(idx);
            }
        }
        if let Some(sheet) = reg.structural_sheet
            && let Some(set) = self.structural_set.get_mut(&sheet)
        {
            set.remove(&fid);
        }
    }

    /// Calls `emit(fid)` for every direct formula dependent of cell `key`.
    pub fn for_each_cell_dependent(
        &self,
        key: WorkbookCellKey,
        workbook: &Workbook<BoundFormula>,
        mut emit: impl FnMut(u32),
    ) {
        let (sheet_id, row, col) = key;
        if let Some(list) = self.single_deps.get(&key) {
            for &fid in list {
                emit(fid);
            }
        }

        let Some(sheet) = workbook.sheet(sheet_id) else {
            return;
        };
        let row_axis = sheet.row_axis();
        let col_axis = sheet.col_axis();
        let (Some(r_ord), Some(c_ord)) =
            (row_axis.axis_order_of(row.0), col_axis.axis_order_of(col.0))
        else {
            return;
        };

        if let Some(list) = self.col_ranges.get(&(sheet_id, col)) {
            for sub in list {
                if span_contains_order(row_axis, sub.first_row.0, sub.last_row.0, r_ord) {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
            for sub in list {
                if span_contains_order(col_axis, sub.first_col.0, sub.last_col.0, c_ord)
                    && span_contains_order(row_axis, sub.first_row.0, sub.last_row.0, r_ord)
                {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.full_col_deps.get(&sheet_id) {
            for sub in list {
                if span_contains_order(col_axis, sub.first_col.0, sub.last_col.0, c_ord) {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.full_row_deps.get(&sheet_id) {
            for sub in list {
                if span_contains_order(row_axis, sub.first_row.0, sub.last_row.0, r_ord) {
                    emit(sub.fid);
                }
            }
        }
    }

    /// Marks formulas affected by inserting `inserted` rows on `sheet_id`
    /// (after the axis has already been updated): structure-sensitive formulas
    /// (`ROW`/`COLUMN`), full-column ranges, and ranges whose two anchors sit
    /// strictly on opposite sides of the inserted run.
    pub fn on_rows_inserted(
        &self,
        sheet_id: SheetId,
        inserted: IdRun,
        workbook: &Workbook<BoundFormula>,
        mut emit: impl FnMut(u32),
    ) {
        if let Some(set) = self.structural_set.get(&sheet_id) {
            for &fid in set {
                emit(fid);
            }
        }
        if let Some(list) = self.full_col_deps.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
        let Some(sheet) = workbook.sheet(sheet_id) else {
            return;
        };
        let row_axis = sheet.row_axis();
        let Some(ins_order) = row_axis.axis_order_of(inserted.first_id) else {
            return;
        };

        if let Some(cols) = self.sheet_col_range_keys.get(&sheet_id) {
            for &col in cols {
                if let Some(list) = self.col_ranges.get(&(sheet_id, col)) {
                    for sub in list {
                        if span_strictly_straddles(
                            row_axis,
                            sub.first_row.0,
                            sub.last_row.0,
                            ins_order,
                        ) {
                            emit(sub.fid);
                        }
                    }
                }
            }
        }
        if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
            for sub in list {
                if span_strictly_straddles(row_axis, sub.first_row.0, sub.last_row.0, ins_order) {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.full_row_deps.get(&sheet_id) {
            for sub in list {
                if span_strictly_straddles(row_axis, sub.first_row.0, sub.last_row.0, ins_order) {
                    emit(sub.fid);
                }
            }
        }
    }

    /// Marks formulas affected by inserting `inserted` columns on `sheet_id`.
    pub fn on_cols_inserted(
        &self,
        sheet_id: SheetId,
        inserted: IdRun,
        workbook: &Workbook<BoundFormula>,
        mut emit: impl FnMut(u32),
    ) {
        if let Some(set) = self.structural_set.get(&sheet_id) {
            for &fid in set {
                emit(fid);
            }
        }
        if let Some(list) = self.full_row_deps.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
        let Some(sheet) = workbook.sheet(sheet_id) else {
            return;
        };
        let col_axis = sheet.col_axis();
        let Some(ins_order) = col_axis.axis_order_of(inserted.first_id) else {
            return;
        };
        if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
            for sub in list {
                if span_strictly_straddles(col_axis, sub.first_col.0, sub.last_col.0, ins_order) {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.full_col_deps.get(&sheet_id) {
            for sub in list {
                if span_strictly_straddles(col_axis, sub.first_col.0, sub.last_col.0, ins_order) {
                    emit(sub.fid);
                }
            }
        }
    }

    /// Marks formulas affected by deleting, undeleting, or removing `runs` of
    /// rows on `sheet_id`.
    pub fn on_rows_toggled(
        &self,
        sheet_id: SheetId,
        runs: &[IdRun],
        workbook: &Workbook<BoundFormula>,
        mut emit: impl FnMut(u32),
    ) {
        if let Some(set) = self.structural_set.get(&sheet_id) {
            for &fid in set {
                emit(fid);
            }
        }
        if let Some(list) = self.full_col_deps.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
        let Some(sheet) = workbook.sheet(sheet_id) else {
            return;
        };
        let row_axis = sheet.row_axis();
        let col_axis = sheet.col_axis();

        // Any single-cell reference to `(sheet_id, deleted_row, any_col)` or
        // range overlapping `deleted_row` must recalculate (`#REF!` or shrunk sum).
        for run in runs {
            for raw_row in run.iter() {
                let Some(r_ord) = row_axis.axis_order_of(raw_row) else {
                    continue;
                };
                // Check single-cell refs across columns that have been allocated or
                // base columns up to live_len + tombstones (bounded by `col_axis.runs()`).
                for col_run in col_axis.runs() {
                    for raw_col in col_run.ids.iter() {
                        if let Some(list) =
                            self.single_deps
                                .get(&(sheet_id, RowId(raw_row), ColId(raw_col)))
                        {
                            for &fid in list {
                                emit(fid);
                            }
                        }
                    }
                }
                if let Some(cols) = self.sheet_col_range_keys.get(&sheet_id) {
                    for &col in cols {
                        if let Some(list) = self.col_ranges.get(&(sheet_id, col)) {
                            for sub in list {
                                if span_contains_order(
                                    row_axis,
                                    sub.first_row.0,
                                    sub.last_row.0,
                                    r_ord,
                                ) {
                                    emit(sub.fid);
                                }
                            }
                        }
                    }
                }
                if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
                    for sub in list {
                        if span_contains_order(row_axis, sub.first_row.0, sub.last_row.0, r_ord) {
                            emit(sub.fid);
                        }
                    }
                }
                if let Some(list) = self.full_row_deps.get(&sheet_id) {
                    for sub in list {
                        if span_contains_order(row_axis, sub.first_row.0, sub.last_row.0, r_ord) {
                            emit(sub.fid);
                        }
                    }
                }
            }
        }
    }

    /// Marks formulas affected by deleting, undeleting, or removing `runs` of
    /// columns on `sheet_id`.
    pub fn on_cols_toggled(
        &self,
        sheet_id: SheetId,
        runs: &[IdRun],
        workbook: &Workbook<BoundFormula>,
        mut emit: impl FnMut(u32),
    ) {
        if let Some(set) = self.structural_set.get(&sheet_id) {
            for &fid in set {
                emit(fid);
            }
        }
        if let Some(list) = self.full_row_deps.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
        let Some(sheet) = workbook.sheet(sheet_id) else {
            return;
        };
        let row_axis = sheet.row_axis();
        let col_axis = sheet.col_axis();

        for run in runs {
            for raw_col in run.iter() {
                let col = ColId(raw_col);
                if let Some(list) = self.col_ranges.get(&(sheet_id, col)) {
                    for sub in list {
                        emit(sub.fid);
                    }
                }
                for row_run in row_axis.runs() {
                    for raw_row in row_run.ids.iter() {
                        if let Some(list) = self.single_deps.get(&(sheet_id, RowId(raw_row), col)) {
                            for &fid in list {
                                emit(fid);
                            }
                        }
                    }
                }
                if let Some(c_ord) = col_axis.axis_order_of(raw_col) {
                    if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
                        for sub in list {
                            if span_contains_order(col_axis, sub.first_col.0, sub.last_col.0, c_ord)
                            {
                                emit(sub.fid);
                            }
                        }
                    }
                    if let Some(list) = self.full_col_deps.get(&sheet_id) {
                        for sub in list {
                            if span_contains_order(col_axis, sub.first_col.0, sub.last_col.0, c_ord)
                            {
                                emit(sub.fid);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Marks structure-sensitive formulas and range subscriptions on `sheet_id`
    /// dirty after rows or columns are reordered (`MoveRows` / `MoveCols`).
    pub fn on_sheet_reordered(&self, sheet_id: SheetId, mut emit: impl FnMut(u32)) {
        if let Some(set) = self.structural_set.get(&sheet_id) {
            for &fid in set {
                emit(fid);
            }
        }
        if let Some(cols) = self.sheet_col_range_keys.get(&sheet_id) {
            for &col in cols {
                if let Some(list) = self.col_ranges.get(&(sheet_id, col)) {
                    for sub in list {
                        emit(sub.fid);
                    }
                }
            }
        }
        if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
        if let Some(list) = self.full_col_deps.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
        if let Some(list) = self.full_row_deps.get(&sheet_id) {
            for sub in list {
                emit(sub.fid);
            }
        }
    }

    /// Emits all formula dependents overlapping the rectangular/run-product
    /// region `(sheet_id, rows × cols)` (used by [`reticula_core::ops::Op::FillFormula`]
    /// so range subscriptions are tested once per run rather than once per cell).
    pub fn on_rect_written(
        &self,
        sheet_id: SheetId,
        rows: &[IdRun],
        cols: &[IdRun],
        workbook: &Workbook<BoundFormula>,
        mut emit: impl FnMut(u32),
    ) {
        if !self.single_deps.is_empty() {
            for c_run in cols {
                for raw_c in c_run.iter() {
                    let col = ColId(raw_c);
                    for r_run in rows {
                        for raw_r in r_run.iter() {
                            if let Some(list) = self.single_deps.get(&(sheet_id, RowId(raw_r), col))
                            {
                                for &fid in list {
                                    emit(fid);
                                }
                            }
                        }
                    }
                }
            }
        }

        let Some(sheet) = workbook.sheet(sheet_id) else {
            return;
        };
        let row_axis = sheet.row_axis();
        let col_axis = sheet.col_axis();

        for c_run in cols {
            for raw_c in c_run.iter() {
                if let Some(list) = self.col_ranges.get(&(sheet_id, ColId(raw_c))) {
                    for sub in list {
                        if span_overlaps_runs(row_axis, sub.first_row.0, sub.last_row.0, rows) {
                            emit(sub.fid);
                        }
                    }
                }
            }
        }
        if let Some(list) = self.multi_col_ranges.get(&sheet_id) {
            for sub in list {
                if span_overlaps_runs(col_axis, sub.first_col.0, sub.last_col.0, cols)
                    && span_overlaps_runs(row_axis, sub.first_row.0, sub.last_row.0, rows)
                {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.full_col_deps.get(&sheet_id) {
            for sub in list {
                if span_overlaps_runs(col_axis, sub.first_col.0, sub.last_col.0, cols) {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = self.full_row_deps.get(&sheet_id) {
            for sub in list {
                if span_overlaps_runs(row_axis, sub.first_row.0, sub.last_row.0, rows) {
                    emit(sub.fid);
                }
            }
        }
    }
}

fn collect_refs(expr: &BoundExpr, host_sheet: SheetId, fid: u32, reg: &mut FormulaRegistration) {
    match expr {
        BoundExpr::Literal(_) | BoundExpr::ArrayLiteral { .. } | BoundExpr::Name(_) => {}
        BoundExpr::Reference(r) => match r {
            BoundRef::Invalid { .. } => {}
            BoundRef::Cell {
                sheet, row, col, ..
            } => {
                let s = sheet.unwrap_or(host_sheet);
                let key = (s, *row, *col);
                if !reg.single_cells.contains(&key) {
                    reg.single_cells.push(key);
                }
            }
            BoundRef::Area {
                sheet,
                first_row,
                first_col,
                last_row,
                last_col,
                ..
            } => {
                let s = sheet.unwrap_or(host_sheet);
                if first_col == last_col {
                    let entry = (s, *first_col, *first_row, *last_row);
                    if !reg.col_spans.contains(&entry) {
                        reg.col_spans.push(entry);
                    }
                } else {
                    let sub = AreaSub {
                        fid,
                        first_row: *first_row,
                        last_row: *last_row,
                        first_col: *first_col,
                        last_col: *last_col,
                    };
                    if !reg.areas.contains(&(s, sub)) {
                        reg.areas.push((s, sub));
                    }
                }
            }
            BoundRef::ColRange {
                sheet,
                first_col,
                last_col,
                ..
            } => {
                let s = sheet.unwrap_or(host_sheet);
                let sub = ColSpanSub {
                    fid,
                    first_col: *first_col,
                    last_col: *last_col,
                };
                if !reg.full_cols.contains(&(s, sub)) {
                    reg.full_cols.push((s, sub));
                }
            }
            BoundRef::RowRange {
                sheet,
                first_row,
                last_row,
                ..
            } => {
                let s = sheet.unwrap_or(host_sheet);
                let sub = RowSpanSub {
                    fid,
                    first_row: *first_row,
                    last_row: *last_row,
                };
                if !reg.full_rows.contains(&(s, sub)) {
                    reg.full_rows.push((s, sub));
                }
            }
        },
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => {
            collect_refs(inner, host_sheet, fid, reg);
        }
        BoundExpr::Binary { left, right, .. } => {
            collect_refs(left, host_sheet, fid, reg);
            collect_refs(right, host_sheet, fid, reg);
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                collect_refs(arg, host_sheet, fid, reg);
            }
        }
    }
}

fn span_contains_order(axis: &Axis, a: u64, b: u64, target_order: u32) -> bool {
    let (Some(oa), Some(ob)) = (axis.axis_order_of(a), axis.axis_order_of(b)) else {
        return false;
    };
    let (lo, hi) = if oa <= ob { (oa, ob) } else { (ob, oa) };
    (lo..=hi).contains(&target_order)
}

fn span_strictly_straddles(axis: &Axis, a: u64, b: u64, ins_order: u32) -> bool {
    let (Some(oa), Some(ob)) = (axis.axis_order_of(a), axis.axis_order_of(b)) else {
        return false;
    };
    let (lo, hi) = if oa <= ob { (oa, ob) } else { (ob, oa) };
    lo < ins_order && ins_order < hi
}

fn span_overlaps_runs(axis: &Axis, a: u64, b: u64, runs: &[IdRun]) -> bool {
    let (Some(oa), Some(ob)) = (axis.axis_order_of(a), axis.axis_order_of(b)) else {
        return false;
    };
    let (lo, hi) = if oa <= ob { (oa, ob) } else { (ob, oa) };
    let span_len = hi.saturating_sub(lo).saturating_add(1);
    for run in runs {
        if let (Some(o_first), Some(last_id)) = (
            axis.axis_order_of(run.first_id),
            run.id_at(run.len.saturating_sub(1)),
        ) && let Some(o_last) = axis.axis_order_of(last_id)
            && o_first <= o_last
            && o_last.saturating_sub(o_first).saturating_add(1) == run.len
        {
            if lo <= o_last && o_first <= hi {
                return true;
            }
            continue;
        }
        if span_len < run.len {
            for ord in lo..=hi {
                if let Some((id, _)) = axis.id_at_axis_order(ord)
                    && run.contains(id)
                {
                    return true;
                }
            }
        } else {
            for id in run.iter() {
                if let Some(ord) = axis.axis_order_of(id)
                    && (lo..=hi).contains(&ord)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn remove_first_u32(list: &mut Vec<u32>, target: u32) {
    if let Some(idx) = list.iter().position(|&x| x == target) {
        list.swap_remove(idx);
    }
}
