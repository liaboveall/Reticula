//! Logical and type-information functions (`17-m3a.md` 5.1 — 12 functions:
//! `IFS`, `SWITCH`, `XOR`, `ISERR`, `ISNA`, `ISNONTEXT`, `ISFORMULA`, `ISREF`,
//! `ISEVEN`, `ISODD`, `ERROR.TYPE`, `TYPE`).

use alloc::vec::Vec;

use reticula_core::id::{ColId, RowId};
use reticula_core::num;
use reticula_core::ops::{FormulaMode, Workbook};
use reticula_core::value::{CalcError, Value};

use super::criteria::ref_live_origin_and_size;
use super::{RefCellVal, empty_to_zero, get_sa, lift_unary, shape_of, visit_reference_cells};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ResolvedRef, ScalarOrArray,
    coerce_for_scalar_op, implicit_intersect, read_cell_effective,
};

/// Evaluates Excel scalar equality for `SWITCH` (case-insensitive strings,
/// numeric D-027/float match). Errors propagate cleanly.
pub fn switch_scalar_eq(a: &EvalScalar, b: &EvalScalar) -> Result<bool, CalcError> {
    match (a, b) {
        (EvalScalar::Error(e), _) | (_, EvalScalar::Error(e)) => Err(*e),
        (EvalScalar::Empty, EvalScalar::Empty) => Ok(true),
        (EvalScalar::Empty, EvalScalar::Number(n)) | (EvalScalar::Number(n), EvalScalar::Empty) => {
            Ok(*n == 0.0)
        }
        (EvalScalar::Empty, EvalScalar::Text(s)) | (EvalScalar::Text(s), EvalScalar::Empty) => {
            Ok(s.is_empty())
        }
        (EvalScalar::Empty, EvalScalar::Bool(b)) | (EvalScalar::Bool(b), EvalScalar::Empty) => {
            Ok(!*b)
        }
        (EvalScalar::Number(x), EvalScalar::Number(y)) => Ok(*x == *y),
        (EvalScalar::Bool(x), EvalScalar::Bool(y)) => Ok(*x == *y),
        (EvalScalar::Text(x), EvalScalar::Text(y)) => {
            if x.is_ascii() && y.is_ascii() {
                Ok(x.eq_ignore_ascii_case(y))
            } else {
                Ok(x.to_lowercase() == y.to_lowercase())
            }
        }
        _ => Ok(false),
    }
}

/// Evaluates `XOR(logical1, [logical2], ...)`.
pub fn eval_xor(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut true_count: usize = 0;
    let mut seen_any = false;
    for arg in args {
        match arg {
            EvalValue::Missing | EvalValue::Scalar(EvalScalar::Empty) => {}
            EvalValue::Scalar(s) => match s.to_bool() {
                Ok(b) => {
                    seen_any = true;
                    if b {
                        true_count = true_count.saturating_add(1);
                    }
                }
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            },
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    match item {
                        EvalScalar::Error(e) => return EvalValue::Scalar(EvalScalar::Error(*e)),
                        EvalScalar::Bool(b) => {
                            seen_any = true;
                            if *b {
                                true_count = true_count.saturating_add(1);
                            }
                        }
                        EvalScalar::Number(n) => {
                            seen_any = true;
                            if *n != 0.0 {
                                true_count = true_count.saturating_add(1);
                            }
                        }
                        EvalScalar::Empty | EvalScalar::Text(_) => {}
                    }
                }
            }
            EvalValue::Reference(r) => {
                let res = visit_reference_cells(r, workbook, spills, |_sheet, cell| match cell {
                    RefCellVal::Word(bits) => match Value::unpack(bits) {
                        Value::Error(e) => Err(e),
                        Value::Bool(b) => {
                            seen_any = true;
                            if b {
                                true_count = true_count.saturating_add(1);
                            }
                            Ok(())
                        }
                        Value::Number(n) => {
                            seen_any = true;
                            if n != 0.0 {
                                true_count = true_count.saturating_add(1);
                            }
                            Ok(())
                        }
                        Value::Empty | Value::Text(_) | Value::Array(_) => Ok(()),
                    },
                    RefCellVal::Shadow(s) => match s {
                        EvalScalar::Error(e) => Err(*e),
                        EvalScalar::Bool(b) => {
                            seen_any = true;
                            if *b {
                                true_count = true_count.saturating_add(1);
                            }
                            Ok(())
                        }
                        EvalScalar::Number(n) => {
                            seen_any = true;
                            if *n != 0.0 {
                                true_count = true_count.saturating_add(1);
                            }
                            Ok(())
                        }
                        EvalScalar::Empty | EvalScalar::Text(_) => Ok(()),
                    },
                });
                if let Err(e) = res {
                    return EvalValue::Scalar(EvalScalar::Error(e));
                }
            }
        }
    }
    EvalValue::Scalar(if seen_any {
        EvalScalar::Bool(true_count % 2 == 1)
    } else {
        EvalScalar::Error(CalcError::Value)
    })
}

/// Evaluates `ISERR(value)`.
pub fn eval_iserr(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |x| {
        EvalScalar::Bool(matches!(x, EvalScalar::Error(e) if *e != CalcError::Na))
    })
}

/// Evaluates `ISNA(value)`.
pub fn eval_isna(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |x| {
        EvalScalar::Bool(matches!(x, EvalScalar::Error(CalcError::Na)))
    })
}

/// Evaluates `ISNONTEXT(value)`.
pub fn eval_isnontext(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |x| {
        EvalScalar::Bool(!matches!(x, EvalScalar::Text(_)))
    })
}

/// Evaluates `ISEVEN(number)` (`is_even == true`) or `ISODD(number)` (`is_even == false`).
pub fn eval_iseven_or_isodd(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_even: bool,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |x| match x {
        EvalScalar::Error(e) => EvalScalar::Error(*e),
        EvalScalar::Bool(_) | EvalScalar::Text(_) => EvalScalar::Error(CalcError::Value),
        EvalScalar::Empty => EvalScalar::Bool(is_even),
        EvalScalar::Number(n) => {
            if !n.is_finite() {
                return EvalScalar::Error(CalcError::Num);
            }
            let t = num::trunc(*n);
            let even = t % 2.0 == 0.0;
            EvalScalar::Bool(if is_even { even } else { !even })
        }
    })
}

/// Evaluates `ISREF(value)` (non-dereferenced).
pub fn eval_isref(args: &[EvalValue]) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(arg0) = args.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let is_ref = matches!(
        arg0,
        EvalValue::Reference(r) if !matches!(r, ResolvedRef::Invalid(_))
    );
    EvalValue::Scalar(EvalScalar::Bool(is_ref))
}

/// Evaluates `ISFORMULA(reference)` (non-dereferenced).
pub fn eval_isformula(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    caller: CallerContext,
) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(EvalValue::Reference(r)) = args.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    match r {
        ResolvedRef::Invalid(e) => EvalValue::Scalar(EvalScalar::Error(*e)),
        ResolvedRef::Cell { sheet, row, col } => {
            if workbook.sheet(*sheet).is_none() {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
            }
            EvalValue::Scalar(EvalScalar::Bool(
                workbook.formula(*sheet, *row, *col).is_some(),
            ))
        }
        ResolvedRef::Area { .. } | ResolvedRef::FullCols { .. } | ResolvedRef::FullRows { .. } => {
            let (sheet_id, r0, rows, c0, cols) = match ref_live_origin_and_size(r, workbook) {
                Ok(v) => v,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let Some(sh) = workbook.sheet(sheet_id) else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
            };
            if caller.mode == FormulaMode::Legacy {
                let target_r = if rows == 1 {
                    Some(r0)
                } else {
                    workbook
                        .sheet(caller.sheet)
                        .and_then(|h| h.row_axis().id_to_pos(caller.row.0))
                        .filter(|&p| p >= r0 && p < r0.saturating_add(rows))
                };
                let target_c = if cols == 1 {
                    Some(c0)
                } else {
                    workbook
                        .sheet(caller.sheet)
                        .and_then(|h| h.col_axis().id_to_pos(caller.col.0))
                        .filter(|&p| p >= c0 && p < c0.saturating_add(cols))
                };
                let (Some(rp), Some(cp)) = (target_r, target_c) else {
                    return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
                };
                let (Some(rid), Some(cid)) =
                    (sh.row_axis().pos_to_id(rp), sh.col_axis().pos_to_id(cp))
                else {
                    return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
                };
                return EvalValue::Scalar(EvalScalar::Bool(
                    workbook.formula(sheet_id, RowId(rid), ColId(cid)).is_some(),
                ));
            }
            let Some(total) = (rows as usize).checked_mul(cols as usize) else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
            };
            if total > MAX_ARRAY_CELLS {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
            }
            let mut vals = Vec::with_capacity(total);
            for dr in 0..rows {
                let Some(rid) = sh.row_axis().pos_to_id(r0.saturating_add(dr)) else {
                    return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
                };
                for dc in 0..cols {
                    let Some(cid) = sh.col_axis().pos_to_id(c0.saturating_add(dc)) else {
                        return EvalValue::Scalar(EvalScalar::Error(CalcError::Ref));
                    };
                    vals.push(EvalScalar::Bool(
                        workbook.formula(sheet_id, RowId(rid), ColId(cid)).is_some(),
                    ));
                }
            }
            match Array2D::new(rows, cols, vals) {
                Ok(arr) => EvalValue::Array(arr),
                Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    }
}

/// Evaluates `ERROR.TYPE(error_val)`.
pub fn eval_error_type(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |x| match x {
        EvalScalar::Error(e) => match e {
            CalcError::Null => EvalScalar::Number(1.0),
            CalcError::Div0 => EvalScalar::Number(2.0),
            CalcError::Value => EvalScalar::Number(3.0),
            CalcError::Ref => EvalScalar::Number(4.0),
            CalcError::Name => EvalScalar::Number(5.0),
            CalcError::Num => EvalScalar::Number(6.0),
            CalcError::Na => EvalScalar::Number(7.0),
            CalcError::Spill => EvalScalar::Number(9.0),
            CalcError::Calc => EvalScalar::Number(14.0),
            CalcError::Cycle => EvalScalar::Error(CalcError::Na),
        },
        _ => EvalScalar::Error(CalcError::Na),
    })
}

/// Evaluates `TYPE(value)`.
pub fn eval_type(
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
    let scalar_code = |s: &EvalScalar| -> f64 {
        match s {
            EvalScalar::Empty | EvalScalar::Number(_) => 1.0,
            EvalScalar::Text(_) => 2.0,
            EvalScalar::Bool(_) => 4.0,
            EvalScalar::Error(_) => 16.0,
        }
    };
    let code = match arg0 {
        EvalValue::Missing => 1.0,
        EvalValue::Scalar(s) => scalar_code(s),
        EvalValue::Array(_) => 64.0,
        EvalValue::Reference(r) => match r {
            ResolvedRef::Invalid(_) => 16.0,
            ResolvedRef::Cell { sheet, row, col } => {
                scalar_code(&read_cell_effective(workbook, spills, *sheet, *row, *col))
            }
            ResolvedRef::Area { .. }
            | ResolvedRef::FullCols { .. }
            | ResolvedRef::FullRows { .. } => match ref_live_origin_and_size(r, workbook) {
                Err(_) => 16.0,
                Ok((sheet_id, r0, rows, c0, cols)) => {
                    if rows == 1 && cols == 1 {
                        if let Some(sh) = workbook.sheet(sheet_id)
                            && let (Some(rid), Some(cid)) =
                                (sh.row_axis().pos_to_id(r0), sh.col_axis().pos_to_id(c0))
                        {
                            scalar_code(&read_cell_effective(
                                workbook,
                                spills,
                                sheet_id,
                                RowId(rid),
                                ColId(cid),
                            ))
                        } else {
                            16.0
                        }
                    } else {
                        64.0
                    }
                }
            },
        },
    };
    EvalValue::Scalar(EvalScalar::Number(code))
}

/// Evaluates `IFS(cond1, val1, ...)` for array conditions once branch values
/// have been lazily evaluated.
pub fn combine_ifs_array(
    conds: &[ScalarOrArray],
    vals: Vec<Option<EvalValue>>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    let mut out_rows: u32 = 1;
    let mut out_cols: u32 = 1;
    for c in conds {
        let (r, col) = shape_of(c);
        out_rows = out_rows.max(r);
        out_cols = out_cols.max(col);
    }
    let mut coerced_vals: Vec<Option<ScalarOrArray>> = Vec::with_capacity(vals.len());
    for v_opt in vals {
        match v_opt {
            None => coerced_vals.push(None),
            Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(sa) => {
                    let (r, col) = shape_of(&sa);
                    out_rows = out_rows.max(r);
                    out_cols = out_cols.max(col);
                    coerced_vals.push(Some(sa));
                }
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
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
            let mut cell = EvalScalar::Error(CalcError::Na);
            for (i, cond) in conds.iter().enumerate() {
                let cs = get_sa(cond, r, c);
                match cs.to_bool() {
                    Err(e) => {
                        cell = EvalScalar::Error(e);
                        break;
                    }
                    Ok(true) => {
                        cell = match coerced_vals.get(i).and_then(|o| o.as_ref()) {
                            Some(vs) => empty_to_zero(get_sa(vs, r, c)),
                            None => EvalScalar::Number(0.0),
                        };
                        break;
                    }
                    Ok(false) => {}
                }
            }
            out.push(cell);
        }
    }
    match Array2D::new(out_rows, out_cols, out) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Normalizes a chosen scalar result from a lazily-evaluated branch (omitted or
/// blank converts to `0.0` at value position when coerced in scalar op).
#[must_use]
pub fn finalize_branch_value(
    val: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    match val {
        EvalValue::Missing => EvalValue::Scalar(EvalScalar::Number(0.0)),
        EvalValue::Reference(r) => {
            if caller.mode == FormulaMode::Legacy {
                let s = implicit_intersect(EvalValue::Reference(r), workbook, spills, caller);
                EvalValue::Scalar(empty_to_zero(s))
            } else {
                EvalValue::Reference(r)
            }
        }
        other => other,
    }
}
