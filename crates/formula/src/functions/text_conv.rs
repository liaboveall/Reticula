//! Layer-1 text, conversion, rounding, and volatile functions (`FIND`, `SEARCH`,
//! `SUBSTITUTE`, `REPLACE`, `VALUE`, `TEXT`, `FLOOR`, `CEILING`, `TODAY`, `NOW`,
//! `RAND`, `RANDBETWEEN`; `14-m1b.md` 5.3).

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::decimal15::{RoundingMode, excel_ceiling, excel_floor, round_decimal};
use reticula_core::format::{format_excel_text, ymd_to_serial};
use reticula_core::input::{InferredInput, infer_input};
use reticula_core::num;
use reticula_core::ops::{MAX_CELL_TEXT_UTF16, ScalarValue, Workbook};
use reticula_core::value::{CalcError, Value};

use super::wildcard::wildcard_search_substring;
use super::{
    EvalEnv, RefCellVal, checked_text, get_sa, lift_binary, lift_ternary, lift_unary, shape_of,
    trunc_to_isize, visit_reference_cells,
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

// ---------------------------------------------------------------------------
// M3a Text functions (TEXTJOIN, TEXTBEFORE/AFTER, TEXTSPLIT, EXACT, PROPER,
// REPT, CHAR, CODE, UNICHAR, UNICODE, CLEAN, T, N, FIXED, DOLLAR, NUMBERVALUE,
// CONCATENATE)
// ---------------------------------------------------------------------------

fn read_scalar_str_opt(
    opt: Option<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<Option<String>, CalcError> {
    match opt {
        None | Some(EvalValue::Missing) => Ok(None),
        Some(v) => {
            let sa = coerce_for_scalar_op(v, workbook, spills, caller)?;
            let ScalarOrArray::Scalar(sc) = sa else {
                return Err(CalcError::Value);
            };
            sc.to_text().map(Some)
        }
    }
}

fn read_scalar_isize_opt(
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

fn read_scalar_bool_opt(
    opt: Option<EvalValue>,
    default: bool,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<bool, CalcError> {
    match opt {
        None | Some(EvalValue::Missing) => Ok(default),
        Some(v) => {
            let sa = coerce_for_scalar_op(v, workbook, spills, caller)?;
            let ScalarOrArray::Scalar(sc) = sa else {
                return Err(CalcError::Value);
            };
            sc.to_bool()
        }
    }
}

fn collect_delimiters(
    arg: &EvalValue,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> Result<Vec<String>, CalcError> {
    if matches!(arg, EvalValue::Missing) {
        return Ok(Vec::new());
    }
    match coerce_for_scalar_op(arg.clone(), workbook, spills, caller)? {
        ScalarOrArray::Scalar(sc) => Ok(alloc::vec![sc.to_text()?]),
        ScalarOrArray::Array(arr) => {
            let mut delims = Vec::with_capacity(arr.values.len());
            for item in &arr.values {
                delims.push(item.to_text()?);
            }
            Ok(delims)
        }
    }
}

/// Evaluates `CONCATENATE(text1, [text2], ...)`.
pub fn eval_concatenate(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut parts = Vec::with_capacity(args.len());
    let mut out_rows: u32 = 1;
    let mut out_cols: u32 = 1;
    let mut any_array = false;
    for arg in args {
        if matches!(arg, EvalValue::Missing) {
            parts.push(ScalarOrArray::Scalar(EvalScalar::Text(String::new())));
            continue;
        }
        match coerce_for_scalar_op(arg, workbook, spills, caller) {
            Ok(sa) => {
                if let ScalarOrArray::Array(ref a) = sa {
                    any_array = true;
                    out_rows = out_rows.max(a.rows);
                    out_cols = out_cols.max(a.cols);
                }
                parts.push(sa);
            }
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        }
    }
    let join_at = |r: u32, c: u32| -> EvalScalar {
        let mut out = String::new();
        for p in &parts {
            let s = match get_sa(p, r, c).to_text() {
                Ok(t) => t,
                Err(e) => return EvalScalar::Error(e),
            };
            let new_u16 = out
                .encode_utf16()
                .count()
                .saturating_add(s.encode_utf16().count());
            if new_u16 > MAX_CELL_TEXT_UTF16 {
                return EvalScalar::Error(CalcError::Value);
            }
            out.push_str(&s);
        }
        EvalScalar::Text(out)
    };
    if !any_array {
        return EvalValue::Scalar(join_at(0, 0));
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
            out.push(join_at(r, c));
        }
    }
    match Array2D::new(out_rows, out_cols, out) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `TEXTJOIN(delimiter, ignore_empty, text1, [text2], ...)`.
pub fn eval_textjoin(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if args.len() < 3 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(delim_arg), Some(ignore_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let delims = match collect_delimiters(&delim_arg, workbook, spills, caller) {
        Ok(d) => {
            if d.is_empty() {
                alloc::vec![String::new()]
            } else {
                d
            }
        }
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let ignore_empty = match coerce_for_scalar_op(ignore_arg, workbook, spills, caller) {
        Ok(ScalarOrArray::Scalar(sc)) => match sc.to_bool() {
            Ok(b) => b,
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
        Ok(ScalarOrArray::Array(_)) => {
            return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
        }
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };

    let mut out = String::new();
    let mut out_utf16 = 0_usize;
    let mut first_item = true;
    let mut delim_idx = 0_usize;

    let mut push_text = |s: &str, is_empty: bool| -> Result<(), CalcError> {
        if ignore_empty && is_empty {
            return Ok(());
        }
        if !first_item {
            let delim = delims
                .get(delim_idx.checked_rem(delims.len()).unwrap_or(0))
                .map_or("", String::as_str);
            delim_idx = delim_idx.saturating_add(1);
            let du16 = delim.encode_utf16().count();
            if out_utf16.saturating_add(du16) > MAX_CELL_TEXT_UTF16 {
                return Err(CalcError::Value);
            }
            out.push_str(delim);
            out_utf16 = out_utf16.saturating_add(du16);
        }
        let su16 = s.encode_utf16().count();
        if out_utf16.saturating_add(su16) > MAX_CELL_TEXT_UTF16 {
            return Err(CalcError::Value);
        }
        out.push_str(s);
        out_utf16 = out_utf16.saturating_add(su16);
        first_item = false;
        Ok(())
    };

    for arg in it {
        match arg {
            EvalValue::Missing => {
                if !ignore_empty && let Err(e) = push_text("", true) {
                    return EvalValue::Scalar(EvalScalar::Error(e));
                }
            }
            EvalValue::Scalar(s) => {
                let is_empty = matches!(s, EvalScalar::Empty)
                    || matches!(&s, EvalScalar::Text(t) if t.is_empty());
                let txt = match s.to_text() {
                    Ok(t) => t,
                    Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
                };
                if let Err(e) = push_text(&txt, is_empty) {
                    return EvalValue::Scalar(EvalScalar::Error(e));
                }
            }
            EvalValue::Array(arr) => {
                for item in &arr.values {
                    let is_empty = matches!(item, EvalScalar::Empty)
                        || matches!(item, EvalScalar::Text(t) if t.is_empty());
                    let txt = match item.to_text() {
                        Ok(t) => t,
                        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
                    };
                    if let Err(e) = push_text(&txt, is_empty) {
                        return EvalValue::Scalar(EvalScalar::Error(e));
                    }
                }
            }
            EvalValue::Reference(r) => {
                let res = visit_reference_cells(&r, workbook, spills, |sheet, cell| {
                    let s = match cell {
                        RefCellVal::Word(bits) => {
                            EvalScalar::from_sheet_value(sheet, Value::unpack(bits))
                        }
                        RefCellVal::Shadow(sc) => sc.clone(),
                    };
                    let is_empty = matches!(s, EvalScalar::Empty)
                        || matches!(&s, EvalScalar::Text(t) if t.is_empty());
                    let txt = s.to_text()?;
                    push_text(&txt, is_empty)
                });
                if let Err(e) = res {
                    return EvalValue::Scalar(EvalScalar::Error(e));
                }
            }
        }
    }

    EvalValue::Scalar(EvalScalar::Text(out))
}

fn find_delim_positions(
    haystack: &str,
    delims: &[String],
    case_insensitive: bool,
) -> Vec<(usize, usize)> {
    // Returns sorted non-overlapping `(byte_start, byte_end)` matches left-to-right.
    let mut non_empty: Vec<&str> = delims
        .iter()
        .map(String::as_str)
        .filter(|d| !d.is_empty())
        .collect();
    // Prefer longest delimiter first when sharing start offset.
    non_empty.sort_by_key(|a| core::cmp::Reverse(a.len()));
    let mut matches = Vec::new();
    let (h_cmp, d_cmp_owned): (String, Vec<String>) = if case_insensitive {
        (
            haystack.to_lowercase(),
            non_empty.iter().map(|d| d.to_lowercase()).collect(),
        )
    } else {
        (String::new(), Vec::new())
    };
    let h_slice = if case_insensitive {
        h_cmp.as_str()
    } else {
        haystack
    };

    let mut pos = 0_usize;
    while pos < h_slice.len() {
        let mut matched: Option<usize> = None;
        let tail = h_slice.get(pos..).unwrap_or("");
        if case_insensitive {
            for pat in &d_cmp_owned {
                if tail.starts_with(pat.as_str()) {
                    matched = Some(pat.len());
                    break;
                }
            }
        } else {
            for &pat in &non_empty {
                if tail.starts_with(pat) {
                    matched = Some(pat.len());
                    break;
                }
            }
        }
        if let Some(len) = matched
            && !case_insensitive
        {
            matches.push((pos, pos.saturating_add(len)));
            pos = pos.saturating_add(len.max(1));
        } else if let Some(len) = matched {
            // Map lowercased match byte len safely via char boundary check on original string.
            let orig_byte = map_lower_span(haystack, &h_cmp, pos, len);
            if let Some((os, oe)) = orig_byte {
                matches.push((os, oe));
                pos = pos.saturating_add(len.max(1));
            } else {
                pos = advance_char_byte(h_slice, pos);
            }
        } else {
            pos = advance_char_byte(h_slice, pos);
        }
    }
    matches
}

fn advance_char_byte(s: &str, idx: usize) -> usize {
    let ch_len = s
        .get(idx..)
        .and_then(|t| t.chars().next())
        .map_or(1, char::len_utf8);
    idx.saturating_add(ch_len)
}

fn map_lower_span(
    orig: &str,
    lower: &str,
    start_lower: usize,
    len_lower: usize,
) -> Option<(usize, usize)> {
    let end_lower = start_lower.saturating_add(len_lower);
    let mut cur_lower = 0_usize;
    let mut orig_start: Option<usize> = None;
    let mut orig_end: Option<usize> = None;
    for (ob, ch) in orig.char_indices() {
        if cur_lower == start_lower && orig_start.is_none() {
            orig_start = Some(ob);
        }
        let next_lower: usize = ch.to_lowercase().map(char::len_utf8).sum();
        cur_lower = cur_lower.saturating_add(next_lower);
        if cur_lower == end_lower {
            orig_end = Some(ob.saturating_add(ch.len_utf8()));
            break;
        }
    }
    let _ = lower;
    match (orig_start, orig_end) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    }
}

/// Evaluates `TEXTBEFORE` (`after == false`) or `TEXTAFTER` (`after == true`).
pub fn eval_text_before_or_after(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    after: bool,
) -> EvalValue {
    if !(2..=6).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(txt_arg), Some(delim_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let inst = match read_scalar_isize_opt(it.next(), 1, workbook, spills, caller) {
        Ok(0) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let match_mode = match read_scalar_isize_opt(it.next(), 0, workbook, spills, caller) {
        Ok(v @ (0 | 1)) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let match_end = match read_scalar_isize_opt(it.next(), 0, workbook, spills, caller) {
        Ok(v @ (0 | 1)) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let if_not_found = match it.next() {
        None | Some(EvalValue::Missing) => None,
        Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
            Ok(ScalarOrArray::Scalar(sc)) => Some(sc),
            Ok(ScalarOrArray::Array(_)) => {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            }
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
    };

    let delims = match collect_delimiters(&delim_arg, workbook, spills, caller) {
        Ok(d) => d,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if delims.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }

    let apply_one = |s_scalar: &EvalScalar| -> EvalScalar {
        let s = match s_scalar.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if delims.iter().any(String::is_empty) {
            if (!after && inst > 0) || (after && inst < 0) {
                return EvalScalar::Text(String::new());
            }
            return EvalScalar::Text(s);
        }
        let hits = find_delim_positions(&s, &delims, match_mode == 1);
        let k = inst.unsigned_abs();
        let selected = if inst > 0 {
            hits.get(k.saturating_sub(1)).copied()
        } else {
            hits.len()
                .checked_sub(k)
                .and_then(|idx| hits.get(idx).copied())
        };
        if let Some((ds, de)) = selected {
            let slice = if after {
                s.get(de..).unwrap_or("")
            } else {
                s.get(..ds).unwrap_or("")
            };
            EvalScalar::Text(String::from(slice))
        } else if match_end == 1 && k <= hits.len().saturating_add(1) {
            // Virtual end delimiter at end (inst > 0) or start (inst < 0).
            let res = match (after, inst > 0) {
                (false, true) | (true, false) => s,
                (false, false) | (true, true) => String::new(),
            };
            EvalScalar::Text(res)
        } else {
            if_not_found
                .clone()
                .unwrap_or(EvalScalar::Error(CalcError::Na))
        }
    };

    let txt_sa = match coerce_for_scalar_op(txt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match txt_sa {
        ScalarOrArray::Scalar(sc) => EvalValue::Scalar(apply_one(&sc)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(apply_one).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

fn split_by_delimiters<'a>(
    text: &'a str,
    delims: &[String],
    case_insensitive: bool,
    ignore_empty: bool,
) -> Vec<&'a str> {
    if delims.is_empty() || delims.iter().all(String::is_empty) {
        return alloc::vec![text];
    }
    let hits = find_delim_positions(text, delims, case_insensitive);
    let mut parts = Vec::with_capacity(hits.len().saturating_add(1));
    let mut last = 0_usize;
    for (s, e) in hits {
        let seg = text.get(last..s).unwrap_or("");
        if !ignore_empty || !seg.is_empty() {
            parts.push(seg);
        }
        last = e;
    }
    let tail = text.get(last..).unwrap_or("");
    if !ignore_empty || !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// Evaluates `TEXTSPLIT(text, col_delimiter, [row_delimiter], [ignore_empty], [match_mode], [pad_with])`.
pub fn eval_textsplit(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=6).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(txt_arg), Some(col_delim_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let row_delim_arg = it.next().unwrap_or(EvalValue::Missing);
    let ignore_empty = match read_scalar_bool_opt(it.next(), false, workbook, spills, caller) {
        Ok(b) => b,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let match_mode = match read_scalar_isize_opt(it.next(), 0, workbook, spills, caller) {
        Ok(v @ (0 | 1)) => v,
        Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let pad_with = match it.next() {
        None | Some(EvalValue::Missing) => EvalScalar::Error(CalcError::Na),
        Some(v) => match coerce_for_scalar_op(v, workbook, spills, caller) {
            Ok(ScalarOrArray::Scalar(sc)) => sc,
            Ok(ScalarOrArray::Array(_)) => {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            }
            Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
        },
    };

    let text = match read_scalar_str_opt(Some(txt_arg), workbook, spills, caller) {
        Ok(Some(s)) => s,
        Ok(None) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let col_delims = match collect_delimiters(&col_delim_arg, workbook, spills, caller) {
        Ok(d) => d,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let row_delims = match collect_delimiters(&row_delim_arg, workbook, spills, caller) {
        Ok(d) => d,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if col_delims.is_empty() && row_delims.is_empty() {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let ci = match_mode == 1;
    let raw_rows = if row_delims.is_empty() {
        alloc::vec![text.as_str()]
    } else {
        split_by_delimiters(&text, &row_delims, ci, ignore_empty)
    };

    let mut grid_rows: Vec<Vec<EvalScalar>> = Vec::with_capacity(raw_rows.len());
    let mut max_cols: usize = 0;
    for r_str in raw_rows {
        let cells: Vec<EvalScalar> = if col_delims.is_empty() {
            alloc::vec![EvalScalar::Text(String::from(r_str))]
        } else {
            split_by_delimiters(r_str, &col_delims, ci, ignore_empty)
                .into_iter()
                .map(|c| EvalScalar::Text(String::from(c)))
                .collect()
        };
        max_cols = max_cols.max(cells.len());
        grid_rows.push(cells);
    }

    if grid_rows.is_empty() || max_cols == 0 {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let (Ok(out_r), Ok(out_c)) = (u32::try_from(grid_rows.len()), u32::try_from(max_cols)) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    let Some(total) = grid_rows.len().checked_mul(max_cols) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    };
    if total > MAX_ARRAY_CELLS {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Calc));
    }
    let mut flat = Vec::with_capacity(total);
    for row in grid_rows {
        let row_len = row.len();
        for item in row {
            flat.push(item);
        }
        for _ in row_len..max_cols {
            flat.push(pad_with.clone());
        }
    }
    match Array2D::new(out_r, out_c, flat) {
        Ok(arr) => EvalValue::Array(arr),
        Err(e) => EvalValue::Scalar(EvalScalar::Error(e)),
    }
}

/// Evaluates `EXACT(text1, text2)` (case-sensitive comparison).
pub fn eval_exact(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |a, b| {
        let sa = match a.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let sb = match b.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        EvalScalar::Bool(sa == sb)
    })
}

/// Evaluates `PROPER(text)`.
pub fn eval_proper(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| {
        let s = match a.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let mut out = String::with_capacity(s.len());
        let mut at_word_start = true;
        for ch in s.chars() {
            if ch.is_alphabetic() {
                if at_word_start {
                    for uc in ch.to_uppercase() {
                        out.push(uc);
                    }
                    at_word_start = false;
                } else {
                    for lc in ch.to_lowercase() {
                        out.push(lc);
                    }
                }
            } else {
                out.push(ch);
                at_word_start = true;
            }
        }
        checked_text(out)
    })
}

/// Evaluates `REPT(text, number_times)`.
pub fn eval_rept(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |t_arg, n_arg| {
        let t = match t_arg.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let n_raw = match n_arg.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let n_i = match trunc_to_isize(n_raw) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if n_i < 0 {
            return EvalScalar::Error(CalcError::Value);
        }
        let Ok(n) = usize::try_from(n_i) else {
            return EvalScalar::Error(CalcError::Value);
        };
        if n == 0 || t.is_empty() {
            return EvalScalar::Text(String::new());
        }
        let u16_unit = t.encode_utf16().count();
        if u16_unit
            .checked_mul(n)
            .is_none_or(|len| len > MAX_CELL_TEXT_UTF16)
        {
            return EvalScalar::Error(CalcError::Value);
        }
        EvalScalar::Text(t.repeat(n))
    })
}

const WIN1252_80_9F: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// Evaluates `CHAR(number)`.
pub fn eval_char(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| {
        let n = match a.to_number().and_then(trunc_to_isize) {
            Ok(v) if (1..=255).contains(&v) => u8::try_from(v).unwrap_or(1),
            Ok(_) => return EvalScalar::Error(CalcError::Value),
            Err(e) => return EvalScalar::Error(e),
        };
        let ch = if (0x80..=0x9F).contains(&n) {
            WIN1252_80_9F
                .get(usize::from(n.saturating_sub(0x80)))
                .copied()
                .unwrap_or(char::from(n))
        } else {
            char::from(n)
        };
        let mut s = String::with_capacity(4);
        s.push(ch);
        EvalScalar::Text(s)
    })
}

/// Evaluates `CODE(text)`.
pub fn eval_code(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| {
        let s = match a.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let Some(ch) = s.chars().next() else {
            return EvalScalar::Error(CalcError::Value);
        };
        let cp = u32::from(ch);
        if (1..=0x7F).contains(&cp) || (0xA0..=0xFF).contains(&cp) {
            return EvalScalar::Number(f64::from(cp));
        }
        if let Some(pos) = WIN1252_80_9F.iter().position(|&c| c == ch) {
            return EvalScalar::Number(f64::from(
                0x80_u32.saturating_add(u32::try_from(pos).unwrap_or(0)),
            ));
        }
        if let Ok(b) = u8::try_from(cp) {
            return EvalScalar::Number(f64::from(b));
        }
        EvalScalar::Number(63.0)
    })
}

/// Evaluates `UNICHAR(number)`.
pub fn eval_unichar(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| {
        let n = match a.to_number().and_then(trunc_to_isize) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if n <= 0 {
            return EvalScalar::Error(CalcError::Value);
        }
        let Ok(u) = u32::try_from(n) else {
            return EvalScalar::Error(CalcError::Value);
        };
        let Some(ch) = char::from_u32(u) else {
            return EvalScalar::Error(CalcError::Na);
        };
        let mut s = String::with_capacity(4);
        s.push(ch);
        EvalScalar::Text(s)
    })
}

/// Evaluates `UNICODE(text)`.
pub fn eval_unicode(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| {
        let s = match a.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let Some(ch) = s.chars().next() else {
            return EvalScalar::Error(CalcError::Value);
        };
        EvalScalar::Number(f64::from(u32::from(ch)))
    })
}

/// Evaluates `CLEAN(text)`.
pub fn eval_clean(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| {
        let s = match a.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let cleaned: String = s.chars().filter(|&ch| u32::from(ch) >= 0x20).collect();
        EvalScalar::Text(cleaned)
    })
}

/// Evaluates `T(value)`.
pub fn eval_t(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| match a {
        EvalScalar::Error(e) => EvalScalar::Error(*e),
        EvalScalar::Text(s) => EvalScalar::Text(s.clone()),
        _ => EvalScalar::Text(String::new()),
    })
}

/// Evaluates `N(value)`.
pub fn eval_n(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |a| match a {
        EvalScalar::Error(e) => EvalScalar::Error(*e),
        EvalScalar::Number(n) => EvalScalar::number(*n),
        EvalScalar::Bool(true) => EvalScalar::Number(1.0),
        EvalScalar::Bool(false) | EvalScalar::Text(_) | EvalScalar::Empty => {
            EvalScalar::Number(0.0)
        }
    })
}

fn format_fixed_or_dollar(
    val: f64,
    decimals: isize,
    use_commas: bool,
    currency: bool,
) -> Result<String, CalcError> {
    if !val.is_finite() {
        return Err(CalcError::Num);
    }
    let Ok(d_i32) = i32::try_from(decimals) else {
        return Err(CalcError::Value);
    };
    let rounded =
        round_decimal(val, d_i32, RoundingMode::HalfAwayFromZero).ok_or(CalcError::Num)?;
    let is_neg = rounded < 0.0;
    let abs = rounded.abs();
    let frac_digits = usize::try_from(decimals.max(0)).unwrap_or(0).min(127);

    // Format `abs` with fixed decimal places via core formatter `alloc::format!`.
    let formatted = alloc::format!("{abs:.frac_digits$}");
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (formatted.as_str(), None),
    };

    let int_with_commas = if use_commas && int_part.len() > 3 {
        let mut s = String::with_capacity(int_part.len().saturating_add(int_part.len() / 3));
        let rem = int_part.len() % 3;
        for (idx, ch) in int_part.chars().enumerate() {
            if idx > 0 && (idx == rem || (idx > rem && idx.saturating_sub(rem).is_multiple_of(3))) {
                s.push(',');
            }
            s.push(ch);
        }
        s
    } else {
        String::from(int_part)
    };

    let mut num_str = int_with_commas;
    if let Some(fp) = frac_part
        && !fp.is_empty()
    {
        num_str.push('.');
        num_str.push_str(fp);
    }

    let final_str = if currency {
        if is_neg {
            alloc::format!("(${num_str})")
        } else {
            alloc::format!("${num_str}")
        }
    } else if is_neg {
        alloc::format!("-{num_str}")
    } else {
        num_str
    };
    if final_str.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        return Err(CalcError::Value);
    }
    Ok(final_str)
}

/// Evaluates `FIXED(number, [decimals], [no_commas])`.
pub fn eval_fixed(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(num_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let dec = match read_scalar_isize_opt(it.next(), 2, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let no_commas = match read_scalar_bool_opt(it.next(), false, workbook, spills, caller) {
        Ok(b) => b,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let sa = match coerce_for_scalar_op(num_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let fmt_one = |s: &EvalScalar| -> EvalScalar {
        let n = match s.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        match format_fixed_or_dollar(n, dec, !no_commas, false) {
            Ok(txt) => EvalScalar::Text(txt),
            Err(e) => EvalScalar::Error(e),
        }
    };
    match sa {
        ScalarOrArray::Scalar(sc) => EvalValue::Scalar(fmt_one(&sc)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(fmt_one).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

/// Evaluates `DOLLAR(number, [decimals])`.
pub fn eval_dollar(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=2).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(num_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let dec = match read_scalar_isize_opt(it.next(), 2, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let sa = match coerce_for_scalar_op(num_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let fmt_one = |s: &EvalScalar| -> EvalScalar {
        let n = match s.to_number() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        match format_fixed_or_dollar(n, dec, true, true) {
            Ok(txt) => EvalScalar::Text(txt),
            Err(e) => EvalScalar::Error(e),
        }
    };
    match sa {
        ScalarOrArray::Scalar(sc) => EvalValue::Scalar(fmt_one(&sc)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(fmt_one).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}

/// Evaluates `NUMBERVALUE(text, [decimal_separator], [group_separator])`.
pub fn eval_numbervalue(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(txt_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let dec_ch = match read_scalar_str_opt(it.next(), workbook, spills, caller) {
        Ok(None) => '.',
        Ok(Some(s)) => match s.chars().next() {
            Some(ch) => ch,
            None => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        },
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let grp_ch = match read_scalar_str_opt(it.next(), workbook, spills, caller) {
        Ok(None) => ',',
        Ok(Some(s)) => match s.chars().next() {
            Some(ch) => ch,
            None => return EvalValue::Scalar(EvalScalar::Error(CalcError::Value)),
        },
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    if dec_ch == grp_ch {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }

    let parse_nv = |s_scalar: &EvalScalar| -> EvalScalar {
        if let EvalScalar::Error(e) = s_scalar {
            return EvalScalar::Error(*e);
        }
        if let EvalScalar::Number(n) = s_scalar {
            return EvalScalar::number(*n);
        }
        let raw = match s_scalar.to_text() {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.is_empty() {
            return EvalScalar::Number(0.0);
        }
        let mut body = compact.as_str();
        let mut pct_count = 0_i32;
        while let Some(rest) = body.strip_suffix('%') {
            pct_count = pct_count.saturating_add(1);
            body = rest;
        }
        if body.contains('%') {
            return EvalScalar::Error(CalcError::Value);
        }
        let mut dec_seen = false;
        let mut norm = String::with_capacity(body.len());
        for ch in body.chars() {
            if ch == dec_ch {
                if dec_seen {
                    return EvalScalar::Error(CalcError::Value);
                }
                dec_seen = true;
                norm.push('.');
            } else if ch == grp_ch {
                if dec_seen {
                    return EvalScalar::Error(CalcError::Value);
                }
            } else {
                norm.push(ch);
            }
        }
        let Ok(mut val) = norm.parse::<f64>() else {
            return EvalScalar::Error(CalcError::Value);
        };
        for _ in 0..pct_count {
            val /= 100.0;
        }
        if val.is_finite() {
            EvalScalar::number(val)
        } else {
            EvalScalar::Error(CalcError::Num)
        }
    };

    let sa = match coerce_for_scalar_op(txt_arg, workbook, spills, caller) {
        Ok(v) => v,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    match sa {
        ScalarOrArray::Scalar(sc) => EvalValue::Scalar(parse_nv(&sc)),
        ScalarOrArray::Array(arr) => {
            let values = arr.values.iter().map(parse_nv).collect();
            EvalValue::Array(Array2D {
                rows: arr.rows,
                cols: arr.cols,
                values,
            })
        }
    }
}
