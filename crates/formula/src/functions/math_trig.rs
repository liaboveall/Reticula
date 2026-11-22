//! Math and trigonometry functions (`17-m3a.md` 5.1 — 29 functions).
//!
//! All transcendental operations use `reticula_core::num` (`libm`) to guarantee
//! bit-exact reproducibility across x86_64, aarch64, Windows, Linux, macOS,
//! and wasm32.

use alloc::vec::Vec;
use core::f64::consts::PI;

use reticula_core::decimal15::round_to_15_sig_figs;
use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::value::{CalcError, Value};

use super::{
    RefCellVal, get_sa, lift_binary, lift_unary, shape_of, visit_numeric_arg, visit_reference_cells,
};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ScalarOrArray,
    coerce_for_scalar_op,
};

fn num_result(v: f64) -> EvalScalar {
    if v.is_finite() {
        EvalScalar::number(v)
    } else {
        EvalScalar::Error(CalcError::Num)
    }
}

fn unary_math(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    f: impl Fn(f64) -> Result<f64, CalcError>,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |x| match x.to_number() {
        Ok(n) => match f(n) {
            Ok(v) => num_result(v),
            Err(e) => EvalScalar::Error(e),
        },
        Err(e) => EvalScalar::Error(e),
    })
}

/// Evaluates `PI()`.
pub fn eval_pi(args: &[EvalValue]) -> EvalValue {
    if !args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    EvalValue::Scalar(EvalScalar::Number(PI))
}

/// Evaluates `SIGN(number)`.
pub fn eval_sign(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        Ok(if n > 0.0 {
            1.0
        } else if n < 0.0 {
            -1.0
        } else {
            0.0
        })
    })
}

/// Evaluates `EVEN(number)`.
pub fn eval_even(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if n == 0.0 {
            return Ok(0.0);
        }
        let ceiled = num::ceil(n.abs());
        let mag = if ceiled % 2.0 == 0.0 {
            ceiled
        } else {
            ceiled + 1.0
        };
        Ok(if n < 0.0 { -mag } else { mag })
    })
}

/// Evaluates `ODD(number)`.
pub fn eval_odd(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if n == 0.0 {
            return Ok(1.0);
        }
        let ceiled = num::ceil(n.abs());
        let mag = if ceiled % 2.0 != 0.0 {
            ceiled
        } else {
            ceiled + 1.0
        };
        Ok(if n < 0.0 { -mag } else { mag })
    })
}

/// Evaluates `MROUND(number, multiple)`.
pub fn eval_mround(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |num_arg, mult_arg| {
        let x = match num_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let m = match mult_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if m == 0.0 || x == 0.0 {
            return EvalScalar::Number(0.0);
        }
        if (x > 0.0 && m < 0.0) || (x < 0.0 && m > 0.0) {
            return EvalScalar::Error(CalcError::Num);
        }
        let ax = round_to_15_sig_figs(x.abs());
        let am = round_to_15_sig_figs(m.abs());
        let ratio = round_to_15_sig_figs(ax / am);
        if !ratio.is_finite() {
            return EvalScalar::Error(CalcError::Num);
        }
        // Half away from zero.
        let fl = num::floor(ratio);
        let frac = ratio - fl;
        let mult_count = if frac >= 0.5 { fl + 1.0 } else { fl };
        let res = mult_count * m.abs();
        let signed = if x < 0.0 { -res } else { res };
        num_result(round_to_15_sig_figs(signed))
    })
}

/// Evaluates `QUOTIENT(numerator, denominator)`.
pub fn eval_quotient(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |num_arg, den_arg| {
        let a = match num_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let b = match den_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if b == 0.0 {
            return EvalScalar::Error(CalcError::Div0);
        }
        num_result(num::trunc(a / b))
    })
}

/// Evaluates `FACT(number)`.
pub fn eval_fact(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if n < 0.0 {
            return Err(CalcError::Num);
        }
        let k = num::trunc(n);
        if k > 170.0 {
            return Err(CalcError::Num);
        }
        #[expect(clippy::cast_possible_truncation, reason = "bounded to 0..=170")]
        #[expect(clippy::cast_sign_loss, reason = "non-negative after guard")]
        let ki = k as u32;
        let mut acc = 1.0_f64;
        for i in 2..=ki {
            acc *= f64::from(i);
        }
        Ok(acc)
    })
}

fn check_comb_perm_args(n_raw: f64, k_raw: f64) -> Result<(u64, u64), CalcError> {
    if n_raw < 0.0 || k_raw < 0.0 {
        return Err(CalcError::Num);
    }
    let nt = num::trunc(n_raw);
    let kt = num::trunc(k_raw);
    if nt < kt || nt > 9_007_199_254_740_991.0 {
        return Err(CalcError::Num);
    }
    #[expect(clippy::cast_possible_truncation, reason = "bounded to <= 2^53")]
    #[expect(clippy::cast_sign_loss, reason = "non-negative after guard")]
    Ok((nt as u64, kt as u64))
}

/// Evaluates `COMBIN(number, number_chosen)`.
pub fn eval_combin(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |n_arg, k_arg| {
        let (n, k) = match (n_arg.to_number(), k_arg.to_number()) {
            (Ok(nv), Ok(kv)) => match check_comb_perm_args(nv, kv) {
                Ok(pair) => pair,
                Err(e) => return EvalScalar::Error(e),
            },
            (Err(e), _) | (_, Err(e)) => return EvalScalar::Error(e),
        };
        let m = k.min(n.saturating_sub(k));
        if m == 0 {
            return EvalScalar::Number(1.0);
        }
        let mut res = 1.0_f64;
        for i in 1..=m {
            #[expect(clippy::cast_precision_loss, reason = "combinatorial steps in f64")]
            let factor = (n.saturating_sub(m).saturating_add(i)) as f64;
            #[expect(clippy::cast_precision_loss, reason = "divisor step in f64")]
            let div = i as f64;
            res = res * factor / div;
            if !res.is_finite() {
                return EvalScalar::Error(CalcError::Num);
            }
        }
        num_result(num::round(res))
    })
}

/// Evaluates `PERMUT(number, number_chosen)`.
pub fn eval_permut(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |n_arg, k_arg| {
        let (n, k) = match (n_arg.to_number(), k_arg.to_number()) {
            (Ok(nv), Ok(kv)) => match check_comb_perm_args(nv, kv) {
                Ok(pair) => pair,
                Err(e) => return EvalScalar::Error(e),
            },
            (Err(e), _) | (_, Err(e)) => return EvalScalar::Error(e),
        };
        let mut res = 1.0_f64;
        let start = n.saturating_sub(k).saturating_add(1);
        for v in start..=n {
            #[expect(clippy::cast_precision_loss, reason = "permutation product in f64")]
            let vf = v as f64;
            res *= vf;
            if !res.is_finite() {
                return EvalScalar::Error(CalcError::Num);
            }
        }
        num_result(res)
    })
}

const MAX_INT_GCD_LCM: f64 = 9_007_199_254_740_991.0;

fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a.checked_rem(b).unwrap_or(0);
        a = b;
        b = t;
    }
    a
}

fn eval_gcd_or_lcm(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    is_lcm: bool,
) -> EvalScalar {
    if args.is_empty() {
        return EvalScalar::Error(CalcError::Value);
    }
    let mut vals: Vec<u64> = Vec::new();
    let mut push_num = |n: f64| -> Result<(), CalcError> {
        if !n.is_finite() || n < 0.0 || num::trunc(n) > MAX_INT_GCD_LCM {
            return Err(CalcError::Num);
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounded <= MAX_INT_GCD_LCM"
        )]
        #[expect(clippy::cast_sign_loss, reason = "non-negative after guard")]
        let v = num::trunc(n) as u64;
        vals.push(v);
        Ok(())
    };

    for arg in args {
        match arg {
            EvalValue::Missing => {}
            EvalValue::Scalar(s) => match s {
                EvalScalar::Error(e) => return EvalScalar::Error(*e),
                EvalScalar::Bool(_) => return EvalScalar::Error(CalcError::Value),
                EvalScalar::Empty => {
                    if let Err(e) = push_num(0.0) {
                        return EvalScalar::Error(e);
                    }
                }
                _ => match s.to_number() {
                    Ok(n) => {
                        if let Err(e) = push_num(n) {
                            return EvalScalar::Error(e);
                        }
                    }
                    Err(e) => return EvalScalar::Error(e),
                },
            },
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    match item {
                        EvalScalar::Error(e) => return EvalScalar::Error(*e),
                        EvalScalar::Number(n) => {
                            if let Err(e) = push_num(*n) {
                                return EvalScalar::Error(e);
                            }
                        }
                        EvalScalar::Bool(_) | EvalScalar::Text(_) => {
                            return EvalScalar::Error(CalcError::Value);
                        }
                        EvalScalar::Empty => {}
                    }
                }
            }
            EvalValue::Reference(r) => {
                let res = visit_reference_cells(r, workbook, spills, |_sheet, cell| match cell {
                    RefCellVal::Word(bits) => match Value::unpack(bits) {
                        Value::Error(e) => Err(e),
                        Value::Number(n) => push_num(n),
                        Value::Empty => Ok(()),
                        Value::Bool(_) | Value::Text(_) | Value::Array(_) => Err(CalcError::Value),
                    },
                    RefCellVal::Shadow(s) => match s {
                        EvalScalar::Error(e) => Err(*e),
                        EvalScalar::Number(n) => push_num(*n),
                        EvalScalar::Empty => Ok(()),
                        EvalScalar::Bool(_) | EvalScalar::Text(_) => Err(CalcError::Value),
                    },
                });
                if let Err(e) = res {
                    return EvalScalar::Error(e);
                }
            }
        }
    }

    if !is_lcm {
        let mut acc = 0_u64;
        for v in vals {
            acc = gcd_u64(acc, v);
        }
        #[expect(clippy::cast_precision_loss, reason = "result bounded <= 2^53")]
        EvalScalar::Number(acc as f64)
    } else {
        if vals.is_empty() {
            return EvalScalar::Number(0.0);
        }
        let mut acc = 1_u128;
        for v in vals {
            if v == 0 {
                return EvalScalar::Number(0.0);
            }
            let vu = u128::from(v);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "gcd(acc, vu) <= vu <= u64::MAX"
            )]
            let g = gcd_u64(acc.checked_rem(vu).unwrap_or(0) as u64, v);
            let next = acc
                .checked_div(u128::from(g))
                .unwrap_or(0)
                .saturating_mul(vu);
            #[expect(clippy::cast_possible_truncation, reason = "compared against 2^53")]
            #[expect(clippy::cast_sign_loss, reason = "constant is positive")]
            if next > (MAX_INT_GCD_LCM as u128) {
                return EvalScalar::Error(CalcError::Num);
            }
            acc = next;
        }
        #[expect(clippy::cast_precision_loss, reason = "checked <= 2^53 - 1")]
        EvalScalar::Number(acc as f64)
    }
}

/// Evaluates `GCD(number1, [number2], ...)`.
pub fn eval_gcd(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    EvalValue::Scalar(eval_gcd_or_lcm(args, workbook, spills, false))
}

/// Evaluates `LCM(number1, [number2], ...)`.
pub fn eval_lcm(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    EvalValue::Scalar(eval_gcd_or_lcm(args, workbook, spills, true))
}

/// Evaluates `RADIANS(angle)`.
pub fn eval_radians(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |d| Ok(d * (PI / 180.0)))
}

/// Evaluates `DEGREES(angle)`.
pub fn eval_degrees(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |r| Ok(r * (180.0 / PI)))
}

/// Evaluates `EXP(number)`.
pub fn eval_exp(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        let res = num::exp(n);
        if res.is_finite() {
            Ok(res)
        } else {
            Err(CalcError::Num)
        }
    })
}

/// Evaluates `LN(number)`.
pub fn eval_ln(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if n <= 0.0 {
            Err(CalcError::Num)
        } else {
            Ok(num::ln(n))
        }
    })
}

/// Evaluates `LOG10(number)`.
pub fn eval_log10(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if n <= 0.0 {
            Err(CalcError::Num)
        } else {
            Ok(num::log10(n))
        }
    })
}

/// Evaluates `LOG(number, [base])`.
pub fn eval_log(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=2).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(a0) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let num_sa = match coerce_for_scalar_op(a0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let base_sa = match it.next() {
        None | Some(EvalValue::Missing) => ScalarOrArray::Scalar(EvalScalar::Number(10.0)),
        Some(a1) => match coerce_for_scalar_op(a1, workbook, spills, caller) {
            Ok(v) => v,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
    };
    let log_one = |xs: &EvalScalar, bs: &EvalScalar| -> EvalScalar {
        let x = match xs.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let b = match bs.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if x <= 0.0 || b <= 0.0 {
            return EvalScalar::Error(CalcError::Num);
        }
        if b == 1.0 {
            return EvalScalar::Error(CalcError::Div0);
        }
        if b == 10.0 {
            num_result(num::log10(x))
        } else {
            num_result(num::ln(x) / num::ln(b))
        }
    };
    if let (ScalarOrArray::Scalar(xs), ScalarOrArray::Scalar(bs)) = (&num_sa, &base_sa) {
        return EvalValue::Scalar(log_one(xs, bs));
    }
    let (r1, c1) = shape_of(&num_sa);
    let (r2, c2) = shape_of(&base_sa);
    let (out_r, out_c) = (r1.max(r2), c1.max(c2));
    let Some(total) = (out_r as usize).checked_mul(out_c as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut vals = Vec::with_capacity(total);
    for r in 0..out_r {
        for c in 0..out_c {
            vals.push(log_one(&get_sa(&num_sa, r, c), &get_sa(&base_sa, r, c)));
        }
    }
    match Array2D::new(out_r, out_c, vals) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `SIN(number)`.
pub fn eval_sin(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::sin(n)))
}

/// Evaluates `COS(number)`.
pub fn eval_cos(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::cos(n)))
}

/// Evaluates `TAN(number)`.
pub fn eval_tan(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::tan(n)))
}

/// Evaluates `ASIN(number)`.
pub fn eval_asin(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if !(-1.0..=1.0).contains(&n) {
            Err(CalcError::Num)
        } else {
            Ok(num::asin(n))
        }
    })
}

/// Evaluates `ACOS(number)`.
pub fn eval_acos(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| {
        if !(-1.0..=1.0).contains(&n) {
            Err(CalcError::Num)
        } else {
            Ok(num::acos(n))
        }
    })
}

/// Evaluates `ATAN(number)`.
pub fn eval_atan(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::atan(n)))
}

/// Evaluates `ATAN2(x_num, y_num)`.
pub fn eval_atan2(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |x_arg, y_arg| {
        let x = match x_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let y = match y_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if x == 0.0 && y == 0.0 {
            return EvalScalar::Error(CalcError::Div0);
        }
        num_result(num::atan2(y, x))
    })
}

/// Evaluates `SINH(number)`.
pub fn eval_sinh(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::sinh(n)))
}

/// Evaluates `COSH(number)`.
pub fn eval_cosh(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::cosh(n)))
}

/// Evaluates `TANH(number)`.
pub fn eval_tanh(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    unary_math(args, workbook, spills, caller, |n| Ok(num::tanh(n)))
}

/// Evaluates `PRODUCT(number1, [number2], ...)`.
pub fn eval_product(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut prod = 1.0_f64;
    let mut seen = false;
    for arg in args {
        if let Err(e) = visit_numeric_arg(arg, workbook, spills, |n| {
            seen = true;
            prod *= n;
            if !prod.is_finite() {
                return Err(CalcError::Num);
            }
            Ok(())
        }) {
            return EvalValue::Scalar(EvalScalar::Error(e));
        }
    }
    EvalValue::Scalar(if seen {
        EvalScalar::number(prod)
    } else {
        EvalScalar::Number(0.0)
    })
}

/// Evaluates `SUMSQ(number1, [number2], ...)`.
pub fn eval_sumsq(
    args: &[EvalValue],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut sum = 0.0_f64;
    for arg in args {
        if let Err(e) = visit_numeric_arg(arg, workbook, spills, |n| {
            sum += n * n;
            if !sum.is_finite() {
                return Err(CalcError::Num);
            }
            Ok(())
        }) {
            return EvalValue::Scalar(EvalScalar::Error(e));
        }
    }
    EvalValue::Scalar(EvalScalar::number(sum))
}
