//! Layer-1 date functions (`DATE`, `YEAR`, `MONTH`, `DAY`, `EDATE`, `EOMONTH`,
//! `DAYS`) on the Excel 1900 serial calendar (`14-m1b.md` 5.3).
//!
//! Preserves historical `1900-02-29 = 60` (`DATE(1900, 2, 29) = 60`,
//! `DATE(1900, 3, 0) = 60`, `YEAR(0) = 1900`, `MONTH(0) = 1`, `DAY(0) = 0`).

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::format::{days_in_month_gregorian, serial_to_ymd, ymd_to_serial};
use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::value::{CalcError, Value};

use super::{
    RefCellVal, lift_binary, lift_ternary, lift_unary, text_conv::coerce_date_or_number,
    trunc_to_isize, visit_reference_cells,
};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{
    Array2D, CallerContext, EvalScalar, EvalValue, ScalarOrArray, coerce_for_scalar_op,
};

/// Maximum valid Excel 1900-system date serial (`9999-12-31`).
const MAX_DATE_SERIAL: i64 = 2_958_465;

/// Evaluates `DATE(year, month, day)`.
pub fn eval_date(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_ternary(args, workbook, spills, caller, |y_arg, m_arg, d_arg| {
        let y_raw = match coerce_date_or_number(y_arg).and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let m_raw = match coerce_date_or_number(m_arg).and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let d_raw = match coerce_date_or_number(d_arg).and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };

        let year = if (0..=1899).contains(&y_raw) {
            y_raw.saturating_add(1900)
        } else if (1900..=9999).contains(&y_raw) {
            y_raw
        } else {
            return EvalScalar::Error(CalcError::Num);
        };

        // Normalise `(year, m_raw)` into `(norm_y, norm_m in 1..=12)` via Euclidean division.
        let total_months = year
            .saturating_mul(12)
            .saturating_add(m_raw.saturating_sub(1));
        let norm_y = total_months.div_euclid(12);
        let norm_m_i64 = total_months.rem_euclid(12).saturating_add(1);

        let (Ok(norm_y_i32), Ok(norm_m_u8)) = (i32::try_from(norm_y), u8::try_from(norm_m_i64))
        else {
            return EvalScalar::Error(CalcError::Num);
        };
        let Some(first_of_month) = first_of_month_serial_i64(norm_y_i32, norm_m_u8) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let Some(serial) = first_of_month.checked_add(d_raw.saturating_sub(1)) else {
            return EvalScalar::Error(CalcError::Num);
        };
        if !(0..=MAX_DATE_SERIAL).contains(&serial) {
            return EvalScalar::Error(CalcError::Num);
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "`0..=2_958_465` is exact in 53-bit f64"
        )]
        EvalScalar::Number(serial as f64)
    })
}

/// Evaluates `YEAR`, `MONTH`, or `DAY` (`part`: `0 = YEAR`, `1 = MONTH`, `2 = DAY`).
pub fn eval_date_part(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    part: u8,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |arg| {
        let serial = match coerce_date_or_number(arg) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let Some((y, m, d)) = excel_serial_to_ymd_with_zero(serial) else {
            return EvalScalar::Error(CalcError::Num);
        };
        match part {
            0 => EvalScalar::Number(f64::from(y)),
            1 => EvalScalar::Number(f64::from(m)),
            _ => EvalScalar::Number(f64::from(d)),
        }
    })
}

/// Evaluates `EDATE(start_date, months)` (`end_of_month == false`) or
/// `EOMONTH(start_date, months)` (`end_of_month == true`).
pub fn eval_edate_or_eomonth(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    end_of_month: bool,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |start_arg, months_arg| {
        let start = match coerce_date_or_number(start_arg) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let months = match coerce_date_or_number(months_arg).and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let Some((y, m, d)) = excel_serial_to_ymd_with_zero(start) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let base_months = i64::from(y)
            .saturating_mul(12)
            .saturating_add(i64::from(m).saturating_sub(1));
        let Some(target_months) = base_months.checked_add(months) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let target_y_i64 = target_months.div_euclid(12);
        let target_m_i64 = target_months.rem_euclid(12).saturating_add(1);
        let (Ok(target_y), Ok(target_m)) =
            (i32::try_from(target_y_i64), u8::try_from(target_m_i64))
        else {
            return EvalScalar::Error(CalcError::Num);
        };
        if !(1900..=9999).contains(&target_y) {
            return EvalScalar::Error(CalcError::Num);
        }
        let max_day = days_in_month_gregorian(target_y, target_m);
        let target_d = if end_of_month {
            max_day
        } else {
            d.clamp(1, max_day)
        };
        ymd_to_serial(target_y, target_m, target_d)
            .map_or(EvalScalar::Error(CalcError::Num), EvalScalar::number)
    })
}

/// Evaluates `DAYS(end_date, start_date)`.
pub fn eval_days(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_binary(args, workbook, spills, caller, |end_arg, start_arg| {
        let end_num = match coerce_date_or_number(end_arg) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let start_num = match coerce_date_or_number(start_arg) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let end_day = num::floor(end_num);
        let start_day = num::floor(start_num);
        #[expect(
            clippy::cast_precision_loss,
            reason = "`MAX_DATE_SERIAL = 2_958_465` is exact in f64"
        )]
        let max_f = MAX_DATE_SERIAL as f64;
        if end_day < 0.0 || end_day > max_f || start_day < 0.0 || start_day > max_f {
            return EvalScalar::Error(CalcError::Num);
        }
        EvalScalar::number(end_day - start_day)
    })
}

fn excel_serial_to_ymd_with_zero(serial: f64) -> Option<(i32, u8, u8)> {
    if !serial.is_finite() || serial < 0.0 {
        return None;
    }
    let whole = num::floor(serial);
    if whole == 0.0 {
        return Some((1900, 1, 0));
    }
    serial_to_ymd(whole)
}

/// Computes the 1900-system serial of `(year, month, 1)` for arbitrary proleptic
/// `year` (allowing intermediate years `< 1900` or `> 9999` before `day` offset
/// brings the final serial back into `0..=2_958_465`).
fn first_of_month_serial_i64(year: i32, month: u8) -> Option<i64> {
    if !(1..=12).contains(&month) || !(-20_000..=20_000).contains(&year) {
        return None;
    }
    if year == 1900 && month == 1 {
        return Some(1);
    }
    if year == 1900 && month == 2 {
        return Some(32);
    }
    // Howard Hinnant `days_from_civil(year, month, 1)` relative to `1970-01-01`.
    let m = i64::from(month);
    let y = i64::from(year).saturating_sub(i64::from(month <= 2));
    let era = if y >= 0 { y } else { y.saturating_sub(399) } / 400;
    let yoe = y.saturating_sub(era.saturating_mul(400));
    let mp = if m > 2 {
        m.saturating_sub(3)
    } else {
        m.saturating_add(9)
    };
    let doy = mp.saturating_mul(153).saturating_add(2) / 5;
    let doe = yoe
        .saturating_mul(365)
        .saturating_add(yoe / 4)
        .saturating_sub(yoe / 100)
        .saturating_add(doy);
    let days_since_unix = era
        .saturating_mul(146_097)
        .saturating_add(doe)
        .saturating_sub(719_468);
    let mut serial = days_since_unix.saturating_add(25_569);
    // Below `1900-03-01` (proleptic `days_since_unix + 25_569 == 61`), Excel's
    // fictional `1900-02-29` shifts pre-March-1900 proleptic dates by -1.
    if year < 1900 {
        serial = serial.saturating_sub(1);
    }
    Some(serial)
}

fn trunc_i64(n: f64) -> Result<i64, CalcError> {
    if !n.is_finite() {
        return Err(CalcError::Num);
    }
    let t = num::trunc(n);
    if !(-1_000_000_000.0..=1_000_000_000.0).contains(&t) {
        return Err(CalcError::Num);
    }
    #[expect(clippy::cast_possible_truncation, reason = "clamped to `[-1e9, 1e9]`")]
    Ok(t as i64)
}

// ---------------------------------------------------------------------------
// M3a Date & Time functions (TIME, HOUR, MINUTE, SECOND, DATEVALUE, TIMEVALUE,
// WEEKDAY, WEEKNUM, ISOWEEKNUM, DATEDIF, DAYS360, NETWORKDAYS, WORKDAY, YEARFRAC)
// ---------------------------------------------------------------------------

/// Evaluates `TIME(hour, minute, second)`.
pub fn eval_time(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_ternary(args, workbook, spills, caller, |h_arg, m_arg, s_arg| {
        let h = match h_arg.to_number().and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let m = match m_arg.to_number().and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let s = match s_arg.to_number().and_then(trunc_i64) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let total_secs = h
            .saturating_mul(3_600)
            .saturating_add(m.saturating_mul(60))
            .saturating_add(s);
        if total_secs < 0 {
            return EvalScalar::Error(CalcError::Num);
        }
        let secs_of_day = total_secs.rem_euclid(86_400);
        #[expect(clippy::cast_precision_loss, reason = "0..86400 fits in f64")]
        EvalScalar::number((secs_of_day as f64) / 86_400.0)
    })
}

/// Evaluates `HOUR` (`part == 0`), `MINUTE` (`part == 1`), or `SECOND` (`part == 2`).
pub fn eval_time_part(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
    part: u8,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |arg| {
        let serial = match coerce_date_or_number(arg) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if !serial.is_finite() || serial < 0.0 {
            return EvalScalar::Error(CalcError::Num);
        }
        let frac = serial - num::floor(serial);
        let rounded_secs = num::round(frac * 86_400.0);
        #[expect(clippy::cast_possible_truncation, reason = "0..=86400")]
        #[expect(clippy::cast_sign_loss, reason = "non-negative after check")]
        let secs = (rounded_secs as u64) % 86_400;
        let out = match part {
            0 => secs / 3_600,
            1 => (secs % 3_600) / 60,
            _ => secs % 60,
        };
        EvalScalar::Number(f64::from(u32::try_from(out).unwrap_or(0)))
    })
}

fn month_abbr_to_num(s: &str) -> Option<u8> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}

fn expand_two_digit_year(y_raw: i32) -> i32 {
    if (0..=29).contains(&y_raw) {
        y_raw.saturating_add(2000)
    } else if (30..=99).contains(&y_raw) {
        y_raw.saturating_add(1900)
    } else {
        y_raw
    }
}

fn parse_time_str(raw: &str) -> Option<f64> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let mut ampm: Option<bool> = None; // Some(true) = PM, Some(false) = AM
    if let Some(rest) = s
        .strip_suffix("AM")
        .or_else(|| s.strip_suffix("am"))
        .or_else(|| s.strip_suffix("Am"))
        .or_else(|| s.strip_suffix("aM"))
    {
        ampm = Some(false);
        s = rest.trim();
    } else if let Some(rest) = s
        .strip_suffix("PM")
        .or_else(|| s.strip_suffix("pm"))
        .or_else(|| s.strip_suffix("Pm"))
        .or_else(|| s.strip_suffix("pM"))
    {
        ampm = Some(true);
        s = rest.trim();
    }
    let mut parts = s.split(':');
    let h_str = parts.next()?.trim();
    let m_str = parts.next()?.trim();
    let sec_opt = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let mut hh: u32 = h_str.parse().ok()?;
    let mm: u32 = m_str.parse().ok()?;
    if mm >= 60 {
        return None;
    }
    let ss: f64 = match sec_opt {
        None => 0.0,
        Some(sv) => {
            let v: f64 = sv.trim().parse().ok()?;
            if !(0.0..60.0).contains(&v) {
                return None;
            }
            v
        }
    };
    match ampm {
        Some(is_pm) => {
            if hh == 0 || hh > 12 {
                return None;
            }
            if is_pm && hh < 12 {
                hh = hh.saturating_add(12);
            } else if !is_pm && hh == 12 {
                hh = 0;
            }
        }
        None => {
            if hh >= 24 {
                return None;
            }
        }
    }
    Some((f64::from(hh) * 3_600.0 + f64::from(mm) * 60.0 + ss) / 86_400.0)
}

fn parse_date_only_str(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // 1. Numeric `YYYY-MM-DD`, `YYYY/MM/DD`, or US `MM/DD/YYYY` / `MM-DD-YYYY`.
    for sep in ['/', '-'] {
        let parts: Vec<&str> = s.split(sep).collect();
        if parts.len() == 3 {
            let (p0, p1, p2) = (
                parts.first()?.trim(),
                parts.get(1)?.trim(),
                parts.get(2)?.trim(),
            );
            if p0.len() == 4
                && let (Ok(y), Ok(m), Ok(d)) =
                    (p0.parse::<i32>(), p1.parse::<u8>(), p2.parse::<u8>())
            {
                return ymd_to_serial(y, m, d);
            }
            if (p2.len() == 4 || p2.len() == 2)
                && let (Ok(m), Ok(d), Ok(y_raw)) =
                    (p0.parse::<u8>(), p1.parse::<u8>(), p2.parse::<i32>())
            {
                let y = if p2.len() == 2 {
                    expand_two_digit_year(y_raw)
                } else {
                    y_raw
                };
                return ymd_to_serial(y, m, d);
            }
            // `"15-Jan-2026"` or `"2026-Jan-15"`
            if let Some(m) = month_abbr_to_num(p1) {
                if p0.len() == 4
                    && let (Ok(y), Ok(d)) = (p0.parse::<i32>(), p2.parse::<u8>())
                {
                    return ymd_to_serial(y, m, d);
                }
                if let (Ok(d), Ok(y_raw)) = (p0.parse::<u8>(), p2.parse::<i32>()) {
                    let y = if p2.len() == 2 {
                        expand_two_digit_year(y_raw)
                    } else {
                        y_raw
                    };
                    return ymd_to_serial(y, m, d);
                }
            }
        }
    }
    // 2. Named months `"Jan 15, 2026"` / `"15 Jan 2026"` / `"January 15 2026"`.
    let cleaned: String = s.replace(',', " ");
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.len() == 3 {
        let (w0, w1, w2) = (*words.first()?, *words.get(1)?, *words.get(2)?);
        if let Some(m) = month_abbr_to_num(w0)
            && let (Ok(d), Ok(y_raw)) = (w1.parse::<u8>(), w2.parse::<i32>())
        {
            let y = if w2.len() == 2 {
                expand_two_digit_year(y_raw)
            } else {
                y_raw
            };
            return ymd_to_serial(y, m, d);
        }
        if let Some(m) = month_abbr_to_num(w1)
            && let (Ok(d), Ok(y_raw)) = (w0.parse::<u8>(), w2.parse::<i32>())
        {
            let y = if w2.len() == 2 {
                expand_two_digit_year(y_raw)
            } else {
                y_raw
            };
            return ymd_to_serial(y, m, d);
        }
    }
    None
}

fn parse_datetime_split(raw: &str) -> Option<(Option<f64>, Option<f64>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(d) = parse_date_only_str(trimmed) {
        return Some((Some(d), None));
    }
    if let Some(t) = parse_time_str(trimmed) {
        return Some((None, Some(t)));
    }
    for split_ch in ['T', ' '] {
        for (idx, ch) in trimmed.char_indices() {
            if ch == split_ch {
                let left = trimmed.get(..idx)?;
                let right = trimmed.get(idx.saturating_add(1)..)?;
                if let (Some(d), Some(t)) = (parse_date_only_str(left), parse_time_str(right)) {
                    return Some((Some(d), Some(t)));
                }
            }
        }
    }
    None
}

/// Evaluates `DATEVALUE(date_text)`.
pub fn eval_datevalue(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |arg| match arg {
        EvalScalar::Error(e) => EvalScalar::Error(*e),
        EvalScalar::Text(s) => match parse_datetime_split(s) {
            Some((Some(d), _)) => EvalScalar::number(num::floor(d)),
            _ => EvalScalar::Error(CalcError::Value),
        },
        _ => EvalScalar::Error(CalcError::Value),
    })
}

/// Evaluates `TIMEVALUE(time_text)`.
pub fn eval_timevalue(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |arg| match arg {
        EvalScalar::Error(e) => EvalScalar::Error(*e),
        EvalScalar::Text(s) => match parse_datetime_split(s) {
            Some((_, Some(t))) => EvalScalar::number(t),
            Some((Some(_), None)) => EvalScalar::Number(0.0),
            _ => EvalScalar::Error(CalcError::Value),
        },
        _ => EvalScalar::Error(CalcError::Value),
    })
}

fn dow0_sunday_from_serial(whole: i64) -> i64 {
    whole.saturating_add(6).rem_euclid(7)
}

/// Evaluates `WEEKDAY(serial_number, [return_type])`.
pub fn eval_weekday(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=2).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(s_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let ret_type = match it.next() {
        None | Some(EvalValue::Missing) => 1_isize,
        Some(v) => {
            let sa = match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(x) => x,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_number().and_then(trunc_to_isize) {
                Ok(n) => n,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };

    let sa = match coerce_for_scalar_op(s_arg, workbook, spills, caller) {
        Ok(x) => x,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let calc = |s: &EvalScalar| -> EvalScalar {
        let serial = match coerce_date_or_number(s) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        if !serial.is_finite() || serial < 0.0 || serial > (MAX_DATE_SERIAL as f64) + 0.999999 {
            return EvalScalar::Error(CalcError::Num);
        }
        let whole = match trunc_i64(num::floor(serial)) {
            Ok(w) => w,
            Err(e) => return EvalScalar::Error(e),
        };
        let dow0_sun = dow0_sunday_from_serial(whole); // 0=Sun..6=Sat
        let res = match ret_type {
            1 | 17 => dow0_sun.saturating_add(1),
            2 | 11 => dow0_sun.saturating_add(6).rem_euclid(7).saturating_add(1),
            3 => dow0_sun.saturating_add(6).rem_euclid(7),
            12 => dow0_sun.saturating_add(5).rem_euclid(7).saturating_add(1),
            13 => dow0_sun.saturating_add(4).rem_euclid(7).saturating_add(1),
            14 => dow0_sun.saturating_add(3).rem_euclid(7).saturating_add(1),
            15 => dow0_sun.saturating_add(2).rem_euclid(7).saturating_add(1),
            16 => dow0_sun.saturating_add(1).rem_euclid(7).saturating_add(1),
            _ => return EvalScalar::Error(CalcError::Num),
        };
        EvalScalar::Number(f64::from(i32::try_from(res).unwrap_or(1)))
    };
    match sa {
        ScalarOrArray::Scalar(sc) => EvalValue::Scalar(calc(&sc)),
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

fn day_of_year(year: i32, month: u8, day: u8) -> u32 {
    let mut doy: u32 = 0;
    for m in 1..month {
        doy = doy.saturating_add(u32::from(days_in_month_gregorian(year, m)));
    }
    doy.saturating_add(u32::from(day))
}

fn iso_week_number(serial: f64) -> Result<u32, CalcError> {
    let whole = trunc_i64(num::floor(serial))?;
    if !(1..=MAX_DATE_SERIAL).contains(&whole) {
        return Err(CalcError::Num);
    }
    // Monday-based day of week (0=Mon..6=Sun).
    let dow_mon = dow0_sunday_from_serial(whole)
        .saturating_add(6)
        .rem_euclid(7);
    let thu_serial = whole.saturating_add(3_i64.saturating_sub(dow_mon));
    #[expect(clippy::cast_precision_loss, reason = "date serial fits in f64")]
    let (thu_y, thu_m, thu_d) = serial_to_ymd(thu_serial as f64).ok_or(CalcError::Num)?;
    let thu_doy = day_of_year(thu_y, thu_m, thu_d);
    Ok(((thu_doy.saturating_sub(1)) / 7).saturating_add(1))
}

/// Evaluates `ISOWEEKNUM(date)`.
pub fn eval_isoweeknum(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_unary(args, workbook, spills, caller, |arg| {
        let serial = match coerce_date_or_number(arg) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        iso_week_number(serial).map_or_else(EvalScalar::Error, |w| EvalScalar::Number(f64::from(w)))
    })
}

/// Evaluates `WEEKNUM(serial_number, [return_type])`.
pub fn eval_weeknum(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(1..=2).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let Some(s_arg) = it.next() else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let ret_type = match it.next() {
        None | Some(EvalValue::Missing) => 1_isize,
        Some(v) => {
            let sa = match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(x) => x,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_number().and_then(trunc_to_isize) {
                Ok(n) => n,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };

    let sa = match coerce_for_scalar_op(s_arg, workbook, spills, caller) {
        Ok(x) => x,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    let calc = |s: &EvalScalar| -> EvalScalar {
        let serial = match coerce_date_or_number(s) {
            Ok(v) => v,
            Err(e) => return EvalScalar::Error(e),
        };
        let whole = match trunc_i64(num::floor(serial)) {
            Ok(w) => w,
            Err(e) => return EvalScalar::Error(e),
        };
        if !(0..=MAX_DATE_SERIAL).contains(&whole) {
            return EvalScalar::Error(CalcError::Num);
        }
        if ret_type == 21 {
            return iso_week_number(serial)
                .map_or_else(EvalScalar::Error, |w| EvalScalar::Number(f64::from(w)));
        }
        let week_start_sun0 = match ret_type {
            1 | 17 => 0_i64,
            2 | 11 => 1,
            12 => 2,
            13 => 3,
            14 => 4,
            15 => 5,
            16 => 6,
            _ => return EvalScalar::Error(CalcError::Num),
        };
        let Some((y, m, d)) = excel_serial_to_ymd_with_zero(serial) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let jan1 = ymd_to_serial(y, 1, 1).unwrap_or(1.0);
        let jan1_i = trunc_i64(jan1).unwrap_or(1);
        let jan1_dow0 = dow0_sunday_from_serial(jan1_i);
        let offset = jan1_dow0.saturating_sub(week_start_sun0).rem_euclid(7);
        let doy = i64::from(day_of_year(y, m, d));
        let week = doy
            .saturating_sub(1)
            .saturating_add(offset)
            .div_euclid(7)
            .saturating_add(1);
        EvalScalar::Number(f64::from(i32::try_from(week).unwrap_or(1)))
    };
    match sa {
        ScalarOrArray::Scalar(sc) => EvalValue::Scalar(calc(&sc)),
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

/// Evaluates `DATEDIF(start_date, end_date, unit)`.
pub fn eval_datedif(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    lift_ternary(args, workbook, spills, caller, |s_arg, e_arg, u_arg| {
        let s_raw = match coerce_date_or_number(s_arg) {
            Ok(v) => num::floor(v),
            Err(e) => return EvalScalar::Error(e),
        };
        let e_raw = match coerce_date_or_number(e_arg) {
            Ok(v) => num::floor(v),
            Err(e) => return EvalScalar::Error(e),
        };
        let unit = match u_arg.to_text() {
            Ok(u) => u,
            Err(e) => return EvalScalar::Error(e),
        };
        let (Ok(si), Ok(ei)) = (trunc_i64(s_raw), trunc_i64(e_raw)) else {
            return EvalScalar::Error(CalcError::Num);
        };
        if si < 0 || ei < si || ei > MAX_DATE_SERIAL {
            return EvalScalar::Error(CalcError::Num);
        }
        let Some((sy, sm, sd)) = excel_serial_to_ymd_with_zero(s_raw) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let Some((ey, em, ed)) = excel_serial_to_ymd_with_zero(e_raw) else {
            return EvalScalar::Error(CalcError::Num);
        };
        let total_months = i64::from(ey)
            .saturating_sub(i64::from(sy))
            .saturating_mul(12)
            .saturating_add(i64::from(em).saturating_sub(i64::from(sm)))
            .saturating_sub(i64::from(ed < sd));

        let ans: i64 = if unit.eq_ignore_ascii_case("D") {
            ei.saturating_sub(si)
        } else if unit.eq_ignore_ascii_case("M") {
            total_months
        } else if unit.eq_ignore_ascii_case("Y") {
            total_months / 12
        } else if unit.eq_ignore_ascii_case("YM") {
            total_months % 12
        } else if unit.eq_ignore_ascii_case("MD") {
            if ed >= sd {
                i64::from(ed.saturating_sub(sd))
            } else {
                let (pm_y, pm_m) = if em == 1 {
                    (ey.saturating_sub(1), 12)
                } else {
                    (ey, em.saturating_sub(1))
                };
                let dim = days_in_month_gregorian(pm_y, pm_m);
                i64::from(dim)
                    .saturating_sub(i64::from(sd))
                    .saturating_add(i64::from(ed))
            }
        } else if unit.eq_ignore_ascii_case("YD") {
            let max_d_sy = days_in_month_gregorian(sy, em);
            let adj_d = ed.min(max_d_sy).max(1);
            let same_yr = ymd_to_serial(sy, em, adj_d).unwrap_or(s_raw);
            let same_yr_i = trunc_i64(same_yr).unwrap_or(si);
            if same_yr_i >= si {
                same_yr_i.saturating_sub(si)
            } else {
                let ny = sy.saturating_add(1);
                let max_d_ny = days_in_month_gregorian(ny, em);
                let next_yr = ymd_to_serial(ny, em, ed.min(max_d_ny).max(1)).unwrap_or(e_raw);
                trunc_i64(next_yr).unwrap_or(ei).saturating_sub(si)
            }
        } else {
            return EvalScalar::Error(CalcError::Num);
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "date difference <= 3M fits in f64"
        )]
        EvalScalar::number(ans as f64)
    })
}

fn days360_calc(sy: i32, sm: u8, sd: u8, ey: i32, em: u8, ed: u8, european: bool) -> i64 {
    let is_last_of_feb = |y: i32, m: u8, d: u8| m == 2 && d == days_in_month_gregorian(y, 2);
    let mut d1 = i64::from(sd);
    let mut d2 = i64::from(ed);
    if european {
        d1 = d1.min(30);
        d2 = d2.min(30);
    } else {
        if is_last_of_feb(sy, sm, sd) {
            if is_last_of_feb(ey, em, ed) {
                d2 = 30;
            }
            d1 = 30;
        }
        if d1 == 31 || d1 == 30 {
            d1 = 30;
            if d2 == 31 {
                d2 = 30;
            }
        } else if d1 == 31 {
            d1 = 30;
        }
    }
    i64::from(ey)
        .saturating_sub(i64::from(sy))
        .saturating_mul(360)
        .saturating_add(
            i64::from(em)
                .saturating_sub(i64::from(sm))
                .saturating_mul(30),
        )
        .saturating_add(d2.saturating_sub(d1))
}

/// Evaluates `DAYS360(start_date, end_date, [method])`.
pub fn eval_days360(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(s_arg), Some(e_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let euro = match it.next() {
        None | Some(EvalValue::Missing) => false,
        Some(v) => {
            let sa = match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(x) => x,
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
    lift_binary(
        alloc::vec![s_arg, e_arg],
        workbook,
        spills,
        caller,
        |sa, ea| {
            let s = match coerce_date_or_number(sa) {
                Ok(v) => num::floor(v),
                Err(e) => return EvalScalar::Error(e),
            };
            let e = match coerce_date_or_number(ea) {
                Ok(v) => num::floor(v),
                Err(err) => return EvalScalar::Error(err),
            };
            let (Some((sy, sm, sd)), Some((ey, em, ed))) = (
                excel_serial_to_ymd_with_zero(s),
                excel_serial_to_ymd_with_zero(e),
            ) else {
                return EvalScalar::Error(CalcError::Num);
            };
            #[expect(clippy::cast_precision_loss, reason = "day count <= 3M fits in f64")]
            EvalScalar::number(days360_calc(sy, sm, sd, ey, em, ed, euro) as f64)
        },
    )
}

fn collect_holiday_serials(
    arg_opt: Option<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
) -> Result<BTreeSet<i64>, CalcError> {
    let mut set = BTreeSet::new();
    let Some(arg) = arg_opt else {
        return Ok(set);
    };
    let mut add_scalar = |s: &EvalScalar| -> Result<(), CalcError> {
        if matches!(s, EvalScalar::Empty) {
            return Ok(());
        }
        let d = num::floor(coerce_date_or_number(s)?);
        let di = trunc_i64(d)?;
        if !(0..=MAX_DATE_SERIAL).contains(&di) {
            return Err(CalcError::Num);
        }
        set.insert(di);
        Ok(())
    };
    match arg {
        EvalValue::Missing => Ok(set),
        EvalValue::Scalar(s) => {
            add_scalar(&s)?;
            Ok(set)
        }
        EvalValue::Array(arr) => {
            for item in &arr.values {
                add_scalar(item)?;
            }
            Ok(set)
        }
        EvalValue::Reference(r) => {
            visit_reference_cells(&r, workbook, spills, |sheet, cell| {
                let s = match cell {
                    RefCellVal::Word(bits) => {
                        EvalScalar::from_sheet_value(sheet, Value::unpack(bits))
                    }
                    RefCellVal::Shadow(sc) => sc.clone(),
                };
                add_scalar(&s)
            })?;
            Ok(set)
        }
    }
}

fn is_weekday(whole: i64) -> bool {
    let dow = dow0_sunday_from_serial(whole);
    dow != 0 && dow != 6
}

fn weekdays_in_inclusive_range(lo: i64, hi: i64) -> i64 {
    let total = hi.saturating_sub(lo).saturating_add(1);
    let full_weeks = total / 7;
    let rem = total % 7;
    let mut count = full_weeks.saturating_mul(5);
    for off in 0..rem {
        if is_weekday(lo.saturating_add(off)) {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Evaluates `NETWORKDAYS(start_date, end_date, [holidays])`.
pub fn eval_networkdays(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(s_arg), Some(e_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let holidays = match collect_holiday_serials(it.next(), workbook, spills) {
        Ok(h) => h,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    lift_binary(
        alloc::vec![s_arg, e_arg],
        workbook,
        spills,
        caller,
        |sa, ea| {
            let s = match coerce_date_or_number(sa)
                .map(num::floor)
                .and_then(trunc_i64)
            {
                Ok(v) if (0..=MAX_DATE_SERIAL).contains(&v) => v,
                Ok(_) => return EvalScalar::Error(CalcError::Num),
                Err(e) => return EvalScalar::Error(e),
            };
            let e = match coerce_date_or_number(ea)
                .map(num::floor)
                .and_then(trunc_i64)
            {
                Ok(v) if (0..=MAX_DATE_SERIAL).contains(&v) => v,
                Ok(_) => return EvalScalar::Error(CalcError::Num),
                Err(err) => return EvalScalar::Error(err),
            };
            let (lo, hi, sign) = if s <= e {
                (s, e, 1_i64)
            } else {
                (e, s, -1_i64)
            };
            let mut work = weekdays_in_inclusive_range(lo, hi);
            for &h in holidays.range(lo..=hi) {
                if is_weekday(h) {
                    work = work.saturating_sub(1);
                }
            }
            #[expect(clippy::cast_precision_loss, reason = "day count <= 3M fits in f64")]
            EvalScalar::number(work.saturating_mul(sign) as f64)
        },
    )
}

/// Evaluates `WORKDAY(start_date, days, [holidays])`.
pub fn eval_workday(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(s_arg), Some(d_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let holidays = match collect_holiday_serials(it.next(), workbook, spills) {
        Ok(h) => h,
        Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
    };
    lift_binary(
        alloc::vec![s_arg, d_arg],
        workbook,
        spills,
        caller,
        |sa, da| {
            let mut cur = match coerce_date_or_number(sa)
                .map(num::floor)
                .and_then(trunc_i64)
            {
                Ok(v) if (0..=MAX_DATE_SERIAL).contains(&v) => v,
                Ok(_) => return EvalScalar::Error(CalcError::Num),
                Err(e) => return EvalScalar::Error(e),
            };
            let days = match da.to_number().and_then(trunc_i64) {
                Ok(v) => v,
                Err(e) => return EvalScalar::Error(e),
            };
            if days == 0 {
                #[expect(clippy::cast_precision_loss, reason = "serial fits in f64")]
                return EvalScalar::number(cur as f64);
            }
            let step = if days > 0 { 1_i64 } else { -1_i64 };
            let mut rem = days.abs();
            while rem > 0 {
                cur = cur.saturating_add(step);
                if !(0..=MAX_DATE_SERIAL).contains(&cur) {
                    return EvalScalar::Error(CalcError::Num);
                }
                if is_weekday(cur) && !holidays.contains(&cur) {
                    rem = rem.saturating_sub(1);
                }
            }
            #[expect(clippy::cast_precision_loss, reason = "serial fits in f64")]
            EvalScalar::number(cur as f64)
        },
    )
}

fn is_leap_year_gregorian(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

/// Evaluates `YEARFRAC(start_date, end_date, [basis])`.
pub fn eval_yearfrac(
    args: Vec<EvalValue>,
    workbook: &Workbook<BoundFormula>,
    spills: &SpillRegistry,
    caller: CallerContext,
) -> EvalValue {
    if !(2..=3).contains(&args.len()) {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    }
    let mut it = args.into_iter();
    let (Some(s_arg), Some(e_arg)) = (it.next(), it.next()) else {
        return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
    };
    let basis = match it.next() {
        None | Some(EvalValue::Missing) => 0_isize,
        Some(v) => {
            let sa = match coerce_for_scalar_op(v, workbook, spills, caller) {
                Ok(x) => x,
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            };
            let ScalarOrArray::Scalar(sc) = sa else {
                return EvalValue::Scalar(EvalScalar::Error(CalcError::Value));
            };
            match sc.to_number().and_then(trunc_to_isize) {
                Ok(n) if (0..=4).contains(&n) => n,
                Ok(_) => return EvalValue::Scalar(EvalScalar::Error(CalcError::Num)),
                Err(e) => return EvalValue::Scalar(EvalScalar::Error(e)),
            }
        }
    };
    lift_binary(
        alloc::vec![s_arg, e_arg],
        workbook,
        spills,
        caller,
        |sa, ea| {
            let s_raw = match coerce_date_or_number(sa)
                .map(num::floor)
                .and_then(trunc_i64)
            {
                Ok(v) if (0..=MAX_DATE_SERIAL).contains(&v) => v,
                Ok(_) => return EvalScalar::Error(CalcError::Num),
                Err(e) => return EvalScalar::Error(e),
            };
            let e_raw = match coerce_date_or_number(ea)
                .map(num::floor)
                .and_then(trunc_i64)
            {
                Ok(v) if (0..=MAX_DATE_SERIAL).contains(&v) => v,
                Ok(_) => return EvalScalar::Error(CalcError::Num),
                Err(err) => return EvalScalar::Error(err),
            };
            let (s, e) = if s_raw <= e_raw {
                (s_raw, e_raw)
            } else {
                (e_raw, s_raw)
            };
            #[expect(clippy::cast_precision_loss, reason = "serial fits in f64")]
            let (Some((sy, sm, sd)), Some((ey, em, ed))) = (
                excel_serial_to_ymd_with_zero(s as f64),
                excel_serial_to_ymd_with_zero(e as f64),
            ) else {
                return EvalScalar::Error(CalcError::Num);
            };
            #[expect(clippy::cast_precision_loss, reason = "day diff fits in f64")]
            let actual_days = e.saturating_sub(s) as f64;
            let frac = match basis {
                0 => (days360_calc(sy, sm, sd, ey, em, ed, false) as f64) / 360.0,
                1 => {
                    let year_spans_gt_one = ey > sy.saturating_add(1)
                        || (ey == sy.saturating_add(1) && (em > sm || (em == sm && ed > sd)));
                    let denom = if year_spans_gt_one {
                        let mut sum_days = 0_i64;
                        let years = i64::from(ey.saturating_sub(sy).saturating_add(1));
                        for y in sy..=ey {
                            sum_days = sum_days.saturating_add(if is_leap_year_gregorian(y) {
                                366
                            } else {
                                365
                            });
                        }
                        #[expect(clippy::cast_precision_loss, reason = "year count fits in f64")]
                        {
                            (sum_days as f64) / (years as f64)
                        }
                    } else {
                        let has_feb29 = if sy == ey {
                            is_leap_year_gregorian(sy)
                                && (sm < 2 || (sm == 2 && sd <= 29))
                                && (em > 2 || (em == 2 && ed == 29))
                        } else {
                            (is_leap_year_gregorian(sy) && (sm < 2 || (sm == 2 && sd <= 29)))
                                || (is_leap_year_gregorian(ey) && (em > 2 || (em == 2 && ed == 29)))
                        };
                        if has_feb29 { 366.0 } else { 365.0 }
                    };
                    actual_days / denom
                }
                2 => actual_days / 360.0,
                3 => actual_days / 365.0,
                _ => (days360_calc(sy, sm, sd, ey, em, ed, true) as f64) / 360.0,
            };
            EvalScalar::number(frac)
        },
    )
}
