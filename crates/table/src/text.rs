//! What a cell shows.

use alloc::string::String;
use core::fmt::Write;

use reticula_core::format::{self, NumberFormat};
use reticula_core::nearest;

use crate::calendar::{self, DAY_MICROS};
use crate::types::{CellValue, ColumnType};

/// Writes what a cell of a column shows. A text cell's text is not known
/// here, and nothing is written for it.
pub fn write_value(kind: ColumnType, value: CellValue, out: &mut String) {
    match (kind, value) {
        (_, CellValue::Blank | CellValue::Text) => {}
        (_, CellValue::Float(number)) => {
            format::write_number(number, NumberFormat::General, out);
        }
        (ColumnType::Decimal { scale, fixed }, CellValue::Int(mantissa)) => {
            write_decimal(i128::from(mantissa), scale, fixed, out);
        }
        (ColumnType::Date, CellValue::Int(days)) => write_date(days, out),
        (ColumnType::DateTime, CellValue::Int(micros)) => write_date_time(micros, out),
        (ColumnType::Boolean, CellValue::Int(truth)) => {
            out.push_str(if truth == 0 { "FALSE" } else { "TRUE" });
        }
        (ColumnType::Integer | ColumnType::Float | ColumnType::Text, CellValue::Int(number)) => {
            let _ = write!(out, "{number}");
        }
    }
}

/// Writes the whole of what a cell holds: like [`write_value`], except that
/// a floating-point number has as many digits as it takes to tell it from the
/// numbers next to it.
pub fn write_exact(kind: ColumnType, value: CellValue, out: &mut String) {
    let start = out.len();
    write_value(kind, value, out);
    if let CellValue::Float(number) = value
        && out
            .get(start..)
            .and_then(|text| nearest::parse(text.as_bytes()))
            != Some(number)
    {
        out.truncate(start);
        let size = if number < 0.0 { -number } else { number };
        if (1e-9..1e21).contains(&size) {
            let _ = write!(out, "{number}");
        } else {
            // With the sign of the exponent, as numbers are shown elsewhere.
            let _ = write!(out, "{number:E}");
            let sign = out.get(start..).and_then(|text| {
                let at = text.find('E')?.checked_add(1)?;
                (!text.get(at..)?.starts_with('-')).then(|| start.saturating_add(at))
            });
            if let Some(at) = sign {
                out.insert(at, '+');
            }
        }
    }
}

/// Writes an integer divided by ten to the power of `scale`, with all of the
/// decimals or without the zeros at the end.
pub fn write_decimal(mantissa: i128, scale: u8, fixed: bool, out: &mut String) {
    let mut scale = u32::from(scale);
    let mut mantissa = mantissa;
    if !fixed {
        while scale > 0 && mantissa.checked_rem(10) == Some(0) {
            mantissa = mantissa.checked_div(10).unwrap_or(0);
            scale = scale.saturating_sub(1);
        }
    }
    if mantissa < 0 {
        out.push('-');
    }
    let magnitude = mantissa.unsigned_abs();
    let factor = 10_u128.checked_pow(scale).unwrap_or(u128::MAX);
    let whole = magnitude.checked_div(factor).unwrap_or(0);
    let _ = write!(out, "{whole}");
    if scale > 0 {
        let rest = magnitude.checked_rem(factor).unwrap_or(0);
        let width = usize::try_from(scale).unwrap_or(0);
        let _ = write!(out, ".{rest:0width$}");
    }
}

/// Writes a date as `yyyy-mm-dd`.
pub fn write_date(days: i64, out: &mut String) {
    let (year, month, day) = calendar::civil_from_days(days);
    let _ = write!(out, "{year:04}-{month:02}-{day:02}");
}

/// Writes a date with its time as `yyyy-mm-dd hh:mm:ss`, with as many
/// decimals of a second as it has, in threes.
pub fn write_date_time(micros: i64, out: &mut String) {
    write_date(micros.div_euclid(DAY_MICROS), out);
    let of_day = micros.rem_euclid(DAY_MICROS);
    let seconds = of_day.div_euclid(1_000_000);
    let fraction = of_day.rem_euclid(1_000_000);
    let (minutes, second) = (seconds.div_euclid(60), seconds.rem_euclid(60));
    let (hour, minute) = (minutes.div_euclid(60), minutes.rem_euclid(60));
    let _ = write!(out, " {hour:02}:{minute:02}:{second:02}");
    if fraction != 0 {
        if fraction.rem_euclid(1_000) == 0 {
            let _ = write!(out, ".{:03}", fraction.div_euclid(1_000));
        } else {
            let _ = write!(out, ".{fraction:06}");
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    fn shown(kind: ColumnType, value: CellValue) -> String {
        let mut out = String::new();
        write_value(kind, value, &mut out);
        out
    }

    #[test]
    fn numbers() {
        let decimal = |fixed| ColumnType::Decimal { scale: 2, fixed };
        assert_eq!(
            shown(ColumnType::Integer, CellValue::Int(i64::MIN)),
            "-9223372036854775808"
        );
        assert_eq!(shown(decimal(true), CellValue::Int(59_450)), "594.50");
        assert_eq!(shown(decimal(false), CellValue::Int(59_450)), "594.5");
        assert_eq!(shown(decimal(false), CellValue::Int(59_400)), "594");
        assert_eq!(shown(decimal(true), CellValue::Int(-5)), "-0.05");
        assert_eq!(shown(decimal(false), CellValue::Int(0)), "0");
        assert_eq!(
            shown(
                ColumnType::Decimal {
                    scale: 18,
                    fixed: true
                },
                CellValue::Int(i64::MAX)
            ),
            "9.223372036854775807"
        );
        assert_eq!(shown(ColumnType::Float, CellValue::Float(0.1)), "0.1");
        assert_eq!(shown(ColumnType::Boolean, CellValue::Int(1)), "TRUE");
        assert_eq!(shown(ColumnType::Integer, CellValue::Blank), "");
    }

    #[test]
    fn every_digit_of_a_number() {
        let exact = |number: f64| {
            let mut out = String::from("x");
            write_exact(ColumnType::Float, CellValue::Float(number), &mut out);
            let text = out.split_off(1);
            assert_eq!(text.parse::<f64>(), Ok(number), "{text}");
            text
        };
        assert_eq!(exact(0.1), "0.1");
        assert_eq!(exact(0.1234567890123456), "0.1234567890123456");
        assert_eq!(exact(1_234_567_890.123_456_7), "1234567890.1234567");
        assert_eq!(exact(-0.1 - 0.2), "-0.30000000000000004");
        assert_eq!(exact(9.361_259_524_962_352e298), "9.361259524962352E+298");
        assert_eq!(exact(1.234_567_890_123_456_7e-12), "1.2345678901234567E-12");
        for number in [
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            1e21,
            123_456_789_012_345_680.0,
        ] {
            exact(number);
            exact(-number);
        }
        let mut out = String::new();
        write_exact(ColumnType::Integer, CellValue::Int(7), &mut out);
        assert_eq!(out, "7");
    }

    #[test]
    fn times() {
        assert_eq!(shown(ColumnType::Date, CellValue::Int(0)), "1970-01-01");
        assert_eq!(shown(ColumnType::Date, CellValue::Int(-1)), "1969-12-31");
        assert_eq!(
            shown(ColumnType::DateTime, CellValue::Int(-1)),
            "1969-12-31 23:59:59.999999"
        );
        assert_eq!(
            shown(ColumnType::DateTime, CellValue::Int(1_500_000)),
            "1970-01-01 00:00:01.500"
        );
        assert_eq!(
            shown(ColumnType::DateTime, CellValue::Int(0)),
            "1970-01-01 00:00:00"
        );
    }
}
