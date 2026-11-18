//! Workbook state, atomic identity-addressed operations, limits and inverses.
//!
//! Every mutating operation names its targets by stable [`SheetId`], [`RowId`]
//! and [`ColId`] (D-022). Before touching state, [`Workbook::apply`] validates
//! all structural and size invariants so any rejected operation leaves the
//! workbook bit-for-bit unchanged and never panics.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::axis::{AxisError, MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN};
use crate::digest::Digest64;
use crate::format::NumberFormat;
use crate::id::{ColId, IdAllocator, IdRun, RowId, SheetId};
use crate::sheet::Sheet;
use crate::value::{CalcError, Value};

/// Maximum worksheets in one workbook.
pub const MAX_SHEETS: usize = 256;

/// Maximum UTF-16 code units in a single text cell (Excel parity).
pub const MAX_CELL_TEXT_UTF16: usize = 32_767;

/// Maximum Unicode scalar values in a formula string.
pub const MAX_FORMULA_CHARS: usize = 8_192;

/// Maximum discrete cell edits in a single [`Op::SetCells`] transaction.
pub const MAX_EDITS_PER_OP: usize = 10_000;

/// Maximum estimated heap footprint of a local workbook (`1 GiB`).
pub const MAX_WORKBOOK_BYTES: usize = 1_073_741_824;

/// Evaluation mode of a formula (D-028).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FormulaMode {
    /// Dynamic array mode (authored formulas in Reticula and modern Excel).
    #[default]
    Dynamic,
    /// Legacy implicit-intersection mode (imported pre-dynamic formulas).
    Legacy,
}

/// Unbound formula text stored when using [`Workbook`] without the formula
/// binder crate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFormula {
    /// Raw formula text beginning with `'='`.
    pub source: String,
    /// Dynamic vs legacy array evaluation mode.
    pub mode: FormulaMode,
}

/// Trait implemented by formula representations stored in [`Workbook`] (both
/// [`RawFormula`] in `reticula-core` and bound formulas in `reticula-formula`).
pub trait FormulaPayload: Clone + PartialEq {
    /// Approximate heap bytes owned by this formula entry.
    fn estimated_bytes(&self) -> usize;

    /// Validates static limits (such as length or nesting) prior to insertion.
    fn validate(&self) -> Result<(), OpError>;

    /// Feeds a deterministic canonical representation into `digest`.
    fn write_digest(&self, digest: &mut Digest64);
}

impl FormulaPayload for RawFormula {
    fn estimated_bytes(&self) -> usize {
        self.source.capacity().saturating_add(size_of::<Self>())
    }

    fn validate(&self) -> Result<(), OpError> {
        if !self.source.starts_with('=') {
            return Err(OpError::InvalidFormula);
        }
        if self.source.chars().count() > MAX_FORMULA_CHARS {
            return Err(OpError::FormulaTooLong);
        }
        Ok(())
    }

    fn write_digest(&self, digest: &mut Digest64) {
        let mode_tag = match self.mode {
            FormulaMode::Dynamic => 1_u8,
            FormulaMode::Legacy => 2_u8,
        };
        digest.write_bytes(&[mode_tag]);
        digest.write_bytes(self.source.as_bytes());
    }
}

/// Self-contained scalar value carried in operations and undo records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScalarValue {
    /// Finite IEEE 754 number (`-0.0` is canonicalised to `+0.0`).
    Number(f64),
    /// UTF-8 string content (interned into the target sheet on apply).
    Text(String),
    /// Logical boolean.
    Bool(bool),
    /// Formula or domain error.
    Error(CalcError),
}

/// Authored content of a single cell.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum CellInput<F = RawFormula> {
    /// Empty cell.
    #[default]
    Empty,
    /// Constant scalar value.
    Scalar(ScalarValue),
    /// Formula payload plus optional initial cached scalar value.
    Formula {
        /// Stored formula representation.
        formula: F,
        /// Optional cached evaluation result (`None` leaves [`Value::Empty`]
        /// until recalculation runs).
        cached: Option<ScalarValue>,
    },
}

/// Change to a cell's explicit per-cell [`NumberFormat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatPatch {
    /// Keep whatever explicit format the cell currently has.
    #[default]
    Keep,
    /// Set an explicit per-cell format.
    Set(NumberFormat),
    /// Remove any explicit per-cell format (fall back to column default).
    Clear,
}

/// One cell mutation within [`Op::SetCells`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellEdit<F = RawFormula> {
    /// Target row identity.
    pub row: RowId,
    /// Target column identity.
    pub col: ColId,
    /// New cell input.
    pub input: CellInput<F>,
    /// Explicit number-format change.
    pub format: FormatPatch,
}

/// Exact prior state of one cell, recorded so [`InverseOp::RestoreCells`]
/// restores values, bound formulas and explicit formats bit-for-bit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellSnapshot<F = RawFormula> {
    /// Target row identity.
    pub row: RowId,
    /// Target column identity.
    pub col: ColId,
    /// Previous cell input (including cached formula value if it was a formula).
    pub input: CellInput<F>,
    /// Previous explicit per-cell format (`None` if inherited from column).
    pub explicit_format: Option<NumberFormat>,
}

/// Atomic workbook mutation (M1a operation set, D-022).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op<F = RawFormula> {
    /// Writes up to [`MAX_EDITS_PER_OP`] cells on one worksheet.
    SetCells {
        /// Target worksheet.
        sheet: SheetId,
        /// Pairwise-distinct cell edits.
        edits: Vec<CellEdit<F>>,
    },
    /// Clears values and formulas across the Cartesian product `rows × cols`.
    ClearRange {
        /// Target worksheet.
        sheet: SheetId,
        /// Target row identity runs.
        rows: Vec<IdRun>,
        /// Target column identity runs.
        cols: Vec<IdRun>,
    },
    /// Inserts a fresh run of row identities directly after `after` (`None`
    /// inserts at the top of the sheet).
    InsertRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Preceding row anchor, or `None` for the start of the axis.
        after: Option<RowId>,
        /// Fresh row identities to insert.
        ids: IdRun,
    },
    /// Inserts a fresh run of column identities directly after `after` (`None`
    /// inserts at the left of the sheet).
    InsertCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Preceding column anchor, or `None` for the start of the axis.
        after: Option<ColId>,
        /// Fresh column identities to insert.
        ids: IdRun,
    },
    /// Marks every row in `ids` deleted (incrementing tombstone counts).
    DeleteRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Target row identity runs.
        ids: Vec<IdRun>,
    },
    /// Marks every column in `ids` deleted (incrementing tombstone counts).
    DeleteCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Target column identity runs.
        ids: Vec<IdRun>,
    },
}

/// Exact in-memory inverse produced by [`Workbook::apply`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InverseOp<F = RawFormula> {
    /// Restores previous cell inputs and explicit formats.
    RestoreCells {
        /// Target worksheet.
        sheet: SheetId,
        /// Prior states of modified cells.
        cells: Vec<CellSnapshot<F>>,
    },
    /// Physically removes rows just inserted by [`Op::InsertRows`].
    RemoveInsertedRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Inserted row run to remove.
        ids: IdRun,
    },
    /// Physically removes columns just inserted by [`Op::InsertCols`].
    RemoveInsertedCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Inserted column run to remove.
        ids: IdRun,
    },
    /// Decrements deletion counts incremented by [`Op::DeleteRows`].
    UndeleteRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Deleted row runs to restore.
        ids: Vec<IdRun>,
    },
    /// Decrements deletion counts incremented by [`Op::DeleteCols`].
    UndeleteCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Deleted column runs to restore.
        ids: Vec<IdRun>,
    },
}

/// Reason an operation or inverse operation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpError {
    /// Referenced [`SheetId`] does not exist in the workbook.
    SheetNotFound,
    /// Workbook already holds [`MAX_SHEETS`] worksheets.
    TooManySheets,
    /// [`Op::SetCells`] exceeded [`MAX_EDITS_PER_OP`].
    TooManyEdits,
    /// Two edits in the same transaction targeted the same `(row, col)`.
    DuplicateCellTarget,
    /// Referenced row or column identity does not exist on the sheet's axis.
    TargetNotFound,
    /// Axis mutation failed validation (anchor missing, duplicate ID, run or
    /// axis length limit exceeded).
    Axis(AxisError),
    /// A numeric scalar was `NaN` or infinite.
    NonFiniteNumber,
    /// A text scalar exceeded [`MAX_CELL_TEXT_UTF16`] UTF-16 code units.
    TextTooLong,
    /// Formula string exceeded [`MAX_FORMULA_CHARS`].
    FormulaTooLong,
    /// Formula payload failed structural validation.
    InvalidFormula,
    /// String table index space was exhausted.
    StringPoolExhausted,
    /// Operation would push the workbook past [`MAX_WORKBOOK_BYTES`].
    MemoryLimitExceeded,
}

impl From<AxisError> for OpError {
    fn from(err: AxisError) -> Self {
        Self::Axis(err)
    }
}

/// Optional provenance metadata when a workbook originated from a deterministic
/// synthetic sample generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedOrigin {
    /// Generator seed.
    pub seed: u64,
    /// Requested data row count.
    pub rows: u32,
    /// Generator format version.
    pub generator_version: u32,
}

/// Cell coordinate across the workbook, ordered deterministically.
pub type WorkbookCellKey = (SheetId, RowId, ColId);

/// Multi-sheet workbook owning sheets, formulas, ID allocator and memory estimate.
#[derive(Clone, Debug)]
pub struct Workbook<F = RawFormula> {
    sheets: Vec<Sheet>,
    formulas: BTreeMap<WorkbookCellKey, F>,
    formulas_bytes: usize,
    allocator: IdAllocator,
    next_sheet_id: u32,
    seed_origin: Option<SeedOrigin>,
}

impl<F: FormulaPayload> Default for Workbook<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FormulaPayload> Workbook<F> {
    /// Creates a workbook containing a single standard empty `Sheet1`.
    #[must_use]
    pub fn new() -> Self {
        let sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
        Self {
            sheets: alloc::vec![sheet],
            formulas: BTreeMap::new(),
            formulas_bytes: 0,
            allocator: IdAllocator::local(),
            next_sheet_id: 1,
            seed_origin: None,
        }
    }

    /// Wraps a pre-built single [`Sheet`] (such as `sample::generate`) into a
    /// workbook.
    #[must_use]
    pub fn from_sheet(sheet: Sheet, seed_origin: Option<SeedOrigin>) -> Self {
        let next_id = sheet.id().0.saturating_add(1);
        Self {
            sheets: alloc::vec![sheet],
            formulas: BTreeMap::new(),
            formulas_bytes: 0,
            allocator: IdAllocator::local(),
            next_sheet_id: next_id,
            seed_origin,
        }
    }

    /// Reconstructs a workbook from decoded snapshot parts.
    pub fn from_parts(
        sheets: Vec<Sheet>,
        formulas: BTreeMap<WorkbookCellKey, F>,
        allocator: IdAllocator,
        next_sheet_id: u32,
        seed_origin: Option<SeedOrigin>,
    ) -> Result<Self, OpError> {
        if sheets.is_empty() || sheets.len() > MAX_SHEETS {
            return Err(OpError::TooManySheets);
        }
        let mut seen_ids = BTreeSet::new();
        for sheet in &sheets {
            if !seen_ids.insert(sheet.id()) {
                return Err(OpError::SheetNotFound);
            }
            if sheet.row_axis().total_len() > MAX_ROW_AXIS_LEN
                || sheet.col_axis().total_len() > MAX_COL_AXIS_LEN
            {
                return Err(OpError::Axis(AxisError::LimitExceeded));
            }
        }
        let mut formulas_bytes = 0usize;
        for (&(sheet_id, row, col), formula) in &formulas {
            let sheet = sheets
                .iter()
                .find(|s| s.id() == sheet_id)
                .ok_or(OpError::SheetNotFound)?;
            if !sheet.row_axis().contains_id(row.0) || !sheet.col_axis().contains_id(col.0) {
                return Err(OpError::TargetNotFound);
            }
            formula.validate()?;
            formulas_bytes = formulas_bytes.saturating_add(formula_entry_bytes(formula));
        }
        let wb = Self {
            sheets,
            formulas,
            formulas_bytes,
            allocator,
            next_sheet_id,
            seed_origin,
        };
        if wb.estimated_bytes() > MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }
        Ok(wb)
    }

    /// Worksheets in workbook order.
    #[must_use]
    pub fn sheets(&self) -> &[Sheet] {
        &self.sheets
    }

    /// Mutable worksheets slice (used by bulk initialisation).
    pub fn sheets_mut(&mut self) -> &mut [Sheet] {
        &mut self.sheets
    }

    /// Looks up a worksheet by [`SheetId`].
    #[must_use]
    pub fn sheet(&self, id: SheetId) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.id() == id)
    }

    /// Looks up a mutable worksheet by [`SheetId`].
    pub fn sheet_mut(&mut self, id: SheetId) -> Option<&mut Sheet> {
        self.sheets.iter_mut().find(|s| s.id() == id)
    }

    /// Appends a new standard worksheet if under [`MAX_SHEETS`].
    pub fn add_sheet(&mut self, name: String) -> Result<SheetId, OpError> {
        if self.sheets.len() >= MAX_SHEETS {
            return Err(OpError::TooManySheets);
        }
        let id = SheetId(self.next_sheet_id);
        self.next_sheet_id = self.next_sheet_id.saturating_add(1);
        self.sheets.push(Sheet::standard(id, name));
        Ok(id)
    }

    /// Stored formula map across the workbook.
    #[must_use]
    pub const fn formulas(&self) -> &BTreeMap<WorkbookCellKey, F> {
        &self.formulas
    }

    /// Looks up the formula at `(sheet, row, col)`.
    #[must_use]
    pub fn formula(&self, sheet: SheetId, row: RowId, col: ColId) -> Option<&F> {
        self.formulas.get(&(sheet, row, col))
    }

    /// Bulk-inserts a pre-validated formula and its initial cached value during
    /// sample generation.
    pub fn insert_seeded_formula(
        &mut self,
        sheet: SheetId,
        row: RowId,
        col: ColId,
        formula: F,
        cached: Value,
    ) -> bool {
        let Some(target) = self.sheet_mut(sheet) else {
            return false;
        };
        if !target.set_value_by_id(row, col, cached) {
            return false;
        }
        self.put_formula((sheet, row, col), formula);
        true
    }

    /// Updates the cached value of `(sheet, row, col)` during formula
    /// recalculation without altering the stored formula entry.
    pub fn set_cached_value(
        &mut self,
        sheet: SheetId,
        row: RowId,
        col: ColId,
        value: Value,
    ) -> bool {
        let Some(target) = self.sheet_mut(sheet) else {
            return false;
        };
        target.set_value_by_id(row, col, value)
    }

    /// Local identity allocator of this workbook.
    #[must_use]
    pub const fn allocator(&self) -> &IdAllocator {
        &self.allocator
    }

    /// Mutable identity allocator for issuing fresh row or column [`IdRun`]s.
    pub const fn allocator_mut(&mut self) -> &mut IdAllocator {
        &mut self.allocator
    }

    /// Next sheet identifier counter.
    #[must_use]
    pub const fn next_sheet_id(&self) -> u32 {
        self.next_sheet_id
    }

    /// Optional sample-seed provenance.
    #[must_use]
    pub const fn seed_origin(&self) -> Option<SeedOrigin> {
        self.seed_origin
    }

    /// Approximate heap bytes owned by all sheets and formulas in this workbook.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let sheets_mem = self
            .sheets
            .iter()
            .fold(0usize, |acc, s| acc.saturating_add(s.estimated_bytes()));
        sheets_mem.saturating_add(self.formulas_bytes)
    }

    /// Reads the complete logical [`CellSnapshot`] of `(sheet, row, col)`.
    #[must_use]
    pub fn cell_snapshot(
        &self,
        sheet_id: SheetId,
        row: RowId,
        col: ColId,
    ) -> Option<CellSnapshot<F>> {
        let sheet = self.sheet(sheet_id)?;
        let val = sheet.value_by_id(row, col);
        let scalar = value_to_scalar(sheet, val);
        let input = if let Some(formula) = self.formulas.get(&(sheet_id, row, col)) {
            CellInput::Formula {
                formula: formula.clone(),
                cached: scalar,
            }
        } else {
            match scalar {
                None => CellInput::Empty,
                Some(s) => CellInput::Scalar(s),
            }
        };
        Some(CellSnapshot {
            row,
            col,
            input,
            explicit_format: sheet.cell_format_by_id(row, col),
        })
    }

    /// Validates and applies `op` atomically, returning its exact [`InverseOp`].
    pub fn apply(&mut self, op: Op<F>) -> Result<InverseOp<F>, OpError> {
        match op {
            Op::SetCells { sheet, edits } => self.apply_set_cells(sheet, edits),
            Op::ClearRange { sheet, rows, cols } => self.apply_clear_range(sheet, &rows, &cols),
            Op::InsertRows { sheet, after, ids } => self.apply_insert_rows(sheet, after, ids),
            Op::InsertCols { sheet, after, ids } => self.apply_insert_cols(sheet, after, ids),
            Op::DeleteRows { sheet, ids } => self.apply_delete_rows(sheet, ids),
            Op::DeleteCols { sheet, ids } => self.apply_delete_cols(sheet, ids),
        }
    }

    /// Applies an [`InverseOp`] previously returned by [`Self::apply`].
    pub fn apply_inverse(&mut self, inverse: InverseOp<F>) -> Result<(), OpError> {
        match inverse {
            InverseOp::RestoreCells { sheet, cells } => self.restore_cells(sheet, cells),
            InverseOp::RemoveInsertedRows { sheet, ids } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.row_axis_mut().remove_inserted(ids)?;
                Ok(())
            }
            InverseOp::RemoveInsertedCols { sheet, ids } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.col_axis_mut().remove_inserted(ids)?;
                Ok(())
            }
            InverseOp::UndeleteRows { sheet, ids } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.row_axis_mut().undelete_runs(&ids)?;
                Ok(())
            }
            InverseOp::UndeleteCols { sheet, ids } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.col_axis_mut().undelete_runs(&ids)?;
                Ok(())
            }
        }
    }

    /// Computes a deterministic 64-bit digest of the workbook's visible and
    /// tombstoned axis structure, stored cell values, explicit formats and
    /// formulas.
    ///
    /// String pool capacity or orphaned interned strings that are no longer
    /// referenced by any cell do not affect the logical digest, so `apply`
    /// followed by `apply_inverse` always restores the exact same digest.
    #[must_use]
    pub fn logical_digest(&self) -> u64 {
        let mut d = Digest64::new();
        d.write_u32(u32::try_from(self.sheets.len()).unwrap_or(u32::MAX));
        for sheet in &self.sheets {
            d.write_u32(sheet.id().0);
            d.write_bytes(sheet.name().as_bytes());

            // Row and column axes (runs + tombstones).
            write_axis_digest(sheet.row_axis(), &mut d);
            write_axis_digest(sheet.col_axis(), &mut d);

            // Column default formats for columns that have non-General defaults.
            for (slot_idx, &fmt) in sheet.col_formats().iter().enumerate() {
                if fmt != NumberFormat::General
                    && let Ok(slot_u32) = u32::try_from(slot_idx)
                    && let Some(col_id) = sheet.col_slots().slot_to_id(slot_u32)
                {
                    d.write_u64(col_id);
                    d.write_bytes(&[format_tag(fmt)]);
                }
            }

            // Explicit per-cell formats (BTreeMap is already sorted by (RowId, ColId)).
            d.write_u32(u32::try_from(sheet.cell_formats().len()).unwrap_or(u32::MAX));
            for (&(row, col), &fmt) in sheet.cell_formats() {
                d.write_u64(row.0);
                d.write_u64(col.0);
                d.write_bytes(&[format_tag(fmt)]);
            }

            // Non-empty cell values sorted by `(ColId, RowId)` so slot allocation
            // history never perturbs logical equivalence.
            let mut non_empty: Vec<(u64, u64, Value)> = Vec::new();
            for (col_slot_idx, column) in sheet.raw_columns().iter().enumerate() {
                let Ok(col_slot) = u32::try_from(col_slot_idx) else {
                    continue;
                };
                let Some(col_id) = sheet.col_slots().slot_to_id(col_slot) else {
                    continue;
                };
                column.for_each_non_empty(|row_slot, val| {
                    if let Some(row_id) = sheet.row_slots().slot_to_id(row_slot) {
                        non_empty.push((col_id, row_id, val));
                    }
                });
            }
            non_empty.sort_unstable_by_key(|&(c, r, _)| (c, r));
            d.write_u32(u32::try_from(non_empty.len()).unwrap_or(u32::MAX));
            for (col_id, row_id, val) in non_empty {
                d.write_u64(col_id);
                d.write_u64(row_id);
                write_value_digest(sheet, val, &mut d);
            }
        }

        // Formulas (BTreeMap is already sorted by (SheetId, RowId, ColId)).
        d.write_u32(u32::try_from(self.formulas.len()).unwrap_or(u32::MAX));
        for (&(sheet_id, row, col), formula) in &self.formulas {
            d.write_u32(sheet_id.0);
            d.write_u64(row.0);
            d.write_u64(col.0);
            formula.write_digest(&mut d);
        }
        d.finish()
    }

    fn apply_set_cells(
        &mut self,
        sheet_id: SheetId,
        edits: Vec<CellEdit<F>>,
    ) -> Result<InverseOp<F>, OpError> {
        if edits.len() > MAX_EDITS_PER_OP {
            return Err(OpError::TooManyEdits);
        }

        let added_bytes: usize = {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            let mut seen = BTreeSet::new();
            let mut extra_bytes = 0usize;
            for edit in &edits {
                if !seen.insert((edit.row, edit.col)) {
                    return Err(OpError::DuplicateCellTarget);
                }
                if !sheet.row_axis().contains_id(edit.row.0)
                    || !sheet.col_axis().contains_id(edit.col.0)
                {
                    return Err(OpError::TargetNotFound);
                }
                extra_bytes = extra_bytes.saturating_add(validate_cell_input(&edit.input)?);
            }
            extra_bytes
        };

        if self.estimated_bytes().saturating_add(added_bytes) > MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }

        // Changing a cell invalidates pure sample-seed equivalence.
        self.seed_origin = None;

        let mut prior = Vec::with_capacity(edits.len());
        for edit in &edits {
            if let Some(snap) = self.cell_snapshot(sheet_id, edit.row, edit.col) {
                prior.push(snap);
            }
        }

        for edit in edits {
            self.write_validated_cell(sheet_id, edit.row, edit.col, edit.input, edit.format)?;
        }

        Ok(InverseOp::RestoreCells {
            sheet: sheet_id,
            cells: prior,
        })
    }

    fn apply_clear_range(
        &mut self,
        sheet_id: SheetId,
        rows: &[IdRun],
        cols: &[IdRun],
    ) -> Result<InverseOp<F>, OpError> {
        let targets: Vec<(RowId, ColId)> = {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.row_axis().validate_existing_runs(rows)?;
            sheet.col_axis().validate_existing_runs(cols)?;

            let mut affected = BTreeSet::new();
            // 1. Non-empty stored values in matching columns.
            for col_run in cols {
                for raw_col_id in col_run.iter() {
                    if let Some(col_slot) = sheet.col_slots().get(raw_col_id)
                        && let Some(column) = sheet.raw_columns().get(col_slot as usize)
                    {
                        column.for_each_non_empty(|row_slot, _| {
                            if let Some(raw_row_id) = sheet.row_slots().slot_to_id(row_slot)
                                && id_in_runs(raw_row_id, rows)
                            {
                                affected.insert((RowId(raw_row_id), ColId(raw_col_id)));
                            }
                        });
                    }
                }
            }
            // 2. Stored formulas on `sheet_id` within `rows × cols` (including
            // formulas whose cached value is currently Empty).
            let start_key = (sheet_id, RowId(0), ColId(0));
            let end_key = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
            for (&(_, r, c), _) in self.formulas.range(start_key..=end_key) {
                if id_in_runs(r.0, rows) && id_in_runs(c.0, cols) {
                    affected.insert((r, c));
                }
            }
            affected.into_iter().collect()
        };

        self.seed_origin = None;

        let mut prior = Vec::with_capacity(targets.len());
        for &(row, col) in &targets {
            if let Some(snap) = self.cell_snapshot(sheet_id, row, col) {
                prior.push(snap);
            }
        }
        for (row, col) in targets {
            self.write_validated_cell(sheet_id, row, col, CellInput::Empty, FormatPatch::Keep)?;
        }

        Ok(InverseOp::RestoreCells {
            sheet: sheet_id,
            cells: prior,
        })
    }

    fn apply_insert_rows(
        &mut self,
        sheet_id: SheetId,
        after: Option<RowId>,
        ids: IdRun,
    ) -> Result<InverseOp<F>, OpError> {
        if self.estimated_bytes() >= MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        target
            .row_axis_mut()
            .insert_after(after.map(|r| r.0), ids)?;
        self.seed_origin = None;
        Ok(InverseOp::RemoveInsertedRows {
            sheet: sheet_id,
            ids,
        })
    }

    fn apply_insert_cols(
        &mut self,
        sheet_id: SheetId,
        after: Option<ColId>,
        ids: IdRun,
    ) -> Result<InverseOp<F>, OpError> {
        if self.estimated_bytes() >= MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        target
            .col_axis_mut()
            .insert_after(after.map(|c| c.0), ids)?;
        self.seed_origin = None;
        Ok(InverseOp::RemoveInsertedCols {
            sheet: sheet_id,
            ids,
        })
    }

    fn apply_delete_rows(
        &mut self,
        sheet_id: SheetId,
        ids: Vec<IdRun>,
    ) -> Result<InverseOp<F>, OpError> {
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        target.row_axis_mut().delete_runs(&ids)?;
        self.seed_origin = None;
        Ok(InverseOp::UndeleteRows {
            sheet: sheet_id,
            ids,
        })
    }

    fn apply_delete_cols(
        &mut self,
        sheet_id: SheetId,
        ids: Vec<IdRun>,
    ) -> Result<InverseOp<F>, OpError> {
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        target.col_axis_mut().delete_runs(&ids)?;
        self.seed_origin = None;
        Ok(InverseOp::UndeleteCols {
            sheet: sheet_id,
            ids,
        })
    }

    fn restore_cells(
        &mut self,
        sheet_id: SheetId,
        cells: Vec<CellSnapshot<F>>,
    ) -> Result<(), OpError> {
        {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            for cell in &cells {
                if !sheet.row_axis().contains_id(cell.row.0)
                    || !sheet.col_axis().contains_id(cell.col.0)
                {
                    return Err(OpError::TargetNotFound);
                }
                validate_cell_input(&cell.input)?;
            }
        }
        for cell in cells {
            let patch = match cell.explicit_format {
                Some(fmt) => FormatPatch::Set(fmt),
                None => FormatPatch::Clear,
            };
            self.write_validated_cell(sheet_id, cell.row, cell.col, cell.input, patch)?;
        }
        Ok(())
    }

    fn put_formula(&mut self, key: WorkbookCellKey, formula: F) {
        let added = formula_entry_bytes(&formula);
        if let Some(old) = self.formulas.insert(key, formula) {
            self.formulas_bytes = self
                .formulas_bytes
                .saturating_sub(formula_entry_bytes(&old));
        }
        self.formulas_bytes = self.formulas_bytes.saturating_add(added);
    }

    fn take_formula(&mut self, key: WorkbookCellKey) {
        if let Some(old) = self.formulas.remove(&key) {
            self.formulas_bytes = self
                .formulas_bytes
                .saturating_sub(formula_entry_bytes(&old));
        }
    }

    fn write_validated_cell(
        &mut self,
        sheet_id: SheetId,
        row: RowId,
        col: ColId,
        input: CellInput<F>,
        format: FormatPatch,
    ) -> Result<(), OpError> {
        match input {
            CellInput::Empty => {
                self.take_formula((sheet_id, row, col));
                let sheet = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
                sheet.set_value_by_id(row, col, Value::Empty);
                apply_format_patch(sheet, row, col, format);
            }
            CellInput::Scalar(scalar) => {
                self.take_formula((sheet_id, row, col));
                let sheet = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
                let value = intern_scalar(sheet, scalar)?;
                sheet.set_value_by_id(row, col, value);
                apply_format_patch(sheet, row, col, format);
            }
            CellInput::Formula { formula, cached } => {
                let sheet = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
                let cached_val = match cached {
                    None => Value::Empty,
                    Some(s) => intern_scalar(sheet, s)?,
                };
                sheet.set_value_by_id(row, col, cached_val);
                apply_format_patch(sheet, row, col, format);
                self.put_formula((sheet_id, row, col), formula);
            }
        }
        Ok(())
    }
}

fn formula_entry_bytes<F: FormulaPayload>(formula: &F) -> usize {
    formula
        .estimated_bytes()
        .saturating_add(size_of::<WorkbookCellKey>().saturating_mul(2))
}

fn validate_scalar(scalar: &ScalarValue) -> Result<usize, OpError> {
    match scalar {
        ScalarValue::Number(n) => {
            if !n.is_finite() {
                return Err(OpError::NonFiniteNumber);
            }
            Ok(8)
        }
        ScalarValue::Text(text) => {
            if text.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
                return Err(OpError::TextTooLong);
            }
            Ok(text.len().saturating_add(16))
        }
        ScalarValue::Bool(_) | ScalarValue::Error(_) => Ok(8),
    }
}

fn validate_cell_input<F: FormulaPayload>(input: &CellInput<F>) -> Result<usize, OpError> {
    match input {
        CellInput::Empty => Ok(0),
        CellInput::Scalar(s) => validate_scalar(s),
        CellInput::Formula { formula, cached } => {
            formula.validate()?;
            let cached_bytes = match cached {
                None => 0,
                Some(s) => validate_scalar(s)?,
            };
            Ok(formula.estimated_bytes().saturating_add(cached_bytes))
        }
    }
}

fn intern_scalar(sheet: &mut Sheet, scalar: ScalarValue) -> Result<Value, OpError> {
    match scalar {
        ScalarValue::Number(n) => Ok(Value::number(n)),
        ScalarValue::Text(text) => {
            let id = sheet
                .add_string(&text)
                .ok_or(OpError::StringPoolExhausted)?;
            Ok(Value::Text(id))
        }
        ScalarValue::Bool(flag) => Ok(Value::Bool(flag)),
        ScalarValue::Error(err) => Ok(Value::Error(err)),
    }
}

fn value_to_scalar(sheet: &Sheet, value: Value) -> Option<ScalarValue> {
    match value {
        Value::Empty => None,
        Value::Number(n) => Some(ScalarValue::Number(if n == 0.0 { 0.0 } else { n })),
        Value::Text(id) => Some(ScalarValue::Text(String::from(sheet.strings().get(id)))),
        Value::Bool(flag) => Some(ScalarValue::Bool(flag)),
        Value::Error(err) => Some(ScalarValue::Error(err)),
        Value::Array(_) => Some(ScalarValue::Error(CalcError::Spill)),
    }
}

fn apply_format_patch(sheet: &mut Sheet, row: RowId, col: ColId, patch: FormatPatch) {
    match patch {
        FormatPatch::Keep => {}
        FormatPatch::Set(fmt) => {
            sheet.set_cell_format_by_id(row, col, Some(fmt));
        }
        FormatPatch::Clear => {
            sheet.set_cell_format_by_id(row, col, None);
        }
    }
}

fn id_in_runs(id: u64, runs: &[IdRun]) -> bool {
    runs.iter().any(|run| run.contains(id))
}

fn write_axis_digest(axis: &crate::axis::Axis, d: &mut Digest64) {
    use crate::id::{id_counter, id_stream};

    d.write_u32(axis.live_len());
    d.write_u32(axis.total_len());

    // Coalesce adjacent contiguous `IdRun`s so internal run-splitting history
    // (e.g. splitting a base run around a tombstoned prefix and later undoing an
    // insertion) never alters the logical digest.
    let mut coalesced: Vec<IdRun> = Vec::new();
    let mut tombstoned: Vec<(u64, u32)> = Vec::new();

    for run in axis.runs() {
        if let Some(last) = coalesced.last_mut()
            && id_stream(last.first_id) == id_stream(run.ids.first_id)
            && id_counter(last.first_id).checked_add(last.len) == Some(id_counter(run.ids.first_id))
        {
            last.len = last.len.saturating_add(run.ids.len);
        } else {
            coalesced.push(run.ids);
        }

        if let Some(counts) = &run.tombstones {
            for (idx, &cnt) in counts.iter().enumerate() {
                if cnt > 0
                    && let Ok(off) = u32::try_from(idx)
                    && let Some(id) = run.ids.id_at(off)
                {
                    tombstoned.push((id, cnt));
                }
            }
        }
    }

    d.write_u32(u32::try_from(coalesced.len()).unwrap_or(u32::MAX));
    for r in coalesced {
        d.write_u64(r.first_id);
        d.write_u32(r.len);
    }
    d.write_u32(u32::try_from(tombstoned.len()).unwrap_or(u32::MAX));
    for (id, cnt) in tombstoned {
        d.write_u64(id);
        d.write_u32(cnt);
    }
}

const fn format_tag(fmt: NumberFormat) -> u8 {
    match fmt {
        NumberFormat::General => 0,
        NumberFormat::Fixed2 => 1,
        NumberFormat::Percent => 2,
        NumberFormat::Date => 3,
    }
}

fn write_value_digest(sheet: &Sheet, val: Value, d: &mut Digest64) {
    match val {
        Value::Empty => d.write_bytes(&[0]),
        Value::Number(n) => {
            d.write_bytes(&[1]);
            d.write_f64(n);
        }
        Value::Text(id) => {
            d.write_bytes(&[2]);
            d.write_bytes(sheet.strings().get(id).as_bytes());
        }
        Value::Bool(flag) => d.write_bytes(&[3, u8::from(flag)]),
        Value::Error(err) => d.write_bytes(&[4, err as u8]),
        Value::Array(id) => {
            d.write_bytes(&[5]);
            d.write_u32(id);
        }
    }
}

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

    use alloc::string::String;
    use alloc::vec;

    use super::{
        CellEdit, CellInput, FormatPatch, FormulaMode, Op, OpError, RawFormula, ScalarValue,
        Workbook,
    };
    use crate::format::NumberFormat;
    use crate::id::{ColId, IdRun, RowId, SheetId, pack_id};
    use crate::value::{CalcError, Value};

    #[test]
    fn every_op_and_its_inverse_round_trip_to_identical_digest() {
        let mut wb: Workbook = Workbook::new();
        let sheet = SheetId(0);
        let initial_digest = wb.logical_digest();

        // 1. SetCells (scalar + formula + explicit format).
        let op1 = Op::SetCells {
            sheet,
            edits: vec![
                CellEdit {
                    row: RowId(0),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Text(String::from("Price"))),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(1),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(19.95)),
                    format: FormatPatch::Set(NumberFormat::Fixed2),
                },
                CellEdit {
                    row: RowId(1),
                    col: ColId(1),
                    input: CellInput::Formula {
                        formula: RawFormula {
                            source: String::from("=A2*2"),
                            mode: FormulaMode::Dynamic,
                        },
                        cached: Some(ScalarValue::Number(39.9)),
                    },
                    format: FormatPatch::Keep,
                },
            ],
        };
        let inv1 = wb.apply(op1).unwrap();
        let after_op1 = wb.logical_digest();
        assert_ne!(after_op1, initial_digest);

        // 2. InsertRows at top.
        let inserted_rows = wb.allocator_mut().alloc_run(2).unwrap();
        let inv2 = wb
            .apply(Op::InsertRows {
                sheet,
                after: None,
                ids: inserted_rows,
            })
            .unwrap();
        let after_op2 = wb.logical_digest();

        // 3. DeleteRows (including double delete on row 1).
        let row1_run = IdRun::new(1, 1).unwrap();
        let inv3 = wb
            .apply(Op::DeleteRows {
                sheet,
                ids: vec![row1_run],
            })
            .unwrap();
        let after_op3 = wb.logical_digest();
        let inv4 = wb
            .apply(Op::DeleteRows {
                sheet,
                ids: vec![row1_run],
            })
            .unwrap();

        // 4. ClearRange over `(0..=1) x (0..=1)`.
        let inv5 = wb
            .apply(Op::ClearRange {
                sheet,
                rows: vec![IdRun::new(0, 2).unwrap()],
                cols: vec![IdRun::new(0, 2).unwrap()],
            })
            .unwrap();

        // Unwind in LIFO order and verify exact digest restoration at each step.
        wb.apply_inverse(inv5).unwrap();
        wb.apply_inverse(inv4).unwrap();
        assert_eq!(wb.logical_digest(), after_op3);
        wb.apply_inverse(inv3).unwrap();
        assert_eq!(wb.logical_digest(), after_op2);
        wb.apply_inverse(inv2).unwrap();
        assert_eq!(wb.logical_digest(), after_op1);
        wb.apply_inverse(inv1).unwrap();
        assert_eq!(wb.logical_digest(), initial_digest);
    }

    #[test]
    fn invalid_operations_are_rejected_atomically_without_mutating_state() {
        let mut wb: Workbook = Workbook::new();
        let sheet = SheetId(0);
        wb.apply(Op::SetCells {
            sheet,
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Number(42.0)),
                format: FormatPatch::Keep,
            }],
        })
        .unwrap();
        let baseline = wb.logical_digest();

        // Duplicate target in same SetCells batch -> rejected, row (0,0) untouched.
        let dup = wb.apply(Op::SetCells {
            sheet,
            edits: vec![
                CellEdit {
                    row: RowId(0),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(1.0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(0),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(2.0)),
                    format: FormatPatch::Keep,
                },
            ],
        });
        assert_eq!(dup, Err(OpError::DuplicateCellTarget));
        assert_eq!(wb.logical_digest(), baseline);

        // Unknown row ID -> rejected, earlier edits in same batch not applied.
        let unknown_row = RowId(pack_id(9, 99, 1).unwrap());
        let bad_target = wb.apply(Op::SetCells {
            sheet,
            edits: vec![
                CellEdit {
                    row: RowId(0),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(999.0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: unknown_row,
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Error(CalcError::Na)),
                    format: FormatPatch::Keep,
                },
            ],
        });
        assert_eq!(bad_target, Err(OpError::TargetNotFound));
        assert_eq!(wb.sheet(sheet).unwrap().value(0, 0), Value::Number(42.0));
        assert_eq!(wb.logical_digest(), baseline);

        // NaN number -> rejected.
        let nan_edit = wb.apply(Op::SetCells {
            sheet,
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(f64::NAN)),
                format: FormatPatch::Keep,
            }],
        });
        assert_eq!(nan_edit, Err(OpError::NonFiniteNumber));
        assert_eq!(wb.logical_digest(), baseline);
    }
}
