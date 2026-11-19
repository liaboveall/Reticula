//! Layer-1 conditional aggregation functions (`COUNTBLANK`, `COUNTIF`,
//! `SUMIF`, `AVERAGEIF`; `14-m1b.md` 5.3).
//!
//! Numeric criteria comparisons use D-027 15-significant-digit rules
//! ([`cmp_excel_f64`]); per S3b Excel 16.0.20711 measurements, `SUMIF` does not
//! apply outermost cancellation tail-zeroing internally.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::decimal15::cmp_excel_f64;
use reticula_core::id::{ColId, IdRun, RowId};
use reticula_core::ops::Workbook;
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block};
use reticula_core::value::{CalcError, Value};

use super::text_conv::parse_excel_value_str;
use super::visit_reference_words;
use super::wildcard::{WildToken, compile_wildcard, wildcard_matches_full};
use crate::binder::BoundFormula;
use crate::value_eval::{
    CallerContext, EvalScalar, EvalValue, ResolvedRef, ScalarOrArray, coerce_for_scalar_op,
};

/// Evaluates `COUNTBLANK(range)`.
pub fn eval_countblank(args: &[EvalValue], workbook: &Workbook<BoundFormula>) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(arg0) = args.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    match arg0 {
        EvalValue::Reference(r) => match count_blanks_in_ref(r, workbook) {
            Ok(cnt) =>
            {
                #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
                EvalValue::Scalar(EvalScalar::Number(cnt as f64))
            }
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        },
        EvalValue::Array(arr) => {
            let cnt = arr
                .values
                .iter()
                .filter(|v| {
                    matches!(v, EvalScalar::Empty)
                        || matches!(v, EvalScalar::Text(s) if s.is_empty())
                })
                .count();
            #[expect(clippy::cast_precision_loss, reason = "array size <= 1M fits in f64")]
            EvalValue::Scalar(EvalScalar::Number(cnt as f64))
        }
        EvalValue::Scalar(EvalScalar::Error(e)) => EvalValue::Scalar(EvalScalar::Error(*e)),
        EvalValue::Scalar(_) => EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
    }
}

fn count_blanks_in_ref(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
) -> Result<u64, CalcError> {
    let target_sheet = match r {
        ResolvedRef::Invalid(e) => return Err(*e),
        ResolvedRef::Cell { sheet, .. }
        | ResolvedRef::Area { sheet, .. }
        | ResolvedRef::FullCols { sheet, .. }
        | ResolvedRef::FullRows { sheet, .. } => workbook.sheet(*sheet).ok_or(CalcError::Ref)?,
    };
    let mut empty_blanks: u64 = 0;
    let mut text_blanks: u64 = 0;
    visit_reference_words_fast_empty(
        r,
        workbook,
        |empty_count| {
            empty_blanks = empty_blanks.saturating_add(empty_count);
        },
        |bits| {
            match Value::unpack(bits) {
                Value::Empty => text_blanks = text_blanks.saturating_add(1),
                Value::Text(id) if target_sheet.strings().get(id).is_empty() => {
                    text_blanks = text_blanks.saturating_add(1);
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    Ok(empty_blanks.saturating_add(text_blanks))
}

/// Evaluates `COUNTIF(range, criteria)`.
pub fn eval_countif(
    mut args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    caller: CallerContext,
) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(crit_val) = args.pop() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let Some(range_val) = args.pop() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };

    let crit_sa = match coerce_for_scalar_op(crit_val, workbook, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match crit_sa {
        ScalarOrArray::Scalar(c) => EvalValue::Scalar(countif_one(&range_val, &c, workbook)),
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|c| countif_one(&range_val, c, workbook))
                .collect();
            EvalValue::Array(crate::value_eval::Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn countif_one(
    range_val: &EvalValue,
    crit_scalar: &EvalScalar,
    workbook: &Workbook<BoundFormula>,
) -> EvalScalar {
    let pred = CompiledCriterion::compile(crit_scalar);
    match range_val {
        EvalValue::Reference(r) => {
            let sheet = match r {
                ResolvedRef::Invalid(e) => return EvalScalar::Error(*e),
                ResolvedRef::Cell { sheet, .. }
                | ResolvedRef::Area { sheet, .. }
                | ResolvedRef::FullCols { sheet, .. }
                | ResolvedRef::FullRows { sheet, .. } => {
                    let Some(s) = workbook.sheet(*sheet) else {
                        return EvalScalar::Error(CalcError::Ref);
                    };
                    s
                }
            };
            let matches_empty = pred.matches(&EvalScalar::Empty);
            let mut empty_matches: u64 = 0;
            let mut non_empty_matches: u64 = 0;
            let res = visit_reference_words_fast_empty(
                r,
                workbook,
                |empty_n| {
                    if matches_empty {
                        empty_matches = empty_matches.saturating_add(empty_n);
                    }
                },
                |bits| {
                    let cell_val = EvalScalar::from_sheet_value(sheet, Value::unpack(bits));
                    if pred.matches(&cell_val) {
                        non_empty_matches = non_empty_matches.saturating_add(1);
                    }
                    Ok(())
                },
            );
            let count = empty_matches.saturating_add(non_empty_matches);
            match res {
                Ok(()) =>
                {
                    #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
                    EvalScalar::Number(count as f64)
                }
                Err(e) => EvalScalar::Error(e),
            }
        }
        EvalValue::Array(arr) => {
            let count = arr.values.iter().filter(|v| pred.matches(v)).count();
            #[expect(clippy::cast_precision_loss, reason = "array size fits in f64")]
            EvalScalar::Number(count as f64)
        }
        EvalValue::Scalar(EvalScalar::Error(e)) => EvalScalar::Error(*e),
        EvalValue::Scalar(_) => EvalScalar::Error(CalcError::Value),
    }
}

/// Evaluates `SUMIF(range, criteria, [sum_range])` (`is_average == false`) or
/// `AVERAGEIF(range, criteria, [average_range])` (`is_average == true`).
pub fn eval_sumif_or_averageif(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    caller: CallerContext,
    is_average: bool,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(range_val), Some(crit_val)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let val_range = it.next();

    let crit_sa = match coerce_for_scalar_op(crit_val, workbook, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    match crit_sa {
        ScalarOrArray::Scalar(c) => EvalValue::Scalar(sumif_one(
            &range_val,
            &c,
            val_range.as_ref(),
            workbook,
            is_average,
        )),
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|c| sumif_one(&range_val, c, val_range.as_ref(), workbook, is_average))
                .collect();
            EvalValue::Array(crate::value_eval::Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn sumif_one(
    range_val: &EvalValue,
    crit_scalar: &EvalScalar,
    val_range_opt: Option<&EvalValue>,
    workbook: &Workbook<BoundFormula>,
    is_average: bool,
) -> EvalScalar {
    let pred = CompiledCriterion::compile(crit_scalar);

    // Per S3b Excel 16.0.20711: `SUMIF` does NOT apply cancellation tail-zeroing!
    let mut sum = 0.0_f64;
    let mut count: u64 = 0;

    match (range_val, val_range_opt) {
        (EvalValue::Reference(crit_ref), None) => {
            let sheet = match crit_ref {
                ResolvedRef::Invalid(e) => return EvalScalar::Error(*e),
                ResolvedRef::Cell { sheet, .. }
                | ResolvedRef::Area { sheet, .. }
                | ResolvedRef::FullCols { sheet, .. }
                | ResolvedRef::FullRows { sheet, .. } => {
                    let Some(s) = workbook.sheet(*sheet) else {
                        return EvalScalar::Error(CalcError::Ref);
                    };
                    s
                }
            };
            let res = visit_reference_words(crit_ref, workbook, |bits| {
                let raw_val = Value::unpack(bits);
                let cell_s = EvalScalar::from_sheet_value(sheet, raw_val);
                if pred.matches(&cell_s)
                    && let EvalScalar::Number(n) = cell_s
                {
                    sum += n;
                    if !sum.is_finite() {
                        return Err(CalcError::Num);
                    }
                    count = count.saturating_add(1);
                }
                Ok(())
            });
            if let Err(e) = res {
                return EvalScalar::Error(e);
            }
        }
        (EvalValue::Reference(crit_ref), Some(EvalValue::Reference(sum_ref))) => {
            if let Err(e) =
                sumif_paired_refs(crit_ref, sum_ref, &pred, workbook, &mut sum, &mut count)
            {
                return EvalScalar::Error(e);
            }
        }
        (EvalValue::Array(crit_arr), None) => {
            for item in &crit_arr.values {
                if pred.matches(item) {
                    match item {
                        EvalScalar::Error(e) => return EvalScalar::Error(*e),
                        EvalScalar::Number(n) => {
                            sum += *n;
                            if !sum.is_finite() {
                                return EvalScalar::Error(CalcError::Num);
                            }
                            count = count.saturating_add(1);
                        }
                        _ => {}
                    }
                }
            }
        }
        (EvalValue::Scalar(EvalScalar::Error(e)), _)
        | (_, Some(EvalValue::Scalar(EvalScalar::Error(e)))) => return EvalScalar::Error(*e),
        _ => return EvalScalar::Error(CalcError::Value),
    }

    if is_average {
        if count == 0 {
            return EvalScalar::Error(CalcError::Div0);
        }
        #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
        EvalScalar::number(sum / (count as f64))
    } else {
        EvalScalar::number(sum)
    }
}

/// Evaluates `SUMIF` / `AVERAGEIF` when a separate `sum_range` reference is
/// supplied: Excel anchors `sum_range` at its top-left live coordinate and
/// takes the exact `(rows, cols)` shape of `crit_ref`.
fn sumif_paired_refs(
    crit_ref: &ResolvedRef,
    sum_ref: &ResolvedRef,
    pred: &CompiledCriterion,
    workbook: &Workbook<BoundFormula>,
    sum: &mut f64,
    count: &mut u64,
) -> Result<(), CalcError> {
    let (crit_sheet_id, crit_r0, crit_rows, crit_c0, crit_cols) =
        ref_live_origin_and_size(crit_ref, workbook)?;
    let (sum_sheet_id, sum_r0, _, sum_c0, _) = ref_live_origin_and_size(sum_ref, workbook)?;
    let crit_sheet = workbook.sheet(crit_sheet_id).ok_or(CalcError::Ref)?;
    let sum_sheet = workbook.sheet(sum_sheet_id).ok_or(CalcError::Ref)?;

    let mut crit_col_ids = Vec::with_capacity(crit_cols as usize);
    crit_sheet
        .col_axis()
        .for_each_live_positions(crit_c0, crit_cols, |run| crit_col_ids.extend(run.iter()));
    let mut sum_col_ids = Vec::with_capacity(crit_cols as usize);
    sum_sheet
        .col_axis()
        .for_each_live_positions(sum_c0, crit_cols, |run| sum_col_ids.extend(run.iter()));

    let mut crit_row_ids = Vec::new();
    crit_sheet
        .row_axis()
        .for_each_live_positions(crit_r0, crit_rows, |run| crit_row_ids.extend(run.iter()));
    let mut sum_row_ids = Vec::new();
    sum_sheet
        .row_axis()
        .for_each_live_positions(sum_r0, crit_rows, |run| sum_row_ids.extend(run.iter()));

    for (r_idx, &crit_r_raw) in crit_row_ids.iter().enumerate() {
        let sum_r_opt = sum_row_ids.get(r_idx).copied();
        for (c_idx, &crit_c_raw) in crit_col_ids.iter().enumerate() {
            let crit_val = EvalScalar::from_sheet_value(
                crit_sheet,
                crit_sheet.value_by_id(RowId(crit_r_raw), ColId(crit_c_raw)),
            );
            if pred.matches(&crit_val) {
                let Some(sr) = sum_r_opt else {
                    continue;
                };
                let Some(&sc) = sum_col_ids.get(c_idx) else {
                    continue;
                };
                match sum_sheet.value_by_id(RowId(sr), ColId(sc)) {
                    Value::Error(e) => return Err(e),
                    Value::Array(_) => return Err(CalcError::Spill),
                    Value::Number(n) => {
                        *sum += n;
                        if !sum.is_finite() {
                            return Err(CalcError::Num);
                        }
                        *count = count.saturating_add(1);
                    }
                    Value::Empty | Value::Text(_) | Value::Bool(_) => {}
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn ref_live_origin_and_size(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
) -> Result<(reticula_core::id::SheetId, u32, u32, u32, u32), CalcError> {
    match r {
        ResolvedRef::Invalid(e) => Err(*e),
        ResolvedRef::Cell { sheet, row, col } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let rp = s.row_axis().id_to_pos(row.0).ok_or(CalcError::Ref)?;
            let cp = s.col_axis().id_to_pos(col.0).ok_or(CalcError::Ref)?;
            Ok((*sheet, rp, 1, cp, 1))
        }
        ResolvedRef::Area {
            sheet,
            first_row,
            last_row,
            first_col,
            last_col,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (r1, r2) =
                crate::binder::live_span_positions(s.row_axis(), first_row.0, last_row.0)
                    .ok_or(CalcError::Ref)?;
            let (c1, c2) =
                crate::binder::live_span_positions(s.col_axis(), first_col.0, last_col.0)
                    .ok_or(CalcError::Ref)?;
            Ok((
                *sheet,
                r1,
                r2.saturating_sub(r1).saturating_add(1),
                c1,
                c2.saturating_sub(c1).saturating_add(1),
            ))
        }
        ResolvedRef::FullCols {
            sheet,
            first_col,
            last_col,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let rows = s.row_axis().live_len();
            if rows == 0 {
                return Err(CalcError::Ref);
            }
            let (c1, c2) =
                crate::binder::live_span_positions(s.col_axis(), first_col.0, last_col.0)
                    .ok_or(CalcError::Ref)?;
            Ok((*sheet, 0, rows, c1, c2.saturating_sub(c1).saturating_add(1)))
        }
        ResolvedRef::FullRows {
            sheet,
            first_row,
            last_row,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let cols = s.col_axis().live_len();
            if cols == 0 {
                return Err(CalcError::Ref);
            }
            let (r1, r2) =
                crate::binder::live_span_positions(s.row_axis(), first_row.0, last_row.0)
                    .ok_or(CalcError::Ref)?;
            Ok((*sheet, r1, r2.saturating_sub(r1).saturating_add(1), 0, cols))
        }
    }
}

/// Fast block-aware word visitor that invokes `on_empty_run(count)` for
/// contiguous empty blocks/columns instead of calling `on_word` millions of
/// times on empty cells (critical for `COUNTBLANK(A:A)` and `COUNTIF(A:A, ...)`).
fn visit_reference_words_fast_empty(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    mut on_empty_run: impl FnMut(u64),
    mut on_word: impl FnMut(u64) -> Result<(), CalcError>,
) -> Result<(), CalcError> {
    let (sheet_id, first_row, last_row, first_col, last_col) = match r {
        ResolvedRef::Invalid(e) => return Err(*e),
        ResolvedRef::Cell { sheet, row, col } => {
            let target = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let bits = target.value_by_id(*row, *col).pack();
            if bits == Value::EMPTY_BITS {
                on_empty_run(1);
                return Ok(());
            }
            return on_word(bits);
        }
        ResolvedRef::Area {
            sheet,
            first_row,
            last_row,
            first_col,
            last_col,
        } => (*sheet, first_row.0, last_row.0, first_col.0, last_col.0),
        ResolvedRef::FullCols {
            sheet,
            first_col,
            last_col,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (Some(r1), Some(r2)) = (
                s.row_axis().pos_to_id(0),
                s.row_axis()
                    .pos_to_id(s.row_axis().live_len().saturating_sub(1)),
            ) else {
                return Err(CalcError::Ref);
            };
            (*sheet, r1, r2, first_col.0, last_col.0)
        }
        ResolvedRef::FullRows {
            sheet,
            first_row,
            last_row,
        } => {
            let s = workbook.sheet(*sheet).ok_or(CalcError::Ref)?;
            let (Some(c1), Some(c2)) = (
                s.col_axis().pos_to_id(0),
                s.col_axis()
                    .pos_to_id(s.col_axis().live_len().saturating_sub(1)),
            ) else {
                return Err(CalcError::Ref);
            };
            (*sheet, first_row.0, last_row.0, c1, c2)
        }
    };

    let sheet = workbook.sheet(sheet_id).ok_or(CalcError::Ref)?;
    let mut col_ids = Vec::new();
    if !sheet
        .col_axis()
        .for_each_live_in_range(first_col, last_col, |run| col_ids.extend(run.iter()))
        || col_ids.is_empty()
    {
        return Err(CalcError::Ref);
    }
    let mut row_runs: Vec<IdRun> = Vec::new();
    if !sheet
        .row_axis()
        .for_each_live_in_range(first_row, last_row, |run| row_runs.push(run))
        || row_runs.is_empty()
    {
        return Err(CalcError::Ref);
    }
    let total_rows: u64 = row_runs
        .iter()
        .fold(0_u64, |acc, run| acc.saturating_add(u64::from(run.len)));

    for &col_id in &col_ids {
        let Some(col_slot) = sheet.col_slots().get(col_id) else {
            on_empty_run(total_rows);
            continue;
        };
        let Some(column) = sheet.raw_columns().get(col_slot as usize) else {
            on_empty_run(total_rows);
            continue;
        };
        stream_col_runs_fast_empty(sheet, column, &row_runs, &mut on_empty_run, &mut on_word)?;
    }
    Ok(())
}

fn stream_col_runs_fast_empty(
    sheet: &Sheet,
    column: &reticula_core::store::Column,
    row_runs: &[IdRun],
    on_empty_run: &mut impl FnMut(u64),
    on_word: &mut impl FnMut(u64) -> Result<(), CalcError>,
) -> Result<(), CalcError> {
    let base_bound = u64::from(sheet.row_slots().base_len());
    let blocks = column.blocks();
    for run in row_runs {
        let end_excl = run.first_id.saturating_add(u64::from(run.len));
        if end_excl <= base_bound
            && let Ok(mut cur) = u32::try_from(run.first_id)
        {
            let end = cur.saturating_add(run.len);
            while cur < end {
                let block_idx = (cur / BLOCK_ROWS) as usize;
                let off = (cur % BLOCK_ROWS) as usize;
                let rem = (BLOCK_ROWS as usize).saturating_sub(off);
                let take = rem.min((end.saturating_sub(cur)) as usize);
                match blocks.get(block_idx) {
                    None | Some(Block::Empty) => {
                        on_empty_run(take as u64);
                    }
                    Some(Block::Dense(words)) => {
                        if let Some(slice) = words.get(off..off.saturating_add(take)) {
                            for &bits in slice {
                                if bits == Value::EMPTY_BITS {
                                    on_empty_run(1);
                                } else {
                                    on_word(bits)?;
                                }
                            }
                        }
                    }
                    Some(Block::Sparse(_)) => {
                        let take_u32 = u32::try_from(take).unwrap_or(0);
                        for s in cur..cur.saturating_add(take_u32) {
                            let bits = column.get_packed(s);
                            if bits == Value::EMPTY_BITS {
                                on_empty_run(1);
                            } else {
                                on_word(bits)?;
                            }
                        }
                    }
                }
                cur = cur.saturating_add(u32::try_from(take).unwrap_or(1));
            }
        } else {
            for row_id in run.iter() {
                let bits = sheet
                    .row_slots()
                    .get(row_id)
                    .map_or(Value::EMPTY_BITS, |slot| column.get_packed(slot));
                if bits == Value::EMPTY_BITS {
                    on_empty_run(1);
                } else {
                    on_word(bits)?;
                }
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------- criterion compiler

#[derive(Clone, Copy)]
enum RelOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

enum CompiledCriterion {
    Never,
    NotEmpty,
    EqEmptyOrBlankText,
    EqZeroOnly,
    NumRel(RelOp, f64),
    BoolRel {
        target: bool,
        not_equal: bool,
    },
    ErrorRel {
        target: CalcError,
        not_equal: bool,
    },
    TextWild {
        tokens: Vec<WildToken>,
        not_equal: bool,
    },
    TextRel(RelOp, String),
}

impl CompiledCriterion {
    fn compile(crit: &EvalScalar) -> Self {
        match crit {
            EvalScalar::Empty => Self::EqZeroOnly,
            EvalScalar::Number(n) => Self::NumRel(RelOp::Eq, *n),
            EvalScalar::Bool(b) => Self::BoolRel {
                target: *b,
                not_equal: false,
            },
            EvalScalar::Error(e) => Self::ErrorRel {
                target: *e,
                not_equal: false,
            },
            EvalScalar::Text(s) => Self::compile_text(s),
        }
    }

    fn compile_text(s: &str) -> Self {
        let (op, rest, explicit_op) = if let Some(r) = s.strip_prefix("<=") {
            (RelOp::Le, r, true)
        } else if let Some(r) = s.strip_prefix(">=") {
            (RelOp::Ge, r, true)
        } else if let Some(r) = s.strip_prefix("<>") {
            (RelOp::Ne, r, true)
        } else if let Some(r) = s.strip_prefix('<') {
            (RelOp::Lt, r, true)
        } else if let Some(r) = s.strip_prefix('>') {
            (RelOp::Gt, r, true)
        } else if let Some(r) = s.strip_prefix('=') {
            (RelOp::Eq, r, true)
        } else {
            (RelOp::Eq, s, false)
        };

        if rest.is_empty() {
            return match op {
                RelOp::Eq => Self::EqEmptyOrBlankText,
                RelOp::Ne => Self::NotEmpty,
                RelOp::Lt | RelOp::Le | RelOp::Gt | RelOp::Ge => Self::Never,
            };
        }

        // Numeric / ISO date criterion.
        if let Some(num) = parse_excel_value_str(rest) {
            return Self::NumRel(op, num);
        }

        // Boolean / error criterion on `=` or `<>`.
        if matches!(op, RelOp::Eq | RelOp::Ne) {
            let not_eq = matches!(op, RelOp::Ne);
            if rest.eq_ignore_ascii_case("TRUE") {
                return Self::BoolRel {
                    target: true,
                    not_equal: not_eq,
                };
            }
            if rest.eq_ignore_ascii_case("FALSE") {
                return Self::BoolRel {
                    target: false,
                    not_equal: not_eq,
                };
            }
            if let Some(err) = CalcError::parse_literal(rest) {
                return Self::ErrorRel {
                    target: err,
                    not_equal: not_eq,
                };
            }
            let _ = explicit_op;
            let (tokens, _) = compile_wildcard(rest);
            return Self::TextWild {
                tokens,
                not_equal: not_eq,
            };
        }

        Self::TextRel(op, rest.to_lowercase())
    }

    fn matches(&self, val: &EvalScalar) -> bool {
        match self {
            Self::Never => false,
            Self::NotEmpty => !matches!(val, EvalScalar::Empty),
            Self::EqEmptyOrBlankText => {
                matches!(val, EvalScalar::Empty)
                    || matches!(val, EvalScalar::Text(s) if s.is_empty())
            }
            Self::EqZeroOnly => {
                matches!(val, EvalScalar::Number(n) if cmp_excel_f64(*n, 0.0) == Some(Ordering::Equal))
            }
            Self::NumRel(op, target) => match op {
                RelOp::Eq => match val {
                    EvalScalar::Number(n) => cmp_excel_f64(*n, *target) == Some(Ordering::Equal),
                    EvalScalar::Text(s) => parse_excel_value_str(s)
                        .is_some_and(|n| cmp_excel_f64(n, *target) == Some(Ordering::Equal)),
                    _ => false,
                },
                RelOp::Ne => match val {
                    EvalScalar::Number(n) => cmp_excel_f64(*n, *target) != Some(Ordering::Equal),
                    EvalScalar::Text(s) => parse_excel_value_str(s)
                        .is_none_or(|n| cmp_excel_f64(n, *target) != Some(Ordering::Equal)),
                    _ => true,
                },
                RelOp::Lt => {
                    matches!(val, EvalScalar::Number(n) if cmp_excel_f64(*n, *target) == Some(Ordering::Less))
                }
                RelOp::Le => matches!(
                    val,
                    EvalScalar::Number(n)
                        if matches!(cmp_excel_f64(*n, *target), Some(Ordering::Less | Ordering::Equal))
                ),
                RelOp::Gt => {
                    matches!(val, EvalScalar::Number(n) if cmp_excel_f64(*n, *target) == Some(Ordering::Greater))
                }
                RelOp::Ge => matches!(
                    val,
                    EvalScalar::Number(n)
                        if matches!(cmp_excel_f64(*n, *target), Some(Ordering::Greater | Ordering::Equal))
                ),
            },
            Self::BoolRel { target, not_equal } => match val {
                EvalScalar::Bool(b) => {
                    if *not_equal {
                        *b != *target
                    } else {
                        *b == *target
                    }
                }
                _ => *not_equal,
            },
            Self::ErrorRel { target, not_equal } => match val {
                EvalScalar::Error(e) => {
                    if *not_equal {
                        *e != *target
                    } else {
                        *e == *target
                    }
                }
                _ => *not_equal,
            },
            Self::TextWild { tokens, not_equal } => match val {
                EvalScalar::Text(s) => {
                    let m = wildcard_matches_full(tokens, s);
                    if *not_equal { !m } else { m }
                }
                EvalScalar::Empty => *not_equal,
                _ => *not_equal,
            },
            Self::TextRel(op, lower_target) => {
                let EvalScalar::Text(s) = val else {
                    return false;
                };
                let ord = s.to_lowercase().cmp(lower_target);
                match op {
                    RelOp::Lt => ord == Ordering::Less,
                    RelOp::Le => matches!(ord, Ordering::Less | Ordering::Equal),
                    RelOp::Gt => ord == Ordering::Greater,
                    RelOp::Ge => matches!(ord, Ordering::Greater | Ordering::Equal),
                    RelOp::Eq => ord == Ordering::Equal,
                    RelOp::Ne => ord != Ordering::Equal,
                }
            }
        }
    }
}
