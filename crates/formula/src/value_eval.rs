//! Runtime evaluation values, D-027 15-digit operators and D-028 dynamic-array
//! broadcasting / implicit intersection.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::decimal15::{cmp_excel_f64, snap_cancelled_zero};
use reticula_core::format::{NumberFormat, write_number};
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::input::{InferredInput, infer_input};
use reticula_core::num;
use reticula_core::ops::{FormulaMode, ScalarValue, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::value::{CalcError, Value};

use crate::ast::{BinaryOp, Literal, UnaryOp};
use crate::binder::{BoundFormula, BoundRef, live_span_positions};
use crate::spill::SpillRegistry;

/// Maximum cells materialised in an in-memory dynamic array (`1,048,576`).
pub const MAX_ARRAY_CELLS: usize = 1_048_576;

/// Scalar value during formula evaluation (preserves `Empty` distinct from `0`
/// and `""` until coerced by an operator or function).
#[derive(Clone, Debug, PartialEq)]
pub enum EvalScalar {
    /// Blank cell.
    Empty,
    /// Finite IEEE 754 number (`-0.0` canonicalised to `+0.0`).
    Number(f64),
    /// Text string.
    Text(String),
    /// Logical value.
    Bool(bool),
    /// Calculation error.
    Error(CalcError),
}

impl EvalScalar {
    /// Converts a literal AST leaf into an [`EvalScalar`].
    #[must_use]
    pub fn from_literal(lit: &Literal) -> Self {
        match lit {
            Literal::Number(n) => Self::number(*n),
            Literal::Text(s) => Self::Text(s.clone()),
            Literal::Bool(b) => Self::Bool(*b),
            Literal::Error(e) => Self::Error(*e),
        }
    }

    /// Canonical numeric constructor (`-0.0 -> 0.0`, non-finite -> `#NUM!`).
    #[must_use]
    pub const fn number(n: f64) -> Self {
        if !n.is_finite() {
            Self::Error(CalcError::Num)
        } else if n == 0.0 {
            Self::Number(0.0)
        } else {
            Self::Number(n)
        }
    }

    /// Reads a stored [`Value`] from `sheet` into an [`EvalScalar`].
    #[must_use]
    pub fn from_sheet_value(sheet: &Sheet, val: Value) -> Self {
        match val {
            Value::Empty => Self::Empty,
            Value::Number(n) => Self::number(n),
            Value::Text(id) => Self::Text(String::from(sheet.strings().get(id))),
            Value::Bool(b) => Self::Bool(b),
            Value::Error(e) => Self::Error(e),
            Value::Array(_) => Self::Error(CalcError::Spill),
        }
    }

    /// Coerces a scalar operand to a finite number for arithmetic (`Empty -> 0`,
    /// `Bool -> 0/1`, numeric `Text -> f64`, non-numeric `Text -> #VALUE!`).
    pub fn to_number(&self) -> Result<f64, CalcError> {
        match self {
            Self::Error(e) => Err(*e),
            Self::Empty => Ok(0.0),
            Self::Number(n) => Ok(*n),
            Self::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Self::Text(s) => {
                if s.trim_matches(' ').is_empty() {
                    return Err(CalcError::Value);
                }
                match infer_input(s) {
                    Ok(InferredInput::Scalar {
                        value: ScalarValue::Number(n),
                        ..
                    }) => Ok(n),
                    _ => Err(CalcError::Value),
                }
            }
        }
    }

    /// Coerces a scalar operand to text (for `&` and text functions), using
    /// Excel `General` formatting for numbers.
    pub fn to_text(&self) -> Result<String, CalcError> {
        match self {
            Self::Error(e) => Err(*e),
            Self::Empty => Ok(String::new()),
            Self::Text(s) => Ok(s.clone()),
            Self::Bool(true) => Ok(String::from("TRUE")),
            Self::Bool(false) => Ok(String::from("FALSE")),
            Self::Number(n) => {
                let mut out = String::new();
                write_number(*n, NumberFormat::General, &mut out);
                Ok(out)
            }
        }
    }

    /// Coerces a scalar operand to `bool` (`Empty -> false`, `Number(0) -> false`,
    /// non-zero `Number -> true`, `"TRUE"/"FALSE"` text case-insensitive, else `#VALUE!`).
    pub fn to_bool(&self) -> Result<bool, CalcError> {
        match self {
            Self::Error(e) => Err(*e),
            Self::Empty => Ok(false),
            Self::Bool(b) => Ok(*b),
            Self::Number(n) => Ok(*n != 0.0),
            Self::Text(s) => {
                if s.eq_ignore_ascii_case("TRUE") {
                    Ok(true)
                } else if s.eq_ignore_ascii_case("FALSE") {
                    Ok(false)
                } else {
                    Err(CalcError::Value)
                }
            }
        }
    }
}

/// Rectangular 2D dynamic array (`rows >= 1`, `cols >= 1`, row-major).
#[derive(Clone, Debug, PartialEq)]
pub struct Array2D {
    /// Number of rows.
    pub rows: u32,
    /// Number of columns.
    pub cols: u32,
    /// Row-major elements (`len == rows * cols`).
    pub values: Vec<EvalScalar>,
}

impl Array2D {
    /// Constructs a validated 2D array, returning `#CALC!` if empty or larger
    /// than [`MAX_ARRAY_CELLS`].
    pub fn new(rows: u32, cols: u32, values: Vec<EvalScalar>) -> Result<Self, CalcError> {
        let total = (rows as usize)
            .checked_mul(cols as usize)
            .ok_or(CalcError::Calc)?;
        if total == 0 || total > MAX_ARRAY_CELLS || total != values.len() {
            return Err(CalcError::Calc);
        }
        Ok(Self { rows, cols, values })
    }

    /// Broadcast lookup at `(r, c)` following Excel dynamic-array rules: a
    /// dimension of length `1` repeats across any target size, while out-of-range
    /// indices on dimensions `> 1` yield `#N/A`.
    #[must_use]
    pub fn get_broadcast(&self, r: u32, c: u32) -> EvalScalar {
        let rr = if self.rows == 1 {
            0
        } else if r < self.rows {
            r
        } else {
            return EvalScalar::Error(CalcError::Na);
        };
        let cc = if self.cols == 1 {
            0
        } else if c < self.cols {
            c
        } else {
            return EvalScalar::Error(CalcError::Na);
        };
        let idx = (rr as usize)
            .saturating_mul(self.cols as usize)
            .saturating_add(cc as usize);
        self.values
            .get(idx)
            .cloned()
            .unwrap_or(EvalScalar::Error(CalcError::Na))
    }
}

/// Live reference passed to range-aware functions (`SUM`, `ISBLANK`, `ROW`, `@`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedRef {
    /// Single live cell `(sheet, row, col)`.
    Cell {
        /// Worksheet identifier.
        sheet: SheetId,
        /// Row identity.
        row: RowId,
        /// Column identity.
        col: ColId,
    },
    /// Live rectangular area span.
    Area {
        /// Worksheet identifier.
        sheet: SheetId,
        /// Top anchor row identity.
        first_row: RowId,
        /// Bottom anchor row identity.
        last_row: RowId,
        /// Left anchor column identity.
        first_col: ColId,
        /// Right anchor column identity.
        last_col: ColId,
    },
    /// Full-column span.
    FullCols {
        /// Worksheet identifier.
        sheet: SheetId,
        /// Left anchor column identity.
        first_col: ColId,
        /// Right anchor column identity.
        last_col: ColId,
    },
    /// Full-row span.
    FullRows {
        /// Worksheet identifier.
        sheet: SheetId,
        /// Top anchor row identity.
        first_row: RowId,
        /// Bottom anchor row identity.
        last_row: RowId,
    },
    /// Deleted or unresolvable reference (`#REF!`).
    Invalid(CalcError),
}

/// Runtime value flowing through the formula evaluator.
#[derive(Clone, Debug, PartialEq)]
pub enum EvalValue {
    /// Single scalar value.
    Scalar(EvalScalar),
    /// 2D dynamic array.
    Array(Array2D),
    /// Lazy cell/range reference.
    Reference(ResolvedRef),
}

/// Coordinates of the cell whose formula is currently being evaluated (used by
/// `ROW()`, `COLUMN()`, and `@` / legacy implicit intersection).
#[derive(Clone, Copy, Debug)]
pub struct CallerContext {
    /// Worksheet hosting the formula.
    pub sheet: SheetId,
    /// Formula cell's row identity.
    pub row: RowId,
    /// Formula cell's column identity.
    pub col: ColId,
    /// Formula's array mode (`Dynamic` vs `Legacy`).
    pub mode: FormulaMode,
}

/// Resolves a [`BoundRef`] against `workbook` from `host_sheet`.
#[must_use]
pub fn resolve_bound_ref(
    bound: &BoundRef,
    workbook: &Workbook<BoundFormula>,
    host_sheet: SheetId,
) -> ResolvedRef {
    match bound {
        BoundRef::Invalid { .. } => ResolvedRef::Invalid(CalcError::Ref),
        BoundRef::Cell {
            sheet, row, col, ..
        } => {
            let target_id = sheet.unwrap_or(host_sheet);
            let Some(s) = workbook.live_sheet(target_id) else {
                return ResolvedRef::Invalid(CalcError::Ref);
            };
            if s.row_axis().id_to_pos(row.0).is_none() || s.col_axis().id_to_pos(col.0).is_none() {
                return ResolvedRef::Invalid(CalcError::Ref);
            }
            ResolvedRef::Cell {
                sheet: target_id,
                row: *row,
                col: *col,
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
            let target_id = sheet.unwrap_or(host_sheet);
            let Some(s) = workbook.live_sheet(target_id) else {
                return ResolvedRef::Invalid(CalcError::Ref);
            };
            if live_span_positions(s.row_axis(), first_row.0, last_row.0).is_none()
                || live_span_positions(s.col_axis(), first_col.0, last_col.0).is_none()
            {
                return ResolvedRef::Invalid(CalcError::Ref);
            }
            ResolvedRef::Area {
                sheet: target_id,
                first_row: *first_row,
                last_row: *last_row,
                first_col: *first_col,
                last_col: *last_col,
            }
        }
        BoundRef::ColRange {
            sheet,
            first_col,
            last_col,
            ..
        } => {
            let target_id = sheet.unwrap_or(host_sheet);
            let Some(s) = workbook.live_sheet(target_id) else {
                return ResolvedRef::Invalid(CalcError::Ref);
            };
            if live_span_positions(s.col_axis(), first_col.0, last_col.0).is_none()
                || s.row_axis().live_len() == 0
            {
                return ResolvedRef::Invalid(CalcError::Ref);
            }
            ResolvedRef::FullCols {
                sheet: target_id,
                first_col: *first_col,
                last_col: *last_col,
            }
        }
        BoundRef::RowRange {
            sheet,
            first_row,
            last_row,
            ..
        } => {
            let target_id = sheet.unwrap_or(host_sheet);
            let Some(s) = workbook.live_sheet(target_id) else {
                return ResolvedRef::Invalid(CalcError::Ref);
            };
            if live_span_positions(s.row_axis(), first_row.0, last_row.0).is_none()
                || s.col_axis().live_len() == 0
            {
                return ResolvedRef::Invalid(CalcError::Ref);
            }
            ResolvedRef::FullRows {
                sheet: target_id,
                first_row: *first_row,
                last_row: *last_row,
            }
        }
    }
}

/// Applies Excel implicit intersection (`@` or `FormulaMode::Legacy`, D-028) to
/// `value` relative to `caller`.
#[must_use]
pub fn implicit_intersect(
    value: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalScalar {
    match value {
        EvalValue::Scalar(s) => s,
        EvalValue::Array(arr) => {
            if arr.rows == 1 && arr.cols == 1 {
                arr.values
                    .first()
                    .cloned()
                    .unwrap_or(EvalScalar::Error(CalcError::Value))
            } else {
                EvalScalar::Error(CalcError::Value)
            }
        }
        EvalValue::Reference(r) => intersect_reference(&r, workbook, spills, caller),
    }
}

fn intersect_reference(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalScalar {
    match r {
        ResolvedRef::Invalid(e) => EvalScalar::Error(*e),
        ResolvedRef::Cell { sheet, row, col } => {
            read_cell_effective(workbook, spills, *sheet, *row, *col)
        }
        ResolvedRef::Area {
            sheet,
            first_row,
            last_row,
            first_col,
            last_col,
        } => {
            let Some(target) = workbook.sheet(*sheet) else {
                return EvalScalar::Error(CalcError::Ref);
            };
            let Some(host) = workbook.sheet(caller.sheet) else {
                return EvalScalar::Error(CalcError::Ref);
            };
            let Some((r1, r2)) = live_span_positions(target.row_axis(), first_row.0, last_row.0)
            else {
                return EvalScalar::Error(CalcError::Ref);
            };
            let Some((c1, c2)) = live_span_positions(target.col_axis(), first_col.0, last_col.0)
            else {
                return EvalScalar::Error(CalcError::Ref);
            };
            if r1 == r2 && c1 == c2 {
                let (Some(rid), Some(cid)) = (
                    target.row_axis().pos_to_id(r1),
                    target.col_axis().pos_to_id(c1),
                ) else {
                    return EvalScalar::Error(CalcError::Ref);
                };
                return read_cell_effective(workbook, spills, *sheet, RowId(rid), ColId(cid));
            }
            if c1 == c2
                && let Some(caller_r) = host.row_axis().id_to_pos(caller.row.0)
                && (r1..=r2).contains(&caller_r)
                && let (Some(rid), Some(cid)) = (
                    target.row_axis().pos_to_id(caller_r),
                    target.col_axis().pos_to_id(c1),
                )
            {
                return read_cell_effective(workbook, spills, *sheet, RowId(rid), ColId(cid));
            }
            if r1 == r2
                && let Some(caller_c) = host.col_axis().id_to_pos(caller.col.0)
                && (c1..=c2).contains(&caller_c)
                && let (Some(rid), Some(cid)) = (
                    target.row_axis().pos_to_id(r1),
                    target.col_axis().pos_to_id(caller_c),
                )
            {
                return read_cell_effective(workbook, spills, *sheet, RowId(rid), ColId(cid));
            }
            EvalScalar::Error(CalcError::Value)
        }
        ResolvedRef::FullCols {
            sheet,
            first_col,
            last_col,
        } => {
            let (Some(target), Some(host)) = (workbook.sheet(*sheet), workbook.sheet(caller.sheet))
            else {
                return EvalScalar::Error(CalcError::Ref);
            };
            let Some((c1, c2)) = live_span_positions(target.col_axis(), first_col.0, last_col.0)
            else {
                return EvalScalar::Error(CalcError::Ref);
            };
            if c1 == c2
                && let Some(caller_r) = host.row_axis().id_to_pos(caller.row.0)
                && let (Some(rid), Some(cid)) = (
                    target.row_axis().pos_to_id(caller_r),
                    target.col_axis().pos_to_id(c1),
                )
            {
                return read_cell_effective(workbook, spills, *sheet, RowId(rid), ColId(cid));
            }
            EvalScalar::Error(CalcError::Value)
        }
        ResolvedRef::FullRows {
            sheet,
            first_row,
            last_row,
        } => {
            let (Some(target), Some(host)) = (workbook.sheet(*sheet), workbook.sheet(caller.sheet))
            else {
                return EvalScalar::Error(CalcError::Ref);
            };
            let Some((r1, r2)) = live_span_positions(target.row_axis(), first_row.0, last_row.0)
            else {
                return EvalScalar::Error(CalcError::Ref);
            };
            if r1 == r2
                && let Some(caller_c) = host.col_axis().id_to_pos(caller.col.0)
                && let (Some(rid), Some(cid)) = (
                    target.row_axis().pos_to_id(r1),
                    target.col_axis().pos_to_id(caller_c),
                )
            {
                return read_cell_effective(workbook, spills, *sheet, RowId(rid), ColId(cid));
            }
            EvalScalar::Error(CalcError::Value)
        }
    }
}

/// Materialises an [`EvalValue`] into either a single [`EvalScalar`] or a 2D
/// [`Array2D`] according to `caller.mode` (`Legacy` intersects ranges to a
/// scalar; `Dynamic` materialises multi-cell references as arrays).
pub fn coerce_for_scalar_op(
    value: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<ScalarOrArray, CalcError> {
    if caller.mode == FormulaMode::Legacy {
        return Ok(ScalarOrArray::Scalar(implicit_intersect(
            value, workbook, spills, caller,
        )));
    }
    match value {
        EvalValue::Scalar(s) => Ok(ScalarOrArray::Scalar(s)),
        EvalValue::Array(arr) => {
            if arr.rows == 1 && arr.cols == 1 {
                Ok(ScalarOrArray::Scalar(
                    arr.values
                        .first()
                        .cloned()
                        .unwrap_or(EvalScalar::Error(CalcError::Calc)),
                ))
            } else {
                Ok(ScalarOrArray::Array(arr))
            }
        }
        EvalValue::Reference(r) => materialise_reference(&r, workbook, spills),
    }
}

/// Either a single scalar or a multi-cell 2D array ready for element-wise lifting.
pub enum ScalarOrArray {
    /// Single scalar.
    Scalar(EvalScalar),
    /// Multi-cell array (`rows * cols > 1`).
    Array(Array2D),
}

impl ScalarOrArray {
    const fn shape(&self) -> (u32, u32) {
        match self {
            Self::Scalar(_) => (1, 1),
            Self::Array(a) => (a.rows, a.cols),
        }
    }

    fn get_at(&self, r: u32, c: u32) -> EvalScalar {
        match self {
            Self::Scalar(s) => s.clone(),
            Self::Array(a) => a.get_broadcast(r, c),
        }
    }
}

/// Materialises a [`ResolvedRef`] into a [`ScalarOrArray`], returning `#CALC!`
/// if a full-column/row reference exceeds [`MAX_ARRAY_CELLS`].
pub fn materialise_reference(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> Result<ScalarOrArray, CalcError> {
    match r {
        ResolvedRef::Invalid(e) => Ok(ScalarOrArray::Scalar(EvalScalar::Error(*e))),
        ResolvedRef::Cell { sheet, row, col } => Ok(ScalarOrArray::Scalar(read_cell_effective(
            workbook, spills, *sheet, *row, *col,
        ))),
        ResolvedRef::Area {
            sheet,
            first_row,
            last_row,
            first_col,
            last_col,
        } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            materialise_spans(
                target,
                spills,
                first_row.0,
                last_row.0,
                first_col.0,
                last_col.0,
            )
        }
        ResolvedRef::FullCols {
            sheet,
            first_col,
            last_col,
        } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (Some(r_start), Some(r_end)) = (
                target.row_axis().pos_to_id(0),
                target
                    .row_axis()
                    .pos_to_id(target.row_axis().live_len().saturating_sub(1)),
            ) else {
                return Err(CalcError::Ref);
            };
            materialise_spans(target, spills, r_start, r_end, first_col.0, last_col.0)
        }
        ResolvedRef::FullRows {
            sheet,
            first_row,
            last_row,
        } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (Some(c_start), Some(c_end)) = (
                target.col_axis().pos_to_id(0),
                target
                    .col_axis()
                    .pos_to_id(target.col_axis().live_len().saturating_sub(1)),
            ) else {
                return Err(CalcError::Ref);
            };
            materialise_spans(target, spills, first_row.0, last_row.0, c_start, c_end)
        }
    }
}

fn materialise_spans(
    sheet: &Sheet,
    spills: &SpillRegistry,
    first_row: u64,
    last_row: u64,
    first_col: u64,
    last_col: u64,
) -> Result<ScalarOrArray, CalcError> {
    let mut row_ids = Vec::new();
    let mut row_count = 0usize;
    let mut overflow = false;
    let ok_r = sheet
        .row_axis()
        .for_each_live_in_range(first_row, last_row, |run| {
            row_count = row_count.saturating_add(run.len as usize);
            if row_count > MAX_ARRAY_CELLS {
                overflow = true;
                return;
            }
            row_ids.extend(run.iter());
        });
    if !ok_r {
        return Err(CalcError::Ref);
    }
    if overflow || row_ids.is_empty() {
        return Err(if overflow {
            CalcError::Calc
        } else {
            CalcError::Ref
        });
    }

    let mut col_ids = Vec::new();
    let ok_c = sheet
        .col_axis()
        .for_each_live_in_range(first_col, last_col, |run| {
            col_ids.extend(run.iter());
        });
    if !ok_c || col_ids.is_empty() {
        return Err(CalcError::Ref);
    }

    let total = row_ids
        .len()
        .checked_mul(col_ids.len())
        .ok_or(CalcError::Calc)?;
    if total > MAX_ARRAY_CELLS {
        return Err(CalcError::Calc);
    }
    let sheet_has_spills = spills.has_spills_on_sheet(sheet.id());
    if total == 1 {
        let (Some(&r), Some(&c)) = (row_ids.first(), col_ids.first()) else {
            return Err(CalcError::Ref);
        };
        let scalar = if sheet_has_spills {
            read_sheet_cell_effective(sheet, spills, RowId(r), ColId(c))
        } else {
            EvalScalar::from_sheet_value(sheet, sheet.value_by_id(RowId(r), ColId(c)))
        };
        return Ok(ScalarOrArray::Scalar(scalar));
    }

    let mut values = Vec::with_capacity(total);
    if sheet_has_spills {
        for &r in &row_ids {
            for &c in &col_ids {
                values.push(read_sheet_cell_effective(sheet, spills, RowId(r), ColId(c)));
            }
        }
    } else {
        for &r in &row_ids {
            for &c in &col_ids {
                values.push(EvalScalar::from_sheet_value(
                    sheet,
                    sheet.value_by_id(RowId(r), ColId(c)),
                ));
            }
        }
    }
    let rows_u32 = u32::try_from(row_ids.len()).map_err(|_| CalcError::Calc)?;
    let cols_u32 = u32::try_from(col_ids.len()).map_err(|_| CalcError::Calc)?;
    Ok(ScalarOrArray::Array(Array2D {
        rows: rows_u32,
        cols: cols_u32,
        values,
    }))
}

/// Reads the effective scalar value at `(sheet_id, row, col)` including any
/// materialized dynamic-array spill shadow overlay.
#[must_use]
pub fn read_cell_effective(
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    sheet_id: SheetId,
    row: RowId,
    col: ColId,
) -> EvalScalar {
    let Some(sheet) = workbook.sheet(sheet_id) else {
        return EvalScalar::Error(CalcError::Ref);
    };
    read_sheet_cell_effective(sheet, spills, row, col)
}

/// Reads the effective scalar value at `(row, col)` on `sheet` including any
/// materialized dynamic-array spill shadow overlay.
#[must_use]
pub fn read_sheet_cell_effective(
    sheet: &Sheet,
    spills: &SpillRegistry,
    row: RowId,
    col: ColId,
) -> EvalScalar {
    let val = sheet.value_by_id(row, col);
    if val != Value::Empty {
        return EvalScalar::from_sheet_value(sheet, val);
    }
    spills
        .shadow_value(sheet.id(), row, col)
        .cloned()
        .unwrap_or(EvalScalar::Empty)
}

/// Evaluates a unary operator with D-028 dynamic-array lifting or `@`
/// implicit intersection.
#[must_use]
pub fn eval_unary(
    op: UnaryOp,
    operand: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if op == UnaryOp::ImplicitIntersection {
        return EvalValue::Scalar(implicit_intersect(operand, workbook, spills, caller));
    }
    let lifted = match coerce_for_scalar_op(operand, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match lifted {
        ScalarOrArray::Scalar(s) => EvalValue::Scalar(apply_unary_scalar(op, &s)),
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|s| apply_unary_scalar(op, s))
                .collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn apply_unary_scalar(op: UnaryOp, s: &EvalScalar) -> EvalScalar {
    let n = match s.to_number() {
        Ok(v) => v,
        Err(e) => return EvalScalar::Error(e),
    };
    match op {
        UnaryOp::Plus => EvalScalar::number(n),
        UnaryOp::Neg => EvalScalar::number(-n),
        UnaryOp::Percent => EvalScalar::number(n / 100.0),
        UnaryOp::ImplicitIntersection => s.clone(),
    }
}

/// Evaluates a binary operator with D-027 15-digit comparison / outermost
/// tail-zeroing and D-028 dynamic-array broadcasting.
#[must_use]
pub fn eval_binary(
    op: BinaryOp,
    lhs: EvalValue,
    rhs: EvalValue,
    outermost_add_sub: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    let left = match coerce_for_scalar_op(lhs, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let right = match coerce_for_scalar_op(rhs, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    if let (ScalarOrArray::Scalar(a), ScalarOrArray::Scalar(b)) = (&left, &right) {
        return EvalValue::Scalar(apply_binary_scalar(op, a, b, outermost_add_sub));
    }

    let (r1, c1) = left.shape();
    let (r2, c2) = right.shape();
    let out_rows = r1.max(r2);
    let out_cols = c1.max(c2);
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }

    let mut values = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            let a = left.get_at(r, c);
            let b = right.get_at(r, c);
            values.push(apply_binary_scalar(op, &a, &b, outermost_add_sub));
        }
    }
    match Array2D::new(out_rows, out_cols, values) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates a single scalar-by-scalar binary operation under D-027.
#[must_use]
pub fn apply_binary_scalar(
    op: BinaryOp,
    a: &EvalScalar,
    b: &EvalScalar,
    outermost_add_sub: bool,
) -> EvalScalar {
    if let EvalScalar::Error(e) = a {
        return EvalScalar::Error(*e);
    }
    if let EvalScalar::Error(e) = b {
        return EvalScalar::Error(*e);
    }

    match op {
        BinaryOp::Add => {
            let (Ok(x), Ok(y)) = (a.to_number(), b.to_number()) else {
                return first_num_error(a, b);
            };
            let raw = x + y;
            if !raw.is_finite() {
                return EvalScalar::Error(CalcError::Num);
            }
            let snapped = if outermost_add_sub {
                snap_cancelled_zero(x, raw)
            } else {
                raw
            };
            EvalScalar::number(snapped)
        }
        BinaryOp::Sub => {
            let (Ok(x), Ok(y)) = (a.to_number(), b.to_number()) else {
                return first_num_error(a, b);
            };
            let raw = x - y;
            if !raw.is_finite() {
                return EvalScalar::Error(CalcError::Num);
            }
            let snapped = if outermost_add_sub {
                snap_cancelled_zero(x, raw)
            } else {
                raw
            };
            EvalScalar::number(snapped)
        }
        BinaryOp::Mul => {
            let (Ok(x), Ok(y)) = (a.to_number(), b.to_number()) else {
                return first_num_error(a, b);
            };
            EvalScalar::number(x * y)
        }
        BinaryOp::Div => {
            let (Ok(x), Ok(y)) = (a.to_number(), b.to_number()) else {
                return first_num_error(a, b);
            };
            if y == 0.0 {
                return EvalScalar::Error(CalcError::Div0);
            }
            EvalScalar::number(x / y)
        }
        BinaryOp::Pow => {
            let (Ok(x), Ok(y)) = (a.to_number(), b.to_number()) else {
                return first_num_error(a, b);
            };
            if x == 0.0 && y == 0.0 {
                return EvalScalar::Error(CalcError::Num);
            }
            if x == 0.0 && y < 0.0 {
                return EvalScalar::Error(CalcError::Div0);
            }
            if x < 0.0 && num::trunc(y) != y {
                return EvalScalar::Error(CalcError::Num);
            }
            EvalScalar::number(num::pow(x, y))
        }
        BinaryOp::Concat => {
            let mut s1 = match a.to_text() {
                Ok(s) => s,
                Err(e) => return EvalScalar::Error(e),
            };
            let s2 = match b.to_text() {
                Ok(s) => s,
                Err(e) => return EvalScalar::Error(e),
            };
            s1.push_str(&s2);
            EvalScalar::Text(s1)
        }
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            let ord = compare_excel_scalars(a, b);
            let flag = match op {
                BinaryOp::Eq => ord == Ordering::Equal,
                BinaryOp::Ne => ord != Ordering::Equal,
                BinaryOp::Lt => ord == Ordering::Less,
                BinaryOp::Gt => ord == Ordering::Greater,
                BinaryOp::Le => ord != Ordering::Greater,
                BinaryOp::Ge => ord != Ordering::Less,
                _ => false,
            };
            EvalScalar::Bool(flag)
        }
    }
}

fn first_num_error(a: &EvalScalar, b: &EvalScalar) -> EvalScalar {
    if let Err(e) = a.to_number() {
        return EvalScalar::Error(e);
    }
    if let Err(e) = b.to_number() {
        return EvalScalar::Error(e);
    }
    EvalScalar::Error(CalcError::Value)
}

/// Compares two non-error scalars under Excel's total order:
/// - `Empty` coerces to the zero value of the counterpart (`0.0`, `""`, `false`);
/// - Cross-type order is `Number < Text < Bool`;
/// - Numbers compare after rounding both sides to 15 significant digits (D-027);
/// - Strings compare case-insensitively.
#[must_use]
pub fn compare_excel_scalars(a: &EvalScalar, b: &EvalScalar) -> Ordering {
    match (a, b) {
        (EvalScalar::Empty, EvalScalar::Empty) => Ordering::Equal,
        (EvalScalar::Empty, EvalScalar::Number(y)) => {
            cmp_excel_f64(0.0, *y).unwrap_or(Ordering::Equal)
        }
        (EvalScalar::Number(x), EvalScalar::Empty) => {
            cmp_excel_f64(*x, 0.0).unwrap_or(Ordering::Equal)
        }
        (EvalScalar::Empty, EvalScalar::Text(y)) => cmp_text_ci("", y),
        (EvalScalar::Text(x), EvalScalar::Empty) => cmp_text_ci(x, ""),
        (EvalScalar::Empty, EvalScalar::Bool(y)) => false.cmp(y),
        (EvalScalar::Bool(x), EvalScalar::Empty) => x.cmp(&false),

        (EvalScalar::Number(x), EvalScalar::Number(y)) => {
            cmp_excel_f64(*x, *y).unwrap_or(Ordering::Equal)
        }
        (EvalScalar::Text(x), EvalScalar::Text(y)) => cmp_text_ci(x, y),
        (EvalScalar::Bool(x), EvalScalar::Bool(y)) => x.cmp(y),

        // Cross-type: Number < Text < Bool.
        (EvalScalar::Number(_), EvalScalar::Text(_) | EvalScalar::Bool(_)) => Ordering::Less,
        (EvalScalar::Text(_) | EvalScalar::Bool(_), EvalScalar::Number(_)) => Ordering::Greater,
        (EvalScalar::Text(_), EvalScalar::Bool(_)) => Ordering::Less,
        (EvalScalar::Bool(_), EvalScalar::Text(_)) => Ordering::Greater,

        (EvalScalar::Error(_), _) | (_, EvalScalar::Error(_)) => Ordering::Equal,
    }
}

fn cmp_text_ci(a: &str, b: &str) -> Ordering {
    let mut ia = a.chars().flat_map(char::to_uppercase);
    let mut ib = b.chars().flat_map(char::to_uppercase);
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => match ca.cmp(&cb) {
                Ordering::Equal => {}
                non_eq => return non_eq,
            },
        }
    }
}

/// Converts a top-level formula result [`EvalValue`] into the [`EvalScalar`] to
/// cache in the cell:
/// - `Legacy` mode applies implicit intersection;
/// - `Dynamic` mode unwraps a `1×1` reference or array (turning a blank target
///   reference `=A1` into `0.0` just like Excel), and returns `#SPILL!` for any
///   array or range with more than one cell (`13-m1a.md` 5.4).
#[must_use]
pub fn finalize_top_level(
    value: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalScalar {
    if caller.mode == FormulaMode::Legacy {
        let s = implicit_intersect(value, workbook, spills, caller);
        return empty_ref_becomes_zero(s);
    }
    match value {
        EvalValue::Scalar(s) => s,
        EvalValue::Array(arr) => {
            if arr.rows == 1 && arr.cols == 1 {
                arr.values
                    .first()
                    .cloned()
                    .unwrap_or(EvalScalar::Error(CalcError::Calc))
            } else {
                EvalScalar::Error(CalcError::Spill)
            }
        }
        EvalValue::Reference(r) => match materialise_reference(&r, workbook, spills) {
            Ok(ScalarOrArray::Scalar(s)) => empty_ref_becomes_zero(s),
            Ok(ScalarOrArray::Array(_)) => EvalScalar::Error(CalcError::Spill),
            Err(e) => EvalScalar::Error(e),
        },
    }
}

fn empty_ref_becomes_zero(s: EvalScalar) -> EvalScalar {
    match s {
        EvalScalar::Empty => EvalScalar::Number(0.0),
        other => other,
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

    use alloc::vec;

    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{
        CellEdit, CellInput, FormatPatch, FormulaMode, Op, ScalarValue, Workbook,
    };
    use reticula_core::value::CalcError;

    use super::{
        Array2D, CallerContext, EvalScalar, EvalValue, apply_binary_scalar, eval_binary,
        finalize_top_level, implicit_intersect, resolve_bound_ref,
    };
    use crate::ast::BinaryOp;
    use crate::binder::{BoundFormula, BoundRef};
    use crate::spill::SpillRegistry;

    #[test]
    fn d027_comparisons_and_outermost_tail_zeroing_match_excel() {
        // 0.1 + 0.2 == 0.3 under 15-digit comparison!
        let sum = EvalScalar::Number(0.1_f64 + 0.2_f64);
        let three_tenths = EvalScalar::Number(0.3_f64);
        assert_eq!(
            apply_binary_scalar(BinaryOp::Eq, &sum, &three_tenths, false),
            EvalScalar::Bool(true)
        );

        // Outermost subtraction snaps `0.3 - (0.1 + 0.2)` to exact `0.0`, while
        // parenthesised `(0.3 - (0.1 + 0.2))` keeps the non-zero IEEE residual.
        let snapped = apply_binary_scalar(BinaryOp::Sub, &three_tenths, &sum, true);
        let unsnapped = apply_binary_scalar(BinaryOp::Sub, &three_tenths, &sum, false);
        assert_eq!(snapped, EvalScalar::Number(0.0));
        assert!(matches!(unsnapped, EvalScalar::Number(v) if v != 0.0));
    }

    #[test]
    fn d028_dynamic_broadcasting_spill_and_legacy_intersection_work() {
        let mut wb: Workbook<BoundFormula> = Workbook::new();
        let sheet = SheetId(0);
        wb.apply(Op::SetCells {
            sheet,
            edits: vec![
                CellEdit {
                    row: RowId(1),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(10.0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(2),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(20.0)),
                    format: FormatPatch::Keep,
                },
            ],
        })
        .unwrap();

        let area_ref = BoundRef::Area {
            sheet: None,
            missing_sheet: None,
            first_row: RowId(1),
            first_col: ColId(0),
            last_row: RowId(2),
            last_col: ColId(0),
            abs_first_col: false,
            abs_first_row: false,
            abs_last_col: false,
            abs_last_row: false,
        };
        let resolved = resolve_bound_ref(&area_ref, &wb, sheet);

        let spills = SpillRegistry::new();

        // In Legacy mode (or `@A2:A3`), row 2 (`RowId(1)`) intersects `A2:A3` -> `10.0`,
        // while row 5 (`RowId(4)`) is outside `A2:A3` -> `#VALUE!`.
        let legacy_hit = implicit_intersect(
            EvalValue::Reference(resolved.clone()),
            &wb,
            &spills,
            CallerContext {
                sheet,
                row: RowId(1),
                col: ColId(1),
                mode: FormulaMode::Legacy,
            },
        );
        assert_eq!(legacy_hit, EvalScalar::Number(10.0));

        let legacy_miss = implicit_intersect(
            EvalValue::Reference(resolved.clone()),
            &wb,
            &spills,
            CallerContext {
                sheet,
                row: RowId(4),
                col: ColId(1),
                mode: FormulaMode::Legacy,
            },
        );
        assert_eq!(legacy_miss, EvalScalar::Error(CalcError::Value));

        // In Dynamic mode, `A2:A3 * 2` broadcasts to `[20, 40]`, which spills (`#SPILL!`)
        // when returned directly as a cell result.
        let dyn_caller = CallerContext {
            sheet,
            row: RowId(1),
            col: ColId(1),
            mode: FormulaMode::Dynamic,
        };
        let multiplied = eval_binary(
            BinaryOp::Mul,
            EvalValue::Reference(resolved),
            EvalValue::Scalar(EvalScalar::Number(2.0)),
            false,
            &wb,
            &spills,
            dyn_caller,
        );
        assert_eq!(
            multiplied,
            EvalValue::Array(Array2D {
                rows: 2,
                cols: 1,
                values: vec![EvalScalar::Number(20.0), EvalScalar::Number(40.0)],
            })
        );
        assert_eq!(
            finalize_top_level(multiplied, &wb, &spills, dyn_caller),
            EvalScalar::Error(CalcError::Spill)
        );
    }
}
