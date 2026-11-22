//! Financial functions (`17-m3a.md` 5.1 — 8 functions: `PMT`, `PV`, `FV`, `NPER`,
//! `RATE`, `NPV`, `IRR`, `SLN`).
//!
//! Iterative solvers (`RATE`, `IRR`) use deterministic Newton-Raphson iterations
//! backed by bounded bisection fallback to guarantee identical cross-platform
//! results (D-021).

use alloc::vec::Vec;

use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::value::{CalcError, Value};

use super::{RefCellVal, visit_numeric_arg, visit_reference_cells};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    CallerContext, EvalScalar, EvalValue, ScalarOrArray, coerce_for_scalar_op,
};

fn read_scalar_num_arg(
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

fn read_opt_num_arg(
    opt: Option<EvalValue>,
    default: f64,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<f64, CalcError> {
    match opt {
        None | Some(EvalValue::Missing) => Ok(default),
        Some(v) => read_scalar_num_arg(v, workbook, spills, caller),
    }
}

fn fin_result(v: f64) -> EvalValue {
    if v.is_finite() {
        EvalValue::Scalar(EvalScalar::number(v))
    } else {
        EvalValue::Scalar(EvalScalar::Error(CalcError::Num))
    }
}

/// Evaluates Excel annuity residual:
/// `pv * (1+r)^n + pmt * (1+r*t) * (((1+r)^n - 1) / r) + fv`.
fn annuity_eval(rate: f64, nper: f64, pmt: f64, pv: f64, fv: f64, ptype: f64) -> Option<f64> {
    if rate <= -1.0 {
        return None;
    }
    if rate.abs() < 1e-11 {
        let g = 1.0 + nper * rate + 0.5 * nper * (nper - 1.0) * rate * rate;
        let annuity_factor = nper + 0.5 * nper * (nper - 1.0) * rate;
        let res = pv * g + pmt * (1.0 + rate * ptype) * annuity_factor + fv;
        res.is_finite().then_some(res)
    } else {
        let g = num::pow(1.0 + rate, nper);
        if !g.is_finite() {
            return None;
        }
        let res = pv * g + pmt * (1.0 + rate * ptype) * ((g - 1.0) / rate) + fv;
        res.is_finite().then_some(res)
    }
}

/// Evaluates `PMT(rate, nper, pv, [fv], [type])`.
pub fn eval_pmt(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(3..=5).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(r_arg), Some(n_arg), Some(pv_arg)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let rate = match read_scalar_num_arg(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let nper = match read_scalar_num_arg(n_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pv = match read_scalar_num_arg(pv_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let fv = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype_raw = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype = if ptype_raw == 0.0 { 0.0 } else { 1.0 };

    if nper == 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    if rate == 0.0 {
        return fin_result(-(pv + fv) / nper);
    }
    if rate <= -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let g = num::pow(1.0 + rate, nper);
    let denom = (1.0 + rate * ptype) * (g - 1.0);
    if !g.is_finite() || denom == 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    fin_result(-rate * (pv * g + fv) / denom)
}

/// Evaluates `PV(rate, nper, pmt, [fv], [type])`.
pub fn eval_pv(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(3..=5).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(r_arg), Some(n_arg), Some(pmt_arg)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let rate = match read_scalar_num_arg(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let nper = match read_scalar_num_arg(n_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pmt = match read_scalar_num_arg(pmt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let fv = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype_raw = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype = if ptype_raw == 0.0 { 0.0 } else { 1.0 };

    if rate == 0.0 {
        return fin_result(-(fv + pmt * nper));
    }
    if rate <= -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let g = num::pow(1.0 + rate, nper);
    if !g.is_finite() || g == 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    fin_result(-(fv + pmt * (1.0 + rate * ptype) * ((g - 1.0) / rate)) / g)
}

/// Evaluates `FV(rate, nper, pmt, [pv], [type])`.
pub fn eval_fv(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(3..=5).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(r_arg), Some(n_arg), Some(pmt_arg)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let rate = match read_scalar_num_arg(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let nper = match read_scalar_num_arg(n_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pmt = match read_scalar_num_arg(pmt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pv = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype_raw = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype = if ptype_raw == 0.0 { 0.0 } else { 1.0 };

    if rate == 0.0 {
        return fin_result(-(pv + pmt * nper));
    }
    if rate <= -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let g = num::pow(1.0 + rate, nper);
    if !g.is_finite() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    fin_result(-(pv * g + pmt * (1.0 + rate * ptype) * ((g - 1.0) / rate)))
}

/// Evaluates `NPER(rate, pmt, pv, [fv], [type])`.
pub fn eval_nper(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(3..=5).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(r_arg), Some(pmt_arg), Some(pv_arg)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let rate = match read_scalar_num_arg(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pmt = match read_scalar_num_arg(pmt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pv = match read_scalar_num_arg(pv_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let fv = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype_raw = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype = if ptype_raw == 0.0 { 0.0 } else { 1.0 };

    if rate == 0.0 {
        if pmt == 0.0 {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        }
        return fin_result(-(pv + fv) / pmt);
    }
    if rate <= -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let z = pmt * (1.0 + rate * ptype) / rate;
    let denom = pv + z;
    if denom == 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let ratio = (z - fv) / denom;
    if ratio <= 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    fin_result(num::ln(ratio) / num::ln(1.0 + rate))
}

/// Evaluates `RATE(nper, pmt, pv, [fv], [type], [guess])`.
pub fn eval_rate(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(3..=6).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(n_arg), Some(pmt_arg), Some(pv_arg)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let nper = match read_scalar_num_arg(n_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pmt = match read_scalar_num_arg(pmt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pv = match read_scalar_num_arg(pv_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let fv = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype_raw = match read_opt_num_arg(it.next(), 0.0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let guess = match read_opt_num_arg(it.next(), 0.1, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ptype = if ptype_raw == 0.0 { 0.0 } else { 1.0 };

    if nper <= 0.0 || guess <= -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }

    let eval_f = |r: f64| annuity_eval(r, nper, pmt, pv, fv, ptype);

    // 1. Deterministic Newton-Raphson from `guess`.
    let mut r = guess;
    for _ in 0..80 {
        let Some(f0) = eval_f(r) else {
            break;
        };
        if f0.abs() < 1e-11 {
            return fin_result(r);
        }
        let h = (r.abs() * 1e-6).max(1e-7);
        let Some(f1) = eval_f(r + h) else {
            break;
        };
        let deriv = (f1 - f0) / h;
        if !deriv.is_finite() || deriv.abs() < 1e-18 {
            break;
        }
        let next = (r - f0 / deriv).clamp(-0.999_999, 1e6);
        let delta = (next - r).abs();
        r = next;
        if delta < 1e-12
            && let Some(chk) = eval_f(r)
            && chk.abs() < 1e-7
        {
            return fin_result(r);
        }
    }

    // 2. Deterministic grid bracket + bisection fallback.
    let grid: &[f64] = &[
        -0.99, -0.9, -0.5, -0.2, -0.05, 0.0, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 25.0,
    ];
    for pair in grid.windows(2) {
        let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let (Some(fa), Some(fb)) = (eval_f(a), eval_f(b)) else {
            continue;
        };
        if fa == 0.0 {
            return fin_result(a);
        }
        if (fa < 0.0 && fb > 0.0) || (fa > 0.0 && fb < 0.0) {
            let mut lo = a;
            let mut hi = b;
            let mut flo = fa;
            for _ in 0..100 {
                let mid = 0.5 * (lo + hi);
                let Some(fmid) = eval_f(mid) else {
                    break;
                };
                if fmid == 0.0 || (hi - lo) * 0.5 < 1e-12 {
                    return fin_result(mid);
                }
                if (flo < 0.0 && fmid < 0.0) || (flo > 0.0 && fmid > 0.0) {
                    lo = mid;
                    flo = fmid;
                } else {
                    hi = mid;
                }
            }
            return fin_result(0.5 * (lo + hi));
        }
    }

    EvalValue::Scalar(EvalScalar::Error(CalcError::Num))
}

/// Evaluates `NPV(rate, value1, [value2], ...)`.
pub fn eval_npv(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.len() < 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(r_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let rate = match read_scalar_num_arg(r_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if rate == -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Div0));
    }
    let factor = 1.0 + rate;
    let mut sum = 0.0_f64;
    let mut period: i32 = 1;
    let rest: Vec<EvalValue> = it.collect();
    for arg in &rest {
        if let Err(e) = visit_numeric_arg(arg, workbook, spills, |cf| {
            let denom = num::pow(factor, f64::from(period));
            if denom == 0.0 {
                return Err(CalcError::Div0);
            }
            sum += cf / denom;
            if !sum.is_finite() {
                return Err(CalcError::Num);
            }
            period = period.saturating_add(1);
            Ok(())
        }) {
            return EvalValue::Scalar(EvalScalar::Error(e));
        }
    }
    fin_result(sum)
}

/// Evaluates `IRR(values, [guess])`.
pub fn eval_irr(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=2).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(vals_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let guess = match read_opt_num_arg(it.next(), 0.1, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if guess <= -1.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }

    let mut flows = Vec::new();
    match &vals_arg {
        EvalValue::Reference(r) => {
            if let Err(e) = visit_reference_cells(r, workbook, spills, |_sheet, cell| match cell {
                RefCellVal::Word(bits) => match Value::unpack(bits) {
                    Value::Error(e) => Err(e),
                    Value::Number(n) => {
                        flows.push(n);
                        Ok(())
                    }
                    Value::Empty | Value::Text(_) | Value::Bool(_) | Value::Array(_) => Ok(()),
                },
                RefCellVal::Shadow(s) => match s {
                    EvalScalar::Error(e) => Err(*e),
                    EvalScalar::Number(n) => {
                        flows.push(*n);
                        Ok(())
                    }
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => Ok(()),
                },
            }) {
                return EvalValue::Scalar(EvalScalar::Error(e));
            }
        }
        EvalValue::Array(arr) => {
            for item in &arr.values {
                match item {
                    EvalScalar::Error(e) => return EvalValue::Scalar(EvalScalar::Error(*e)),
                    EvalScalar::Number(n) => flows.push(*n),
                    EvalScalar::Empty | EvalScalar::Text(_) | EvalScalar::Bool(_) => {}
                }
            }
        }
        EvalValue::Scalar(EvalScalar::Error(e)) => {
            return EvalValue::Scalar(EvalScalar::Error(*e));
        }
        EvalValue::Scalar(_) | EvalValue::Missing => {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
        }
    }

    let has_pos = flows.iter().any(|&v| v > 0.0);
    let has_neg = flows.iter().any(|&v| v < 0.0);
    if !has_pos || !has_neg {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }

    let irr_val = |r: f64| -> Option<(f64, f64)> {
        if r <= -1.0 {
            return None;
        }
        let inv = 1.0 / (1.0 + r);
        let mut f = 0.0_f64;
        let mut df = 0.0_f64;
        let mut pow_inv = 1.0_f64;
        for (i, &cf) in flows.iter().enumerate() {
            f += cf * pow_inv;
            if i > 0 {
                #[expect(clippy::cast_precision_loss, reason = "flow count fits in f64")]
                let k = i as f64;
                df -= k * cf * pow_inv * inv;
            }
            pow_inv *= inv;
        }
        (f.is_finite() && df.is_finite()).then_some((f, df))
    };

    // 1. Deterministic Newton-Raphson from `guess`.
    let mut r = guess;
    for _ in 0..100 {
        let Some((f0, df)) = irr_val(r) else {
            break;
        };
        if f0.abs() < 1e-11 {
            return fin_result(r);
        }
        if df.abs() < 1e-18 {
            break;
        }
        let next = (r - f0 / df).clamp(-0.999_999, 1e6);
        let delta = (next - r).abs();
        r = next;
        if delta < 1e-12
            && let Some((chk, _)) = irr_val(r)
            && chk.abs() < 1e-7
        {
            return fin_result(r);
        }
    }

    // 2. Deterministic grid bracket + bisection fallback.
    let grid: &[f64] = &[
        -0.99, -0.9, -0.5, -0.2, -0.05, 0.0, 0.05, 0.1, 0.2, 0.5, 1.0, 3.0, 10.0, 100.0,
    ];
    for pair in grid.windows(2) {
        let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let (Some((fa, _)), Some((fb, _))) = (irr_val(a), irr_val(b)) else {
            continue;
        };
        if fa == 0.0 {
            return fin_result(a);
        }
        if (fa < 0.0 && fb > 0.0) || (fa > 0.0 && fb < 0.0) {
            let mut lo = a;
            let mut hi = b;
            let mut flo = fa;
            for _ in 0..100 {
                let mid = 0.5 * (lo + hi);
                let Some((fmid, _)) = irr_val(mid) else {
                    break;
                };
                if fmid == 0.0 || (hi - lo) * 0.5 < 1e-12 {
                    return fin_result(mid);
                }
                if (flo < 0.0 && fmid < 0.0) || (flo > 0.0 && fmid > 0.0) {
                    lo = mid;
                    flo = fmid;
                } else {
                    hi = mid;
                }
            }
            return fin_result(0.5 * (lo + hi));
        }
    }

    EvalValue::Scalar(EvalScalar::Error(CalcError::Num))
}

/// Evaluates `SLN(cost, salvage, life)`.
pub fn eval_sln(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.len() != 3 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(c_arg), Some(s_arg), Some(l_arg)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let cost = match read_scalar_num_arg(c_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let salvage = match read_scalar_num_arg(s_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let life = match read_scalar_num_arg(l_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if life == 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Div0));
    }
    fin_result((cost - salvage) / life)
}
