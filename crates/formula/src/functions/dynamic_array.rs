//! Dynamic-array functions (`17-m3a.md` 5.1 — 13 functions: `TRANSPOSE`,
//! `FILTER`, `SORT`, `SORTBY`, `UNIQUE`, `SEQUENCE`, `RANDARRAY`, `CHOOSECOLS`,
//! `CHOOSEROWS`, `TAKE`, `DROP`, `HSTACK`, `VSTACK`).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::value::CalcError;

use super::trunc_to_isize;
use super::{EvalEnv, empty_to_zero};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ScalarOrArray,
    coerce_for_scalar_op, materialise_reference,
};

/// Materializes any [`EvalValue`] into an explicit [`Array2D`] (`Scalar` -> `1×1`).
pub fn value_to_array2d(
    val: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> Result<Array2D, CalcError> {
    match val {
        EvalValue::Missing => Err(CalcError::Value),
        EvalValue::Scalar(s) => Array2D::new(1, 1, alloc::vec![s]),
        EvalValue::Array(arr) => Ok(arr),
        EvalValue::Reference(r) => match materialise_reference(&r, workbook, spills) {
            Ok(ScalarOrArray::Scalar(s)) => Array2D::new(1, 1, alloc::vec![s]),
            Ok(ScalarOrArray::Array(arr)) => Ok(arr),
            Err(e) => Err(e),
        },
    }
}

fn scalar_num(
    arg: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<f64, CalcError> {
    let sa = coerce_for_scalar_op(arg, workbook, spills, caller)?;
    let ScalarOrArray::Scalar(sc) = sa else {
        return Err(CalcError::Value);
    };
    sc.to_number()
}

fn scalar_bool(
    arg: EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<bool, CalcError> {
    let sa = coerce_for_scalar_op(arg, workbook, spills, caller)?;
    let ScalarOrArray::Scalar(sc) = sa else {
        return Err(CalcError::Value);
    };
    sc.to_bool()
}

fn opt_num(
    opt: Option<EvalValue>,
    default: f64,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<f64, CalcError> {
    match opt {
        None | Some(EvalValue::Missing) => Ok(default),
        Some(v) => scalar_num(v, workbook, spills, caller),
    }
}

fn opt_bool(
    opt: Option<EvalValue>,
    default: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<bool, CalcError> {
    match opt {
        None | Some(EvalValue::Missing) => Ok(default),
        Some(v) => scalar_bool(v, workbook, spills, caller),
    }
}

/// Key used to hash/dedup and compare [`EvalScalar`] in `SORT` / `UNIQUE`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ScalarKey {
    Empty,
    Number(u64),
    TextLower(String),
    Bool(bool),
    Error(CalcError),
}

impl ScalarKey {
    fn from_scalar(s: &EvalScalar) -> Self {
        match s {
            EvalScalar::Empty => Self::Empty,
            EvalScalar::Number(n) => {
                let c = if *n == 0.0 { 0.0 } else { *n };
                Self::Number(c.to_bits())
            }
            EvalScalar::Text(t) => Self::TextLower(t.to_lowercase()),
            EvalScalar::Bool(b) => Self::Bool(*b),
            EvalScalar::Error(e) => Self::Error(*e),
        }
    }
}

/// Excel sort comparison: Numbers < Text < Booleans < Errors, with Empty ALWAYS last!
fn cmp_scalar_sort(a: &EvalScalar, b: &EvalScalar, asc: bool) -> Ordering {
    match (
        matches!(a, EvalScalar::Empty),
        matches!(b, EvalScalar::Empty),
    ) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    let ord = match (a, b) {
        (EvalScalar::Number(x), EvalScalar::Number(y)) => {
            let cx = if *x == 0.0 { 0.0 } else { *x };
            let cy = if *y == 0.0 { 0.0 } else { *y };
            cx.total_cmp(&cy)
        }
        (EvalScalar::Number(_), _) => Ordering::Less,
        (_, EvalScalar::Number(_)) => Ordering::Greater,

        (EvalScalar::Text(x), EvalScalar::Text(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (EvalScalar::Text(_), _) => Ordering::Less,
        (_, EvalScalar::Text(_)) => Ordering::Greater,

        (EvalScalar::Bool(x), EvalScalar::Bool(y)) => x.cmp(y),
        (EvalScalar::Bool(_), _) => Ordering::Less,
        (_, EvalScalar::Bool(_)) => Ordering::Greater,

        (EvalScalar::Error(x), EvalScalar::Error(y)) => x.cmp(y),
        (EvalScalar::Empty, _) | (_, EvalScalar::Empty) => Ordering::Equal,
    };
    if asc { ord } else { ord.reverse() }
}

/// Evaluates `TRANSPOSE(array)`.
pub fn eval_transpose(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.len() != 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let Some(arg0) = args.into_iter().next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let arr = match value_to_array2d(arg0, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let Some(total) = (arr.rows as usize).checked_mul(arr.cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let mut out = Vec::with_capacity(total);
    for c in 0..arr.cols {
        for r in 0..arr.rows {
            out.push(empty_to_zero(arr.get_at(r, c)));
        }
    }
    match Array2D::new(arr.cols, arr.rows, out) {
        Ok(res) => EvalValue::Array(res),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `SEQUENCE(rows, [columns], [start], [step])`.
pub fn eval_sequence(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=4).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(r_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let rows_f = match scalar_num(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let cols_f = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let start = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let step = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let (Ok(ri), Ok(ci)) = (trunc_to_isize(rows_f), trunc_to_isize(cols_f)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    };
    if ri < 1 || ci < 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let (Ok(rows), Ok(cols)) = (u32::try_from(ri), u32::try_from(ci)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let Some(total) = (rows as usize).checked_mul(cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut vals = Vec::with_capacity(total);
    for idx in 0..total {
        #[expect(
            clippy::cast_precision_loss,
            reason = "sequence index <= 1M fits in f64"
        )]
        let k = idx as f64;
        let v = start + k * step;
        if !v.is_finite() {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        }
        vals.push(EvalScalar::number(v));
    }
    match Array2D::new(rows, cols, vals) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `RANDARRAY([rows], [columns], [min], [max], [whole_number])`.
pub fn eval_randarray(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    env: &mut EvalEnv,
) -> EvalValue {
    if args.len() > 5 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let rows_f = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let cols_f = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let min_v = match opt_num(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let max_v = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let whole = match opt_bool(it.next(), false, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let (Ok(ri), Ok(ci)) = (trunc_to_isize(rows_f), trunc_to_isize(cols_f)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    };
    if ri < 1 || ci < 1 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    if min_v > max_v {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let (Ok(rows), Ok(cols)) = (u32::try_from(ri), u32::try_from(ci)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let Some(total) = (rows as usize).checked_mul(cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut vals = Vec::with_capacity(total);
    if whole {
        let lo_f = num::ceil(min_v);
        let hi_f = num::floor(max_v);
        if lo_f != min_v || hi_f != max_v || lo_f > hi_f || lo_f < -9.0e15 || hi_f > 9.0e15 {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
        #[expect(clippy::cast_possible_truncation, reason = "bounded to [-9e15, 9e15]")]
        let (lo_i, hi_i) = (lo_f as i64, hi_f as i64);
        let Some(diff) = hi_i.checked_sub(lo_i) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        };
        let Ok(diff_u64) = u64::try_from(diff) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        };
        let Some(span) = diff_u64.checked_add(1) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        };
        for _ in 0..total {
            let off = env.rng.next_below(span);
            let off_i64 = i64::try_from(off).unwrap_or(0);
            let drawn = lo_i.saturating_add(off_i64);
            #[expect(clippy::cast_precision_loss, reason = "bounded to [-9e15, 9e15]")]
            let df = drawn as f64;
            vals.push(EvalScalar::number(df));
        }
    } else {
        let span = max_v - min_v;
        if !span.is_finite() {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        }
        for _ in 0..total {
            let v = min_v + env.rng.next_f64() * span;
            vals.push(EvalScalar::number(v));
        }
    }
    match Array2D::new(rows, cols, vals) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `FILTER(array, include, [if_empty])`.
pub fn eval_filter(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(arr_arg), Some(inc_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let if_empty = it.next().filter(|v| !matches!(v, EvalValue::Missing));
    let arr = match value_to_array2d(arr_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let inc = match value_to_array2d(inc_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let filter_rows = if inc.cols == 1 && inc.rows == arr.rows {
        true
    } else if inc.rows == 1 && inc.cols == arr.cols {
        false
    } else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };

    let mut mask = Vec::with_capacity(inc.values.len());
    for s in &inc.values {
        match s.to_bool() {
            Ok(b) => mask.push(b),
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }

    if filter_rows {
        let kept: Vec<u32> = mask
            .iter()
            .enumerate()
            .filter_map(|(i, &keep)| keep.then_some(u32::try_from(i).unwrap_or(0)))
            .collect();
        let Ok(out_r) = u32::try_from(kept.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        if out_r == 0 {
            return if_empty.unwrap_or(EvalValue::Scalar(EvalScalar::Error(CalcError::Calc)));
        }
        let mut out = Vec::with_capacity(kept.len().saturating_mul(arr.cols as usize));
        for r in kept {
            for c in 0..arr.cols {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(out_r, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        let kept: Vec<u32> = mask
            .iter()
            .enumerate()
            .filter_map(|(i, &keep)| keep.then_some(u32::try_from(i).unwrap_or(0)))
            .collect();
        let Ok(out_c) = u32::try_from(kept.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        if out_c == 0 {
            return if_empty.unwrap_or(EvalValue::Scalar(EvalScalar::Error(CalcError::Calc)));
        }
        let mut out = Vec::with_capacity((arr.rows as usize).saturating_mul(kept.len()));
        for r in 0..arr.rows {
            for &c in &kept {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, out_c, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
}

/// Evaluates `SORT(array, [sort_index], [sort_order], [by_col])`.
pub fn eval_sort(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=4).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(arr_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let arr = match value_to_array2d(arr_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let idx_f = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ord_f = match opt_num(it.next(), 1.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let by_col = match opt_bool(it.next(), false, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let (Ok(idx_i), Ok(ord_i)) = (trunc_to_isize(idx_f), trunc_to_isize(ord_f)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let asc = match ord_i {
        1 => true,
        -1 => false,
        _ => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
    };
    let axis_limit = if by_col { arr.rows } else { arr.cols };
    if idx_i < 1 || u32::try_from(idx_i).ok().is_none_or(|v| v > axis_limit) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let sort_pos = u32::try_from(idx_i.saturating_sub(1)).unwrap_or(0);

    if !by_col {
        let mut order: Vec<u32> = (0..arr.rows).collect();
        order.sort_by(|&ra, &rb| {
            cmp_scalar_sort(&arr.get_at(ra, sort_pos), &arr.get_at(rb, sort_pos), asc)
        });
        let mut out = Vec::with_capacity(arr.values.len());
        for r in order {
            for c in 0..arr.cols {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        let mut order: Vec<u32> = (0..arr.cols).collect();
        order.sort_by(|&ca, &cb| {
            cmp_scalar_sort(&arr.get_at(sort_pos, ca), &arr.get_at(sort_pos, cb), asc)
        });
        let mut out = Vec::with_capacity(arr.values.len());
        for r in 0..arr.rows {
            for &c in &order {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
}

/// Evaluates `SORTBY(array, by_array1, [sort_order1], ...)`.
pub fn eval_sortby(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.len() < 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(arr_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let arr = match value_to_array2d(arr_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let mut keys: Vec<(Array2D, bool)> = Vec::new();
    while let Some(by_arg) = it.next() {
        let by = match value_to_array2d(by_arg, workbook, spills) {
            Ok(a) => a,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let ord_f = match opt_num(it.next(), 1.0, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let Ok(ord_i) = trunc_to_isize(ord_f) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        let asc = match ord_i {
            1 => true,
            -1 => false,
            _ => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        };
        keys.push((by, asc));
    }

    let Some((first_by, _)) = keys.first() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let sort_rows = if first_by.cols == 1 && first_by.rows == arr.rows {
        true
    } else if first_by.rows == 1 && first_by.cols == arr.cols {
        false
    } else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };

    for (by, _) in &keys {
        let ok = if sort_rows {
            by.cols == 1 && by.rows == arr.rows
        } else {
            by.rows == 1 && by.cols == arr.cols
        };
        if !ok {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
    }

    if sort_rows {
        let mut order: Vec<u32> = (0..arr.rows).collect();
        order.sort_by(|&ra, &rb| {
            for (by, asc) in &keys {
                let ord = cmp_scalar_sort(&by.get_at(ra, 0), &by.get_at(rb, 0), *asc);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        let mut out = Vec::with_capacity(arr.values.len());
        for r in order {
            for c in 0..arr.cols {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        let mut order: Vec<u32> = (0..arr.cols).collect();
        order.sort_by(|&ca, &cb| {
            for (by, asc) in &keys {
                let ord = cmp_scalar_sort(&by.get_at(0, ca), &by.get_at(0, cb), *asc);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        let mut out = Vec::with_capacity(arr.values.len());
        for r in 0..arr.rows {
            for &c in &order {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
}

/// Evaluates `UNIQUE(array, [by_col], [exactly_once])`.
pub fn eval_unique(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(arr_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let arr = match value_to_array2d(arr_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let by_col = match opt_bool(it.next(), false, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let exactly_once = match opt_bool(it.next(), false, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let outer = if by_col { arr.cols } else { arr.rows };
    let inner = if by_col { arr.rows } else { arr.cols };

    let mut counts: alloc::collections::BTreeMap<Vec<ScalarKey>, usize> =
        alloc::collections::BTreeMap::new();
    let mut keys: Vec<Vec<ScalarKey>> = Vec::with_capacity(outer as usize);
    for i in 0..outer {
        let mut k = Vec::with_capacity(inner as usize);
        for j in 0..inner {
            let s = if by_col {
                arr.get_at(j, i)
            } else {
                arr.get_at(i, j)
            };
            k.push(ScalarKey::from_scalar(&s));
        }
        let c = counts.entry(k.clone()).or_insert(0);
        *c = c.saturating_add(1);
        keys.push(k);
    }

    let mut seen: alloc::collections::BTreeSet<Vec<ScalarKey>> =
        alloc::collections::BTreeSet::new();
    let mut kept_indices = Vec::new();
    for (i, k) in keys.into_iter().enumerate() {
        let cnt = counts.get(&k).copied().unwrap_or(0);
        if exactly_once {
            if cnt == 1 {
                kept_indices.push(u32::try_from(i).unwrap_or(0));
            }
        } else if seen.insert(k) {
            kept_indices.push(u32::try_from(i).unwrap_or(0));
        }
    }

    if kept_indices.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }

    if !by_col {
        let Ok(out_r) = u32::try_from(kept_indices.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        let mut out = Vec::with_capacity(kept_indices.len().saturating_mul(arr.cols as usize));
        for r in kept_indices {
            for c in 0..arr.cols {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(out_r, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        let Ok(out_c) = u32::try_from(kept_indices.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        let mut out = Vec::with_capacity((arr.rows as usize).saturating_mul(kept_indices.len()));
        for r in 0..arr.rows {
            for &c in &kept_indices {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, out_c, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
}

/// Evaluates `CHOOSECOLS` (`is_rows == false`) or `CHOOSEROWS` (`is_rows == true`).
pub fn eval_choose_slice(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_rows: bool,
) -> EvalValue {
    if args.len() < 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(arr_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let arr = match value_to_array2d(arr_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let limit = if is_rows { arr.rows } else { arr.cols };
    let Ok(limit_i) = isize::try_from(limit) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let mut chosen = Vec::new();
    for idx_arg in it {
        let idx_f = match scalar_num(idx_arg, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let Ok(idx) = trunc_to_isize(idx_f) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        if idx == 0 || idx > limit_i || idx < limit_i.saturating_neg() {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
        let zero_based = if idx > 0 {
            idx.saturating_sub(1)
        } else {
            limit_i.saturating_add(idx)
        };
        chosen.push(u32::try_from(zero_based).unwrap_or(0));
    }
    if chosen.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    if is_rows {
        let Ok(out_r) = u32::try_from(chosen.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        let Some(total) = chosen.len().checked_mul(arr.cols as usize) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        if total > MAX_ARRAY_CELLS {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        }
        let mut out = Vec::with_capacity(total);
        for r in chosen {
            for c in 0..arr.cols {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(out_r, arr.cols, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        let Ok(out_c) = u32::try_from(chosen.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        let Some(total) = (arr.rows as usize).checked_mul(chosen.len()) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        if total > MAX_ARRAY_CELLS {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        }
        let mut out = Vec::with_capacity(total);
        for r in 0..arr.rows {
            for &c in &chosen {
                out.push(empty_to_zero(arr.get_at(r, c)));
            }
        }
        match Array2D::new(arr.rows, out_c, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
}

fn slice_take_bounds(total: u32, spec: isize) -> Result<(u32, u32), CalcError> {
    if spec == 0 {
        return Err(CalcError::Calc);
    }
    let abs_u = spec.unsigned_abs();
    let count = u32::try_from(abs_u).unwrap_or(total).min(total);
    if spec > 0 {
        Ok((0, count))
    } else {
        Ok((total.saturating_sub(count), total))
    }
}

fn slice_drop_bounds(total: u32, spec: isize) -> Result<(u32, u32), CalcError> {
    let abs_u = spec.unsigned_abs();
    let drop_n = u32::try_from(abs_u).unwrap_or(total).min(total);
    let (s, e) = if spec >= 0 {
        (drop_n, total)
    } else {
        (0, total.saturating_sub(drop_n))
    };
    if s >= e {
        Err(CalcError::Calc)
    } else {
        Ok((s, e))
    }
}

/// Evaluates `TAKE` (`is_drop == false`) or `DROP` (`is_drop == true`).
pub fn eval_take_or_drop(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_drop: bool,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(arr_arg), Some(r_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let c_arg = it.next().unwrap_or(EvalValue::Missing);
    let r_missing = matches!(r_arg, EvalValue::Missing);
    let c_missing = matches!(c_arg, EvalValue::Missing);
    if r_missing && c_missing {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }

    let arr = match value_to_array2d(arr_arg, workbook, spills) {
        Ok(a) => a,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let (r_start, r_end) = if r_missing {
        (0, arr.rows)
    } else {
        let rf = match scalar_num(r_arg, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let Ok(ri) = trunc_to_isize(rf) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        match if is_drop {
            slice_drop_bounds(arr.rows, ri)
        } else {
            slice_take_bounds(arr.rows, ri)
        } {
            Ok(b) => b,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    };

    let (c_start, c_end) = if c_missing {
        (0, arr.cols)
    } else {
        let cf = match scalar_num(c_arg, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        };
        let Ok(ci) = trunc_to_isize(cf) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        };
        match if is_drop {
            slice_drop_bounds(arr.cols, ci)
        } else {
            slice_take_bounds(arr.cols, ci)
        } {
            Ok(b) => b,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    };

    let out_r = r_end.saturating_sub(r_start);
    let out_c = c_end.saturating_sub(c_start);
    let Some(total) = (out_r as usize).checked_mul(out_c as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let mut out = Vec::with_capacity(total);
    for r in r_start..r_end {
        for c in c_start..c_end {
            out.push(empty_to_zero(arr.get_at(r, c)));
        }
    }
    match Array2D::new(out_r, out_c, out) {
        Ok(res) => EvalValue::Array(res),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `HSTACK` (`horizontal == true`) or `VSTACK` (`horizontal == false`).
pub fn eval_stack(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    horizontal: bool,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut pieces: Vec<Array2D> = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            EvalValue::Missing => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
            other => match value_to_array2d(other, workbook, spills) {
                Ok(a) => pieces.push(a),
                Err(e) => pieces.push(Array2D {
                    rows: 1,
                    cols: 1,
                    values: vec![EvalScalar::Error(e)],
                }),
            },
        }
    }

    if horizontal {
        let out_r = pieces.iter().map(|p| p.rows).max().unwrap_or(1);
        let mut out_c: u32 = 0;
        for p in &pieces {
            out_c = match out_c.checked_add(p.cols) {
                Some(v) => v,
                None => return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc)),
            };
        }
        let Some(total) = (out_r as usize).checked_mul(out_c as usize) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        if total > MAX_ARRAY_CELLS {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        }
        let mut out = Vec::with_capacity(total);
        for r in 0..out_r {
            for p in &pieces {
                for c in 0..p.cols {
                    if r < p.rows {
                        out.push(empty_to_zero(p.get_at(r, c)));
                    } else {
                        out.push(EvalScalar::Error(CalcError::Na));
                    }
                }
            }
        }
        match Array2D::new(out_r, out_c, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    } else {
        let out_c = pieces.iter().map(|p| p.cols).max().unwrap_or(1);
        let mut out_r: u32 = 0;
        for p in &pieces {
            out_r = match out_r.checked_add(p.rows) {
                Some(v) => v,
                None => return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc)),
            };
        }
        let Some(total) = (out_r as usize).checked_mul(out_c as usize) else {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        };
        if total > MAX_ARRAY_CELLS {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
        }
        let mut out = Vec::with_capacity(total);
        for p in &pieces {
            for r in 0..p.rows {
                for c in 0..out_c {
                    if c < p.cols {
                        out.push(empty_to_zero(p.get_at(r, c)));
                    } else {
                        out.push(EvalScalar::Error(CalcError::Na));
                    }
                }
            }
        }
        match Array2D::new(out_r, out_c, out) {
            Ok(res) => EvalValue::Array(res),
            Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
}
