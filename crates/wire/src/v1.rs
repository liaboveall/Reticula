//! Frozen v1 wire schema (`D-020` & `14-m1b.md` 5.5).
//!
//! IMPORTANT: Every type in this module is intentionally decoupled from
//! `reticula_core` and `reticula_formula` in-memory types and locked by a
//! SHA-256 checksum in `tests/compat/hashes.txt`. Future refactors of internal
//! engine structures adapt `convert_v1.rs` without modifying this file.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// Contiguous identity run `[first_id, first_id + len)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireIdRunV1 {
    /// Starting 64-bit packed identity.
    pub first_id: u64,
    /// Run length (`>= 1`).
    pub len: u32,
}

/// Persisted axis run with optional per-entry deletion tombstone counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireAxisRunV1 {
    /// Starting 64-bit packed identity.
    pub first_id: u64,
    /// Run length (`>= 1`).
    pub len: u32,
    /// Per-entry deletion counters (`None` when all entries in run are live).
    pub tombstones: Option<Vec<u32>>,
}

/// Canonical spreadsheet error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireCalcErrorV1 {
    /// `#DIV/0!`
    Div0,
    /// `#VALUE!`
    Value,
    /// `#REF!`
    Ref,
    /// `#NAME?`
    Name,
    /// `#NUM!`
    Num,
    /// `#N/A`
    Na,
    /// `#NULL!`
    Null,
    /// `#SPILL!`
    Spill,
    /// `#CALC!`
    Calc,
    /// `#CYCLE!`
    Cycle,
}

/// Self-contained scalar value on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireScalarValueV1 {
    /// Finite IEEE 754 number.
    Number(f64),
    /// UTF-8 text scalar.
    Text(String),
    /// Logical boolean.
    Bool(bool),
    /// Error code.
    Error(WireCalcErrorV1),
}

/// Currency glyph selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireCurrencySymbolV1 {
    /// `$`
    Usd,
    /// `¥`
    Cny,
    /// `€`
    Eur,
}

/// Frozen M1b subset of number and date/time display formats (`14-m1b.md` 5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireNumberFormatV1 {
    /// General (`General`).
    General,
    /// Two fixed decimal places (`0.00`).
    Fixed2,
    /// Whole percent (`0%`).
    Percent,
    /// ISO date (`yyyy-mm-dd`).
    Date,
    /// Rounded integer (`0`).
    Integer,
    /// Fixed decimals with optional thousands separator.
    Fixed {
        /// Decimal places (`0..=6`).
        decimals: u8,
        /// `,` thousands grouping flag.
        thousands: bool,
    },
    /// Currency prefix with thousands grouping.
    Currency {
        /// Currency symbol.
        symbol: WireCurrencySymbolV1,
        /// Decimal places (`0..=4`).
        decimals: u8,
    },
    /// Percent with fractional places (`1..=4`).
    PercentDecimals {
        /// Decimal places (`1..=4`).
        decimals: u8,
    },
    /// Scientific notation (`0.00E+00`).
    Scientific {
        /// Mantissa fractional places (`1..=6`).
        decimals: u8,
    },
    /// Slash date (`yyyy/mm/dd`).
    DateSlash,
    /// US date (`mm/dd/yyyy`).
    DateUs,
    /// Year-month (`yyyy-mm`).
    DateYearMonth,
    /// ISO date + 24h time.
    DateTime {
        /// Include `:ss`.
        seconds: bool,
    },
    /// 24h time only.
    Time {
        /// Include `:ss`.
        seconds: bool,
    },
    /// Literal text mode (`@`).
    PlainText,
}

/// Explicit number-format mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFormatPatchV1 {
    /// Leave existing format unchanged.
    Keep,
    /// Set an explicit format.
    Set(WireNumberFormatV1),
    /// Clear explicit override.
    Clear,
}

/// Scope targeted by [`WireOpV1::SetFormat`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFormatTargetV1 {
    /// Cell rectangle / multi-run intersection.
    Cells {
        /// Row identity runs.
        rows: Vec<WireIdRunV1>,
        /// Column identity runs.
        cols: Vec<WireIdRunV1>,
    },
    /// Whole rows.
    Rows(Vec<WireIdRunV1>),
    /// Whole columns.
    Cols(Vec<WireIdRunV1>),
}

/// Horizontal cell alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireHorizontalAlignV1 {
    /// Type-driven alignment.
    Auto,
    /// Left-aligned.
    Left,
    /// Centered.
    Center,
    /// Right-aligned.
    Right,
}

/// Foreground or fill color value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireColorValueV1 {
    /// Semantic light/dark-adaptive palette index (`0..8`).
    Palette(u8),
    /// 24-bit sRGB (`0x00RRGGBB`).
    Rgb(u32),
}

/// Tri-state color attribute patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireColorPatchV1 {
    /// Keep existing attribute.
    Keep,
    /// Set color.
    Set(WireColorValueV1),
    /// Clear color to default.
    Clear,
}

/// Cell edge border stroke weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireBorderWeightV1 {
    /// No border.
    None,
    /// 1px solid border.
    Thin,
    /// 2px solid border.
    Medium,
}

/// Four-edge border strokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireBorderSetV1 {
    /// Top edge.
    pub top: WireBorderWeightV1,
    /// Right edge.
    pub right: WireBorderWeightV1,
    /// Bottom edge.
    pub bottom: WireBorderWeightV1,
    /// Left edge.
    pub left: WireBorderWeightV1,
}

/// Resolved cell visual style entry in a sheet's style table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCellStyleV1 {
    /// Bold font weight.
    pub bold: bool,
    /// Italic font style.
    pub italic: bool,
    /// Strikethrough decoration.
    pub strikethrough: bool,
    /// Horizontal alignment.
    pub align: WireHorizontalAlignV1,
    /// Optional text color.
    pub text_color: Option<WireColorValueV1>,
    /// Optional background fill color.
    pub fill_color: Option<WireColorValueV1>,
    /// Four-edge borders.
    pub borders: WireBorderSetV1,
}

/// Composable visual style override patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStylePatchV1 {
    /// Bold override.
    pub bold: Option<bool>,
    /// Italic override.
    pub italic: Option<bool>,
    /// Strikethrough override.
    pub strikethrough: Option<bool>,
    /// Alignment override.
    pub align: Option<WireHorizontalAlignV1>,
    /// Text color patch.
    pub text_color: WireColorPatchV1,
    /// Fill color patch.
    pub fill_color: WireColorPatchV1,
    /// Top border override.
    pub border_top: Option<WireBorderWeightV1>,
    /// Right border override.
    pub border_right: Option<WireBorderWeightV1>,
    /// Bottom border override.
    pub border_bottom: Option<WireBorderWeightV1>,
    /// Left border override.
    pub border_left: Option<WireBorderWeightV1>,
}

/// Formula array evaluation mode (D-028).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFormulaModeV1 {
    /// Dynamic array evaluation.
    Dynamic,
    /// Legacy implicit-intersection evaluation.
    Legacy,
}

/// Formula unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireUnaryOpV1 {
    /// `+x`
    Plus,
    /// `-x`
    Neg,
    /// `@x`
    ImplicitIntersection,
    /// `x%`
    Percent,
}

/// Formula binary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireBinaryOpV1 {
    /// `a ^ b`
    Pow,
    /// `a * b`
    Mul,
    /// `a / b`
    Div,
    /// `a + b`
    Add,
    /// `a - b`
    Sub,
    /// `a & b`
    Concat,
    /// `a = b`
    Eq,
    /// `a <> b`
    Ne,
    /// `a < b`
    Lt,
    /// `a > b`
    Gt,
    /// `a <= b`
    Le,
    /// `a >= b`
    Ge,
}

/// Formula AST scalar literal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireLiteralV1 {
    /// Finite IEEE 754 number.
    Number(f64),
    /// String literal.
    Text(String),
    /// Boolean literal.
    Bool(bool),
    /// Error literal.
    Error(WireCalcErrorV1),
}

/// Identity-bound cell or range reference on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireBoundRefV1 {
    /// Single cell reference.
    Cell {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name at bind time, if any.
        missing_sheet: Option<String>,
        /// Row identity.
        row: u64,
        /// Column identity.
        col: u64,
        /// `$` column lock.
        abs_col: bool,
        /// `$` row lock.
        abs_row: bool,
    },
    /// Rectangular cell area.
    Area {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Top row anchor.
        first_row: u64,
        /// Left column anchor.
        first_col: u64,
        /// Bottom row anchor.
        last_row: u64,
        /// Right column anchor.
        last_col: u64,
        /// `$` on `first_col`.
        abs_first_col: bool,
        /// `$` on `first_row`.
        abs_first_row: bool,
        /// `$` on `last_col`.
        abs_last_col: bool,
        /// `$` on `last_row`.
        abs_last_row: bool,
    },
    /// Full-column range.
    ColRange {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Left column anchor.
        first_col: u64,
        /// Right column anchor.
        last_col: u64,
        /// `$` on `first_col`.
        abs_first: bool,
        /// `$` on `last_col`.
        abs_last: bool,
    },
    /// Full-row range.
    RowRange {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Top row anchor.
        first_row: u64,
        /// Bottom row anchor.
        last_row: u64,
        /// `$` on `first_row`.
        abs_first: bool,
        /// `$` on `last_row`.
        abs_last: bool,
    },
    /// Invalid / deleted reference (`#REF!`).
    Invalid {
        /// Optional sheet prefix for display.
        sheet_name: Option<String>,
    },
}

/// Identity-bound formula expression node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireBoundExprV1 {
    /// Scalar literal.
    Literal(WireLiteralV1),
    /// Constant 2D array literal.
    ArrayLiteral {
        /// Row count.
        rows: u32,
        /// Column count.
        cols: u32,
        /// Row-major elements.
        elements: Vec<WireLiteralV1>,
    },
    /// Bound cell/range reference.
    Reference(WireBoundRefV1),
    /// Unresolved identifier.
    Name(String),
    /// Explicit parentheses `(expr)`.
    Paren(Box<Self>),
    /// Unary operation.
    Unary {
        /// Operator.
        op: WireUnaryOpV1,
        /// Inner expression.
        expr: Box<Self>,
    },
    /// Binary operation.
    Binary {
        /// Operator.
        op: WireBinaryOpV1,
        /// Left expression.
        left: Box<Self>,
        /// Right expression.
        right: Box<Self>,
    },
    /// Function call.
    Call {
        /// Uppercase function name.
        name: String,
        /// Arguments.
        args: Vec<Self>,
    },
}

/// Persisted bound formula.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireBoundFormulaV1 {
    /// Array evaluation mode.
    pub mode: WireFormulaModeV1,
    /// Root expression.
    pub root: WireBoundExprV1,
}

/// Axis target inside [`WireTemplateFormulaV1`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireAxisTargetV1 {
    /// Fixed identity.
    Fixed(u64),
    /// Signed identity offset `target_id - self_id`.
    IdDelta(i64),
}

/// Template reference inside [`WireTemplateFormulaV1`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireTemplateRefV1 {
    /// Single cell.
    Cell {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Row rule.
        row: WireAxisTargetV1,
        /// Column rule.
        col: WireAxisTargetV1,
        /// `$` column lock.
        abs_col: bool,
        /// `$` row lock.
        abs_row: bool,
    },
    /// Rectangular area.
    Area {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Top row rule.
        first_row: WireAxisTargetV1,
        /// Left column rule.
        first_col: WireAxisTargetV1,
        /// Bottom row rule.
        last_row: WireAxisTargetV1,
        /// Right column rule.
        last_col: WireAxisTargetV1,
        /// `$` on `first_col`.
        abs_first_col: bool,
        /// `$` on `first_row`.
        abs_first_row: bool,
        /// `$` on `last_col`.
        abs_last_col: bool,
        /// `$` on `last_row`.
        abs_last_row: bool,
    },
    /// Full-column range.
    ColRange {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Left column rule.
        first_col: WireAxisTargetV1,
        /// Right column rule.
        last_col: WireAxisTargetV1,
        /// `$` on `first_col`.
        abs_first: bool,
        /// `$` on `last_col`.
        abs_last: bool,
    },
    /// Full-row range.
    RowRange {
        /// Target worksheet (`None` = host sheet).
        sheet: Option<u32>,
        /// Unresolved sheet name, if any.
        missing_sheet: Option<String>,
        /// Top row rule.
        first_row: WireAxisTargetV1,
        /// Bottom row rule.
        last_row: WireAxisTargetV1,
        /// `$` on `first_row`.
        abs_first: bool,
        /// `$` on `last_row`.
        abs_last: bool,
    },
    /// Invalid reference (`#REF!`).
    Invalid {
        /// Optional sheet prefix.
        sheet_name: Option<String>,
    },
}

/// Template expression tree inside [`WireTemplateFormulaV1`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireTemplateExprV1 {
    /// Scalar literal.
    Literal(WireLiteralV1),
    /// Constant 2D array literal.
    ArrayLiteral {
        /// Row count.
        rows: u32,
        /// Column count.
        cols: u32,
        /// Row-major elements.
        elements: Vec<WireLiteralV1>,
    },
    /// Template reference.
    Reference(WireTemplateRefV1),
    /// Unresolved identifier.
    Name(String),
    /// Explicit parentheses.
    Paren(Box<Self>),
    /// Unary operation.
    Unary {
        /// Operator.
        op: WireUnaryOpV1,
        /// Inner expression.
        expr: Box<Self>,
    },
    /// Binary operation.
    Binary {
        /// Operator.
        op: WireBinaryOpV1,
        /// Left expression.
        left: Box<Self>,
        /// Right expression.
        right: Box<Self>,
    },
    /// Function call.
    Call {
        /// Uppercase function name.
        name: String,
        /// Arguments.
        args: Vec<Self>,
    },
}

/// Relative-ID formula template carried by [`WireOpV1::FillFormula`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTemplateFormulaV1 {
    /// Array evaluation mode.
    pub mode: WireFormulaModeV1,
    /// Root template expression.
    pub root: WireTemplateExprV1,
}

/// Cell content payload on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireCellInputV1 {
    /// Empty cell.
    Empty,
    /// Scalar constant.
    Scalar(WireScalarValueV1),
    /// Bound formula plus optional cached evaluation result.
    Formula {
        /// Bound formula.
        formula: WireBoundFormulaV1,
        /// Cached scalar value, if known.
        cached: Option<WireScalarValueV1>,
    },
}

/// Single cell edit inside [`WireOpV1::SetCells`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireCellEditV1 {
    /// Row identity.
    pub row: u64,
    /// Column identity.
    pub col: u64,
    /// Cell input.
    pub input: WireCellInputV1,
    /// Explicit number-format change.
    pub format: WireFormatPatchV1,
}

/// Frozen v1 atomic workbook mutation (`14-m1b.md` 5.1 & 5.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireOpV1 {
    /// Discrete cell edits.
    SetCells {
        /// Worksheet ID.
        sheet: u32,
        /// Cell edits.
        edits: Vec<WireCellEditV1>,
    },
    /// Clears values and formulas across `rows × cols`.
    ClearRange {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs.
        rows: Vec<WireIdRunV1>,
        /// Column runs.
        cols: Vec<WireIdRunV1>,
    },
    /// Inserts fresh row run after `after`.
    InsertRows {
        /// Worksheet ID.
        sheet: u32,
        /// Predecessor row ID (`None` = top).
        after: Option<u64>,
        /// Inserted row run.
        ids: WireIdRunV1,
    },
    /// Inserts fresh column run after `after`.
    InsertCols {
        /// Worksheet ID.
        sheet: u32,
        /// Predecessor column ID (`None` = left).
        after: Option<u64>,
        /// Inserted column run.
        ids: WireIdRunV1,
    },
    /// Tombstones row runs.
    DeleteRows {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Tombstones column runs.
    DeleteCols {
        /// Worksheet ID.
        sheet: u32,
        /// Column runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Restores tombstoned row runs.
    UndeleteRows {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Restores tombstoned column runs.
    UndeleteCols {
        /// Worksheet ID.
        sheet: u32,
        /// Column runs.
        ids: Vec<WireIdRunV1>,
    },
    /// Moves row runs after `after` (with E25 range-anchor handoff).
    MoveRows {
        /// Worksheet ID.
        sheet: u32,
        /// Row runs to move.
        ids: Vec<WireIdRunV1>,
        /// Destination predecessor (`None` = top).
        after: Option<u64>,
    },
    /// Moves column runs after `after` (with E25 range-anchor handoff).
    MoveCols {
        /// Worksheet ID.
        sheet: u32,
        /// Column runs to move.
        ids: Vec<WireIdRunV1>,
        /// Destination predecessor (`None` = left).
        after: Option<u64>,
    },
    /// Sets or resets custom row pixel heights.
    ResizeRows {
        /// Worksheet ID.
        sheet: u32,
        /// `(row_run, height_px)` pairs.
        sizes: Vec<(WireIdRunV1, Option<u16>)>,
    },
    /// Sets or resets custom column pixel widths.
    ResizeCols {
        /// Worksheet ID.
        sheet: u32,
        /// `(col_run, width_px)` pairs.
        sizes: Vec<(WireIdRunV1, Option<u16>)>,
    },
    /// Sets frozen rows and columns.
    SetFreeze {
        /// Worksheet ID.
        sheet: u32,
        /// Frozen top rows.
        frozen_rows: u32,
        /// Frozen left columns.
        frozen_cols: u32,
    },
    /// Applies number format and/or composable style patch.
    SetFormat {
        /// Worksheet ID.
        sheet: u32,
        /// Scope target.
        target: WireFormatTargetV1,
        /// Number format patch.
        number_format: WireFormatPatchV1,
        /// Style patch.
        style: WireStylePatchV1,
    },
    /// Writes a column-major rectangular block of cells.
    SetBlock {
        /// Worksheet ID.
        sheet: u32,
        /// Target row runs.
        rows: Vec<WireIdRunV1>,
        /// Target column IDs.
        cols: Vec<u64>,
        /// Column-major cell vectors.
        columns_data: Vec<Vec<WireCellInputV1>>,
    },
    /// Fills `rows × cols` using a constant-size relative formula template.
    FillFormula {
        /// Worksheet ID.
        sheet: u32,
        /// Target row runs.
        rows: Vec<WireIdRunV1>,
        /// Target column runs.
        cols: Vec<WireIdRunV1>,
        /// Formula template.
        template: WireTemplateFormulaV1,
    },
    /// Adds a worksheet.
    AddSheet {
        /// Fresh worksheet ID.
        sheet: u32,
        /// Requested tab name.
        name: String,
        /// Predecessor sheet in tab order (`None` = first).
        after: Option<u32>,
    },
    /// Renames a worksheet.
    RenameSheet {
        /// Worksheet ID.
        sheet: u32,
        /// New tab name.
        name: String,
    },
    /// Tombstones a worksheet.
    DeleteSheet {
        /// Worksheet ID.
        sheet: u32,
    },
    /// Restores a tombstoned worksheet.
    UndeleteSheet {
        /// Worksheet ID.
        sheet: u32,
    },
    /// Reorders a worksheet in the tab bar.
    MoveSheet {
        /// Worksheet ID.
        sheet: u32,
        /// Predecessor sheet (`None` = first).
        after: Option<u32>,
    },
}

/// Named batch of atomic operations forming one user transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTxnV1 {
    /// Human-readable undo/history label (e.g. `"Edit V1"`).
    pub label: String,
    /// Atomic operations executed in order.
    pub ops: Vec<WireOpV1>,
}

/// Unacknowledged client transaction queued for server sequencing (M2a, D-020).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WirePendingTxnV1 {
    /// Originating replica ID.
    pub replica: u32,
    /// Monotonic client-local sequence number.
    pub client_seq: u64,
    /// Latest server sequence observed when authored.
    pub base_server_seq: u64,
    /// Wall-clock authoring timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Authored transaction payload.
    pub txn: WireTxnV1,
}

/// Metadata frame written immediately after the 16-byte `RTCLWAL1` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireWalMetaV1 {
    /// Sequence number of the base snapshot this WAL builds upon.
    pub base_snapshot_seq: u64,
}

/// Committed transaction frame inside an `RTCLWAL1` log (`14-m1b.md` 5.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireWalRecordV1 {
    /// Strictly contiguous 1-based sequence (`base_snapshot_seq + 1`, ...).
    pub seq: u64,
    /// Wall-clock timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Human-readable transaction label (shown in Safe Mode recovery list).
    pub label: String,
    /// Atomic operations in this transaction.
    pub ops: Vec<WireOpV1>,
}

/// Deterministic sample generator parameters (`SeedOrigin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSeedOriginV1 {
    /// Generator PRNG seed.
    pub seed: u64,
    /// Row count generated.
    pub rows: u32,
    /// Sample schema generator version.
    pub generator_version: u32,
}

/// Per-replica deduplication and allocator high-water mark stored in
/// `RTCLSNAP` v1 (`D-026`, empty for local-only documents, populated in M2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireReplicaWatermarkV1 {
    /// Replica identifier.
    pub replica: u32,
    /// Highest `client_seq` sequenced from this replica.
    pub last_client_seq: u64,
    /// Highest identity counter observed from this replica.
    pub max_id_counter: u32,
}

/// `Strings` section payload in `RTCLSNAP` v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStringsSectionV1 {
    /// Per-sheet interned string pools in sheet order.
    pub sheets: Vec<WireSheetStringsV1>,
}

/// One worksheet's string pool on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSheetStringsV1 {
    /// Concatenated UTF-8 bytes.
    pub bytes: Vec<u8>,
    /// Cumulative byte end offsets.
    pub ends: Vec<u32>,
}

/// `AxesAndMetrics` section payload in `RTCLSNAP` v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireAxesMetricsSectionV1 {
    /// Identity allocator epoch.
    pub allocator_epoch: u8,
    /// Identity allocator replica ID.
    pub allocator_replica: u32,
    /// Identity allocator next counter.
    pub allocator_next_counter: u32,
    /// Next fresh `SheetId` counter.
    pub next_sheet_id: u32,
    /// Per-sheet axes, slot maps, dimensions, freeze counts and number formats.
    pub sheets: Vec<WireSheetAxesMetricsV1>,
}

/// Per-sheet structural & metric metadata in `RTCLSNAP` v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSheetAxesMetricsV1 {
    /// Stable worksheet ID.
    pub id: u32,
    /// Tab display name.
    pub name: String,
    /// Tombstone deletion counter (`0` = live).
    pub deleted_count: u32,
    /// Frozen top rows.
    pub frozen_rows: u32,
    /// Frozen left columns.
    pub frozen_cols: u32,
    /// Row axis runs.
    pub row_runs: Vec<WireAxisRunV1>,
    /// Column axis runs.
    pub col_runs: Vec<WireAxisRunV1>,
    /// Row `SlotMap` identity base bound.
    pub row_slot_base: u32,
    /// Row `SlotMap` extra allocated IDs.
    pub row_slot_extra: Vec<u64>,
    /// Column `SlotMap` identity base bound.
    pub col_slot_base: u32,
    /// Column `SlotMap` extra allocated IDs.
    pub col_slot_extra: Vec<u64>,
    /// Explicit custom row heights `(row_id, px)`.
    pub row_heights: Vec<(u64, u16)>,
    /// Explicit custom column widths `(col_id, px)`.
    pub col_widths: Vec<(u64, u16)>,
    /// Slot-indexed column number formats.
    pub col_formats: Vec<WireNumberFormatV1>,
    /// Whole-row number format overrides `(row_id, format)`.
    pub row_formats: Vec<(u64, WireNumberFormatV1)>,
    /// Per-cell number format overrides `(row_id, col_id, format)`.
    pub cell_formats: Vec<(u64, u64, WireNumberFormatV1)>,
}

/// `Styles` section payload in `RTCLSNAP` v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStylesSectionV1 {
    /// Per-sheet style tables and patches in sheet order.
    pub sheets: Vec<WireSheetStylesV1>,
}

/// Per-sheet resolved style table and composable patches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSheetStylesV1 {
    /// Deduplicated style table (`entries[0]` is default).
    pub table: Vec<WireCellStyleV1>,
    /// Whole-column style patches `(col_id, patch)`.
    pub col_styles: Vec<(u64, WireStylePatchV1)>,
    /// Whole-row style patches `(row_id, patch)`.
    pub row_styles: Vec<(u64, WireStylePatchV1)>,
    /// Per-cell style patches `(row_id, col_id, patch)`.
    pub cell_styles: Vec<(u64, u64, WireStylePatchV1)>,
}

/// One persisted cell formula entry inside a chunk of `Formulas` in `RTCLSNAP` v1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireFormulaEntryV1 {
    /// Worksheet ID.
    pub sheet: u32,
    /// Row ID.
    pub row: u64,
    /// Column ID.
    pub col: u64,
    /// Bound formula AST.
    pub formula: WireBoundFormulaV1,
}

/// `ReplicaWatermarks` section payload in `RTCLSNAP` v1 (`D-026`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireWatermarksSectionV1 {
    /// Sorted per-replica high-water marks (empty for local-only M1b docs).
    pub entries: Vec<WireReplicaWatermarkV1>,
}
