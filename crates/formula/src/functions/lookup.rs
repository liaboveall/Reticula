//! Layer-1 lookup & reference functions (`ROWS`, `COLUMNS`, `INDEX`, `MATCH`,
//! `VLOOKUP`, `HLOOKUP`, and `CHOOSE` combiner; `14-m1b.md` 5.3).
//!
//! Exact lookup (`MATCH(..., 0)`, `VLOOKUP(..., FALSE)`, `HLOOKUP(..., FALSE)`)
//! compares numbers **bitwise** (`to_bits()`, after canonicalising `-0.0` to
//! `+0.0`) per S3b Excel 16.0.20711 verification (310/310 cases), and matches
//! strings case-insensitively with `*` / `?` / `~` wildcards.

use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::decimal15::cmp_excel_f64;
use reticula_core::id::{ColId, IdRun, RowId};
use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block};
use reticula_core::value::{CalcError, Value};

use super::criteria::ref_live_origin_and_size;
use super::wildcard::{WildToken, compile_wildcard, wildcard_matches_full};
use super::{empty_to_zero, get_sa, shape_of, trunc_to_isize};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ResolvedRef, ScalarOrArray,
    coerce_for_scalar_op, read_sheet_cell_effective,
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
        ScalarOrArray::Scalar(needle) => {
            EvalValue::Scalar(match_one(&needle, &array_arg, match_type, workbook, spills))
        }
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|needle| match_one(needle, &array_arg, match_type, workbook, spills))
                .collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn match_one(
    needle: &EvalScalar,
    array_arg: &EvalValue,
    match_type: i32,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
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
                search_sheet_column(sheet, c0, r0, rows, &norm_needle, match_type, spills)
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
        EvalValue::Scalar(_) => EvalScalar::Error(CalcError::Na),
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
        )),
        ScalarOrArray::Array(arr) => {
            let values = arr
                .values
                .iter()
                .map(|needle| {
                    vh_lookup_one(
                        needle,
                        &table_arg,
                        index_u32,
                        match_type,
                        is_vertical,
                        workbook,
                        spills,
                    )
                })
                .collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn vh_lookup_one(
    needle: &EvalScalar,
    table_arg: &EvalValue,
    index_1based: u32,
    match_type: i32,
    is_vertical: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
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
                let Some(row_offset) =
                    search_sheet_column(sheet, c0, r0, rows, &norm_needle, match_type, spills)
                else {
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
        EvalValue::Scalar(_) => EvalScalar::Error(CalcError::Na),
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
fn search_sheet_column(
    sheet: &Sheet,
    col_pos: u32,
    first_row_pos: u32,
    row_count: u32,
    needle: &EvalScalar,
    match_type: i32,
    spills: &SpillRegistry,
) -> Option<u32> {
    let col_id = sheet.col_axis().pos_to_id(col_pos)?;
    if match_type == 0 {
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
