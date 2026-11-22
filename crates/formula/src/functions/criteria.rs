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
use super::wildcard::{WildToken, compile_wildcard, wildcard_matches_full};
use super::{RefCellVal, visit_reference_cells};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    CallerContext, EvalScalar, EvalValue, ResolvedRef, ScalarOrArray, coerce_for_scalar_op,
    read_sheet_cell_effective,
};

/// Evaluates `COUNTBLANK(range)`.
pub fn eval_countblank(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(arg0) = args.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    match arg0 {
        EvalValue::Reference(r) => match count_blanks_in_ref(r, workbook, spills) {
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
        EvalValue::Scalar(_) | EvalValue::Missing => {
            EvalValue::Scalar(EvalScalar::Error(CalcError::Value))
        }
    }
}

fn count_blanks_in_ref(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
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
    if spills.has_spills_on_sheet(target_sheet.id()) {
        visit_shadows_in_ref(r, workbook, spills, |s| {
            if !matches!(s, EvalScalar::Empty) && !matches!(s, EvalScalar::Text(t) if t.is_empty())
            {
                empty_blanks = empty_blanks.saturating_sub(1);
            }
        });
    }
    Ok(empty_blanks.saturating_add(text_blanks))
}

/// Evaluates `COUNTIF(range, criteria)`.
pub fn eval_countif(
    mut args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
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

    let crit_sa = match coerce_for_scalar_op(crit_val, workbook, spills, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match crit_sa {
        ScalarOrArray::Scalar(c) => {
            EvalValue::Scalar(countif_one(&range_val, &c, workbook, spills))
        }
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|c| countif_one(&range_val, c, workbook, spills))
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
    spills: &SpillRegistry,
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
            if res.is_ok() && spills.has_spills_on_sheet(sheet.id()) {
                visit_shadows_in_ref(r, workbook, spills, |s| {
                    if !matches!(s, EvalScalar::Empty) {
                        if matches_empty {
                            empty_matches = empty_matches.saturating_sub(1);
                        }
                        if pred.matches(s) {
                            non_empty_matches = non_empty_matches.saturating_add(1);
                        }
                    }
                });
            }
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
        EvalValue::Scalar(_) | EvalValue::Missing => EvalScalar::Error(CalcError::Value),
    }
}

/// Evaluates `SUMIF(range, criteria, [sum_range])` (`is_average == false`) or
/// `AVERAGEIF(range, criteria, [average_range])` (`is_average == true`).
pub fn eval_sumif_or_averageif(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
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

    let crit_sa = match coerce_for_scalar_op(crit_val, workbook, spills, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    match crit_sa {
        ScalarOrArray::Scalar(c) => EvalValue::Scalar(sumif_one(
            &range_val,
            &c,
            val_range.as_ref(),
            workbook,
            spills,
            is_average,
        )),
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|c| {
                    sumif_one(
                        &range_val,
                        c,
                        val_range.as_ref(),
                        workbook,
                        spills,
                        is_average,
                    )
                })
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
    spills: &SpillRegistry,
    is_average: bool,
) -> EvalScalar {
    let pred = CompiledCriterion::compile(crit_scalar);

    // Per S3b Excel 16.0.20711: `SUMIF` does NOT apply cancellation tail-zeroing!
    let mut sum = 0.0_f64;
    let mut count: u64 = 0;

    match (range_val, val_range_opt) {
        (EvalValue::Reference(crit_ref), None) => {
            let res = visit_reference_cells(crit_ref, workbook, spills, |sheet, cell| {
                let cell_s = match cell {
                    RefCellVal::Word(bits) => {
                        EvalScalar::from_sheet_value(sheet, Value::unpack(bits))
                    }
                    RefCellVal::Shadow(s) => s.clone(),
                };
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
            if let Err(e) = sumif_paired_refs(
                crit_ref, sum_ref, &pred, workbook, spills, &mut sum, &mut count,
            ) {
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
    spills: &SpillRegistry,
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
            let crit_val =
                read_sheet_cell_effective(crit_sheet, spills, RowId(crit_r_raw), ColId(crit_c_raw));
            if pred.matches(&crit_val) {
                let Some(sr) = sum_r_opt else {
                    continue;
                };
                let Some(&sc) = sum_col_ids.get(c_idx) else {
                    continue;
                };
                match read_sheet_cell_effective(sum_sheet, spills, RowId(sr), ColId(sc)) {
                    EvalScalar::Error(e) => return Err(e),
                    EvalScalar::Number(n) => {
                        *sum += n;
                        if !sum.is_finite() {
                            return Err(CalcError::Num);
                        }
                        *count = count.saturating_add(1);
                    }
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => {}
                }
            }
        }
    }
    Ok(())
}

fn visit_shadows_in_ref(
    r: &ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    mut emit: impl FnMut(&EvalScalar),
) {
    let Ok((sheet_id, r0, rows, c0, cols)) = ref_live_origin_and_size(r, workbook) else {
        return;
    };
    if rows == 0 || cols == 0 || !spills.has_spills_on_sheet(sheet_id) {
        return;
    }
    let Some(sheet) = workbook.sheet(sheet_id) else {
        return;
    };
    let r2_ord = r0.saturating_add(rows.saturating_sub(1));
    sheet.col_axis().for_each_live_positions(c0, cols, |c_run| {
        for cid in c_run.iter() {
            spills.for_each_shadow_in_col_span(sheet, ColId(cid), r0, r2_ord, |_, scalar| {
                emit(scalar);
            });
        }
    });
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

// ---------------------------------------------------------------------------
// Multi-criteria aggregation: COUNTIFS / SUMIFS / AVERAGEIFS / MAXIFS / MINIFS
// ---------------------------------------------------------------------------

/// Multi-criteria aggregation kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MultiIfsKind {
    /// `COUNTIFS`
    Count,
    /// `SUMIFS`
    Sum,
    /// `AVERAGEIFS`
    Average,
    /// `MAXIFS`
    Max,
    /// `MINIFS`
    Min,
}

enum IfsSource<'a> {
    Ref {
        sheet: &'a Sheet,
        r0: u32,
        row_ids: Vec<u64>,
        col_ids: Vec<u64>,
    },
    Arr(&'a crate::value_eval::Array2D),
}

impl<'a> IfsSource<'a> {
    fn from_arg(
        arg: &'a EvalValue,
        workbook: &'a Workbook<BoundFormula>,
    ) -> Result<(Self, u32, u32), CalcError> {
        match arg {
            EvalValue::Reference(r) => {
                let (sid, r0, rows, c0, cols) = ref_live_origin_and_size(r, workbook)?;
                let sheet = workbook.sheet(sid).ok_or(CalcError::Ref)?;
                let mut col_ids = Vec::with_capacity(cols as usize);
                sheet
                    .col_axis()
                    .for_each_live_positions(c0, cols, |run| col_ids.extend(run.iter()));
                let mut row_ids = Vec::new();
                // Defer materializing 1M rows for full columns unless needed.
                if rows <= 65_536 {
                    row_ids.reserve(rows as usize);
                    sheet
                        .row_axis()
                        .for_each_live_positions(r0, rows, |run| row_ids.extend(run.iter()));
                }
                Ok((
                    Self::Ref {
                        sheet,
                        r0,
                        row_ids,
                        col_ids,
                    },
                    rows,
                    cols,
                ))
            }
            EvalValue::Array(arr) => Ok((Self::Arr(arr), arr.rows, arr.cols)),
            EvalValue::Scalar(_) | EvalValue::Missing => Err(CalcError::Value),
        }
    }

    fn read(&self, r: u32, c: u32, spills: &SpillRegistry) -> EvalScalar {
        match self {
            Self::Ref {
                sheet,
                r0,
                row_ids,
                col_ids,
                ..
            } => {
                let rid_opt = if !row_ids.is_empty() {
                    row_ids.get(r as usize).copied()
                } else {
                    sheet.row_axis().pos_to_id(r0.saturating_add(r))
                };
                let Some(rid) = rid_opt else {
                    return EvalScalar::Error(CalcError::Ref);
                };
                let Some(&cid) = col_ids.get(c as usize) else {
                    return EvalScalar::Error(CalcError::Ref);
                };
                read_sheet_cell_effective(sheet, spills, RowId(rid), ColId(cid))
            }
            Self::Arr(arr) => arr.get_at(r, c),
        }
    }
}

/// Evaluates `COUNTIFS`, `SUMIFS`, `AVERAGEIFS`, `MAXIFS`, or `MINIFS`.
pub fn eval_multi_ifs(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    kind: MultiIfsKind,
) -> EvalValue {
    let min_args = if kind == MultiIfsKind::Count { 2 } else { 3 };
    let pair_rem = if kind == MultiIfsKind::Count { 0 } else { 1 };
    if args.len() < min_args || args.len() % 2 != pair_rem {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let target_arg = if kind == MultiIfsKind::Count {
        None
    } else {
        it.next()
    };

    let mut pairs = Vec::new();
    while let (Some(r_arg), Some(c_arg)) = (it.next(), it.next()) {
        let crit_sa = match coerce_for_scalar_op(c_arg, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let ScalarOrArray::Scalar(crit_sc) = crit_sa else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        let pred = CompiledCriterion::compile(&crit_sc);
        pairs.push((r_arg, pred));
    }

    let target_src = match target_arg.as_ref() {
        None => None,
        Some(a) => match IfsSource::from_arg(a, workbook) {
            Ok(src) => Some(src),
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
    };
    let mut crit_srcs = Vec::with_capacity(pairs.len());
    for (r_arg, pred) in &pairs {
        match IfsSource::from_arg(r_arg, workbook) {
            Ok((src, rr, cc)) => crit_srcs.push((src, rr, cc, pred)),
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }

    let (rows, cols) = if let Some((_, tr, tc)) = &target_src {
        (*tr, *tc)
    } else if let Some((_, cr, cc, _)) = crit_srcs.first() {
        (*cr, *cc)
    } else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };

    for (_, cr, cc, _) in &crit_srcs {
        if *cr != rows || *cc != cols {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
    }

    // Fast skip tail rows for full-column references when any criterion rejects blank.
    let any_rejects_empty = crit_srcs
        .iter()
        .any(|(_, _, _, p)| !p.matches(&EvalScalar::Empty));
    let mut scan_rows = rows;
    if rows > 65_536 && any_rejects_empty {
        let mut max_pop_row: u32 = 0;
        let check_src = |src: &IfsSource<'_>, bound: &mut u32| {
            if let IfsSource::Ref { sheet, col_ids, .. } = src {
                let base_blocks_rows = sheet.row_slots().base_len();
                for &cid in col_ids {
                    if let Some(cs) = sheet.col_slots().get(cid)
                        && let Some(col) = sheet.raw_columns().get(cs as usize)
                    {
                        let populated_slots = u32::try_from(col.blocks().len())
                            .unwrap_or(u32::MAX)
                            .saturating_mul(BLOCK_ROWS)
                            .min(base_blocks_rows);
                        if populated_slots > *bound {
                            *bound = populated_slots;
                        }
                    }
                    if spills.has_shadows_in_col(sheet.id(), ColId(cid)) {
                        *bound = rows;
                    }
                }
                if sheet.row_slots().base_len() < sheet.row_axis().live_len() {
                    *bound = rows;
                }
            }
        };
        if let Some((ref t_s, _, _)) = target_src {
            check_src(t_s, &mut max_pop_row);
        }
        for (c_s, _, _, _) in &crit_srcs {
            check_src(c_s, &mut max_pop_row);
        }
        scan_rows = rows.min(max_pop_row);
    }

    let mut sum = 0.0_f64;
    let mut count: u64 = 0;
    let mut best: Option<f64> = None;

    for r in 0..scan_rows {
        for c in 0..cols {
            let matched = crit_srcs
                .iter()
                .all(|(src, _, _, pred)| pred.matches(&src.read(r, c, spills)));
            if !matched {
                continue;
            }
            if kind == MultiIfsKind::Count {
                count = count.saturating_add(1);
                continue;
            }
            let Some((ref t_src, _, _)) = target_src else {
                continue;
            };
            match t_src.read(r, c, spills) {
                EvalScalar::Error(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
                EvalScalar::Number(n) => {
                    count = count.saturating_add(1);
                    match kind {
                        MultiIfsKind::Sum | MultiIfsKind::Average => {
                            sum += n;
                            if !sum.is_finite() {
                                return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
                            }
                        }
                        MultiIfsKind::Max => {
                            best = Some(best.map_or(n, |cur| cur.max(n)));
                        }
                        MultiIfsKind::Min => {
                            best = Some(best.map_or(n, |cur| cur.min(n)));
                        }
                        MultiIfsKind::Count => {}
                    }
                }
                EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => {}
            }
        }
    }

    let scalar = match kind {
        MultiIfsKind::Count =>
        {
            #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
            EvalScalar::Number(count as f64)
        }
        MultiIfsKind::Sum => EvalScalar::number(sum),
        MultiIfsKind::Average => {
            if count == 0 {
                EvalScalar::Error(CalcError::Div0)
            } else {
                #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
                EvalScalar::number(sum / (count as f64))
            }
        }
        MultiIfsKind::Max | MultiIfsKind::Min => EvalScalar::number(best.unwrap_or(0.0)),
    };
    EvalValue::Scalar(scalar)
}

// ---------------------------------------------------------------- SUMPRODUCT

/// Evaluates `SUMPRODUCT(array1, [array2], ...)`.
/// Per `17-m3a.md` & D-027, does not apply outermost Add/Sub tail-zeroing.
pub fn eval_sumproduct(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut arrays = Vec::with_capacity(args.len());
    for arg in args {
        match super::dynamic_array::value_to_array2d(arg, workbook, spills) {
            Ok(a) => arrays.push(a),
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
    let Some(first) = arrays.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let (rows, cols) = (first.rows, first.cols);
    for a in &arrays {
        if a.rows != rows || a.cols != cols {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
    }
    let total_len = first.values.len();
    let mut sum = 0.0_f64;
    for i in 0..total_len {
        let mut prod = 1.0_f64;
        for a in &arrays {
            let Some(v) = a.values.get(i) else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            let factor = match v {
                EvalScalar::Error(e) => return EvalValue::Scalar(EvalScalar::Error(*e)),
                EvalScalar::Number(n) => *n,
                EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => 0.0,
            };
            prod *= factor;
            if !prod.is_finite() {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
            }
        }
        sum += prod;
        if !sum.is_finite() {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        }
    }
    EvalValue::Scalar(EvalScalar::number(sum))
}

// ----------------------------------------------------- SUBTOTAL & AGGREGATE

fn contains_nested_subtotal_or_aggregate(
    expr: &crate::binder::BoundExpr,
    include_aggregate: bool,
) -> bool {
    use crate::binder::BoundExpr;
    match expr {
        BoundExpr::Call { name, args } => {
            if name == "SUBTOTAL" || (include_aggregate && name == "AGGREGATE") {
                return true;
            }
            args.iter()
                .any(|a| contains_nested_subtotal_or_aggregate(a, include_aggregate))
        }
        BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. }
        | BoundExpr::SpillRef(inner) => {
            contains_nested_subtotal_or_aggregate(inner, include_aggregate)
        }
        BoundExpr::Binary { left, right, .. } => {
            contains_nested_subtotal_or_aggregate(left, include_aggregate)
                || contains_nested_subtotal_or_aggregate(right, include_aggregate)
        }
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Reference(_)
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
    }
}

#[derive(Clone, Copy)]
struct SubtotalFlags {
    ignore_hidden: bool,
    ignore_nested: bool,
    include_aggregate_nested: bool,
    ignore_errors: bool,
    collect_counta_only: bool,
}

fn collect_subtotal_cells(
    refs: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    flags: SubtotalFlags,
    is_row_hidden: Option<&dyn Fn(reticula_core::id::SheetId, RowId) -> bool>,
) -> Result<(Vec<f64>, u64), CalcError> {
    let mut nums = Vec::new();
    let mut counta: u64 = 0;

    fn visit_sub_scalar(
        s: &EvalScalar,
        ignore_errors: bool,
        nums: &mut Vec<f64>,
        counta: &mut u64,
    ) -> Result<(), CalcError> {
        match s {
            EvalScalar::Error(e) => {
                if !ignore_errors {
                    return Err(*e);
                }
            }
            EvalScalar::Number(n) => {
                *counta = counta.saturating_add(1);
                nums.push(*n);
            }
            EvalScalar::Bool(_) | EvalScalar::Text(_) => {
                *counta = counta.saturating_add(1);
            }
            EvalScalar::Empty => {}
        }
        Ok(())
    }

    for arg in refs {
        match arg {
            EvalValue::Missing => {}
            EvalValue::Scalar(s) => {
                if flags.collect_counta_only {
                    if !matches!(s, EvalScalar::Empty) {
                        counta = counta.saturating_add(1);
                    }
                } else if matches!(s, EvalScalar::Error(_)) && !flags.ignore_errors {
                    if let EvalScalar::Error(e) = s {
                        return Err(*e);
                    }
                } else if !matches!(s, EvalScalar::Empty | EvalScalar::Error(_)) {
                    let n = s.to_number()?;
                    counta = counta.saturating_add(1);
                    nums.push(n);
                }
            }
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    visit_sub_scalar(item, flags.ignore_errors, &mut nums, &mut counta)?;
                }
            }
            EvalValue::Reference(r) => {
                let (sid, r0, rows, c0, cols) = ref_live_origin_and_size(r, workbook)?;
                let sheet = workbook.sheet(sid).ok_or(CalcError::Ref)?;
                let mut col_ids = Vec::with_capacity(cols as usize);
                sheet
                    .col_axis()
                    .for_each_live_positions(c0, cols, |run| col_ids.extend(run.iter()));
                let mut row_ids = Vec::with_capacity((rows as usize).min(4096));
                sheet
                    .row_axis()
                    .for_each_live_positions(r0, rows, |run| row_ids.extend(run.iter()));
                for rid_raw in row_ids {
                    let rid = RowId(rid_raw);
                    if flags.ignore_hidden && is_row_hidden.is_some_and(|f| f(sid, rid)) {
                        continue;
                    }
                    for &cid_raw in &col_ids {
                        let cid = ColId(cid_raw);
                        if flags.ignore_nested
                            && let Some(formula) = workbook.formula(sid, rid, cid)
                            && contains_nested_subtotal_or_aggregate(
                                &formula.root,
                                flags.include_aggregate_nested,
                            )
                        {
                            continue;
                        }
                        let sc = read_sheet_cell_effective(sheet, spills, rid, cid);
                        visit_sub_scalar(&sc, flags.ignore_errors, &mut nums, &mut counta)?;
                    }
                }
            }
        }
    }
    Ok((nums, counta))
}

fn eval_basic_sub_agg_code(
    code: isize,
    nums: &[f64],
    counta: u64,
    k_opt: Option<f64>,
) -> EvalScalar {
    match code {
        1 => {
            if nums.is_empty() {
                return EvalScalar::Error(CalcError::Div0);
            }
            let mut acc = super::SumAcc::new();
            for &n in nums {
                if let Err(e) = acc.push(n) {
                    return EvalScalar::Error(e);
                }
            }
            #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
            let denom = acc.count as f64;
            EvalScalar::number(acc.finish() / denom)
        }
        2 =>
        {
            #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
            EvalScalar::Number(nums.len() as f64)
        }
        3 =>
        {
            #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
            EvalScalar::Number(counta as f64)
        }
        4 => {
            let best = nums.iter().copied().reduce(f64::max).unwrap_or(0.0);
            EvalScalar::number(best)
        }
        5 => {
            let best = nums.iter().copied().reduce(f64::min).unwrap_or(0.0);
            EvalScalar::number(best)
        }
        6 => {
            if nums.is_empty() {
                return EvalScalar::Number(0.0);
            }
            let mut prod = 1.0_f64;
            for &n in nums {
                prod *= n;
                if !prod.is_finite() {
                    return EvalScalar::Error(CalcError::Num);
                }
            }
            EvalScalar::number(prod)
        }
        9 => {
            let mut acc = super::SumAcc::new();
            for &n in nums {
                if let Err(e) = acc.push(n) {
                    return EvalScalar::Error(e);
                }
            }
            EvalScalar::number(acc.finish())
        }
        7 | 8 | 10..=19 => super::stats::aggregate_stat_code(code, nums, k_opt),
        _ => EvalScalar::Error(CalcError::Value),
    }
}

/// Evaluates `SUBTOTAL(function_num, ref1, [ref2], ...)`.
pub fn eval_subtotal(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_row_hidden: Option<&dyn Fn(reticula_core::id::SheetId, RowId) -> bool>,
) -> EvalValue {
    if args.len() < 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(fn_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let fn_sa = match coerce_for_scalar_op(fn_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ScalarOrArray::Scalar(fn_sc) = fn_sa else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let Ok(fn_num) = fn_sc.to_number().and_then(super::trunc_to_isize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let code = if (1..=11).contains(&fn_num) {
        fn_num
    } else if (101..=111).contains(&fn_num) {
        fn_num.saturating_sub(100)
    } else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };

    let refs: Vec<EvalValue> = it.collect();
    let (nums, counta) = match collect_subtotal_cells(
        &refs,
        workbook,
        spills,
        SubtotalFlags {
            ignore_hidden: true,
            ignore_nested: true,
            include_aggregate_nested: false,
            ignore_errors: false,
            collect_counta_only: code == 3,
        },
        is_row_hidden,
    ) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    EvalValue::Scalar(eval_basic_sub_agg_code(code, &nums, counta, None))
}

/// Evaluates `AGGREGATE(function_num, options, ref1, [ref2_or_k], ...)`.
pub fn eval_aggregate(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_row_hidden: Option<&dyn Fn(reticula_core::id::SheetId, RowId) -> bool>,
) -> EvalValue {
    if args.len() < 3 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(fn_arg), Some(opt_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let fn_sa = match coerce_for_scalar_op(fn_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let opt_sa = match coerce_for_scalar_op(opt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let (ScalarOrArray::Scalar(fn_sc), ScalarOrArray::Scalar(opt_sc)) = (fn_sa, opt_sa) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let Ok(fn_num) = fn_sc.to_number().and_then(super::trunc_to_isize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let Ok(options) = opt_sc.to_number().and_then(super::trunc_to_isize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    if !(1..=19).contains(&fn_num) || !(0..=7).contains(&options) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }

    let ignore_nested = options <= 3;
    let ignore_hidden = matches!(options, 1 | 3 | 5 | 7);
    let ignore_errors = matches!(options, 2 | 3 | 6 | 7);

    let mut rest: Vec<EvalValue> = it.collect();
    let k_opt = if fn_num >= 14 {
        if rest.len() != 2 {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
        let Some(k_arg) = rest.pop() else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        let k_sa = match coerce_for_scalar_op(k_arg, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let ScalarOrArray::Scalar(k_sc) = k_sa else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        match k_sc.to_number() {
            Ok(v) => Some(v),
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        None
    };

    let (nums, counta) = match collect_subtotal_cells(
        &rest,
        workbook,
        spills,
        SubtotalFlags {
            ignore_hidden,
            ignore_nested,
            include_aggregate_nested: true,
            ignore_errors,
            collect_counta_only: fn_num == 3,
        },
        is_row_hidden,
    ) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    EvalValue::Scalar(eval_basic_sub_agg_code(fn_num, &nums, counta, k_opt))
}
