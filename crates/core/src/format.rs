//! Numbers as text.
//!
//! The engine formats numbers itself so that every target shows the same
//! digits. This is the first cut: just enough formats for the sample data.
//! Format codes and the general format arrive with the formula engine.

use alloc::string::String;
use core::fmt::Write as _;

use crate::num;

/// How a column shows its numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumberFormat {
    /// Whole numbers as they are; anything else with two decimals.
    #[default]
    General,
    /// Two decimals.
    Fixed2,
    /// A fraction as a whole percentage: 0.13 as `13%`.
    Percent,
    /// A date serial number as `yyyy-mm-dd`.
    Date,
}

/// Largest magnitude that is formatted digit by digit.
const EXACT_LIMIT: f64 = 1.0e15;

/// Appends `value`, formatted, to `out`.
pub fn write_number(value: f64, format: NumberFormat, out: &mut String) {
    // Writing to a `String` cannot fail.
    let _ = match format {
        NumberFormat::General if value == num::trunc(value) => write_scaled(value, 0, out),
        NumberFormat::General | NumberFormat::Fixed2 => write_scaled(value, 2, out),
        NumberFormat::Percent => {
            write_scaled(value * 100.0, 0, out).and_then(|()| out.write_char('%'))
        }
        NumberFormat::Date => write_date(value, out),
    };
}

/// Writes `value` with a fixed number of decimals, rounding halves away from zero.
fn write_scaled(value: f64, decimals: u32, out: &mut String) -> core::fmt::Result {
    let scale = 10_u64.pow(decimals);
    let scaled = num::round(value * to_f64(scale));
    if scaled.is_nan() || scaled.abs() >= EXACT_LIMIT {
        // Not a number at all, or too large to print exactly digit by digit.
        return write!(out, "{value:e}");
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

/// Writes a date serial number (days, with 1 being 1900-01-01) as a date.
/// Serials before March 1900 are shown as plain numbers: the 1900 date system
/// has a day there that never existed, which this first cut does not model.
fn write_date(serial: f64, out: &mut String) -> core::fmt::Result {
    const FIRST: f64 = 61.0; // 1900-03-01
    const LAST: f64 = 2_958_465.0; // 9999-12-31
    let day = num::floor(serial);
    if !(FIRST..=LAST).contains(&day) {
        return write_scaled(serial, 0, out);
    }
    // Days since 1970-01-01; serial 25569 is that date.
    let days = to_i64(day).saturating_sub(25_569);
    let (year, month, day) = civil_from_days(days);
    write!(out, "{year:04}-{month:02}-{day:02}")
}

/// The proleptic Gregorian date of a day count from 1970-01-01.
/// (Howard Hinnant's `civil_from_days`.)
#[expect(
    clippy::arithmetic_side_effects,
    reason = "no step can overflow: the era is at most 2^63 / 146097, and everything else is               derived from a day within one era"
)]
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days.saturating_add(719_468);
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

    use super::{NumberFormat, write_number};

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
        assert_eq!(text(1.23456, NumberFormat::General), "1.23");
        assert_eq!(text(12.5, NumberFormat::Fixed2), "12.50");
        assert_eq!(text(1234.005, NumberFormat::Fixed2), "1234.01");
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
    fn dates() {
        assert_eq!(text(61.0, NumberFormat::Date), "1900-03-01");
        assert_eq!(text(25_569.0, NumberFormat::Date), "1970-01-01");
        assert_eq!(text(36_526.0, NumberFormat::Date), "2000-01-01");
        assert_eq!(text(45_351.0, NumberFormat::Date), "2024-02-29");
        assert_eq!(text(46_342.75, NumberFormat::Date), "2026-11-16");
        assert_eq!(text(2_958_465.0, NumberFormat::Date), "9999-12-31");
        // Outside the modelled range a serial is shown as a number.
        assert_eq!(text(60.0, NumberFormat::Date), "60");
        assert_eq!(text(-1.0, NumberFormat::Date), "-1");
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
