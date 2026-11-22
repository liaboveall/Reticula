//! Layer-1 date functions (`DATE`, `YEAR`, `MONTH`, `DAY`, `EDATE`, `EOMONTH`,
//! `DAYS`) on the Excel 1900 serial calendar (`14-m1b.md` 5.3).
//!
//! Preserves historical `1900-02-29 = 60` (`DATE(1900, 2, 29) = 60`,
//! `DATE(1900, 3, 0) = 60`, `YEAR(0) = 1900`, `MONTH(0) = 1`, `DAY(0) = 0`).

use alloc::vec::Vec;

use reticula_core::format::{days_in_month_gregorian, serial_to_ymd, ymd_to_serial};
use reticula_core::num;
use reticula_core::ops::Workbook;
use reticula_core::value::CalcError;

use super::{lift_binary, lift_ternary, lift_unary, text_conv::coerce_date_or_number};
use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::{CallerContext, EvalScalar, EvalValue};

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
