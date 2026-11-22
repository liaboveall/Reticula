//! Statistical functions (`17-m3a.md` 5.1 — 16 unique functions + 6 aliases).

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::value::{CalcError, Value};

use super::{RefCellVal, trunc_to_isize, visit_numeric_arg, visit_reference_cells};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ScalarOrArray,
    coerce_for_scalar_op,
};

fn canon_f64(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
}

fn collect_numbers_from_args(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> Result<Vec<f64>, CalcError> {
    let mut vals = Vec::new();
    for arg in args {
        visit_numeric_arg(arg, workbook, spills, |n| {
            vals.push(canon_f64(n));
            Ok(())
        })?;
    }
    Ok(vals)
}

/// Collects numbers ONLY inside `Reference` / `Array` (or numeric single scalar)
/// ignoring text/bool/empty inside ranges/arrays, for `array`-typed parameters.
fn collect_array_numbers(
    arg: &EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> Result<Vec<f64>, CalcError> {
    let mut vals = Vec::new();
    match arg {
        EvalValue::Missing => return Err(CalcError::Value),
        EvalValue::Scalar(s) => match s {
            EvalScalar::Error(e) => return Err(*e),
            EvalScalar::Number(n) => vals.push(canon_f64(*n)),
            EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => {}
        },
        EvalValue::Array(arr) => {
            for item in &arr.values {
                match item {
                    EvalScalar::Error(e) => return Err(*e),
                    EvalScalar::Number(n) => vals.push(canon_f64(*n)),
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => {}
                }
            }
        }
        EvalValue::Reference(r) => {
            visit_reference_cells(r, workbook, spills, |_sheet, cell| match cell {
                RefCellVal::Word(bits) => match Value::unpack(bits) {
                    Value::Error(e) => Err(e),
                    Value::Number(n) => {
                        vals.push(canon_f64(n));
                        Ok(())
                    }
                    Value::Empty | Value::Text(_) | Value::Bool(_) | Value::Array(_) => Ok(()),
                },
                RefCellVal::Shadow(s) => match s {
                    EvalScalar::Error(e) => Err(*e),
                    EvalScalar::Number(n) => {
                        vals.push(canon_f64(*n));
                        Ok(())
                    }
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => Ok(()),
                },
            })?;
        }
    }
    Ok(vals)
}

fn collect_raw_cells(
    arg: &EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> Result<Vec<EvalScalar>, CalcError> {
    match arg {
        EvalValue::Missing => Err(CalcError::Value),
        EvalValue::Scalar(s) => Ok(alloc::vec![s.clone()]),
        EvalValue::Array(arr) => Ok(arr.values.clone()),
        EvalValue::Reference(r) => {
            let mut out = Vec::new();
            visit_reference_cells(r, workbook, spills, |sheet, cell| {
                let sc = match cell {
                    RefCellVal::Word(bits) => {
                        EvalScalar::from_sheet_value(sheet, Value::unpack(bits))
                    }
                    RefCellVal::Shadow(s) => s.clone(),
                };
                out.push(sc);
                Ok(())
            })?;
            Ok(out)
        }
    }
}

/// Evaluates `MEDIAN(number1, [number2], ...)`.
pub fn eval_median(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut vals = match collect_numbers_from_args(args, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if vals.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    vals.sort_by(f64::total_cmp);
    let mid = vals.len() / 2;
    let res = if vals.len() % 2 == 1 {
        vals.get(mid).copied().unwrap_or(0.0)
    } else {
        let a = vals.get(mid.saturating_sub(1)).copied().unwrap_or(0.0);
        let b = vals.get(mid).copied().unwrap_or(0.0);
        (a + b) * 0.5
    };
    EvalValue::Scalar(EvalScalar::number(res))
}

/// Evaluates `MODE.SNGL` / `MODE(number1, [number2], ...)`.
pub fn eval_mode_sngl(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let vals = match collect_numbers_from_args(args, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    let mut first_order: Vec<u64> = Vec::new();
    for v in vals {
        let bits = canon_f64(v).to_bits();
        let entry = counts.entry(bits).or_insert_with(|| {
            first_order.push(bits);
            0
        });
        *entry = entry.saturating_add(1);
    }
    let mut best_bits: Option<u64> = None;
    let mut best_count: usize = 1;
    for bits in first_order {
        let cnt = counts.get(&bits).copied().unwrap_or(0);
        if cnt > best_count {
            best_count = cnt;
            best_bits = Some(bits);
        }
    }
    match best_bits {
        Some(bits) => EvalValue::Scalar(EvalScalar::number(f64::from_bits(bits))),
        None => EvalValue::Scalar(EvalScalar::Error(CalcError::Na)),
    }
}

/// Evaluates `LARGE(array, k)` (`is_large == true`) or `SMALL(array, k)`.
pub fn eval_large_or_small(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_large: bool,
) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(arr_arg), Some(k_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let mut vals = match collect_array_numbers(&arr_arg, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    vals.sort_by(f64::total_cmp);

    let k_sa = match coerce_for_scalar_op(k_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pick = |ks: &EvalScalar| -> EvalScalar {
        let k_f = match ks.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if k_f < 1.0 || vals.is_empty() {
            return EvalScalar::Error(CalcError::Num);
        }
        let k_i = match trunc_to_isize(k_f) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let Ok(k_u) = usize::try_from(k_i) else {
            return EvalScalar::Error(CalcError::Num);
        };
        if k_u == 0 || k_u > vals.len() {
            return EvalScalar::Error(CalcError::Num);
        }
        let idx = if is_large {
            vals.len().saturating_sub(k_u)
        } else {
            k_u.saturating_sub(1)
        };
        vals.get(idx)
            .copied()
            .map_or(EvalScalar::Error(CalcError::Num), EvalScalar::number)
    };
    match k_sa {
        ScalarOrArray::Scalar(ks) => EvalValue::Scalar(pick(&ks)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(pick).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

/// Evaluates `RANK.EQ` / `RANK` (`avg == false`) or `RANK.AVG` (`avg == true`).
pub fn eval_rank(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    avg: bool,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(num_arg), Some(ref_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let asc = match it.next() {
        None | Some(EvalValue::Missing) => false,
        Some(ord_arg) => {
            let sa = match coerce_for_scalar_op(ord_arg, workbook, spills, caller) {
                Ok(v) => v,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_number() {
                Ok(n) => n != 0.0,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };
    let vals = match collect_array_numbers(&ref_arg, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let num_sa = match coerce_for_scalar_op(num_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let rank_one = |xs: &EvalScalar| -> EvalScalar {
        let x = match xs.to_number() {
            Ok(v) => canon_f64(v),
            Err(e) => return EvalScalar::Error(e),
        };
        let mut before: usize = 0;
        let mut equal: usize = 0;
        for &v in &vals {
            if v == x {
                equal = equal.saturating_add(1);
            } else if (asc && v < x) || (!asc && v > x) {
                before = before.saturating_add(1);
            }
        }
        if equal == 0 {
            return EvalScalar::Error(CalcError::Na);
        }
        #[expect(clippy::cast_precision_loss, reason = "list count <= 1M fits in f64")]
        let res = if avg && equal > 1 {
            (before as f64) + 1.0 + ((equal.saturating_sub(1)) as f64) * 0.5
        } else {
            (before.saturating_add(1)) as f64
        };
        EvalScalar::number(res)
    };
    match num_sa {
        ScalarOrArray::Scalar(xs) => EvalValue::Scalar(rank_one(&xs)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(rank_one).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn percentile_inc_sorted(sorted: &[f64], k: f64) -> Result<f64, CalcError> {
    if sorted.is_empty() || !(0.0..=1.0).contains(&k) {
        return Err(CalcError::Num);
    }
    let last = sorted.len().saturating_sub(1);
    #[expect(clippy::cast_precision_loss, reason = "array length fits in f64")]
    let idx = k * (last as f64);
    #[expect(clippy::cast_possible_truncation, reason = "idx <= sorted.len() - 1")]
    #[expect(clippy::cast_sign_loss, reason = "idx >= 0")]
    let lo = num::floor(idx) as usize;
    let hi = (lo.saturating_add(1)).min(last);
    #[expect(clippy::cast_precision_loss, reason = "lo <= sorted.len() - 1")]
    let frac = idx - (lo as f64);
    let vlo = sorted.get(lo).copied().unwrap_or(0.0);
    let vhi = sorted.get(hi).copied().unwrap_or(vlo);
    Ok(vlo + frac * (vhi - vlo))
}

fn percentile_exc_sorted(sorted: &[f64], k: f64) -> Result<f64, CalcError> {
    let n = sorted.len();
    if n == 0 || k <= 0.0 || k >= 1.0 {
        return Err(CalcError::Num);
    }
    #[expect(clippy::cast_precision_loss, reason = "array length fits in f64")]
    let nf = n as f64;
    let pos1 = k * (nf + 1.0);
    if pos1 < 1.0 || pos1 > nf {
        return Err(CalcError::Num);
    }
    let idx0 = pos1 - 1.0;
    #[expect(clippy::cast_possible_truncation, reason = "0 <= idx0 <= n - 1")]
    #[expect(clippy::cast_sign_loss, reason = "idx0 >= 0")]
    let lo = num::floor(idx0) as usize;
    let hi = (lo.saturating_add(1)).min(n.saturating_sub(1));
    #[expect(clippy::cast_precision_loss, reason = "lo <= n - 1")]
    let frac = idx0 - (lo as f64);
    let vlo = sorted.get(lo).copied().unwrap_or(0.0);
    let vhi = sorted.get(hi).copied().unwrap_or(vlo);
    Ok(vlo + frac * (vhi - vlo))
}

/// Evaluates `PERCENTILE.INC` / `PERCENTILE` (`exc == false`) or `PERCENTILE.EXC` (`exc == true`).
pub fn eval_percentile(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    exc: bool,
) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(arr_arg), Some(k_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let mut sorted = match collect_array_numbers(&arr_arg, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    sorted.sort_by(f64::total_cmp);
    let k_sa = match coerce_for_scalar_op(k_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let calc = |ks: &EvalScalar| -> EvalScalar {
        let k = match ks.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let res = if exc {
            percentile_exc_sorted(&sorted, k)
        } else {
            percentile_inc_sorted(&sorted, k)
        };
        res.map_or_else(EvalScalar::Error, EvalScalar::number)
    };
    match k_sa {
        ScalarOrArray::Scalar(ks) => EvalValue::Scalar(calc(&ks)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(calc).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

/// Evaluates `QUARTILE.INC` / `QUARTILE` (`exc == false`) or `QUARTILE.EXC` (`exc == true`).
pub fn eval_quartile(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    exc: bool,
) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(arr_arg), Some(q_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let mut sorted = match collect_array_numbers(&arr_arg, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    sorted.sort_by(f64::total_cmp);
    let q_sa = match coerce_for_scalar_op(q_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let calc = |qs: &EvalScalar| -> EvalScalar {
        let qf = match qs.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let Ok(qi) = trunc_to_isize(qf) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let valid = if exc {
            (1..=3).contains(&qi)
        } else {
            (0..=4).contains(&qi)
        };
        if !valid {
            return EvalScalar::Error(CalcError::Num);
        }
        #[expect(clippy::cast_precision_loss, reason = "qi in 0..=4")]
        let k = (qi as f64) * 0.25;
        let res = if exc {
            percentile_exc_sorted(&sorted, k)
        } else {
            percentile_inc_sorted(&sorted, k)
        };
        res.map_or_else(EvalScalar::Error, EvalScalar::number)
    };
    match q_sa {
        ScalarOrArray::Scalar(qs) => EvalValue::Scalar(calc(&qs)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(calc).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

/// Evaluates `VAR.S` / `VAR` / `VAR.P` / `STDEV.S` / `STDEV` / `STDEV.P`.
pub fn eval_var_or_stdev(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    is_sample: bool,
    is_stdev: bool,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let vals = match collect_numbers_from_args(args, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let n = vals.len();
    let min_n = if is_sample { 2 } else { 1 };
    if n < min_n {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Div0));
    }
    #[expect(clippy::cast_precision_loss, reason = "sample size <= 1M fits in f64")]
    let nf = n as f64;
    let sum: f64 = vals.iter().copied().sum();
    if !sum.is_finite() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let mean = sum / nf;
    let mut ss = 0.0_f64;
    for v in vals {
        let d = v - mean;
        ss += d * d;
    }
    if !ss.is_finite() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    #[expect(clippy::cast_precision_loss, reason = "sample size fits in f64")]
    let denom = if is_sample {
        n.saturating_sub(1) as f64
    } else {
        nf
    };
    let var = ss / denom;
    let res = if is_stdev { num::sqrt(var) } else { var };
    EvalValue::Scalar(EvalScalar::number(res))
}

/// Evaluates `CORREL(array1, array2)`.
pub fn eval_correl(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    let (Some(a0), Some(a1)) = (args.first(), args.get(1)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let c1 = match collect_raw_cells(a0, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let c2 = match collect_raw_cells(a1, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if c1.len() != c2.len() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Na));
    }
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for (x, y) in c1.iter().zip(c2.iter()) {
        if let EvalScalar::Error(e) = x {
            return EvalValue::Scalar(EvalScalar::Error(*e));
        }
        if let EvalScalar::Error(e) = y {
            return EvalValue::Scalar(EvalScalar::Error(*e));
        }
        if let (EvalScalar::Number(nx), EvalScalar::Number(ny)) = (x, y) {
            xs.push(*nx);
            ys.push(*ny);
        }
    }
    let n = xs.len();
    if n == 0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Div0));
    }
    #[expect(clippy::cast_precision_loss, reason = "sample count fits in f64")]
    let nf = n as f64;
    let mx: f64 = xs.iter().copied().sum::<f64>() / nf;
    let my: f64 = ys.iter().copied().sum::<f64>() / nf;
    let mut sxy = 0.0_f64;
    let mut sxx = 0.0_f64;
    let mut syy = 0.0_f64;
    for (&x, &y) in xs.iter().zip(ys.iter()) {
        let dx = x - mx;
        let dy = y - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    let denom = num::sqrt(sxx * syy);
    if denom == 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Div0));
    }
    let r = (sxy / denom).clamp(-1.0, 1.0);
    EvalValue::Scalar(EvalScalar::number(r))
}

/// Computes quantile / median / mode / large / small / stdev / var for
/// `SUBTOTAL` and `AGGREGATE` on pre-filtered numeric values `vals`.
pub(crate) fn aggregate_stat_code(code: isize, vals: &[f64], k_opt: Option<f64>) -> EvalScalar {
    match code {
        7 | 8 | 10 | 11 => {
            let is_sample = matches!(code, 7 | 10);
            let is_stdev = matches!(code, 7 | 8);
            let n = vals.len();
            if n < if is_sample { 2 } else { 1 } {
                return EvalScalar::Error(CalcError::Div0);
            }
            #[expect(clippy::cast_precision_loss, reason = "sample count fits in f64")]
            let nf = n as f64;
            let mean: f64 = vals.iter().copied().sum::<f64>() / nf;
            let ss: f64 = vals.iter().map(|&v| (v - mean) * (v - mean)).sum();
            #[expect(clippy::cast_precision_loss, reason = "sample count fits in f64")]
            let denom = if is_sample {
                n.saturating_sub(1) as f64
            } else {
                nf
            };
            let var = ss / denom;
            EvalScalar::number(if is_stdev { num::sqrt(var) } else { var })
        }
        12 => {
            if vals.is_empty() {
                return EvalScalar::Error(CalcError::Num);
            }
            let mut sorted = vals.to_vec();
            sorted.sort_by(f64::total_cmp);
            let mid = sorted.len() / 2;
            let res = if sorted.len() % 2 == 1 {
                sorted.get(mid).copied().unwrap_or(0.0)
            } else {
                let a = sorted.get(mid.saturating_sub(1)).copied().unwrap_or(0.0);
                let b = sorted.get(mid).copied().unwrap_or(0.0);
                (a + b) * 0.5
            };
            EvalScalar::number(res)
        }
        13 => {
            let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
            let mut order: Vec<u64> = Vec::new();
            for &v in vals {
                let bits = canon_f64(v).to_bits();
                let c = counts.entry(bits).or_insert_with(|| {
                    order.push(bits);
                    0
                });
                *c = c.saturating_add(1);
            }
            let mut best: Option<u64> = None;
            let mut best_c = 1_usize;
            for bits in order {
                let c = counts.get(&bits).copied().unwrap_or(0);
                if c > best_c {
                    best_c = c;
                    best = Some(bits);
                }
            }
            best.map_or(EvalScalar::Error(CalcError::Na), |bits| {
                EvalScalar::number(f64::from_bits(bits))
            })
        }
        14 | 15 => {
            let Some(kf) = k_opt else {
                return EvalScalar::Error(CalcError::Value);
            };
            if kf < 1.0 || vals.is_empty() {
                return EvalScalar::Error(CalcError::Num);
            }
            let Ok(ki) = trunc_to_isize(kf) else {
                return EvalScalar::Error(CalcError::Num);
            };
            let Ok(ku) = usize::try_from(ki) else {
                return EvalScalar::Error(CalcError::Num);
            };
            if ku == 0 || ku > vals.len() {
                return EvalScalar::Error(CalcError::Num);
            }
            let mut sorted = vals.to_vec();
            sorted.sort_by(f64::total_cmp);
            let idx = if code == 14 {
                sorted.len().saturating_sub(ku)
            } else {
                ku.saturating_sub(1)
            };
            sorted
                .get(idx)
                .copied()
                .map_or(EvalScalar::Error(CalcError::Num), EvalScalar::number)
        }
        16 | 18 => {
            let Some(k) = k_opt else {
                return EvalScalar::Error(CalcError::Value);
            };
            let mut sorted = vals.to_vec();
            sorted.sort_by(f64::total_cmp);
            let res = if code == 16 {
                percentile_inc_sorted(&sorted, k)
            } else {
                percentile_exc_sorted(&sorted, k)
            };
            res.map_or_else(EvalScalar::Error, EvalScalar::number)
        }
        17 | 19 => {
            let Some(qf) = k_opt else {
                return EvalScalar::Error(CalcError::Value);
            };
            let Ok(qi) = trunc_to_isize(qf) else {
                return EvalScalar::Error(CalcError::Num);
            };
            let valid = if code == 19 {
                (1..=3).contains(&qi)
            } else {
                (0..=4).contains(&qi)
            };
            if !valid {
                return EvalScalar::Error(CalcError::Num);
            }
            let mut sorted = vals.to_vec();
            sorted.sort_by(f64::total_cmp);
            #[expect(clippy::cast_precision_loss, reason = "qi in 0..=4")]
            let k = (qi as f64) * 0.25;
            let res = if code == 19 {
                percentile_exc_sorted(&sorted, k)
            } else {
                percentile_inc_sorted(&sorted, k)
            };
            res.map_or_else(EvalScalar::Error, EvalScalar::number)
        }
        _ => EvalScalar::Error(CalcError::Value),
    }
}

/// Evaluates `FREQUENCY(data_array, bins_array)`.
pub fn eval_frequency(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    let (Some(a0), Some(a1)) = (args.first(), args.get(1)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let data = match collect_array_numbers(a0, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let mut bins = match collect_array_numbers(a1, workbook, spills) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if bins.len() >= MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    bins.sort_by(f64::total_cmp);
    let mut counts = vec![0_u64; bins.len().saturating_add(1)];
    for x in data {
        let slot = bins.partition_point(|&b| b < x);
        if let Some(c) = counts.get_mut(slot) {
            *c = c.saturating_add(1);
        }
    }
    let Ok(rows) = u32::try_from(counts.len()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let values = counts
        .into_iter()
        .map(|c| {
            #[expect(clippy::cast_precision_loss, reason = "cell count fits in f64")]
            EvalScalar::Number(c as f64)
        })
        .collect();
    match Array2D::new(rows, 1, values) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}
