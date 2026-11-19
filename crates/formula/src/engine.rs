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
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;

use hashbrown::HashMap;
use reticula_core::axis::{Axis, MAX_COL_AXIS_LEN};
use reticula_core::decimal15::{RoundingMode, round_decimal, snap_cancelled_zero};
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::input::{InferredInput, infer_input, infer_input_for_cell};
use reticula_core::memory::try_reserve_heap;
use reticula_core::ops::{
    CellEdit, CellInput, CellSnapshot, FormatBackup, FormatPatch, FormulaMode, InverseOp, Op,
    OpError, ScalarValue, SeedOrigin, Workbook, WorkbookCellKey,
};
use reticula_core::sample;
use reticula_core::sheet::Sheet;
use reticula_core::snapshot::{SnapshotError, decode_snapshot, encode_snapshot};
use reticula_core::value::{CalcError, Value};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

use crate::address::CellAddr;
use crate::ast::{BinaryOp, Literal, MAX_NESTING_DEPTH};
use crate::binder::{BoundExpr, BoundFormula, BoundRef, live_span_positions};
use crate::dep_graph::{DepGraph, FormulaRegistration};
use crate::functions::{EvalEnv, combine_choose, combine_if, combine_iferror, eval_eager_function};
use crate::parser::ParseError;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, ResolvedRef, coerce_for_scalar_op, eval_binary,
    eval_unary, finalize_top_level, resolve_bound_ref,
};

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

const STATE_CLEAN: u8 = 0;
const STATE_DIRTY: u8 = 1;
const STATE_EVALUATING: u8 = 2;

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

#[derive(Clone, Debug)]
struct FormulaSlot {
    key: WorkbookCellKey,
    reg: FormulaRegistration,
}

enum ExprOutcome {
    Ready(EvalValue),
    Blocked(SmallVec<[u32; 4]>),
    Cycle(u32),
}

/// Complete single-node spreadsheet engine (`Workbook` + `DepGraph` + time-sliced
/// recalculator).
#[derive(Clone, Debug)]
pub struct FormulaEngine {
    workbook: Workbook<BoundFormula>,
    dep_graph: DepGraph,
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

        for (&key, formula) in workbook.formulas() {
            let fid = u32::try_from(slots.len()).unwrap_or(0);
            let (reg, is_volatile) = dep_graph.register_with_flags(fid, key.0, formula);
            if is_volatile {
                volatile_set.insert(fid);
            }
            slots.push(Some(FormulaSlot { key, reg }));
            cell_to_fid.insert(key, fid);
        }

        Self {
            workbook,
            dep_graph,
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
        if !self.volatile_set.is_empty() {
            let roots: Vec<u32> = self.volatile_set.iter().copied().collect();
            self.propagate_dirty(roots);
        }
    }

    /// Underlying [`Workbook<BoundFormula>`].
    #[must_use]
    pub const fn workbook(&self) -> &Workbook<BoundFormula> {
        &self.workbook
    }

    /// Interns resolved styles for `page` on `sheet_id` before immutable
    /// viewport page encoding.
    pub fn prepare_page_styles(&mut self, sheet_id: SheetId, page: reticula_core::viewport::Page) {
        if let Some(sheet) = self.workbook.sheet_mut(sheet_id) {
            reticula_core::viewport::prepare_page_styles(sheet, page);
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
        let mut wb = core::mem::take(&mut self.workbook);
        wb.compact_local_garbage();
        *self = Self::from_workbook_clean(wb);
    }

    /// Mutable access to the workbook's [`IdAllocator`] for allocating fresh
    /// row/column [`IdRun`]s prior to [`Self::apply_op`].
    pub const fn allocator_mut(&mut self) -> &mut reticula_core::id::IdAllocator {
        self.workbook.allocator_mut()
    }

    /// Whether dirty formulas remain queued for recalculation.
    #[must_use]
    pub fn has_pending_recalc(&self) -> bool {
        self.dirty_cursor < self.dirty_queue.len() || !self.call_stack.is_empty()
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

        let inv = self.apply_op(Op::SetCells {
            sheet: sheet_id,
            edits: vec![CellEdit {
                row,
                col,
                input,
                format,
            }],
        })?;
        if !self.volatile_set.is_empty() {
            let vol_roots: Vec<u32> = self.volatile_set.iter().copied().collect();
            self.propagate_dirty(vol_roots);
        }

        Ok(inv)
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
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_rows_inserted(*sheet, *ids, &self.workbook, |fid| root_dirty.push(fid));
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RemoveInsertedCols { sheet, ids } => {
                self.changes.structural = true;
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_cols_inserted(*sheet, *ids, &self.workbook, |fid| root_dirty.push(fid));
                self.propagate_dirty(root_dirty);
            }
            InverseOp::UndeleteRows { sheet, ids } => {
                self.changes.structural = true;
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_rows_toggled(*sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
                self.propagate_dirty(root_dirty);
            }
            InverseOp::UndeleteCols { sheet, ids } => {
                self.changes.structural = true;
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_cols_toggled(*sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RedeleteRows { sheet, ids } => {
                self.on_rows_undeleted(*sheet, ids);
            }
            InverseOp::RedeleteCols { sheet, ids } => {
                self.on_cols_undeleted(*sheet, ids);
            }
            InverseOp::RestoreMovedRows {
                sheet, rewritten, ..
            }
            | InverseOp::RestoreMovedCols {
                sheet, rewritten, ..
            } => {
                self.on_axis_reordered(*sheet, rewritten);
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
        }

        if !self.volatile_set.is_empty() {
            let vol_roots: Vec<u32> = self.volatile_set.iter().copied().collect();
            self.propagate_dirty(vol_roots);
        }

        Ok(inv)
    }

    /// Applies an [`InverseOp`] and marks affected formulas dirty so undo/rollback
    /// restores both stored state and derived values.
    pub fn apply_inverse(&mut self, inv: InverseOp<BoundFormula>) -> Result<(), OpError> {
        // For `RemoveInsertedRows`/`Cols`, capture straddling ranges BEFORE
        // removing the run from the axis!
        let mut pre_dirty: Vec<u32> = Vec::new();
        match &inv {
            InverseOp::RemoveInsertedRows { sheet, ids } => {
                self.dep_graph.on_rows_toggled(
                    *sheet,
                    core::slice::from_ref(ids),
                    &self.workbook,
                    |fid| pre_dirty.push(fid),
                );
            }
            InverseOp::RemoveInsertedCols { sheet, ids } => {
                self.dep_graph.on_cols_toggled(
                    *sheet,
                    core::slice::from_ref(ids),
                    &self.workbook,
                    |fid| pre_dirty.push(fid),
                );
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
            InverseOp::RemoveInsertedRows { .. } | InverseOp::RemoveInsertedCols { .. } => {
                self.changes.structural = true;
                self.propagate_dirty(pre_dirty);
            }
            InverseOp::UndeleteRows { sheet, ids } => {
                self.on_rows_undeleted(sheet, &ids);
            }
            InverseOp::UndeleteCols { sheet, ids } => {
                self.on_cols_undeleted(sheet, &ids);
            }
            InverseOp::RedeleteRows { sheet, ids } => {
                self.changes.structural = true;
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_rows_toggled(sheet, &ids, &self.workbook, |fid| root_dirty.push(fid));
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RedeleteCols { sheet, ids } => {
                self.changes.structural = true;
                let mut root_dirty = Vec::new();
                self.dep_graph
                    .on_cols_toggled(sheet, &ids, &self.workbook, |fid| root_dirty.push(fid));
                self.propagate_dirty(root_dirty);
            }
            InverseOp::RestoreMovedRows {
                sheet, rewritten, ..
            }
            | InverseOp::RestoreMovedCols {
                sheet, rewritten, ..
            } => {
                self.on_axis_reordered(sheet, &rewritten);
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
                    return RecalcStatus::Complete { recalculated: done };
                };
                self.call_stack.push(root_fid);
            }

            let Some(&cur_fid) = self.call_stack.last() else {
                continue;
            };
            if self.state_of(cur_fid) == STATE_CLEAN || self.slot_of(cur_fid).is_none() {
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
                    let final_scalar = finalize_top_level(val, &self.workbook, caller);
                    self.store_eval_scalar(key, final_scalar);
                    self.set_state(cur_fid, STATE_CLEAN);
                    let _ = self.call_stack.pop();
                    self.wave_done = self.wave_done.saturating_add(1);
                    since_check = since_check.saturating_add(1);
                    if since_check >= stride {
                        since_check = 0;
                        if self.has_pending_recalc() && should_yield(self.wave_done) {
                            let rem = self
                                .dirty_queue
                                .len()
                                .saturating_sub(self.dirty_cursor)
                                .saturating_add(self.call_stack.len());
                            let total = self
                                .wave_done
                                .saturating_add(u32::try_from(rem).unwrap_or(u32::MAX));
                            return RecalcStatus::InProgress {
                                done: self.wave_done,
                                total,
                            };
                        }
                    }
                }
                ExprOutcome::Blocked(deps) => {
                    // Push dirty precedents in reverse so the first precedent is
                    // evaluated first on top of `call_stack`.
                    for &dep_fid in deps.iter().rev() {
                        if self.state_of(dep_fid) == STATE_DIRTY {
                            self.call_stack.push(dep_fid);
                        }
                    }
                }
                ExprOutcome::Cycle(target_fid) => {
                    self.unwind_cycle(target_fid);
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

    // -------------------------------------------------------- internal helpers

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
                        if let Some(new_f) = self.workbook.formula(sheet, row, col) {
                            let fid = if let Some(reused) = self.free_slots.pop() {
                                reused
                            } else {
                                let fresh = u32::try_from(self.slots.len()).unwrap_or(0);
                                self.slots.push(None);
                                self.states.push(STATE_CLEAN);
                                fresh
                            };
                            let (reg, is_volatile) =
                                self.dep_graph.register_with_flags(fid, key.0, new_f);
                            if is_volatile {
                                self.volatile_set.insert(fid);
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
                    }
                }
            }
        }
        let mut root_dirty = Vec::new();
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
        self.propagate_dirty(root_dirty);
    }

    fn on_rows_undeleted(&mut self, sheet: SheetId, ids: &[IdRun]) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        self.dep_graph
            .on_rows_toggled(sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
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
        self.propagate_dirty(root_dirty);
    }

    fn on_cols_undeleted(&mut self, sheet: SheetId, ids: &[IdRun]) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        self.dep_graph
            .on_cols_toggled(sheet, ids, &self.workbook, |fid| root_dirty.push(fid));
        let start = (sheet, RowId(0), ColId(0));
        let end = (sheet, RowId(u64::MAX), ColId(u64::MAX));
        for (&k @ (_, _, col), _) in self.workbook.formulas().range(start..=end) {
            if ids.iter().any(|r| r.contains(col.0))
                && let Some(&fid) = self.cell_to_fid.get(&k)
            {
                root_dirty.push(fid);
            }
        }
        self.propagate_dirty(root_dirty);
    }

    fn on_axis_reordered(&mut self, sheet: SheetId, rewritten: &[(WorkbookCellKey, BoundFormula)]) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        for &(key, _) in rewritten {
            self.remove_slot_by_key(key);
            if let Some(updated_f) = self.workbook.formula(key.0, key.1, key.2).cloned() {
                let fid = self.insert_slot(key, &updated_f, false);
                root_dirty.push(fid);
            }
        }
        self.dep_graph
            .on_sheet_reordered(sheet, |fid| root_dirty.push(fid));
        self.propagate_dirty(root_dirty);
    }

    fn on_sheet_visibility_changed(&mut self, sheet: SheetId) {
        self.changes.structural = true;
        let mut root_dirty = Vec::new();
        for idx in 0..self.slots.len() {
            if let Some(slot) = self.slots.get(idx).and_then(Option::as_ref)
                && (slot.key.0 == sheet || slot.reg.references_sheet(sheet))
                && let Ok(fid) = u32::try_from(idx)
            {
                root_dirty.push(fid);
            }
        }
        self.propagate_dirty(root_dirty);
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
        let (reg, is_volatile) = self.dep_graph.register_with_flags(fid, key.0, formula);
        if is_volatile {
            self.volatile_set.insert(fid);
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
    }

    fn propagate_dirty(&mut self, roots: Vec<u32>) {
        if roots.is_empty() {
            return;
        }
        self.dirty_queue.reserve(roots.len());
        let mut bfs: Vec<u32> = Vec::with_capacity(roots.len());
        for fid in roots {
            if self.state_of(fid) == STATE_CLEAN && self.slot_of(fid).is_some() {
                self.set_state(fid, STATE_DIRTY);
                self.dirty_queue.push(fid);
                bfs.push(fid);
            }
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
            for dep_fid in newly_found {
                if self.state_of(dep_fid) == STATE_CLEAN && self.slot_of(dep_fid).is_some() {
                    self.set_state(dep_fid, STATE_DIRTY);
                    self.dirty_queue.push(dep_fid);
                    bfs.push(dep_fid);
                }
            }
        }
        if !bfs.is_empty() {
            self.dirty_needs_sort = true;
        }
    }

    fn abort_paused_call_stack(&mut self) {
        if self.call_stack.is_empty() {
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

    fn unwind_cycle(&mut self, target_fid: u32) {
        let start_idx = self
            .call_stack
            .iter()
            .rposition(|&f| f == target_fid)
            .unwrap_or(0);
        while self.call_stack.len() > start_idx {
            let Some(fid) = self.call_stack.pop() else {
                break;
            };
            if let Some(key) = self.slot_of(fid).map(|s| s.key) {
                self.store_eval_scalar(key, EvalScalar::Error(CalcError::Cycle));
            }
            self.set_state(fid, STATE_CLEAN);
            self.wave_done = self.wave_done.saturating_add(1);
        }
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
            BoundExpr::Name(_) => {
                ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(CalcError::Name)))
            }
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
                ExprOutcome::Ready(eval_unary(*op, inner_val, &self.workbook, caller))
            }
            BoundExpr::Binary { op, left, right } => {
                let lhs = match self.eval_expr(left, caller, false, next_depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&lhs) {
                    return wait;
                }
                let rhs = match self.eval_expr(right, caller, false, next_depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&rhs) {
                    return wait;
                }
                let snap = is_outermost && matches!(op, BinaryOp::Add | BinaryOp::Sub);
                ExprOutcome::Ready(eval_binary(*op, lhs, rhs, snap, &self.workbook, caller))
            }
            BoundExpr::Call { name, args } => self.eval_call(name, args, caller, next_depth),
        }
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
            let cond_sa = match coerce_for_scalar_op(cond_raw, &self.workbook, caller) {
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

            let when_true = if need_true {
                let Some(t_expr) = args.get(1) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Value,
                    )));
                };
                let tv = match self.eval_expr(t_expr, caller, false, depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&tv) {
                    return wait;
                }
                Some(tv)
            } else {
                None
            };

            let when_false = if need_false {
                let Some(f_expr) = args.get(2) else {
                    return ExprOutcome::Ready(EvalValue::Scalar(EvalScalar::Error(
                        CalcError::Value,
                    )));
                };
                let fv = match self.eval_expr(f_expr, caller, false, depth) {
                    ExprOutcome::Ready(v) => v,
                    other => return other,
                };
                if let Err(wait) = self.ensure_value_ready(&fv) {
                    return wait;
                }
                Some(fv)
            } else {
                None
            };

            return ExprOutcome::Ready(combine_if(
                cond_sa,
                when_true,
                when_false,
                args.len() == 3,
                &self.workbook,
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
            let p_sa = match coerce_for_scalar_op(p_raw, &self.workbook, caller) {
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
            let idx_sa = match coerce_for_scalar_op(idx_raw, &self.workbook, caller) {
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
            for (i, c_expr) in choice_exprs.iter().enumerate() {
                if !needed_mask.get(i).copied().unwrap_or(false) {
                    evaluated_choices.push(None);
                    continue;
                }
                let val = match self.eval_expr(c_expr, caller, false, depth) {
                    ExprOutcome::Ready(v) => v,
                    ExprOutcome::Cycle(c) => return ExprOutcome::Cycle(c),
                    ExprOutcome::Blocked(b) => {
                        for f in b {
                            if !blocked.contains(&f) {
                                blocked.push(f);
                            }
                        }
                        evaluated_choices.push(None);
                        continue;
                    }
                };
                if let Err(wait) = self.ensure_value_ready(&val) {
                    match wait {
                        ExprOutcome::Cycle(c) => return ExprOutcome::Cycle(c),
                        ExprOutcome::Blocked(b) => {
                            for f in b {
                                if !blocked.contains(&f) {
                                    blocked.push(f);
                                }
                            }
                        }
                        ExprOutcome::Ready(_) => {}
                    }
                }
                evaluated_choices.push(Some(val));
            }
            if !blocked.is_empty() {
                return ExprOutcome::Blocked(blocked);
            }
            return ExprOutcome::Ready(combine_choose(
                idx_sa,
                evaluated_choices,
                &self.workbook,
                caller,
            ));
        }

        // 2. Coordinate/shape-only functions (`ROW`, `COLUMN`, `ROWS`, `COLUMNS`)
        // do NOT dereference cell values inside their reference argument!
        let deref_args = !matches!(name, "ROW" | "COLUMN" | "ROWS" | "COLUMNS");

        let mut evaluated_args = Vec::with_capacity(args.len());
        let mut all_blocked: SmallVec<[u32; 4]> = SmallVec::new();
        for arg in args {
            let val = match self.eval_expr(arg, caller, false, depth) {
                ExprOutcome::Ready(v) => v,
                ExprOutcome::Cycle(c) => return ExprOutcome::Cycle(c),
                ExprOutcome::Blocked(b) => {
                    for f in b {
                        if !all_blocked.contains(&f) {
                            all_blocked.push(f);
                        }
                    }
                    continue;
                }
            };
            if deref_args && let Err(wait) = self.ensure_value_ready(&val) {
                match wait {
                    ExprOutcome::Cycle(c) => return ExprOutcome::Cycle(c),
                    ExprOutcome::Blocked(b) => {
                        for f in b {
                            if !all_blocked.contains(&f) {
                                all_blocked.push(f);
                            }
                        }
                    }
                    ExprOutcome::Ready(_) => {}
                }
            }
            evaluated_args.push(val);
        }
        if !all_blocked.is_empty() {
            return ExprOutcome::Blocked(all_blocked);
        }

        ExprOutcome::Ready(eval_eager_function(
            name,
            evaluated_args,
            &self.workbook,
            caller,
            &mut self.env,
        ))
    }

    fn ensure_value_ready(&self, val: &EvalValue) -> Result<(), ExprOutcome> {
        let EvalValue::Reference(r) = val else {
            return Ok(());
        };
        match r {
            ResolvedRef::Invalid(_) => Ok(()),
            ResolvedRef::Cell { sheet, row, col } => {
                if let Some(&fid) = self.cell_to_fid.get(&(*sheet, *row, *col)) {
                    match self.state_of(fid) {
                        STATE_EVALUATING => return Err(ExprOutcome::Cycle(fid)),
                        STATE_DIRTY => {
                            let mut b = SmallVec::new();
                            b.push(fid);
                            return Err(ExprOutcome::Blocked(b));
                        }
                        _ => {}
                    }
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
        if live_span_positions(sheet.row_axis(), first_row, last_row).is_none() {
            return Ok(());
        }

        let mut active_cols: SmallVec<[ColId; 4]> = SmallVec::new();
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
                }
            });
        if active_cols.is_empty() {
            return Ok(());
        }

        let mut blocked: SmallVec<[u32; 4]> = SmallVec::new();
        let mut cycle_target: Option<u32> = None;
        sheet
            .row_axis()
            .for_each_live_in_range(first_row, last_row, |run| {
                if cycle_target.is_some() {
                    return;
                }
                for raw_r in run.iter() {
                    let row = RowId(raw_r);
                    for &col in &active_cols {
                        if let Some(&fid) = self.cell_to_fid.get(&(sheet_id, row, col)) {
                            match self.state_of(fid) {
                                STATE_EVALUATING => {
                                    cycle_target = Some(fid);
                                    return;
                                }
                                STATE_DIRTY => {
                                    blocked.push(fid);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });

        if let Some(cyc) = cycle_target {
            return Err(ExprOutcome::Cycle(cyc));
        }
        if !blocked.is_empty() {
            return Err(ExprOutcome::Blocked(blocked));
        }
        Ok(())
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

    use reticula_core::id::{ColId, IdRun, RowId, SheetId};
    use reticula_core::ops::Op;
    use reticula_core::value::{CalcError, Value};

    use super::{FormulaEngine, RecalcStatus};

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
}
