//! Formula evaluation & interruptible incremental recalculation engine
//! (`13-m1a.md` 5.6 & 5.8).
//!
//! [`FormulaEngine`] pairs a [`Workbook<BoundFormula>`] with a [`DepGraph`] and
//! a dense `FormulaId` slot table:
//! - Cell-to-cell dependency chains are evaluated using an explicit heap stack
//!   (`call_stack`), never Rust call-stack recursion, so a `1,000,000`-cell
//!   chain (`B5`) runs comfortably inside WebAssembly's default stack.
//! - Circular references are detected in O(1) via `STATE_EVALUATING` and
//!   unwound across the cycle to `#CYCLE!`.
//! - [`FormulaEngine::recalc_with_budget`] yields cleanly between slices, and
//!   any intervening edit resets paused `EVALUATING` frames back to `DIRTY`
//!   without losing work already completed in earlier slices.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::{HashMap, HashSet};
use reticula_core::axis::{Axis, MAX_COL_AXIS_LEN};
use reticula_core::decimal15::{RoundingMode, round_decimal, snap_cancelled_zero};
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::input::{InferredInput, infer_input, infer_input_for_cell};
use reticula_core::memory::try_reserve_heap;
use reticula_core::ops::{
    CellEdit, CellInput, CellSnapshot, FormatBackup, FormatPatch, FormulaMode, InverseOp,
    NamedRangeKey, Op, OpError, PermutedCellBackup, ScalarValue, SeedOrigin, Workbook,
    WorkbookCellKey,
};
use reticula_core::sample;
use reticula_core::sheet::Sheet;
use reticula_core::snapshot::{SnapshotError, decode_snapshot, encode_snapshot};
use reticula_core::value::{CalcError, Value};
use reticula_core::viewport::{
    FLAG_FORMULA, FLAG_SPILL, Page, ViewportCellOverlay, encode_page_with_formulas,
    encode_projected_page_with_overlay, prepare_projected_page_styles,
};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::address::CellAddr;
use crate::ast::{BinaryOp, Literal, MAX_NESTING_DEPTH};
use crate::binder::{BoundExpr, BoundFormula, BoundRef, live_span_positions};
use crate::dep_graph::{DepGraph, FormulaRegistration};
use crate::filter::{
    ColFilterCriterion, DistinctColumnValues, FilterTable, VisibleRowProjection,
    distinct_column_values, evaluate_filter_table,
};
use crate::functions::{EvalEnv, combine_choose, combine_if, combine_iferror, eval_eager_function};
use crate::parser::ParseError;
use crate::spill::{CommitSpillOutcome, SpillCollisionOrder, SpillRegistry};
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, ResolvedRef, ScalarOrArray,
    coerce_for_scalar_op, eval_binary, eval_unary, finalize_top_level, materialise_reference,
    resolve_bound_ref,
};

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
type FxSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;

const STATE_CLEAN: u8 = 0;
const STATE_DIRTY: u8 = 1;
const STATE_EVALUATING: u8 = 2;
const STATE_SCC_WAIT: u8 = 3;
const MAX_WAVE_SPILL_REQUEUES: u8 = 16;

/// Number of live formula rows populated in `M2..=M100001` of the default
/// 1,000,000-row sample (`13-m1a.md` 5.8).
pub const SAMPLE_FORMULA_ROWS: u32 = 100_000;

/// Sample generator format version recorded in [`SeedOrigin`].
pub const SAMPLE_GENERATOR_VERSION: u32 = 2;

/// Result of one incremental recalculation slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecalcStatus {
    /// Recalculation yielded with dirty formulas still remaining.
    InProgress {
        /// Formulas evaluated so far in the current recalculation wave.
        done: u32,
        /// Current total formulas queued in this wave (`done + remaining`).
        total: u32,
    },
    /// All dirty formulas have settled to [`STATE_CLEAN`].
    Complete {
        /// Total formulas recalculated across the wave.
        recalculated: u32,
    },
}

/// Summary of cells or structural axes dirtied since the last viewport flush.
#[derive(Clone, Debug, Default)]
pub struct DirtyChanges {
    /// True if row or column axes changed (requiring all subscribed viewport
    /// pages on affected sheets to re-push).
    pub structural: bool,
    /// Individual cells whose visible value or format changed.
    pub cells: Vec<WorkbookCellKey>,
}

/// Authoring / inspection error surfaced by [`FormulaEngine::edit_cell_raw`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditError {
    /// Formula syntax or static limit error.
    Parse(ParseError),
    /// Workbook operation or limit error.
    Op(OpError),
}

impl From<ParseError> for EditError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

impl From<OpError> for EditError {
    fn from(e: OpError) -> Self {
        Self::Op(e)
    }
}

/// Detailed state of a single cell for formula-bar and `F2` inspection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedCell {
    /// Canonical A1 coordinate (`"A1"`, `"V1"`).
    pub address: String,
    /// Raw authoring string (`"=SUM(M2:M100001)"`, `"'001"`, `"2026-11-18"`, `"42"`).
    pub raw: String,
    /// Formatted display text (`"123.45"`, `"TRUE"`, `"#DIV/0!"`).
    pub display: String,
    /// Whether the cell currently holds a formula.
    pub is_formula: bool,
    /// Error code string (`"#DIV/0!"`, `"#SPILL!"`, `"#CYCLE!"`), if in error.
    pub error: Option<String>,
}

/// Error returned by [`FormulaEngine::evaluate_transient_formula`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransientEvalError {
    /// Target worksheet does not exist or has been deleted.
    SheetNotFound,
    /// Anchor cell coordinates are outside the worksheet's live axes.
    OutOfBounds,
    /// Formula syntax or static limit error.
    Parse(ParseError),
}

/// Result of evaluating a formula without mutating the workbook or dependency graph.
#[derive(Clone, Debug, PartialEq)]
pub enum TransientEvalValue {
    /// Single scalar evaluation outcome.
    Scalar(EvalScalar),
    /// Dynamic 2D array outcome (`rows x cols`).
    Array(Array2D),
}

/// Direct precedent cell inspected for an errored formula cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrecedentTrace {
    /// Target worksheet display name.
    pub sheet_name: String,
    /// Canonical A1 coordinate (`"B2"`).
    pub address: String,
    /// Formatted display text of the precedent cell.
    pub display: String,
    /// Error code string if the precedent cell itself holds an error.
    pub error: Option<String>,
}

/// Diagnostic record for a cell currently holding a calculation error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorCellTrace {
    /// Worksheet identity.
    pub sheet_id: SheetId,
    /// Worksheet display name.
    pub sheet_name: String,
    /// Row identity.
    pub row: RowId,
    /// Column identity.
    pub col: ColId,
    /// Canonical A1 coordinate (`"D5"`).
    pub address: String,
    /// Calculation error code (`"#DIV/0!"`, `"#REF!"`, `"#SPILL!"`, etc.).
    pub error: String,
    /// Raw formula string if this cell contains a formula (`None` for literal errors).
    pub formula: Option<String>,
    /// Direct precedent cells (capped at 16 per errored cell).
    pub precedents: Vec<PrecedentTrace>,
}

#[derive(Clone, Debug)]
struct FormulaSlot {
    key: WorkbookCellKey,
    reg: FormulaRegistration,
}

#[derive(Clone, Debug, Default)]
struct CycleSpillRecord {
    peers: Vec<WorkbookCellKey>,
    cleared_shadows: Vec<WorkbookCellKey>,
}

enum ExprOutcome {
    Ready(EvalValue),
    Blocked(SmallVec<[u32; 4]>),
    Cycle(usize),
}

#[derive(Clone, Copy)]
struct DepView<'a> {
    dep_graph: &'a DepGraph,
    name_dep_graph: &'a DepGraph,
    name_slots: &'a [Option<(NamedRangeKey, FormulaRegistration)>],
    cell_to_fid: &'a FxMap<WorkbookCellKey, u32>,
    slots: &'a [Option<FormulaSlot>],
    workbook: &'a Workbook<BoundFormula>,
}

/// Complete single-node spreadsheet engine (`Workbook` + `DepGraph` + time-sliced
/// recalculator).
#[derive(Clone, Debug)]
pub struct FormulaEngine {
    workbook: Workbook<BoundFormula>,
    dep_graph: DepGraph,
    spills: SpillRegistry,
    name_dep_graph: DepGraph,
    name_slots: Vec<Option<(NamedRangeKey, FormulaRegistration)>>,
    name_to_nid: BTreeMap<NamedRangeKey, u32>,
    free_name_slots: Vec<u32>,
    volatile_names: BTreeSet<u32>,
    filter_sensitive_names: BTreeSet<u32>,
    name_eval_stack: Vec<NamedRangeKey>,
    wave_spill_requeues: BTreeMap<u32, u8>,
    wave_cycle_verified: BTreeSet<u32>,
    scc_stack: Vec<u32>,
    scc_low: BTreeMap<u32, usize>,
    wave_deferred_dirty: Vec<u32>,
    cycle_spills: BTreeMap<WorkbookCellKey, CycleSpillRecord>,
    cycle_shadow_watch: BTreeMap<WorkbookCellKey, Vec<WorkbookCellKey>>,
    slots: Vec<Option<FormulaSlot>>,
    states: Vec<u8>,
    free_slots: Vec<u32>,
    cell_to_fid: FxMap<WorkbookCellKey, u32>,
    /// Count of formulas in `(sheet, col)` currently in `DIRTY` or `EVALUATING`
    /// state — lets range aggregations skip per-cell `cell_to_fid` checks in
    /// O(1) whenever an entire column is clean.
    non_clean_in_col: FxMap<(SheetId, ColId), u32>,
    dirty_queue: Vec<u32>,
    dirty_cursor: usize,
    dirty_needs_sort: bool,
    call_stack: Vec<u32>,
    wave_done: u32,
    changes: DirtyChanges,
    volatile_set: BTreeSet<u32>,
    filter_sensitive_set: BTreeSet<u32>,
    filters: BTreeMap<SheetId, (FilterTable, VisibleRowProjection)>,
    env: EvalEnv,
}

impl Default for FormulaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FormulaEngine {
    /// Creates an engine holding a single standard empty `Sheet1`.
    #[must_use]
    pub fn new() -> Self {
        Self::from_workbook_clean(Workbook::new())
    }

    /// Rebuilds the dependency graph from an existing `Workbook<BoundFormula>`
    /// whose cached values are already valid (such as a decoded snapshot).
    #[must_use]
    pub fn from_workbook_clean(workbook: Workbook<BoundFormula>) -> Self {
        let count = workbook.formulas().len();
        let mut dep_graph = DepGraph::with_formula_capacity(count);
        let mut slots = Vec::with_capacity(count);
        let states = vec![STATE_CLEAN; count];
        let mut cell_to_fid = FxMap::with_capacity_and_hasher(count, BuildHasherDefault::default());
        let mut volatile_set = BTreeSet::new();
        let mut filter_sensitive_set = BTreeSet::new();

        for (&key, formula) in workbook.formulas() {
            let fid = u32::try_from(slots.len()).unwrap_or(0);
            let (reg, flags) = dep_graph.register_detailed(fid, key.0, formula);
            if flags.volatile {
                volatile_set.insert(fid);
            }
            if flags.filter_sensitive {
                filter_sensitive_set.insert(fid);
            }
            slots.push(Some(FormulaSlot { key, reg }));
            cell_to_fid.insert(key, fid);
        }

        let name_count = workbook.named_ranges().len();
        let mut name_dep_graph = DepGraph::with_formula_capacity(name_count);
        let mut name_slots = Vec::with_capacity(name_count);
        let mut name_to_nid = BTreeMap::new();
        let mut volatile_names = BTreeSet::new();
        let mut filter_sensitive_names = BTreeSet::new();
        let fallback_sheet = workbook_fallback_sheet(&workbook);
        for (nkey, def) in workbook.named_ranges() {
            let nid = u32::try_from(name_slots.len()).unwrap_or(0);
            let host_sheet = nkey.scope.unwrap_or(fallback_sheet);
            let (reg, flags) = name_dep_graph.register_detailed(nid, host_sheet, &def.formula);
            if flags.volatile {
                volatile_names.insert(nid);
            }
            if flags.filter_sensitive {
                filter_sensitive_names.insert(nid);
            }
            name_slots.push(Some((nkey.clone(), reg)));
            name_to_nid.insert(nkey.clone(), nid);
        }

        Self {
            workbook,
            dep_graph,
            spills: SpillRegistry::new(),
            name_dep_graph,
            name_slots,
            name_to_nid,
            free_name_slots: Vec::new(),
            volatile_names,
            filter_sensitive_names,
            name_eval_stack: Vec::new(),
            wave_spill_requeues: BTreeMap::new(),
            wave_cycle_verified: BTreeSet::new(),
            scc_stack: Vec::new(),
            scc_low: BTreeMap::new(),
            wave_deferred_dirty: Vec::new(),
            cycle_spills: BTreeMap::new(),
            cycle_shadow_watch: BTreeMap::new(),
            slots,
            states,
            free_slots: Vec::new(),
            cell_to_fid,
            non_clean_in_col: FxMap::default(),
            dirty_queue: Vec::new(),
            dirty_cursor: 0,
            dirty_needs_sort: false,
            call_stack: Vec::new(),
            wave_done: 0,
            changes: DirtyChanges::default(),
            volatile_set,
            filter_sensitive_set,
            filters: BTreeMap::new(),
            env: EvalEnv::default(),
        }
    }

    /// Rebuilds an engine from `workbook` and marks every formula dirty for a
    /// full recalculation from scratch.
    #[must_use]
    pub fn from_workbook_recalc_all(workbook: Workbook<BoundFormula>) -> Self {
        let mut engine = Self::from_workbook_clean(workbook);
        let len = u32::try_from(engine.slots.len()).unwrap_or(0);
        for fid in 0..len {
            engine.mark_fid_dirty_only(fid);
        }
        engine.recalc_all();
        engine.changes = DirtyChanges::default();
        engine
    }

    /// Installs a reconciled workbook and queues full recalculation for later slices.
    pub fn replace_reconciled_workbook(&mut self, workbook: Workbook<BoundFormula>) {
        let mut engine = Self::from_workbook_clean(workbook);
        engine.env = self.env.clone();
        let count = u32::try_from(engine.slots.len()).unwrap_or(0);
        for fid in 0..count {
            engine.mark_fid_dirty_only(fid);
        }
        *self = engine;
    }

    /// Generates the upgraded seeded sample (`13-m1a.md` 5.8):
    /// - `rows × 22` columns (`A..T` sales columns + `U1:V2` parameter panel);
    /// - `U1 = "促销立减率"`, `V1 = 0%` (`Percent`), `U2 = "前10万单实收合计"`,
    ///   `V2 = "=SUM(M2:M100001)"` (`Fixed2`);
    /// - Rows `2..=min(rows, 100_001)` in column `M` carry bound formulas
    ///   `=ROUND(J{r}*K{r}*(1-L{r}-$V$1), 2)` with pre-populated cached values
    ///   and pre-registered reverse edges so cold start requires zero redundant
    ///   evaluation while editing `V1` immediately triggers `100,001` cells!
    #[must_use]
    pub fn open_sample(seed: u64, rows: u32) -> Self {
        let formula_rows = rows.saturating_sub(1).min(SAMPLE_FORMULA_ROWS);
        let extra_formula_heap = (formula_rows as usize).saturating_mul(1_152);
        let total_reserve = (rows as usize)
            .saturating_mul(200)
            .saturating_add(extra_formula_heap)
            .saturating_add(16 * 1024 * 1024);
        let _ = try_reserve_heap(total_reserve);

        let mut sheet = sample::generate(seed, rows);
        // Extend column axis from 20 (`A..T`) to 22 (`A..V`) using base IDs 20 and 21.
        let new_col_axis = Axis::with_base(22, MAX_COL_AXIS_LEN);
        *sheet.col_axis_mut() = new_col_axis;
        sheet.set_col_format_by_id(ColId(20), NumberFormat::General);
        sheet.set_col_format_by_id(ColId(21), NumberFormat::General);

        let sheet_id = sheet.id();
        if rows >= 1 {
            if let Some(lbl_rate) = sheet.add_string("促销立减率") {
                sheet.set_value_by_id(RowId(0), ColId(20), Value::Text(lbl_rate));
            }
            sheet.set_value_by_id(RowId(0), ColId(21), Value::Number(0.0));
            sheet.set_cell_format_by_id(RowId(0), ColId(21), Some(NumberFormat::Percent));
        }
        if rows >= 2 {
            if let Some(lbl_sum) = sheet.add_string("前10万单实收合计") {
                sheet.set_value_by_id(RowId(1), ColId(20), Value::Text(lbl_sum));
            }
            sheet.set_cell_format_by_id(RowId(1), ColId(21), Some(NumberFormat::Fixed2));
        }

        // Reconcile `M` (`col 12`) cached values on `1..=formula_rows` with
        // exact 15-digit `ROUND(J*K*(1-L), 2)` and accumulate `V2`'s initial sum.
        let mut sum = 0.0_f64;
        let mut prev_sum = 0.0_f64;
        for r in 1..=formula_rows {
            let row_id = RowId(u64::from(r));
            let j = match sheet.value_by_id(row_id, ColId(9)) {
                Value::Number(v) => v,
                _ => 0.0,
            };
            let k = match sheet.value_by_id(row_id, ColId(10)) {
                Value::Number(v) => v,
                _ => 0.0,
            };
            let l = match sheet.value_by_id(row_id, ColId(11)) {
                Value::Number(v) => v,
                _ => 0.0,
            };
            let amt =
                round_decimal(j * k * (1.0 - l), 2, RoundingMode::HalfAwayFromZero).unwrap_or(0.0);
            sheet.set_value_by_id(row_id, ColId(12), Value::number(amt));
            // Also keep profit (`col 14`) & tax (`col 17`) consistent if `amt` changed by 1 cent.
            prev_sum = sum;
            sum += amt;
        }
        let v2_initial = if formula_rows >= 2 {
            snap_cancelled_zero(prev_sum, sum)
        } else {
            sum
        };
        if rows >= 2 {
            sheet.set_value_by_id(RowId(1), ColId(21), Value::number(v2_initial));
        }

        let origin = SeedOrigin {
            seed,
            rows,
            generator_version: SAMPLE_GENERATOR_VERSION,
        };
        let total_formulas = if formula_rows > 0 {
            (formula_rows as usize).saturating_add(1)
        } else {
            0
        };
        let mut wb = Workbook::from_sheet(sheet, Some(origin));
        let mut dep_graph = DepGraph::new();
        if formula_rows > 0 {
            dep_graph.reserve_single_dependents((sheet_id, RowId(0), ColId(21)), total_formulas);
        }
        let mut slots = Vec::with_capacity(total_formulas);
        let states = vec![STATE_CLEAN; total_formulas];
        let mut cell_to_fid =
            FxMap::with_capacity_and_hasher(total_formulas, BuildHasherDefault::default());

        for r in 1..=formula_rows {
            let row_id = RowId(u64::from(r));
            let formula = build_sample_row_formula(row_id);
            let cached = wb
                .sheet(sheet_id)
                .map_or(Value::Number(0.0), |s| s.value_by_id(row_id, ColId(12)));
            let fid = u32::try_from(slots.len()).unwrap_or(0);
            let key = (sheet_id, row_id, ColId(12));
            let reg = dep_graph.register(fid, sheet_id, &formula);
            wb.insert_seeded_formula(sheet_id, row_id, ColId(12), formula, cached);
            slots.push(Some(FormulaSlot { key, reg }));
            cell_to_fid.insert(key, fid);
        }

        if formula_rows > 0 {
            let sum_formula = BoundFormula {
                mode: FormulaMode::Dynamic,
                root: BoundExpr::Call {
                    name: String::from("SUM"),
                    args: vec![BoundExpr::Reference(BoundRef::Area {
                        sheet: None,
                        missing_sheet: None,
                        first_row: RowId(1),
                        first_col: ColId(12),
                        last_row: RowId(u64::from(formula_rows)),
                        last_col: ColId(12),
                        abs_first_col: false,
                        abs_first_row: false,
                        abs_last_col: false,
                        abs_last_row: false,
                    })],
                },
            };
            let fid = u32::try_from(slots.len()).unwrap_or(0);
            let key = (sheet_id, RowId(1), ColId(21));
            let reg = dep_graph.register(fid, sheet_id, &sum_formula);
            wb.insert_seeded_formula(
                sheet_id,
                RowId(1),
                ColId(21),
                sum_formula,
                Value::number(v2_initial),
            );
            slots.push(Some(FormulaSlot { key, reg }));
            cell_to_fid.insert(key, fid);
        }

        Self {
            workbook: wb,
            dep_graph,
            spills: SpillRegistry::new(),
            name_dep_graph: DepGraph::new(),
            name_slots: Vec::new(),
            name_to_nid: BTreeMap::new(),
            free_name_slots: Vec::new(),
            volatile_names: BTreeSet::new(),
            filter_sensitive_names: BTreeSet::new(),
            name_eval_stack: Vec::new(),
            wave_spill_requeues: BTreeMap::new(),
            wave_cycle_verified: BTreeSet::new(),
            scc_stack: Vec::new(),
            scc_low: BTreeMap::new(),
            wave_deferred_dirty: Vec::new(),
            cycle_spills: BTreeMap::new(),
            cycle_shadow_watch: BTreeMap::new(),
            slots,
            states,
            free_slots: Vec::new(),
            cell_to_fid,
            non_clean_in_col: FxMap::default(),
            dirty_queue: Vec::new(),
            dirty_cursor: 0,
            dirty_needs_sort: false,
            call_stack: Vec::new(),
            wave_done: 0,
            changes: DirtyChanges::default(),
            volatile_set: BTreeSet::new(),
            filter_sensitive_set: BTreeSet::new(),
            filters: BTreeMap::new(),
            env: EvalEnv::default(),
        }
    }

    /// Current host-injected volatile evaluation environment.
    #[must_use]
    pub const fn eval_env(&self) -> &EvalEnv {
        &self.env
    }

    /// Injects a fresh volatile evaluation environment (`now_serial` in the
    /// Excel 1900 date system and a 64-bit PRNG seed) and marks all volatile
    /// formulas (`TODAY`, `NOW`, `RAND`, `RANDBETWEEN`) and their dependents
    /// dirty (`14-m1b.md` 5.3).
    pub fn set_eval_env(&mut self, now_serial: f64, seed: u64) {
        self.env = EvalEnv::new(now_serial, seed);
        self.invalidate_volatile_after_edit();
    }

    /// Underlying [`Workbook<BoundFormula>`].
    #[must_use]
    pub const fn workbook(&self) -> &Workbook<BoundFormula> {
        &self.workbook
    }

    /// Active and blocked dynamic-array spill registry.
    #[must_use]
    pub const fn spills(&self) -> &SpillRegistry {
        &self.spills
    }

    /// Returns the materialized `(rows, cols)` spill shape anchored at `anchor`, if active.
    #[must_use]
    pub fn spill_range(&self, anchor: WorkbookCellKey) -> Option<(u32, u32)> {
        self.spills.spill_shape(anchor)
    }

    /// Installs (or replaces) a worker-local AutoFilter table on `sheet_id`.
    /// Filtering never mutates [`Workbook`], WAL logs, or `StateDigest`.
    pub fn set_filter_table(
        &mut self,
        sheet_id: SheetId,
        header_row: u32,
        last_row: u32,
        first_col: u32,
        last_col: u32,
    ) -> bool {
        let Some(sheet) = self.workbook.live_sheet(sheet_id) else {
            return false;
        };
        if header_row >= sheet.rows() || first_col >= sheet.cols() {
            return false;
        }
        let clamped_last_row = last_row.max(header_row).min(sheet.rows().saturating_sub(1));
        let lo_col = first_col.min(last_col).min(sheet.cols().saturating_sub(1));
        let hi_col = first_col.max(last_col).min(sheet.cols().saturating_sub(1));
        let table = FilterTable {
            header_row: RowId(sheet.row_id(header_row)),
            last_row: RowId(sheet.row_id(clamped_last_row)),
            first_col: ColId(sheet.col_id(lo_col)),
            last_col: ColId(sheet.col_id(hi_col)),
            criteria: BTreeMap::new(),
        };
        let proj = VisibleRowProjection::identity(sheet.rows());
        let had_hidden = self
            .filters
            .insert(sheet_id, (table, proj))
            .is_some_and(|(_, old)| !old.is_empty());
        self.changes.structural = true;
        if had_hidden {
            self.invalidate_filter_sensitive();
        }
        true
    }

    /// Sets or clears one column criterion on `sheet_id`'s AutoFilter and
    /// updates its worker-local [`VisibleRowProjection`].
    pub fn set_column_filter(
        &mut self,
        sheet_id: SheetId,
        col_pos: u32,
        criterion: Option<ColFilterCriterion>,
    ) -> bool {
        let Some(sheet) = self.workbook.live_sheet(sheet_id) else {
            return false;
        };
        let Some((table, old_proj)) = self.filters.get_mut(&sheet_id) else {
            return false;
        };
        let Some(bounds) = table.resolve_bounds(sheet) else {
            return false;
        };
        if col_pos < bounds.first_col || col_pos > bounds.last_col || col_pos >= sheet.cols() {
            return false;
        }
        let col_id = ColId(sheet.col_id(col_pos));
        match criterion {
            Some(c) => {
                table.criteria.insert(col_id, c);
            }
            None => {
                table.criteria.remove(&col_id);
            }
        }
        let Some(new_proj) = evaluate_filter_table(sheet, table, Some(&self.spills)) else {
            return false;
        };
        let hidden_changed = old_proj.hidden_row_ids() != new_proj.hidden_row_ids();
        *old_proj = new_proj;
        self.changes.structural = true;
        if hidden_changed {
            self.invalidate_filter_sensitive();
        }
        true
    }

    /// Re-evaluates all column criteria on `sheet_id`'s AutoFilter against live
    /// cell and spill values.
    pub fn reapply_filter(&mut self, sheet_id: SheetId) {
        let Some(sheet) = self.workbook.live_sheet(sheet_id) else {
            return;
        };
        let Some((table, old_proj)) = self.filters.get_mut(&sheet_id) else {
            return;
        };
        if let Some(new_proj) = evaluate_filter_table(sheet, table, Some(&self.spills)) {
            let hidden_changed = old_proj.hidden_row_ids() != new_proj.hidden_row_ids();
            *old_proj = new_proj;
            if hidden_changed {
                self.changes.structural = true;
                self.invalidate_filter_sensitive();
            }
        }
    }

    /// Removes any worker-local AutoFilter from `sheet_id`, unhiding all rows.
    pub fn clear_filter(&mut self, sheet_id: SheetId) {
        if let Some((_, old_proj)) = self.filters.remove(&sheet_id) {
            self.changes.structural = true;
            if !old_proj.is_empty() {
                self.invalidate_filter_sensitive();
            }
        }
    }

    /// Returns the active [`FilterTable`] and [`VisibleRowProjection`] for `sheet_id`.
    #[must_use]
    pub fn filter_state(&self, sheet_id: SheetId) -> Option<(&FilterTable, &VisibleRowProjection)> {
        self.filters.get(&sheet_id).map(|(t, p)| (t, p))
    }

    /// Returns `true` if `row_id` on `sheet_id` is currently hidden by AutoFilter.
    #[must_use]
    pub fn is_row_hidden(&self, sheet_id: SheetId, row_id: RowId) -> bool {
        self.filters
            .get(&sheet_id)
            .is_some_and(|(_, p)| p.is_row_id_hidden(row_id))
    }

    /// Maps a visible 0-based display row on `sheet_id` to its physical row position.
    #[must_use]
    pub fn display_to_physical_row(&self, sheet_id: SheetId, display_row: u32) -> Option<u32> {
        let sheet = self.workbook.sheet(sheet_id)?;
        if let Some((_, proj)) = self.filters.get(&sheet_id) {
            proj.display_to_physical(display_row)
        } else if display_row < sheet.rows() {
            Some(display_row)
        } else {
            None
        }
    }

    /// Maps a physical 0-based row on `sheet_id` to its visible display row
    /// position, or `None` if hidden or out of bounds.
    #[must_use]
    pub fn physical_to_display_row(&self, sheet_id: SheetId, physical_row: u32) -> Option<u32> {
        let sheet = self.workbook.sheet(sheet_id)?;
        if let Some((_, proj)) = self.filters.get(&sheet_id) {
            proj.physical_to_display(physical_row)
        } else if physical_row < sheet.rows() {
            Some(physical_row)
        } else {
            None
        }
    }

    /// Number of visible rows on `sheet_id` under the active filter projection.
    #[must_use]
    pub fn visible_row_count(&self, sheet_id: SheetId) -> u32 {
        if let Some((_, proj)) = self.filters.get(&sheet_id) {
            proj.visible_rows()
        } else {
            self.workbook.sheet(sheet_id).map_or(0, Sheet::rows)
        }
    }

    /// Collects sorted distinct display values in column `col_pos` of `sheet_id`'s
    /// active filter table (respecting criteria on other columns).
    #[must_use]
    pub fn distinct_filter_values(
        &self,
        sheet_id: SheetId,
        col_pos: u32,
        limit: usize,
    ) -> DistinctColumnValues {
        let Some(sheet) = self.workbook.live_sheet(sheet_id) else {
            return DistinctColumnValues::default();
        };
        let Some((table, _)) = self.filters.get(&sheet_id) else {
            return DistinctColumnValues::default();
        };
        if col_pos >= sheet.cols() {
            return DistinctColumnValues::default();
        }
        distinct_column_values(
            sheet,
            table,
            ColId(sheet.col_id(col_pos)),
            Some(&self.spills),
            limit,
        )
    }

    /// Encodes one v2 viewport page of `sheet_id`, marking formula cells with
    /// `FLAG_FORMULA` and overlaid spill shadow cells with `FLAG_SPILL`.
    #[must_use]
    pub fn encode_viewport_page(
        &self,
        sheet_id: SheetId,
        page: Page,
        epoch: u32,
        text: &mut String,
    ) -> Option<Vec<u8>> {
        let sheet = self.workbook.sheet(sheet_id)?;
        let proj = self.filters.get(&sheet_id).map(|(_, p)| p);
        if proj.is_none() && !self.spills.has_spills_on_sheet(sheet_id) {
            return Some(encode_page_with_formulas(
                sheet,
                page,
                epoch,
                text,
                |r, c| self.has_formula(sheet_id, r, c),
            ));
        }
        let visible_rows = proj.map_or_else(|| sheet.rows(), VisibleRowProjection::visible_rows);
        Some(encode_projected_page_with_overlay(
            sheet,
            page,
            epoch,
            visible_rows,
            |disp| match proj {
                Some(p) => p.display_to_physical(disp),
                None => Some(disp),
            },
            text,
            |r, c| {
                let mut ov = ViewportCellOverlay::default();
                if self.has_formula(sheet_id, r, c) {
                    ov.extra_flags |= FLAG_FORMULA;
                } else if self.spills.is_blocked_anchor((sheet_id, r, c)) {
                    ov.extra_flags |= FLAG_SPILL;
                } else if let Some((val, flags)) = self.spills.viewport_overlay(sheet_id, r, c) {
                    ov.value = Some(val);
                    ov.extra_flags |= flags;
                }
                ov
            },
        ))
    }

    /// Interns resolved styles for `page` on `sheet_id` before immutable
    /// viewport page encoding.
    pub fn prepare_page_styles(&mut self, sheet_id: SheetId, page: reticula_core::viewport::Page) {
        let proj = self.filters.get(&sheet_id).map(|(_, p)| p);
        if let Some(sheet) = self.workbook.sheet_mut(sheet_id) {
            let visible_rows =
                proj.map_or_else(|| sheet.rows(), VisibleRowProjection::visible_rows);
            prepare_projected_page_styles(sheet, page, visible_rows, |disp| match proj {
                Some(p) => p.display_to_physical(disp),
                None => Some(disp),
            });
        }
    }

    /// Pre-interns combined `col × row` and `cell` styles on `sheet_id` so the
    /// sheet's style table covers visible combinations before `SheetInfo` is
    /// sent to the main thread.
    pub fn ensure_sheet_styles_interned(&mut self, sheet_id: SheetId) {
        let Some(sheet) = self.workbook.sheet_mut(sheet_id) else {
            return;
        };
        if sheet.col_styles().is_empty()
            && sheet.row_styles().is_empty()
            && sheet.cell_styles().is_empty()
        {
            return;
        }
        let mut col_patches = Vec::new();
        for &cp in sheet.col_styles().values() {
            if !col_patches.contains(&cp) {
                col_patches.push(cp);
                if col_patches.len() >= 64 {
                    break;
                }
            }
        }
        let mut row_patches = Vec::new();
        for &rp in sheet.row_styles().values() {
            if !row_patches.contains(&rp) {
                row_patches.push(rp);
                if row_patches.len() >= 64 {
                    break;
                }
            }
        }
        let base = reticula_core::style::CellStyle::default();
        for &cp in &col_patches {
            let col_s = cp.apply_to(base);
            let _ = sheet.styles_mut().intern(col_s);
            for &rp in &row_patches {
                let _ = sheet.styles_mut().intern(rp.apply_to(col_s));
            }
        }
        let cell_keys: Vec<(RowId, ColId)> = sheet.cell_styles().keys().copied().collect();
        for (r, c) in cell_keys {
            let eff = sheet.effective_style_by_id(r, c);
            let _ = sheet.styles_mut().intern(eff);
        }
    }

    /// Allocates and returns a fresh [`SheetId`] from the workbook's sheet
    /// counter for [`Op::AddSheet`].
    pub fn alloc_sheet_id(&mut self) -> SheetId {
        self.workbook.alloc_sheet_id()
    }

    /// Purges tombstoned worksheets, rows, columns and orphaned strings when the
    /// session undo stack is empty (`14-m1b.md` 5.5), rebuilding the dependency
    /// graph cleanly over the compacted workbook.
    pub fn compact_local_garbage(&mut self) {
        self.recalc_all();
        let mut spill_anchors = Vec::new();
        if !self.spills.is_empty() {
            for sheet in self.workbook.live_sheets() {
                spill_anchors.extend(self.spills.anchors_on_sheet(sheet.id()));
            }
        }
        for (anchor, rec) in &self.cycle_spills {
            spill_anchors.push(*anchor);
            spill_anchors.extend_from_slice(&rec.peers);
        }
        let mut wb = core::mem::take(&mut self.workbook);
        wb.compact_local_garbage();
        let mut rebuilt = Self::from_workbook_clean(wb);
        if !spill_anchors.is_empty() {
            let mut roots = Vec::new();
            for anchor in spill_anchors {
                if let Some(&fid) = rebuilt.cell_to_fid.get(&anchor) {
                    roots.push(fid);
                }
            }
            rebuilt.propagate_dirty(roots);
            rebuilt.recalc_all();
            rebuilt.changes = DirtyChanges::default();
        }
        *self = rebuilt;
    }

    /// Mutable access to the workbook's [`IdAllocator`] for allocating fresh
    /// row/column [`IdRun`]s prior to [`Self::apply_op`].
    pub const fn allocator_mut(&mut self) -> &mut reticula_core::id::IdAllocator {
        self.workbook.allocator_mut()
    }

    /// Whether dirty formulas remain queued for recalculation.
    #[must_use]
    pub fn has_pending_recalc(&self) -> bool {
        self.dirty_cursor < self.dirty_queue.len()
            || !self.call_stack.is_empty()
            || !self.scc_stack.is_empty()
    }

    /// O(1) check whether `(sheet_id, row, col)` holds a bound formula.
    #[inline]
    #[must_use]
    pub fn has_formula(&self, sheet_id: SheetId, row: RowId, col: ColId) -> bool {
        !self.cell_to_fid.is_empty() && self.cell_to_fid.contains_key(&(sheet_id, row, col))
    }

    /// Takes accumulated dirty cell / structural notifications since the last
    /// call.
    pub fn take_dirty_changes(&mut self) -> DirtyChanges {
        core::mem::take(&mut self.changes)
    }

    /// Encodes the entire workbook (including bound formulas and cached values)
    /// into an `RTCLSNAP` v0 byte buffer.
    pub fn encode_snapshot(&self) -> Result<Vec<u8>, SnapshotError> {
        encode_snapshot(&self.workbook)
    }

    /// Decodes an `RTCLSNAP` v0 byte slice into a ready-to-use [`FormulaEngine`].
    pub fn decode_snapshot(bytes: &[u8]) -> Result<(Self, String), SnapshotError> {
        let (wb, ver) = decode_snapshot(bytes)?;
        Ok((Self::from_workbook_clean(wb), ver))
    }

    /// Inspects `(sheet_id, row, col)` for the formula bar or `F2` editor.
    #[must_use]
    pub fn inspect_cell(&self, sheet_id: SheetId, row: RowId, col: ColId) -> Option<InspectedCell> {
        let sheet = self.workbook.sheet(sheet_id)?;
        let r_pos = sheet.row_axis().id_to_pos(row.0)?;
        let c_pos = sheet.col_axis().id_to_pos(col.0)?;

        let mut address = String::new();
        CellAddr {
            col: c_pos,
            row: r_pos,
            abs_col: false,
            abs_row: false,
        }
        .write_a1(&mut address);

        let val = sheet.value_by_id(row, col);
        if val == Value::Empty
            && self.workbook.formula(sheet_id, row, col).is_none()
            && let Some(shadow) = self.spills.shadow_value(sheet_id, row, col)
        {
            let mut display = String::new();
            let error = match shadow {
                EvalScalar::Empty => None,
                EvalScalar::Number(n) => {
                    let packed = Value::number(*n);
                    sheet.format_value(packed, row, col, &mut display);
                    if let Value::Error(e) = packed {
                        Some(String::from(e.as_str()))
                    } else {
                        None
                    }
                }
                EvalScalar::Bool(b) => {
                    sheet.format_value(Value::Bool(*b), row, col, &mut display);
                    None
                }
                EvalScalar::Error(e) => {
                    display.push_str(e.as_str());
                    Some(String::from(e.as_str()))
                }
                EvalScalar::Text(s) => {
                    display.push_str(s);
                    None
                }
            };
            return Some(InspectedCell {
                address,
                raw: String::new(),
                display,
                is_formula: false,
                error,
            });
        }

        let mut display = String::new();
        sheet.format_value(val, row, col, &mut display);

        let error = match val {
            Value::Error(e) => Some(String::from(e.as_str())),
            Value::Array(_) => Some(String::from("#SPILL!")),
            _ => None,
        };

        if let Some(bound) = self.workbook.formula(sheet_id, row, col) {
            return Some(InspectedCell {
                address,
                raw: bound.unparse(&self.workbook, sheet_id),
                display,
                is_formula: true,
                error,
            });
        }

        let raw = match val {
            Value::Empty => String::new(),
            Value::Number(_) | Value::Bool(_) | Value::Error(_) | Value::Array(_) => {
                display.clone()
            }
            Value::Text(id) => {
                let text = sheet.strings().get(id);
                // Re-add leading apostrophe if re-submitting `text` unquoted
                // would be inferred as a formula, boolean, error, date or number!
                let needs_quote = !text.is_empty()
                    && (text.starts_with('=')
                        || text.starts_with('\'')
                        || !matches!(
                            infer_input(text),
                            Ok(InferredInput::Scalar {
                                value: ScalarValue::Text(_),
                                ..
                            })
                        ));
                if needs_quote {
                    let mut quoted = String::with_capacity(text.len().saturating_add(1));
                    quoted.push('\'');
                    quoted.push_str(text);
                    quoted
                } else {
                    String::from(text)
                }
            }
        };

        Some(InspectedCell {
            address,
            raw,
            display,
            is_formula: false,
            error,
        })
    }

    /// Evaluates a formula string against the live workbook at `(sheet, anchor_pos)`
    /// without persisting any cell edits, dependency-graph mutations, spill updates,
    /// or `StateDigest` changes.
    ///
    /// If dirty formulas are pending, recalculates them first so precedents are clean.
    pub fn evaluate_transient_formula(
        &mut self,
        sheet: SheetId,
        anchor_pos: (u32, u32),
        formula_text: &str,
    ) -> Result<TransientEvalValue, TransientEvalError> {
        if self.has_pending_recalc() {
            let _ = self.recalc_all();
        }
        let (row_id, col_id) = {
            let s = self
                .workbook
                .live_sheet(sheet)
                .ok_or(TransientEvalError::SheetNotFound)?;
            let r = s
                .row_axis()
                .pos_to_id(anchor_pos.0)
                .ok_or(TransientEvalError::OutOfBounds)?;
            let c = s
                .col_axis()
                .pos_to_id(anchor_pos.1)
                .ok_or(TransientEvalError::OutOfBounds)?;
            (RowId(r), ColId(c))
        };

        let trimmed = formula_text.trim();
        let normalized = if trimmed.starts_with('=') {
            String::from(trimmed)
        } else {
            let mut s = String::with_capacity(trimmed.len().saturating_add(1));
            s.push('=');
            s.push_str(trimmed);
            s
        };

        let bound =
            BoundFormula::parse_and_bind(&normalized, FormulaMode::Dynamic, &self.workbook, sheet)
                .map_err(TransientEvalError::Parse)?;

        let saved_env = self.env.clone();
        let saved_name_stack = core::mem::take(&mut self.name_eval_stack);
        let caller = CallerContext {
            sheet,
            row: row_id,
            col: col_id,
            mode: FormulaMode::Dynamic,
        };
        let outcome = self.eval_root_formula(&bound.root, caller);
        self.env = saved_env;
        self.name_eval_stack = saved_name_stack;

        let value = match outcome {
            ExprOutcome::Ready(v) => match v {
                EvalValue::Scalar(s) => TransientEvalValue::Scalar(s),
                EvalValue::Array(arr) => TransientEvalValue::Array(arr),
                EvalValue::Missing => TransientEvalValue::Scalar(EvalScalar::Number(0.0)),
                EvalValue::Reference(r) => {
                    match materialise_reference(&r, &self.workbook, &self.spills) {
                        Ok(ScalarOrArray::Scalar(s)) => TransientEvalValue::Scalar(s),
                        Ok(ScalarOrArray::Array(arr)) => TransientEvalValue::Array(arr),
                        Err(e) => TransientEvalValue::Scalar(EvalScalar::Error(e)),
                    }
                }
            },
            ExprOutcome::Cycle(_) => {
                TransientEvalValue::Scalar(EvalScalar::Error(CalcError::Cycle))
            }
            ExprOutcome::Blocked(_) => {
                TransientEvalValue::Scalar(EvalScalar::Error(CalcError::Calc))
            }
        };
        Ok(value)
    }

    /// Lists cells currently displaying a calculation error in deterministic
    /// `(sheet tab order, row, col)` order, together with each formula's direct
    /// precedent cells (capped at 16 precedents per error cell).
    #[must_use]
    pub fn list_error_cells(
        &self,
        sheet_filter: Option<SheetId>,
        max_count: usize,
    ) -> Vec<ErrorCellTrace> {
        let mut results = Vec::new();
        if max_count == 0 {
            return results;
        }
        for sheet in self.workbook.live_sheets() {
            let sid = sheet.id();
            if sheet_filter.is_some_and(|want| want != sid) {
                continue;
            }
            let mut found: Vec<(u32, u32, RowId, ColId, String)> = Vec::new();
            sheet.for_each_non_empty_cell(|r_pos, c_pos, row_id, col_id, val| match val {
                Value::Error(e) => {
                    found.push((r_pos, c_pos, row_id, col_id, String::from(e.as_str())));
                }
                Value::Array(_) => found.push((
                    r_pos,
                    c_pos,
                    row_id,
                    col_id,
                    String::from(CalcError::Spill.as_str()),
                )),
                _ => {}
            });
            found.sort_by_key(|&(r_pos, c_pos, _, _, _)| (r_pos, c_pos));
            for (r_pos, c_pos, row_id, col_id, err_str) in found {
                if results.len() >= max_count {
                    return results;
                }
                let mut address = String::new();
                CellAddr {
                    col: c_pos,
                    row: r_pos,
                    abs_col: false,
                    abs_row: false,
                }
                .write_a1(&mut address);
                let bound = self.workbook.formula(sid, row_id, col_id);
                let formula = bound.map(|f| f.unparse(&self.workbook, sid));
                let mut precedents = Vec::new();
                if let Some(f) = bound {
                    let mut seen = BTreeSet::new();
                    collect_bound_precedents(
                        &self.workbook,
                        sid,
                        &f.root,
                        &mut seen,
                        &mut precedents,
                        16,
                    );
                }
                results.push(ErrorCellTrace {
                    sheet_id: sid,
                    sheet_name: String::from(sheet.name()),
                    row: row_id,
                    col: col_id,
                    address,
                    error: err_str,
                    formula,
                    precedents,
                });
            }
        }
        results
    }

    /// Parses raw user input under E30 (respecting [`NumberFormat::PlainText`]
    /// `@` on the target cell), binds any `'='` formula, applies the single-cell
    /// edit atomically, and schedules affected dependents.
    pub fn edit_cell_raw(
        &mut self,
        sheet_id: SheetId,
        row: RowId,
        col: ColId,
        raw: &str,
    ) -> Result<InverseOp<BoundFormula>, EditError> {
        let op = self.build_cell_edit(sheet_id, row, col, raw)?;
        let inverse = self.apply_op(op)?;
        self.invalidate_volatile_after_edit();
        Ok(inverse)
    }

    /// Binds a raw cell edit without applying it so another workbook can apply the same operation.
    pub fn build_cell_edit(
        &self,
        sheet_id: SheetId,
        row: RowId,
        col: ColId,
        raw: &str,
    ) -> Result<Op<BoundFormula>, EditError> {
        let (has_explicit_fmt, effective_fmt) =
            self.workbook
                .sheet(sheet_id)
                .map_or((false, NumberFormat::General), |s| {
                    (
                        s.cell_format_by_id(row, col).is_some(),
                        s.effective_format_by_id(row, col),
                    )
                });
        let inferred = infer_input_for_cell(raw, effective_fmt)?;

        let (input, format) = match inferred {
            InferredInput::Empty => (CellInput::Empty, FormatPatch::Keep),
            InferredInput::Scalar {
                value,
                suggested_format,
            } => {
                let patch = match suggested_format {
                    Some(fmt) if !has_explicit_fmt => FormatPatch::Set(fmt),
                    _ => FormatPatch::Keep,
                };
                (CellInput::Scalar(value), patch)
            }
            InferredInput::Formula(raw_f) => {
                let bound = BoundFormula::parse_and_bind(
                    &raw_f.source,
                    raw_f.mode,
                    &self.workbook,
                    sheet_id,
                )?;
                (
                    CellInput::Formula {
                        formula: bound,
                        cached: None,
                    },
                    FormatPatch::Keep,
                )
            }
        };

        Ok(Op::SetCells {
            sheet: sheet_id,
            edits: vec![CellEdit {
                row,
                col,
                input,
                format,
            }],
        })
    }

    /// Invalidates volatile formulas following direct cell input.
    pub fn invalidate_volatile_after_edit(&mut self) {
        if self.volatile_set.is_empty() && self.volatile_names.is_empty() {
            return;
        }
        let mut vol_roots: Vec<u32> = self.volatile_set.iter().copied().collect();
        self.collect_volatile_name_dependents(&mut vol_roots);
        self.propagate_dirty(vol_roots);
    }

    fn invalidate_filter_sensitive(&mut self) {
        if self.filter_sensitive_set.is_empty() && self.filter_sensitive_names.is_empty() {
            return;
        }
        let mut roots: Vec<u32> = self.filter_sensitive_set.iter().copied().collect();
        if !self.filter_sensitive_names.is_empty() {
            let nids: Vec<u32> = self.filter_sensitive_names.iter().copied().collect();
            self.collect_name_dependents_from_nids(&nids, &mut roots);
        }
        self.propagate_dirty(roots);
    }

    fn refresh_filter_projection_after_structure(&mut self, sheet_id: SheetId) {
        let Some((mut table, old_proj)) = self.filters.remove(&sheet_id) else {
            return;
        };
        let Some(sheet) = self.workbook.live_sheet(sheet_id) else {
            if !old_proj.is_empty() {
                self.invalidate_filter_sensitive();
            }
            return;
        };
        let Some(header_pos) = sheet.row_axis().id_to_pos(table.header_row.0) else {
            if !old_proj.is_empty() {
                self.invalidate_filter_sensitive();
            }
            return;
        };
        if sheet.row_axis().id_to_pos(table.last_row.0).is_none() {
            let fallback_row = sheet
                .used_extent()
                .map_or(header_pos, |(r, _)| r.max(header_pos));
            table.last_row = RowId(sheet.row_id(fallback_row.min(sheet.rows().saturating_sub(1))));
        }
        let first_pos = sheet.col_axis().id_to_pos(table.first_col.0);
        let last_pos = sheet.col_axis().id_to_pos(table.last_col.0);
        match (first_pos, last_pos) {
            (Some(_), Some(_)) => {}
            (Some(c), None) | (None, Some(c)) => {
                let cid = ColId(sheet.col_id(c));
                table.first_col = cid;
                table.last_col = cid;
            }
            (None, None) => {
                if !old_proj.is_empty() {
                    self.invalidate_filter_sensitive();
                }
                return;
            }
        }
        let new_proj = VisibleRowProjection::from_hidden_row_ids(sheet, old_proj.hidden_row_ids());
        let hidden_changed = old_proj.hidden_row_ids() != new_proj.hidden_row_ids();
        self.filters.insert(sheet_id, (table, new_proj));
        if hidden_changed {
            self.invalidate_filter_sensitive();
        }
    }

    /// Applies a multi-operation transaction atomically: if any operation fails,
    /// all earlier operations in `ops` are rolled back via [`Self::apply_inverse`]
    /// so the engine remains unchanged.
    pub fn apply_txn(
        &mut self,
        ops: Vec<Op<BoundFormula>>,
    ) -> Result<Vec<InverseOp<BoundFormula>>, OpError> {
        let mut inverses = Vec::with_capacity(ops.len());
        for op in ops {
            match self.apply_op(op) {
                Ok(inv) => inverses.push(inv),
                Err(err) => {
                    for inv in inverses.into_iter().rev() {
                        let _ = self.apply_inverse(inv);
                    }
                    return Err(err);
                }
            }
        }
        Ok(inverses)
    }

    /// Validates and applies `op` to the workbook, updating the dependency
    /// graph and queuing affected formulas for recalculation.
    pub fn apply_op(&mut self, op: Op<BoundFormula>) -> Result<InverseOp<BoundFormula>, OpError> {
        let inv = self.workbook.apply(op)?;
        self.abort_paused_call_stack();

        match &inv {
            InverseOp::RestoreCells { sheet, cells } => {
                let mut root_dirty: Vec<u32> = Vec::new();
                if cells.len() > 256 {
                    self.changes.structural = true;
                }
                for cell in cells {
                    let key = (*sheet, cell.row, cell.col);
                    if cells.len() <= 256 {
                        self.changes.cells.push(key);
                    }
                    self.remove_slot_by_key(key);
                    self.notify_cell_written_for_spills(key, &mut root_dirty);
                    if let Some(new_f) = self.workbook.formula(*sheet, cell.row, cell.col).cloned()
                    {
                        let fid = self.insert_slot(key, &new_f, false);
                        root_dirty.push(fid);
                    }
                    self.collect_direct_dependents(key, &mut root_dirty);
                }
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RestoreFilledRange {
                sheet, rows, cols, ..
            } => {
                self.on_filled_range_applied(*sheet, rows, cols);
            }
            InverseOp::RemoveInsertedRows { sheet, ids } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(*sheet);
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_rows_inserted(*sheet, *ids, &self.workbook, |fid| root_dirty.push(fid));
                self.collect_name_dep_structural_rows_inserted(*sheet, *ids, &mut root_dirty);
                self.refresh_sheet_spills_on_structural_change(*sheet, false, &mut root_dirty);
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RemoveInsertedCols { sheet, ids } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(*sheet);
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_cols_inserted(*sheet, *ids, &self.workbook, |fid| root_dirty.push(fid));
                self.collect_name_dep_structural_cols_inserted(*sheet, *ids, &mut root_dirty);
                self.refresh_sheet_spills_on_structural_change(*sheet, false, &mut root_dirty);
                self.propagate_dirty(root_dirty);
            }
            InverseOp::UndeleteRows { sheet, ids } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(*sheet);
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_rows_toggled(*sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
                self.collect_name_dep_structural_rows_toggled(*sheet, ids, &mut root_dirty);
                self.refresh_sheet_spills_on_structural_change(*sheet, false, &mut root_dirty);
                self.propagate_dirty(root_dirty);
            }
            InverseOp::UndeleteCols { sheet, ids } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(*sheet);
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_cols_toggled(*sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
                self.collect_name_dep_structural_cols_toggled(*sheet, ids, &mut root_dirty);
                self.refresh_sheet_spills_on_structural_change(*sheet, false, &mut root_dirty);
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RedeleteRows { sheet, ids } => {
                self.refresh_filter_projection_after_structure(*sheet);
                self.on_rows_undeleted(*sheet, ids);
            }
            InverseOp::RedeleteCols { sheet, ids } => {
                self.refresh_filter_projection_after_structure(*sheet);
                self.on_cols_undeleted(*sheet, ids);
            }
            InverseOp::RestoreMovedRows {
                sheet,
                rewritten,
                rewritten_names,
                ..
            }
            | InverseOp::RestoreMovedCols {
                sheet,
                rewritten,
                rewritten_names,
                ..
            } => {
                self.on_axis_reordered(*sheet, rewritten, rewritten_names);
            }
            InverseOp::RestoreRowHeights { .. }
            | InverseOp::RestoreColWidths { .. }
            | InverseOp::RestoreFreeze { .. }
            | InverseOp::RemoveAddedSheet { .. }
            | InverseOp::RestoreSheetName { .. }
            | InverseOp::RestoreSheetOrder { .. } => {
                self.changes.structural = true;
            }
            InverseOp::RestoreFormat { sheet, backup } => {
                if let FormatBackup::Cells(entries) = backup
                    && entries.len() <= 256
                {
                    for &(r, c, _, _) in entries {
                        self.changes.cells.push((*sheet, r, c));
                    }
                } else {
                    self.changes.structural = true;
                }
            }
            InverseOp::UndeleteSheet { sheet } | InverseOp::RedeleteSheet { sheet, .. } => {
                self.on_sheet_visibility_changed(*sheet);
            }
            InverseOp::RestorePermutedCells { sheet, prior } => {
                self.on_permuted_cells_changed(*sheet, prior);
            }
            InverseOp::RestoreMovedCells {
                prior_cells,
                rewritten_formulas,
                rewritten_names,
            } => {
                self.on_moved_cells_changed(prior_cells, rewritten_formulas, rewritten_names);
            }
            InverseOp::RestoreNamedRange { key, .. } => {
                self.on_named_range_changed(key);
            }
        }

        self.invalidate_volatile_after_edit();

        Ok(inv)
    }

    /// Applies an [`InverseOp`] and marks affected formulas dirty so undo/rollback
    /// restores both stored state and derived values.
    pub fn apply_inverse(&mut self, inv: InverseOp<BoundFormula>) -> Result<(), OpError> {
        // For `RemoveInsertedRows`/`Cols`, capture straddling ranges BEFORE
        // removing the run from the axis!
        let mut pre_dirty: Vec<u32> = Vec::new();
        let mut pre_nids: Vec<u32> = Vec::new();
        match &inv {
            InverseOp::RemoveInsertedRows { sheet, ids } => {
                self.dep_graph.on_rows_toggled(
                    *sheet,
                    core::slice::from_ref(ids),
                    &self.workbook,
                    |fid| pre_dirty.push(fid),
                );
                if !self.name_slots.is_empty() {
                    self.name_dep_graph.on_rows_toggled(
                        *sheet,
                        core::slice::from_ref(ids),
                        &self.workbook,
                        |nid| pre_nids.push(nid),
                    );
                }
            }
            InverseOp::RemoveInsertedCols { sheet, ids } => {
                self.dep_graph.on_cols_toggled(
                    *sheet,
                    core::slice::from_ref(ids),
                    &self.workbook,
                    |fid| pre_dirty.push(fid),
                );
                if !self.name_slots.is_empty() {
                    self.name_dep_graph.on_cols_toggled(
                        *sheet,
                        core::slice::from_ref(ids),
                        &self.workbook,
                        |nid| pre_nids.push(nid),
                    );
                }
            }
            _ => {}
        }

        self.workbook.apply_inverse(inv.clone())?;
        self.abort_paused_call_stack();

        match inv {
            InverseOp::RestoreCells { sheet, cells } => {
                let mut root_dirty = Vec::new();
                let small_batch = cells.len() <= 256;
                if !small_batch {
                    self.changes.structural = true;
                }
                for cell in cells {
                    let key = (sheet, cell.row, cell.col);
                    if small_batch {
                        self.changes.cells.push(key);
                    }
                    self.remove_slot_by_key(key);
                    self.notify_cell_written_for_spills(key, &mut root_dirty);
                    if let CellInput::Formula { formula, .. } = &cell.input {
                        let fid = self.insert_slot(key, formula, false);
                        root_dirty.push(fid);
                    }
                    self.collect_direct_dependents(key, &mut root_dirty);
                }
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RestoreFilledRange {
                sheet,
                rows,
                cols,
                prior_non_empty,
            } => {
                self.on_filled_range_restored(sheet, &rows, &cols, &prior_non_empty);
            }
            InverseOp::RemoveInsertedRows { sheet, .. }
            | InverseOp::RemoveInsertedCols { sheet, .. } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(sheet);
                self.collect_name_dependents_from_nids(&pre_nids, &mut pre_dirty);
                self.refresh_sheet_spills_on_structural_change(sheet, false, &mut pre_dirty);
                self.propagate_dirty(pre_dirty);
            }
            InverseOp::UndeleteRows { sheet, ids } => {
                self.refresh_filter_projection_after_structure(sheet);
                self.on_rows_undeleted(sheet, &ids);
            }
            InverseOp::UndeleteCols { sheet, ids } => {
                self.refresh_filter_projection_after_structure(sheet);
                self.on_cols_undeleted(sheet, &ids);
            }
            InverseOp::RedeleteRows { sheet, ids } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(sheet);
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_rows_toggled(sheet, &ids, &self.workbook, |fid| root_dirty.push(fid));
                self.collect_name_dep_structural_rows_toggled(sheet, &ids, &mut root_dirty);
                self.refresh_sheet_spills_on_structural_change(sheet, false, &mut root_dirty);
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RedeleteCols { sheet, ids } => {
                self.changes.structural = true;
                self.refresh_filter_projection_after_structure(sheet);
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_cols_toggled(sheet, &ids, &self.workbook, |fid| root_dirty.push(fid));
                self.collect_name_dep_structural_cols_toggled(sheet, &ids, &mut root_dirty);
                self.refresh_sheet_spills_on_structural_change(sheet, false, &mut root_dirty);
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RestoreMovedRows {
                sheet,
                rewritten,
                rewritten_names,
                ..
            }
            | InverseOp::RestoreMovedCols {
                sheet,
                rewritten,
                rewritten_names,
                ..
            } => {
                self.on_axis_reordered(sheet, &rewritten, &rewritten_names);
            }
            InverseOp::RemoveAddedSheet { sheet, .. } => {
                self.changes.structural = true;
                let to_remove: Vec<WorkbookCellKey> = (0..self.slots.len())
                    .filter_map(|idx| {
                        self.slots
                            .get(idx)
                            .and_then(Option::as_ref)
                            .and_then(|s| (s.key.0 == sheet).then_some(s.key))
                    })
                    .collect();
                for k in to_remove {
                    self.remove_slot_by_key(k);
                }
                self.on_sheet_visibility_changed(sheet);
            }
            InverseOp::RestoreRowHeights { .. }
            | InverseOp::RestoreColWidths { .. }
            | InverseOp::RestoreFreeze { .. }
            | InverseOp::RestoreSheetName { .. }
            | InverseOp::RestoreSheetOrder { .. } => {
                self.changes.structural = true;
            }
            InverseOp::RestoreFormat { sheet, backup } => {
                if let FormatBackup::Cells(entries) = &backup
                    && entries.len() <= 256
                {
                    for &(r, c, _, _) in entries {
                        self.changes.cells.push((sheet, r, c));
                    }
                } else {
                    self.changes.structural = true;
                }
            }
            InverseOp::UndeleteSheet { sheet } | InverseOp::RedeleteSheet { sheet, .. } => {
                self.on_sheet_visibility_changed(sheet);
            }
            InverseOp::RestorePermutedCells { sheet, prior } => {
                self.on_permuted_cells_changed(sheet, &prior);
            }
            InverseOp::RestoreMovedCells {
                prior_cells,
                rewritten_formulas,
                rewritten_names,
            } => {
                self.on_moved_cells_changed(&prior_cells, &rewritten_formulas, &rewritten_names);
            }
            InverseOp::RestoreNamedRange { key, .. } => {
                self.on_named_range_changed(&key);
            }
        }

        Ok(())
    }

    /// Evaluates up to `max_cells` formulas in this slice and returns the
    /// current [`RecalcStatus`].
    pub fn recalc_step(&mut self, max_cells: usize) -> RecalcStatus {
        let limit = max_cells.max(1);
        let mut evaluated_in_slice = 0usize;
        self.recalc_with_budget(1, |_| {
            evaluated_in_slice = evaluated_in_slice.saturating_add(1);
            evaluated_in_slice >= limit
        })
    }

    /// Evaluates dirty formulas until either all settle (`Complete`) or
    /// `should_yield(wave_done)` returns `true` (checked every `check_every`
    /// completed formulas).
    pub fn recalc_with_budget(
        &mut self,
        check_every: usize,
        mut should_yield: impl FnMut(u32) -> bool,
    ) -> RecalcStatus {
        self.prepare_dirty_queue();

        let stride = check_every.max(1);
        let mut since_check = 0usize;

        loop {
            if self.call_stack.is_empty() {
                if !self.scc_stack.is_empty() {
                    self.release_all_scc_wait_to_dirty();
                }
                if self.dirty_needs_sort {
                    self.prepare_dirty_queue();
                }
                // Advance `dirty_cursor` to the next still-DIRTY formula.
                let mut next_root: Option<u32> = None;
                while let Some(&fid) = self.dirty_queue.get(self.dirty_cursor) {
                    self.dirty_cursor = self.dirty_cursor.saturating_add(1);
                    if self.state_of(fid) == STATE_DIRTY && self.slot_of(fid).is_some() {
                        next_root = Some(fid);
                        break;
                    }
                }
                let Some(root_fid) = next_root else {
                    let done = self.wave_done;
                    self.dirty_queue.clear();
                    self.dirty_cursor = 0;
                    self.dirty_needs_sort = false;
                    self.wave_done = 0;
                    self.wave_spill_requeues.clear();
                    self.wave_cycle_verified.clear();
                    self.scc_stack.clear();
                    self.scc_low.clear();
                    self.wave_deferred_dirty.clear();
                    return RecalcStatus::Complete { recalculated: done };
                };
                self.call_stack.push(root_fid);
            }

            let Some(&cur_fid) = self.call_stack.last() else {
                continue;
            };
            if matches!(self.state_of(cur_fid), STATE_CLEAN | STATE_SCC_WAIT)
                || self.slot_of(cur_fid).is_none()
            {
                let _ = self.call_stack.pop();
                continue;
            }

            self.set_state(cur_fid, STATE_EVALUATING);
            let Some(key) = self.slot_of(cur_fid).map(|s| s.key) else {
                let _ = self.call_stack.pop();
                continue;
            };
            let Some(formula) = self.workbook.formula(key.0, key.1, key.2).cloned() else {
                self.set_state(cur_fid, STATE_CLEAN);
                let _ = self.call_stack.pop();
                continue;
            };

            let caller = CallerContext {
                sheet: key.0,
                row: key.1,
                col: key.2,
                mode: formula.mode,
            };

            match self.eval_root_formula(&formula.root, caller) {
                ExprOutcome::Ready(val) => {
                    let my_pos = self.call_stack.len().saturating_sub(1);
                    self.commit_evaluated_formula(cur_fid, key, caller, val);
                    if !self.scc_stack.is_empty() {
                        self.release_scc_wait_at_or_above(my_pos);
                    }
                    self.wave_done = self.wave_done.saturating_add(1);
                    since_check = since_check.saturating_add(1);
                    if since_check >= stride {
                        since_check = 0;
                        if self.has_pending_recalc() && should_yield(self.wave_done) {
                            return self.in_progress_status();
                        }
                    }
                }
                ExprOutcome::Blocked(deps) => {
                    let count = deps.len();
                    // Push dirty precedents in reverse so the first precedent is
                    // evaluated first on top of `call_stack`.
                    for &dep_fid in deps.iter().rev() {
                        if self.state_of(dep_fid) == STATE_DIRTY {
                            self.call_stack.push(dep_fid);
                        }
                    }
                    since_check = since_check.saturating_add(count.min(stride).max(1));
                    if since_check >= stride {
                        since_check = 0;
                        if self.has_pending_recalc() && should_yield(self.wave_done) {
                            return self.in_progress_status();
                        }
                    }
                }
                ExprOutcome::Cycle(min_pos) => {
                    self.handle_cycle(cur_fid, min_pos);
                }
            }
        }
    }

    /// Runs recalculation synchronously to completion and returns the number of
    /// formulas evaluated.
    pub fn recalc_all(&mut self) -> u32 {
        match self.recalc_with_budget(usize::MAX, |_| false) {
            RecalcStatus::Complete { recalculated } => recalculated,
            RecalcStatus::InProgress { done, .. } => done,
        }
    }

    fn in_progress_status(&self) -> RecalcStatus {
        let rem = self
            .dirty_queue
            .len()
            .saturating_sub(self.dirty_cursor)
            .saturating_add(self.call_stack.len())
            .saturating_add(self.scc_stack.len());
        let total = self
            .wave_done
            .saturating_add(u32::try_from(rem).unwrap_or(u32::MAX));
        RecalcStatus::InProgress {
            done: self.wave_done,
            total,
        }
    }

    // -------------------------------------------------------- internal helpers

    fn commit_evaluated_formula(
        &mut self,
        cur_fid: u32,
        key: WorkbookCellKey,
        caller: CallerContext,
        val: EvalValue,
    ) {
        if caller.mode == FormulaMode::Legacy {
            let final_scalar = finalize_top_level(val, &self.workbook, &self.spills, caller);
            self.finish_scalar_formula(cur_fid, key, final_scalar);
            return;
        }

        enum TopLevelResult {
            Scalar(EvalScalar),
            Array(Array2D),
        }

        let top = match val {
            EvalValue::Scalar(s) => TopLevelResult::Scalar(s),
            EvalValue::Missing => TopLevelResult::Scalar(EvalScalar::Number(0.0)),
            EvalValue::Array(arr) => TopLevelResult::Array(arr),
            EvalValue::Reference(r) => {
                match materialise_reference(&r, &self.workbook, &self.spills) {
                    Ok(ScalarOrArray::Scalar(s)) => {
                        TopLevelResult::Scalar(if s == EvalScalar::Empty {
                            EvalScalar::Number(0.0)
                        } else {
                            s
                        })
                    }
                    Ok(ScalarOrArray::Array(mut arr)) => {
                        for item in &mut arr.values {
                            if *item == EvalScalar::Empty {
                                *item = EvalScalar::Number(0.0);
                            }
                        }
                        TopLevelResult::Array(arr)
                    }
                    Err(e) => TopLevelResult::Scalar(EvalScalar::Error(e)),
                }
            }
        };

        match top {
            TopLevelResult::Scalar(s) => {
                self.finish_scalar_formula(cur_fid, key, s);
            }
            TopLevelResult::Array(arr) => {
                let old_bits = self
                    .workbook
                    .sheet(key.0)
                    .map(|s| s.value_by_id(key.1, key.2).pack());
                let old_shape = self.spills.spill_shape(key);
                let was_active = old_shape.is_some();
                let dep_view = DepView {
                    dep_graph: &self.dep_graph,
                    name_dep_graph: &self.name_dep_graph,
                    name_slots: &self.name_slots,
                    cell_to_fid: &self.cell_to_fid,
                    slots: &self.slots,
                    workbook: &self.workbook,
                };
                match self.spills.commit_array_with_order(
                    key,
                    arr,
                    dep_view.workbook,
                    |spills, a, a_fp, b| {
                        Self::classify_spill_collision_in(&dep_view, spills, a, a_fp, b)
                    },
                ) {
                    CommitSpillOutcome::Materialized {
                        anchor_scalar,
                        changed_shadows,
                        unblocked_watchers,
                        displaced_anchors,
                    } => {
                        let new_shape = self.spills.spill_shape(key);
                        let shape_changed = old_shape != new_shape;
                        self.store_eval_scalar(key, anchor_scalar);
                        let new_bits = self
                            .workbook
                            .sheet(key.0)
                            .map(|s| s.value_by_id(key.1, key.2).pack());
                        let anchor_changed = old_bits != new_bits;
                        self.record_changed_cells(&changed_shadows);
                        self.set_state(cur_fid, STATE_CLEAN);
                        let _ = self.call_stack.pop();

                        let mut post_dirty = Vec::new();
                        if anchor_changed {
                            self.collect_direct_dependents(key, &mut post_dirty);
                        }
                        if shape_changed || anchor_changed || !changed_shadows.is_empty() {
                            self.collect_spill_anchor_dependents(key, &mut post_dirty);
                        }
                        for displaced in displaced_anchors {
                            self.collect_direct_dependents(displaced, &mut post_dirty);
                        }
                        // Strip `cur_fid` for direct anchor / spill-anchor dependents before
                        // collecting changed-shadow dependents so self-shadow cycles
                        // (`A1 = =A2:A3`) re-evaluate once and report `#CYCLE!` consistently
                        // on both fresh and incremental recalculation.
                        post_dirty.retain(|&fid| fid != cur_fid);
                        for shadow_key in changed_shadows {
                            self.collect_direct_dependents(shadow_key, &mut post_dirty);
                            if self.spills.anchor_of_cell(shadow_key) == Some(key) {
                                self.wake_cycle_shadow_watchers(
                                    shadow_key,
                                    Some(cur_fid),
                                    Some(key),
                                    &mut post_dirty,
                                );
                            }
                        }
                        self.push_unblocked_watchers(&unblocked_watchers, cur_fid, &mut post_dirty);
                        self.drain_cycle_spill_anchor(key, Some(cur_fid), &mut post_dirty);
                        self.propagate_dirty_in_wave(post_dirty, &[]);
                    }
                    CommitSpillOutcome::Blocked {
                        revoked_anchors,
                        freed_shadows,
                        unblocked_watchers,
                    } => {
                        self.store_eval_scalar(key, EvalScalar::Error(CalcError::Spill));
                        let new_bits = self
                            .workbook
                            .sheet(key.0)
                            .map(|s| s.value_by_id(key.1, key.2).pack());
                        let anchor_changed = old_bits != new_bits;
                        self.set_state(cur_fid, STATE_CLEAN);
                        let _ = self.call_stack.pop();

                        let mut post_dirty = Vec::new();
                        if anchor_changed || was_active {
                            self.collect_direct_dependents(key, &mut post_dirty);
                        }
                        for revoked in revoked_anchors {
                            self.store_eval_scalar(revoked, EvalScalar::Error(CalcError::Spill));
                            self.collect_direct_dependents(revoked, &mut post_dirty);
                        }
                        self.record_changed_cells(&freed_shadows);
                        for shadow_key in freed_shadows {
                            self.collect_direct_dependents(shadow_key, &mut post_dirty);
                        }
                        self.push_unblocked_watchers(&unblocked_watchers, cur_fid, &mut post_dirty);
                        post_dirty.retain(|&fid| fid != cur_fid);
                        if !self.cycle_shadow_watch.is_empty()
                            && let Some(fp) = self
                                .spills
                                .dynamic_blocked_footprint(key)
                                .map(<[WorkbookCellKey]>::to_vec)
                        {
                            for cell in fp {
                                self.wake_cycle_shadow_watchers(
                                    cell,
                                    Some(cur_fid),
                                    Some(key),
                                    &mut post_dirty,
                                );
                            }
                        }
                        self.drain_cycle_spill_anchor(key, Some(cur_fid), &mut post_dirty);
                        self.propagate_dirty_in_wave(post_dirty, &[]);
                    }
                }
            }
        }
    }

    fn finish_scalar_formula(&mut self, cur_fid: u32, key: WorkbookCellKey, scalar: EvalScalar) {
        let clear_out = self.spills.clear_anchor(key);
        self.store_eval_scalar(key, scalar);
        self.set_state(cur_fid, STATE_CLEAN);
        let _ = self.call_stack.pop();
        let mut post_dirty = Vec::new();
        if clear_out.had_state {
            self.record_changed_cells(&clear_out.cleared_shadows);
            for shadow_key in clear_out.cleared_shadows {
                self.collect_direct_dependents(shadow_key, &mut post_dirty);
            }
            self.collect_spill_anchor_dependents(key, &mut post_dirty);
            self.push_unblocked_watchers(&clear_out.unblocked_watchers, cur_fid, &mut post_dirty);
            post_dirty.retain(|&fid| fid != cur_fid);
        }
        self.drain_cycle_spill_anchor(key, Some(cur_fid), &mut post_dirty);
        if !post_dirty.is_empty() {
            self.propagate_dirty_in_wave(post_dirty, &[]);
        }
    }

    fn record_cycle_spill(
        &mut self,
        anchor: WorkbookCellKey,
        unwound_keys: &[WorkbookCellKey],
        cleared_shadows: &[WorkbookCellKey],
    ) {
        if let Some(prev) = self.cycle_spills.get(&anchor) {
            for cell in &prev.cleared_shadows {
                if let Some(anchors) = self.cycle_shadow_watch.get_mut(cell) {
                    anchors.retain(|&a| a != anchor);
                    if anchors.is_empty() {
                        self.cycle_shadow_watch.remove(cell);
                    }
                }
            }
        }
        let mut peers = self
            .cycle_spills
            .remove(&anchor)
            .map(|r| r.peers)
            .unwrap_or_default();
        for &peer in unwound_keys {
            if peer != anchor && !peers.contains(&peer) {
                peers.push(peer);
            }
        }
        let mut dedup_shadows = Vec::with_capacity(cleared_shadows.len());
        for &cell in cleared_shadows {
            if !dedup_shadows.contains(&cell) {
                dedup_shadows.push(cell);
            }
        }
        if peers.is_empty() && dedup_shadows.is_empty() {
            return;
        }
        for &cell in &dedup_shadows {
            let entry = self.cycle_shadow_watch.entry(cell).or_default();
            if !entry.contains(&anchor) {
                entry.push(anchor);
            }
        }
        self.cycle_spills.insert(
            anchor,
            CycleSpillRecord {
                peers,
                cleared_shadows: dedup_shadows,
            },
        );
    }

    fn drain_cycle_spill_anchor(
        &mut self,
        anchor: WorkbookCellKey,
        exclude_fid: Option<u32>,
        out: &mut Vec<u32>,
    ) {
        let Some(rec) = self.cycle_spills.remove(&anchor) else {
            return;
        };
        for cell in rec.cleared_shadows {
            if let Some(anchors) = self.cycle_shadow_watch.get_mut(&cell) {
                anchors.retain(|&a| a != anchor);
                if anchors.is_empty() {
                    self.cycle_shadow_watch.remove(&cell);
                }
            }
        }
        for peer in rec.peers {
            if let Some(&pfid) = self.cell_to_fid.get(&peer)
                && Some(pfid) != exclude_fid
                && self.state_of(pfid) == STATE_CLEAN
            {
                out.push(pfid);
            }
        }
    }

    fn wake_cycle_shadow_watchers(
        &mut self,
        cell: WorkbookCellKey,
        exclude_fid: Option<u32>,
        claimer: Option<WorkbookCellKey>,
        out: &mut Vec<u32>,
    ) {
        let mut matched: SmallVec<[(u32, bool); 4]> = SmallVec::new();
        {
            let Some(anchors) = self.cycle_shadow_watch.get(&cell) else {
                return;
            };
            let dep_view = DepView {
                dep_graph: &self.dep_graph,
                name_dep_graph: &self.name_dep_graph,
                name_slots: &self.name_slots,
                cell_to_fid: &self.cell_to_fid,
                slots: &self.slots,
                workbook: &self.workbook,
            };
            for &anchor in anchors {
                let Some(&afid) = self.cell_to_fid.get(&anchor) else {
                    continue;
                };
                if Some(afid) == exclude_fid {
                    continue;
                }
                if let Some(claimer_key) = claimer {
                    let cleared = self
                        .cycle_spills
                        .get(&anchor)
                        .map_or(&[][..], |r| r.cleared_shadows.as_slice());
                    let anchor_reaches_claimer = Self::formula_reaches_in(
                        &dep_view,
                        &self.spills,
                        anchor,
                        claimer_key,
                        anchor,
                        cleared,
                    );
                    let claimer_reaches_anchor = Self::formula_reaches_in(
                        &dep_view,
                        &self.spills,
                        claimer_key,
                        anchor,
                        anchor,
                        cleared,
                    );
                    if anchor_reaches_claimer && !claimer_reaches_anchor {
                        continue;
                    }
                }
                matched.push((afid, self.state_of(afid) == STATE_CLEAN));
            }
        }
        for (afid, is_clean) in matched {
            self.wave_cycle_verified.remove(&afid);
            if is_clean {
                out.push(afid);
            }
        }
    }

    fn push_unblocked_watchers(
        &self,
        watchers: &[WorkbookCellKey],
        self_fid: u32,
        out: &mut Vec<u32>,
    ) {
        for w in watchers {
            if let Some(&fid) = self.cell_to_fid.get(w)
                && fid != self_fid
            {
                out.push(fid);
            }
        }
    }

    fn record_changed_cells(&mut self, cells: &[WorkbookCellKey]) {
        if cells.is_empty() || self.changes.structural {
            return;
        }
        if self.changes.cells.len().saturating_add(cells.len()) > 256 {
            self.changes.structural = true;
            return;
        }
        self.changes.cells.extend_from_slice(cells);
    }

    fn notify_cell_written_for_spills(&mut self, key: WorkbookCellKey, root_dirty: &mut Vec<u32>) {
        if self.spills.is_empty() && self.cycle_spills.is_empty() {
            return;
        }
        self.drain_cycle_spill_anchor(key, None, root_dirty);
        self.wake_cycle_shadow_watchers(key, None, None, root_dirty);
        let clear_out = self.spills.clear_anchor(key);
        if clear_out.had_state {
            self.record_changed_cells(&clear_out.cleared_shadows);
            for shadow_key in clear_out.cleared_shadows {
                self.collect_direct_dependents(shadow_key, root_dirty);
            }
            self.collect_spill_anchor_dependents(key, root_dirty);
            for w in clear_out.unblocked_watchers {
                if let Some(&wfid) = self.cell_to_fid.get(&w)
                    && self.state_of(wfid) == STATE_CLEAN
                {
                    root_dirty.push(wfid);
                }
            }
        }
        if let Some(owner) = self.spills.anchor_of_cell(key)
            && let Some(&ofid) = self.cell_to_fid.get(&owner)
            && self.state_of(ofid) == STATE_CLEAN
        {
            root_dirty.push(ofid);
        }
        for w in self.spills.watchers_of_cell(key) {
            if let Some(&wfid) = self.cell_to_fid.get(&w)
                && self.state_of(wfid) == STATE_CLEAN
            {
                root_dirty.push(wfid);
            }
        }
    }

    fn refresh_sheet_spills_on_structural_change(
        &mut self,
        sheet_id: SheetId,
        sheet_removed_or_deleted: bool,
        out: &mut Vec<u32>,
    ) {
        if self.spills.is_empty() && self.cycle_spills.is_empty() {
            return;
        }
        let anchors = self.spills.anchors_on_sheet(sheet_id);
        for anchor in anchors {
            let is_live = !sheet_removed_or_deleted
                && self.workbook.live_sheet(sheet_id).is_some_and(|s| {
                    s.row_axis().id_to_pos(anchor.1.0).is_some()
                        && s.col_axis().id_to_pos(anchor.2.0).is_some()
                });
            let clear_out = self.spills.clear_anchor(anchor);
            if clear_out.had_state {
                self.record_changed_cells(&clear_out.cleared_shadows);
                for shadow_key in clear_out.cleared_shadows {
                    self.collect_direct_dependents(shadow_key, out);
                }
                self.collect_spill_anchor_dependents(anchor, out);
                for w in clear_out.unblocked_watchers {
                    if let Some(&wfid) = self.cell_to_fid.get(&w)
                        && self.state_of(wfid) == STATE_CLEAN
                    {
                        out.push(wfid);
                    }
                }
            }
            if is_live
                && let Some(&fid) = self.cell_to_fid.get(&anchor)
                && self.state_of(fid) == STATE_CLEAN
            {
                out.push(fid);
            }
        }
        if !self.cycle_spills.is_empty() {
            let lo = (sheet_id, RowId(0), ColId(0));
            let hi = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
            let peer_anchors: Vec<WorkbookCellKey> =
                self.cycle_spills.range(lo..=hi).map(|(k, _)| *k).collect();
            for anchor in peer_anchors {
                let is_live = !sheet_removed_or_deleted
                    && self.workbook.live_sheet(sheet_id).is_some_and(|s| {
                        s.row_axis().id_to_pos(anchor.1.0).is_some()
                            && s.col_axis().id_to_pos(anchor.2.0).is_some()
                    });
                self.drain_cycle_spill_anchor(anchor, None, out);
                if is_live
                    && let Some(&fid) = self.cell_to_fid.get(&anchor)
                    && self.state_of(fid) == STATE_CLEAN
                {
                    out.push(fid);
                }
            }
        }
    }

    fn on_filled_range_applied(&mut self, sheet: SheetId, rows: &[IdRun], cols: &[IdRun]) {
        let total_r: usize = rows
            .iter()
            .fold(0usize, |acc, r| acc.saturating_add(r.len as usize));
        let total_c: usize = cols
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(c.len as usize));
        let total_cells = total_r.saturating_mul(total_c);
        if total_cells > 256 {
            self.changes.structural = true;
        }
        if total_cells > 64 {
            let extra = total_cells.saturating_sub(self.free_slots.len());
            self.slots.reserve(extra);
            self.states.reserve(extra);
            self.cell_to_fid.reserve(total_cells);
            self.dep_graph.reserve_for_formulas(total_cells);
        }

        let check_spills = !self.spills.is_empty() || !self.cycle_spills.is_empty();
        let mut root_dirty = Vec::with_capacity(total_cells.saturating_add(8));
        for c_run in cols {
            for raw_c in c_run.iter() {
                let col = ColId(raw_c);
                for r_run in rows {
                    for raw_r in r_run.iter() {
                        let row = RowId(raw_r);
                        let key = (sheet, row, col);
                        if total_cells <= 256 {
                            self.changes.cells.push(key);
                        }
                        if self.cell_to_fid.contains_key(&key) {
                            self.remove_slot_by_key(key);
                        }
                        if check_spills {
                            self.notify_cell_written_for_spills(key, &mut root_dirty);
                        }
                        if let Some(new_f) = self.workbook.formula(sheet, row, col) {
                            let fid = if let Some(reused) = self.free_slots.pop() {
                                reused
                            } else {
                                let fresh = u32::try_from(self.slots.len()).unwrap_or(0);
                                self.slots.push(None);
                                self.states.push(STATE_CLEAN);
                                fresh
                            };
                            let (reg, flags) = self.dep_graph.register_detailed(fid, key.0, new_f);
                            if flags.volatile {
                                self.volatile_set.insert(fid);
                            }
                            if flags.filter_sensitive {
                                self.filter_sensitive_set.insert(fid);
                            }
                            if let Some(slot_ref) = self.slots.get_mut(fid as usize) {
                                *slot_ref = Some(FormulaSlot { key, reg });
                            }
                            self.cell_to_fid.insert(key, fid);
                            root_dirty.push(fid);
                        }
                    }
                }
            }
        }
        self.dep_graph
            .on_rect_written(sheet, rows, cols, &self.workbook, |fid| {
                if self.state_of(fid) == STATE_CLEAN {
                    root_dirty.push(fid);
                }
            });
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .on_rect_written(sheet, rows, cols, &self.workbook, |nid| seed_nids.push(nid));
            self.collect_name_dependents_from_nids(&seed_nids, &mut root_dirty);
        }
        self.propagate_dirty(root_dirty);
    }

    fn on_filled_range_restored(
        &mut self,
        sheet: SheetId,
        rows: &[IdRun],
        cols: &[IdRun],
        prior_non_empty: &[CellSnapshot<BoundFormula>],
    ) {
        let total_r: usize = rows
            .iter()
            .fold(0usize, |acc, r| acc.saturating_add(r.len as usize));
        let total_c: usize = cols
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(c.len as usize));
        let total_cells = total_r.saturating_mul(total_c);
        if total_cells > 256 {
            self.changes.structural = true;
        }
        let check_spills = !self.spills.is_empty() || !self.cycle_spills.is_empty();
        let mut root_dirty = Vec::new();
        for c_run in cols {
            for raw_c in c_run.iter() {
                let col = ColId(raw_c);
                for r_run in rows {
                    for raw_r in r_run.iter() {
                        let key = (sheet, RowId(raw_r), col);
                        if total_cells <= 256 {
                            self.changes.cells.push(key);
                        }
                        self.remove_slot_by_key(key);
                        if check_spills {
                            self.notify_cell_written_for_spills(key, &mut root_dirty);
                        }
                    }
                }
            }
        }
        for cell in prior_non_empty {
            if let CellInput::Formula { formula, .. } = &cell.input {
                let key = (sheet, cell.row, cell.col);
                let fid = self.insert_slot(key, formula, false);
                root_dirty.push(fid);
            }
        }
        self.dep_graph
            .on_rect_written(sheet, rows, cols, &self.workbook, |fid| {
                if self.state_of(fid) == STATE_CLEAN {
                    root_dirty.push(fid);
                }
            });
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .on_rect_written(sheet, rows, cols, &self.workbook, |nid| seed_nids.push(nid));
            self.collect_name_dependents_from_nids(&seed_nids, &mut root_dirty);
        }
        self.propagate_dirty(root_dirty);
    }

    fn on_rows_undeleted(&mut self, sheet: SheetId, ids: &[IdRun]) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        self.dep_graph
            .on_rows_toggled(sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
        self.collect_name_dep_structural_rows_toggled(sheet, ids, &mut root_dirty);
        for run in ids {
            for raw_r in run.iter() {
                let start = (sheet, RowId(raw_r), ColId(0));
                let end = (sheet, RowId(raw_r), ColId(u64::MAX));
                for (&k, _) in self.workbook.formulas().range(start..=end) {
                    if let Some(&fid) = self.cell_to_fid.get(&k) {
                        root_dirty.push(fid);
                    }
                }
            }
        }
        self.refresh_sheet_spills_on_structural_change(sheet, false, &mut root_dirty);
        self.propagate_dirty(root_dirty);
    }

    fn on_cols_undeleted(&mut self, sheet: SheetId, ids: &[IdRun]) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        self.dep_graph
            .on_cols_toggled(sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
        self.collect_name_dep_structural_cols_toggled(sheet, ids, &mut root_dirty);
        let start = (sheet, RowId(0), ColId(0));
        let end = (sheet, RowId(u64::MAX), ColId(u64::MAX));
        for (&k @ (_, _, col), _) in self.workbook.formulas().range(start..=end) {
            if ids.iter().any(|r| r.contains(col.0))
                && let Some(&fid) = self.cell_to_fid.get(&k)
            {
                root_dirty.push(fid);
            }
        }
        self.refresh_sheet_spills_on_structural_change(sheet, false, &mut root_dirty);
        self.propagate_dirty(root_dirty);
    }

    fn on_axis_reordered(
        &mut self,
        sheet: SheetId,
        rewritten: &[(WorkbookCellKey, BoundFormula)],
        rewritten_names: &[(NamedRangeKey, BoundFormula)],
    ) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        for &(key, _) in rewritten {
            self.remove_slot_by_key(key);
            if let Some(updated_f) = self.workbook.formula(key.0, key.1, key.2).cloned() {
                let fid = self.insert_slot(key, &updated_f, false);
                root_dirty.push(fid);
            }
        }
        let mut changed_names = Vec::new();
        for (nkey, _) in rewritten_names {
            self.sync_named_range_slot(nkey);
            changed_names.push(nkey.canonical_name.clone());
        }
        self.dep_graph
            .on_sheet_reordered(sheet, |fid| root_dirty.push(fid));
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .on_sheet_reordered(sheet, |nid| seed_nids.push(nid));
            self.collect_name_dependents_from_nids(&seed_nids, &mut root_dirty);
        }
        self.collect_name_dependents_for_names(&changed_names, &mut root_dirty);
        self.refresh_sheet_spills_on_structural_change(sheet, false, &mut root_dirty);
        self.propagate_dirty(root_dirty);
        self.refresh_filter_projection_after_structure(sheet);
    }

    fn on_sheet_visibility_changed(&mut self, sheet: SheetId) {
        self.changes.structural = true;
        let is_deleted = self.workbook.live_sheet(sheet).is_none();
        if is_deleted {
            self.clear_filter(sheet);
        }
        let mut root_dirty = Vec::new();
        for idx in 0..self.slots.len() {
            if let Some(slot) = self.slots.get(idx).and_then(Option::as_ref)
                && (slot.key.0 == sheet || slot.reg.references_sheet(sheet))
                && let Ok(fid) = u32::try_from(idx)
            {
                root_dirty.push(fid);
            }
        }
        if !self.name_slots.is_empty() {
            let mut affected_names = Vec::new();
            for entry in self.name_slots.iter().flatten() {
                if entry.0.scope == Some(sheet) || entry.1.references_sheet(sheet) {
                    affected_names.push(entry.0.canonical_name.clone());
                }
            }
            self.collect_name_dependents_for_names(&affected_names, &mut root_dirty);
        }
        self.refresh_sheet_spills_on_structural_change(sheet, is_deleted, &mut root_dirty);
        self.propagate_dirty(root_dirty);
    }

    fn on_permuted_cells_changed(
        &mut self,
        sheet: SheetId,
        prior: &[PermutedCellBackup<BoundFormula>],
    ) {
        if prior.len() > 256 {
            self.changes.structural = true;
        }
        let mut root_dirty = Vec::new();
        for backup in prior {
            let key = (sheet, backup.row, backup.col);
            if prior.len() <= 256 {
                self.changes.cells.push(key);
            }
            self.remove_slot_by_key(key);
            self.notify_cell_written_for_spills(key, &mut root_dirty);
            if let Some(new_f) = self
                .workbook
                .formula(sheet, backup.row, backup.col)
                .cloned()
            {
                let fid = self.insert_slot(key, &new_f, false);
                root_dirty.push(fid);
            }
            self.collect_direct_dependents(key, &mut root_dirty);
        }
        self.propagate_dirty(root_dirty);
        self.reapply_filter(sheet);
    }

    fn on_moved_cells_changed(
        &mut self,
        prior_cells: &[(SheetId, PermutedCellBackup<BoundFormula>)],
        rewritten_formulas: &[(WorkbookCellKey, BoundFormula)],
        rewritten_names: &[(NamedRangeKey, BoundFormula)],
    ) {
        let mut touched = BTreeSet::new();
        for (sheet, backup) in prior_cells {
            touched.insert((*sheet, backup.row, backup.col));
        }
        for &(key, _) in rewritten_formulas {
            touched.insert(key);
        }
        if touched.len() > 256 {
            self.changes.structural = true;
        }
        let mut root_dirty = Vec::new();
        for key in touched {
            if !self.changes.structural {
                self.changes.cells.push(key);
            }
            self.remove_slot_by_key(key);
            self.notify_cell_written_for_spills(key, &mut root_dirty);
            if let Some(new_f) = self.workbook.formula(key.0, key.1, key.2).cloned() {
                let fid = self.insert_slot(key, &new_f, false);
                root_dirty.push(fid);
            }
            self.collect_direct_dependents(key, &mut root_dirty);
        }
        let mut changed_names = Vec::new();
        for (nkey, _) in rewritten_names {
            self.sync_named_range_slot(nkey);
            changed_names.push(nkey.canonical_name.clone());
        }
        self.collect_name_dependents_for_names(&changed_names, &mut root_dirty);
        self.propagate_dirty(root_dirty);
    }

    fn on_named_range_changed(&mut self, key: &NamedRangeKey) {
        self.sync_named_range_slot(key);
        let mut root_dirty = Vec::new();
        self.collect_name_dependents_for_names(
            core::slice::from_ref(&key.canonical_name),
            &mut root_dirty,
        );
        self.propagate_dirty(root_dirty);
    }

    fn sync_named_range_slot(&mut self, key: &NamedRangeKey) {
        if let Some(old_nid) = self.name_to_nid.remove(key) {
            self.volatile_names.remove(&old_nid);
            self.filter_sensitive_names.remove(&old_nid);
            if let Some(slot_opt) = self.name_slots.get_mut(old_nid as usize)
                && let Some((_, old_reg)) = slot_opt.take()
            {
                self.name_dep_graph.unregister(old_nid, &old_reg);
                self.free_name_slots.push(old_nid);
            }
        }
        let Some(def) = self.workbook.named_ranges().get(key) else {
            return;
        };
        let nid = if let Some(reused) = self.free_name_slots.pop() {
            reused
        } else {
            let fresh = u32::try_from(self.name_slots.len()).unwrap_or(0);
            self.name_slots.push(None);
            fresh
        };
        let host_sheet = key
            .scope
            .unwrap_or_else(|| workbook_fallback_sheet(&self.workbook));
        let (reg, flags) = self
            .name_dep_graph
            .register_detailed(nid, host_sheet, &def.formula);
        if flags.volatile {
            self.volatile_names.insert(nid);
        }
        if flags.filter_sensitive {
            self.filter_sensitive_names.insert(nid);
        }
        if let Some(slot_ref) = self.name_slots.get_mut(nid as usize) {
            *slot_ref = Some((key.clone(), reg));
        }
        self.name_to_nid.insert(key.clone(), nid);
    }

    fn collect_volatile_name_dependents(&self, out: &mut Vec<u32>) {
        if self.volatile_names.is_empty() {
            return;
        }
        let seed_nids: Vec<u32> = self.volatile_names.iter().copied().collect();
        self.collect_name_dependents_from_nids(&seed_nids, out);
    }

    fn collect_name_dep_structural_rows_inserted(
        &self,
        sheet: SheetId,
        ids: IdRun,
        out: &mut Vec<u32>,
    ) {
        if self.name_slots.is_empty() {
            return;
        }
        let mut seed_nids = Vec::new();
        self.name_dep_graph
            .on_rows_inserted(sheet, ids, &self.workbook, |nid| seed_nids.push(nid));
        self.collect_name_dependents_from_nids(&seed_nids, out);
    }

    fn collect_name_dep_structural_cols_inserted(
        &self,
        sheet: SheetId,
        ids: IdRun,
        out: &mut Vec<u32>,
    ) {
        if self.name_slots.is_empty() {
            return;
        }
        let mut seed_nids = Vec::new();
        self.name_dep_graph
            .on_cols_inserted(sheet, ids, &self.workbook, |nid| seed_nids.push(nid));
        self.collect_name_dependents_from_nids(&seed_nids, out);
    }

    fn collect_name_dep_structural_rows_toggled(
        &self,
        sheet: SheetId,
        ids: &[IdRun],
        out: &mut Vec<u32>,
    ) {
        if self.name_slots.is_empty() {
            return;
        }
        let mut seed_nids = Vec::new();
        self.name_dep_graph
            .on_rows_toggled(sheet, ids, &self.workbook, |nid| seed_nids.push(nid));
        self.collect_name_dependents_from_nids(&seed_nids, out);
    }

    fn collect_name_dep_structural_cols_toggled(
        &self,
        sheet: SheetId,
        ids: &[IdRun],
        out: &mut Vec<u32>,
    ) {
        if self.name_slots.is_empty() {
            return;
        }
        let mut seed_nids = Vec::new();
        self.name_dep_graph
            .on_cols_toggled(sheet, ids, &self.workbook, |nid| seed_nids.push(nid));
        self.collect_name_dependents_from_nids(&seed_nids, out);
    }

    fn collect_name_dependents_from_nids(&self, seed_nids: &[u32], out: &mut Vec<u32>) {
        Self::collect_name_dependents_from_nids_with(
            &self.dep_graph,
            &self.name_dep_graph,
            &self.name_slots,
            seed_nids,
            |fid| {
                if self.state_of(fid) == STATE_CLEAN {
                    out.push(fid);
                }
            },
        );
    }

    fn collect_name_dependents_from_nids_with(
        dep_graph: &DepGraph,
        name_dep_graph: &DepGraph,
        name_slots: &[Option<(NamedRangeKey, FormulaRegistration)>],
        seed_nids: &[u32],
        emit: impl FnMut(u32),
    ) {
        if seed_nids.is_empty() {
            return;
        }
        let mut names = Vec::with_capacity(seed_nids.len());
        for &nid in seed_nids {
            if let Some((nkey, _)) = name_slots.get(nid as usize).and_then(Option::as_ref) {
                names.push(nkey.canonical_name.clone());
            }
        }
        Self::collect_name_dependents_for_names_with(
            dep_graph,
            name_dep_graph,
            name_slots,
            &names,
            emit,
        );
    }

    fn collect_name_dependents_for_names(&self, seed_names: &[String], out: &mut Vec<u32>) {
        Self::collect_name_dependents_for_names_with(
            &self.dep_graph,
            &self.name_dep_graph,
            &self.name_slots,
            seed_names,
            |fid| {
                if self.state_of(fid) == STATE_CLEAN {
                    out.push(fid);
                }
            },
        );
    }

    fn collect_name_dependents_for_names_with(
        dep_graph: &DepGraph,
        name_dep_graph: &DepGraph,
        name_slots: &[Option<(NamedRangeKey, FormulaRegistration)>],
        seed_names: &[String],
        mut emit: impl FnMut(u32),
    ) {
        if seed_names.is_empty() {
            return;
        }
        let mut seen_names: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<String> = Vec::with_capacity(seed_names.len());
        for s in seed_names {
            let canon = s.to_ascii_lowercase();
            if seen_names.insert(canon.clone()) {
                queue.push(canon);
            }
        }
        let mut head = 0usize;
        while let Some(canon) = queue.get(head).cloned() {
            head = head.saturating_add(1);
            dep_graph.for_each_name_dependent(&canon, &mut emit);
            name_dep_graph.for_each_name_dependent(&canon, |nid| {
                if let Some((dep_key, _)) = name_slots.get(nid as usize).and_then(Option::as_ref)
                    && seen_names.insert(dep_key.canonical_name.clone())
                {
                    queue.push(dep_key.canonical_name.clone());
                }
            });
        }
    }

    fn collect_spill_anchor_dependents(&self, anchor: WorkbookCellKey, out: &mut Vec<u32>) {
        self.dep_graph.for_each_spill_dependent(anchor, |fid| {
            if self.state_of(fid) == STATE_CLEAN {
                out.push(fid);
            }
        });
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .for_each_spill_dependent(anchor, |nid| seed_nids.push(nid));
            self.collect_name_dependents_from_nids(&seed_nids, out);
        }
    }

    fn slot_of(&self, fid: u32) -> Option<&FormulaSlot> {
        self.slots.get(fid as usize).and_then(Option::as_ref)
    }

    fn state_of(&self, fid: u32) -> u8 {
        self.states
            .get(fid as usize)
            .copied()
            .unwrap_or(STATE_CLEAN)
    }

    fn set_state(&mut self, fid: u32, new_state: u8) {
        let old = self.state_of(fid);
        if old == new_state {
            return;
        }
        if let Some(slot_state) = self.states.get_mut(fid as usize) {
            *slot_state = new_state;
        }
        if let Some(slot) = self.slots.get(fid as usize).and_then(Option::as_ref) {
            let col_key = (slot.key.0, slot.key.2);
            let was_clean = old == STATE_CLEAN;
            let now_clean = new_state == STATE_CLEAN;
            if was_clean && !now_clean {
                let entry = self.non_clean_in_col.entry(col_key).or_insert(0);
                *entry = entry.saturating_add(1);
            } else if !was_clean
                && now_clean
                && let Some(entry) = self.non_clean_in_col.get_mut(&col_key)
            {
                *entry = entry.saturating_sub(1);
                if *entry == 0 {
                    self.non_clean_in_col.remove(&col_key);
                }
            }
        }
    }

    fn insert_slot(&mut self, key: WorkbookCellKey, formula: &BoundFormula, dirty: bool) -> u32 {
        let fid = if let Some(reused) = self.free_slots.pop() {
            reused
        } else {
            let fresh = u32::try_from(self.slots.len()).unwrap_or(0);
            self.slots.push(None);
            self.states.push(STATE_CLEAN);
            fresh
        };
        let (reg, flags) = self.dep_graph.register_detailed(fid, key.0, formula);
        if flags.volatile {
            self.volatile_set.insert(fid);
        }
        if flags.filter_sensitive {
            self.filter_sensitive_set.insert(fid);
        }
        if let Some(slot_ref) = self.slots.get_mut(fid as usize) {
            *slot_ref = Some(FormulaSlot { key, reg });
        }
        self.cell_to_fid.insert(key, fid);
        if dirty {
            self.mark_fid_dirty_only(fid);
        }
        fid
    }

    fn remove_slot_by_key(&mut self, key: WorkbookCellKey) {
        let Some(fid) = self.cell_to_fid.remove(&key) else {
            return;
        };
        self.volatile_set.remove(&fid);
        self.filter_sensitive_set.remove(&fid);
        self.set_state(fid, STATE_CLEAN);
        if let Some(slot_opt) = self.slots.get_mut(fid as usize)
            && let Some(old_slot) = slot_opt.take()
        {
            self.dep_graph.unregister(fid, &old_slot.reg);
            self.free_slots.push(fid);
        }
    }

    fn mark_fid_dirty_only(&mut self, fid: u32) {
        if self.state_of(fid) == STATE_CLEAN && self.slot_of(fid).is_some() {
            self.set_state(fid, STATE_DIRTY);
            self.dirty_queue.push(fid);
            self.dirty_needs_sort = true;
        }
    }

    fn collect_direct_dependents(&self, key: WorkbookCellKey, out: &mut Vec<u32>) {
        self.dep_graph
            .for_each_cell_dependent(key, &self.workbook, |fid| {
                if self.state_of(fid) == STATE_CLEAN {
                    out.push(fid);
                }
            });
        self.collect_spill_anchor_dependents(key, out);
        if !self.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            self.name_dep_graph
                .for_each_cell_dependent(key, &self.workbook, |nid| seed_nids.push(nid));
            self.collect_name_dependents_from_nids(&seed_nids, out);
        }
    }

    fn collect_cell_dependents_unfiltered(
        view: &DepView<'_>,
        key: WorkbookCellKey,
        include_spill_anchor: bool,
        out: &mut Vec<u32>,
    ) {
        view.dep_graph
            .for_each_cell_dependent(key, view.workbook, |fid| out.push(fid));
        if include_spill_anchor {
            view.dep_graph
                .for_each_spill_dependent(key, |fid| out.push(fid));
        }
        if !view.name_slots.is_empty() {
            let mut seed_nids = Vec::new();
            view.name_dep_graph
                .for_each_cell_dependent(key, view.workbook, |nid| seed_nids.push(nid));
            if include_spill_anchor {
                view.name_dep_graph
                    .for_each_spill_dependent(key, |nid| seed_nids.push(nid));
            }
            Self::collect_name_dependents_from_nids_with(
                view.dep_graph,
                view.name_dep_graph,
                view.name_slots,
                &seed_nids,
                |fid| out.push(fid),
            );
        }
    }

    fn formula_reaches_in(
        view: &DepView<'_>,
        spills: &SpillRegistry,
        start: WorkbookCellKey,
        target: WorkbookCellKey,
        override_anchor: WorkbookCellKey,
        override_footprint: &[WorkbookCellKey],
    ) -> bool {
        let Some(&start_fid) = view.cell_to_fid.get(&start) else {
            return false;
        };
        let Some(&target_fid) = view.cell_to_fid.get(&target) else {
            return false;
        };
        if start_fid == target_fid {
            return false;
        }
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        let mut queue: Vec<u32> = Vec::new();
        visited.insert(start_fid);
        queue.push(start_fid);

        let mut head = 0usize;
        let mut next_fids: Vec<u32> = Vec::new();
        while let Some(&fid) = queue.get(head) {
            head = head.saturating_add(1);
            let Some(cell_key) = view
                .slots
                .get(fid as usize)
                .and_then(Option::as_ref)
                .map(|s| s.key)
            else {
                continue;
            };
            next_fids.clear();
            Self::collect_cell_dependents_unfiltered(view, cell_key, true, &mut next_fids);
            if cell_key == override_anchor {
                for &shadow_key in override_footprint {
                    if shadow_key != cell_key {
                        Self::collect_cell_dependents_unfiltered(
                            view,
                            shadow_key,
                            false,
                            &mut next_fids,
                        );
                    }
                }
            } else if spills.has_spills_on_sheet(cell_key.0)
                && let Some(entry) = spills.active_spill(cell_key)
            {
                for &row in &entry.rows {
                    for &col in &entry.cols {
                        let shadow_key = (cell_key.0, row, col);
                        if shadow_key != cell_key {
                            Self::collect_cell_dependents_unfiltered(
                                view,
                                shadow_key,
                                false,
                                &mut next_fids,
                            );
                        }
                    }
                }
            } else if let Some(fp) = spills.dynamic_blocked_footprint(cell_key) {
                for &shadow_key in fp {
                    if shadow_key != cell_key {
                        Self::collect_cell_dependents_unfiltered(
                            view,
                            shadow_key,
                            false,
                            &mut next_fids,
                        );
                    }
                }
            }

            for &dep_fid in &next_fids {
                if dep_fid == target_fid {
                    return true;
                }
                if visited.insert(dep_fid) {
                    queue.push(dep_fid);
                }
            }
        }
        false
    }

    fn classify_spill_collision_in(
        view: &DepView<'_>,
        spills: &SpillRegistry,
        anchor: WorkbookCellKey,
        anchor_footprint: &[WorkbookCellKey],
        other: WorkbookCellKey,
    ) -> SpillCollisionOrder {
        let a_reaches_b =
            Self::formula_reaches_in(view, spills, anchor, other, anchor, anchor_footprint);
        let b_reaches_a =
            Self::formula_reaches_in(view, spills, other, anchor, anchor, anchor_footprint);
        if b_reaches_a && !a_reaches_b {
            SpillCollisionOrder::OtherUpstream
        } else if a_reaches_b && !b_reaches_a {
            SpillCollisionOrder::AnchorUpstream
        } else {
            SpillCollisionOrder::Symmetric
        }
    }

    fn propagate_dirty(&mut self, roots: Vec<u32>) {
        self.wave_cycle_verified.clear();
        self.propagate_dirty_impl(roots, &[], false);
    }

    /// Marks `roots` and their transitive dependents dirty within an active recalc
    /// wave, capping per-formula re-queues at [`MAX_WAVE_SPILL_REQUEUES`] and
    /// skipping any `fid` in `exclude` to prevent shadow-mediated infinite loops.
    fn propagate_dirty_in_wave(&mut self, roots: Vec<u32>, exclude: &[u32]) {
        self.propagate_dirty_impl(roots, exclude, true);
    }

    fn try_mark_dirty_for_bfs(
        &mut self,
        fid: u32,
        exclude: &[u32],
        cap_wave_requeues: bool,
        bfs: &mut Vec<u32>,
    ) {
        if exclude.contains(&fid) || self.slot_of(fid).is_none() {
            return;
        }
        let st = self.state_of(fid);
        if st == STATE_SCC_WAIT {
            if !self.wave_deferred_dirty.contains(&fid) {
                self.wave_deferred_dirty.push(fid);
            }
            return;
        }
        if st != STATE_CLEAN {
            return;
        }
        if cap_wave_requeues {
            let count = self.wave_spill_requeues.entry(fid).or_insert(0);
            if *count >= MAX_WAVE_SPILL_REQUEUES {
                return;
            }
            *count = count.saturating_add(1);
        }
        self.set_state(fid, STATE_DIRTY);
        self.dirty_queue.push(fid);
        bfs.push(fid);
    }

    fn propagate_dirty_impl(&mut self, roots: Vec<u32>, exclude: &[u32], cap_wave_requeues: bool) {
        if roots.is_empty() {
            return;
        }
        self.dirty_queue.reserve(roots.len());
        let mut bfs: Vec<u32> = Vec::with_capacity(roots.len());
        for fid in roots {
            self.try_mark_dirty_for_bfs(fid, exclude, cap_wave_requeues, &mut bfs);
        }
        let mut head = 0usize;
        while let Some(&fid) = bfs.get(head) {
            head = head.saturating_add(1);
            let Some(cell_key) = self.slot_of(fid).map(|s| s.key) else {
                continue;
            };
            let wb_ref = &self.workbook;
            let states_ref = &self.states;
            let mut newly_found: SmallVec<[u32; 8]> = SmallVec::new();
            self.dep_graph
                .for_each_cell_dependent(cell_key, wb_ref, |dep_fid| {
                    let st = states_ref
                        .get(dep_fid as usize)
                        .copied()
                        .unwrap_or(STATE_CLEAN);
                    if st == STATE_CLEAN {
                        newly_found.push(dep_fid);
                    }
                });
            self.dep_graph
                .for_each_spill_dependent(cell_key, |dep_fid| {
                    let st = states_ref
                        .get(dep_fid as usize)
                        .copied()
                        .unwrap_or(STATE_CLEAN);
                    if st == STATE_CLEAN {
                        newly_found.push(dep_fid);
                    }
                });
            let has_names = !self.name_slots.is_empty();
            let mut seed_nids = Vec::new();
            if has_names {
                self.name_dep_graph
                    .for_each_cell_dependent(cell_key, wb_ref, |nid| seed_nids.push(nid));
                self.name_dep_graph
                    .for_each_spill_dependent(cell_key, |nid| seed_nids.push(nid));
            }
            if self.spills.has_spills_on_sheet(cell_key.0)
                && let Some(entry) = self.spills.active_spill(cell_key)
            {
                for &row in &entry.rows {
                    for &col in &entry.cols {
                        let shadow_key = (cell_key.0, row, col);
                        if shadow_key == cell_key {
                            continue;
                        }
                        self.dep_graph
                            .for_each_cell_dependent(shadow_key, wb_ref, |dep_fid| {
                                let st = states_ref
                                    .get(dep_fid as usize)
                                    .copied()
                                    .unwrap_or(STATE_CLEAN);
                                if st == STATE_CLEAN {
                                    newly_found.push(dep_fid);
                                }
                            });
                        if has_names {
                            self.name_dep_graph.for_each_cell_dependent(
                                shadow_key,
                                wb_ref,
                                |nid| seed_nids.push(nid),
                            );
                        }
                    }
                }
            }
            if !seed_nids.is_empty() {
                let mut name_fids = Vec::new();
                self.collect_name_dependents_from_nids(&seed_nids, &mut name_fids);
                newly_found.extend(name_fids);
            }
            for dep_fid in newly_found {
                self.try_mark_dirty_for_bfs(dep_fid, exclude, cap_wave_requeues, &mut bfs);
            }
        }
        if !bfs.is_empty() {
            self.dirty_needs_sort = true;
        }
    }

    fn abort_paused_call_stack(&mut self) {
        self.wave_cycle_verified.clear();
        self.wave_deferred_dirty.clear();
        if self.call_stack.is_empty() && self.scc_stack.is_empty() {
            return;
        }
        let paused = core::mem::take(&mut self.call_stack);
        for fid in paused {
            if self.state_of(fid) == STATE_EVALUATING {
                self.set_state(fid, STATE_DIRTY);
                self.dirty_queue.push(fid);
                self.dirty_needs_sort = true;
            }
        }
        let parked = core::mem::take(&mut self.scc_stack);
        self.scc_low.clear();
        for fid in parked {
            if self.state_of(fid) == STATE_SCC_WAIT {
                self.set_state(fid, STATE_DIRTY);
                self.dirty_queue.push(fid);
                self.dirty_needs_sort = true;
            }
        }
    }

    fn release_scc_wait_at_or_above(&mut self, min_pos: usize) {
        self.release_scc_wait_while(Some(min_pos));
    }

    fn release_all_scc_wait_to_dirty(&mut self) {
        self.release_scc_wait_while(None);
    }

    fn release_scc_wait_while(&mut self, min_pos: Option<usize>) {
        let mut repropagate = Vec::new();
        while let Some(&top) = self.scc_stack.last() {
            if let Some(threshold) = min_pos
                && self.scc_low.get(&top).copied().unwrap_or(0) < threshold
            {
                break;
            }
            let _ = self.scc_stack.pop();
            let _ = self.scc_low.remove(&top);
            if self.state_of(top) == STATE_SCC_WAIT {
                if self.wave_deferred_dirty.contains(&top) {
                    self.wave_deferred_dirty.retain(|&f| f != top);
                    self.set_state(top, STATE_CLEAN);
                    repropagate.push(top);
                } else {
                    self.set_state(top, STATE_DIRTY);
                    self.dirty_queue.push(top);
                    self.dirty_needs_sort = true;
                }
            }
        }
        if !repropagate.is_empty() {
            self.propagate_dirty_in_wave(repropagate.clone(), &[]);
            for fid in repropagate {
                if self.state_of(fid) == STATE_CLEAN && self.slot_of(fid).is_some() {
                    self.set_state(fid, STATE_DIRTY);
                    self.dirty_queue.push(fid);
                    self.dirty_needs_sort = true;
                }
            }
        }
    }

    fn flush_deferred_dirty(&mut self) {
        if self.wave_deferred_dirty.is_empty() {
            return;
        }
        let deferred = core::mem::take(&mut self.wave_deferred_dirty);
        let mut ready = Vec::new();
        let mut ensure_dirty = Vec::new();
        let mut remaining = Vec::new();
        for fid in deferred {
            match self.state_of(fid) {
                STATE_CLEAN => ready.push(fid),
                STATE_DIRTY => {
                    self.set_state(fid, STATE_CLEAN);
                    ready.push(fid);
                    ensure_dirty.push(fid);
                }
                STATE_SCC_WAIT => remaining.push(fid),
                _ => {}
            }
        }
        self.wave_deferred_dirty = remaining;
        if !ready.is_empty() {
            self.propagate_dirty_in_wave(ready, &[]);
        }
        for fid in ensure_dirty {
            if self.state_of(fid) == STATE_CLEAN && self.slot_of(fid).is_some() {
                self.set_state(fid, STATE_DIRTY);
                self.dirty_queue.push(fid);
                self.dirty_needs_sort = true;
            }
        }
    }

    fn prepare_dirty_queue(&mut self) {
        if !self.dirty_needs_sort {
            return;
        }
        if self.dirty_cursor > 0 {
            self.dirty_queue.drain(0..self.dirty_cursor);
            self.dirty_cursor = 0;
        }
        let slots_ref = &self.slots;
        self.dirty_queue.sort_unstable_by_key(|&fid| {
            slots_ref
                .get(fid as usize)
                .and_then(Option::as_ref)
                .map_or((SheetId(u32::MAX), RowId(u64::MAX), ColId(u64::MAX)), |s| {
                    s.key
                })
        });
        self.dirty_queue.dedup();
        self.dirty_needs_sort = false;
    }

    fn collect_cleared_spill_dependents(
        &self,
        fid: u32,
        key: WorkbookCellKey,
        clear_out: &crate::spill::ClearSpillOutcome,
        post_dirty: &mut Vec<u32>,
    ) {
        for &shadow_key in &clear_out.cleared_shadows {
            self.collect_direct_dependents(shadow_key, post_dirty);
        }
        self.collect_spill_anchor_dependents(key, post_dirty);
        self.push_unblocked_watchers(&clear_out.unblocked_watchers, fid, post_dirty);
    }

    fn handle_cycle(&mut self, cur_fid: u32, min_pos: usize) {
        let my_pos = self.call_stack.len().saturating_sub(1);
        if min_pos < my_pos {
            let _ = self.call_stack.pop();
            self.set_state(cur_fid, STATE_SCC_WAIT);
            self.scc_stack.push(cur_fid);
            self.scc_low.insert(cur_fid, min_pos);
            return;
        }
        let _ = self.call_stack.pop();
        let mut scc_fids: SmallVec<[u32; 8]> = SmallVec::new();
        scc_fids.push(cur_fid);
        while let Some(&top) = self.scc_stack.last() {
            let top_low = self.scc_low.get(&top).copied().unwrap_or(0);
            if top_low < my_pos {
                break;
            }
            let _ = self.scc_stack.pop();
            let _ = self.scc_low.remove(&top);
            if self.state_of(top) == STATE_SCC_WAIT {
                scc_fids.push(top);
            }
        }

        let mut active_spill_fids: SmallVec<[u32; 4]> = SmallVec::new();
        let mut unverified_cycle_spill_fids: SmallVec<[u32; 4]> = SmallVec::new();
        let mut needs_verify = false;
        for &fid in &scc_fids {
            if let Some(key) = self.slot_of(fid).map(|s| s.key) {
                if self.spills.active_spill(key).is_some() {
                    active_spill_fids.push(fid);
                    if !self.wave_cycle_verified.contains(&fid) {
                        needs_verify = true;
                    }
                } else if self.cycle_spills.contains_key(&key)
                    && !self.wave_cycle_verified.contains(&fid)
                {
                    unverified_cycle_spill_fids.push(fid);
                    needs_verify = true;
                }
            }
        }
        if needs_verify {
            // Clear stale active spills (and unverified prior-wave cycle watches)
            // along the candidate SCC once per wave so conditional formulas that
            // stop spilling re-evaluate before `#CYCLE!` is committed.
            for fid in active_spill_fids {
                self.wave_cycle_verified.insert(fid);
            }
            for &fid in &unverified_cycle_spill_fids {
                self.wave_cycle_verified.insert(fid);
            }
            let mut cleared_anchors = Vec::new();
            let mut post_dirty = Vec::new();
            for &fid in &scc_fids {
                if let Some(key) = self.slot_of(fid).map(|s| s.key) {
                    if self.spills.active_spill(key).is_some() {
                        let clear_out = self.spills.clear_anchor(key);
                        if clear_out.had_state {
                            self.record_changed_cells(&clear_out.cleared_shadows);
                            cleared_anchors.push((fid, key, clear_out));
                        }
                    } else if unverified_cycle_spill_fids.contains(&fid) {
                        self.drain_cycle_spill_anchor(key, None, &mut post_dirty);
                    }
                }
                self.set_state(fid, STATE_DIRTY);
                self.dirty_queue.push(fid);
                self.dirty_needs_sort = true;
            }
            for (fid, key, clear_out) in cleared_anchors {
                self.collect_cleared_spill_dependents(fid, key, &clear_out, &mut post_dirty);
            }
            if !post_dirty.is_empty() {
                self.propagate_dirty_in_wave(post_dirty, &[]);
            }
            self.flush_deferred_dirty();
            return;
        }

        let mut cleared_anchors = Vec::new();
        let mut retained_cycle_anchors: SmallVec<[WorkbookCellKey; 4]> = SmallVec::new();
        let mut unwound_keys: SmallVec<[WorkbookCellKey; 8]> = SmallVec::new();
        let mut post_dirty = Vec::new();
        for &fid in &scc_fids {
            if let Some(key) = self.slot_of(fid).map(|s| s.key) {
                let clear_out = self.spills.clear_anchor(key);
                if clear_out.had_state {
                    self.record_changed_cells(&clear_out.cleared_shadows);
                    cleared_anchors.push((fid, key, clear_out));
                } else if self.cycle_spills.contains_key(&key) {
                    retained_cycle_anchors.push(key);
                } else {
                    self.drain_cycle_spill_anchor(key, None, &mut post_dirty);
                }
                self.store_eval_scalar(key, EvalScalar::Error(CalcError::Cycle));
                unwound_keys.push(key);
            }
            self.set_state(fid, STATE_CLEAN);
            self.wave_done = self.wave_done.saturating_add(1);
        }
        for key in retained_cycle_anchors {
            if let Some(rec) = self.cycle_spills.get_mut(&key) {
                for &peer in &unwound_keys {
                    if peer != key && !rec.peers.contains(&peer) {
                        rec.peers.push(peer);
                    }
                }
            }
        }
        for (fid, key, clear_out) in cleared_anchors {
            self.record_cycle_spill(key, &unwound_keys, &clear_out.cleared_shadows);
            self.collect_cleared_spill_dependents(fid, key, &clear_out, &mut post_dirty);
        }
        if !post_dirty.is_empty() {
            self.propagate_dirty_in_wave(post_dirty, &scc_fids);
        }
        self.flush_deferred_dirty();
    }

    fn store_eval_scalar(&mut self, key: WorkbookCellKey, scalar: EvalScalar) {
        let Some(sheet) = self.workbook.sheet_mut(key.0) else {
            return;
        };
        let old_bits = sheet.value_by_id(key.1, key.2).pack();
        let new_val = match scalar {
            EvalScalar::Empty => Value::Empty,
            EvalScalar::Number(n) => Value::number(n),
            EvalScalar::Bool(b) => Value::Bool(b),
            EvalScalar::Error(e) => Value::Error(e),
            EvalScalar::Text(s) => match sheet.add_string(&s) {
                Some(id) => Value::Text(id),
                None => Value::Error(CalcError::Calc),
            },
        };
        sheet.set_value_by_id(key.1, key.2, new_val);
        if new_val.pack() != old_bits {
            self.changes.cells.push(key);
        }
    }

    fn eval_root_formula(&mut self, root: &BoundExpr, caller: CallerContext) -> ExprOutcome {
        // Outermost `Add`/`Sub` (not wrapped in `Paren`) gets D-027 tail-zeroing!
        let out = self.eval_expr(root, caller, true, 0);
        match out {
            ExprOutcome::Ready(v) => match self.ensure_value_ready(&v) {
                Ok(()) => ExprOutcome::Ready(v),
                Err(blocked_or_cycle) => blocked_or_cycle,
            },
            other => other,
        }
    }

    fn eval_expr(
        &mut self,
        expr: &BoundExpr,
        caller: CallerContext,
        is_outermost: bool,
        depth: usize,
    ) -> ExprOutcome {
        if depth > MAX_NESTING_DEPTH {
            return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Calc)));
        }
        let next_depth = depth.saturating_add(1);

        match expr {
            BoundExpr::Literal(lit) => {
                ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::from_literal(lit)))
            }
            BoundExpr::ArrayLiteral {
                rows,
                cols,
                elements,
            } => {
                let vals = elements.iter().map(EvalScalar::from_literal).collect();
                match Array2D::new(*rows, *cols, vals) {
                    Ok(arr) => ExprOutcome::Ready(EvalValue::Array(arr)),
                    Err(e) => ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
                }
            }
            BoundExpr::Reference(r) => ExprOutcome::Ready(EvalValue::Reference(resolve_bound_ref(
                r,
                &self.workbook,
                caller.sheet,
            ))),
            BoundExpr::Name(raw_name) => {
                self.eval_named_range(caller.sheet, raw_name, caller, next_depth)
            }
            BoundExpr::ScopedName {
                sheet,
                missing_sheet,
                name,
            } => {
                if missing_sheet.is_some() {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Ref,
                    )));
                }
                let Some(sid) = *sheet else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Ref,
                    )));
                };
                if self.workbook.live_sheet(sid).is_none() {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Ref,
                    )));
                }
                self.eval_named_range(sid, name, caller, next_depth)
            }
            BoundExpr::SpillRef(inner) => {
                let inner_val = match self.eval_expr(inner, caller, false, next_depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                let EvalValue::Reference(ResolvedRef::Cell { sheet, row, col }) = inner_val else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Ref,
                    )));
                };
                if let Err(wait) = self.ensure_value_ready(&inner_val) {
                    return wait;
                }
                let Some(entry) = self.spills.active_spill((sheet, row, col)) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Ref,
                    )));
                };
                if entry.rows.len() == 1 && entry.cols.len() == 1 {
                    return ExprOutcome::Ready(EvalValue::Reference(ResolvedRef::Cell {
                        sheet,
                        row,
                        col,
                    }));
                }
                let (Some(&first_row), Some(&last_row), Some(&first_col), Some(&last_col)) = (
                    entry.rows.first(),
                    entry.rows.last(),
                    entry.cols.first(),
                    entry.cols.last(),
                ) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Ref,
                    )));
                };
                ExprOutcome::Ready(EvalValue::Reference(ResolvedRef::Area {
                    sheet,
                    first_row,
                    last_row,
                    first_col,
                    last_col,
                }))
            }
            BoundExpr::Opaque { .. } => {
                ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Name)))
            }
            BoundExpr::PreservedCached { cached, .. } => {
                let scalar = match cached {
                    ScalarValue::Number(n) => EvalScalar::Number(*n),
                    ScalarValue::Text(s) => EvalScalar::Text(s.clone()),
                    ScalarValue::Bool(b) => EvalScalar::Bool(*b),
                    ScalarValue::Error(e) => EvalScalar::Error(*e),
                };
                ExprOutcome::Ready(EvalValue::Scalar(scalar))
            }
            BoundExpr::MissingArg => ExprOutcome::Ready(EvalValue::Missing),
            // Explicit parentheses suppress outermost `Add`/`Sub` tail-zeroing!
            BoundExpr::Paren(inner) => self.eval_expr(inner, caller, false, next_depth),
            BoundExpr::Unary { op, expr: inner } => {
                let inner_val = match self.eval_expr(inner, caller, false, next_depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&inner_val) {
                    return wait;
                }
                ExprOutcome::Ready(eval_unary(
                    *op,
                    inner_val,
                    &self.workbook,
                    &self.spills,
                    caller,
                ))
            }
            BoundExpr::Binary { op, left, right } => {
                let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
                let mut cycle_low: Option<usize> = None;
                let lhs_val = self.eval_and_check_ready(
                    left,
                    caller,
                    next_depth,
                    &mut blocked,
                    &mut cycle_low,
                );
                let rhs_val = self.eval_and_check_ready(
                    right,
                    caller,
                    next_depth,
                    &mut blocked,
                    &mut cycle_low,
                );
                if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                    return wait;
                }
                let (Some(lhs), Some(rhs)) = (lhs_val, rhs_val) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Value,
                    )));
                };
                let snap = is_outermost && matches!(op, BinaryOp::Add | BinaryOp::Sub);
                ExprOutcome::Ready(eval_binary(
                    *op,
                    lhs,
                    rhs,
                    snap,
                    &self.workbook,
                    &self.spills,
                    caller,
                ))
            }
            BoundExpr::Call { name, args } => self.eval_call(name, args, caller, next_depth),
        }
    }

    fn eval_named_range(
        &mut self,
        scope_sheet: SheetId,
        raw_name: &str,
        caller: CallerContext,
        depth: usize,
    ) -> ExprOutcome {
        let Some((key, def)) = self.workbook.resolve_named_range(scope_sheet, raw_name) else {
            return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Name)));
        };
        let key_owned = key.clone();
        let root_owned = def.formula.root.clone();
        let eval_sheet = key_owned.scope.unwrap_or(scope_sheet);
        if self.name_eval_stack.contains(&key_owned) {
            return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Cycle)));
        }
        self.name_eval_stack.push(key_owned);
        let sub_caller = CallerContext {
            sheet: eval_sheet,
            row: caller.row,
            col: caller.col,
            mode: caller.mode,
        };
        let out = self.eval_expr(&root_owned, sub_caller, false, depth);
        let _ = self.name_eval_stack.pop();
        out
    }

    fn eval_call(
        &mut self,
        name: &str,
        args: &[BoundExpr],
        caller: CallerContext,
        depth: usize,
    ) -> ExprOutcome {
        // 1. Short-circuiting functions: `IF`, `IFERROR`, `IFNA`.
        if name == "IF" {
            if !(2..=3).contains(&args.len()) {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let Some(cond_expr) = args.first() else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let cond_raw = match self.eval_expr(cond_expr, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                other => return other,
            };
            if let Err(wait) = self.ensure_value_ready(&cond_raw) {
                return wait;
            }
            let cond_sa = match coerce_for_scalar_op(cond_raw, &self.workbook, &self.spills, caller)
            {
                Ok(sa) => sa,
                Err(e) => return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
            };

            let (need_true, need_false) = match &cond_sa {
                crate::value_eval::ScalarOrArray::Scalar(s) => match s.to_bool() {
                    Err(e) => {
                        return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e)));
                    }
                    Ok(true) => (true, false),
                    Ok(false) => (false, args.len() == 3),
                },
                crate::value_eval::ScalarOrArray::Array(arr) => {
                    let any_t = arr.values.iter().any(|s| matches!(s.to_bool(), Ok(true)));
                    let any_f = arr.values.iter().any(|s| matches!(s.to_bool(), Ok(false)));
                    (any_t, any_f && args.len() == 3)
                }
            };

            let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
            let mut cycle_low: Option<usize> = None;
            let when_true = if need_true {
                let Some(t_expr) = args.get(1) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Value,
                    )));
                };
                self.eval_and_check_ready(t_expr, caller, depth, &mut blocked, &mut cycle_low)
            } else {
                None
            };

            let when_false = if need_false {
                let Some(f_expr) = args.get(2) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Value,
                    )));
                };
                self.eval_and_check_ready(f_expr, caller, depth, &mut blocked, &mut cycle_low)
            } else {
                None
            };

            if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                return wait;
            }

            return ExprOutcome::Ready(combine_if(
                cond_sa,
                when_true,
                when_false,
                args.len() == 3,
                &self.workbook,
                &self.spills,
                caller,
            ));
        }

        if name == "IFERROR" || name == "IFNA" {
            let only_na = name == "IFNA";
            if args.len() != 2 {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let (Some(primary_expr), Some(fb_expr)) = (args.first(), args.get(1)) else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let p_raw = match self.eval_expr(primary_expr, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                other => return other,
            };
            if let Err(wait) = self.ensure_value_ready(&p_raw) {
                return wait;
            }
            let p_sa = match coerce_for_scalar_op(p_raw, &self.workbook, &self.spills, caller) {
                Ok(sa) => sa,
                Err(e) => crate::value_eval::ScalarOrArray::Scalar(EvalScalar::Error(e)),
            };
            let catches = |e: CalcError| !only_na || e == CalcError::Na;
            let need_fb = match &p_sa {
                crate::value_eval::ScalarOrArray::Scalar(EvalScalar::Error(e)) => catches(*e),
                crate::value_eval::ScalarOrArray::Scalar(_) => false,
                crate::value_eval::ScalarOrArray::Array(arr) => arr
                    .values
                    .iter()
                    .any(|s| matches!(s, EvalScalar::Error(e) if catches(*e))),
            };
            let fb_val = if need_fb {
                let v = match self.eval_expr(fb_expr, caller, false, depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&v) {
                    return wait;
                }
                Some(v)
            } else {
                None
            };
            return ExprOutcome::Ready(combine_iferror(
                p_sa,
                fb_val,
                only_na,
                &self.workbook,
                &self.spills,
                caller,
            ));
        }

        if name == "CHOOSE" {
            if args.len() < 2 {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let Some(idx_expr) = args.first() else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let idx_raw = match self.eval_expr(idx_expr, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                other => return other,
            };
            if let Err(wait) = self.ensure_value_ready(&idx_raw) {
                return wait;
            }
            let idx_sa = match coerce_for_scalar_op(idx_raw, &self.workbook, &self.spills, caller) {
                Ok(sa) => sa,
                Err(e) => return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
            };

            let choice_exprs = args.get(1..).unwrap_or(&[]);
            let mut needed_mask = vec![false; choice_exprs.len()];
            let mut mark_index = |s: &EvalScalar| {
                if let Ok(idx_1based) = s.to_number().and_then(crate::functions::trunc_to_isize)
                    && idx_1based >= 1
                    && let Ok(i) = usize::try_from(idx_1based)
                    && let Some(flag) = needed_mask.get_mut(i.saturating_sub(1))
                {
                    *flag = true;
                }
            };
            match &idx_sa {
                crate::value_eval::ScalarOrArray::Scalar(s) => mark_index(s),
                crate::value_eval::ScalarOrArray::Array(arr) => {
                    for s in &arr.values {
                        mark_index(s);
                    }
                }
            }

            let mut evaluated_choices: Vec<Option<EvalValue>> =
                Vec::with_capacity(choice_exprs.len());
            let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
            let mut cycle_low: Option<usize> = None;
            for (i, c_expr) in choice_exprs.iter().enumerate() {
                if !needed_mask.get(i).copied().unwrap_or(false) {
                    evaluated_choices.push(None);
                    continue;
                }
                let val =
                    self.eval_and_check_ready(c_expr, caller, depth, &mut blocked, &mut cycle_low);
                evaluated_choices.push(val);
            }
            if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                return wait;
            }
            return ExprOutcome::Ready(combine_choose(
                idx_sa,
                evaluated_choices,
                &self.workbook,
                &self.spills,
                caller,
            ));
        }

        if name == "IFS" {
            if args.is_empty() || !args.len().is_multiple_of(2) {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let pair_count = args.len() / 2;
            let mut conds_so_far = Vec::with_capacity(pair_count);
            let mut array_mode = false;
            for pair_idx in 0..pair_count {
                let Some(cond_expr) = args.get(pair_idx.saturating_mul(2)) else {
                    break;
                };
                let cond_raw = match self.eval_expr(cond_expr, caller, false, depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&cond_raw) {
                    return wait;
                }
                let cond_sa =
                    match coerce_for_scalar_op(cond_raw, &self.workbook, &self.spills, caller) {
                        Ok(sa) => sa,
                        Err(e) => {
                            return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e)));
                        }
                    };
                match &cond_sa {
                    crate::value_eval::ScalarOrArray::Scalar(sc) if !array_mode => {
                        match sc.to_bool() {
                            Err(e) => {
                                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e)));
                            }
                            Ok(true) => {
                                let Some(val_expr) =
                                    args.get(pair_idx.saturating_mul(2).saturating_add(1))
                                else {
                                    return ExprOutcome::Ready(EvalValue::Scalar(
                                        EvalScalar::Error(CalcError::Value),
                                    ));
                                };
                                let v = match self.eval_expr(val_expr, caller, false, depth) {
                                    ExprOutcome::Ready(v) => v,
                                    other => return other,
                                };
                                if let Err(wait) = self.ensure_value_ready(&v) {
                                    return wait;
                                }
                                return ExprOutcome::Ready(
                                    crate::functions::finalize_branch_value(
                                        v,
                                        &self.workbook,
                                        &self.spills,
                                        caller,
                                    ),
                                );
                            }
                            Ok(false) => {
                                conds_so_far.push(cond_sa);
                            }
                        }
                    }
                    _ => {
                        array_mode = true;
                        conds_so_far.push(cond_sa);
                    }
                }
            }
            if !array_mode {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Na)));
            }
            let mut branch_vals = Vec::with_capacity(pair_count);
            let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
            let mut cycle_low: Option<usize> = None;
            for pair_idx in 0..pair_count {
                let Some(cond_sa) = conds_so_far.get(pair_idx) else {
                    branch_vals.push(None);
                    continue;
                };
                let needed = match cond_sa {
                    crate::value_eval::ScalarOrArray::Scalar(s) => matches!(s.to_bool(), Ok(true)),
                    crate::value_eval::ScalarOrArray::Array(arr) => {
                        arr.values.iter().any(|s| matches!(s.to_bool(), Ok(true)))
                    }
                };
                if !needed {
                    branch_vals.push(None);
                    continue;
                }
                let Some(val_expr) = args.get(pair_idx.saturating_mul(2).saturating_add(1)) else {
                    branch_vals.push(None);
                    continue;
                };
                let val = self.eval_and_check_ready(
                    val_expr,
                    caller,
                    depth,
                    &mut blocked,
                    &mut cycle_low,
                );
                branch_vals.push(val);
            }
            if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                return wait;
            }
            return ExprOutcome::Ready(crate::functions::combine_ifs_array(
                &conds_so_far,
                branch_vals,
                &self.workbook,
                &self.spills,
                caller,
            ));
        }

        if name == "SWITCH" {
            if args.len() < 3 {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let Some(expr_ast) = args.first() else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let target_raw = match self.eval_expr(expr_ast, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                other => return other,
            };
            if let Err(wait) = self.ensure_value_ready(&target_raw) {
                return wait;
            }
            let target_sa =
                match coerce_for_scalar_op(target_raw, &self.workbook, &self.spills, caller) {
                    Ok(sa) => sa,
                    Err(e) => return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
                };
            let rest = args.get(1..).unwrap_or(&[]);
            let pair_count = rest.len() / 2;
            let default_expr = if rest.len() % 2 == 1 {
                rest.last()
            } else {
                None
            };
            if let crate::value_eval::ScalarOrArray::Scalar(ref target_sc) = target_sa {
                if let EvalScalar::Error(e) = target_sc {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(*e)));
                }
                for p in 0..pair_count {
                    let (Some(cand_expr), Some(res_expr)) = (
                        rest.get(p.saturating_mul(2)),
                        rest.get(p.saturating_mul(2).saturating_add(1)),
                    ) else {
                        continue;
                    };
                    let cand_raw = match self.eval_expr(cand_expr, caller, false, depth) {
                        ExprOutcome::Ready(v) => v,
                        other => return other,
                    };
                    if let Err(wait) = self.ensure_value_ready(&cand_raw) {
                        return wait;
                    }
                    let cand_sa = match coerce_for_scalar_op(
                        cand_raw,
                        &self.workbook,
                        &self.spills,
                        caller,
                    ) {
                        Ok(sa) => sa,
                        Err(e) => {
                            return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e)));
                        }
                    };
                    let crate::value_eval::ScalarOrArray::Scalar(cand_sc) = cand_sa else {
                        return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                            CalcError::Value,
                        )));
                    };
                    match crate::functions::switch_scalar_eq(target_sc, &cand_sc) {
                        Ok(true) => {
                            let v = match self.eval_expr(res_expr, caller, false, depth) {
                                ExprOutcome::Ready(v) => v,
                                other => return other,
                            };
                            if let Err(wait) = self.ensure_value_ready(&v) {
                                return wait;
                            }
                            return ExprOutcome::Ready(crate::functions::finalize_branch_value(
                                v,
                                &self.workbook,
                                &self.spills,
                                caller,
                            ));
                        }
                        Ok(false) => {}
                        Err(e) => {
                            return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e)));
                        }
                    }
                }
                if let Some(def_e) = default_expr {
                    let v = match self.eval_expr(def_e, caller, false, depth) {
                        ExprOutcome::Ready(v) => v,
                        other => return other,
                    };
                    if let Err(wait) = self.ensure_value_ready(&v) {
                        return wait;
                    }
                    return ExprOutcome::Ready(crate::functions::finalize_branch_value(
                        v,
                        &self.workbook,
                        &self.spills,
                        caller,
                    ));
                }
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Na)));
            }
            // Array target fallback: evaluate arguments and match elementwise.
            let mut evaled_cands = Vec::with_capacity(pair_count);
            let mut evaled_res = Vec::with_capacity(pair_count);
            let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
            let mut cycle_low: Option<usize> = None;
            for p in 0..pair_count {
                let (Some(cand_e), Some(res_e)) = (
                    rest.get(p.saturating_mul(2)),
                    rest.get(p.saturating_mul(2).saturating_add(1)),
                ) else {
                    continue;
                };
                let cv =
                    self.eval_and_check_ready(cand_e, caller, depth, &mut blocked, &mut cycle_low);
                let rv =
                    self.eval_and_check_ready(res_e, caller, depth, &mut blocked, &mut cycle_low);
                if let (Some(c), Some(r)) = (cv, rv) {
                    evaled_cands.push(c);
                    evaled_res.push(r);
                }
            }
            let evaled_def = default_expr.and_then(|def_e| {
                self.eval_and_check_ready(def_e, caller, depth, &mut blocked, &mut cycle_low)
            });
            if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                return wait;
            }
            let crate::value_eval::ScalarOrArray::Array(arr) = target_sa else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let mut out = Vec::with_capacity(arr.values.len());
            for tsc in &arr.values {
                if let EvalScalar::Error(e) = tsc {
                    out.push(EvalScalar::Error(*e));
                    continue;
                }
                let mut hit: Option<EvalScalar> = None;
                for (cv, rv) in evaled_cands.iter().zip(evaled_res.iter()) {
                    let cs = match coerce_for_scalar_op(
                        cv.clone(),
                        &self.workbook,
                        &self.spills,
                        caller,
                    ) {
                        Ok(crate::value_eval::ScalarOrArray::Scalar(s)) => s,
                        Ok(_) => {
                            hit = Some(EvalScalar::Error(CalcError::Value));
                            break;
                        }
                        Err(e) => {
                            hit = Some(EvalScalar::Error(e));
                            break;
                        }
                    };
                    match crate::functions::switch_scalar_eq(tsc, &cs) {
                        Ok(true) => {
                            let rs = match coerce_for_scalar_op(
                                rv.clone(),
                                &self.workbook,
                                &self.spills,
                                caller,
                            ) {
                                Ok(crate::value_eval::ScalarOrArray::Scalar(s)) => s,
                                Ok(_) => EvalScalar::Error(CalcError::Value),
                                Err(e) => EvalScalar::Error(e),
                            };
                            hit = Some(rs);
                            break;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            hit = Some(EvalScalar::Error(e));
                            break;
                        }
                    }
                }
                let final_cell = match hit {
                    Some(s) => s,
                    None => match &evaled_def {
                        None => EvalScalar::Error(CalcError::Na),
                        Some(dv) => match coerce_for_scalar_op(
                            dv.clone(),
                            &self.workbook,
                            &self.spills,
                            caller,
                        ) {
                            Ok(crate::value_eval::ScalarOrArray::Scalar(s)) => s,
                            Ok(_) => EvalScalar::Error(CalcError::Value),
                            Err(e) => EvalScalar::Error(e),
                        },
                    },
                };
                out.push(final_cell);
            }
            return ExprOutcome::Ready(
                crate::value_eval::Array2D::new(arr.rows, arr.cols, out).map_or_else(
                    |e| EvalValue::Scalar(EvalScalar::Error(e)),
                    EvalValue::Array,
                ),
            );
        }

        if name == "OFFSET" {
            if !(3..=5).contains(&args.len()) {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let Some(ref_expr) = args.first() else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            // Arg 0 MUST NOT be dereferenced!
            let base_ref = match self.eval_expr(ref_expr, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                other => return other,
            };
            let mut evaled = Vec::with_capacity(args.len());
            evaled.push(base_ref.clone());
            let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
            let mut cycle_low: Option<usize> = None;
            for arg in args.get(1..).unwrap_or(&[]) {
                if let Some(v) =
                    self.eval_and_check_ready(arg, caller, depth, &mut blocked, &mut cycle_low)
                {
                    evaled.push(v);
                }
            }
            if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                return wait;
            }
            let out_ref = crate::functions::eval_offset(
                base_ref,
                &evaled,
                &self.workbook,
                &self.spills,
                caller,
            );
            if let Err(wait) = self.ensure_value_ready(&out_ref) {
                return wait;
            }
            return ExprOutcome::Ready(out_ref);
        }

        if name == "INDIRECT" {
            if !(1..=2).contains(&args.len()) {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            }
            let Some(txt_expr) = args.first() else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let txt_raw = match self.eval_expr(txt_expr, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                other => return other,
            };
            if let Err(wait) = self.ensure_value_ready(&txt_raw) {
                return wait;
            }
            let txt_sa = match coerce_for_scalar_op(txt_raw, &self.workbook, &self.spills, caller) {
                Ok(sa) => sa,
                Err(e) => return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
            };
            let a1_style = match args.get(1) {
                None => true,
                Some(a1_expr) => {
                    let av = match self.eval_expr(a1_expr, caller, false, depth) {
                        ExprOutcome::Ready(v) => v,
                        other => return other,
                    };
                    if let Err(wait) = self.ensure_value_ready(&av) {
                        return wait;
                    }
                    if matches!(av, EvalValue::Missing) {
                        true
                    } else {
                        match coerce_for_scalar_op(av, &self.workbook, &self.spills, caller) {
                            Ok(crate::value_eval::ScalarOrArray::Scalar(sc)) => {
                                match sc.to_bool() {
                                    Ok(b) => b,
                                    Err(e) => {
                                        return ExprOutcome::Ready(EvalValue::Scalar(
                                            EvalScalar::Error(e),
                                        ));
                                    }
                                }
                            }
                            Ok(_) => {
                                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                                    CalcError::Value,
                                )));
                            }
                            Err(e) => {
                                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e)));
                            }
                        }
                    }
                }
            };
            let crate::value_eval::ScalarOrArray::Scalar(txt_sc) = txt_sa else {
                return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Value)));
            };
            let ref_str = match txt_sc.to_text() {
                Ok(s) => s,
                Err(e) => return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
            };
            match crate::functions::resolve_indirect(&ref_str, a1_style, &self.workbook, caller) {
                Err(e) => return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(e))),
                Ok(crate::functions::IndirectTarget::Ref(r)) => {
                    let val = EvalValue::Reference(r);
                    if let Err(wait) = self.ensure_value_ready(&val) {
                        return wait;
                    }
                    return ExprOutcome::Ready(val);
                }
                Ok(crate::functions::IndirectTarget::NamedRange { scope_sheet, name }) => {
                    let res = self.eval_named_range(scope_sheet, &name, caller, depth);
                    if let ExprOutcome::Ready(ref val) = res
                        && let Err(wait) = self.ensure_value_ready(val)
                    {
                        return wait;
                    }
                    return res;
                }
                Ok(crate::functions::IndirectTarget::SpillAnchor { sheet, row, col }) => {
                    let cell_val = EvalValue::Reference(ResolvedRef::Cell { sheet, row, col });
                    if let Err(wait) = self.ensure_value_ready(&cell_val) {
                        return wait;
                    }
                    let Some(entry) = self.spills.active_spill((sheet, row, col)) else {
                        return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                            CalcError::Ref,
                        )));
                    };
                    let (Some(&fr), Some(&lr), Some(&fc), Some(&lc)) = (
                        entry.rows.first(),
                        entry.rows.last(),
                        entry.cols.first(),
                        entry.cols.last(),
                    ) else {
                        return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                            CalcError::Ref,
                        )));
                    };
                    let out = EvalValue::Reference(ResolvedRef::Area {
                        sheet,
                        first_row: RowId(fr.0),
                        last_row: RowId(lr.0),
                        first_col: ColId(fc.0),
                        last_col: ColId(lc.0),
                    });
                    if let Err(wait) = self.ensure_value_ready(&out) {
                        return wait;
                    }
                    return ExprOutcome::Ready(out);
                }
            }
        }

        // 2. Coordinate/shape/metadata-only functions (`ROW`, `COLUMN`, `ROWS`,
        // `COLUMNS`, `ISREF`, `ISFORMULA`, `AREAS`) do NOT dereference cell values!
        let deref_args = !matches!(
            name,
            "ROW" | "COLUMN" | "ROWS" | "COLUMNS" | "ISREF" | "ISFORMULA" | "AREAS"
        );

        let mut evaluated_args = Vec::with_capacity(args.len());
        let mut all_blocked: SmallVec<[u32; 4]> = SmallVec::new();
        let mut cycle_low: Option<usize> = None;
        for arg in args {
            let val_opt = if deref_args {
                self.eval_and_check_ready(arg, caller, depth, &mut all_blocked, &mut cycle_low)
            } else {
                match self.eval_expr(arg, caller, false, depth) {
                    ExprOutcome::Ready(v) => Some(v),
                    ExprOutcome::Blocked(b) => {
                        merge_blocked(&mut all_blocked, b);
                        None
                    }
                    ExprOutcome::Cycle(low) => {
                        merge_cycle(&mut cycle_low, low);
                        None
                    }
                }
            };
            if let Some(val) = val_opt {
                evaluated_args.push(val);
            }
        }
        if let Some(wait) = Self::wait_outcome(all_blocked, cycle_low) {
            return wait;
        }

        let filters = &self.filters;
        let filter_pred = |s: SheetId, r: RowId| {
            filters
                .get(&s)
                .is_some_and(|(_, proj)| proj.is_row_id_hidden(r))
        };
        let is_row_hidden = if filters.is_empty() {
            None
        } else {
            Some(&filter_pred as &dyn Fn(SheetId, RowId) -> bool)
        };
        ExprOutcome::Ready(eval_eager_function(
            name,
            evaluated_args,
            &self.workbook,
            &self.spills,
            caller,
            &mut self.env,
            is_row_hidden,
        ))
    }

    fn eval_and_check_ready(
        &mut self,
        expr: &BoundExpr,
        caller: CallerContext,
        depth: usize,
        blocked: &mut SmallVec<[u32; 4]>,
        cycle_low: &mut Option<usize>,
    ) -> Option<EvalValue> {
        let val = match self.eval_expr(expr, caller, false, depth) {
            ExprOutcome::Ready(v) => v,
            ExprOutcome::Blocked(b) => {
                merge_blocked(blocked, b);
                return None;
            }
            ExprOutcome::Cycle(low) => {
                merge_cycle(cycle_low, low);
                return None;
            }
        };
        if let Err(wait) = self.ensure_value_ready(&val) {
            match wait {
                ExprOutcome::Blocked(b) => merge_blocked(blocked, b),
                ExprOutcome::Cycle(low) => merge_cycle(cycle_low, low),
                ExprOutcome::Ready(_) => {}
            }
            return None;
        }
        Some(val)
    }

    fn wait_outcome(blocked: SmallVec<[u32; 4]>, cycle_low: Option<usize>) -> Option<ExprOutcome> {
        if !blocked.is_empty() {
            return Some(ExprOutcome::Blocked(blocked));
        }
        cycle_low.map(ExprOutcome::Cycle)
    }

    fn cycle_low_of(&self, fid: u32) -> Option<usize> {
        match self.state_of(fid) {
            STATE_EVALUATING => self.call_stack.iter().rposition(|&f| f == fid),
            STATE_SCC_WAIT => self.scc_low.get(&fid).copied(),
            _ => None,
        }
    }

    fn record_fid_wait(
        &self,
        fid: u32,
        blocked: &mut SmallVec<[u32; 4]>,
        cycle_low: &mut Option<usize>,
    ) {
        match self.state_of(fid) {
            STATE_EVALUATING | STATE_SCC_WAIT => {
                if let Some(low) = self.cycle_low_of(fid) {
                    merge_cycle(cycle_low, low);
                }
            }
            STATE_DIRTY if !blocked.contains(&fid) => {
                blocked.push(fid);
            }
            _ => {}
        }
    }

    fn ensure_value_ready(&self, val: &EvalValue) -> Result<(), ExprOutcome> {
        let EvalValue::Reference(r) = val else {
            return Ok(());
        };
        match r {
            ResolvedRef::Invalid(_) => Ok(()),
            ResolvedRef::Cell { sheet, row, col } => {
                let target_key = (*sheet, *row, *col);
                let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
                let mut cycle_low: Option<usize> = None;
                let fid_opt = self.cell_to_fid.get(&target_key).copied().or_else(|| {
                    self.spills
                        .anchor_of_cell(target_key)
                        .and_then(|anchor| self.cell_to_fid.get(&anchor).copied())
                });
                if let Some(fid) = fid_opt {
                    self.record_fid_wait(fid, &mut blocked, &mut cycle_low);
                }
                if let Some(anchors) = self.cycle_shadow_watch.get(&target_key) {
                    for &anchor in anchors {
                        if let Some(&fid) = self.cell_to_fid.get(&anchor) {
                            self.record_fid_wait(fid, &mut blocked, &mut cycle_low);
                        }
                    }
                }
                if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
                    return Err(wait);
                }
                Ok(())
            }
            ResolvedRef::Area {
                sheet,
                first_row,
                last_row,
                first_col,
                last_col,
            } => self.check_span_ready(*sheet, first_row.0, last_row.0, first_col.0, last_col.0),
            ResolvedRef::FullCols {
                sheet,
                first_col,
                last_col,
            } => {
                let Some(s) = self.workbook.sheet(*sheet) else {
                    return Ok(());
                };
                let (Some(r1), Some(r2)) = (
                    s.row_axis().pos_to_id(0),
                    s.row_axis()
                        .pos_to_id(s.row_axis().live_len().saturating_sub(1)),
                ) else {
                    return Ok(());
                };
                self.check_span_ready(*sheet, r1, r2, first_col.0, last_col.0)
            }
            ResolvedRef::FullRows {
                sheet,
                first_row,
                last_row,
            } => {
                let Some(s) = self.workbook.sheet(*sheet) else {
                    return Ok(());
                };
                let (Some(c1), Some(c2)) = (
                    s.col_axis().pos_to_id(0),
                    s.col_axis()
                        .pos_to_id(s.col_axis().live_len().saturating_sub(1)),
                ) else {
                    return Ok(());
                };
                self.check_span_ready(*sheet, first_row.0, last_row.0, c1, c2)
            }
        }
    }

    fn check_span_ready(
        &self,
        sheet_id: SheetId,
        first_row: u64,
        last_row: u64,
        first_col: u64,
        last_col: u64,
    ) -> Result<(), ExprOutcome> {
        if self.non_clean_in_col.is_empty() {
            return Ok(());
        }
        let Some(sheet) = self.workbook.sheet(sheet_id) else {
            return Ok(());
        };
        let Some((r1_ord, r2_ord)) = live_span_positions(sheet.row_axis(), first_row, last_row)
        else {
            return Ok(());
        };

        let has_sheet_spills = self.spills.has_spills_on_sheet(sheet_id);
        let mut active_cols: SmallVec<[ColId; 4]> = SmallVec::new();
        let mut shadow_cols: SmallVec<[ColId; 4]> = SmallVec::new();
        sheet
            .col_axis()
            .for_each_live_in_range(first_col, last_col, |run| {
                for raw_c in run.iter() {
                    let col = ColId(raw_c);
                    if self
                        .non_clean_in_col
                        .get(&(sheet_id, col))
                        .copied()
                        .unwrap_or(0)
                        > 0
                    {
                        active_cols.push(col);
                    }
                    if has_sheet_spills && self.spills.has_shadows_in_col(sheet_id, col) {
                        shadow_cols.push(col);
                    }
                }
            });
        if active_cols.is_empty() && shadow_cols.is_empty() && self.cycle_shadow_watch.is_empty() {
            return Ok(());
        }

        let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
        let mut cycle_low: Option<usize> = None;
        if !active_cols.is_empty() {
            sheet
                .row_axis()
                .for_each_live_in_range(first_row, last_row, |run| {
                    for raw_r in run.iter() {
                        let row = RowId(raw_r);
                        for &col in &active_cols {
                            if let Some(&fid) = self.cell_to_fid.get(&(sheet_id, row, col)) {
                                self.record_fid_wait(fid, &mut blocked, &mut cycle_low);
                            }
                        }
                    }
                });
        }

        if !shadow_cols.is_empty() {
            for col in shadow_cols {
                self.spills
                    .for_each_shadow_in_col_span(sheet, col, r1_ord, r2_ord, |row_id, _| {
                        if let Some(anchor) = self.spills.anchor_of_cell((sheet_id, row_id, col))
                            && let Some(&fid) = self.cell_to_fid.get(&anchor)
                        {
                            self.record_fid_wait(fid, &mut blocked, &mut cycle_low);
                        }
                    });
            }
        }

        if !self.cycle_shadow_watch.is_empty()
            && let Some((c1_ord, c2_ord)) =
                live_span_positions(sheet.col_axis(), first_col, last_col)
        {
            let start = (sheet_id, RowId(0), ColId(0));
            let end = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
            for (&(_, cell_row, cell_col), anchors) in self.cycle_shadow_watch.range(start..=end) {
                let Some(r_pos) = sheet.row_axis().id_to_pos(cell_row.0) else {
                    continue;
                };
                if r_pos < r1_ord || r_pos > r2_ord {
                    continue;
                }
                let Some(c_pos) = sheet.col_axis().id_to_pos(cell_col.0) else {
                    continue;
                };
                if c_pos < c1_ord || c_pos > c2_ord {
                    continue;
                }
                for &anchor in anchors {
                    if let Some(&fid) = self.cell_to_fid.get(&anchor) {
                        self.record_fid_wait(fid, &mut blocked, &mut cycle_low);
                    }
                }
            }
        }

        if let Some(wait) = Self::wait_outcome(blocked, cycle_low) {
            return Err(wait);
        }
        Ok(())
    }
}

fn workbook_fallback_sheet(workbook: &Workbook<BoundFormula>) -> SheetId {
    workbook
        .live_sheets()
        .next()
        .or_else(|| workbook.sheets().first())
        .map_or(SheetId(0), Sheet::id)
}

const fn merge_cycle(dst: &mut Option<usize>, pos: usize) {
    *dst = Some(match *dst {
        Some(cur) if cur <= pos => cur,
        _ => pos,
    });
}

fn merge_blocked(dst: &mut SmallVec<[u32; 4]>, src: SmallVec<[u32; 4]>) {
    if src.is_empty() {
        return;
    }
    if dst.is_empty() {
        *dst = src;
        return;
    }
    if dst.len().saturating_add(src.len()) <= 32 {
        for f in src {
            if !dst.contains(&f) {
                dst.push(f);
            }
        }
        return;
    }
    let mut seen: FxSet<u32> = FxSet::with_capacity_and_hasher(
        dst.len().saturating_add(src.len()),
        BuildHasherDefault::default(),
    );
    for &f in dst.iter() {
        seen.insert(f);
    }
    for f in src {
        if seen.insert(f) {
            dst.push(f);
        }
    }
}

/// Constructs `=ROUND(J{r}*K{r}*(1-L{r}-$V$1), 2)` directly without string
/// lexing during cold-start sample generation.
fn build_sample_row_formula(row: RowId) -> BoundFormula {
    let cell_ref = |r: RowId, c: ColId, abs: bool| {
        BoundExpr::Reference(BoundRef::Cell {
            sheet: None,
            missing_sheet: None,
            row: r,
            col: c,
            abs_col: abs,
            abs_row: abs,
        })
    };
    let j = cell_ref(row, ColId(9), false);
    let k = cell_ref(row, ColId(10), false);
    let l = cell_ref(row, ColId(11), false);
    let v1 = cell_ref(RowId(0), ColId(21), true);

    let jk = BoundExpr::Binary {
        op: BinaryOp::Mul,
        left: Box::new(j),
        right: Box::new(k),
    };
    let one_minus_l = BoundExpr::Binary {
        op: BinaryOp::Sub,
        left: Box::new(BoundExpr::Literal(Literal::Number(1.0))),
        right: Box::new(l),
    };
    let factor = BoundExpr::Paren(Box::new(BoundExpr::Binary {
        op: BinaryOp::Sub,
        left: Box::new(one_minus_l),
        right: Box::new(v1),
    }));
    let product = BoundExpr::Binary {
        op: BinaryOp::Mul,
        left: Box::new(jk),
        right: Box::new(factor),
    };
    BoundFormula {
        mode: FormulaMode::Dynamic,
        root: BoundExpr::Call {
            name: String::from("ROUND"),
            args: vec![product, BoundExpr::Literal(Literal::Number(2.0))],
        },
    }
}

fn push_precedent_cell(
    workbook: &Workbook<BoundFormula>,
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
    seen: &mut BTreeSet<WorkbookCellKey>,
    out: &mut Vec<PrecedentTrace>,
    limit: usize,
) {
    if out.len() >= limit || !seen.insert((sheet_id, row, col)) {
        return;
    }
    let Some(sheet) = workbook.live_sheet(sheet_id) else {
        return;
    };
    let (Some(r_pos), Some(c_pos)) = (
        sheet.row_axis().id_to_pos(row.0),
        sheet.col_axis().id_to_pos(col.0),
    ) else {
        return;
    };
    let mut address = String::new();
    CellAddr {
        col: c_pos,
        row: r_pos,
        abs_col: false,
        abs_row: false,
    }
    .write_a1(&mut address);
    let val = sheet.value_by_id(row, col);
    let mut display = String::new();
    sheet.format_value(val, row, col, &mut display);
    let error = match val {
        Value::Error(e) => Some(String::from(e.as_str())),
        Value::Array(_) => Some(String::from(CalcError::Spill.as_str())),
        _ => None,
    };
    out.push(PrecedentTrace {
        sheet_name: String::from(sheet.name()),
        address,
        display,
        error,
    });
}

fn collect_bound_precedents(
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
    expr: &BoundExpr,
    seen: &mut BTreeSet<WorkbookCellKey>,
    out: &mut Vec<PrecedentTrace>,
    limit: usize,
) {
    if out.len() >= limit {
        return;
    }
    match expr {
        BoundExpr::Reference(r) => match r {
            BoundRef::Cell {
                sheet,
                missing_sheet,
                row,
                col,
                ..
            } if missing_sheet.is_none() => {
                push_precedent_cell(
                    workbook,
                    sheet.unwrap_or(host_sheet),
                    *row,
                    *col,
                    seen,
                    out,
                    limit,
                );
            }
            BoundRef::Area {
                sheet,
                missing_sheet,
                first_row,
                first_col,
                last_row,
                last_col,
                ..
            } if missing_sheet.is_none() => {
                let target_sid = sheet.unwrap_or(host_sheet);
                let Some(s) = workbook.live_sheet(target_sid) else {
                    return;
                };
                if let (Some((r0, r1)), Some((c0, c1))) = (
                    live_span_positions(s.row_axis(), first_row.0, last_row.0),
                    live_span_positions(s.col_axis(), first_col.0, last_col.0),
                ) {
                    // Prioritize errored cells inside the area, then corner cells.
                    let mut corners_added = 0_usize;
                    let mut r = r0;
                    while r <= r1 && out.len() < limit && corners_added < 64 {
                        if let Some(rid) = s.row_axis().pos_to_id(r) {
                            let mut c = c0;
                            while c <= c1 && out.len() < limit && corners_added < 64 {
                                if let Some(cid) = s.col_axis().pos_to_id(c) {
                                    let val = s.value_by_id(RowId(rid), ColId(cid));
                                    if matches!(val, Value::Error(_) | Value::Array(_))
                                        || (r == r0 && c == c0)
                                        || (r == r1 && c == c1)
                                    {
                                        push_precedent_cell(
                                            workbook,
                                            target_sid,
                                            RowId(rid),
                                            ColId(cid),
                                            seen,
                                            out,
                                            limit,
                                        );
                                    }
                                }
                                c = c.saturating_add(1);
                                corners_added = corners_added.saturating_add(1);
                            }
                        }
                        r = r.saturating_add(1);
                    }
                }
            }
            _ => {}
        },
        BoundExpr::Paren(inner) | BoundExpr::SpillRef(inner) => {
            collect_bound_precedents(workbook, host_sheet, inner, seen, out, limit);
        }
        BoundExpr::Unary { expr: inner, .. } => {
            collect_bound_precedents(workbook, host_sheet, inner, seen, out, limit);
        }
        BoundExpr::Binary { left, right, .. } => {
            collect_bound_precedents(workbook, host_sheet, left, seen, out, limit);
            collect_bound_precedents(workbook, host_sheet, right, seen, out, limit);
        }
        BoundExpr::Call { args, .. } => {
            for arg in args {
                if out.len() >= limit {
                    break;
                }
                collect_bound_precedents(workbook, host_sheet, arg, seen, out, limit);
            }
        }
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => {}
    }
}

#[allow(dead_code)]
const _KEEP_SHEET_TYPE: Option<&Sheet> = None;

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::vec;
    use alloc::vec::Vec;

    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::Op;
    use reticula_core::value::{CalcError, Value};
    use smallvec::SmallVec;

    use super::{
        FormulaEngine, RecalcStatus, TransientEvalError, TransientEvalValue, merge_blocked,
    };
    use crate::value_eval::EvalScalar;

    #[test]
    fn layer0_functions_cycles_short_circuit_and_structural_recalc_work() {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);

        eng.edit_cell_raw(s, RowId(0), ColId(0), "10").unwrap();
        eng.edit_cell_raw(s, RowId(1), ColId(0), "20").unwrap();
        eng.edit_cell_raw(s, RowId(2), ColId(0), "hello").unwrap();
        eng.edit_cell_raw(s, RowId(3), ColId(0), "TRUE").unwrap();

        eng.edit_cell_raw(s, RowId(0), ColId(1), "=SUM(A1:A4)")
            .unwrap();
        eng.edit_cell_raw(s, RowId(1), ColId(1), "=AVERAGE(A1:A4)")
            .unwrap();
        eng.edit_cell_raw(s, RowId(2), ColId(1), "=COUNT(A1:A4)")
            .unwrap();
        eng.edit_cell_raw(s, RowId(3), ColId(1), "=COUNTA(A1:A4)")
            .unwrap();
        eng.edit_cell_raw(
            s,
            RowId(4),
            ColId(1),
            "=IF(B1>25,UPPER(TRIM(\"  ok  now \")),1/0)",
        )
        .unwrap();
        eng.edit_cell_raw(s, RowId(5), ColId(1), "=ROW()").unwrap();
        eng.recalc_all();

        let sh = eng.workbook().sheet(s).unwrap();
        assert_eq!(sh.value_by_id(RowId(0), ColId(1)), Value::Number(30.0));
        assert_eq!(sh.value_by_id(RowId(1), ColId(1)), Value::Number(15.0));
        assert_eq!(sh.value_by_id(RowId(2), ColId(1)), Value::Number(2.0));
        assert_eq!(sh.value_by_id(RowId(3), ColId(1)), Value::Number(4.0));
        assert_eq!(
            eng.inspect_cell(s, RowId(4), ColId(1)).unwrap().display,
            "OK NOW"
        );
        assert_eq!(sh.value_by_id(RowId(5), ColId(1)), Value::Number(6.0));

        // Insert 1 row at top: `ROW()` at `RowId(5)` updates from 6 to 7!
        let ins = eng.allocator_mut().alloc_run(1).unwrap();
        eng.apply_op(Op::InsertRows {
            sheet: s,
            after: None,
            ids: ins,
        })
        .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s)
                .unwrap()
                .value_by_id(RowId(5), ColId(1)),
            Value::Number(7.0)
        );

        // Mutual cycle on `RowId(0)` (now visible row 2 after the top insert):
        // `C2 = D2 + 1` and `D2 = C2 + 1` -> both `#CYCLE!`, while
        // `E2 = IFERROR(C2, 99)` catches `#CYCLE!` -> `99`!
        eng.edit_cell_raw(s, RowId(0), ColId(2), "=D2+1").unwrap();
        eng.edit_cell_raw(s, RowId(0), ColId(3), "=C2+1").unwrap();
        eng.edit_cell_raw(s, RowId(0), ColId(4), "=IFERROR(C2,99)")
            .unwrap();
        eng.recalc_all();
        let sh2 = eng.workbook().sheet(s).unwrap();
        assert_eq!(
            sh2.value_by_id(RowId(0), ColId(2)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(
            sh2.value_by_id(RowId(0), ColId(3)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(sh2.value_by_id(RowId(0), ColId(4)), Value::Number(99.0));
    }

    #[test]
    fn upgraded_sample_cached_values_match_full_recalc_and_time_slicing_works() {
        let mut eng = FormulaEngine::open_sample(42, 500);
        let s = SheetId(0);

        // Cold-start cached values must match full recalculation from scratch bit-for-bit!
        let cold_digest = eng.workbook().logical_digest();
        let fresh = FormulaEngine::from_workbook_recalc_all(eng.workbook().clone());
        assert_eq!(fresh.workbook().logical_digest(), cold_digest);

        // Changing `V1` from `0%` to `5%` triggers `499 + 1 = 500` formulas!
        eng.edit_cell_raw(s, RowId(0), ColId(21), "5%").unwrap();
        let mut steps = 0;
        let total = loop {
            steps += 1;
            match eng.recalc_step(120) {
                RecalcStatus::InProgress { .. } => {}
                RecalcStatus::Complete { recalculated } => break recalculated,
            }
        };
        assert_eq!(total, 500);
        assert!(steps >= 4);

        // Delete row 3 (`RowId(2)`): `V2` (`RowId(1)`) stays live and shrinks
        // `SUM(M2:M500)` -> `SUM(M2:M499)` automatically!
        let before_del = eng
            .workbook()
            .sheet(s)
            .unwrap()
            .value_by_id(RowId(1), ColId(21));
        eng.apply_op(Op::DeleteRows {
            sheet: s,
            ids: vec![IdRun::new(2, 1).unwrap()],
        })
        .unwrap();
        eng.recalc_all();
        let after_del = eng
            .workbook()
            .sheet(s)
            .unwrap()
            .value_by_id(RowId(1), ColId(21));
        assert_ne!(before_del, after_del);
        assert_eq!(
            eng.inspect_cell(s, RowId(1), ColId(21)).unwrap().raw,
            "=SUM(M2:M499)"
        );
    }

    #[test]
    fn recalculation_and_volatile_formulas_never_perturb_state_digest() {
        let mut eng = FormulaEngine::open_sample(7, 40);
        let s = SheetId(0);
        eng.edit_cell_raw(s, RowId(0), ColId(0), "=SUM(J2:J10)+RAND()")
            .unwrap();
        let before_recalc = eng.workbook().state_digest();
        let before_pop = eng.workbook().populated_cell_count();
        assert_eq!(
            eng.workbook().content_digest(),
            eng.workbook().compute_content_digest_from_scratch()
        );

        eng.recalc_all();
        assert_eq!(eng.workbook().state_digest(), before_recalc);
        assert_eq!(eng.workbook().populated_cell_count(), before_pop);
        assert_eq!(
            eng.workbook().content_digest(),
            eng.workbook().compute_content_digest_from_scratch()
        );
    }

    #[test]
    fn merge_blocked_preserves_order_and_deduplicates_large_inputs() {
        let mut dst: SmallVec<[u32; 4]> = (0..64).collect();
        let src: SmallVec<[u32; 4]> = (32..128).chain(10..40).collect();
        merge_blocked(&mut dst, src);
        let expected: Vec<u32> = (0..128).collect();
        assert_eq!(dst.as_slice(), expected.as_slice());
    }

    #[test]
    fn blocked_yield_resumes_cleanly_and_matches_full_recalc() {
        let mut sliced = FormulaEngine::new();
        let s = SheetId(0);
        sliced.edit_cell_raw(s, RowId(0), ColId(2), "1").unwrap();
        for r in 0..20 {
            sliced
                .edit_cell_raw(s, RowId(r), ColId(1), "=$C$1+1")
                .unwrap();
        }
        // A1 sorts ahead of B1:B20, so evaluating A1 blocks on 20 dirty formulas while done == 0.
        sliced
            .edit_cell_raw(s, RowId(0), ColId(0), "=SUM(B1:B20)")
            .unwrap();
        sliced.recalc_all();

        sliced.edit_cell_raw(s, RowId(0), ColId(2), "5").unwrap();
        let mut yielded_on_zero_done = false;
        let mut slices = 0_u32;
        loop {
            slices += 1;
            let mut ticks = 0_u32;
            let status = sliced.recalc_with_budget(8, |_| {
                ticks += 1;
                ticks >= 1
            });
            match status {
                RecalcStatus::InProgress { done, .. } => {
                    if done == 0 {
                        yielded_on_zero_done = true;
                    }
                    assert!(slices < 500, "recalc must make steady progress");
                }
                RecalcStatus::Complete { .. } => break,
            }
        }
        assert!(yielded_on_zero_done);

        let fresh = FormulaEngine::from_workbook_recalc_all(sliced.workbook().clone());
        assert_eq!(
            sliced.workbook().logical_digest(),
            fresh.workbook().logical_digest()
        );
    }

    #[test]
    fn e14_e24_move_cells_excel_parity_and_undo() {
        use alloc::string::String;
        use reticula_core::ops::FormulaMode;

        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let s1 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Sheet2"),
            after: Some(s0),
        })
        .unwrap();

        // Populate source A1:A3 and external reference targets.
        eng.edit_cell_raw(s0, RowId(0), ColId(2), "5").unwrap(); // C1
        eng.edit_cell_raw(s0, RowId(0), ColId(0), "=C1+10").unwrap(); // A1 (15)
        eng.edit_cell_raw(s0, RowId(1), ColId(0), "=A1*2").unwrap(); // A2 (30, internal ref)
        eng.edit_cell_raw(s0, RowId(2), ColId(0), "7").unwrap(); // A3
        eng.edit_cell_raw(s0, RowId(1), ColId(1), "4").unwrap(); // B2
        eng.edit_cell_raw(s0, RowId(1), ColId(3), "99").unwrap(); // D2 (overwritten target)

        // External formulas watching src, partial range, full src range, and overwritten dst.
        eng.edit_cell_raw(s0, RowId(0), ColId(4), "=A1+1").unwrap(); // E1 -> becomes =D1+1
        eng.edit_cell_raw(s0, RowId(1), ColId(4), "=SUM(A1:B2)")
            .unwrap(); // E2 -> stays =SUM(A1:B2) (partial overlap)
        eng.edit_cell_raw(s0, RowId(2), ColId(4), "=SUM(A1:A3)")
            .unwrap(); // E3 -> becomes =SUM(D1:D3) (endpoints in src)
        eng.edit_cell_raw(s0, RowId(0), ColId(5), "=D2+100")
            .unwrap(); // F1 -> becomes =#REF!+100
        eng.recalc_all();

        let digest_before = eng.workbook().state_digest();

        // Move A1:A3 -> D1:D3 within Sheet1.
        let inv1 = eng
            .apply_op(Op::MoveCells {
                src_sheet: s0,
                src_rows: vec![RowId(0), RowId(1), RowId(2)],
                src_cols: vec![ColId(0)],
                dst_sheet: s0,
                dst_rows: vec![RowId(0), RowId(1), RowId(2)],
                dst_cols: vec![ColId(3)],
            })
            .unwrap();
        eng.recalc_all();

        // Source vacated; moved formulas preserve external C1 and remap internal A1 -> D1.
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Empty
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(3)).unwrap().raw,
            "=C1+10"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(3)),
            Value::Number(15.0)
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(1), ColId(3)).unwrap().raw,
            "=D1*2"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(1), ColId(3)),
            Value::Number(30.0)
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(4)).unwrap().raw,
            "=D1+1"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(16.0)
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(1), ColId(4)).unwrap().raw,
            "=SUM(A1:B2)"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(1), ColId(4)),
            Value::Number(4.0)
        );
        assert_eq!(
            eng.inspect_cell(s0, RowId(2), ColId(4)).unwrap().raw,
            "=SUM(D1:D3)"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(2), ColId(4)),
            Value::Number(52.0) // 15 + 30 + 7
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(5)),
            Value::Error(CalcError::Ref)
        );
        assert!(
            eng.inspect_cell(s0, RowId(0), ColId(5))
                .unwrap()
                .raw
                .contains("#REF!")
        );

        // Named range pointing into src shifts under MoveCells as well!
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("TargetCell"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=D3",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(6), "=TargetCell+1")
            .unwrap(); // G1 -> 8
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(6)),
            Value::Number(8.0)
        );

        // Cross-sheet MoveCells: Sheet1!D1:D3 -> Sheet2!B5:B7.
        let inv_cross = eng
            .apply_op(Op::MoveCells {
                src_sheet: s0,
                src_rows: vec![RowId(0), RowId(1), RowId(2)],
                src_cols: vec![ColId(3)],
                dst_sheet: s1,
                dst_rows: vec![RowId(4), RowId(5), RowId(6)],
                dst_cols: vec![ColId(1)],
            })
            .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(4)).unwrap().raw,
            "=Sheet2!B5+1"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(16.0)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(6)),
            Value::Number(8.0)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s1)
                .unwrap()
                .value_by_id(RowId(4), ColId(1)),
            Value::Number(15.0)
        );

        // Undo cross-sheet move & undo TargetCell/G1 & undo inv1 -> exact baseline StateDigest!
        eng.apply_inverse(inv_cross).unwrap();
        eng.recalc_all();
        eng.edit_cell_raw(s0, RowId(0), ColId(6), "").unwrap();
        eng.apply_op(Op::DeleteNamedRange {
            scope: None,
            name: String::from("TargetCell"),
        })
        .unwrap();
        eng.apply_inverse(inv1).unwrap();
        eng.recalc_all();
        assert_eq!(eng.workbook().state_digest(), digest_before);
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(16.0)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(5)),
            Value::Number(199.0)
        );
    }

    #[test]
    fn e15_permute_content_excel_parity_and_undo() {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);

        eng.edit_cell_raw(s0, RowId(0), ColId(3), "5").unwrap(); // D1 (absolute anchor)
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "100").unwrap(); // B1 (stationary)
        eng.edit_cell_raw(s0, RowId(1), ColId(1), "200").unwrap(); // B2
        eng.edit_cell_raw(s0, RowId(2), ColId(1), "300").unwrap(); // B3

        eng.edit_cell_raw(s0, RowId(0), ColId(0), "30").unwrap(); // A1
        eng.edit_cell_raw(s0, RowId(1), ColId(0), "10").unwrap(); // A2
        eng.edit_cell_raw(s0, RowId(2), ColId(0), "20").unwrap(); // A3

        eng.edit_cell_raw(s0, RowId(0), ColId(2), "=B1*2+$D$1")
            .unwrap(); // C1 -> 205
        eng.edit_cell_raw(s0, RowId(1), ColId(2), "=B2*2+$D$1")
            .unwrap(); // C2 -> 405
        eng.edit_cell_raw(s0, RowId(2), ColId(2), "=B3*2+$D$1")
            .unwrap(); // C3 -> 605
        eng.edit_cell_raw(s0, RowId(0), ColId(4), "=C1+1").unwrap(); // E1 -> external, stays =C1+1
        eng.recalc_all();

        let digest_before = eng.workbook().state_digest();

        // Permute columns [A, C] across rows [1, 2, 3] (IDs 0, 1, 2) with row_perm [1, 2, 0].
        let inv = eng
            .apply_op(Op::PermuteContent {
                sheet: s0,
                rows: vec![RowId(0), RowId(1), RowId(2)],
                cols: vec![ColId(0), ColId(2)],
                row_perm: vec![1, 2, 0],
            })
            .unwrap();
        eng.recalc_all();

        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(10.0)
        );
        // Formula moved from row 1 -> row 0 shifts relative `B2` -> `B1` while keeping `$D$1` fixed!
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(2)).unwrap().raw,
            "=B1*2+$D$1"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(2)),
            Value::Number(205.0)
        );
        // External formula keeps original row anchor `C1`.
        assert_eq!(
            eng.inspect_cell(s0, RowId(0), ColId(4)).unwrap().raw,
            "=C1+1"
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(206.0)
        );

        eng.apply_inverse(inv).unwrap();
        eng.recalc_all();
        assert_eq!(eng.workbook().state_digest(), digest_before);
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(2)),
            Value::Number(205.0)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(1), ColId(2)),
            Value::Number(405.0)
        );
    }

    #[test]
    fn e22_spill_registry_hash_viewport_and_symmetric_collisions() {
        use alloc::string::String;
        use reticula_core::viewport::{
            CELL_FLAGS_AT, CELL_LEN, CELL_TEXT_LEN_AT, CELL_TEXT_START_AT, COL_COUNT_AT,
            FLAG_FORMULA, FLAG_SPILL, HEADER_LEN, ID_LEN, OVERFLOW_COUNT_AT, OVERFLOW_LEN, Page,
            ROW_COUNT_AT,
        };

        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);

        // 1. Clean 2x2 array spill + text spill + `#` + downstream Layer 0/1 consumers.
        eng.edit_cell_raw(s0, RowId(0), ColId(0), "={1,2;3,4}")
            .unwrap(); // A1:B2
        eng.edit_cell_raw(s0, RowId(0), ColId(5), "=A1:B2").unwrap(); // F1:G2 (range spill!)
        eng.edit_cell_raw(s0, RowId(0), ColId(7), "={\"hi\",\"there\"}")
            .unwrap(); // H1:I1 (text spill!)
        eng.edit_cell_raw(s0, RowId(3), ColId(0), "=SUM(A1#)")
            .unwrap(); // A4 -> 10
        eng.edit_cell_raw(s0, RowId(4), ColId(0), "=ROWS(A1#)")
            .unwrap(); // A5 -> 2
        eng.edit_cell_raw(s0, RowId(5), ColId(0), "=COLUMNS(A1#)")
            .unwrap(); // A6 -> 2
        eng.edit_cell_raw(s0, RowId(6), ColId(0), "=INDEX(A1#,2,2)")
            .unwrap(); // A7 -> 4
        eng.edit_cell_raw(s0, RowId(7), ColId(0), "=MATCH(3,A2:B2,0)")
            .unwrap(); // A8 -> 1
        eng.edit_cell_raw(s0, RowId(8), ColId(0), "=VLOOKUP(3,A1:B2,2,FALSE)")
            .unwrap(); // A9 -> 4
        eng.edit_cell_raw(s0, RowId(9), ColId(0), "=COUNTIF(A1:B2,\">1\")")
            .unwrap(); // A10 -> 3
        eng.edit_cell_raw(s0, RowId(10), ColId(0), "=COUNTBLANK(H1:I1)")
            .unwrap(); // A11 -> 0
        eng.edit_cell_raw(s0, RowId(11), ColId(0), "=SUMIF(A1:B2,\">1\")")
            .unwrap(); // A12 -> 9
        eng.edit_cell_raw(s0, RowId(12), ColId(0), "=AVERAGEIF(A1:B2,\">1\")")
            .unwrap(); // A13 -> 3
        eng.recalc_all();

        let s = eng.workbook().sheet(s0).unwrap();
        assert_eq!(s.value_by_id(RowId(0), ColId(0)), Value::Number(1.0));
        assert_eq!(s.value_by_id(RowId(0), ColId(1)), Value::Empty); // Sheet shadow untouched!
        assert_eq!(s.strings().len(), 1); // Only `"hi"` in H1 anchor; `"there"` NOT interned!
        assert_eq!(eng.spill_range((s0, RowId(0), ColId(0))), Some((2, 2)));
        assert_eq!(s.value_by_id(RowId(3), ColId(0)), Value::Number(10.0));
        assert_eq!(s.value_by_id(RowId(4), ColId(0)), Value::Number(2.0));
        assert_eq!(s.value_by_id(RowId(5), ColId(0)), Value::Number(2.0));
        assert_eq!(s.value_by_id(RowId(6), ColId(0)), Value::Number(4.0));
        assert_eq!(s.value_by_id(RowId(7), ColId(0)), Value::Number(1.0));
        assert_eq!(s.value_by_id(RowId(8), ColId(0)), Value::Number(4.0));
        assert_eq!(s.value_by_id(RowId(9), ColId(0)), Value::Number(3.0));
        assert_eq!(s.value_by_id(RowId(10), ColId(0)), Value::Number(0.0));
        assert_eq!(s.value_by_id(RowId(11), ColId(0)), Value::Number(9.0));
        assert_eq!(s.value_by_id(RowId(12), ColId(0)), Value::Number(3.0));

        // Shadow cell inspection & viewport FLAG_SPILL verification.
        let b2_inspect = eng.inspect_cell(s0, RowId(1), ColId(1)).unwrap();
        assert_eq!(b2_inspect.raw, "");
        assert_eq!(b2_inspect.display, "4");
        assert!(!b2_inspect.is_formula);

        let mut scratch = String::new();
        let bytes = eng
            .encode_viewport_page(s0, Page { row: 0, col: 0 }, 1, &mut scratch)
            .unwrap();
        let rows = usize::from(u16::from_le_bytes(
            bytes[ROW_COUNT_AT..ROW_COUNT_AT + 2].try_into().unwrap(),
        ));
        let cols = usize::from(u16::from_le_bytes(
            bytes[COL_COUNT_AT..COL_COUNT_AT + 2].try_into().unwrap(),
        ));
        let overflow_count = usize::from(u16::from_le_bytes(
            bytes[OVERFLOW_COUNT_AT..OVERFLOW_COUNT_AT + 2]
                .try_into()
                .unwrap(),
        ));
        let cells_at = HEADER_LEN + (rows + cols) * ID_LEN;
        let text_at = cells_at + rows * cols * CELL_LEN + overflow_count * OVERFLOW_LEN;
        let page_text = core::str::from_utf8(&bytes[text_at..]).unwrap();
        let page_units: Vec<u16> = page_text.encode_utf16().collect();
        let read_cell = |r: usize, c: usize| -> (String, u16) {
            let rec = cells_at + (r * cols + c) * CELL_LEN;
            let start = u32::from_le_bytes(
                bytes[rec + CELL_TEXT_START_AT..rec + CELL_TEXT_START_AT + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let len = usize::from(u16::from_le_bytes(
                bytes[rec + CELL_TEXT_LEN_AT..rec + CELL_TEXT_LEN_AT + 2]
                    .try_into()
                    .unwrap(),
            ));
            let flags = u16::from_le_bytes(
                bytes[rec + CELL_FLAGS_AT..rec + CELL_FLAGS_AT + 2]
                    .try_into()
                    .unwrap(),
            );
            (
                String::from_utf16(&page_units[start..start + len]).unwrap(),
                flags,
            )
        };
        let (a1_txt, a1_flags) = read_cell(0, 0);
        let (b2_txt, b2_flags) = read_cell(1, 1);
        let (i1_txt, i1_flags) = read_cell(0, 8);
        assert_eq!(a1_txt, "1");
        assert_ne!(a1_flags & FLAG_FORMULA, 0);
        assert_eq!(a1_flags & FLAG_SPILL, 0);
        assert_eq!(b2_txt, "4");
        assert_eq!(b2_flags & FLAG_FORMULA, 0);
        assert_ne!(b2_flags & FLAG_SPILL, 0);
        assert_eq!(i1_txt, "there");
        assert_ne!(i1_flags & FLAG_SPILL, 0);
        assert_eq!(eng.workbook().sheet(s0).unwrap().strings().len(), 1);

        // 2. Static obstacle blocks spill and clearing obstacle pops it open.
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "block").unwrap(); // B1
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Spill)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(0)),
            Value::Error(CalcError::Ref)
        );
        assert!(eng.spills().shadow_value(s0, RowId(1), ColId(1)).is_none());

        eng.edit_cell_raw(s0, RowId(0), ColId(1), "").unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(1.0)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(0)),
            Value::Number(10.0)
        );

        // Structural insert inside spill & compact_local_garbage keep active spill intact.
        let ins_r = eng.allocator_mut().alloc_run(1).unwrap();
        eng.apply_op(Op::InsertRows {
            sheet: s0,
            after: Some(RowId(0)),
            ids: ins_r,
        })
        .unwrap();
        eng.recalc_all();
        eng.compact_local_garbage();
        assert_eq!(eng.spill_range((s0, RowId(0), ColId(0))), Some((2, 2)));

        // 3. Symmetric 3-way chain collision under all 6 insertion/evaluation orders!
        // P at D1={1;2} (D1,D2); Q at B2={10,20,30;40,50,60} (B2..D3); R at A3={100,200} (A3,B3).
        let p = (RowId(0), ColId(3), "={1;2}");
        let q = (RowId(1), ColId(1), "={10,20,30;40,50,60}");
        let r = (RowId(2), ColId(0), "={100,200}");
        let orders = [
            [p, q, r],
            [p, r, q],
            [q, p, r],
            [q, r, p],
            [r, p, q],
            [r, q, p],
        ];
        for order in orders {
            let mut ce = FormulaEngine::new();
            for &(row, col, raw) in &order {
                ce.edit_cell_raw(s0, row, col, raw).unwrap();
                ce.recalc_all();
            }
            let cs = ce.workbook().sheet(s0).unwrap();
            assert_eq!(
                cs.value_by_id(RowId(0), ColId(3)),
                Value::Error(CalcError::Spill)
            );
            assert_eq!(
                cs.value_by_id(RowId(1), ColId(1)),
                Value::Error(CalcError::Spill)
            );
            assert_eq!(
                cs.value_by_id(RowId(2), ColId(0)),
                Value::Error(CalcError::Spill)
            );

            // Way A: Deleting Q pops BOTH P and R open.
            let mut del_q = ce.clone();
            del_q.edit_cell_raw(s0, RowId(1), ColId(1), "").unwrap();
            del_q.recalc_all();
            let ds = del_q.workbook().sheet(s0).unwrap();
            assert_eq!(ds.value_by_id(RowId(0), ColId(3)), Value::Number(1.0));
            assert_eq!(ds.value_by_id(RowId(2), ColId(0)), Value::Number(100.0));
            assert!(
                del_q
                    .spills()
                    .shadow_value(s0, RowId(1), ColId(3))
                    .is_some()
            );

            // Way B: Shrinking Q to scalar opens BOTH P and R.
            let mut shrink_q = ce.clone();
            shrink_q
                .edit_cell_raw(s0, RowId(1), ColId(1), "=99")
                .unwrap();
            shrink_q.recalc_all();
            let ss = shrink_q.workbook().sheet(s0).unwrap();
            assert_eq!(ss.value_by_id(RowId(0), ColId(3)), Value::Number(1.0));
            assert_eq!(ss.value_by_id(RowId(1), ColId(1)), Value::Number(99.0));
            assert_eq!(ss.value_by_id(RowId(2), ColId(0)), Value::Number(100.0));

            // Way C: Statically blocking Q on C2 (not overlapping P or R!) opens BOTH P and R.
            let mut static_q = ce;
            static_q
                .edit_cell_raw(s0, RowId(1), ColId(2), "stop")
                .unwrap();
            static_q.recalc_all();
            let sq = static_q.workbook().sheet(s0).unwrap();
            assert_eq!(sq.value_by_id(RowId(0), ColId(3)), Value::Number(1.0));
            assert_eq!(
                sq.value_by_id(RowId(1), ColId(1)),
                Value::Error(CalcError::Spill)
            );
            assert_eq!(sq.value_by_id(RowId(2), ColId(0)), Value::Number(100.0));
        }
    }

    #[test]
    fn named_ranges_scoping_cycles_e25_and_digest() {
        use alloc::string::String;
        use reticula_core::ops::FormulaMode;

        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        let s1 = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: s1,
            name: String::from("Sheet2"),
            after: Some(s0),
        })
        .unwrap();

        eng.edit_cell_raw(s0, RowId(0), ColId(0), "=Rate").unwrap(); // A1 (#NAME? initially)
        eng.edit_cell_raw(s1, RowId(0), ColId(0), "=Rate").unwrap(); // Sheet2!A1
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "=Sheet2!LocalOnly")
            .unwrap(); // B1 (#NAME? initially)
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Name)
        );
        let baseline_digest = eng.workbook().state_digest();

        // 1. Workbook-scoped name changes digest and resolves `#NAME?`.
        let rate_global = crate::binder::BoundFormula::parse_and_bind(
            "=0.1",
            FormulaMode::Dynamic,
            eng.workbook(),
            s0,
        )
        .unwrap();
        let inv_rate_global = eng
            .apply_op(Op::DefineNamedRange {
                scope: None,
                name: String::from("Rate"),
                formula: rate_global,
                comment: Some(String::from("Global rate")),
                hidden: false,
            })
            .unwrap();
        eng.recalc_all();
        assert_ne!(eng.workbook().state_digest(), baseline_digest);
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.1)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s1)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.1)
        );

        // Deleting restores zero named-range digest contribution!
        eng.apply_inverse(inv_rate_global.clone()).unwrap();
        eng.recalc_all();
        assert_eq!(eng.workbook().state_digest(), baseline_digest);
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Name)
        );

        // Re-define global, then shadow on Sheet1 only.
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("Rate"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=0.1",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.apply_op(Op::DefineNamedRange {
            scope: Some(s0),
            name: String::from("Rate"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=0.25",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.25)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s1)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.1)
        );

        // 2. Scoped name `Sheet2!LocalOnly` + DeleteSheet -> #REF! -> UndeleteSheet recovery.
        eng.edit_cell_raw(s1, RowId(0), ColId(2), "7").unwrap();
        eng.apply_op(Op::DefineNamedRange {
            scope: Some(s1),
            name: String::from("LocalOnly"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=C1*2",
                FormulaMode::Dynamic,
                eng.workbook(),
                s1,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Number(14.0)
        );

        let inv_del_s1 = eng.apply_op(Op::DeleteSheet { sheet: s1 }).unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Error(CalcError::Ref)
        );
        eng.apply_inverse(inv_del_s1).unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Number(14.0)
        );

        // 3. Chained names TaxRate1 -> TaxRate2 -> D1 and invalidation on editing D1.
        eng.edit_cell_raw(s0, RowId(0), ColId(3), "4").unwrap(); // D1
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("TaxRate1"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=Sheet1!D1+1",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("TaxRate2"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=TaxRate1*3",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(4), "=TaxRate2")
            .unwrap(); // E1 -> (4+1)*3=15
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(15.0)
        );
        eng.edit_cell_raw(s0, RowId(0), ColId(3), "9").unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(30.0)
        );

        // 4. Cyclic named ranges & mixed cell+name cycles yield #CYCLE!.
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("CycleOne"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=CycleTwo+1",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("CycleTwo"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=CycleOne+1",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(5), "=CycleOne")
            .unwrap();
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("CycleCell"),
            formula: crate::binder::BoundFormula::parse_and_bind(
                "=Sheet1!G1",
                FormulaMode::Dynamic,
                eng.workbook(),
                s0,
            )
            .unwrap(),
            comment: None,
            hidden: false,
        })
        .unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(6), "=CycleCell")
            .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(5)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(6)),
            Value::Error(CalcError::Cycle)
        );

        // 5. E25 MoveRows updates named range and undo restores exact StateDigest.
        let before_move = eng.workbook().state_digest();
        let inv_mv = eng
            .apply_op(Op::MoveRows {
                sheet: s0,
                ids: vec![IdRun::new(0, 1).unwrap()],
                after: Some(RowId(4)),
            })
            .unwrap();
        eng.recalc_all();
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(4)),
            Value::Number(30.0)
        );
        eng.apply_inverse(inv_mv).unwrap();
        eng.recalc_all();
        assert_eq!(eng.workbook().state_digest(), before_move);
    }

    #[test]
    fn e22_spill_shadow_cycles_and_no_requeue_loops() {
        let s0 = SheetId(0);

        // 1. Two-cell shadow-mediated cycle (`A1 = =B1:B2`, `B1 = =A2:A3`) + observer `C1`.
        let mut eng = FormulaEngine::new();
        eng.edit_cell_raw(s0, RowId(0), ColId(0), "=B1:B2").unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "=A2:A3").unwrap();
        eng.edit_cell_raw(s0, RowId(0), ColId(2), "=A2+1").unwrap();
        eng.recalc_all();

        let fresh = FormulaEngine::from_workbook_recalc_all(eng.workbook().clone());
        for col in 0..3u64 {
            assert_eq!(
                eng.workbook()
                    .sheet(s0)
                    .unwrap()
                    .value_by_id(RowId(0), ColId(col)),
                fresh
                    .workbook()
                    .sheet(s0)
                    .unwrap()
                    .value_by_id(RowId(0), ColId(col))
            );
        }
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(
            eng.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(2)),
            Value::Number(1.0)
        );
        assert!(eng.spills().shadow_value(s0, RowId(1), ColId(0)).is_none());
        assert!(eng.spills().shadow_value(s0, RowId(1), ColId(1)).is_none());

        // 2. Self-shadow cycle (`A1 = =A2:A3`) must match fresh recalc across incremental edits.
        let mut self_cyc = FormulaEngine::new();
        self_cyc
            .edit_cell_raw(s0, RowId(0), ColId(0), "=A2:A3")
            .unwrap();
        self_cyc.recalc_all();
        assert_eq!(
            self_cyc
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Cycle)
        );
        self_cyc.edit_cell_raw(s0, RowId(2), ColId(0), "5").unwrap();
        self_cyc.recalc_all();
        let fresh_self = FormulaEngine::from_workbook_recalc_all(self_cyc.workbook().clone());
        assert_eq!(
            self_cyc
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(
            fresh_self
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Cycle)
        );
        assert!(
            self_cyc
                .spills()
                .shadow_value(s0, RowId(1), ColId(0))
                .is_none()
        );

        // 3. Conditional short-circuit self-shadow reference (`B1 = 1`, `A1 = =IF(B1=1, {1;2}, A2)`).
        let mut cond = FormulaEngine::new();
        cond.edit_cell_raw(s0, RowId(0), ColId(1), "1").unwrap();
        cond.edit_cell_raw(s0, RowId(0), ColId(0), "=IF(B1=1, {1;2}, A2)")
            .unwrap();
        cond.recalc_all();
        let fresh_cond = FormulaEngine::from_workbook_recalc_all(cond.workbook().clone());
        assert_eq!(
            cond.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(1.0)
        );
        assert_eq!(
            cond.spills().shadow_value(s0, RowId(1), ColId(0)),
            Some(&crate::value_eval::EvalScalar::Number(2.0))
        );
        assert_eq!(
            fresh_cond
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(1.0)
        );
        assert_eq!(
            fresh_cond.spills().shadow_value(s0, RowId(1), ColId(0)),
            Some(&crate::value_eval::EvalScalar::Number(2.0))
        );

        // 4. Cleared-shadow cycle recovery across delete/insert/overwrite (proptest seed).
        let mut rec = FormulaEngine::new();
        let rid0 = RowId(
            rec.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(0)
                .unwrap(),
        );
        rec.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(rid0.0, 1).unwrap()],
        })
        .unwrap();
        rec.recalc_all();
        let r3 = RowId(
            rec.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(3)
                .unwrap(),
        );
        rec.edit_cell_raw(s0, r3, ColId(0), "=A1:B1").unwrap();
        rec.recalc_all();
        let r0 = RowId(
            rec.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(0)
                .unwrap(),
        );
        rec.edit_cell_raw(s0, r0, ColId(0), "=COUNTIF(B3:D5, \">1\")")
            .unwrap();
        rec.recalc_all();
        for _ in 0..2 {
            let run = rec.allocator_mut().alloc_run(1).unwrap();
            rec.apply_op(Op::InsertRows {
                sheet: s0,
                after: None,
                ids: run,
            })
            .unwrap();
            rec.recalc_all();
        }
        let r5 = RowId(
            rec.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(5)
                .unwrap(),
        );
        rec.edit_cell_raw(s0, r5, ColId(0), "=A1#").unwrap();
        rec.recalc_all();
        let fresh_rec = FormulaEngine::from_workbook_recalc_all(rec.workbook().clone());
        let p2_row = RowId(
            rec.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(2)
                .unwrap(),
        );
        assert_eq!(
            rec.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(p2_row, ColId(0)),
            Value::Number(0.0)
        );
        assert_eq!(
            fresh_rec
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(p2_row, ColId(0)),
            Value::Number(0.0)
        );

        // 5. Indirect cycle break on a non-cyclic precedent of the cleared spill anchor.
        let mut ind = FormulaEngine::new();
        ind.edit_cell_raw(s0, RowId(0), ColId(2), "1").unwrap(); // C1
        ind.edit_cell_raw(s0, RowId(3), ColId(0), "=IF(C1=1, A1:B1, 0)")
            .unwrap(); // A4 -> shadow B4
        ind.recalc_all();
        ind.edit_cell_raw(s0, RowId(0), ColId(0), "=COUNTIF(B3:D5, \">1\")")
            .unwrap(); // A1 cycles with A4's shadow B4
        ind.recalc_all();
        assert_eq!(
            ind.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Error(CalcError::Cycle)
        );
        ind.edit_cell_raw(s0, RowId(0), ColId(2), "0").unwrap(); // break cycle indirectly
        ind.recalc_all();
        let fresh_ind = FormulaEngine::from_workbook_recalc_all(ind.workbook().clone());
        assert_eq!(
            ind.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.0)
        );
        assert_eq!(
            ind.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(0)),
            Value::Number(0.0)
        );
        assert_eq!(
            fresh_ind
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.0)
        );

        // 6. Tombstoned rows before a spill shadow still match COUNTIF by live row position.
        let mut tomb = FormulaEngine::new();
        let del0 = RowId(
            tomb.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(0)
                .unwrap(),
        );
        tomb.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(del0.0, 1).unwrap()],
        })
        .unwrap();
        let anchor_row = RowId(
            tomb.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(3)
                .unwrap(),
        );
        tomb.edit_cell_raw(s0, anchor_row, ColId(0), "={1,5}")
            .unwrap();
        let query_row = RowId(
            tomb.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(0)
                .unwrap(),
        );
        tomb.edit_cell_raw(s0, query_row, ColId(0), "=COUNTIF(B4:B4, \">1\")")
            .unwrap();
        tomb.recalc_all();
        assert_eq!(
            tomb.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(query_row, ColId(0)),
            Value::Number(1.0)
        );

        // 7. Self-shadow cycle blocked later by a static write or dynamic shadow recovers to #SPILL!.
        let mut blk = FormulaEngine::new();
        blk.edit_cell_raw(s0, RowId(3), ColId(1), "=C3:D4").unwrap(); // B4 self-shadow 2x2
        blk.recalc_all();
        assert_eq!(
            blk.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(1)),
            Value::Error(CalcError::Cycle)
        );
        let blk_del = RowId(
            blk.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(0)
                .unwrap(),
        );
        blk.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(blk_del.0, 1).unwrap()],
        })
        .unwrap();
        blk.recalc_all();
        let p3_row = RowId(
            blk.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(3)
                .unwrap(),
        );
        let p2_b_row = RowId(
            blk.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(2)
                .unwrap(),
        );
        blk.edit_cell_raw(s0, p3_row, ColId(1), "=A1#").unwrap();
        blk.recalc_all();
        let fresh_blk = FormulaEngine::from_workbook_recalc_all(blk.workbook().clone());
        assert_eq!(
            blk.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(p2_b_row, ColId(1)),
            Value::Error(CalcError::Spill)
        );
        assert_eq!(
            fresh_blk
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(p2_b_row, ColId(1)),
            Value::Error(CalcError::Spill)
        );

        // Same recovery when blocked dynamically by another array spilling onto the cleared shadow.
        let mut dyn_blk = FormulaEngine::new();
        dyn_blk
            .edit_cell_raw(s0, RowId(3), ColId(1), "=C3:D4")
            .unwrap(); // B4 self-shadow 2x2 over B4:C5
        dyn_blk.recalc_all();
        dyn_blk
            .edit_cell_raw(s0, RowId(4), ColId(0), "={10,20}")
            .unwrap(); // A5 spills into B5 -> dynamic collision with B4's cleared footprint
        dyn_blk.recalc_all();
        let fresh_dyn = FormulaEngine::from_workbook_recalc_all(dyn_blk.workbook().clone());
        assert_eq!(
            dyn_blk
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(1)),
            Value::Error(CalcError::Spill)
        );
        assert_eq!(
            dyn_blk
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(4), ColId(0)),
            Value::Error(CalcError::Spill)
        );
        assert_eq!(
            fresh_dyn
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(1)),
            Value::Error(CalcError::Spill)
        );
        assert_eq!(
            fresh_dyn
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(4), ColId(0)),
            Value::Error(CalcError::Spill)
        );

        // 8. Multi-hop pure shadow cycle (`C1 -> D3 -> A2 -> C1` via shadows) matches fresh recalc.
        let mut cyc = FormulaEngine::new();
        cyc.edit_cell_raw(s0, RowId(1), ColId(0), "=C2:C3").unwrap();
        cyc.edit_cell_raw(s0, RowId(2), ColId(2), "={1,2;3,4}")
            .unwrap();
        let del_row3 = RowId(
            cyc.workbook()
                .sheet(s0)
                .unwrap()
                .row_axis()
                .pos_to_id(3)
                .unwrap(),
        );
        cyc.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(del_row3.0, 1).unwrap()],
        })
        .unwrap();
        cyc.edit_cell_raw(s0, RowId(0), ColId(2), "=D3:D4").unwrap();
        cyc.edit_cell_raw(s0, RowId(2), ColId(3), "=COUNTIF(A3:C5, \">1\")")
            .unwrap();
        cyc.recalc_all();
        let fresh_cyc = FormulaEngine::from_workbook_recalc_all(cyc.workbook().clone());
        for (r, c, expected) in [
            (RowId(0), ColId(2), Value::Error(CalcError::Cycle)),
            (RowId(1), ColId(0), Value::Error(CalcError::Cycle)),
            (RowId(2), ColId(3), Value::Error(CalcError::Cycle)),
            (RowId(2), ColId(2), Value::Error(CalcError::Spill)),
        ] {
            assert_eq!(
                cyc.workbook().sheet(s0).unwrap().value_by_id(r, c),
                expected
            );
            assert_eq!(
                fresh_cyc.workbook().sheet(s0).unwrap().value_by_id(r, c),
                expected
            );
        }
        assert!(cyc.spills().shadow_value(s0, RowId(1), ColId(2)).is_none());
        assert!(cyc.spills().shadow_value(s0, RowId(2), ColId(0)).is_none());
        assert!(
            fresh_cyc
                .spills()
                .shadow_value(s0, RowId(1), ColId(2))
                .is_none()
        );
        assert!(
            fresh_cyc
                .spills()
                .shadow_value(s0, RowId(2), ColId(0))
                .is_none()
        );

        // 9. Stale-shadow conditional switch must not produce a false `#CYCLE!`.
        // (a) Direct scalar switch clearing a self-referencing fallback branch.
        let mut sw = FormulaEngine::new();
        sw.edit_cell_raw(s0, RowId(0), ColId(1), "1").unwrap();
        sw.edit_cell_raw(s0, RowId(0), ColId(0), "=IF(B1=1, {1;2}, A2)")
            .unwrap();
        sw.recalc_all();
        sw.edit_cell_raw(s0, RowId(0), ColId(1), "0").unwrap();
        sw.recalc_all();
        let fresh_sw = FormulaEngine::from_workbook_recalc_all(sw.workbook().clone());
        assert_eq!(
            sw.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.0)
        );
        assert_eq!(
            fresh_sw
                .workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(0)),
            Value::Number(0.0)
        );
        assert!(sw.spills().shadow_value(s0, RowId(1), ColId(0)).is_none());

        // (b) In-wave conditional switch driven by a spill shadow (`A6 -> A7 -> B1 -> C1`).
        for initial_a6 in [None, Some("={10;99}")] {
            let mut wave_sw = FormulaEngine::new();
            wave_sw
                .edit_cell_raw(s0, RowId(0), ColId(1), "=IF(A7=20, 0, 1)")
                .unwrap();
            wave_sw
                .edit_cell_raw(s0, RowId(0), ColId(2), "=IF(B1=1, {1;2}, D3)")
                .unwrap();
            wave_sw
                .edit_cell_raw(s0, RowId(1), ColId(0), "=C2:C3")
                .unwrap();
            wave_sw
                .edit_cell_raw(s0, RowId(2), ColId(3), "=COUNTIF(A3:C5, \">1\")")
                .unwrap();
            if let Some(a6_raw) = initial_a6 {
                wave_sw
                    .edit_cell_raw(s0, RowId(5), ColId(0), a6_raw)
                    .unwrap();
            }
            wave_sw.recalc_all();
            wave_sw
                .edit_cell_raw(s0, RowId(5), ColId(0), "={10;20}")
                .unwrap();
            wave_sw.recalc_all();
            let fresh_wave = FormulaEngine::from_workbook_recalc_all(wave_sw.workbook().clone());
            for (r, c) in [(RowId(0), ColId(2)), (RowId(2), ColId(3))] {
                assert_eq!(
                    wave_sw.workbook().sheet(s0).unwrap().value_by_id(r, c),
                    Value::Number(0.0)
                );
                assert_eq!(
                    fresh_wave.workbook().sheet(s0).unwrap().value_by_id(r, c),
                    Value::Number(0.0)
                );
            }
            assert!(
                wave_sw
                    .spills()
                    .shadow_value(s0, RowId(1), ColId(2))
                    .is_none()
            );
        }

        // 10. Figure-8 overlapping SCC with COUNTIF + active spill matches fresh recalc regardless of root order.
        let mut scc = FormulaEngine::new();
        let row_at = |e: &FormulaEngine, pos: u32| {
            RowId(
                e.workbook()
                    .sheet(s0)
                    .unwrap()
                    .row_axis()
                    .pos_to_id(pos)
                    .unwrap(),
            )
        };
        scc.edit_cell_raw(s0, row_at(&scc, 0), ColId(0), "=A1")
            .unwrap();
        scc.recalc_all();
        let ins = scc.allocator_mut().alloc_run(1).unwrap();
        scc.apply_op(Op::InsertRows {
            sheet: s0,
            after: None,
            ids: ins,
        })
        .unwrap();
        scc.recalc_all();
        scc.edit_cell_raw(s0, row_at(&scc, 2), ColId(0), "=COUNTIF(C1:E3, \">1\")")
            .unwrap();
        scc.recalc_all();
        let del_p3 = row_at(&scc, 3);
        scc.apply_op(Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(del_p3.0, 1).unwrap()],
        })
        .unwrap();
        scc.recalc_all();
        scc.edit_cell_raw(s0, row_at(&scc, 1), ColId(2), "=COUNTIF(D2:F4, \">1\")")
            .unwrap();
        scc.recalc_all();
        scc.edit_cell_raw(s0, row_at(&scc, 2), ColId(3), "=COUNTIF(A1:C3, \">1\")")
            .unwrap();
        scc.recalc_all();
        scc.edit_cell_raw(s0, row_at(&scc, 0), ColId(3), "=A4:A5")
            .unwrap();
        scc.recalc_all();
        scc.edit_cell_raw(s0, row_at(&scc, 2), ColId(2), "=COUNTIF(D1:F3, \">1\")")
            .unwrap();
        scc.recalc_all();
        scc.edit_cell_raw(s0, row_at(&scc, 0), ColId(6), "=COUNTIF(D3:D3, \">1\")")
            .unwrap();
        scc.recalc_all();

        let fresh_scc = FormulaEngine::from_workbook_recalc_all(scc.workbook().clone());
        for (pos_r, c, expected) in [
            (1, ColId(0), Value::Error(CalcError::Cycle)),
            (1, ColId(2), Value::Error(CalcError::Cycle)),
            (2, ColId(0), Value::Error(CalcError::Cycle)),
            (2, ColId(2), Value::Error(CalcError::Cycle)),
            (2, ColId(3), Value::Error(CalcError::Cycle)),
            (0, ColId(3), Value::Number(0.0)),
            (0, ColId(6), Value::Number(0.0)),
        ] {
            let rid = row_at(&scc, pos_r);
            assert_eq!(
                scc.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
            assert_eq!(
                fresh_scc.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
        }
        let d2_rid = row_at(&scc, 1);
        assert_eq!(
            scc.spills().shadow_value(s0, d2_rid, ColId(3)),
            Some(&crate::value_eval::EvalScalar::Number(0.0))
        );
        assert_eq!(
            fresh_scc.spills().shadow_value(s0, d2_rid, ColId(3)),
            Some(&crate::value_eval::EvalScalar::Number(0.0))
        );

        // 11. Dependent `#` / shadow spills yield cleanly to upstream spills across all orders.
        // (a) 3-cell symmetric + downstream `#` collision (`C6`, `B7`, `D5 = =C6#`, proptest seed).
        let c6 = (RowId(5), ColId(2), "={1,2;3,4}");
        let b7 = (RowId(6), ColId(1), "={1,2}");
        let d5 = (RowId(4), ColId(3), "=C6#");
        let orders_3 = [
            [c6, b7, d5],
            [c6, d5, b7],
            [b7, c6, d5],
            [b7, d5, c6],
            [d5, c6, b7],
            [d5, b7, c6],
        ];
        for order in orders_3 {
            let mut dep_eng = FormulaEngine::new();
            for &(row, col, raw) in &order {
                dep_eng.edit_cell_raw(s0, row, col, raw).unwrap();
                dep_eng.recalc_all();
            }
            let fresh_dep = FormulaEngine::from_workbook_recalc_all(dep_eng.workbook().clone());
            for (row, col, expected) in [
                (RowId(5), ColId(2), Value::Error(CalcError::Spill)),
                (RowId(6), ColId(1), Value::Error(CalcError::Spill)),
                (RowId(4), ColId(3), Value::Error(CalcError::Ref)),
            ] {
                assert_eq!(
                    dep_eng.workbook().sheet(s0).unwrap().value_by_id(row, col),
                    expected
                );
                assert_eq!(
                    fresh_dep
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(row, col),
                    expected
                );
            }
            assert!(!dep_eng.spills().has_spills_on_sheet(s0));
            assert!(!fresh_dep.spills().has_spills_on_sheet(s0));

            // Delete B7 -> C6 pops open cleanly and D5 becomes `#SPILL!`.
            dep_eng.edit_cell_raw(s0, RowId(6), ColId(1), "").unwrap();
            dep_eng.recalc_all();
            let fresh_open = FormulaEngine::from_workbook_recalc_all(dep_eng.workbook().clone());
            for (row, col, expected) in [
                (RowId(5), ColId(2), Value::Number(1.0)),
                (RowId(6), ColId(1), Value::Empty),
                (RowId(4), ColId(3), Value::Error(CalcError::Spill)),
            ] {
                assert_eq!(
                    dep_eng.workbook().sheet(s0).unwrap().value_by_id(row, col),
                    expected
                );
                assert_eq!(
                    fresh_open
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(row, col),
                    expected
                );
            }
            assert_eq!(dep_eng.spill_range((s0, RowId(5), ColId(2))), Some((2, 2)));
            assert_eq!(
                fresh_open.spill_range((s0, RowId(5), ColId(2))),
                Some((2, 2))
            );

            // Restore B7 -> returns to `C6=#SPILL!`, `B7=#SPILL!`, `D5=#REF!`.
            dep_eng
                .edit_cell_raw(s0, RowId(6), ColId(1), "={1,2}")
                .unwrap();
            dep_eng.recalc_all();
            let fresh_restored =
                FormulaEngine::from_workbook_recalc_all(dep_eng.workbook().clone());
            for (row, col, expected) in [
                (RowId(5), ColId(2), Value::Error(CalcError::Spill)),
                (RowId(6), ColId(1), Value::Error(CalcError::Spill)),
                (RowId(4), ColId(3), Value::Error(CalcError::Ref)),
            ] {
                assert_eq!(
                    dep_eng.workbook().sheet(s0).unwrap().value_by_id(row, col),
                    expected
                );
                assert_eq!(
                    fresh_restored
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(row, col),
                    expected
                );
            }
        }

        // (b) 2-cell dependent overlap (`C6 = ={1,2;3,4}`, `D5 = =C6#`) in both orders.
        for order in [[c6, d5], [d5, c6]] {
            let mut two = FormulaEngine::new();
            for &(row, col, raw) in &order {
                two.edit_cell_raw(s0, row, col, raw).unwrap();
                two.recalc_all();
            }
            let fresh_two = FormulaEngine::from_workbook_recalc_all(two.workbook().clone());
            for engine_ref in [&two, &fresh_two] {
                assert_eq!(
                    engine_ref
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(RowId(5), ColId(2)),
                    Value::Number(1.0)
                );
                assert_eq!(
                    engine_ref
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(RowId(4), ColId(3)),
                    Value::Error(CalcError::Spill)
                );
                assert_eq!(
                    engine_ref.spill_range((s0, RowId(5), ColId(2))),
                    Some((2, 2))
                );
            }
        }

        // (c) 4-cell cascade (`C6`, `B7`, `B1 = =C6#`, `A2 = ={10,20}`).
        let mut cas = FormulaEngine::new();
        cas.edit_cell_raw(s0, RowId(5), ColId(2), "={1,2;3,4}")
            .unwrap();
        cas.edit_cell_raw(s0, RowId(6), ColId(1), "={1,2}").unwrap();
        cas.edit_cell_raw(s0, RowId(0), ColId(1), "=C6#").unwrap();
        cas.edit_cell_raw(s0, RowId(1), ColId(0), "={10,20}")
            .unwrap();
        cas.recalc_all();
        let fresh_cas = FormulaEngine::from_workbook_recalc_all(cas.workbook().clone());
        for (row, col, expected) in [
            (RowId(5), ColId(2), Value::Error(CalcError::Spill)),
            (RowId(6), ColId(1), Value::Error(CalcError::Spill)),
            (RowId(0), ColId(1), Value::Error(CalcError::Ref)),
            (RowId(1), ColId(0), Value::Number(10.0)),
        ] {
            assert_eq!(
                cas.workbook().sheet(s0).unwrap().value_by_id(row, col),
                expected
            );
            assert_eq!(
                fresh_cas
                    .workbook()
                    .sheet(s0)
                    .unwrap()
                    .value_by_id(row, col),
                expected
            );
        }
        assert_eq!(
            cas.spills().shadow_value(s0, RowId(1), ColId(1)),
            Some(&crate::value_eval::EvalScalar::Number(20.0))
        );
        assert_eq!(
            fresh_cas.spills().shadow_value(s0, RowId(1), ColId(1)),
            Some(&crate::value_eval::EvalScalar::Number(20.0))
        );

        // (d) Shadow-dependency displacement (`D5 = =C7:C8`, `C6 = ={1,2;3,4}`) in both orders.
        let d5_rng = (RowId(4), ColId(3), "=C7:C8");
        for order in [[d5_rng, c6], [c6, d5_rng]] {
            let mut disp = FormulaEngine::new();
            for &(row, col, raw) in &order {
                disp.edit_cell_raw(s0, row, col, raw).unwrap();
                disp.recalc_all();
            }
            let fresh_disp = FormulaEngine::from_workbook_recalc_all(disp.workbook().clone());
            for engine_ref in [&disp, &fresh_disp] {
                assert_eq!(
                    engine_ref
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(RowId(5), ColId(2)),
                    Value::Number(1.0)
                );
                assert_eq!(
                    engine_ref
                        .workbook()
                        .sheet(s0)
                        .unwrap()
                        .value_by_id(RowId(4), ColId(3)),
                    Value::Error(CalcError::Spill)
                );
                assert_eq!(
                    engine_ref.spill_range((s0, RowId(5), ColId(2))),
                    Some((2, 2))
                );
            }
        }

        // 12. Dynamically-blocked spill overlapping a cleared cycle shadow wakes `#CYCLE!` to `#SPILL!`.
        let mut s4 = FormulaEngine::new();
        s4.edit_cell_raw(s0, RowId(1), ColId(0), "=B2:C2").unwrap();
        s4.recalc_all();
        let ins_s4 = s4.allocator_mut().alloc_run(1).unwrap();
        s4.apply_op(Op::InsertRows {
            sheet: s0,
            after: Some(RowId(0)),
            ids: ins_s4,
        })
        .unwrap();
        s4.recalc_all();
        s4.edit_cell_raw(s0, row_at(&s4, 1), ColId(0), "={1,2}")
            .unwrap();
        s4.recalc_all();
        s4.edit_cell_raw(s0, row_at(&s4, 0), ColId(1), "={1;2;3}")
            .unwrap();
        s4.recalc_all();
        let fresh_s4 = FormulaEngine::from_workbook_recalc_all(s4.workbook().clone());
        for (pos_r, c, expected) in [
            (0, ColId(1), Value::Error(CalcError::Spill)),
            (1, ColId(0), Value::Error(CalcError::Spill)),
            (2, ColId(0), Value::Error(CalcError::Spill)),
        ] {
            let rid = row_at(&s4, pos_r);
            assert_eq!(
                s4.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
            assert_eq!(
                fresh_s4.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
        }

        // Clone 1: deleting B1 pops A2 open and returns A3 to `#CYCLE!`.
        let mut c1 = s4.clone();
        c1.edit_cell_raw(s0, row_at(&c1, 0), ColId(1), "").unwrap();
        c1.recalc_all();
        let fresh_c1 = FormulaEngine::from_workbook_recalc_all(c1.workbook().clone());
        for (pos_r, c, expected) in [
            (0, ColId(1), Value::Empty),
            (1, ColId(0), Value::Number(1.0)),
            (2, ColId(0), Value::Error(CalcError::Cycle)),
        ] {
            let rid = row_at(&c1, pos_r);
            assert_eq!(
                c1.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
            assert_eq!(
                fresh_c1.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
        }
        assert_eq!(c1.spill_range((s0, row_at(&c1, 1), ColId(0))), Some((1, 2)));

        // Clone 2: deleting A2 lets B1 spill over B1:B3, keeping A3 at `#SPILL!`.
        let mut c2 = s4.clone();
        c2.edit_cell_raw(s0, row_at(&c2, 1), ColId(0), "").unwrap();
        c2.recalc_all();
        let fresh_c2 = FormulaEngine::from_workbook_recalc_all(c2.workbook().clone());
        for (pos_r, c, expected) in [
            (0, ColId(1), Value::Number(1.0)),
            (1, ColId(0), Value::Empty),
            (2, ColId(0), Value::Error(CalcError::Spill)),
        ] {
            let rid = row_at(&c2, pos_r);
            assert_eq!(
                c2.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
            assert_eq!(
                fresh_c2.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
        }
        assert_eq!(c2.spill_range((s0, row_at(&c2, 0), ColId(1))), Some((3, 1)));

        // Clone 3: static write at B2 keeps B1/A2 `#SPILL!` and returns A3 to `#CYCLE!`.
        let mut c3 = s4;
        c3.edit_cell_raw(s0, row_at(&c3, 1), ColId(1), "x").unwrap();
        c3.recalc_all();
        let fresh_c3 = FormulaEngine::from_workbook_recalc_all(c3.workbook().clone());
        for (pos_r, c, expected) in [
            (0, ColId(1), Value::Error(CalcError::Spill)),
            (1, ColId(0), Value::Error(CalcError::Spill)),
            (2, ColId(0), Value::Error(CalcError::Cycle)),
        ] {
            let rid = row_at(&c3, pos_r);
            assert_eq!(
                c3.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
            assert_eq!(
                fresh_c3.workbook().sheet(s0).unwrap().value_by_id(rid, c),
                expected
            );
        }

        // Reverse sorted-order variant: cycle anchor C1 precedes blocking A2 in fresh recalc order.
        let mut rev = FormulaEngine::new();
        rev.edit_cell_raw(s0, RowId(0), ColId(2), "=C2:C3").unwrap();
        rev.recalc_all();
        rev.edit_cell_raw(s0, RowId(0), ColId(1), "={10;20}")
            .unwrap();
        rev.recalc_all();
        rev.edit_cell_raw(s0, RowId(1), ColId(0), "={1,2,3}")
            .unwrap();
        rev.recalc_all();
        let fresh_rev = FormulaEngine::from_workbook_recalc_all(rev.workbook().clone());
        for (row, col, expected) in [
            (RowId(0), ColId(1), Value::Error(CalcError::Spill)),
            (RowId(1), ColId(0), Value::Error(CalcError::Spill)),
            (RowId(0), ColId(2), Value::Error(CalcError::Spill)),
        ] {
            assert_eq!(
                rev.workbook().sheet(s0).unwrap().value_by_id(row, col),
                expected
            );
            assert_eq!(
                fresh_rev
                    .workbook()
                    .sheet(s0)
                    .unwrap()
                    .value_by_id(row, col),
                expected
            );
        }

        // 13. Stale `cycle_spills` peers wake cleanly when an anchor enters a new `#CYCLE!`.
        let mut s5 = FormulaEngine::new();
        s5.edit_cell_raw(s0, RowId(3), ColId(1), "=B1:C1").unwrap();
        s5.recalc_all();
        s5.edit_cell_raw(s0, RowId(0), ColId(2), "=COUNTIF(C2:E4, \">1\")")
            .unwrap();
        s5.recalc_all();
        assert_eq!(
            s5.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(3), ColId(1)),
            Value::Error(CalcError::Cycle)
        );
        assert_eq!(
            s5.workbook()
                .sheet(s0)
                .unwrap()
                .value_by_id(RowId(0), ColId(2)),
            Value::Error(CalcError::Cycle)
        );
        s5.edit_cell_raw(s0, RowId(0), ColId(1), "=B3:B4").unwrap();
        s5.recalc_all();
        let fresh_s5 = FormulaEngine::from_workbook_recalc_all(s5.workbook().clone());
        for (row, col, expected) in [
            (RowId(0), ColId(1), Value::Error(CalcError::Cycle)),
            (RowId(3), ColId(1), Value::Error(CalcError::Cycle)),
            (RowId(0), ColId(2), Value::Number(0.0)),
        ] {
            assert_eq!(
                s5.workbook().sheet(s0).unwrap().value_by_id(row, col),
                expected
            );
            assert_eq!(
                fresh_s5.workbook().sheet(s0).unwrap().value_by_id(row, col),
                expected
            );
        }
        s5.edit_cell_raw(s0, RowId(0), ColId(1), "").unwrap();
        s5.recalc_all();
        let fresh_s5_after = FormulaEngine::from_workbook_recalc_all(s5.workbook().clone());
        for (row, col, expected) in [
            (RowId(0), ColId(1), Value::Empty),
            (RowId(3), ColId(1), Value::Error(CalcError::Cycle)),
            (RowId(0), ColId(2), Value::Error(CalcError::Cycle)),
        ] {
            assert_eq!(
                s5.workbook().sheet(s0).unwrap().value_by_id(row, col),
                expected
            );
            assert_eq!(
                fresh_s5_after
                    .workbook()
                    .sheet(s0)
                    .unwrap()
                    .value_by_id(row, col),
                expected
            );
        }
    }

    #[test]
    fn autofilter_projection_recalculates_subtotal_skips_hidden_rows_and_preserves_digest() {
        use alloc::string::String;

        use reticula_core::viewport::Page;

        use crate::filter::{ColFilterCriterion, FilterCmpOp};

        let mut engine = FormulaEngine::new();
        let s0 = SheetId(0);
        engine
            .edit_cell_raw(s0, RowId(0), ColId(0), "Amount")
            .unwrap();
        engine.edit_cell_raw(s0, RowId(1), ColId(0), "10").unwrap();
        engine.edit_cell_raw(s0, RowId(2), ColId(0), "25").unwrap();
        engine.edit_cell_raw(s0, RowId(3), ColId(0), "40").unwrap();
        engine
            .edit_cell_raw(s0, RowId(0), ColId(1), "=SUBTOTAL(109, A2:A4)")
            .unwrap();
        engine
            .edit_cell_raw(s0, RowId(0), ColId(2), "=AGGREGATE(9, 5, A2:A4)")
            .unwrap();
        engine.recalc_all();

        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 1),
            Value::number(75.0)
        );
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 2),
            Value::number(75.0)
        );

        let digest_before = engine.workbook().state_digest();
        let initial_rows = engine.workbook().sheet(s0).unwrap().rows();
        assert!(engine.set_filter_table(s0, 0, 3, 0, 0));
        assert!(engine.set_column_filter(
            s0,
            0,
            Some(ColFilterCriterion::NumberCompare {
                op: FilterCmpOp::Ge,
                value: 20.0,
            })
        ));
        engine.recalc_all();

        // Filter hides row 1 (value 10), leaves workbook StateDigest untouched.
        assert_eq!(engine.workbook().state_digest(), digest_before);
        assert!(engine.is_row_hidden(s0, RowId(1)));
        assert!(!engine.is_row_hidden(s0, RowId(2)));
        assert_eq!(engine.visible_row_count(s0), initial_rows - 1);
        assert_eq!(engine.display_to_physical_row(s0, 0), Some(0));
        assert_eq!(engine.display_to_physical_row(s0, 1), Some(2));
        assert_eq!(engine.physical_to_display_row(s0, 1), None);
        assert_eq!(engine.physical_to_display_row(s0, 2), Some(1));

        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 1),
            Value::number(65.0)
        );
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 2),
            Value::number(65.0)
        );

        // Encoded viewport page 0 includes row IDs [0, 2, 3, ...], skipping hidden RowId(1).
        let mut text = String::new();
        let bytes = engine
            .encode_viewport_page(s0, Page { row: 0, col: 0 }, 1, &mut text)
            .unwrap();
        let header_row_ids_start = reticula_core::viewport::HEADER_LEN;
        let r0 = u64::from_le_bytes(
            bytes[header_row_ids_start..header_row_ids_start + 8]
                .try_into()
                .unwrap(),
        );
        let r1 = u64::from_le_bytes(
            bytes[header_row_ids_start + 8..header_row_ids_start + 16]
                .try_into()
                .unwrap(),
        );
        assert_eq!(r0, 0);
        assert_eq!(r1, 2);

        // Inserting a row before header shifts filter header_row from display/phys 0 to 1.
        engine
            .apply_op(Op::InsertRows {
                sheet: s0,
                after: None,
                ids: IdRun::new(1_048_576, 1).unwrap(),
            })
            .unwrap();
        assert_eq!(engine.visible_row_count(s0), initial_rows);
        assert_eq!(engine.display_to_physical_row(s0, 1), Some(1));
        assert_eq!(engine.display_to_physical_row(s0, 2), Some(3));

        engine.clear_filter(s0);
        engine.recalc_all();
        assert_eq!(engine.visible_row_count(s0), initial_rows + 1);
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(1, 1),
            Value::number(75.0)
        );
    }

    #[test]
    fn transient_formula_evaluates_without_side_effects_and_list_error_cells_traces_precedents() {
        let mut engine = FormulaEngine::new();
        let s0 = SheetId(0);
        engine.edit_cell_raw(s0, RowId(0), ColId(0), "10").unwrap();
        engine.edit_cell_raw(s0, RowId(1), ColId(0), "0").unwrap();
        engine
            .edit_cell_raw(s0, RowId(2), ColId(0), "=A1/A2")
            .unwrap();
        engine
            .edit_cell_raw(s0, RowId(3), ColId(0), "=A3+5")
            .unwrap();
        engine.recalc_all();

        let digest_before = engine.workbook().state_digest();
        let formulas_before = engine.workbook().formulas().len();

        // Scalar transient evaluation (with and without leading `=`).
        let sum_val = engine
            .evaluate_transient_formula(s0, (0, 1), "=SUM(A1:A2)*ROW()")
            .unwrap();
        assert_eq!(
            sum_val,
            TransientEvalValue::Scalar(EvalScalar::Number(10.0))
        );
        let no_eq = engine
            .evaluate_transient_formula(s0, (4, 1), "IFERROR(A3, 99)")
            .unwrap();
        assert_eq!(no_eq, TransientEvalValue::Scalar(EvalScalar::Number(99.0)));

        // Dynamic array transient evaluation.
        let arr_val = engine
            .evaluate_transient_formula(s0, (0, 2), "=SEQUENCE(2, 2, 1, 1)")
            .unwrap();
        match arr_val {
            TransientEvalValue::Array(arr) => {
                assert_eq!((arr.rows, arr.cols), (2, 2));
                assert_eq!(arr.values[0], EvalScalar::Number(1.0));
                assert_eq!(arr.values[3], EvalScalar::Number(4.0));
            }
            TransientEvalValue::Scalar(_) => panic!("expected 2x2 array"),
        }

        // Workbook state, formulas, spills, and digest are untouched.
        assert_eq!(engine.workbook().state_digest(), digest_before);
        assert_eq!(engine.workbook().formulas().len(), formulas_before);
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 1),
            Value::Empty
        );
        assert_eq!(
            engine.workbook().sheet(s0).unwrap().value(0, 2),
            Value::Empty
        );

        // Bounds / missing sheet / parse error paths.
        assert_eq!(
            engine.evaluate_transient_formula(SheetId(999), (0, 0), "=1+1"),
            Err(TransientEvalError::SheetNotFound)
        );
        assert_eq!(
            engine.evaluate_transient_formula(s0, (u32::MAX, 0), "=1+1"),
            Err(TransientEvalError::OutOfBounds)
        );
        assert!(matches!(
            engine.evaluate_transient_formula(s0, (0, 0), "=SUM("),
            Err(TransientEvalError::Parse(_))
        ));

        // Error trace lists A3 (#DIV/0!) and A4 (#DIV/0!) with direct precedents.
        let errors = engine.list_error_cells(None, 10);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].address, "A3");
        assert_eq!(errors[0].error, "#DIV/0!");
        assert_eq!(errors[0].formula.as_deref(), Some("=A1/A2"));
        assert_eq!(errors[0].precedents.len(), 2);
        assert_eq!(errors[0].precedents[0].address, "A1");
        assert_eq!(errors[0].precedents[1].address, "A2");
        assert_eq!(errors[1].address, "A4");
        assert_eq!(errors[1].precedents[0].address, "A3");
        assert_eq!(errors[1].precedents[0].error.as_deref(), Some("#DIV/0!"));
    }
}
