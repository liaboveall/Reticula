//! Numbers and dates as text, and 1900-system calendar conversions.
//!
//! The engine formats numbers itself so that every target shows the exact same
//! digits. `General` renders up to 15 significant decimal digits with trailing
//! fractional zeros stripped; `Date` uses the 1900 serial system, preserving
//! the historical `1900-02-29` (`serial = 60`) compatibility rule.

use alloc::string::String;
use core::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::decimal15::{RoundingMode, digits15, round_decimal};
use crate::num;

/// How a cell or column shows its numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NumberFormat {
    /// Up to 15 significant digits; scientific notation outside `[1e-4, 1e15)`.
    #[default]
    General,
    /// Fixed two decimal places (`0.00`).
    Fixed2,
    /// Whole percentage (`0%`, e.g. `0.13` as `13%`).
    Percent,
    /// ISO date (`yyyy-mm-dd`) in the 1900 serial number system.
    Date,
}

/// Largest magnitude formatted in fixed-point form.
const EXACT_LIMIT: f64 = 1.0e15;

/// Smallest non-zero magnitude formatted in fixed-point `General` form.
const GENERAL_MIN_FIXED: f64 = 1.0e-4;

/// Appends `value`, formatted according to `format`, to `out`.
pub fn write_number(value: f64, format: NumberFormat, out: &mut String) {
    let _ = match format {
        NumberFormat::General => write_general(value, out),
        NumberFormat::Fixed2 => write_scaled(value, 2, out),
        NumberFormat::Percent => {
            write_scaled(value * 100.0, 0, out).and_then(|()| out.write_char('%'))
        }
        NumberFormat::Date => write_date(value, out),
    };
}

/// Writes `value` in Excel-like `General` format (15 significant digits).
fn write_general(value: f64, out: &mut String) -> core::fmt::Result {
    if !value.is_finite() {
        return write!(out, "{value}");
    }
    if value == 0.0 {
        return out.write_char('0');
    }
    let Some(d) = digits15(value) else {
        return out.write_char('0');
    };
    if d.negative {
        out.write_char('-')?;
    }

    // `value ≈ digits * 10^(-scale)`, where `digits` has 15 digits
    // (`exponent = 14 - scale`).
    let exponent = 14_i32.saturating_sub(d.scale);
    let abs_val = value.abs();
    if !(GENERAL_MIN_FIXED..EXACT_LIMIT).contains(&abs_val) {
        return write_scientific_15(d.digits, exponent, out);
    }

    // Strip trailing zeros from the 15-digit significand.
    let mut sig = d.digits;
    let mut scale = d.scale;
    while scale > 0 && sig.is_multiple_of(10) {
        sig /= 10;
        scale = scale.saturating_sub(1);
    }

    if scale <= 0 {
        write!(out, "{sig}")?;
        for _ in 0..scale.saturating_neg() {
            out.write_char('0')?;
        }
        return Ok(());
    }

    // `scale > 0`: decimal point sits `scale` digits from the right of `sig`.
    let mut digits_buf = [b'0'; 20];
    let mut len = 0usize;
    let mut rem = sig;
    while rem > 0 {
        let digit = u8::try_from(rem % 10).unwrap_or(0);
        if let Some(slot) = digits_buf.get_mut(len) {
            *slot = b'0'.saturating_add(digit);
        }
        len = len.saturating_add(1);
        rem /= 10;
    }

    #[expect(clippy::cast_sign_loss, reason = "`scale > 0`, checked above")]
    let frac_digits = scale as usize;
    if len <= frac_digits {
        out.write_str("0.")?;
        for _ in 0..frac_digits.saturating_sub(len) {
            out.write_char('0')?;
        }
        for idx in (0..len).rev() {
            if let Some(ch) = digits_buf.get(idx) {
                out.write_char(char::from(*ch))?;
            }
        }
    } else {
        let int_digits = len.saturating_sub(frac_digits);
        for idx in (len.saturating_sub(int_digits)..len).rev() {
            if let Some(ch) = digits_buf.get(idx) {
                out.write_char(char::from(*ch))?;
            }
        }
        out.write_char('.')?;
        for idx in (0..frac_digits).rev() {
            if let Some(ch) = digits_buf.get(idx) {
                out.write_char(char::from(*ch))?;
            }
        }
    }
    Ok(())
}

fn write_scientific_15(digits: u64, exponent: i32, out: &mut String) -> core::fmt::Result {
    let mut sig = digits;
    while sig >= 10 && sig.is_multiple_of(10) {
        sig /= 10;
    }
    let mut rev = [b'0'; 16];
    let mut len = 0usize;
    while sig > 0 {
        let d = u8::try_from(sig % 10).unwrap_or(0);
        if let Some(slot) = rev.get_mut(len) {
            *slot = b'0'.saturating_add(d);
        }
        len = len.saturating_add(1);
        sig /= 10;
    }
    if let Some(lead) = len.checked_sub(1).and_then(|i| rev.get(i)) {
        out.write_char(char::from(*lead))?;
    }
    if len > 1 {
        out.write_char('.')?;
        for idx in (0..len.saturating_sub(1)).rev() {
            if let Some(ch) = rev.get(idx) {
                out.write_char(char::from(*ch))?;
            }
        }
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    let abs_exp = exponent.unsigned_abs();
    write!(out, "E{sign}{abs_exp:02}")
}

/// Writes `value` with a fixed number of decimals, rounding halves away from
/// zero on the 15-digit decimal value.
fn write_scaled(value: f64, decimals: u32, out: &mut String) -> core::fmt::Result {
    if !value.is_finite() {
        return write!(out, "{value}");
    }
    #[expect(
        clippy::cast_possible_wrap,
        reason = "`decimals <= 10` for all callers"
    )]
    let places = decimals as i32;
    let rounded = round_decimal(value, places, RoundingMode::HalfAwayFromZero).unwrap_or(value);
    let scale = 10_u64.pow(decimals);
    let scaled = num::round(rounded * to_f64(scale));
    if scaled.is_nan() || scaled.abs() >= EXACT_LIMIT {
        return write_general(value, out);
    }
    let magnitude = to_u64(scaled.abs());
    if scaled < 0.0 && magnitude != 0 {
        out.write_char('-')?;
    }
    let whole = magnitude.checked_div(scale).unwrap_or(0);
    write!(out, "{whole}")?;
    if decimals > 0 {
        let fraction = magnitude.checked_rem(scale).unwrap_or(0);
        write!(out, ".{fraction:0width$}", width = decimals as usize)?;
    }
    Ok(())
}

/// Converts a 1900-system serial number (`1` = `1900-01-01`, `60` =
/// `1900-02-29`, `2_958_465` = `9999-12-31`) to `(year, month, day)`.
#[must_use]
pub fn serial_to_ymd(serial: f64) -> Option<(i32, u8, u8)> {
    const FIRST: f64 = 1.0;
    const LAST: f64 = 2_958_465.0; // 9999-12-31
    if !serial.is_finite() {
        return None;
    }
    let day = num::floor(serial);
    if !(FIRST..=LAST).contains(&day) {
        return None;
    }
    let serial_int = to_i64(day);
    if serial_int <= 31 {
        return Some((1900, 1, u8::try_from(serial_int).ok()?));
    }
    if serial_int <= 59 {
        return Some((1900, 2, u8::try_from(serial_int.saturating_sub(31)).ok()?));
    }
    if serial_int == 60 {
        // Historical 1900-02-29 preserved for Excel / XLSX compatibility.
        return Some((1900, 2, 29));
    }
    let days_since_epoch = serial_int.saturating_sub(25_569);
    let (year, month, day_of_month) = civil_from_days(days_since_epoch);
    Some((
        i32::try_from(year).ok()?,
        u8::try_from(month).ok()?,
        u8::try_from(day_of_month).ok()?,
    ))
}

/// Converts a calendar date `(year, month, day)` in `1900..=9999` to an Excel
/// 1900-system serial number, returning `None` if the date does not exist
/// (`1900-02-29` is accepted as `60.0`).
#[must_use]
pub fn ymd_to_serial(year: i32, month: u8, day: u8) -> Option<f64> {
    if !(1900..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    if year == 1900 && month == 2 && day == 29 {
        return Some(60.0);
    }
    let max_day = days_in_month(year, month);
    if day > max_day {
        return None;
    }
    if year == 1900 && month == 1 {
        return Some(f64::from(day));
    }
    if year == 1900 && month == 2 {
        return Some(f64::from(31_u8.saturating_add(day)));
    }
    let days = days_from_civil(i64::from(year), i64::from(month), i64::from(day));
    let serial = days.saturating_add(25_569);
    #[expect(
        clippy::cast_precision_loss,
        reason = "serials in `61..=2_958_465` fit in `f64` with 30+ bits to spare"
    )]
    Some(serial as f64)
}

const fn is_gregorian_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

const fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_gregorian_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn write_date(serial: f64, out: &mut String) -> core::fmt::Result {
    if let Some((year, month, day)) = serial_to_ymd(serial) {
        write!(out, "{year:04}-{month:02}-{day:02}")
    } else {
        write_general(serial, out)
    }
}

/// Proleptic Gregorian date of a day count from `1970-01-01`
/// (Howard Hinnant's `civil_from_days`).
#[expect(
    clippy::arithmetic_side_effects,
    reason = "no step can overflow: `days` lies inside `[-25508, 2932896]`"
)]
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Day count from `1970-01-01` for a proleptic Gregorian date
/// (Howard Hinnant's `days_from_civil`).
#[expect(
    clippy::arithmetic_side_effects,
    reason = "year is bounded to `1900..=9999`, so every term fits comfortably in `i64`"
)]
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let yoe = adjusted_year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[expect(clippy::cast_precision_loss, reason = "a power of ten up to 100")]
const fn to_f64(small: u64) -> f64 {
    small as f64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the argument is a non-negative whole number below 1e15"
)]
const fn to_u64(whole: f64) -> u64 {
    whole as u64
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the argument is a whole number within the date range"
)]
const fn to_i64(whole: f64) -> i64 {
    whole as i64
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use super::{NumberFormat, serial_to_ymd, write_number, ymd_to_serial};

    fn text(value: f64, format: NumberFormat) -> String {
        let mut out = String::new();
        write_number(value, format, &mut out);
        out
    }

    #[test]
    fn general_and_fixed() {
        assert_eq!(text(0.0, NumberFormat::General), "0");
        assert_eq!(text(-0.0, NumberFormat::General), "0");
        assert_eq!(text(100_042.0, NumberFormat::General), "100042");
        assert_eq!(text(-17.0, NumberFormat::General), "-17");
        assert_eq!(text(1.23456, NumberFormat::General), "1.23456");
        assert_eq!(text(0.1 + 0.2, NumberFormat::General), "0.3");
        assert_eq!(text(1.25e16, NumberFormat::General), "1.25E+16");
        assert_eq!(text(-5.0e-5, NumberFormat::General), "-5E-05");
        assert_eq!(text(12.5, NumberFormat::Fixed2), "12.50");
        assert_eq!(text(1234.005, NumberFormat::Fixed2), "1234.01");
        assert_eq!(text(1.005, NumberFormat::Fixed2), "1.01");
        assert_eq!(text(-0.004, NumberFormat::Fixed2), "0.00");
        assert_eq!(text(-0.005, NumberFormat::Fixed2), "-0.01");
        assert_eq!(text(0.07, NumberFormat::Fixed2), "0.07");
    }

    #[test]
    fn percent() {
        assert_eq!(text(0.13, NumberFormat::Percent), "13%");
        assert_eq!(text(0.0, NumberFormat::Percent), "0%");
        assert_eq!(text(1.0, NumberFormat::Percent), "100%");
    }

    #[test]
    fn dates_include_1900_leap_bug_and_round_trip() {
        assert_eq!(text(1.0, NumberFormat::Date), "1900-01-01");
        assert_eq!(text(59.0, NumberFormat::Date), "1900-02-28");
        assert_eq!(text(60.0, NumberFormat::Date), "1900-02-29");
        assert_eq!(text(61.0, NumberFormat::Date), "1900-03-01");
        assert_eq!(text(25_569.0, NumberFormat::Date), "1970-01-01");
        assert_eq!(text(36_526.0, NumberFormat::Date), "2000-01-01");
        assert_eq!(text(45_351.0, NumberFormat::Date), "2024-02-29");
        assert_eq!(text(46_342.75, NumberFormat::Date), "2026-11-16");
        assert_eq!(text(2_958_465.0, NumberFormat::Date), "9999-12-31");
        assert_eq!(text(0.0, NumberFormat::Date), "0");
        assert_eq!(text(-1.0, NumberFormat::Date), "-1");

        for (y, m, d, expected) in [
            (1900, 1, 1, 1.0),
            (1900, 2, 28, 59.0),
            (1900, 2, 29, 60.0),
            (1900, 3, 1, 61.0),
            (1970, 1, 1, 25_569.0),
            (2026, 11, 18, 46_344.0),
            (9999, 12, 31, 2_958_465.0),
        ] {
            assert_eq!(ymd_to_serial(y, m, d), Some(expected));
            assert_eq!(serial_to_ymd(expected), Some((y, m, d)));
        }
        assert_eq!(ymd_to_serial(2023, 2, 29), None);
        assert_eq!(ymd_to_serial(1899, 12, 31), None);
    }

    #[test]
    fn out_of_range_values_do_not_panic() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1.0e300,
            -1.0e300,
        ] {
            for format in [
                NumberFormat::General,
                NumberFormat::Fixed2,
                NumberFormat::Percent,
                NumberFormat::Date,
            ] {
                assert!(!text(value, format).is_empty());
            }
        }
    }
}
