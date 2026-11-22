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

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use reticula_core::axis::Axis;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{Workbook, WorkbookCellKey};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::binder::{BoundExpr, BoundFormula, BoundRef};
use crate::functions::catalog::is_volatile_function;

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// Inline single-dependent storage avoiding a separate heap `Vec` for cells
/// referenced by only one formula (the common case in 1M-row sheets).
#[derive(Clone, Debug)]
enum DepList {
    One(u32),
    Many(Vec<u32>),
}

impl DepList {
    #[inline]
    fn push(&mut self, fid: u32) {
        match self {
            Self::One(first) => {
                let prev = *first;
                *self = Self::Many(vec![prev, fid]);
            }
            Self::Many(vec) => vec.push(fid),
        }
    }

    #[inline]
    fn remove_first(&mut self, target: u32) -> bool {
        match self {
            Self::One(first) => *first == target,
            Self::Many(vec) => {
                if let Some(idx) = vec.iter().position(|&x| x == target) {
                    vec.swap_remove(idx);
                }
                vec.is_empty()
            }
        }
    }

    #[inline]
    fn for_each(&self, mut emit: impl FnMut(u32)) {
        match self {
            Self::One(fid) => emit(*fid),
            Self::Many(vec) => {
                for &fid in vec {
                    emit(fid);
                }
            }
        }
    }
}

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

#[derive(Clone, Debug, Default)]
struct RangeRegistrations {
    col_spans: Vec<(SheetId, ColId, RowId, RowId)>,
    areas: Vec<(SheetId, AreaSub)>,
    full_cols: Vec<(SheetId, ColSpanSub)>,
    full_rows: Vec<(SheetId, RowSpanSub)>,
}

/// Reverse edges registered by one formula, kept so removal is fast without
/// scanning the entire graph.
#[derive(Clone, Debug, Default)]
struct ExtraDeps {
    names: Vec<String>,
    spill_anchors: Vec<WorkbookCellKey>,
    scoped_sheets: Vec<SheetId>,
}

/// Compact per-formula reverse-dependency handles retained so unregistering
/// or replacing a formula touches only the edges it actually created without
/// scanning the entire graph.
#[derive(Clone, Debug, Default)]
pub struct FormulaRegistration {
    single_cells: SmallVec<[WorkbookCellKey; 3]>,
    ranges: Option<Box<RangeRegistrations>>,
    structural_sheet: Option<SheetId>,
    extra: Option<Box<ExtraDeps>>,
}

impl FormulaRegistration {
    fn ranges_mut(&mut self) -> &mut RangeRegistrations {
        self.ranges
            .get_or_insert_with(|| Box::new(RangeRegistrations::default()))
    }

    fn extra_mut(&mut self) -> &mut ExtraDeps {
        self.extra
            .get_or_insert_with(|| Box::new(ExtraDeps::default()))
    }

    /// Whether this formula statically subscribes to any cell or range on `target`.
    #[must_use]
    pub fn references_sheet(&self, target: SheetId) -> bool {
        if self.single_cells.iter().any(|&(s, _, _)| s == target) {
            return true;
        }
        if let Some(extra) = self.extra.as_deref()
            && (extra.scoped_sheets.contains(&target)
                || extra.spill_anchors.iter().any(|&(s, _, _)| s == target))
        {
            return true;
        }
        let Some(ranges) = self.ranges.as_deref() else {
            return false;
        };
        ranges.col_spans.iter().any(|&(s, _, _, _)| s == target)
            || ranges.areas.iter().any(|&(s, _)| s == target)
            || ranges.full_cols.iter().any(|&(s, _)| s == target)
            || ranges.full_rows.iter().any(|&(s, _)| s == target)
    }
}

/// Reverse dependency graph keyed by dense `FormulaId` (`u32`).
#[derive(Clone, Debug, Default)]
pub struct DepGraph {
    single_deps: FxMap<WorkbookCellKey, DepList>,
    col_ranges: FxMap<(SheetId, ColId), Vec<RowSpanSub>>,
    sheet_col_range_keys: BTreeMap<SheetId, BTreeSet<ColId>>,
    multi_col_ranges: BTreeMap<SheetId, Vec<AreaSub>>,
    full_col_deps: BTreeMap<SheetId, Vec<ColSpanSub>>,
    full_row_deps: BTreeMap<SheetId, Vec<RowSpanSub>>,
    structural_set: BTreeMap<SheetId, BTreeSet<u32>>,
    name_deps: BTreeMap<String, Vec<u32>>,
    spill_deps: BTreeMap<WorkbookCellKey, Vec<u32>>,
}

impl DepGraph {
    /// Creates an empty dependency graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty dependency graph with `single_deps` pre-sized for
    /// roughly `formula_count` formulas (avoids repeated hash-table rehashing
    /// when decoding large snapshots).
    #[must_use]
    pub fn with_formula_capacity(formula_count: usize) -> Self {
        let single_cap = formula_count.saturating_mul(2);
        Self {
            single_deps: FxMap::with_capacity_and_hasher(single_cap, BuildHasherDefault::default()),
            ..Self::default()
        }
    }

    /// Pre-reserves space in `single_deps` for `additional_formulas` newly added
    /// formulas (used by `Op::FillFormula` to avoid repeated rehashing on 1M-row fills).
    pub fn reserve_for_formulas(&mut self, additional_formulas: usize) {
        self.single_deps
            .reserve(additional_formulas.saturating_mul(2));
    }

    /// Pre-reserves capacity in the single-cell reverse map for a known hot
    /// precedent cell (used by `open_sample` for `$V$1` with 100k dependents).
    pub fn reserve_single_dependents(&mut self, cell: WorkbookCellKey, capacity: usize) {
        if capacity <= 1 {
            return;
        }
        match self.single_deps.entry(cell) {
            Entry::Occupied(mut slot) => match slot.get_mut() {
                DepList::Many(vec) => vec.reserve(capacity),
                DepList::One(first) => {
                    let prev = *first;
                    let mut vec = Vec::with_capacity(capacity.max(2));
                    vec.push(prev);
                    *slot.get_mut() = DepList::Many(vec);
                }
            },
            Entry::Vacant(slot) => {
                slot.insert(DepList::Many(Vec::with_capacity(capacity)));
            }
        }
    }

    /// Extracts and registers all static precedents of `formula` (located on
    /// `host_sheet`) under `fid`.
    pub fn register(
        &mut self,
        fid: u32,
        host_sheet: SheetId,
        formula: &BoundFormula,
    ) -> FormulaRegistration {
        self.register_with_flags(fid, host_sheet, formula).0
    }

    /// Single-pass variant of [`Self::register`] that also reports whether
    /// `formula` invokes any volatile function (`TODAY`, `NOW`, `RAND`,
    /// `RANDBETWEEN`), avoiding redundant AST walks during snapshot decode.
    pub fn register_with_flags(
        &mut self,
        fid: u32,
        host_sheet: SheetId,
        formula: &BoundFormula,
    ) -> (FormulaRegistration, bool) {
        let mut reg = FormulaRegistration::default();
        let mut flags = WalkFlags::default();
        collect_refs(&formula.root, host_sheet, fid, &mut reg, &mut flags);

        for &cell_key in &reg.single_cells {
            match self.single_deps.entry(cell_key) {
                Entry::Vacant(slot) => {
                    slot.insert(DepList::One(fid));
                }
                Entry::Occupied(mut slot) => {
                    slot.get_mut().push(fid);
                }
            }
        }
        if let Some(ranges) = reg.ranges.as_deref() {
            for &(sheet, col, r1, r2) in &ranges.col_spans {
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
            for &(sheet, sub) in &ranges.areas {
                self.multi_col_ranges.entry(sheet).or_default().push(sub);
            }
            for &(sheet, sub) in &ranges.full_cols {
                self.full_col_deps.entry(sheet).or_default().push(sub);
            }
            for &(sheet, sub) in &ranges.full_rows {
                self.full_row_deps.entry(sheet).or_default().push(sub);
            }
        }
        if flags.structural {
            reg.structural_sheet = Some(host_sheet);
            self.structural_set
                .entry(host_sheet)
                .or_default()
                .insert(fid);
        }
        if let Some(extra) = reg.extra.as_deref() {
            for name in &extra.names {
                self.name_deps.entry(name.clone()).or_default().push(fid);
            }
            for &anchor in &extra.spill_anchors {
                self.spill_deps.entry(anchor).or_default().push(fid);
            }
        }
        (reg, flags.volatile)
    }

    /// Removes all reverse edges previously recorded in `reg` for `fid`.
    pub fn unregister(&mut self, fid: u32, reg: &FormulaRegistration) {
        for cell_key in &reg.single_cells {
            if let Some(list) = self.single_deps.get_mut(cell_key)
                && list.remove_first(fid)
            {
                self.single_deps.remove(cell_key);
            }
        }
        if let Some(ranges) = reg.ranges.as_deref() {
            for &(sheet, col, r1, r2) in &ranges.col_spans {
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
            for &(sheet, sub) in &ranges.areas {
                if let Some(list) = self.multi_col_ranges.get_mut(&sheet)
                    && let Some(idx) = list.iter().position(|x| *x == sub)
                {
                    list.swap_remove(idx);
                }
            }
            for &(sheet, sub) in &ranges.full_cols {
                if let Some(list) = self.full_col_deps.get_mut(&sheet)
                    && let Some(idx) = list.iter().position(|x| *x == sub)
                {
                    list.swap_remove(idx);
                }
            }
            for &(sheet, sub) in &ranges.full_rows {
                if let Some(list) = self.full_row_deps.get_mut(&sheet)
                    && let Some(idx) = list.iter().position(|x| *x == sub)
                {
                    list.swap_remove(idx);
                }
            }
        }
        if let Some(sheet) = reg.structural_sheet
            && let Some(set) = self.structural_set.get_mut(&sheet)
        {
            set.remove(&fid);
        }
        if let Some(extra) = reg.extra.as_deref() {
            for name in &extra.names {
                if let Some(list) = self.name_deps.get_mut(name) {
                    if let Some(idx) = list.iter().position(|&x| x == fid) {
                        list.swap_remove(idx);
                    }
                    if list.is_empty() {
                        self.name_deps.remove(name);
                    }
                }
            }
            for &anchor in &extra.spill_anchors {
                if let Some(list) = self.spill_deps.get_mut(&anchor) {
                    if let Some(idx) = list.iter().position(|&x| x == fid) {
                        list.swap_remove(idx);
                    }
                    if list.is_empty() {
                        self.spill_deps.remove(&anchor);
                    }
                }
            }
        }
    }

    /// Calls `emit(fid)` for every registered consumer referencing `canonical_name`
    /// (case-folded with `.to_ascii_lowercase()`, either unqualified or sheet-scoped).
    pub fn for_each_name_dependent(&self, canonical_name: &str, mut emit: impl FnMut(u32)) {
        if let Some(list) = self.name_deps.get(canonical_name) {
            for &fid in list {
                emit(fid);
            }
        }
    }

    /// Calls `emit(fid)` for every formula referencing `anchor#`.
    pub fn for_each_spill_dependent(&self, anchor: WorkbookCellKey, mut emit: impl FnMut(u32)) {
        if let Some(list) = self.spill_deps.get(&anchor) {
            for &fid in list {
                emit(fid);
            }
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
            list.for_each(&mut emit);
        }

        let col_range_list = self.col_ranges.get(&(sheet_id, col));
        let multi_list = self.multi_col_ranges.get(&sheet_id);
        let full_col_list = self.full_col_deps.get(&sheet_id);
        let full_row_list = self.full_row_deps.get(&sheet_id);
        if col_range_list.is_none()
            && multi_list.is_none_or(Vec::is_empty)
            && full_col_list.is_none_or(Vec::is_empty)
            && full_row_list.is_none_or(Vec::is_empty)
        {
            return;
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

        if let Some(list) = col_range_list {
            for sub in list {
                if span_contains_order(row_axis, sub.first_row.0, sub.last_row.0, r_ord) {
                    emit(sub.fid);
                }
            }
        }
        if let Some(list) = multi_list {
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
                            list.for_each(&mut emit);
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
                            list.for_each(&mut emit);
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
                                list.for_each(&mut emit);
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

#[derive(Default)]
struct WalkFlags {
    structural: bool,
    volatile: bool,
}

fn collect_refs(
    expr: &BoundExpr,
    host_sheet: SheetId,
    fid: u32,
    reg: &mut FormulaRegistration,
    flags: &mut WalkFlags,
) {
    match expr {
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
        BoundExpr::Name(s) => {
            let canon = s.to_ascii_lowercase();
            let names = &mut reg.extra_mut().names;
            if !names.contains(&canon) {
                names.push(canon);
            }
        }
        BoundExpr::ScopedName { sheet, name, .. } => {
            let canon = name.to_ascii_lowercase();
            let extra = reg.extra_mut();
            if !extra.names.contains(&canon) {
                extra.names.push(canon);
            }
            if let Some(sid) = *sheet
                && !extra.scoped_sheets.contains(&sid)
            {
                extra.scoped_sheets.push(sid);
            }
        }
        BoundExpr::SpillRef(inner) => {
            let mut cur = inner.as_ref();
            while let BoundExpr::Paren(p) = cur {
                cur = p;
            }
            if let BoundExpr::Reference(BoundRef::Cell {
                sheet, row, col, ..
            }) = cur
            {
                let anchor = (sheet.unwrap_or(host_sheet), *row, *col);
                let anchors = &mut reg.extra_mut().spill_anchors;
                if !anchors.contains(&anchor) {
                    anchors.push(anchor);
                }
            }
            collect_refs(inner, host_sheet, fid, reg, flags);
        }
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
                let ranges = reg.ranges_mut();
                if first_col == last_col {
                    let entry = (s, *first_col, *first_row, *last_row);
                    if !ranges.col_spans.contains(&entry) {
                        ranges.col_spans.push(entry);
                    }
                } else {
                    let sub = AreaSub {
                        fid,
                        first_row: *first_row,
                        last_row: *last_row,
                        first_col: *first_col,
                        last_col: *last_col,
                    };
                    if !ranges.areas.contains(&(s, sub)) {
                        ranges.areas.push((s, sub));
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
                let ranges = reg.ranges_mut();
                if !ranges.full_cols.contains(&(s, sub)) {
                    ranges.full_cols.push((s, sub));
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
                let ranges = reg.ranges_mut();
                if !ranges.full_rows.contains(&(s, sub)) {
                    ranges.full_rows.push((s, sub));
                }
            }
        },
        BoundExpr::Paren(inner) | BoundExpr::Unary { expr: inner, .. } => {
            collect_refs(inner, host_sheet, fid, reg, flags);
        }
        BoundExpr::Binary { left, right, .. } => {
            collect_refs(left, host_sheet, fid, reg, flags);
            collect_refs(right, host_sheet, fid, reg, flags);
        }
        BoundExpr::Call { name, args } => {
            if name == "ROW" || name == "COLUMN" {
                flags.structural = true;
            }
            if is_volatile_function(name) {
                flags.volatile = true;
            }
            for arg in args {
                collect_refs(arg, host_sheet, fid, reg, flags);
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
