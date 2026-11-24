//! Layer-1 lookup & reference functions (`ROWS`, `COLUMNS`, `INDEX`, `MATCH`,
//! `VLOOKUP`, `HLOOKUP`, and `CHOOSE` combiner; `14-m1b.md` 5.3).
//!
//! Exact lookup (`MATCH(..., 0)`, `VLOOKUP(..., FALSE)`, `HLOOKUP(..., FALSE)`)
//! compares numbers **bitwise** (`to_bits()`, after canonicalising `-0.0` to
//! `+0.0`) per S3b Excel 16.0.20711 verification (310/310 cases), and matches
//! strings case-insensitively with `*` / `?` / `~` wildcards.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::axis::{MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN};
use reticula_core::decimal15::cmp_excel_f64;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block};
use reticula_core::value::{CalcError, Value};

use super::criteria::ref_live_origin_and_size;
use super::wildcard::{WildToken, compile_wildcard, wildcard_matches_full};
use super::{empty_to_zero, get_sa, shape_of, trunc_to_isize};
use crate::address::{write_col_letters, write_sheet_prefix};
use crate::binder::{BoundExpr, BoundFormula, bind_expr};
use crate::lookup_index::{LookupIndexCache, LookupProbeResult, has_wildcard_tokens};
use crate::parser::parse_formula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ResolvedRef, ScalarOrArray,
    coerce_for_scalar_op, read_sheet_cell_effective, resolve_bound_ref,
};

/// Evaluates `ROWS(array)` (`is_rows == true`) or `COLUMNS(array)` (`is_rows == false`).
pub fn eval_rows_or_columns(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    is_rows: bool,
) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(arg0) = args.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    match arg0 {
        EvalValue::Reference(r) => match ref_live_origin_and_size(r, workbook) {
            Ok((_, _, rows, _, cols)) => {
                let n = if is_rows { rows } else { cols };
                EvalValue::Scalar(EvalScalar::Number(f64::from(n)))
            }
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        },
        EvalValue::Array(arr) => {
            let n = if is_rows { arr.rows } else { arr.cols };
            EvalValue::Scalar(EvalScalar::Number(f64::from(n)))
        }
        EvalValue::Scalar(EvalScalar::Error(e)) => EvalValue::Scalar(EvalScalar::Error(*e)),
        EvalValue::Scalar(_) => EvalValue::Scalar(EvalScalar::Number(1.0)),
        EvalValue::Missing => EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
    }
}

/// Evaluates `CHOOSE(index_num, value1, ...)` from pre-evaluated `index` and
/// lazily-evaluated `chosen_branches` (`choices[i]` is `Some` iff index `i + 1`
/// was actually requested by `index`).
pub fn combine_choose(
    index: ScalarOrArray,
    choices: Vec<Option<EvalValue>>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if choices.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    if let ScalarOrArray::Scalar(idx_s) = index {
        let idx = match idx_s.to_number().and_then(trunc_to_isize) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        if idx < 1 {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
        #[expect(clippy::cast_sign_loss, reason = "`idx >= 1`")]
        let zero_idx = (idx as usize).saturating_sub(1);
        return match choices.into_iter().nth(zero_idx).flatten() {
            Some(v) => v,
            None => EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        };
    }

    let ScalarOrArray::Array(idx_arr) = index else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let mut coerced_choices: Vec<Option<ScalarOrArray>> = Vec::with_capacity(choices.len());
    let mut out_rows = idx_arr.rows;
    let mut out_cols = idx_arr.cols;
    for opt in choices {
        match opt {
            None => coerced_choices.push(None),
            Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(sa) => {
                    let (r, c) = shape_of(&sa);
                    out_rows = out_rows.max(r);
                    out_cols = out_cols.max(c);
                    coerced_choices.push(Some(sa));
                }
                Err(e) => {
                    coerced_choices.push(Some(ScalarOrArray::Scalar(EvalScalar::Error(e))));
                }
            },
        }
    }
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut out = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            let idx_s = idx_arr.get_broadcast(r, c);
            let cell = match idx_s.to_number().and_then(trunc_to_isize) {
                Err(e) => EvalScalar::Error(e),
                Ok(n) if n < 1 => EvalScalar::Error(CalcError::Value),
                Ok(n) => {
                    #[expect(clippy::cast_sign_loss, reason = "`n >= 1`")]
                    let zi = (n as usize).saturating_sub(1);
                    match coerced_choices.get(zi).and_then(Option::as_ref) {
                        Some(sa) => empty_to_zero(get_sa(sa, r, c)),
                        None => EvalScalar::Error(CalcError::Value),
                    }
                }
            };
            out.push(cell);
        }
    }
    match Array2D::new(out_rows, out_cols, out) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `INDEX(array, row_num, [column_num])`.
pub fn eval_index(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let two_arg = args.len() == 2;
    let mut it = args.into_iter();
    let (Some(source), Some(row_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let col_arg = it
        .next()
        .unwrap_or(EvalValue::Scalar(EvalScalar::Number(0.0)));

    let row_sa = match coerce_for_scalar_op(row_arg, workbook, spills, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let col_sa = match coerce_for_scalar_op(col_arg, workbook, spills, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let (ScalarOrArray::Scalar(r_sc), ScalarOrArray::Scalar(c_sc)) = (row_sa, col_sa) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let r_num = match r_sc.to_number().and_then(trunc_to_isize) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let c_num = match c_sc.to_number().and_then(trunc_to_isize) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if r_num < 0 || c_num < 0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let (Ok(r_u32), Ok(c_u32)) = (u32::try_from(r_num), u32::try_from(c_num)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };

    match source {
        EvalValue::Reference(r) => index_into_ref(&r, r_u32, c_u32, two_arg, workbook),
        EvalValue::Array(arr) => index_into_array(&arr, r_u32, c_u32, two_arg),
        EvalValue::Scalar(EvalScalar::Error(e)) => EvalValue::Scalar(EvalScalar::Error(e)),
        EvalValue::Missing => EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        EvalValue::Scalar(s) => {
            if r_u32 <= 1 && c_u32 <= 1 {
                EvalValue::Scalar(empty_to_zero(s))
            } else {
                EvalValue::Scalar(EvalScalar::Error(CalcError::Ref))
            }
        }
    }
}

fn effective_index_coords(num_rows: u32, r_u32: u32, c_u32: u32, two_arg: bool) -> (u32, u32) {
    if two_arg {
        if num_rows == 1 {
            (0, r_u32)
        } else {
            (r_u32, 0)
        }
    } else {
        (r_u32, c_u32)
    }
}

fn index_into_ref(
    r: &ResolvedRef,
    r_u32: u32,
    c_u32: u32,
    two_arg: bool,
    workbook: &Workbook<BoundFormula>,
) -> EvalValue {
    let (sheet_id, r0, num_rows, c0, num_cols) = match ref_live_origin_and_size(r, workbook) {
        Ok(info) => info,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let (r_eff, c_eff) = effective_index_coords(num_rows, r_u32, c_u32, two_arg);
    if r_eff > num_rows || c_eff > num_cols {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    }
    let Some(sheet) = workbook.sheet(sheet_id) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };

    let (r_start_pos, r_end_pos) = if r_eff == 0 {
        (r0, r0.saturating_add(num_rows.saturating_sub(1)))
    } else {
        let p = r0.saturating_add(r_eff.saturating_sub(1));
        (p, p)
    };
    let (c_start_pos, c_end_pos) = if c_eff == 0 {
        (c0, c0.saturating_add(num_cols.saturating_sub(1)))
    } else {
        let p = c0.saturating_add(c_eff.saturating_sub(1));
        (p, p)
    };

    let (Some(fr_id), Some(lr_id), Some(fc_id), Some(lc_id)) = (
        sheet.row_axis().pos_to_id(r_start_pos),
        sheet.row_axis().pos_to_id(r_end_pos),
        sheet.col_axis().pos_to_id(c_start_pos),
        sheet.col_axis().pos_to_id(c_end_pos),
    ) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };

    if r_start_pos == r_end_pos && c_start_pos == c_end_pos {
        EvalValue::Reference(ResolvedRef::Cell {
            sheet: sheet_id,
            row: RowId(fr_id),
            col: ColId(fc_id),
        })
    } else {
        EvalValue::Reference(ResolvedRef::Area {
            sheet: sheet_id,
            first_row: RowId(fr_id),
            last_row: RowId(lr_id),
            first_col: ColId(fc_id),
            last_col: ColId(lc_id),
        })
    }
}

fn index_into_array(arr: &Array2D, r_u32: u32, c_u32: u32, two_arg: bool) -> EvalValue {
    let (r_eff, c_eff) = effective_index_coords(arr.rows, r_u32, c_u32, two_arg);
    if r_eff > arr.rows || c_eff > arr.cols {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    }
    let (r_start, r_end) = if r_eff == 0 {
        (1, arr.rows)
    } else {
        (r_eff, r_eff)
    };
    let (c_start, c_end) = if c_eff == 0 {
        (1, arr.cols)
    } else {
        (c_eff, c_eff)
    };
    if r_start == r_end && c_start == c_end {
        let idx = ((r_start.saturating_sub(1)) as usize)
            .saturating_mul(arr.cols as usize)
            .saturating_add((c_start.saturating_sub(1)) as usize);
        let val = arr
            .values
            .get(idx)
            .cloned()
            .map_or(EvalScalar::Error(CalcError::Ref), empty_to_zero);
        return EvalValue::Scalar(val);
    }
    let out_r = r_end.saturating_sub(r_start).saturating_add(1);
    let out_c = c_end.saturating_sub(c_start).saturating_add(1);
    let mut vals = Vec::with_capacity((out_r as usize).saturating_mul(out_c as usize));
    for r in r_start..=r_end {
        for c in c_start..=c_end {
            let idx = ((r.saturating_sub(1)) as usize)
                .saturating_mul(arr.cols as usize)
                .saturating_add((c.saturating_sub(1)) as usize);
            vals.push(
                arr.values
                    .get(idx)
                    .cloned()
                    .map_or(EvalScalar::Error(CalcError::Ref), empty_to_zero),
            );
        }
    }
    match Array2D::new(out_r, out_c, vals) {
        Ok(sub) => EvalValue::Array(sub),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `MATCH(lookup_value, lookup_array, [match_type])`.
pub fn eval_match(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    lookup_cache: &mut LookupIndexCache,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(lookup_arg), Some(array_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let match_type: i32 = match it.next() {
        None => 1,
        Some(mt_arg) => {
            let sa = match coerce_for_scalar_op(mt_arg, workbook, spills, caller) {
                Ok(v) => v,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_number() {
                Ok(n) => {
                    let t = num::trunc(n);
                    if t > 0.0 {
                        1
                    } else if t < 0.0 {
                        -1
                    } else {
                        0
                    }
                }
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };

    let lookup_sa = match coerce_for_scalar_op(lookup_arg, workbook, spills, caller) {
        Ok(sa) => sa,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match lookup_sa {
        ScalarOrArray::Scalar(needle) => EvalValue::Scalar(match_one(
            &needle,
            &array_arg,
            match_type,
            workbook,
            spills,
            lookup_cache,
        )),
        ScalarOrArray::Array(arr) => {
            let mut values = Vec::with_capacity(arr.values.len());
            for needle in &arr.values {
                values.push(match_one(
                    needle,
                    &array_arg,
                    match_type,
                    workbook,
                    spills,
                    lookup_cache,
                ));
            }
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

/// Evaluates exact `MATCH(needle, reference, 0)` for scalar needles.
pub(crate) fn match_exact_on_ref(
    needle: &EvalScalar,
    reference: ResolvedRef,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    lookup_cache: &mut LookupIndexCache,
) -> EvalScalar {
    match_one(
        needle,
        &EvalValue::Reference(reference),
        0,
        workbook,
        spills,
        lookup_cache,
    )
}

/// Evaluates exact `VLOOKUP(needle, table_reference, col_index, FALSE)` for
/// scalar needles.
pub(crate) fn vlookup_exact_on_ref(
    needle: &EvalScalar,
    table: ResolvedRef,
    col_index: u32,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    lookup_cache: &mut LookupIndexCache,
) -> EvalScalar {
    if col_index < 1 {
        return EvalScalar::Error(CalcError::Value);
    }
    vh_lookup_one(
        needle,
        &EvalValue::Reference(table),
        col_index,
        0,
        true,
        workbook,
        spills,
        lookup_cache,
    )
}

fn match_one(
    needle: &EvalScalar,
    array_arg: &EvalValue,
    match_type: i32,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    lookup_cache: &mut LookupIndexCache,
) -> EvalScalar {
    if let EvalScalar::Error(e) = needle {
        return EvalScalar::Error(*e);
    }
    // Empty lookup_value is treated as `0.0` in Excel lookup functions!
    let norm_needle = match needle {
        EvalScalar::Empty => EvalScalar::Number(0.0),
        other => other.clone(),
    };

    match array_arg {
        EvalValue::Reference(r) => {
            let (sheet_id, r0, rows, c0, cols) = match ref_live_origin_and_size(r, workbook) {
                Ok(info) => info,
                Err(e) => return EvalScalar::Error(e),
            };
            if rows > 1 && cols > 1 {
                return EvalScalar::Error(CalcError::Na);
            }
            let Some(sheet) = workbook.sheet(sheet_id) else {
                return EvalScalar::Error(CalcError::Ref);
            };
            let found = if cols == 1 {
                search_sheet_column(
                    sheet,
                    c0,
                    r0,
                    rows,
                    &norm_needle,
                    match_type,
                    spills,
                    lookup_cache,
                )
            } else {
                search_sheet_row(sheet, r0, c0, cols, &norm_needle, match_type, spills)
            };
            found.map_or(EvalScalar::Error(CalcError::Na), |idx0| {
                EvalScalar::Number(f64::from(idx0.saturating_add(1)))
            })
        }
        EvalValue::Array(arr) => {
            if arr.rows > 1 && arr.cols > 1 {
                return EvalScalar::Error(CalcError::Na);
            }
            search_scalar_slice(&arr.values, &norm_needle, match_type).map_or(
                EvalScalar::Error(CalcError::Na),
                |idx0| {
                    #[expect(clippy::cast_precision_loss, reason = "`idx0 <= 1M`")]
                    EvalScalar::Number(idx0.saturating_add(1) as f64)
                },
            )
        }
        EvalValue::Scalar(EvalScalar::Error(e)) => EvalScalar::Error(*e),
        EvalValue::Scalar(_) | EvalValue::Missing => EvalScalar::Error(CalcError::Na),
    }
}

/// Evaluates `VLOOKUP(lookup_value, table_array, col_index_num, [range_lookup])`
/// (`is_vertical == true`) or `HLOOKUP(..., row_index_num, [range_lookup])`
/// (`is_vertical == false`).
pub fn eval_vlookup_or_hlookup(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_vertical: bool,
    lookup_cache: &mut LookupIndexCache,
) -> EvalValue {
    if !(3..=4).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(lookup_arg), Some(table_arg), Some(idx_arg)) = (it.next(), it.next(), it.next())
    else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let approx_mode: bool = match it.next() {
        None => true,
        Some(rl_arg) => {
            let sa = match coerce_for_scalar_op(rl_arg, workbook, spills, caller) {
                Ok(v) => v,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_bool() {
                Ok(b) => b,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };

    let idx_sa = match coerce_for_scalar_op(idx_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ScalarOrArray::Scalar(idx_sc) = idx_sa else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let index_1based = match idx_sc.to_number().and_then(trunc_to_isize) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if index_1based < 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Ok(index_u32) = u32::try_from(index_1based) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };

    let lookup_sa = match coerce_for_scalar_op(lookup_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let match_type = i32::from(approx_mode);
    match lookup_sa {
        ScalarOrArray::Scalar(needle) => EvalValue::Scalar(vh_lookup_one(
            &needle,
            &table_arg,
            index_u32,
            match_type,
            is_vertical,
            workbook,
            spills,
            lookup_cache,
        )),
        ScalarOrArray::Array(arr) => {
            let mut values = Vec::with_capacity(arr.values.len());
            for needle in &arr.values {
                values.push(vh_lookup_one(
                    needle,
                    &table_arg,
                    index_u32,
                    match_type,
                    is_vertical,
                    workbook,
                    spills,
                    lookup_cache,
                ));
            }
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "central VLOOKUP/HLOOKUP evaluator"
)]
fn vh_lookup_one(
    needle: &EvalScalar,
    table_arg: &EvalValue,
    index_1based: u32,
    match_type: i32,
    is_vertical: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    lookup_cache: &mut LookupIndexCache,
) -> EvalScalar {
    if let EvalScalar::Error(e) = needle {
        return EvalScalar::Error(*e);
    }
    let norm_needle = match needle {
        EvalScalar::Empty => EvalScalar::Number(0.0),
        other => other.clone(),
    };

    match table_arg {
        EvalValue::Reference(r) => {
            let (sheet_id, r0, rows, c0, cols) = match ref_live_origin_and_size(r, workbook) {
                Ok(info) => info,
                Err(e) => return EvalScalar::Error(e),
            };
            if (is_vertical && index_1based > cols) || (!is_vertical && index_1based > rows) {
                return EvalScalar::Error(CalcError::Ref);
            }
            let Some(sheet) = workbook.sheet(sheet_id) else {
                return EvalScalar::Error(CalcError::Ref);
            };
            if is_vertical {
                let Some(row_offset) = search_sheet_column(
                    sheet,
                    c0,
                    r0,
                    rows,
                    &norm_needle,
                    match_type,
                    spills,
                    lookup_cache,
                ) else {
                    return EvalScalar::Error(CalcError::Na);
                };
                let target_r_pos = r0.saturating_add(row_offset);
                let target_c_pos = c0.saturating_add(index_1based.saturating_sub(1));
                let (Some(rid), Some(cid)) = (
                    sheet.row_axis().pos_to_id(target_r_pos),
                    sheet.col_axis().pos_to_id(target_c_pos),
                ) else {
                    return EvalScalar::Error(CalcError::Ref);
                };
                empty_to_zero(read_sheet_cell_effective(
                    sheet,
                    spills,
                    RowId(rid),
                    ColId(cid),
                ))
            } else {
                let Some(col_offset) =
                    search_sheet_row(sheet, r0, c0, cols, &norm_needle, match_type, spills)
                else {
                    return EvalScalar::Error(CalcError::Na);
                };
                let target_r_pos = r0.saturating_add(index_1based.saturating_sub(1));
                let target_c_pos = c0.saturating_add(col_offset);
                let (Some(rid), Some(cid)) = (
                    sheet.row_axis().pos_to_id(target_r_pos),
                    sheet.col_axis().pos_to_id(target_c_pos),
                ) else {
                    return EvalScalar::Error(CalcError::Ref);
                };
                empty_to_zero(read_sheet_cell_effective(
                    sheet,
                    spills,
                    RowId(rid),
                    ColId(cid),
                ))
            }
        }
        EvalValue::Array(arr) => {
            if (is_vertical && index_1based > arr.cols) || (!is_vertical && index_1based > arr.rows)
            {
                return EvalScalar::Error(CalcError::Ref);
            }
            if is_vertical {
                let col0: Vec<EvalScalar> = (0..arr.rows)
                    .filter_map(|r| {
                        arr.values
                            .get((r as usize).saturating_mul(arr.cols as usize))
                            .cloned()
                    })
                    .collect();
                let Some(row_off) = search_scalar_slice(&col0, &norm_needle, match_type) else {
                    return EvalScalar::Error(CalcError::Na);
                };
                let flat = row_off
                    .saturating_mul(arr.cols as usize)
                    .saturating_add((index_1based.saturating_sub(1)) as usize);
                arr.values
                    .get(flat)
                    .cloned()
                    .map_or(EvalScalar::Error(CalcError::Ref), empty_to_zero)
            } else {
                let Some(row0) = arr.values.get(..(arr.cols as usize)) else {
                    return EvalScalar::Error(CalcError::Ref);
                };
                let Some(col_off) = search_scalar_slice(row0, &norm_needle, match_type) else {
                    return EvalScalar::Error(CalcError::Na);
                };
                let flat = ((index_1based.saturating_sub(1)) as usize)
                    .saturating_mul(arr.cols as usize)
                    .saturating_add(col_off);
                arr.values
                    .get(flat)
                    .cloned()
                    .map_or(EvalScalar::Error(CalcError::Ref), empty_to_zero)
            }
        }
        EvalValue::Scalar(EvalScalar::Error(e)) => EvalScalar::Error(*e),
        EvalValue::Scalar(_) | EvalValue::Missing => EvalScalar::Error(CalcError::Na),
    }
}

// ------------------------------------------------ fast column / row searchers

enum ExactMatcher {
    /// S3b verified: exact numeric match compares IEEE 754 bit patterns directly!
    NumberBits(u64),
    Bool(bool),
    PlainTextLower(alloc::string::String),
    WildText(Vec<WildToken>),
}

impl ExactMatcher {
    fn from_needle(needle: &EvalScalar) -> Option<Self> {
        match needle {
            EvalScalar::Number(n) => {
                let canon = if *n == 0.0 { 0.0 } else { *n };
                Some(Self::NumberBits(canon.to_bits()))
            }
            EvalScalar::Bool(b) => Some(Self::Bool(*b)),
            EvalScalar::Text(s) => {
                let (tokens, has_special) = compile_wildcard(s);
                if has_special {
                    Some(Self::WildText(tokens))
                } else {
                    Some(Self::PlainTextLower(s.to_lowercase()))
                }
            }
            EvalScalar::Empty | EvalScalar::Error(_) => None,
        }
    }

    #[inline]
    fn matches_packed(&self, sheet: &Sheet, bits: u64) -> bool {
        if bits == Value::EMPTY_BITS {
            return false;
        }
        match (self, Value::unpack(bits)) {
            (Self::NumberBits(target_bits), Value::Number(n)) => {
                let canon = if n == 0.0 { 0.0 } else { n };
                canon.to_bits() == *target_bits
            }
            (Self::Bool(tb), Value::Bool(b)) => b == *tb,
            (Self::PlainTextLower(lower), Value::Text(id)) => {
                let cell_s = sheet.strings().get(id);
                if cell_s.is_ascii() && lower.is_ascii() {
                    cell_s.eq_ignore_ascii_case(lower)
                } else {
                    cell_s.to_lowercase() == *lower
                }
            }
            (Self::WildText(tokens), Value::Text(id)) => {
                wildcard_matches_full(tokens, sheet.strings().get(id))
            }
            _ => false,
        }
    }

    fn matches_scalar(&self, val: &EvalScalar) -> bool {
        match (self, val) {
            (Self::NumberBits(target_bits), EvalScalar::Number(n)) => {
                let canon = if *n == 0.0 { 0.0 } else { *n };
                canon.to_bits() == *target_bits
            }
            (Self::Bool(tb), EvalScalar::Bool(b)) => b == tb,
            (Self::PlainTextLower(lower), EvalScalar::Text(s)) => {
                if s.is_ascii() && lower.is_ascii() {
                    s.eq_ignore_ascii_case(lower)
                } else {
                    s.to_lowercase() == *lower
                }
            }
            (Self::WildText(tokens), EvalScalar::Text(s)) => wildcard_matches_full(tokens, s),
            _ => false,
        }
    }
}

/// Searches `row_count` live rows of column `col_pos` starting at `first_row_pos`
/// on `sheet`, returning the 0-based offset within `0..row_count`.
///
/// For `match_type == 0` (exact match, including **B6** `100k × VLOOKUP(..., FALSE)`),
/// scans contiguous `Dense` 64-bit word slices directly without allocating!
#[expect(
    clippy::too_many_arguments,
    reason = "vertical column search integrates cache, spill shadows, and match modes"
)]
fn search_sheet_column(
    sheet: &Sheet,
    col_pos: u32,
    first_row_pos: u32,
    row_count: u32,
    needle: &EvalScalar,
    match_type: i32,
    spills: &SpillRegistry,
    lookup_cache: &mut LookupIndexCache,
) -> Option<u32> {
    let col_id = sheet.col_axis().pos_to_id(col_pos)?;
    if match_type == 0 {
        if !spills.has_shadows_in_col(sheet.id(), ColId(col_id))
            && !matches!(needle, EvalScalar::Text(s) if has_wildcard_tokens(s))
            && let Some(idx) =
                lookup_cache.get_or_build(sheet, ColId(col_id), first_row_pos, row_count, 1)
            && let LookupProbeResult::Hit(hit) = idx.probe_scalar(sheet, needle, true)
        {
            return hit;
        }
        let matcher = ExactMatcher::from_needle(needle)?;
        if spills.has_shadows_in_col(sheet.id(), ColId(col_id)) {
            let mut offset_idx: u32 = 0;
            let mut found: Option<u32> = None;
            sheet
                .row_axis()
                .for_each_live_positions(first_row_pos, row_count, |run| {
                    if found.is_some() {
                        return;
                    }
                    for row_id in run.iter() {
                        let scalar =
                            read_sheet_cell_effective(sheet, spills, RowId(row_id), ColId(col_id));
                        if matcher.matches_scalar(&scalar) {
                            found = Some(offset_idx);
                            return;
                        }
                        offset_idx = offset_idx.saturating_add(1);
                    }
                });
            return found;
        }
        let col_slot = sheet.col_slots().get(col_id)?;
        let column = sheet.raw_columns().get(col_slot as usize)?;

        let mut row_runs: Vec<IdRun> = Vec::new();
        sheet
            .row_axis()
            .for_each_live_positions(first_row_pos, row_count, |r| row_runs.push(r));

        let base_bound = u64::from(sheet.row_slots().base_len());
        let blocks = column.blocks();
        let mut offset_idx: u32 = 0;

        for run in &row_runs {
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
                    let take_u32 = u32::try_from(take).unwrap_or(0);
                    match blocks.get(block_idx) {
                        None | Some(Block::Empty) => {}
                        Some(Block::Dense(words)) => {
                            if let Some(slice) = words.get(off..off.saturating_add(take)) {
                                for (k, &bits) in slice.iter().enumerate() {
                                    if matcher.matches_packed(sheet, bits) {
                                        return Some(
                                            offset_idx
                                                .saturating_add(u32::try_from(k).unwrap_or(0)),
                                        );
                                    }
                                }
                            }
                        }
                        Some(Block::Sparse(_)) => {
                            for k in 0..take_u32 {
                                let bits = column.get_packed(cur.saturating_add(k));
                                if matcher.matches_packed(sheet, bits) {
                                    return Some(offset_idx.saturating_add(k));
                                }
                            }
                        }
                    }
                    cur = cur.saturating_add(take_u32.max(1));
                    offset_idx = offset_idx.saturating_add(take_u32);
                }
            } else {
                for row_id in run.iter() {
                    if let Some(rs) = sheet.row_slots().get(row_id)
                        && matcher.matches_packed(sheet, column.get_packed(rs))
                    {
                        return Some(offset_idx);
                    }
                    offset_idx = offset_idx.saturating_add(1);
                }
            }
        }
        return None;
    }

    // Approximate match (`match_type == 1` ascending or `-1` descending):
    approx_search_len(row_count, needle, match_type, |idx| {
        let rid = sheet
            .row_axis()
            .pos_to_id(first_row_pos.saturating_add(idx))?;
        Some(read_sheet_cell_effective(
            sheet,
            spills,
            RowId(rid),
            ColId(col_id),
        ))
    })
}

fn search_sheet_row(
    sheet: &Sheet,
    row_pos: u32,
    first_col_pos: u32,
    col_count: u32,
    needle: &EvalScalar,
    match_type: i32,
    spills: &SpillRegistry,
) -> Option<u32> {
    let row_id = sheet.row_axis().pos_to_id(row_pos)?;
    if match_type == 0 {
        let matcher = ExactMatcher::from_needle(needle)?;
        let has_spills = spills.has_spills_on_sheet(sheet.id());
        let mut offset_idx: u32 = 0;
        let mut found: Option<u32> = None;
        sheet
            .col_axis()
            .for_each_live_positions(first_col_pos, col_count, |c_run| {
                if found.is_some() {
                    return;
                }
                for cid in c_run.iter() {
                    let matched = if has_spills {
                        let scalar =
                            read_sheet_cell_effective(sheet, spills, RowId(row_id), ColId(cid));
                        matcher.matches_scalar(&scalar)
                    } else {
                        let bits = sheet.value_by_id(RowId(row_id), ColId(cid)).pack();
                        matcher.matches_packed(sheet, bits)
                    };
                    if matched {
                        found = Some(offset_idx);
                        return;
                    }
                    offset_idx = offset_idx.saturating_add(1);
                }
            });
        return found;
    }

    approx_search_len(col_count, needle, match_type, |idx| {
        let cid = sheet
            .col_axis()
            .pos_to_id(first_col_pos.saturating_add(idx))?;
        Some(read_sheet_cell_effective(
            sheet,
            spills,
            RowId(row_id),
            ColId(cid),
        ))
    })
}

fn search_scalar_slice(
    slice: &[EvalScalar],
    needle: &EvalScalar,
    match_type: i32,
) -> Option<usize> {
    if match_type == 0 {
        let matcher = ExactMatcher::from_needle(needle)?;
        return slice.iter().position(|v| matcher.matches_scalar(v));
    }
    let Ok(len_u32) = u32::try_from(slice.len()) else {
        return None;
    };
    approx_search_len(len_u32, needle, match_type, |idx| {
        slice.get(idx as usize).cloned()
    })
    .map(|u| u as usize)
}

/// Excel binary-search approximate match (`match_type == 1` ascending or `-1`
/// descending), skipping mismatched types/blanks/errors and extending across
/// duplicate equal neighbours so the last duplicate (`match_type == 1`) or last
/// descending duplicate (`match_type == -1`) is returned just like Excel.
fn approx_search_len(
    len: u32,
    needle: &EvalScalar,
    match_type: i32,
    read_at: impl Fn(u32) -> Option<EvalScalar>,
) -> Option<u32> {
    if len == 0 {
        return None;
    }
    // Collect `(original_offset, value)` of entries matching `needle`'s value type.
    // This matches Excel's behaviour of ignoring blanks, errors, and other types
    // during binary search!
    let mut same_type: Vec<(u32, EvalScalar)> = Vec::new();
    for i in 0..len {
        if let Some(v) = read_at(i)
            && same_value_kind(&v, needle)
        {
            same_type.push((i, v));
        }
    }
    if same_type.is_empty() {
        return None;
    }

    let mut low = 0usize;
    let mut high = same_type.len().saturating_sub(1);
    let mut best: Option<usize> = None;

    while low <= high {
        let mid = low.saturating_add(high.saturating_sub(low) / 2);
        let Some((_, mid_val)) = same_type.get(mid) else {
            break;
        };
        let ord = cmp_same_kind(mid_val, needle)?;
        if match_type > 0 {
            match ord {
                Ordering::Less | Ordering::Equal => {
                    best = Some(mid);
                    low = mid.saturating_add(1);
                }
                Ordering::Greater => {
                    if mid == 0 {
                        break;
                    }
                    high = mid.saturating_sub(1);
                }
            }
        } else {
            match ord {
                Ordering::Greater | Ordering::Equal => {
                    best = Some(mid);
                    low = mid.saturating_add(1);
                }
                Ordering::Less => {
                    if mid == 0 {
                        break;
                    }
                    high = mid.saturating_sub(1);
                }
            }
        }
    }

    best.and_then(|pos| same_type.get(pos).map(|&(orig_idx, _)| orig_idx))
}

const fn same_value_kind(a: &EvalScalar, b: &EvalScalar) -> bool {
    matches!(
        (a, b),
        (EvalScalar::Number(_), EvalScalar::Number(_))
            | (EvalScalar::Text(_), EvalScalar::Text(_))
            | (EvalScalar::Bool(_), EvalScalar::Bool(_))
    )
}

fn cmp_same_kind(a: &EvalScalar, b: &EvalScalar) -> Option<Ordering> {
    match (a, b) {
        (EvalScalar::Number(x), EvalScalar::Number(y)) => cmp_excel_f64(*x, *y),
        (EvalScalar::Text(x), EvalScalar::Text(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        (EvalScalar::Bool(x), EvalScalar::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

// ------------------------------------------------ XLOOKUP / XMATCH / LOOKUP

fn scalar_exact_literal(a: &EvalScalar, b: &EvalScalar) -> bool {
    match (a, b) {
        (EvalScalar::Empty, EvalScalar::Empty) => false,
        (EvalScalar::Number(x), EvalScalar::Number(y)) => {
            let cx = if *x == 0.0 { 0.0 } else { *x };
            let cy = if *y == 0.0 { 0.0 } else { *y };
            cx.to_bits() == cy.to_bits()
        }
        (EvalScalar::Bool(x), EvalScalar::Bool(y)) => *x == *y,
        (EvalScalar::Text(x), EvalScalar::Text(y)) => {
            if x.is_ascii() && y.is_ascii() {
                x.eq_ignore_ascii_case(y)
            } else {
                x.to_lowercase() == y.to_lowercase()
            }
        }
        _ => false,
    }
}

enum VectorSource<'a> {
    RefCol {
        sheet: &'a Sheet,
        col_id: u64,
        r0: u32,
        len: u32,
    },
    RefRow {
        sheet: &'a Sheet,
        row_id: u64,
        c0: u32,
        len: u32,
    },
    Slice {
        vals: Vec<EvalScalar>,
    },
}

impl<'a> VectorSource<'a> {
    fn from_value(
        val: &EvalValue,
        workbook: &'a Workbook<BoundFormula>,
    ) -> Result<(Self, bool, u32), CalcError> {
        // Returns `(source, is_col_vector, length)`.
        match val {
            EvalValue::Reference(r) => {
                let (sid, r0, rows, c0, cols) = ref_live_origin_and_size(r, workbook)?;
                if rows > 1 && cols > 1 {
                    return Err(CalcError::Value);
                }
                let sheet = workbook.sheet(sid).ok_or(CalcError::Ref)?;
                if cols == 1 {
                    let cid = sheet.col_axis().pos_to_id(c0).ok_or(CalcError::Ref)?;
                    Ok((
                        Self::RefCol {
                            sheet,
                            col_id: cid,
                            r0,
                            len: rows,
                        },
                        true,
                        rows,
                    ))
                } else {
                    let rid = sheet.row_axis().pos_to_id(r0).ok_or(CalcError::Ref)?;
                    Ok((
                        Self::RefRow {
                            sheet,
                            row_id: rid,
                            c0,
                            len: cols,
                        },
                        false,
                        cols,
                    ))
                }
            }
            EvalValue::Array(arr) => {
                if arr.rows > 1 && arr.cols > 1 {
                    return Err(CalcError::Value);
                }
                let is_col = arr.cols == 1 && arr.rows >= 1;
                let len = if is_col { arr.rows } else { arr.cols };
                Ok((
                    Self::Slice {
                        vals: arr.values.clone(),
                    },
                    is_col,
                    len,
                ))
            }
            EvalValue::Scalar(s) => Ok((
                Self::Slice {
                    vals: alloc::vec![s.clone()],
                },
                true,
                1,
            )),
            EvalValue::Missing => Err(CalcError::Value),
        }
    }

    fn read_at(&self, idx: u32, spills: &SpillRegistry) -> Option<EvalScalar> {
        match self {
            Self::RefCol {
                sheet,
                col_id,
                r0,
                len,
            } => {
                if idx >= *len {
                    return None;
                }
                let rid = sheet.row_axis().pos_to_id(r0.saturating_add(idx))?;
                Some(read_sheet_cell_effective(
                    sheet,
                    spills,
                    RowId(rid),
                    ColId(*col_id),
                ))
            }
            Self::RefRow {
                sheet,
                row_id,
                c0,
                len,
            } => {
                if idx >= *len {
                    return None;
                }
                let cid = sheet.col_axis().pos_to_id(c0.saturating_add(idx))?;
                Some(read_sheet_cell_effective(
                    sheet,
                    spills,
                    RowId(*row_id),
                    ColId(cid),
                ))
            }
            Self::Slice { vals } => vals.get(idx as usize).cloned(),
        }
    }
}

fn x_search_index(
    src: &VectorSource<'_>,
    len: u32,
    needle: &EvalScalar,
    match_mode: isize,
    search_mode: isize,
    spills: &SpillRegistry,
    lookup_cache: &mut LookupIndexCache,
) -> Option<u32> {
    if len == 0 {
        return None;
    }
    if let EvalScalar::Error(_) = needle {
        return None;
    }
    let norm_needle = match needle {
        EvalScalar::Empty => EvalScalar::Number(0.0),
        other => other.clone(),
    };

    // Binary searches (`search_mode == 2` ascending, `-2` descending).
    if search_mode == 2 || search_mode == -2 {
        let asc = search_mode == 2;
        let mut same_type: Vec<(u32, EvalScalar)> = Vec::new();
        for i in 0..len {
            if let Some(v) = src.read_at(i, spills)
                && same_value_kind(&v, &norm_needle)
            {
                same_type.push((i, v));
            }
        }
        if same_type.is_empty() {
            return None;
        }
        let mut low = 0_usize;
        let mut high = same_type.len().saturating_sub(1);
        let mut eq_idx: Option<usize> = None;
        let mut best_smaller: Option<usize> = None;
        let mut best_larger: Option<usize> = None;

        while low <= high {
            let mid = low.saturating_add(high.saturating_sub(low) / 2);
            let Some((_, mv)) = same_type.get(mid) else {
                break;
            };
            let ord = cmp_same_kind(mv, &norm_needle)?;
            match ord {
                Ordering::Equal => {
                    eq_idx = Some(mid);
                    break;
                }
                Ordering::Less => {
                    best_smaller = Some(mid);
                    if asc {
                        low = mid.saturating_add(1);
                    } else {
                        if mid == 0 {
                            break;
                        }
                        high = mid.saturating_sub(1);
                    }
                }
                Ordering::Greater => {
                    best_larger = Some(mid);
                    if asc {
                        if mid == 0 {
                            break;
                        }
                        high = mid.saturating_sub(1);
                    } else {
                        low = mid.saturating_add(1);
                    }
                }
            }
        }
        let picked = match match_mode {
            0 => eq_idx,
            -1 => eq_idx.or(best_smaller),
            1 => eq_idx.or(best_larger),
            _ => None,
        };
        return picked.and_then(|pos| same_type.get(pos).map(|&(orig, _)| orig));
    }

    let reverse = search_mode == -1;
    if !reverse
        && (match_mode == 0 || match_mode == 2)
        && let VectorSource::RefCol {
            sheet,
            col_id,
            r0,
            len: col_len,
        } = src
        && !spills.has_shadows_in_col(sheet.id(), ColId(*col_id))
    {
        let allow_wild = match_mode == 2;
        if !(allow_wild && matches!(&norm_needle, EvalScalar::Text(s) if has_wildcard_tokens(s)))
            && let Some(idx) = lookup_cache.get_or_build(sheet, ColId(*col_id), *r0, *col_len, 1)
            && let LookupProbeResult::Hit(hit) = idx.probe_scalar(sheet, &norm_needle, allow_wild)
        {
            return hit;
        }
    }
    if match_mode == 0 {
        for step in 0..len {
            let idx = if reverse {
                len.saturating_sub(1).saturating_sub(step)
            } else {
                step
            };
            if let Some(v) = src.read_at(idx, spills)
                && scalar_exact_literal(&v, &norm_needle)
            {
                return Some(idx);
            }
        }
        return None;
    }

    if match_mode == 2 {
        let matcher = ExactMatcher::from_needle(&norm_needle)?;
        for step in 0..len {
            let idx = if reverse {
                len.saturating_sub(1).saturating_sub(step)
            } else {
                step
            };
            if let Some(v) = src.read_at(idx, spills)
                && matcher.matches_scalar(&v)
            {
                return Some(idx);
            }
        }
        return None;
    }

    // `match_mode == -1` (exact or next smaller) or `1` (exact or next larger).
    let mut best: Option<(u32, EvalScalar)> = None;
    for step in 0..len {
        let idx = if reverse {
            len.saturating_sub(1).saturating_sub(step)
        } else {
            step
        };
        let Some(v) = src.read_at(idx, spills) else {
            continue;
        };
        if !same_value_kind(&v, &norm_needle) {
            continue;
        }
        let Some(ord) = cmp_same_kind(&v, &norm_needle) else {
            continue;
        };
        if ord == Ordering::Equal {
            return Some(idx);
        }
        let candidate = if match_mode == -1 {
            ord == Ordering::Less
        } else {
            ord == Ordering::Greater
        };
        if candidate {
            let better = match &best {
                None => true,
                Some((_, cur_best)) => {
                    let Some(c_ord) = cmp_same_kind(&v, cur_best) else {
                        continue;
                    };
                    if match_mode == -1 {
                        c_ord == Ordering::Greater
                    } else {
                        c_ord == Ordering::Less
                    }
                }
            };
            if better {
                best = Some((idx, v));
            }
        }
    }
    best.map(|(i, _)| i)
}

fn slice_return_at_offset(
    ret_arg: &EvalValue,
    idx: u32,
    lookup_is_col: bool,
    lookup_len: u32,
    workbook: &Workbook<BoundFormula>,
) -> Result<EvalValue, CalcError> {
    match ret_arg {
        EvalValue::Reference(r) => {
            let (sid, r0, rows, c0, cols) = ref_live_origin_and_size(r, workbook)?;
            let sheet = workbook.sheet(sid).ok_or(CalcError::Ref)?;
            if lookup_is_col {
                if rows != lookup_len {
                    return Err(CalcError::Value);
                }
                let rid = sheet
                    .row_axis()
                    .pos_to_id(r0.saturating_add(idx))
                    .ok_or(CalcError::Ref)?;
                if cols == 1 {
                    let cid = sheet.col_axis().pos_to_id(c0).ok_or(CalcError::Ref)?;
                    Ok(EvalValue::Reference(ResolvedRef::Cell {
                        sheet: sid,
                        row: RowId(rid),
                        col: ColId(cid),
                    }))
                } else {
                    let fc = sheet.col_axis().pos_to_id(c0).ok_or(CalcError::Ref)?;
                    let lc = sheet
                        .col_axis()
                        .pos_to_id(c0.saturating_add(cols.saturating_sub(1)))
                        .ok_or(CalcError::Ref)?;
                    Ok(EvalValue::Reference(ResolvedRef::Area {
                        sheet: sid,
                        first_row: RowId(rid),
                        last_row: RowId(rid),
                        first_col: ColId(fc),
                        last_col: ColId(lc),
                    }))
                }
            } else {
                if cols != lookup_len {
                    return Err(CalcError::Value);
                }
                let cid = sheet
                    .col_axis()
                    .pos_to_id(c0.saturating_add(idx))
                    .ok_or(CalcError::Ref)?;
                if rows == 1 {
                    let rid = sheet.row_axis().pos_to_id(r0).ok_or(CalcError::Ref)?;
                    Ok(EvalValue::Reference(ResolvedRef::Cell {
                        sheet: sid,
                        row: RowId(rid),
                        col: ColId(cid),
                    }))
                } else {
                    let fr = sheet.row_axis().pos_to_id(r0).ok_or(CalcError::Ref)?;
                    let lr = sheet
                        .row_axis()
                        .pos_to_id(r0.saturating_add(rows.saturating_sub(1)))
                        .ok_or(CalcError::Ref)?;
                    Ok(EvalValue::Reference(ResolvedRef::Area {
                        sheet: sid,
                        first_row: RowId(fr),
                        last_row: RowId(lr),
                        first_col: ColId(cid),
                        last_col: ColId(cid),
                    }))
                }
            }
        }
        EvalValue::Array(arr) => {
            if lookup_is_col {
                if arr.rows != lookup_len {
                    return Err(CalcError::Value);
                }
                if arr.cols == 1 {
                    Ok(EvalValue::Scalar(empty_to_zero(arr.get_at(idx, 0))))
                } else {
                    let vals = (0..arr.cols)
                        .map(|c| empty_to_zero(arr.get_at(idx, c)))
                        .collect();
                    Ok(EvalValue::Array(Array2D::new(1, arr.cols, vals)?))
                }
            } else {
                if arr.cols != lookup_len {
                    return Err(CalcError::Value);
                }
                if arr.rows == 1 {
                    Ok(EvalValue::Scalar(empty_to_zero(arr.get_at(0, idx))))
                } else {
                    let vals = (0..arr.rows)
                        .map(|r| empty_to_zero(arr.get_at(r, idx)))
                        .collect();
                    Ok(EvalValue::Array(Array2D::new(arr.rows, 1, vals)?))
                }
            }
        }
        EvalValue::Scalar(s) => {
            if lookup_len == 1 {
                Ok(EvalValue::Scalar(s.clone()))
            } else {
                Err(CalcError::Value)
            }
        }
        EvalValue::Missing => Err(CalcError::Value),
    }
}

fn read_opt_isize_lookup(
    opt: Option<EvalValue>,
    default: isize,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<isize, CalcError> {
    match opt {
        None | Some(EvalValue::Missing) => Ok(default),
        Some(v) => {
            let sa = coerce_for_scalar_op(v, workbook, spills, caller)?;
            let ScalarOrArray::Scalar(sc) = sa else {
                return Err(CalcError::Value);
            };
            sc.to_number().and_then(trunc_to_isize)
        }
    }
}

/// Evaluates `XLOOKUP(lookup_value, lookup_array, return_array, [if_not_found], [match_mode], [search_mode])`.
pub fn eval_xlookup(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    lookup_cache: &mut LookupIndexCache,
) -> EvalValue {
    if !(3..=6).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(lookup_arg), Some(lu_arr_arg), Some(ret_arr_arg)) = (it.next(), it.next(), it.next())
    else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let not_found = it.next().filter(|v| !matches!(v, EvalValue::Missing));
    let match_mode = match read_opt_isize_lookup(it.next(), 0, workbook, spills, caller) {
        Ok(v @ -1..=2) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let search_mode = match read_opt_isize_lookup(it.next(), 1, workbook, spills, caller) {
        Ok(v) if matches!(v, 1 | -1 | 2 | -2) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if match_mode == 2 && (search_mode == 2 || search_mode == -2) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }

    let (src, is_col, len) = match VectorSource::from_value(&lu_arr_arg, workbook) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    // Validate `return_array` axis length upfront even if not found.
    let check_dim = slice_return_at_offset(&ret_arr_arg, 0, is_col, len, workbook);
    if len > 0
        && let Err(e) = check_dim
    {
        return EvalValue::Scalar(EvalScalar::Error(e));
    }

    let lu_sa = match coerce_for_scalar_op(lookup_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match lu_sa {
        ScalarOrArray::Scalar(needle) => {
            if let EvalScalar::Error(e) = needle {
                return EvalValue::Scalar(EvalScalar::Error(e));
            }
            if let Some(hit) = x_search_index(
                &src,
                len,
                &needle,
                match_mode,
                search_mode,
                spills,
                lookup_cache,
            ) {
                match slice_return_at_offset(&ret_arr_arg, hit, is_col, len, workbook) {
                    Ok(v) => v,
                    Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
                }
            } else {
                not_found.unwrap_or(EvalValue::Scalar(EvalScalar::Error(CalcError::Na)))
            }
        }
        ScalarOrArray::Array(arr) => {
            let nf_scalar = match not_found {
                None => EvalScalar::Error(CalcError::Na),
                Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
                    Ok(ScalarOrArray::Scalar(s)) => empty_to_zero(s),
                    Ok(ScalarOrArray::Array(a)) => a
                        .values
                        .first()
                        .cloned()
                        .map_or(EvalScalar::Error(CalcError::Na), empty_to_zero),
                    Err(e) => EvalScalar::Error(e),
                },
            };
            let mut out = Vec::with_capacity(arr.values.len());
            for needle in &arr.values {
                if let EvalScalar::Error(e) = needle {
                    out.push(EvalScalar::Error(*e));
                    continue;
                }
                let cell = if let Some(hit) = x_search_index(
                    &src,
                    len,
                    needle,
                    match_mode,
                    search_mode,
                    spills,
                    lookup_cache,
                ) {
                    match slice_return_at_offset(&ret_arr_arg, hit, is_col, len, workbook) {
                        Ok(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
                            Ok(ScalarOrArray::Scalar(s)) => empty_to_zero(s),
                            Ok(ScalarOrArray::Array(a)) => a
                                .values
                                .first()
                                .cloned()
                                .map_or(EvalScalar::Error(CalcError::Value), empty_to_zero),
                            Err(e) => EvalScalar::Error(e),
                        },
                        Err(e) => EvalScalar::Error(e),
                    }
                } else {
                    nf_scalar.clone()
                };
                out.push(cell);
            }
            match Array2D::new(arr.rows, arr.cols, out) {
                Ok(res) => EvalValue::Array(res),
                Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    }
}

/// Evaluates `XMATCH(lookup_value, lookup_array, [match_mode], [search_mode])`.
pub fn eval_xmatch(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    lookup_cache: &mut LookupIndexCache,
) -> EvalValue {
    if !(2..=4).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(lookup_arg), Some(lu_arr_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let match_mode = match read_opt_isize_lookup(it.next(), 0, workbook, spills, caller) {
        Ok(v @ -1..=2) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let search_mode = match read_opt_isize_lookup(it.next(), 1, workbook, spills, caller) {
        Ok(v) if matches!(v, 1 | -1 | 2 | -2) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if match_mode == 2 && (search_mode == 2 || search_mode == -2) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }

    let (src, _, len) = match VectorSource::from_value(&lu_arr_arg, workbook) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let lu_sa = match coerce_for_scalar_op(lookup_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match lu_sa {
        ScalarOrArray::Scalar(needle) => EvalValue::Scalar(match needle {
            EvalScalar::Error(e) => EvalScalar::Error(e),
            ref s => x_search_index(&src, len, s, match_mode, search_mode, spills, lookup_cache)
                .map_or(EvalScalar::Error(CalcError::Na), |idx0| {
                    EvalScalar::Number(f64::from(idx0.saturating_add(1)))
                }),
        }),
        ScalarOrArray::Array(arr) => {
            let mut vals = Vec::with_capacity(arr.values.len());
            for needle in &arr.values {
                let res = match needle {
                    EvalScalar::Error(e) => EvalScalar::Error(*e),
                    s => {
                        x_search_index(&src, len, s, match_mode, search_mode, spills, lookup_cache)
                            .map_or(EvalScalar::Error(CalcError::Na), |idx0| {
                                EvalScalar::Number(f64::from(idx0.saturating_add(1)))
                            })
                    }
                };
                vals.push(res);
            }
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values: vals,
            })
        }
    }
}

/// Evaluates `LOOKUP(lookup_value, lookup_vector_or_array, [result_vector])`.
pub fn eval_lookup(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    lookup_cache: &mut LookupIndexCache,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(lookup_arg), Some(vec_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let res_opt = it.next().filter(|v| !matches!(v, EvalValue::Missing));

    let lu_sa = match coerce_for_scalar_op(lookup_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let mut lookup_scalar = |needle: &EvalScalar,
                             vec_a: &EvalValue,
                             res_a: Option<&EvalValue>|
     -> EvalScalar {
        if let EvalScalar::Error(e) = needle {
            return EvalScalar::Error(*e);
        }
        let norm_needle = match needle {
            EvalScalar::Empty => EvalScalar::Number(0.0),
            other => other.clone(),
        };
        if let Some(ra) = res_a {
            let (l_src, _, l_len) = match VectorSource::from_value(vec_a, workbook) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let (r_src, _, r_len) = match VectorSource::from_value(ra, workbook) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if l_len != r_len {
                return EvalScalar::Error(CalcError::Na);
            }
            let Some(hit) =
                x_search_index(&l_src, l_len, &norm_needle, -1, 2, spills, lookup_cache)
            else {
                return EvalScalar::Error(CalcError::Na);
            };
            r_src
                .read_at(hit, spills)
                .map_or(EvalScalar::Error(CalcError::Na), empty_to_zero)
        } else {
            let arr = match super::dynamic_array::value_to_array2d(vec_a.clone(), workbook, spills)
            {
                Ok(a) => a,
                Err(e) => return EvalScalar::Error(e),
            };
            if arr.cols > arr.rows {
                let first_row: Vec<EvalScalar> = (0..arr.cols).map(|c| arr.get_at(0, c)).collect();
                let Some(hit) = search_scalar_slice(&first_row, &norm_needle, 1) else {
                    return EvalScalar::Error(CalcError::Na);
                };
                empty_to_zero(
                    arr.get_at(arr.rows.saturating_sub(1), u32::try_from(hit).unwrap_or(0)),
                )
            } else {
                let first_col: Vec<EvalScalar> = (0..arr.rows).map(|r| arr.get_at(r, 0)).collect();
                let Some(hit) = search_scalar_slice(&first_col, &norm_needle, 1) else {
                    return EvalScalar::Error(CalcError::Na);
                };
                empty_to_zero(
                    arr.get_at(u32::try_from(hit).unwrap_or(0), arr.cols.saturating_sub(1)),
                )
            }
        }
    };

    match lu_sa {
        ScalarOrArray::Scalar(needle) => {
            EvalValue::Scalar(lookup_scalar(&needle, &vec_arg, res_opt.as_ref()))
        }
        ScalarOrArray::Array(arr) => {
            let vals = arr
                .values
                .iter()
                .map(|n| lookup_scalar(n, &vec_arg, res_opt.as_ref()))
                .collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values: vals,
            })
        }
    }
}

/// Evaluates `ADDRESS(row_num, column_num, [abs_num], [a1], [sheet_text])`.
pub fn eval_address(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=5).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(r_arg), Some(c_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let r_sa = match coerce_for_scalar_op(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let c_sa = match coerce_for_scalar_op(c_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let (ScalarOrArray::Scalar(r_sc), ScalarOrArray::Scalar(c_sc)) = (r_sa, c_sa) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let row_num = match r_sc.to_number().and_then(trunc_to_isize) {
        Ok(v) if (1..=1_048_576).contains(&v) => u32::try_from(v).unwrap_or(1),
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let col_num = match c_sc.to_number().and_then(trunc_to_isize) {
        Ok(v) if (1..=16_384).contains(&v) => u32::try_from(v).unwrap_or(1),
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let abs_num = match read_opt_isize_lookup(it.next(), 1, workbook, spills, caller) {
        Ok(v) if (1..=4).contains(&v) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let a1 = match it.next() {
        None | Some(EvalValue::Missing) => true,
        Some(v) => {
            let sa = match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(s) => s,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_bool() {
                Ok(b) => b,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };
    let sheet_opt: Option<String> = match it.next() {
        None | Some(EvalValue::Missing) => None,
        Some(v) => {
            let sa = match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(s) => s,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_text() {
                Ok(s) if s.is_empty() => None,
                Ok(s) => Some(s),
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };

    let mut out = String::new();
    if let Some(ref s) = sheet_opt {
        write_sheet_prefix(Some(s), &mut out);
    }
    if a1 {
        let abs_col = matches!(abs_num, 1 | 3);
        let abs_row = matches!(abs_num, 1 | 2);
        if abs_col {
            out.push('$');
        }
        write_col_letters(col_num.saturating_sub(1), &mut out);
        if abs_row {
            out.push('$');
        }
        out.push_str(&format!("{row_num}"));
    } else {
        match abs_num {
            1 => out.push_str(&format!("R{row_num}C{col_num}")),
            2 => out.push_str(&format!("R{row_num}C[{col_num}]")),
            3 => out.push_str(&format!("R[{row_num}]C{col_num}")),
            _ => out.push_str(&format!("R[{row_num}]C[{col_num}]")),
        }
    }
    EvalValue::Scalar(EvalScalar::Text(out))
}

/// Evaluates `AREAS(reference)` (non-dereferenced).
pub fn eval_areas(args: &[EvalValue]) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    match args.first() {
        Some(EvalValue::Reference(ResolvedRef::Invalid(e))) => {
            EvalValue::Scalar(EvalScalar::Error(*e))
        }
        Some(EvalValue::Reference(_)) => EvalValue::Scalar(EvalScalar::Number(1.0)),
        _ => EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
    }
}

/// Computes `OFFSET(reference, rows, cols, [height], [width])` without
/// dereferencing `reference`.
pub fn eval_offset(
    base_ref: EvalValue,
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    let EvalValue::Reference(r) = base_ref else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let (sheet_id, r0, orig_rows, c0, orig_cols) = match ref_live_origin_and_size(&r, workbook) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let (Some(dr_arg), Some(dc_arg)) = (args.get(1), args.get(2)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let read_int = |arg: &EvalValue| -> Result<i64, CalcError> {
        let sa = coerce_for_scalar_op(arg.clone(), workbook, spills, caller)?;
        let ScalarOrArray::Scalar(sc) = sa else {
            return Err(CalcError::Value);
        };
        let n = sc.to_number()?;
        if !n.is_finite() {
            return Err(CalcError::Num);
        }
        let t = num::trunc(n);
        if !(-10_000_000.0..=10_000_000.0).contains(&t) {
            return Err(CalcError::Ref);
        }
        #[expect(clippy::cast_possible_truncation, reason = "bounded to +-10M")]
        Ok(t as i64)
    };
    let dr = match read_int(dr_arg) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let dc = match read_int(dc_arg) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let height = match args.get(3) {
        None | Some(EvalValue::Missing) => i64::from(orig_rows),
        Some(a) => match read_int(a) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
    };
    let width = match args.get(4) {
        None | Some(EvalValue::Missing) => i64::from(orig_cols),
        Some(a) => match read_int(a) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
    };
    if height <= 0 || width <= 0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    }

    let Some(sheet) = workbook.sheet(sheet_id) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };
    let live_r = i64::from(sheet.row_axis().live_len());
    let live_c = i64::from(sheet.col_axis().live_len());
    let nr0 = i64::from(r0).saturating_add(dr);
    let nc0 = i64::from(c0).saturating_add(dc);
    let nr1 = nr0.saturating_add(height).saturating_sub(1);
    let nc1 = nc0.saturating_add(width).saturating_sub(1);
    if nr0 < 0 || nc0 < 0 || nr1 >= live_r || nc1 >= live_c {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    }
    let (Ok(fr_pos), Ok(lr_pos), Ok(fc_pos), Ok(lc_pos)) = (
        u32::try_from(nr0),
        u32::try_from(nr1),
        u32::try_from(nc0),
        u32::try_from(nc1),
    ) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };
    let (Some(fr), Some(lr), Some(fc), Some(lc)) = (
        sheet.row_axis().pos_to_id(fr_pos),
        sheet.row_axis().pos_to_id(lr_pos),
        sheet.col_axis().pos_to_id(fc_pos),
        sheet.col_axis().pos_to_id(lc_pos),
    ) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
    };
    if fr_pos == lr_pos && fc_pos == lc_pos {
        EvalValue::Reference(ResolvedRef::Cell {
            sheet: sheet_id,
            row: RowId(fr),
            col: ColId(fc),
        })
    } else {
        EvalValue::Reference(ResolvedRef::Area {
            sheet: sheet_id,
            first_row: RowId(fr),
            last_row: RowId(lr),
            first_col: ColId(fc),
            last_col: ColId(lc),
        })
    }
}

/// Resolution result for `INDIRECT`.
pub enum IndirectTarget {
    /// Direct cell/range reference.
    Ref(ResolvedRef),
    /// Named range that needs evaluation in the engine.
    NamedRange {
        /// Effective scope worksheet ID.
        scope_sheet: SheetId,
        /// Named range spelling.
        name: String,
    },
    /// Spill reference anchor (`A1#`).
    SpillAnchor {
        /// Sheet ID of the anchor.
        sheet: SheetId,
        /// Row ID of the anchor.
        row: RowId,
        /// Col ID of the anchor.
        col: ColId,
    },
}

/// Parses and resolves `INDIRECT(ref_text, [a1])`.
pub fn resolve_indirect(
    ref_text: &str,
    a1: bool,
    workbook: &Workbook<BoundFormula>,
    caller: CallerContext,
) -> Result<IndirectTarget, CalcError> {
    let trimmed = ref_text.trim();
    if trimmed.is_empty() {
        return Err(CalcError::Ref);
    }
    if a1 {
        let src = format!("={trimmed}");
        let expr = parse_formula(&src).map_err(|_| CalcError::Ref)?;
        let bound = bind_expr(&expr, workbook, caller.sheet);
        return match bound {
            BoundExpr::Reference(r) => {
                let res = resolve_bound_ref(&r, workbook, caller.sheet);
                if matches!(res, ResolvedRef::Invalid(_)) {
                    Err(CalcError::Ref)
                } else {
                    Ok(IndirectTarget::Ref(res))
                }
            }
            BoundExpr::Name(name) => Ok(IndirectTarget::NamedRange {
                scope_sheet: caller.sheet,
                name,
            }),
            BoundExpr::ScopedName {
                sheet,
                missing_sheet,
                name,
            } => {
                if missing_sheet.is_some() {
                    return Err(CalcError::Ref);
                }
                let sid = sheet.ok_or(CalcError::Ref)?;
                if workbook.live_sheet(sid).is_none() {
                    return Err(CalcError::Ref);
                }
                Ok(IndirectTarget::NamedRange {
                    scope_sheet: sid,
                    name,
                })
            }
            BoundExpr::SpillRef(inner) => {
                if let BoundExpr::Reference(r) = *inner
                    && let ResolvedRef::Cell { sheet, row, col } =
                        resolve_bound_ref(&r, workbook, caller.sheet)
                {
                    return Ok(IndirectTarget::SpillAnchor { sheet, row, col });
                }
                Err(CalcError::Ref)
            }
            _ => Err(CalcError::Ref),
        };
    }

    // R1C1 parser (`[Sheet!]R[r]C[c]` or `R1C1:R2C2`).
    let (sheet_id, rest) = if let Some(excl) = trimmed.rfind('!') {
        let raw_sh = trimmed.get(..excl).ok_or(CalcError::Ref)?;
        let sh_name = if raw_sh.starts_with('\'') && raw_sh.ends_with('\'') && raw_sh.len() >= 2 {
            raw_sh
                .get(1..raw_sh.len().saturating_sub(1))
                .ok_or(CalcError::Ref)?
                .replace("''", "'")
        } else {
            String::from(raw_sh)
        };
        let sid = workbook
            .live_sheets()
            .find(|s| s.name().eq_ignore_ascii_case(&sh_name))
            .map(Sheet::id)
            .ok_or(CalcError::Ref)?;
        (
            sid,
            trimmed
                .get(excl.saturating_add(1)..)
                .ok_or(CalcError::Ref)?,
        )
    } else {
        (caller.sheet, trimmed)
    };

    let caller_sheet = workbook.sheet(caller.sheet).ok_or(CalcError::Ref)?;
    let target_sheet = workbook.sheet(sheet_id).ok_or(CalcError::Ref)?;
    let caller_r = caller_sheet
        .row_axis()
        .id_to_pos(caller.row.0)
        .ok_or(CalcError::Ref)?;
    let caller_c = caller_sheet
        .col_axis()
        .id_to_pos(caller.col.0)
        .ok_or(CalcError::Ref)?;

    if let Some((lhs, rhs)) = rest.split_once(':') {
        let (r1, c1) = parse_r1c1_cell(lhs.trim(), caller_r, caller_c).ok_or(CalcError::Ref)?;
        let (r2, c2) = parse_r1c1_cell(rhs.trim(), caller_r, caller_c).ok_or(CalcError::Ref)?;
        let (fr_p, lr_p) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
        let (fc_p, lc_p) = if c1 <= c2 { (c1, c2) } else { (c2, c1) };
        let (Some(fr), Some(lr), Some(fc), Some(lc)) = (
            target_sheet.row_axis().pos_to_id(fr_p),
            target_sheet.row_axis().pos_to_id(lr_p),
            target_sheet.col_axis().pos_to_id(fc_p),
            target_sheet.col_axis().pos_to_id(lc_p),
        ) else {
            return Err(CalcError::Ref);
        };
        Ok(IndirectTarget::Ref(ResolvedRef::Area {
            sheet: sheet_id,
            first_row: RowId(fr),
            last_row: RowId(lr),
            first_col: ColId(fc),
            last_col: ColId(lc),
        }))
    } else {
        let (r_p, c_p) = parse_r1c1_cell(rest.trim(), caller_r, caller_c).ok_or(CalcError::Ref)?;
        let (Some(rid), Some(cid)) = (
            target_sheet.row_axis().pos_to_id(r_p),
            target_sheet.col_axis().pos_to_id(c_p),
        ) else {
            return Err(CalcError::Ref);
        };
        Ok(IndirectTarget::Ref(ResolvedRef::Cell {
            sheet: sheet_id,
            row: RowId(rid),
            col: ColId(cid),
        }))
    }
}

fn parse_r1c1_cell(token: &str, base_r: u32, base_c: u32) -> Option<(u32, u32)> {
    let bytes = token.as_bytes();
    if !matches!(bytes.first(), Some(b'R' | b'r')) {
        return None;
    }
    let after_r = token.get(1..)?;
    let c_idx = after_r.find(['C', 'c'])?;
    let r_spec = after_r.get(..c_idx)?;
    let c_spec = after_r.get(c_idx.saturating_add(1)..)?;
    let row = parse_r1c1_coord(r_spec, base_r, MAX_ROW_AXIS_LEN)?;
    let col = parse_r1c1_coord(c_spec, base_c, MAX_COL_AXIS_LEN)?;
    Some((row, col))
}

fn parse_r1c1_coord(spec: &str, base: u32, limit: u32) -> Option<u32> {
    if spec.is_empty() {
        return (base < limit).then_some(base);
    }
    if let Some(inner) = spec.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let off: i64 = inner.parse().ok()?;
        let pos = i64::from(base).checked_add(off)?;
        let u = u32::try_from(pos).ok()?;
        (u < limit).then_some(u)
    } else {
        let one_based: u32 = spec.parse().ok()?;
        let zero_based = one_based.checked_sub(1)?;
        (zero_based < limit).then_some(zero_based)
    }
}
