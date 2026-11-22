//! Layer-1 text, conversion, rounding, and volatile functions (`FIND`, `SEARCH`,
//! `SUBSTITUTE`, `REPLACE`, `VALUE`, `TEXT`, `FLOOR`, `CEILING`, `TODAY`, `NOW`,
//! `RAND`, `RANDBETWEEN`; `14-m1b.md` 5.3).

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::decimal15::{excel_ceiling, excel_floor};
use reticula_core::format::{format_excel_text, ymd_to_serial};
use reticula_core::input::{InferredInput, infer_input};
use reticula_core::num;
use reticula_core::ops::{MAX_CELL_TEXT_UTF16, ScalarValue, Workbook};
use reticula_core::value::CalcError;

use super::wildcard::wildcard_search_substring;
use super::{
    EvalEnv, checked_text, get_sa, lift_binary, lift_ternary, lift_unary, shape_of, trunc_to_isize,
};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, MAX_ARRAY_CELLS, ScalarOrArray,
    coerce_for_scalar_op,
};

/// Evaluates `FLOOR(number, significance)` or `CEILING(number, significance)`.
pub fn eval_floor_or_ceiling(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    is_floor: bool,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |num_arg, sig_arg| {
        let x = match num_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let s = match sig_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let res = if is_floor {
            excel_floor(x, s)
        } else {
            excel_ceiling(x, s)
        };
        res.map_or_else(EvalScalar::Error, EvalScalar::number)
    })
}

/// Evaluates `TODAY()` (`with_time == false`) or `NOW()` (`with_time == true`).
pub fn eval_today_or_now(args: &[EvalValue], env: &EvalEnv, with_time: bool) -> EvalValue {
    if !args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    if !env.now_serial.is_finite() || env.now_serial < 0.0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Num));
    }
    let v = if with_time {
        env.now_serial
    } else {
        num::floor(env.now_serial)
    };
    EvalValue::Scalar(EvalScalar::number(v))
}

/// Evaluates `RAND()`.
pub fn eval_rand(args: &[EvalValue], env: &mut EvalEnv) -> EvalValue {
    if !args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    EvalValue::Scalar(EvalScalar::number(env.rng.next_f64()))
}

/// Evaluates `RANDBETWEEN(bottom, top)` (supports dynamic-array broadcasting).
pub fn eval_randbetween(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    env: &mut EvalEnv,
) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(a0), Some(a1)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let left = match coerce_for_scalar_op(a0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let right = match coerce_for_scalar_op(a1, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let mut draw_one = |lo_s: &EvalScalar, hi_s: &EvalScalar| -> EvalScalar {
        let lo_f = match lo_s.to_number() {
            Ok(v) => num::ceil(v),
            Err(e) => return EvalScalar::Error(e),
        };
        let hi_f = match hi_s.to_number() {
            Ok(v) => num::floor(v),
            Err(e) => return EvalScalar::Error(e),
        };
        if !lo_f.is_finite() || !hi_f.is_finite() || lo_f > hi_f || lo_f < -9.0e15 || hi_f > 9.0e15
        {
            return EvalScalar::Error(CalcError::Num);
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounded to `[-9e15, 9e15]`"
        )]
        let (lo_i, hi_i) = (lo_f as i64, hi_f as i64);
        let Some(diff) = hi_i.checked_sub(lo_i) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let Ok(diff_u64) = u64::try_from(diff) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let Some(span) = diff_u64.checked_add(1) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let offset = env.rng.next_below(span);
        let Ok(offset_i64) = i64::try_from(offset) else {
            return EvalScalar::Error(CalcError::Num);
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "`[-9e15, 9e15]` exact in 53-bit f64"
        )]
        EvalScalar::number(lo_i.saturating_add(offset_i64) as f64)
    };

    if let (ScalarOrArray::Scalar(a), ScalarOrArray::Scalar(b)) = (&left, &right) {
        return EvalValue::Scalar(draw_one(a, b));
    }
    let (r1, c1) = shape_of(&left);
    let (r2, c2) = shape_of(&right);
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
            values.push(draw_one(&get_sa(&left, r, c), &get_sa(&right, r, c)));
        }
    }
    match Array2D::new(out_rows, out_cols, values) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `FIND(find_text, within_text, [start_num])` (`case_sensitive == true`)
/// or `SEARCH(find_text, within_text, [start_num])` (`case_sensitive == false`).
pub fn eval_find_or_search(
    mut args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    case_sensitive: bool,
) -> EvalValue {
    if args.len() == 2 {
        args.push(EvalValue::Scalar(EvalScalar::Number(1.0)));
    }
    lift_ternary(
        args,
        workbook,
        spills,
        caller,
        |find_arg, within_arg, start_arg| {
            let needle = match find_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let haystack = match within_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let start_1based = match start_arg.to_number().and_then(trunc_to_isize) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if start_1based < 1 {
                return EvalScalar::Error(CalcError::Value);
            }
            #[expect(clippy::cast_sign_loss, reason = "`start_1based >= 1`")]
            let start_idx = (start_1based as usize).saturating_sub(1);

            let found_0based = if case_sensitive {
                find_literal_char_offset(&needle, &haystack, start_idx)
            } else {
                wildcard_search_substring(&needle, &haystack, start_idx)
            };
            match found_0based {
                Some(pos) =>
                {
                    #[expect(clippy::cast_precision_loss, reason = "`pos <= 32,768`")]
                    EvalScalar::Number(pos.saturating_add(1) as f64)
                }
                None => EvalScalar::Error(CalcError::Value),
            }
        },
    )
}

fn find_literal_char_offset(needle: &str, haystack: &str, start_char_idx: usize) -> Option<usize> {
    let h_chars: Vec<char> = haystack.chars().collect();
    if start_char_idx > h_chars.len() {
        return None;
    }
    if needle.is_empty() {
        return Some(start_char_idx);
    }
    let n_chars: Vec<char> = needle.chars().collect();
    if n_chars.len() > h_chars.len().saturating_sub(start_char_idx) {
        return None;
    }
    let last_start = h_chars.len().saturating_sub(n_chars.len());
    (start_char_idx..=last_start).find(|&pos| {
        h_chars.get(pos..pos.saturating_add(n_chars.len())) == Some(n_chars.as_slice())
    })
}

/// Evaluates `SUBSTITUTE(text, old_text, new_text, [instance_num])`.
pub fn eval_substitute(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(3..=4).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let has_instance = args.len() == 4;
    let mut it = args.into_iter();
    let (Some(a0), Some(a1), Some(a2)) = (it.next(), it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let a3 = it
        .next()
        .unwrap_or(EvalValue::Scalar(EvalScalar::Number(1.0)));

    lift_4(
        [a0, a1, a2, a3],
        workbook,
        spills,
        caller,
        |text_arg, old_arg, new_arg, inst_arg| {
            let text = match text_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let old_text = match old_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let new_text = match new_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let target_inst: Option<usize> = if has_instance {
                let n = match inst_arg.to_number().and_then(trunc_to_isize) {
                    Ok(v) => v,
                    Err(e) => return EvalScalar::Error(e),
                };
                if n < 1 {
                    return EvalScalar::Error(CalcError::Value);
                }
                #[expect(clippy::cast_sign_loss, reason = "`n >= 1`")]
                Some(n as usize)
            } else {
                None
            };

            if old_text.is_empty() {
                return EvalScalar::Text(text);
            }

            let mut out = String::with_capacity(text.len());
            let mut remainder: &str = &text;
            let mut seen = 0usize;
            while let Some(byte_pos) = remainder.find(&old_text) {
                seen = seen.saturating_add(1);
                let Some(before) = remainder.get(..byte_pos) else {
                    break;
                };
                let after_idx = byte_pos.saturating_add(old_text.len());
                let Some(after) = remainder.get(after_idx..) else {
                    break;
                };
                out.push_str(before);
                if target_inst.is_none_or(|inst| inst == seen) {
                    out.push_str(&new_text);
                    if target_inst == Some(seen) {
                        out.push_str(after);
                        return checked_text(out);
                    }
                } else {
                    out.push_str(&old_text);
                }
                if out.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
                    return EvalScalar::Error(CalcError::Value);
                }
                remainder = after;
            }
            out.push_str(remainder);
            checked_text(out)
        },
    )
}

/// Evaluates `REPLACE(old_text, start_num, num_chars, new_text)`.
pub fn eval_replace(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.len() != 4 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(a0), Some(a1), Some(a2), Some(a3)) = (it.next(), it.next(), it.next(), it.next())
    else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    lift_4(
        [a0, a1, a2, a3],
        workbook,
        spills,
        caller,
        |old_arg, start_arg, len_arg, new_arg| {
            let old_text = match old_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let start_1based = match start_arg.to_number().and_then(trunc_to_isize) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let count = match len_arg.to_number().and_then(trunc_to_isize) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            let new_text = match new_arg.to_text() {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if start_1based < 1 || count < 0 {
                return EvalScalar::Error(CalcError::Value);
            }
            #[expect(
                clippy::cast_sign_loss,
                reason = "`start_1based >= 1` and `count >= 0`"
            )]
            let (start_idx, drop_len) = ((start_1based as usize).saturating_sub(1), count as usize);

            let chars: Vec<char> = old_text.chars().collect();
            let split_a = start_idx.min(chars.len());
            let split_b = split_a.saturating_add(drop_len).min(chars.len());
            let mut out = String::with_capacity(old_text.len().saturating_add(new_text.len()));
            if let Some(prefix) = chars.get(..split_a) {
                out.extend(prefix.iter().copied());
            }
            out.push_str(&new_text);
            if let Some(suffix) = chars.get(split_b..) {
                out.extend(suffix.iter().copied());
            }
            checked_text(out)
        },
    )
}

/// Evaluates `VALUE(text)`.
pub fn eval_value(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |arg| match arg {
        EvalScalar::Error(e) => EvalScalar::Error(*e),
        EvalScalar::Empty => EvalScalar::Number(0.0),
        EvalScalar::Number(n) => EvalScalar::number(*n),
        EvalScalar::Bool(_) => EvalScalar::Error(CalcError::Value),
        EvalScalar::Text(s) => {
            parse_excel_value_str(s).map_or(EvalScalar::Error(CalcError::Value), EvalScalar::number)
        }
    })
}

/// Evaluates `TEXT(value, format_text)`.
pub fn eval_text(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |val_arg, fmt_arg| {
        if let EvalScalar::Error(e) = val_arg {
            return EvalScalar::Error(*e);
        }
        let fmt_str = match fmt_arg.to_text() {
            Ok(s) => s,
            Err(e) => return EvalScalar::Error(e),
        };
        match val_arg {
            EvalScalar::Error(e) => EvalScalar::Error(*e),
            EvalScalar::Bool(true) => EvalScalar::Text(String::from("TRUE")),
            EvalScalar::Bool(false) => EvalScalar::Text(String::from("FALSE")),
            EvalScalar::Empty => format_excel_text(0.0, &fmt_str)
                .map_or(EvalScalar::Error(CalcError::Value), checked_text),
            EvalScalar::Number(n) => format_excel_text(*n, &fmt_str)
                .map_or(EvalScalar::Error(CalcError::Value), checked_text),
            EvalScalar::Text(s) => {
                if let Some(n) = parse_excel_value_str(s) {
                    return format_excel_text(n, &fmt_str)
                        .map_or(EvalScalar::Error(CalcError::Value), checked_text);
                }
                if fmt_str.trim().eq_ignore_ascii_case("@")
                    || fmt_str.trim().eq_ignore_ascii_case("General")
                    || format_excel_text(0.0, &fmt_str).is_some()
                {
                    EvalScalar::Text(s.clone())
                } else {
                    EvalScalar::Error(CalcError::Value)
                }
            }
        }
    })
}

/// Coerces an [`EvalScalar`] into a number or date serial (accepting numbers,
/// booleans, empty, and strings parsed via [`parse_excel_value_str`]).
pub fn coerce_date_or_number(scalar: &EvalScalar) -> Result<f64, CalcError> {
    match scalar {
        EvalScalar::Error(e) => Err(*e),
        EvalScalar::Empty => Ok(0.0),
        EvalScalar::Number(n) => Ok(*n),
        EvalScalar::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        EvalScalar::Text(s) => parse_excel_value_str(s).ok_or(CalcError::Value),
    }
}

/// Parses numeric, currency, thousands-grouped, accounting-parenthesised,
/// percentage, ISO/slash date, and `HH:MM[:SS]` time strings for `VALUE()` and
/// date functions.
#[must_use]
pub fn parse_excel_value_str(raw: &str) -> Option<f64> {
    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return None;
    }

    // 1. Direct E30 scalar inference (plain decimal, scientific, percent, `YYYY-MM-DD`).
    if let Ok(InferredInput::Scalar {
        value: ScalarValue::Number(n),
        ..
    }) = infer_input(trimmed)
    {
        return Some(n);
    }

    // 2. Slash date `YYYY/MM/DD` or optional datetime `YYYY-MM-DD HH:MM[:SS]`.
    if let Some((date_part, time_part)) = trimmed.split_once(' ')
        && let (Some(d), Some(t)) = (
            parse_ymd_slash_or_dash(date_part),
            parse_hms(time_part.trim()),
        )
    {
        return Some(d + t);
    }
    if let Some(d) = parse_ymd_slash_or_dash(trimmed) {
        return Some(d);
    }
    if let Some(t) = parse_hms(trimmed) {
        return Some(t);
    }

    // 3. Accounting parentheses `(1,234.50)` and currency `$` / `¥` / `€` + commas.
    let mut s = trimmed;
    let mut neg = false;
    if let Some(inner) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        neg = true;
        s = inner.trim_matches(' ');
    }
    if let Some(rest) = s.strip_prefix('-') {
        neg = !neg;
        s = rest.trim_matches(' ');
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest.trim_matches(' ');
    }
    for sym in ['$', '¥', '€'] {
        if let Some(rest) = s.strip_prefix(sym) {
            s = rest.trim_matches(' ');
            break;
        }
    }
    if let Some(rest) = s.strip_prefix('-') {
        neg = !neg;
        s = rest.trim_matches(' ');
    }

    let is_pct = if let Some(rest) = s.strip_suffix('%') {
        s = rest.trim_matches(' ');
        true
    } else {
        false
    };

    let de_comma = strip_valid_thousands_commas(s)?;
    let mut query = String::with_capacity(de_comma.len().saturating_add(2));
    if neg {
        query.push('-');
    }
    query.push_str(&de_comma);
    if is_pct {
        query.push('%');
    }
    match infer_input(&query) {
        Ok(InferredInput::Scalar {
            value: ScalarValue::Number(n),
            ..
        }) => Some(if n == 0.0 { 0.0 } else { n }),
        _ => None,
    }
}

fn parse_ymd_slash_or_dash(s: &str) -> Option<f64> {
    let sep = if s.contains('/') { '/' } else { '-' };
    let mut parts = s.split(sep);
    let y_str = parts.next()?;
    let m_str = parts.next()?;
    let d_str = parts.next()?;
    if parts.next().is_some() || y_str.len() != 4 {
        return None;
    }
    let y: i32 = y_str.parse().ok()?;
    let m: u8 = m_str.parse().ok()?;
    let d: u8 = d_str.parse().ok()?;
    ymd_to_serial(y, m, d)
}

fn parse_hms(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h_str = parts.next()?;
    let m_str = parts.next()?;
    let s_opt = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let hh: u32 = h_str.parse().ok()?;
    let mm: u32 = m_str.parse().ok()?;
    if hh >= 24 || mm >= 60 {
        return None;
    }
    let ss: f64 = match s_opt {
        None => 0.0,
        Some(sec_str) => {
            let v: f64 = sec_str.parse().ok()?;
            if !(0.0..60.0).contains(&v) {
                return None;
            }
            v
        }
    };
    let total_secs = f64::from(hh) * 3_600.0 + f64::from(mm) * 60.0 + ss;
    Some(total_secs / 86_400.0)
}

fn strip_valid_thousands_commas(s: &str) -> Option<String> {
    if !s.contains(',') {
        return Some(String::from(s));
    }
    let (int_part, rest) = match s.find(['.', 'e', 'E']) {
        Some(idx) => (s.get(..idx)?, s.get(idx..)?),
        None => (s, ""),
    };
    let groups: Vec<&str> = int_part.split(',').collect();
    let first = *groups.first()?;
    if first.is_empty() || first.len() > 3 || !first.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    for &g in groups.get(1..)? {
        if g.len() != 3 || !g.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let mut out = String::with_capacity(s.len());
    for g in groups {
        out.push_str(g);
    }
    out.push_str(rest);
    Some(out)
}

fn lift_4(
    args: [EvalValue; 4],
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    f: impl Fn(&EvalScalar, &EvalScalar, &EvalScalar, &EvalScalar) -> EvalScalar,
) -> EvalValue {
    let [a0, a1, a2, a3] = args;
    let s0 = match coerce_for_scalar_op(a0, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let s1 = match coerce_for_scalar_op(a1, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let s2 = match coerce_for_scalar_op(a2, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let s3 = match coerce_for_scalar_op(a3, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if let (
        ScalarOrArray::Scalar(w),
        ScalarOrArray::Scalar(x),
        ScalarOrArray::Scalar(y),
        ScalarOrArray::Scalar(z),
    ) = (&s0, &s1, &s2, &s3)
    {
        return EvalValue::Scalar(f(w, x, y, z));
    }
    let (r0, c0) = shape_of(&s0);
    let (r1, c1) = shape_of(&s1);
    let (r2, c2) = shape_of(&s2);
    let (r3, c3) = shape_of(&s3);
    let out_rows = r0.max(r1).max(r2).max(r3);
    let out_cols = c0.max(c1).max(c2).max(c3);
    let Some(total) = (out_rows as usize).checked_mul(out_cols as usize) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut values = Vec::with_capacity(total);
    for r in 0..out_rows {
        for c in 0..out_cols {
            values.push(f(
                &get_sa(&s0, r, c),
                &get_sa(&s1, r, c),
                &get_sa(&s2, r, c),
                &get_sa(&s3, r, c),
            ));
        }
    }
    match Array2D::new(out_rows, out_cols, values) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}
