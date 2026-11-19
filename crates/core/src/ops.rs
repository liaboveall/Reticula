//! Workbook state, atomic identity-addressed operations, limits and inverses
//! (`13-m1a.md` 5.3 & `14-m1b.md` 5.1–5.2).
//!
//! Every mutating operation names its targets by stable [`SheetId`], [`RowId`]
//! and [`ColId`] (D-022). Before touching state, [`Workbook::apply`] validates
//! all structural and size invariants so any rejected operation leaves the
//! workbook bit-for-bit unchanged and never panics.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::axis::{Axis, AxisError, AxisRun, MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN};
use crate::digest::Digest64;
use crate::format::NumberFormat;
use crate::id::{ColId, IdAllocator, IdRun, RowId, SheetId};
use crate::sheet::{
    MAX_COL_WIDTH, MAX_FREEZE_COUNT, MAX_ROW_HEIGHT, MAX_SHEET_NAME_UTF16, MIN_COL_WIDTH,
    MIN_ROW_HEIGHT, Sheet, is_valid_sheet_name,
};
use crate::style::StylePatch;
use crate::value::{CalcError, Value};

/// Maximum worksheets in one workbook.
pub const MAX_SHEETS: usize = 256;

/// Maximum UTF-16 code units in a single text cell (Excel parity).
pub const MAX_CELL_TEXT_UTF16: usize = 32_767;

/// Maximum Unicode scalar values in a formula string.
pub const MAX_FORMULA_CHARS: usize = 8_192;

/// Maximum discrete cell edits in a single [`Op::SetCells`] transaction.
pub const MAX_EDITS_PER_OP: usize = 10_000;

/// Maximum cells materialised by a single [`Op::SetBlock`] or [`Op::FillFormula`].
pub const MAX_BULK_CELLS_PER_OP: usize = 2_000_000;

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
/// [`RawFormula`] in `reticula-core` and `BoundFormula` in `reticula-formula`).
pub trait FormulaPayload: Clone + PartialEq {
    /// Compact relative-ID formula template carried by [`Op::FillFormula`].
    type Template: Clone + core::fmt::Debug + PartialEq + Serialize + DeserializeOwned;

    /// Approximate heap bytes owned by this formula entry.
    fn estimated_bytes(&self) -> usize;

    /// Validates static limits (such as length or nesting) prior to insertion.
    fn validate(&self) -> Result<(), OpError>;

    /// Feeds a deterministic canonical representation into `digest`.
    fn write_digest(&self, digest: &mut Digest64);

    /// Validates static invariants on a [`Op::FillFormula`] template and returns
    /// the approximate heap bytes per expanded formula instance.
    fn validate_template(template: &Self::Template) -> Result<usize, OpError>;

    /// Instantiates `template` for target cell `(row, col)`.
    fn expand_template(template: &Self::Template, row: RowId, col: ColId) -> Self;

    /// Appends `(target_sheet, row_anchor)` and `(target_sheet, col_anchor)`
    /// range endpoints referenced by this formula so [`Workbook`] can execute
    /// E25 anchor handoff in `O(affected formulas)` without scanning the sheet.
    fn collect_range_endpoints(
        &self,
        _host_sheet: SheetId,
        _row_endpoints: &mut Vec<(SheetId, RowId)>,
        _col_endpoints: &mut Vec<(SheetId, ColId)>,
    ) {
    }

    /// Performs E25 range-anchor handoff before `moved` rows on `moved_sheet`
    /// are relocated to `after`. Returns `true` if any anchor in `self` changed.
    fn handoff_row_anchors(
        &mut self,
        _host_sheet: SheetId,
        _moved_sheet: SheetId,
        _row_axis: &Axis,
        _moved: &[IdRun],
        _after: Option<RowId>,
    ) -> bool {
        false
    }

    /// Performs E25 range-anchor handoff before `moved` columns on
    /// `moved_sheet` are relocated to `after`. Returns `true` if `self` changed.
    fn handoff_col_anchors(
        &mut self,
        _host_sheet: SheetId,
        _moved_sheet: SheetId,
        _col_axis: &Axis,
        _moved: &[IdRun],
        _after: Option<ColId>,
    ) -> bool {
        false
    }

    /// Solidifies references prior to local snapshot tombstone GC (`14-m1b.md`
    /// 5.5): single-tombstone range endpoints hand off to live neighbours while
    /// deleted single-cell, all-deleted range, or deleted-sheet references turn
    /// into `#REF!`.
    fn finalize_purged_tombstones(&mut self, _host_sheet: SheetId, _sheets: &[Sheet]) -> bool {
        false
    }
}

impl FormulaPayload for RawFormula {
    type Template = Self;

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

    fn validate_template(template: &Self::Template) -> Result<usize, OpError> {
        template.validate()?;
        Ok(template.estimated_bytes())
    }

    fn expand_template(template: &Self::Template, _row: RowId, _col: ColId) -> Self {
        template.clone()
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

/// Change to a cell, row or column's explicit [`NumberFormat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatPatch {
    /// Keep whatever explicit format currently exists.
    #[default]
    Keep,
    /// Set an explicit format.
    Set(NumberFormat),
    /// Clear explicit format override (fall back to outer scope).
    Clear,
}

impl FormatPatch {
    const fn is_valid(self) -> bool {
        match self {
            Self::Keep | Self::Clear => true,
            Self::Set(fmt) => fmt.is_valid(),
        }
    }
}

/// Target scope of [`Op::SetFormat`] (`14-m1b.md` 5.1 & 5.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatTarget {
    /// Rectangular/multi-run cell region `rows × cols`.
    Cells {
        /// Target row identity runs.
        rows: Vec<IdRun>,
        /// Target column identity runs.
        cols: Vec<IdRun>,
    },
    /// Whole-row format/style overrides keyed directly on `RowId`s.
    Rows(Vec<IdRun>),
    /// Whole-column format/style overrides keyed directly on `ColId`s.
    Cols(Vec<IdRun>),
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
    /// Previous explicit per-cell format (`None` if inherited from row/column).
    pub explicit_format: Option<NumberFormat>,
}

/// Prior format and style overrides saved by [`Op::SetFormat`] for exact
/// inversion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatBackup {
    /// Prior `(row, col, explicit_format, cell_style_patch)` entries.
    Cells(Vec<(RowId, ColId, Option<NumberFormat>, StylePatch)>),
    /// Prior `(row, row_format, row_style_patch)` entries.
    Rows(Vec<(RowId, Option<NumberFormat>, StylePatch)>),
    /// Prior `(col, col_format, col_style_patch)` entries.
    Cols(Vec<(ColId, NumberFormat, StylePatch)>),
}

/// Atomic workbook mutation (M1b complete operation set, D-022).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "F: FormulaPayload + Serialize",
    deserialize = "F: FormulaPayload + DeserializeOwned"
))]
pub enum Op<F: FormulaPayload = RawFormula> {
    /// Writes up to [`MAX_EDITS_PER_OP`] cells on one worksheet.
    SetCells {
        /// Target worksheet.
        sheet: SheetId,
        /// Pairwise-distinct cell edits.
        edits: Vec<CellEdit<F>>,
    },
    /// Clears values and formulas across `rows × cols` (preserving formats and
    /// styles, matching spreadsheet `Delete` key behaviour).
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
    /// Decrements deletion counts on `ids` (forward undo of [`Self::DeleteRows`],
    /// E17/E18).
    UndeleteRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Target row identity runs.
        ids: Vec<IdRun>,
    },
    /// Decrements deletion counts on `ids` (forward undo of [`Self::DeleteCols`]).
    UndeleteCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Target column identity runs.
        ids: Vec<IdRun>,
    },
    /// Moves `ids` on the row axis to sit after `after`, performing E25 range
    /// anchor handoff on inward/crossing endpoint moves (`14-m1b.md` 5.1).
    MoveRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Row identity runs to move.
        ids: Vec<IdRun>,
        /// Destination predecessor (`None` moves to the top of the sheet).
        after: Option<RowId>,
    },
    /// Moves `ids` on the column axis to sit after `after`, performing E25 range
    /// anchor handoff on inward/crossing endpoint moves.
    MoveCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Column identity runs to move.
        ids: Vec<IdRun>,
        /// Destination predecessor (`None` moves to the left of the sheet).
        after: Option<ColId>,
    },
    /// Sets or resets custom pixel heights on row runs.
    ResizeRows {
        /// Target worksheet.
        sheet: SheetId,
        /// `(row_run, Some(height_px) | None)` pairs.
        sizes: Vec<(IdRun, Option<u16>)>,
    },
    /// Sets or resets custom pixel widths on column runs.
    ResizeCols {
        /// Target worksheet.
        sheet: SheetId,
        /// `(col_run, Some(width_px) | None)` pairs.
        sizes: Vec<(IdRun, Option<u16>)>,
    },
    /// Updates frozen top rows and left columns on `sheet`.
    SetFreeze {
        /// Target worksheet.
        sheet: SheetId,
        /// Frozen top row count (`0..=MAX_FREEZE_COUNT`).
        frozen_rows: u32,
        /// Frozen left column count (`0..=MAX_FREEZE_COUNT`).
        frozen_cols: u32,
    },
    /// Applies a number-format change and/or composable visual [`StylePatch`] to
    /// cells, whole rows, or whole columns.
    SetFormat {
        /// Target worksheet.
        sheet: SheetId,
        /// Target cells, rows, or columns.
        target: FormatTarget,
        /// Number format patch.
        number_format: FormatPatch,
        /// Composable visual style patch (`StylePatch::default()` keeps style).
        style: StylePatch,
    },
    /// Writes a column-major block of cells across `rows × cols` (used by paste
    /// and series fill).
    SetBlock {
        /// Target worksheet.
        sheet: SheetId,
        /// Target row identity runs (`total_rows` entries).
        rows: Vec<IdRun>,
        /// Pairwise-distinct target column identities.
        cols: Vec<ColId>,
        /// `cols.len()` column vectors, each of length `total_rows`.
        columns_data: Vec<Vec<CellInput<F>>>,
    },
    /// Fills `rows × cols` with a compact relative-ID formula template whose
    /// encoded byte size is independent of row/column count (`14-m1b.md` 5.2).
    FillFormula {
        /// Target worksheet.
        sheet: SheetId,
        /// Target row identity runs.
        rows: Vec<IdRun>,
        /// Target column identity runs.
        cols: Vec<IdRun>,
        /// Relative-ID formula template.
        template: F::Template,
    },
    /// Creates a new worksheet with stable `sheet` ID after `after` (`None`
    /// places it first), disambiguating duplicate live names per E28.
    AddSheet {
        /// Fresh worksheet identifier.
        sheet: SheetId,
        /// Requested display name.
        name: String,
        /// Preceding worksheet in tab order (`None` inserts at index `0`).
        after: Option<SheetId>,
    },
    /// Renames `sheet` (disambiguating duplicate live names per E28).
    RenameSheet {
        /// Target worksheet.
        sheet: SheetId,
        /// Requested display name.
        name: String,
    },
    /// Tombstones `sheet` (E20; rejected if `sheet` is the last live worksheet).
    DeleteSheet {
        /// Target worksheet.
        sheet: SheetId,
    },
    /// Decrements `sheet`'s deletion count (restoring cross-sheet references
    /// automatically when `deleted_count` reaches `0`, E20).
    UndeleteSheet {
        /// Target worksheet.
        sheet: SheetId,
    },
    /// Reorders `sheet` in the workbook tab bar to sit after `after`.
    MoveSheet {
        /// Target worksheet.
        sheet: SheetId,
        /// Preceding worksheet (`None` moves to first tab).
        after: Option<SheetId>,
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
    /// Restores cells overwritten by [`Op::FillFormula`], clearing newly filled
    /// cells that were previously completely empty and restoring modified ones.
    RestoreFilledRange {
        /// Target worksheet.
        sheet: SheetId,
        /// Row runs covered by the fill.
        rows: Vec<IdRun>,
        /// Column runs covered by the fill.
        cols: Vec<IdRun>,
        /// Prior non-empty / formatted cells inside `rows × cols`.
        prior_non_empty: Vec<CellSnapshot<F>>,
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
    /// Increments deletion counts decremented by [`Op::UndeleteRows`].
    RedeleteRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Row runs to re-delete.
        ids: Vec<IdRun>,
    },
    /// Increments deletion counts decremented by [`Op::UndeleteCols`].
    RedeleteCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Column runs to re-delete.
        ids: Vec<IdRun>,
    },
    /// Restores row axis runs and any formulas rewritten by E25 anchor handoff.
    RestoreMovedRows {
        /// Target worksheet.
        sheet: SheetId,
        /// Exact row axis runs prior to the move.
        prior_runs: Vec<AxisRun>,
        /// `(cell_key, prior_formula)` entries rewritten during E25 handoff.
        rewritten: Vec<(WorkbookCellKey, F)>,
    },
    /// Restores column axis runs and any formulas rewritten by E25 anchor handoff.
    RestoreMovedCols {
        /// Target worksheet.
        sheet: SheetId,
        /// Exact column axis runs prior to the move.
        prior_runs: Vec<AxisRun>,
        /// `(cell_key, prior_formula)` entries rewritten during E25 handoff.
        rewritten: Vec<(WorkbookCellKey, F)>,
    },
    /// Restores custom row heights modified by [`Op::ResizeRows`].
    RestoreRowHeights {
        /// Target worksheet.
        sheet: SheetId,
        /// `(row_id, prior_height)` entries.
        prior: Vec<(RowId, Option<u16>)>,
    },
    /// Restores custom column widths modified by [`Op::ResizeCols`].
    RestoreColWidths {
        /// Target worksheet.
        sheet: SheetId,
        /// `(col_id, prior_width)` entries.
        prior: Vec<(ColId, Option<u16>)>,
    },
    /// Restores `(frozen_rows, frozen_cols)`.
    RestoreFreeze {
        /// Target worksheet.
        sheet: SheetId,
        /// Previous frozen row count.
        frozen_rows: u32,
        /// Previous frozen column count.
        frozen_cols: u32,
    },
    /// Restores formats and styles modified by [`Op::SetFormat`].
    RestoreFormat {
        /// Target worksheet.
        sheet: SheetId,
        /// Prior format/style records.
        backup: FormatBackup,
    },
    /// Physically removes a worksheet just added by [`Op::AddSheet`].
    RemoveAddedSheet {
        /// Added worksheet identifier.
        sheet: SheetId,
        /// Previous `next_sheet_id` counter.
        prior_next_sheet_id: u32,
    },
    /// Restores previous worksheet name.
    RestoreSheetName {
        /// Target worksheet.
        sheet: SheetId,
        /// Previous name.
        prior_name: String,
    },
    /// Decrements sheet deletion count incremented by [`Op::DeleteSheet`].
    UndeleteSheet {
        /// Target worksheet.
        sheet: SheetId,
    },
    /// Increments sheet deletion count decremented by [`Op::UndeleteSheet`] (and
    /// restores prior name if E28 renamed it on undelete).
    RedeleteSheet {
        /// Target worksheet.
        sheet: SheetId,
        /// Previous name before undelete E28 disambiguation.
        prior_name: String,
    },
    /// Restores exact worksheet tab ordering.
    RestoreSheetOrder {
        /// Ordered `SheetId` vector prior to [`Op::MoveSheet`].
        order: Vec<SheetId>,
    },
}

/// Reason an operation or inverse operation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpError {
    /// Referenced [`SheetId`] does not exist in the workbook.
    SheetNotFound,
    /// [`Op::AddSheet`] supplied a [`SheetId`] that already exists.
    DuplicateSheetId,
    /// Workbook already holds [`MAX_SHEETS`] worksheets.
    TooManySheets,
    /// Worksheet name was empty, too long, or contained forbidden characters.
    InvalidSheetName,
    /// Deleting the last remaining live worksheet is forbidden.
    CannotDeleteLastSheet,
    /// Undeleting a worksheet whose deletion count is already zero, or moving
    /// a sheet after itself.
    InvalidSheetOp,
    /// [`Op::SetCells`], [`Op::SetBlock`], or [`Op::FillFormula`] exceeded its
    /// per-operation cell limit.
    TooManyEdits,
    /// Two edits in the same transaction targeted the same cell/column/row.
    DuplicateCellTarget,
    /// Referenced row or column identity does not exist on the sheet's axis.
    TargetNotFound,
    /// Axis mutation failed validation (anchor missing, duplicate ID, run or
    /// axis length limit exceeded, invalid move).
    Axis(AxisError),
    /// Row height, column width, freeze count, format or style value was out of
    /// allowed bounds, or `SetBlock` dimensions were inconsistent.
    InvalidDimensionOrFormat,
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

/// Multi-sheet workbook owning sheets, formulas, range-endpoint index, ID
/// allocator and memory estimate.
#[derive(Clone, Debug)]
pub struct Workbook<F = RawFormula> {
    sheets: Vec<Sheet>,
    formulas: BTreeMap<WorkbookCellKey, F>,
    formulas_bytes: usize,
    /// Reverse lookup `(target_sheet, row_anchor) -> formula_keys` for `O(affected)`
    /// E25 anchor handoff on `MoveRows`.
    row_range_endpoints: BTreeMap<(SheetId, RowId), BTreeSet<WorkbookCellKey>>,
    /// Reverse lookup `(target_sheet, col_anchor) -> formula_keys` for `O(affected)`
    /// E25 anchor handoff on `MoveCols`.
    col_range_endpoints: BTreeMap<(SheetId, ColId), BTreeSet<WorkbookCellKey>>,
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
            sheets: vec![sheet],
            formulas: BTreeMap::new(),
            formulas_bytes: 0,
            row_range_endpoints: BTreeMap::new(),
            col_range_endpoints: BTreeMap::new(),
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
            sheets: vec![sheet],
            formulas: BTreeMap::new(),
            formulas_bytes: 0,
            row_range_endpoints: BTreeMap::new(),
            col_range_endpoints: BTreeMap::new(),
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
        let mut live_sheets = 0usize;
        for sheet in &sheets {
            if !seen_ids.insert(sheet.id()) {
                return Err(OpError::DuplicateSheetId);
            }
            if !sheet.is_deleted() {
                live_sheets = live_sheets.saturating_add(1);
            }
            if sheet.row_axis().total_len() > MAX_ROW_AXIS_LEN
                || sheet.col_axis().total_len() > MAX_COL_AXIS_LEN
            {
                return Err(OpError::Axis(AxisError::LimitExceeded));
            }
        }
        if live_sheets == 0 {
            return Err(OpError::CannotDeleteLastSheet);
        }

        let mut formulas_bytes = 0usize;
        let mut row_range_endpoints: BTreeMap<(SheetId, RowId), BTreeSet<WorkbookCellKey>> =
            BTreeMap::new();
        let mut col_range_endpoints: BTreeMap<(SheetId, ColId), BTreeSet<WorkbookCellKey>> =
            BTreeMap::new();
        let mut r_eps = Vec::new();
        let mut c_eps = Vec::new();

        for (&key @ (sheet_id, row, col), formula) in &formulas {
            let sheet = sheets
                .iter()
                .find(|s| s.id() == sheet_id)
                .ok_or(OpError::SheetNotFound)?;
            if !sheet.row_axis().contains_id(row.0) || !sheet.col_axis().contains_id(col.0) {
                return Err(OpError::TargetNotFound);
            }
            formula.validate()?;
            formulas_bytes = formulas_bytes.saturating_add(formula_entry_bytes(formula));
            r_eps.clear();
            c_eps.clear();
            formula.collect_range_endpoints(sheet_id, &mut r_eps, &mut c_eps);
            for ep in &r_eps {
                row_range_endpoints.entry(*ep).or_default().insert(key);
            }
            for ep in &c_eps {
                col_range_endpoints.entry(*ep).or_default().insert(key);
            }
        }

        let wb = Self {
            sheets,
            formulas,
            formulas_bytes,
            row_range_endpoints,
            col_range_endpoints,
            allocator,
            next_sheet_id,
            seed_origin,
        };
        if wb.estimated_bytes() > MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }
        Ok(wb)
    }

    /// Worksheets in workbook order (both live and tombstoned).
    #[must_use]
    pub fn sheets(&self) -> &[Sheet] {
        &self.sheets
    }

    /// Iterates visible (non-tombstoned) worksheets in tab order.
    pub fn live_sheets(&self) -> impl Iterator<Item = &Sheet> {
        self.sheets.iter().filter(|s| !s.is_deleted())
    }

    /// Mutable worksheets slice (used by bulk initialisation and viewport style
    /// preparation).
    pub fn sheets_mut(&mut self) -> &mut [Sheet] {
        &mut self.sheets
    }

    /// Looks up a worksheet by [`SheetId`] (live or tombstoned).
    #[must_use]
    pub fn sheet(&self, id: SheetId) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.id() == id)
    }

    /// Looks up a live (non-tombstoned) worksheet by [`SheetId`].
    #[must_use]
    pub fn live_sheet(&self, id: SheetId) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.id() == id && !s.is_deleted())
    }

    /// Looks up a mutable worksheet by [`SheetId`].
    pub fn sheet_mut(&mut self, id: SheetId) -> Option<&mut Sheet> {
        self.sheets.iter_mut().find(|s| s.id() == id)
    }

    /// Allocates the next fresh [`SheetId`] and appends a new standard worksheet.
    pub fn add_sheet(&mut self, name: String) -> Result<SheetId, OpError> {
        let id = SheetId(self.next_sheet_id);
        let after = self.sheets.last().map(Sheet::id);
        let _ = self.apply(Op::AddSheet {
            sheet: id,
            name,
            after,
        })?;
        Ok(id)
    }

    /// Allocates and returns a fresh [`SheetId`] without mutating sheets yet.
    pub fn alloc_sheet_id(&mut self) -> SheetId {
        let id = SheetId(self.next_sheet_id);
        self.next_sheet_id = self.next_sheet_id.saturating_add(1);
        id
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

    /// Clears `seed_origin` when materialising a full standalone snapshot.
    pub fn clear_seed_origin(&mut self) {
        self.seed_origin = None;
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

    /// Compacts unreferenced strings across all sheets (safe at any time without
    /// invalidating undo operations).
    pub fn compact_strings(&mut self) {
        for sheet in &mut self.sheets {
            sheet.compact_strings();
        }
    }

    /// Purges tombstoned worksheets, rows, columns and orphaned strings when the
    /// session undo stack is empty (`14-m1b.md` 5.5): first hands off single-end
    /// range anchors to live neighbours and solidifies dead references to
    /// `#REF!`, then physically strips tombstones and compacts string pools.
    pub fn compact_local_garbage(&mut self) {
        // 1. Solidify formula references against current tombstones before
        // removing tombstoned sheets/rows/cols from the workbook.
        let sheets_snapshot = self.sheets.clone();
        let keys: Vec<WorkbookCellKey> = self.formulas.keys().copied().collect();
        for key in keys {
            if let Some(mut f) = self.formulas.get(&key).cloned()
                && f.finalize_purged_tombstones(key.0, &sheets_snapshot)
            {
                self.put_formula(key, f);
            }
        }

        // 2. Remove tombstoned sheets and formulas residing on them.
        let dead_sheets: BTreeSet<SheetId> = self
            .sheets
            .iter()
            .filter(|s| s.is_deleted())
            .map(Sheet::id)
            .collect();
        if !dead_sheets.is_empty() {
            let dead_keys: Vec<WorkbookCellKey> = self
                .formulas
                .keys()
                .filter(|&&(s, _, _)| dead_sheets.contains(&s))
                .copied()
                .collect();
            for k in dead_keys {
                self.take_formula(k);
            }
            self.sheets.retain(|s| !s.is_deleted());
        }

        // 3. Purge axis tombstones on remaining live sheets and drop formulas
        // that lived on purged rows/cols.
        for idx in 0..self.sheets.len() {
            let Some((sheet_id, purged_rows, purged_cols)) = self.sheets.get_mut(idx).map(|s| {
                let id = s.id();
                let (pr, pc) = s.purge_axis_tombstones();
                (id, pr, pc)
            }) else {
                continue;
            };
            if !purged_rows.is_empty() || !purged_cols.is_empty() {
                let row_set: BTreeSet<RowId> = purged_rows.into_iter().collect();
                let col_set: BTreeSet<ColId> = purged_cols.into_iter().collect();
                let start = (sheet_id, RowId(0), ColId(0));
                let end = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
                let to_drop: Vec<WorkbookCellKey> = self
                    .formulas
                    .range(start..=end)
                    .filter_map(|(&k @ (_, r, c), _)| {
                        if row_set.contains(&r) || col_set.contains(&c) {
                            Some(k)
                        } else {
                            None
                        }
                    })
                    .collect();
                for k in to_drop {
                    self.take_formula(k);
                }
            }
        }

        // 4. Compact string pools.
        self.compact_strings();
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
            Op::UndeleteRows { sheet, ids } => self.apply_undelete_rows(sheet, ids),
            Op::UndeleteCols { sheet, ids } => self.apply_undelete_cols(sheet, ids),
            Op::MoveRows { sheet, ids, after } => self.apply_move_rows(sheet, &ids, after),
            Op::MoveCols { sheet, ids, after } => self.apply_move_cols(sheet, &ids, after),
            Op::ResizeRows { sheet, sizes } => self.apply_resize_rows(sheet, &sizes),
            Op::ResizeCols { sheet, sizes } => self.apply_resize_cols(sheet, &sizes),
            Op::SetFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => self.apply_set_freeze(sheet, frozen_rows, frozen_cols),
            Op::SetFormat {
                sheet,
                target,
                number_format,
                style,
            } => self.apply_set_format(sheet, &target, number_format, style),
            Op::SetBlock {
                sheet,
                rows,
                cols,
                columns_data,
            } => self.apply_set_block(sheet, &rows, &cols, columns_data),
            Op::FillFormula {
                sheet,
                rows,
                cols,
                template,
            } => self.apply_fill_formula(sheet, rows, cols, &template),
            Op::AddSheet { sheet, name, after } => self.apply_add_sheet(sheet, &name, after),
            Op::RenameSheet { sheet, name } => self.apply_rename_sheet(sheet, &name),
            Op::DeleteSheet { sheet } => self.apply_delete_sheet(sheet),
            Op::UndeleteSheet { sheet } => self.apply_undelete_sheet(sheet),
            Op::MoveSheet { sheet, after } => self.apply_move_sheet(sheet, after),
        }
    }

    /// Applies an [`InverseOp`] previously returned by [`Self::apply`].
    pub fn apply_inverse(&mut self, inverse: InverseOp<F>) -> Result<(), OpError> {
        match inverse {
            InverseOp::RestoreCells { sheet, cells } => self.restore_cells(sheet, cells),
            InverseOp::RestoreFilledRange {
                sheet,
                rows,
                cols,
                prior_non_empty,
            } => {
                let _ = self.apply_clear_range(sheet, &rows, &cols)?;
                self.restore_cells(sheet, prior_non_empty)
            }
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
            InverseOp::RedeleteRows { sheet, ids } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.row_axis_mut().delete_runs(&ids)?;
                Ok(())
            }
            InverseOp::RedeleteCols { sheet, ids } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.col_axis_mut().delete_runs(&ids)?;
                Ok(())
            }
            InverseOp::RestoreMovedRows {
                sheet,
                prior_runs,
                rewritten,
            } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.row_axis_mut().restore_runs(prior_runs)?;
                for (key, formula) in rewritten {
                    self.put_formula(key, formula);
                }
                Ok(())
            }
            InverseOp::RestoreMovedCols {
                sheet,
                prior_runs,
                rewritten,
            } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.col_axis_mut().restore_runs(prior_runs)?;
                for (key, formula) in rewritten {
                    self.put_formula(key, formula);
                }
                Ok(())
            }
            InverseOp::RestoreRowHeights { sheet, prior } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                for (row, px) in prior {
                    target.set_row_height_by_id(row, px);
                }
                Ok(())
            }
            InverseOp::RestoreColWidths { sheet, prior } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                for (col, px) in prior {
                    target.set_col_width_by_id(col, px);
                }
                Ok(())
            }
            InverseOp::RestoreFreeze {
                sheet,
                frozen_rows,
                frozen_cols,
            } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.set_freeze(frozen_rows, frozen_cols);
                Ok(())
            }
            InverseOp::RestoreFormat { sheet, backup } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                match backup {
                    FormatBackup::Cells(entries) => {
                        for (r, c, fmt, st) in entries {
                            target.set_cell_format_by_id(r, c, fmt);
                            target.set_cell_style_patch(r, c, st);
                        }
                    }
                    FormatBackup::Rows(entries) => {
                        for (r, fmt, st) in entries {
                            target.set_row_format_by_id(r, fmt);
                            target.set_row_style_patch(r, st);
                        }
                    }
                    FormatBackup::Cols(entries) => {
                        for (c, fmt, st) in entries {
                            target.set_col_format_by_id(c, fmt);
                            target.set_col_style_patch(c, st);
                        }
                    }
                }
                Ok(())
            }
            InverseOp::RemoveAddedSheet {
                sheet,
                prior_next_sheet_id,
            } => {
                let idx = self
                    .sheets
                    .iter()
                    .position(|s| s.id() == sheet)
                    .ok_or(OpError::SheetNotFound)?;
                let start = (sheet, RowId(0), ColId(0));
                let end = (sheet, RowId(u64::MAX), ColId(u64::MAX));
                let keys: Vec<WorkbookCellKey> =
                    self.formulas.range(start..=end).map(|(&k, _)| k).collect();
                for k in keys {
                    self.take_formula(k);
                }
                self.sheets.remove(idx);
                self.next_sheet_id = prior_next_sheet_id;
                Ok(())
            }
            InverseOp::RestoreSheetName { sheet, prior_name } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.set_name(prior_name);
                Ok(())
            }
            InverseOp::UndeleteSheet { sheet } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                if !target.decrement_deleted() {
                    return Err(OpError::InvalidSheetOp);
                }
                Ok(())
            }
            InverseOp::RedeleteSheet { sheet, prior_name } => {
                let target = self.sheet_mut(sheet).ok_or(OpError::SheetNotFound)?;
                target.set_name(prior_name);
                if !target.increment_deleted() {
                    return Err(OpError::InvalidSheetOp);
                }
                Ok(())
            }
            InverseOp::RestoreSheetOrder { order } => {
                if order.len() != self.sheets.len() {
                    return Err(OpError::InvalidSheetOp);
                }
                let mut reordered = Vec::with_capacity(order.len());
                for id in order {
                    let pos = self
                        .sheets
                        .iter()
                        .position(|s| s.id() == id)
                        .ok_or(OpError::SheetNotFound)?;
                    reordered.push(self.sheets.remove(pos));
                }
                self.sheets = reordered;
                Ok(())
            }
        }
    }

    /// Computes a deterministic 64-bit digest of the workbook's visible and
    /// tombstoned sheet/axis structure, sizes, freeze panes, formats, styles,
    /// cell values and bound formulas.
    #[must_use]
    pub fn logical_digest(&self) -> u64 {
        let mut d = Digest64::new();
        d.write_u32(u32::try_from(self.sheets.len()).unwrap_or(u32::MAX));
        for sheet in &self.sheets {
            d.write_u32(sheet.id().0);
            d.write_bytes(sheet.name().as_bytes());
            d.write_u32(sheet.deleted_count());
            d.write_u32(sheet.frozen_rows());
            d.write_u32(sheet.frozen_cols());

            write_axis_digest(sheet.row_axis(), &mut d);
            write_axis_digest(sheet.col_axis(), &mut d);

            // Row heights & column widths.
            d.write_u32(u32::try_from(sheet.row_heights().len()).unwrap_or(u32::MAX));
            for (&r, &px) in sheet.row_heights() {
                d.write_u64(r.0);
                d.write_u32(u32::from(px));
            }
            d.write_u32(u32::try_from(sheet.col_widths().len()).unwrap_or(u32::MAX));
            for (&c, &px) in sheet.col_widths() {
                d.write_u64(c.0);
                d.write_u32(u32::from(px));
            }

            // Column default formats (sorted by ColId so slot order never perturbs digest).
            let mut non_gen_cols: Vec<(u64, NumberFormat)> = Vec::new();
            for (slot_idx, &fmt) in sheet.col_formats().iter().enumerate() {
                if fmt != NumberFormat::General
                    && let Ok(slot_u32) = u32::try_from(slot_idx)
                    && let Some(col_id) = sheet.col_slots().slot_to_id(slot_u32)
                {
                    non_gen_cols.push((col_id, fmt));
                }
            }
            non_gen_cols.sort_unstable_by_key(|&(id, _)| id);
            d.write_u32(u32::try_from(non_gen_cols.len()).unwrap_or(u32::MAX));
            for (col_id, fmt) in non_gen_cols {
                d.write_u64(col_id);
                fmt.write_digest(&mut d);
            }

            // Row formats.
            d.write_u32(u32::try_from(sheet.row_formats().len()).unwrap_or(u32::MAX));
            for (&row, &fmt) in sheet.row_formats() {
                d.write_u64(row.0);
                fmt.write_digest(&mut d);
            }

            // Explicit per-cell formats.
            d.write_u32(u32::try_from(sheet.cell_formats().len()).unwrap_or(u32::MAX));
            for (&(row, col), &fmt) in sheet.cell_formats() {
                d.write_u64(row.0);
                d.write_u64(col.0);
                fmt.write_digest(&mut d);
            }

            // Style patches (column, row, cell).
            d.write_u32(u32::try_from(sheet.col_styles().len()).unwrap_or(u32::MAX));
            for (&col, patch) in sheet.col_styles() {
                d.write_u64(col.0);
                patch.write_digest(&mut d);
            }
            d.write_u32(u32::try_from(sheet.row_styles().len()).unwrap_or(u32::MAX));
            for (&row, patch) in sheet.row_styles() {
                d.write_u64(row.0);
                patch.write_digest(&mut d);
            }
            d.write_u32(u32::try_from(sheet.cell_styles().len()).unwrap_or(u32::MAX));
            for (&(row, col), patch) in sheet.cell_styles() {
                d.write_u64(row.0);
                d.write_u64(col.0);
                patch.write_digest(&mut d);
            }

            // Non-empty cell values sorted by `(ColId, RowId)`.
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

        // Formulas.
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
                if !edit.format.is_valid() {
                    return Err(OpError::InvalidDimensionOrFormat);
                }
                extra_bytes = extra_bytes.saturating_add(validate_cell_input(&edit.input)?);
            }
            extra_bytes
        };

        if self.estimated_bytes().saturating_add(added_bytes) > MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }

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

    fn apply_undelete_rows(
        &mut self,
        sheet_id: SheetId,
        ids: Vec<IdRun>,
    ) -> Result<InverseOp<F>, OpError> {
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        target.row_axis_mut().undelete_runs(&ids)?;
        self.seed_origin = None;
        Ok(InverseOp::RedeleteRows {
            sheet: sheet_id,
            ids,
        })
    }

    fn apply_undelete_cols(
        &mut self,
        sheet_id: SheetId,
        ids: Vec<IdRun>,
    ) -> Result<InverseOp<F>, OpError> {
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        target.col_axis_mut().undelete_runs(&ids)?;
        self.seed_origin = None;
        Ok(InverseOp::RedeleteCols {
            sheet: sheet_id,
            ids,
        })
    }

    fn apply_move_rows(
        &mut self,
        sheet_id: SheetId,
        ids: &[IdRun],
        after: Option<RowId>,
    ) -> Result<InverseOp<F>, OpError> {
        // 1. Validate move preconditions before touching any formula or axis.
        {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.row_axis().validate_existing_runs(ids)?;
            if let Some(anchor) = after {
                if !sheet.row_axis().contains_id(anchor.0) {
                    return Err(OpError::Axis(AxisError::AnchorNotFound));
                }
                if id_in_runs(anchor.0, ids) {
                    return Err(OpError::Axis(AxisError::InvalidMove));
                }
            }
        }

        // 2. Collect formulas whose range endpoints sit inside `ids` on `sheet_id`
        // in O(min(|ids|, |sheet_endpoints|)) without scanning all formulas.
        let candidates = self.collect_row_endpoint_candidates(sheet_id, ids);
        let mut rewritten = Vec::new();
        if !candidates.is_empty() {
            let axis_clone = self
                .sheet(sheet_id)
                .map(|s| s.row_axis().clone())
                .ok_or(OpError::SheetNotFound)?;
            for key in candidates {
                if let Some(prior_f) = self.formulas.get(&key).cloned() {
                    let mut next_f = prior_f.clone();
                    if next_f.handoff_row_anchors(key.0, sheet_id, &axis_clone, ids, after) {
                        self.put_formula(key, next_f);
                        rewritten.push((key, prior_f));
                    }
                }
            }
        }

        // 3. Relocate runs on `row_axis`.
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let prior_runs = target.row_axis_mut().move_runs(ids, after.map(|r| r.0))?;
        self.seed_origin = None;

        Ok(InverseOp::RestoreMovedRows {
            sheet: sheet_id,
            prior_runs,
            rewritten,
        })
    }

    fn apply_move_cols(
        &mut self,
        sheet_id: SheetId,
        ids: &[IdRun],
        after: Option<ColId>,
    ) -> Result<InverseOp<F>, OpError> {
        {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.col_axis().validate_existing_runs(ids)?;
            if let Some(anchor) = after {
                if !sheet.col_axis().contains_id(anchor.0) {
                    return Err(OpError::Axis(AxisError::AnchorNotFound));
                }
                if id_in_runs(anchor.0, ids) {
                    return Err(OpError::Axis(AxisError::InvalidMove));
                }
            }
        }

        let candidates = self.collect_col_endpoint_candidates(sheet_id, ids);
        let mut rewritten = Vec::new();
        if !candidates.is_empty() {
            let axis_clone = self
                .sheet(sheet_id)
                .map(|s| s.col_axis().clone())
                .ok_or(OpError::SheetNotFound)?;
            for key in candidates {
                if let Some(prior_f) = self.formulas.get(&key).cloned() {
                    let mut next_f = prior_f.clone();
                    if next_f.handoff_col_anchors(key.0, sheet_id, &axis_clone, ids, after) {
                        self.put_formula(key, next_f);
                        rewritten.push((key, prior_f));
                    }
                }
            }
        }

        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let prior_runs = target.col_axis_mut().move_runs(ids, after.map(|c| c.0))?;
        self.seed_origin = None;

        Ok(InverseOp::RestoreMovedCols {
            sheet: sheet_id,
            prior_runs,
            rewritten,
        })
    }

    fn collect_row_endpoint_candidates(
        &self,
        sheet_id: SheetId,
        ids: &[IdRun],
    ) -> BTreeSet<WorkbookCellKey> {
        let mut out = BTreeSet::new();
        let total_moved: usize = ids
            .iter()
            .fold(0usize, |acc, r| acc.saturating_add(r.len as usize));
        let sheet_eps_bound = self
            .row_range_endpoints
            .range((sheet_id, RowId(0))..=(sheet_id, RowId(u64::MAX)))
            .take(total_moved.saturating_add(1))
            .count();
        if total_moved < sheet_eps_bound {
            for run in ids {
                for raw_id in run.iter() {
                    if let Some(keys) = self.row_range_endpoints.get(&(sheet_id, RowId(raw_id))) {
                        out.extend(keys.iter().copied());
                    }
                }
            }
        } else {
            for (&(_, r), keys) in self
                .row_range_endpoints
                .range((sheet_id, RowId(0))..=(sheet_id, RowId(u64::MAX)))
            {
                if id_in_runs(r.0, ids) {
                    out.extend(keys.iter().copied());
                }
            }
        }
        out
    }

    fn collect_col_endpoint_candidates(
        &self,
        sheet_id: SheetId,
        ids: &[IdRun],
    ) -> BTreeSet<WorkbookCellKey> {
        let mut out = BTreeSet::new();
        let total_moved: usize = ids
            .iter()
            .fold(0usize, |acc, r| acc.saturating_add(r.len as usize));
        let sheet_eps_bound = self
            .col_range_endpoints
            .range((sheet_id, ColId(0))..=(sheet_id, ColId(u64::MAX)))
            .take(total_moved.saturating_add(1))
            .count();
        if total_moved < sheet_eps_bound {
            for run in ids {
                for raw_id in run.iter() {
                    if let Some(keys) = self.col_range_endpoints.get(&(sheet_id, ColId(raw_id))) {
                        out.extend(keys.iter().copied());
                    }
                }
            }
        } else {
            for (&(_, c), keys) in self
                .col_range_endpoints
                .range((sheet_id, ColId(0))..=(sheet_id, ColId(u64::MAX)))
            {
                if id_in_runs(c.0, ids) {
                    out.extend(keys.iter().copied());
                }
            }
        }
        out
    }

    /// Returns `(cell_key, formula)` for every formula whose range endpoint
    /// currently references any row in `ids` on `sheet_id` (`O(affected)`).
    #[must_use]
    pub fn formulas_touching_row_endpoints(
        &self,
        sheet_id: SheetId,
        ids: &[IdRun],
    ) -> Vec<(WorkbookCellKey, F)> {
        self.collect_row_endpoint_candidates(sheet_id, ids)
            .into_iter()
            .filter_map(|k| self.formulas.get(&k).cloned().map(|f| (k, f)))
            .collect()
    }

    /// Returns `(cell_key, formula)` for every formula whose range endpoint
    /// currently references any column in `ids` on `sheet_id` (`O(affected)`).
    #[must_use]
    pub fn formulas_touching_col_endpoints(
        &self,
        sheet_id: SheetId,
        ids: &[IdRun],
    ) -> Vec<(WorkbookCellKey, F)> {
        self.collect_col_endpoint_candidates(sheet_id, ids)
            .into_iter()
            .filter_map(|k| self.formulas.get(&k).cloned().map(|f| (k, f)))
            .collect()
    }

    fn apply_resize_rows(
        &mut self,
        sheet_id: SheetId,
        sizes: &[(IdRun, Option<u16>)],
    ) -> Result<InverseOp<F>, OpError> {
        if sizes.is_empty() {
            return Err(OpError::Axis(AxisError::InvalidRun));
        }
        let runs: Vec<IdRun> = sizes.iter().map(|&(r, _)| r).collect();
        let total_ids = count_ids_checked(&runs, MAX_BULK_CELLS_PER_OP)?;
        {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.row_axis().validate_existing_runs(&runs)?;
            for &(_, px) in sizes {
                if let Some(h) = px
                    && !(MIN_ROW_HEIGHT..=MAX_ROW_HEIGHT).contains(&h)
                {
                    return Err(OpError::InvalidDimensionOrFormat);
                }
            }
        }
        if self
            .estimated_bytes()
            .saturating_add(total_ids.saturating_mul(24))
            > MAX_WORKBOOK_BYTES
        {
            return Err(OpError::MemoryLimitExceeded);
        }

        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let mut prior = Vec::with_capacity(total_ids);
        for &(run, px) in sizes {
            for raw_id in run.iter() {
                let row = RowId(raw_id);
                prior.push((row, target.row_height_by_id(row)));
                target.set_row_height_by_id(row, px);
            }
        }
        self.seed_origin = None;
        Ok(InverseOp::RestoreRowHeights {
            sheet: sheet_id,
            prior,
        })
    }

    fn apply_resize_cols(
        &mut self,
        sheet_id: SheetId,
        sizes: &[(IdRun, Option<u16>)],
    ) -> Result<InverseOp<F>, OpError> {
        if sizes.is_empty() {
            return Err(OpError::Axis(AxisError::InvalidRun));
        }
        let runs: Vec<IdRun> = sizes.iter().map(|&(r, _)| r).collect();
        let total_ids = count_ids_checked(&runs, MAX_BULK_CELLS_PER_OP)?;
        {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.col_axis().validate_existing_runs(&runs)?;
            for &(_, px) in sizes {
                if let Some(w) = px
                    && !(MIN_COL_WIDTH..=MAX_COL_WIDTH).contains(&w)
                {
                    return Err(OpError::InvalidDimensionOrFormat);
                }
            }
        }
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let mut prior = Vec::with_capacity(total_ids);
        for &(run, px) in sizes {
            for raw_id in run.iter() {
                let col = ColId(raw_id);
                prior.push((col, target.col_width_by_id(col)));
                target.set_col_width_by_id(col, px);
            }
        }
        self.seed_origin = None;
        Ok(InverseOp::RestoreColWidths {
            sheet: sheet_id,
            prior,
        })
    }

    fn apply_set_freeze(
        &mut self,
        sheet_id: SheetId,
        frozen_rows: u32,
        frozen_cols: u32,
    ) -> Result<InverseOp<F>, OpError> {
        if frozen_rows > MAX_FREEZE_COUNT || frozen_cols > MAX_FREEZE_COUNT {
            return Err(OpError::InvalidDimensionOrFormat);
        }
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let prior_r = target.frozen_rows();
        let prior_c = target.frozen_cols();
        target.set_freeze(frozen_rows, frozen_cols);
        self.seed_origin = None;
        Ok(InverseOp::RestoreFreeze {
            sheet: sheet_id,
            frozen_rows: prior_r,
            frozen_cols: prior_c,
        })
    }

    fn apply_set_format(
        &mut self,
        sheet_id: SheetId,
        target_spec: &FormatTarget,
        number_format: FormatPatch,
        style: StylePatch,
    ) -> Result<InverseOp<F>, OpError> {
        if !number_format.is_valid() || !style.is_valid() {
            return Err(OpError::InvalidDimensionOrFormat);
        }
        let extra_entries: usize = {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            match target_spec {
                FormatTarget::Cells { rows, cols } => {
                    sheet.row_axis().validate_existing_runs(rows)?;
                    sheet.col_axis().validate_existing_runs(cols)?;
                    let r_n = count_ids_checked(rows, MAX_BULK_CELLS_PER_OP)?;
                    let c_n = count_ids_checked(cols, MAX_BULK_CELLS_PER_OP)?;
                    let prod = r_n.checked_mul(c_n).ok_or(OpError::TooManyEdits)?;
                    if prod > MAX_BULK_CELLS_PER_OP {
                        return Err(OpError::TooManyEdits);
                    }
                    prod
                }
                FormatTarget::Rows(rows) => {
                    sheet.row_axis().validate_existing_runs(rows)?;
                    count_ids_checked(rows, MAX_BULK_CELLS_PER_OP)?
                }
                FormatTarget::Cols(cols) => {
                    sheet.col_axis().validate_existing_runs(cols)?;
                    count_ids_checked(cols, MAX_BULK_CELLS_PER_OP)?
                }
            }
        };
        if self
            .estimated_bytes()
            .saturating_add(extra_entries.saturating_mul(48))
            > MAX_WORKBOOK_BYTES
        {
            return Err(OpError::MemoryLimitExceeded);
        }

        let sheet = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let backup = match target_spec {
            FormatTarget::Cells { rows, cols } => {
                let mut prior = Vec::with_capacity(extra_entries);
                for r_run in rows {
                    for raw_r in r_run.iter() {
                        let r = RowId(raw_r);
                        for c_run in cols {
                            for raw_c in c_run.iter() {
                                let c = ColId(raw_c);
                                let old_fmt = sheet.cell_format_by_id(r, c);
                                let old_st = sheet.cell_style_patch(r, c);
                                prior.push((r, c, old_fmt, old_st));
                                apply_format_patch(sheet, r, c, number_format);
                                if !style.is_empty() {
                                    sheet.set_cell_style_patch(r, c, style.merged_over(old_st));
                                }
                            }
                        }
                    }
                }
                FormatBackup::Cells(prior)
            }
            FormatTarget::Rows(rows) => {
                let mut prior = Vec::with_capacity(extra_entries);
                for r_run in rows {
                    for raw_r in r_run.iter() {
                        let r = RowId(raw_r);
                        let old_fmt = sheet.row_format_by_id(r);
                        let old_st = sheet.row_style_patch(r);
                        prior.push((r, old_fmt, old_st));
                        match number_format {
                            FormatPatch::Keep => {}
                            FormatPatch::Set(fmt) => sheet.set_row_format_by_id(r, Some(fmt)),
                            FormatPatch::Clear => sheet.set_row_format_by_id(r, None),
                        }
                        if !style.is_empty() {
                            sheet.set_row_style_patch(r, style.merged_over(old_st));
                        }
                    }
                }
                FormatBackup::Rows(prior)
            }
            FormatTarget::Cols(cols) => {
                let mut prior = Vec::with_capacity(extra_entries);
                for c_run in cols {
                    for raw_c in c_run.iter() {
                        let c = ColId(raw_c);
                        let old_fmt = sheet.col_format_by_id(c);
                        let old_st = sheet.col_style_patch(c);
                        prior.push((c, old_fmt, old_st));
                        match number_format {
                            FormatPatch::Keep => {}
                            FormatPatch::Set(fmt) => {
                                sheet.set_col_format_by_id(c, fmt);
                            }
                            FormatPatch::Clear => {
                                sheet.set_col_format_by_id(c, NumberFormat::General);
                            }
                        }
                        if !style.is_empty() {
                            sheet.set_col_style_patch(c, style.merged_over(old_st));
                        }
                    }
                }
                FormatBackup::Cols(prior)
            }
        };

        self.seed_origin = None;
        Ok(InverseOp::RestoreFormat {
            sheet: sheet_id,
            backup,
        })
    }

    fn apply_set_block(
        &mut self,
        sheet_id: SheetId,
        rows: &[IdRun],
        cols: &[ColId],
        columns_data: Vec<Vec<CellInput<F>>>,
    ) -> Result<InverseOp<F>, OpError> {
        if cols.is_empty() || columns_data.len() != cols.len() {
            return Err(OpError::InvalidDimensionOrFormat);
        }
        let total_rows = count_ids_checked(rows, MAX_BULK_CELLS_PER_OP)?;
        let total_cells = total_rows
            .checked_mul(cols.len())
            .ok_or(OpError::TooManyEdits)?;
        if total_cells > MAX_BULK_CELLS_PER_OP {
            return Err(OpError::TooManyEdits);
        }

        let added_bytes: usize = {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.row_axis().validate_existing_runs(rows)?;
            let mut seen_cols = BTreeSet::new();
            for &col in cols {
                if !seen_cols.insert(col) {
                    return Err(OpError::DuplicateCellTarget);
                }
                if !sheet.col_axis().contains_id(col.0) {
                    return Err(OpError::TargetNotFound);
                }
            }
            let mut extra = 0usize;
            for col_vec in &columns_data {
                if col_vec.len() != total_rows {
                    return Err(OpError::InvalidDimensionOrFormat);
                }
                for cell in col_vec {
                    extra = extra.saturating_add(validate_cell_input(cell)?);
                }
            }
            extra
        };
        if self.estimated_bytes().saturating_add(added_bytes) > MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }

        let row_ids: Vec<RowId> = rows.iter().flat_map(|r| r.iter().map(RowId)).collect();
        let mut prior = Vec::with_capacity(total_cells);
        for &col in cols {
            for &row in &row_ids {
                if let Some(snap) = self.cell_snapshot(sheet_id, row, col) {
                    prior.push(snap);
                }
            }
        }

        self.seed_origin = None;
        for (&col, col_vec) in cols.iter().zip(columns_data) {
            for (&row, input) in row_ids.iter().zip(col_vec) {
                self.write_validated_cell(sheet_id, row, col, input, FormatPatch::Keep)?;
            }
        }

        Ok(InverseOp::RestoreCells {
            sheet: sheet_id,
            cells: prior,
        })
    }

    fn apply_fill_formula(
        &mut self,
        sheet_id: SheetId,
        rows: Vec<IdRun>,
        cols: Vec<IdRun>,
        template: &F::Template,
    ) -> Result<InverseOp<F>, OpError> {
        let per_cell_bytes = F::validate_template(template)?;
        let r_n = count_ids_checked(&rows, MAX_BULK_CELLS_PER_OP)?;
        let c_n = count_ids_checked(&cols, MAX_BULK_CELLS_PER_OP)?;
        let total_cells = r_n.checked_mul(c_n).ok_or(OpError::TooManyEdits)?;
        if total_cells > MAX_BULK_CELLS_PER_OP {
            return Err(OpError::TooManyEdits);
        }
        {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            sheet.row_axis().validate_existing_runs(&rows)?;
            sheet.col_axis().validate_existing_runs(&cols)?;
        }
        let added_bytes = total_cells.saturating_mul(
            per_cell_bytes.saturating_add(size_of::<WorkbookCellKey>().saturating_mul(2)),
        );
        if self.estimated_bytes().saturating_add(added_bytes) > MAX_WORKBOOK_BYTES {
            return Err(OpError::MemoryLimitExceeded);
        }

        // Capture only previously non-empty/formatted cells in `rows × cols` so
        // filling 1,000,000 empty cells does not allocate 1,000,000 empty snapshots!
        let prior_non_empty: Vec<CellSnapshot<F>> = {
            let sheet = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            let mut affected = BTreeSet::new();
            for col_run in &cols {
                for raw_col_id in col_run.iter() {
                    if let Some(col_slot) = sheet.col_slots().get(raw_col_id)
                        && let Some(column) = sheet.raw_columns().get(col_slot as usize)
                    {
                        column.for_each_non_empty(|row_slot, _| {
                            if let Some(raw_row_id) = sheet.row_slots().slot_to_id(row_slot)
                                && id_in_runs(raw_row_id, &rows)
                            {
                                affected.insert((RowId(raw_row_id), ColId(raw_col_id)));
                            }
                        });
                    }
                }
            }
            let start_key = (sheet_id, RowId(0), ColId(0));
            let end_key = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
            for (&(_, r, c), _) in self.formulas.range(start_key..=end_key) {
                if id_in_runs(r.0, &rows) && id_in_runs(c.0, &cols) {
                    affected.insert((r, c));
                }
            }
            for &(r, c) in sheet.cell_formats().keys() {
                if id_in_runs(r.0, &rows) && id_in_runs(c.0, &cols) {
                    affected.insert((r, c));
                }
            }
            affected
                .into_iter()
                .filter_map(|(r, c)| self.cell_snapshot(sheet_id, r, c))
                .collect()
        };

        self.seed_origin = None;
        if let Some(sheet) = self.sheet_mut(sheet_id) {
            for snap in &prior_non_empty {
                sheet.set_value_by_id(snap.row, snap.col, Value::Empty);
            }
        }
        for c_run in &cols {
            for raw_c in c_run.iter() {
                let col = ColId(raw_c);
                for r_run in &rows {
                    for raw_r in r_run.iter() {
                        let row = RowId(raw_r);
                        let expanded = F::expand_template(template, row, col);
                        self.put_formula((sheet_id, row, col), expanded);
                    }
                }
            }
        }

        Ok(InverseOp::RestoreFilledRange {
            sheet: sheet_id,
            rows,
            cols,
            prior_non_empty,
        })
    }

    fn apply_add_sheet(
        &mut self,
        sheet_id: SheetId,
        requested_name: &str,
        after: Option<SheetId>,
    ) -> Result<InverseOp<F>, OpError> {
        if self.sheets.len() >= MAX_SHEETS {
            return Err(OpError::TooManySheets);
        }
        if self.sheet(sheet_id).is_some() {
            return Err(OpError::DuplicateSheetId);
        }
        if !is_valid_sheet_name(requested_name) {
            return Err(OpError::InvalidSheetName);
        }
        let insert_idx = match after {
            None => 0,
            Some(prev_id) => self
                .sheets
                .iter()
                .position(|s| s.id() == prev_id)
                .ok_or(OpError::SheetNotFound)?
                .saturating_add(1),
        };
        let unique_name = self.disambiguate_sheet_name(requested_name, None);
        let prior_next = self.next_sheet_id;
        if sheet_id.0 >= self.next_sheet_id {
            self.next_sheet_id = sheet_id.0.saturating_add(1);
        }
        self.sheets
            .insert(insert_idx, Sheet::standard(sheet_id, unique_name));
        self.seed_origin = None;
        Ok(InverseOp::RemoveAddedSheet {
            sheet: sheet_id,
            prior_next_sheet_id: prior_next,
        })
    }

    fn apply_rename_sheet(
        &mut self,
        sheet_id: SheetId,
        requested_name: &str,
    ) -> Result<InverseOp<F>, OpError> {
        if !is_valid_sheet_name(requested_name) {
            return Err(OpError::InvalidSheetName);
        }
        let _ = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
        let unique_name = self.disambiguate_sheet_name(requested_name, Some(sheet_id));
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let prior_name = String::from(target.name());
        target.set_name(unique_name);
        self.seed_origin = None;
        Ok(InverseOp::RestoreSheetName {
            sheet: sheet_id,
            prior_name,
        })
    }

    fn apply_delete_sheet(&mut self, sheet_id: SheetId) -> Result<InverseOp<F>, OpError> {
        let is_currently_live = self
            .sheet(sheet_id)
            .ok_or(OpError::SheetNotFound)?
            .deleted_count()
            == 0;
        if is_currently_live && self.live_sheets().count() <= 1 {
            return Err(OpError::CannotDeleteLastSheet);
        }
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        if !target.increment_deleted() {
            return Err(OpError::Axis(AxisError::LimitExceeded));
        }
        self.seed_origin = None;
        Ok(InverseOp::UndeleteSheet { sheet: sheet_id })
    }

    fn apply_undelete_sheet(&mut self, sheet_id: SheetId) -> Result<InverseOp<F>, OpError> {
        let (prior_count, prior_name) = {
            let s = self.sheet(sheet_id).ok_or(OpError::SheetNotFound)?;
            if s.deleted_count() == 0 {
                return Err(OpError::InvalidSheetOp);
            }
            (s.deleted_count(), String::from(s.name()))
        };
        let new_name = if prior_count == 1 {
            Some(self.disambiguate_sheet_name(&prior_name, Some(sheet_id)))
        } else {
            None
        };
        let target = self.sheet_mut(sheet_id).ok_or(OpError::SheetNotFound)?;
        let _ = target.decrement_deleted();
        if let Some(disambiguated) = new_name {
            target.set_name(disambiguated);
        }
        self.seed_origin = None;
        Ok(InverseOp::RedeleteSheet {
            sheet: sheet_id,
            prior_name,
        })
    }

    fn apply_move_sheet(
        &mut self,
        sheet_id: SheetId,
        after: Option<SheetId>,
    ) -> Result<InverseOp<F>, OpError> {
        if after == Some(sheet_id) {
            return Err(OpError::InvalidSheetOp);
        }
        let cur_idx = self
            .sheets
            .iter()
            .position(|s| s.id() == sheet_id)
            .ok_or(OpError::SheetNotFound)?;
        if let Some(prev_id) = after
            && !self.sheets.iter().any(|s| s.id() == prev_id)
        {
            return Err(OpError::SheetNotFound);
        }
        let prior_order: Vec<SheetId> = self.sheets.iter().map(Sheet::id).collect();
        let moved = self.sheets.remove(cur_idx);
        let dest_idx = match after {
            None => 0,
            Some(prev_id) => self
                .sheets
                .iter()
                .position(|s| s.id() == prev_id)
                .map_or(0, |p| p.saturating_add(1)),
        };
        self.sheets.insert(dest_idx, moved);
        self.seed_origin = None;
        Ok(InverseOp::RestoreSheetOrder { order: prior_order })
    }

    /// Implements E28 deterministic sheet name disambiguation (`Data` ->
    /// `Data (2)` -> `Data (3)`), truncating the stem if needed so the result
    /// always stays within [`MAX_SHEET_NAME_UTF16`] (`31`) UTF-16 code units.
    fn disambiguate_sheet_name(&self, desired: &str, ignore: Option<SheetId>) -> String {
        let collides = |candidate: &str| -> bool {
            self.sheets.iter().any(|s| {
                !s.is_deleted()
                    && Some(s.id()) != ignore
                    && s.name().eq_ignore_ascii_case(candidate)
            })
        };
        let trimmed = desired.trim();
        if !collides(trimmed) {
            return String::from(trimmed);
        }
        let mut n: u32 = 2;
        loop {
            let suffix = format!(" ({n})");
            let suffix_units = suffix.encode_utf16().count();
            let max_stem = MAX_SHEET_NAME_UTF16.saturating_sub(suffix_units);
            let mut candidate = truncate_utf16(trimmed, max_stem);
            candidate.push_str(&suffix);
            if !collides(&candidate) {
                return candidate;
            }
            n = n.saturating_add(1);
        }
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
        self.unregister_formula_endpoints(key);
        let mut r_eps = Vec::new();
        let mut c_eps = Vec::new();
        formula.collect_range_endpoints(key.0, &mut r_eps, &mut c_eps);
        for ep in r_eps {
            self.row_range_endpoints.entry(ep).or_default().insert(key);
        }
        for ep in c_eps {
            self.col_range_endpoints.entry(ep).or_default().insert(key);
        }

        let added = formula_entry_bytes(&formula);
        if let Some(old) = self.formulas.insert(key, formula) {
            self.formulas_bytes = self
                .formulas_bytes
                .saturating_sub(formula_entry_bytes(&old));
        }
        self.formulas_bytes = self.formulas_bytes.saturating_add(added);
    }

    fn take_formula(&mut self, key: WorkbookCellKey) {
        self.unregister_formula_endpoints(key);
        if let Some(old) = self.formulas.remove(&key) {
            self.formulas_bytes = self
                .formulas_bytes
                .saturating_sub(formula_entry_bytes(&old));
        }
    }

    fn unregister_formula_endpoints(&mut self, key: WorkbookCellKey) {
        let Some(existing) = self.formulas.get(&key) else {
            return;
        };
        let mut r_eps = Vec::new();
        let mut c_eps = Vec::new();
        existing.collect_range_endpoints(key.0, &mut r_eps, &mut c_eps);
        for ep in r_eps {
            let mut empty = false;
            if let Some(set) = self.row_range_endpoints.get_mut(&ep) {
                set.remove(&key);
                empty = set.is_empty();
            }
            if empty {
                self.row_range_endpoints.remove(&ep);
            }
        }
        for ep in c_eps {
            let mut empty = false;
            if let Some(set) = self.col_range_endpoints.get_mut(&ep) {
                set.remove(&key);
                empty = set.is_empty();
            }
            if empty {
                self.col_range_endpoints.remove(&ep);
            }
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

fn truncate_utf16(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let next = units.saturating_add(ch.len_utf16());
        if next > max_units {
            break;
        }
        out.push(ch);
        units = next;
    }
    out
}

fn count_ids_checked(runs: &[IdRun], cap: usize) -> Result<usize, OpError> {
    if runs.is_empty() {
        return Err(OpError::Axis(AxisError::InvalidRun));
    }
    let mut total = 0usize;
    for r in runs {
        if !r.is_valid() {
            return Err(OpError::Axis(AxisError::InvalidRun));
        }
        total = total
            .checked_add(r.len as usize)
            .ok_or(OpError::TooManyEdits)?;
        if total > cap {
            return Err(OpError::TooManyEdits);
        }
    }
    Ok(total)
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

/// Checks whether `id` falls within any run of `runs`.
#[must_use]
pub fn id_in_runs(id: u64, runs: &[IdRun]) -> bool {
    runs.iter().any(|run| run.contains(id))
}

fn write_axis_digest(axis: &Axis, d: &mut Digest64) {
    use crate::id::{id_counter, id_stream};

    d.write_u32(axis.live_len());
    d.write_u32(axis.total_len());

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
        CellEdit, CellInput, FormatPatch, FormatTarget, FormulaMode, Op, OpError, RawFormula,
        ScalarValue, Workbook,
    };
    use crate::format::NumberFormat;
    use crate::id::{ColId, IdRun, RowId, SheetId, pack_id};
    use crate::style::{BorderWeight, ColorPatch, ColorValue, StylePatch};
    use crate::value::{CalcError, Value};

    #[test]
    fn every_m1b_op_and_its_inverse_round_trip_to_identical_digest() {
        let mut wb: Workbook = Workbook::new();
        let sheet = SheetId(0);
        let initial_digest = wb.logical_digest();

        // 1. SetCells
        let inv1 = wb
            .apply(Op::SetCells {
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
                ],
            })
            .unwrap();
        let d1 = wb.logical_digest();

        // 2. InsertRows + MoveRows
        let inserted = wb.allocator_mut().alloc_run(2).unwrap();
        let inv2 = wb
            .apply(Op::InsertRows {
                sheet,
                after: None,
                ids: inserted,
            })
            .unwrap();
        let d2 = wb.logical_digest();
        let inv3 = wb
            .apply(Op::MoveRows {
                sheet,
                ids: vec![IdRun::new(1, 1).unwrap()],
                after: None,
            })
            .unwrap();
        let d3 = wb.logical_digest();

        // 3. DeleteRows + UndeleteRows
        let inv4 = wb
            .apply(Op::DeleteRows {
                sheet,
                ids: vec![IdRun::new(0, 1).unwrap()],
            })
            .unwrap();
        let d4 = wb.logical_digest();
        let inv5 = wb
            .apply(Op::UndeleteRows {
                sheet,
                ids: vec![IdRun::new(0, 1).unwrap()],
            })
            .unwrap();
        let d5 = wb.logical_digest();

        // 4. ResizeRows + SetFreeze + SetFormat + SetBlock + FillFormula
        let inv6 = wb
            .apply(Op::ResizeRows {
                sheet,
                sizes: vec![(IdRun::new(0, 3).unwrap(), Some(36))],
            })
            .unwrap();
        let d6 = wb.logical_digest();
        let inv7 = wb
            .apply(Op::SetFreeze {
                sheet,
                frozen_rows: 1,
                frozen_cols: 1,
            })
            .unwrap();
        let d7 = wb.logical_digest();
        let inv8 = wb
            .apply(Op::SetFormat {
                sheet,
                target: FormatTarget::Cols(vec![IdRun::new(0, 2).unwrap()]),
                number_format: FormatPatch::Set(NumberFormat::Fixed {
                    decimals: 2,
                    thousands: true,
                }),
                style: StylePatch {
                    bold: Some(true),
                    fill_color: ColorPatch::Set(ColorValue::Palette(1)),
                    border_bottom: Some(BorderWeight::Thin),
                    ..StylePatch::default()
                },
            })
            .unwrap();
        let d8 = wb.logical_digest();
        let inv9 = wb
            .apply(Op::SetBlock {
                sheet,
                rows: vec![IdRun::new(2, 2).unwrap()],
                cols: vec![ColId(0), ColId(1)],
                columns_data: vec![
                    vec![
                        CellInput::Scalar(ScalarValue::Number(10.0)),
                        CellInput::Scalar(ScalarValue::Number(20.0)),
                    ],
                    vec![CellInput::Scalar(ScalarValue::Bool(true)), CellInput::Empty],
                ],
            })
            .unwrap();
        let d9 = wb.logical_digest();
        let inv10 = wb
            .apply(Op::FillFormula {
                sheet,
                rows: vec![IdRun::new(5, 10).unwrap()],
                cols: vec![IdRun::new(2, 1).unwrap()],
                template: RawFormula {
                    source: String::from("=A6*2"),
                    mode: FormulaMode::Dynamic,
                },
            })
            .unwrap();
        let d10 = wb.logical_digest();

        // 5. Sheet ops: AddSheet (with E28 collision!), RenameSheet, MoveSheet, DeleteSheet, UndeleteSheet
        let s1 = SheetId(1);
        let inv11 = wb
            .apply(Op::AddSheet {
                sheet: s1,
                name: String::from("Sheet1"), // collides -> "Sheet1 (2)"
                after: Some(sheet),
            })
            .unwrap();
        assert_eq!(wb.sheet(s1).unwrap().name(), "Sheet1 (2)");
        let d11 = wb.logical_digest();
        let inv12 = wb
            .apply(Op::MoveSheet {
                sheet: s1,
                after: None,
            })
            .unwrap();
        let d12 = wb.logical_digest();
        let inv13 = wb.apply(Op::DeleteSheet { sheet: s1 }).unwrap();
        let d13 = wb.logical_digest();
        // Deleting the last live sheet must fail!
        assert_eq!(
            wb.apply(Op::DeleteSheet { sheet }),
            Err(OpError::CannotDeleteLastSheet)
        );
        let inv14 = wb.apply(Op::UndeleteSheet { sheet: s1 }).unwrap();

        wb.apply_inverse(inv14).unwrap();
        assert_eq!(wb.logical_digest(), d13);
        wb.apply_inverse(inv13).unwrap();
        assert_eq!(wb.logical_digest(), d12);
        wb.apply_inverse(inv12).unwrap();
        assert_eq!(wb.logical_digest(), d11);
        wb.apply_inverse(inv11).unwrap();
        assert_eq!(wb.logical_digest(), d10);
        wb.apply_inverse(inv10).unwrap();
        assert_eq!(wb.logical_digest(), d9);
        wb.apply_inverse(inv9).unwrap();
        assert_eq!(wb.logical_digest(), d8);
        wb.apply_inverse(inv8).unwrap();
        assert_eq!(wb.logical_digest(), d7);
        wb.apply_inverse(inv7).unwrap();
        assert_eq!(wb.logical_digest(), d6);
        wb.apply_inverse(inv6).unwrap();
        assert_eq!(wb.logical_digest(), d5);
        wb.apply_inverse(inv5).unwrap();
        assert_eq!(wb.logical_digest(), d4);
        wb.apply_inverse(inv4).unwrap();
        assert_eq!(wb.logical_digest(), d3);
        wb.apply_inverse(inv3).unwrap();
        assert_eq!(wb.logical_digest(), d2);
        wb.apply_inverse(inv2).unwrap();
        assert_eq!(wb.logical_digest(), d1);
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

        // Moving row 0 to after row 0 -> rejected!
        assert!(
            wb.apply(Op::MoveRows {
                sheet,
                ids: vec![IdRun::new(0, 2).unwrap()],
                after: Some(RowId(1)),
            })
            .is_err()
        );
        assert_eq!(wb.logical_digest(), baseline);

        // Unknown row ID -> rejected.
        let unknown_row = RowId(pack_id(9, 99, 1).unwrap());
        assert_eq!(
            wb.apply(Op::SetCells {
                sheet,
                edits: vec![CellEdit {
                    row: unknown_row,
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Error(CalcError::Na)),
                    format: FormatPatch::Keep,
                }],
            }),
            Err(OpError::TargetNotFound)
        );
        assert_eq!(wb.sheet(sheet).unwrap().value(0, 0), Value::Number(42.0));
        assert_eq!(wb.logical_digest(), baseline);
    }
}
