//! Numbers and dates as text, 1900-system calendar/time conversions, and
//! format-code formatting (`14-m1b.md` 5.4).
//!
//! The engine formats numbers itself so that every target shows the exact same
//! digits. `General` renders up to 15 significant decimal digits with trailing
//! fractional zeros stripped; date/time formats use the 1900 serial system,
//! preserving the historical `1900-02-29` (`serial = 60`) compatibility rule.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::decimal15::{RoundingMode, digits15, round_decimal};
use crate::digest::Digest64;
use crate::num;

/// Supported currency prefix symbols (`14-m1b.md` 5.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CurrencySymbol {
    /// US Dollar (`$`).
    #[default]
    Usd,
    /// Chinese Yuan / Japanese Yen (`¥`).
    Cny,
    /// Euro (`€`).
    Eur,
}

impl CurrencySymbol {
    /// Display glyph of this currency symbol.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usd => "$",
            Self::Cny => "¥",
            Self::Eur => "€",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Usd => 0,
            Self::Cny => 1,
            Self::Eur => 2,
        }
    }
}

/// How a cell, row or column shows its numbers (`14-m1b.md` 5.4).
///
/// The first four variants (`General`, `Fixed2`, `Percent`, `Date`) retain
/// their M1a discriminant positions (`0..=3`) so `RTCLSNAP` v0 snapshots remain
/// wire-compatible.
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
    /// Whole integer (`0`).
    Integer,
    /// Fixed decimal places (`0..=6`) with optional `,` thousands grouping.
    Fixed {
        /// Fractional decimal digits (`0..=6`).
        decimals: u8,
        /// Whether to insert `,` every three integer digits.
        thousands: bool,
    },
    /// Currency symbol + thousands grouping + `0..=4` decimals.
    Currency {
        /// Leading currency symbol.
        symbol: CurrencySymbol,
        /// Fractional decimal digits (`0..=4`).
        decimals: u8,
    },
    /// Percentage with explicit fractional decimals (`1..=4`, e.g. `0.00%`).
    PercentDecimals {
        /// Fractional decimal digits (`0..=4`).
        decimals: u8,
    },
    /// Scientific notation (`0.00E+00`, `1..=6` fractional mantissa digits).
    Scientific {
        /// Fractional mantissa digits (`1..=6`).
        decimals: u8,
    },
    /// Slash date (`yyyy/mm/dd`).
    DateSlash,
    /// US date (`mm/dd/yyyy`).
    DateUs,
    /// Year and month (`yyyy-mm`).
    DateYearMonth,
    /// ISO date plus 24-hour time (`yyyy-mm-dd hh:mm` or `yyyy-mm-dd hh:mm:ss`).
    DateTime {
        /// Whether to include `:ss`.
        seconds: bool,
    },
    /// 24-hour time (`hh:mm` or `hh:mm:ss`).
    Time {
        /// Whether to include `:ss`.
        seconds: bool,
    },
    /// Plain-text cell format (`@`): numbers present in the cell still display
    /// as `General`, while new interactive input skips numeric/date inference.
    PlainText,
}

impl NumberFormat {
    /// Canonicalises equivalent spellings (`Fixed { decimals: 2, thousands: false }`
    /// -> `Fixed2`, `Fixed { decimals: 0, thousands: false }` -> `Integer`,
    /// `PercentDecimals { decimals: 0 }` -> `Percent`) and clamps bounds.
    #[must_use]
    pub const fn normalize(self) -> Self {
        match self {
            Self::Fixed {
                decimals: 0,
                thousands: false,
            } => Self::Integer,
            Self::Fixed {
                decimals: 2,
                thousands: false,
            } => Self::Fixed2,
            Self::Fixed {
                decimals,
                thousands,
            } => Self::Fixed {
                decimals: if decimals > 6 { 6 } else { decimals },
                thousands,
            },
            Self::Currency { symbol, decimals } => Self::Currency {
                symbol,
                decimals: if decimals > 4 { 4 } else { decimals },
            },
            Self::PercentDecimals { decimals: 0 } => Self::Percent,
            Self::PercentDecimals { decimals } => Self::PercentDecimals {
                decimals: if decimals > 4 { 4 } else { decimals },
            },
            Self::Scientific { decimals } => Self::Scientific {
                decimals: if decimals == 0 {
                    2
                } else if decimals > 6 {
                    6
                } else {
                    decimals
                },
            },
            other => other,
        }
    }

    /// Validates parameter bounds on deserialised formats.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        match self {
            Self::Fixed { decimals, .. } => decimals <= 6,
            Self::Currency { decimals, .. } | Self::PercentDecimals { decimals } => decimals <= 4,
            Self::Scientific { decimals } => decimals >= 1 && decimals <= 6,
            _ => true,
        }
    }

    /// Canonical format code string (compatible with [`parse_format_code`] and
    /// `TEXT()`).
    #[must_use]
    pub const fn code_str(self) -> &'static str {
        match self.normalize() {
            Self::General => "General",
            Self::Integer => "0",
            Self::Fixed2 => "0.00",
            Self::Fixed {
                decimals: 0,
                thousands: true,
            } => "#,##0",
            Self::Fixed {
                decimals: 1,
                thousands: false,
            } => "0.0",
            Self::Fixed {
                decimals: 1,
                thousands: true,
            } => "#,##0.0",
            Self::Fixed {
                decimals: 2,
                thousands: true,
            } => "#,##0.00",
            Self::Fixed {
                decimals: 3,
                thousands: false,
            } => "0.000",
            Self::Fixed {
                decimals: 3,
                thousands: true,
            } => "#,##0.000",
            Self::Fixed {
                thousands: false, ..
            } => "0.0000",
            Self::Fixed {
                thousands: true, ..
            } => "#,##0.0000",
            Self::Currency {
                symbol: CurrencySymbol::Usd,
                decimals: 0,
            } => "$#,##0",
            Self::Currency {
                symbol: CurrencySymbol::Usd,
                ..
            } => "$#,##0.00",
            Self::Currency {
                symbol: CurrencySymbol::Cny,
                decimals: 0,
            } => "¥#,##0",
            Self::Currency {
                symbol: CurrencySymbol::Cny,
                ..
            } => "¥#,##0.00",
            Self::Currency {
                symbol: CurrencySymbol::Eur,
                decimals: 0,
            } => "€#,##0",
            Self::Currency {
                symbol: CurrencySymbol::Eur,
                ..
            } => "€#,##0.00",
            Self::Percent => "0%",
            Self::PercentDecimals { decimals: 1 } => "0.0%",
            Self::PercentDecimals { .. } => "0.00%",
            Self::Scientific { .. } => "0.00E+00",
            Self::Date => "yyyy-mm-dd",
            Self::DateSlash => "yyyy/mm/dd",
            Self::DateUs => "mm/dd/yyyy",
            Self::DateYearMonth => "yyyy-mm",
            Self::DateTime { seconds: false } => "yyyy-mm-dd hh:mm",
            Self::DateTime { seconds: true } => "yyyy-mm-dd hh:mm:ss",
            Self::Time { seconds: false } => "hh:mm",
            Self::Time { seconds: true } => "hh:mm:ss",
            Self::PlainText => "@",
        }
    }

    /// Feeds a deterministic representation of this format into `d`.
    pub fn write_digest(self, d: &mut Digest64) {
        match self.normalize() {
            Self::General => d.write_bytes(&[0]),
            Self::Fixed2 => d.write_bytes(&[1]),
            Self::Percent => d.write_bytes(&[2]),
            Self::Date => d.write_bytes(&[3]),
            Self::Integer => d.write_bytes(&[4]),
            Self::Fixed {
                decimals,
                thousands,
            } => d.write_bytes(&[5, decimals, u8::from(thousands)]),
            Self::Currency { symbol, decimals } => d.write_bytes(&[6, symbol.tag(), decimals]),
            Self::PercentDecimals { decimals } => d.write_bytes(&[7, decimals]),
            Self::Scientific { decimals } => d.write_bytes(&[8, decimals]),
            Self::DateSlash => d.write_bytes(&[9]),
            Self::DateUs => d.write_bytes(&[10]),
            Self::DateYearMonth => d.write_bytes(&[11]),
            Self::DateTime { seconds } => d.write_bytes(&[12, u8::from(seconds)]),
            Self::Time { seconds } => d.write_bytes(&[13, u8::from(seconds)]),
            Self::PlainText => d.write_bytes(&[14]),
        }
    }
}

/// Parses a standard format code string into a [`NumberFormat`].
#[must_use]
pub fn parse_format_code(code: &str) -> Option<NumberFormat> {
    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("general") {
        return Some(NumberFormat::General);
    }
    if trimmed == "@" {
        return Some(NumberFormat::PlainText);
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "yyyy-mm-dd" => return Some(NumberFormat::Date),
        "yyyy/mm/dd" => return Some(NumberFormat::DateSlash),
        "mm/dd/yyyy" => return Some(NumberFormat::DateUs),
        "yyyy-mm" => return Some(NumberFormat::DateYearMonth),
        "yyyy-mm-dd hh:mm" => return Some(NumberFormat::DateTime { seconds: false }),
        "yyyy-mm-dd hh:mm:ss" => return Some(NumberFormat::DateTime { seconds: true }),
        "hh:mm" => return Some(NumberFormat::Time { seconds: false }),
        "hh:mm:ss" => return Some(NumberFormat::Time { seconds: true }),
        _ => {}
    }

    // Scientific: `0.00E+00` / `0.0E+00`
    if let Some(mantissa) = lower
        .strip_suffix("e+00")
        .or_else(|| lower.strip_suffix("e+0"))
        && let Some(dec_part) = mantissa.strip_prefix("0.")
        && !dec_part.is_empty()
        && dec_part.bytes().all(|b| b == b'0')
        && let Ok(decimals) = u8::try_from(dec_part.len())
        && decimals <= 6
    {
        return Some(NumberFormat::Scientific { decimals });
    }

    // Percent: `0%`, `0.0%`, `0.00%`, ...
    if let Some(prefix) = trimmed.strip_suffix('%') {
        if prefix == "0" {
            return Some(NumberFormat::Percent);
        }
        if let Some(dec_part) = prefix.strip_prefix("0.")
            && !dec_part.is_empty()
            && dec_part.bytes().all(|b| b == b'0')
            && let Ok(decimals) = u8::try_from(dec_part.len())
            && decimals <= 4
        {
            return Some(NumberFormat::PercentDecimals { decimals }.normalize());
        }
        return None;
    }

    // Currency: `$#,##0`, `$#,##0.00`, `¥#,##0.00`, `€#,##0.00`
    let (currency, num_body) = if let Some(rest) = trimmed.strip_prefix('$') {
        (Some(CurrencySymbol::Usd), rest)
    } else if let Some(rest) = trimmed.strip_prefix('¥') {
        (Some(CurrencySymbol::Cny), rest)
    } else if let Some(rest) = trimmed.strip_prefix('€') {
        (Some(CurrencySymbol::Eur), rest)
    } else {
        (None, trimmed)
    };

    let (thousands, frac) = if let Some(after) = num_body.strip_prefix("#,##0") {
        (true, after)
    } else {
        let after = num_body.strip_prefix('0')?;
        (false, after)
    };

    let decimals: u8 = if frac.is_empty() {
        0
    } else if let Some(zeros) = frac.strip_prefix('.')
        && !zeros.is_empty()
        && zeros.bytes().all(|b| b == b'0')
        && let Ok(d) = u8::try_from(zeros.len())
    {
        d
    } else {
        return None;
    };

    if let Some(symbol) = currency {
        if decimals <= 4 {
            Some(NumberFormat::Currency { symbol, decimals })
        } else {
            None
        }
    } else if decimals <= 6 {
        Some(
            NumberFormat::Fixed {
                decimals,
                thousands,
            }
            .normalize(),
        )
    } else {
        None
    }
}

/// Formats `value` according to an Excel `TEXT(value, format_text)` pattern
/// string (`14-m1b.md` 5.3 & 5.4), returning `None` if `format_text` is
/// unrecognised (`TEXT` evaluates `None` to `#VALUE!`).
#[must_use]
pub fn format_excel_text(value: f64, format_text: &str) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let sections: Vec<&str> = split_format_sections(format_text);
    if sections.is_empty() || sections.len() > 4 {
        return None;
    }
    let (chosen_sec, abs_mode) = if sections.len() == 1 {
        (*sections.first()?, false)
    } else if sections.len() == 2 {
        if value < 0.0 {
            (*sections.get(1)?, true)
        } else {
            (*sections.first()?, false)
        }
    } else if value > 0.0 {
        (*sections.first()?, false)
    } else if value < 0.0 {
        (*sections.get(1)?, true)
    } else {
        (*sections.get(2)?, true)
    };

    let mut stripped = String::new();
    let mut prefix = String::new();
    let mut suffix = String::new();
    extract_literals(chosen_sec, &mut prefix, &mut stripped, &mut suffix)?;

    if stripped.is_empty() {
        let mut out = prefix;
        out.push_str(&suffix);
        return Some(out);
    }

    let fmt = parse_format_code(&stripped)?;
    let target_val = if abs_mode { value.abs() } else { value };
    let mut body = String::new();
    write_number(target_val, fmt, &mut body);
    let mut out = prefix;
    out.push_str(&body);
    out.push_str(&suffix);
    Some(out)
}

fn split_format_sections(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0usize;
    for (idx, ch) in pattern.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch == ';' && !in_quotes {
            if let Some(seg) = pattern.get(start..idx) {
                out.push(seg);
            }
            start = idx.saturating_add(1);
        }
    }
    if let Some(tail) = pattern.get(start..) {
        out.push(tail);
    }
    out
}

fn extract_literals(
    sec: &str,
    prefix: &mut String,
    core_code: &mut String,
    suffix: &mut String,
) -> Option<()> {
    // Strip simple quoted `"literal"` or parentheses `(...)` wrappers while
    // keeping standard core format tokens intact.
    let mut chars = sec.chars().peekable();
    let mut seen_core = false;
    while let Some(ch) = chars.next() {
        if ch == '"' {
            let dest = if seen_core {
                &mut *suffix
            } else {
                &mut *prefix
            };
            let mut closed = false;
            for qch in chars.by_ref() {
                if qch == '"' {
                    closed = true;
                    break;
                }
                dest.push(qch);
            }
            if !closed {
                return None;
            }
        } else if (ch == '(' || ch == ')' || ch == ' ' || ch == '+' || ch == '-')
            && (!seen_core || (ch == ')' || ch == ' '))
            && !(seen_core && core_code.ends_with(['e', 'E']) && (ch == '+' || ch == '-'))
            && !(seen_core
                && (ch == '-' || ch == ' ')
                && core_code
                    .chars()
                    .last()
                    .is_some_and(|c| matches!(c, 'y' | 'Y' | 'm' | 'M' | 'd' | 'D')))
        {
            if seen_core {
                suffix.push(ch);
            } else {
                prefix.push(ch);
            }
        } else {
            seen_core = true;
            core_code.push(ch);
        }
    }
    Some(())
}

/// Largest magnitude formatted in fixed-point form.
const EXACT_LIMIT: f64 = 1.0e15;

/// Smallest non-zero magnitude formatted in fixed-point `General` form.
const GENERAL_MIN_FIXED: f64 = 1.0e-4;

/// Appends `value`, formatted according to `format`, to `out`.
pub fn write_number(value: f64, format: NumberFormat, out: &mut String) {
    let _ = match format.normalize() {
        NumberFormat::General | NumberFormat::PlainText => write_general(value, out),
        NumberFormat::Integer => write_scaled(value, 0, false, None, out),
        NumberFormat::Fixed2 => write_scaled(value, 2, false, None, out),
        NumberFormat::Fixed {
            decimals,
            thousands,
        } => write_scaled(value, u32::from(decimals), thousands, None, out),
        NumberFormat::Currency { symbol, decimals } => {
            write_scaled(value, u32::from(decimals), true, Some(symbol.as_str()), out)
        }
        NumberFormat::Percent => {
            write_scaled(value * 100.0, 0, false, None, out).and_then(|()| out.write_char('%'))
        }
        NumberFormat::PercentDecimals { decimals } => {
            write_scaled(value * 100.0, u32::from(decimals), false, None, out)
                .and_then(|()| out.write_char('%'))
        }
        NumberFormat::Scientific { decimals } => write_scientific_fixed(value, decimals, out),
        NumberFormat::Date => write_date_variant(value, DateStyle::Iso, out),
        NumberFormat::DateSlash => write_date_variant(value, DateStyle::Slash, out),
        NumberFormat::DateUs => write_date_variant(value, DateStyle::Us, out),
        NumberFormat::DateYearMonth => write_date_variant(value, DateStyle::YearMonth, out),
        NumberFormat::DateTime { seconds } => write_datetime(value, seconds, out),
        NumberFormat::Time { seconds } => write_time_only(value, seconds, out),
    };
}

#[derive(Clone, Copy)]
enum DateStyle {
    Iso,
    Slash,
    Us,
    YearMonth,
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

    let exponent = 14_i32.saturating_sub(d.scale);
    let abs_val = value.abs();
    if !(GENERAL_MIN_FIXED..EXACT_LIMIT).contains(&abs_val) {
        return write_scientific_15(d.digits, exponent, out);
    }

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

fn write_scientific_fixed(value: f64, decimals: u8, out: &mut String) -> core::fmt::Result {
    if !value.is_finite() {
        return write!(out, "{value}");
    }
    let dec = usize::from(decimals.clamp(1, 6));
    if value == 0.0 {
        out.write_str("0.")?;
        for _ in 0..dec {
            out.write_char('0')?;
        }
        return out.write_str("E+00");
    }
    let Some(d) = digits15(value) else {
        return out.write_str("0.00E+00");
    };
    if d.negative {
        out.write_char('-')?;
    }
    // Round the 15-digit significand `d.digits` to `1 + dec` significant digits.
    let keep = u32::try_from(dec.saturating_add(1)).unwrap_or(3);
    let drop_pow = 10_u64.pow(15_u32.saturating_sub(keep));
    let half = drop_pow / 2;
    let mut rounded_sig = d
        .digits
        .saturating_add(half)
        .checked_div(drop_pow)
        .unwrap_or(0);
    let mut exponent = 14_i32.saturating_sub(d.scale);
    let carry_bound = 10_u64.pow(keep);
    if rounded_sig >= carry_bound {
        rounded_sig /= 10;
        exponent = exponent.saturating_add(1);
    }
    let frac_scale = 10_u64.pow(u32::from(decimals.clamp(1, 6)));
    let lead = rounded_sig.checked_div(frac_scale).unwrap_or(0);
    let frac = rounded_sig.checked_rem(frac_scale).unwrap_or(0);
    let sign = if exponent < 0 { '-' } else { '+' };
    let abs_exp = exponent.unsigned_abs();
    write!(out, "{lead}.{frac:0dec$}E{sign}{abs_exp:02}")
}

/// Writes `value` with fixed decimals, optional thousands commas, and optional
/// currency symbol, rounding halves away from zero on the 15-digit decimal value.
fn write_scaled(
    value: f64,
    decimals: u32,
    thousands: bool,
    currency: Option<&str>,
    out: &mut String,
) -> core::fmt::Result {
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
    if let Some(sym) = currency {
        out.write_str(sym)?;
    }
    let whole = magnitude.checked_div(scale).unwrap_or(0);
    if thousands {
        write_u64_with_commas(whole, out)?;
    } else {
        write!(out, "{whole}")?;
    }
    if decimals > 0 {
        let fraction = magnitude.checked_rem(scale).unwrap_or(0);
        write!(out, ".{fraction:0width$}", width = decimals as usize)?;
    }
    Ok(())
}

fn write_u64_with_commas(mut whole: u64, out: &mut String) -> core::fmt::Result {
    if whole < 1_000 {
        return write!(out, "{whole}");
    }
    let mut digits = [b'0'; 20];
    let mut len = 0usize;
    while whole > 0 {
        let d = u8::try_from(whole % 10).unwrap_or(0);
        if let Some(slot) = digits.get_mut(len) {
            *slot = b'0'.saturating_add(d);
        }
        len = len.saturating_add(1);
        whole /= 10;
    }
    for pos in (0..len).rev() {
        if let Some(&b) = digits.get(pos) {
            out.write_char(char::from(b))?;
        }
        if pos > 0 && pos.is_multiple_of(3) {
            out.write_char(',')?;
        }
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

/// Number of days in `(year, month)` under proleptic Gregorian rules (`1..=12`).
#[must_use]
pub const fn days_in_month_gregorian(year: i32, month: u8) -> u8 {
    days_in_month(year, month)
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

/// Splits a non-negative serial into `(whole_day_serial, hour, minute, second)`,
/// carrying into `whole_day_serial + 1` when rounding fractional seconds hits
/// `86,400` (or `1,440` minutes when `with_seconds == false`).
fn split_serial_datetime(serial: f64, with_seconds: bool) -> Option<(f64, u8, u8, u8)> {
    if !serial.is_finite() || serial < 0.0 {
        return None;
    }
    let base_day = num::floor(serial);
    let frac = (serial - base_day).clamp(0.0, 1.0);
    if with_seconds {
        let total_secs = to_u64(num::round(frac * 86_400.0));
        let carry_days = if total_secs >= 86_400 { 1.0 } else { 0.0 };
        let secs_in_day = total_secs % 86_400;
        let hh = u8::try_from(secs_in_day / 3_600).ok()?;
        let mm = u8::try_from((secs_in_day % 3_600) / 60).ok()?;
        let ss = u8::try_from(secs_in_day % 60).ok()?;
        Some((base_day + carry_days, hh, mm, ss))
    } else {
        let total_mins = to_u64(num::round(frac * 1_440.0));
        let carry_days = if total_mins >= 1_440 { 1.0 } else { 0.0 };
        let mins_in_day = total_mins % 1_440;
        let hh = u8::try_from(mins_in_day / 60).ok()?;
        let mm = u8::try_from(mins_in_day % 60).ok()?;
        Some((base_day + carry_days, hh, mm, 0))
    }
}

fn write_date_variant(serial: f64, style: DateStyle, out: &mut String) -> core::fmt::Result {
    let Some((year, month, day)) = serial_to_ymd(serial) else {
        return write_general(serial, out);
    };
    match style {
        DateStyle::Iso => write!(out, "{year:04}-{month:02}-{day:02}"),
        DateStyle::Slash => write!(out, "{year:04}/{month:02}/{day:02}"),
        DateStyle::Us => write!(out, "{month:02}/{day:02}/{year:04}"),
        DateStyle::YearMonth => write!(out, "{year:04}-{month:02}"),
    }
}

fn write_datetime(serial: f64, seconds: bool, out: &mut String) -> core::fmt::Result {
    let Some((day_serial, hh, mm, ss)) = split_serial_datetime(serial, seconds) else {
        return write_general(serial, out);
    };
    let Some((year, month, day)) = serial_to_ymd(day_serial) else {
        return write_general(serial, out);
    };
    if seconds {
        write!(out, "{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        write!(out, "{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}")
    }
}

fn write_time_only(serial: f64, seconds: bool, out: &mut String) -> core::fmt::Result {
    // Allow serial >= 0 (including `0.5` -> `12:00` without a date part).
    let Some((_, hh, mm, ss)) = split_serial_datetime(serial, seconds) else {
        return write_general(serial, out);
    };
    if seconds {
        write!(out, "{hh:02}:{mm:02}:{ss:02}")
    } else {
        write!(out, "{hh:02}:{mm:02}")
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

#[expect(clippy::cast_precision_loss, reason = "a power of ten up to 1e10")]
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

    use super::{
        CurrencySymbol, NumberFormat, format_excel_text, parse_format_code, serial_to_ymd,
        write_number, ymd_to_serial,
    };

    fn text(value: f64, format: NumberFormat) -> String {
        let mut out = String::new();
        write_number(value, format, &mut out);
        out
    }

    #[test]
    fn general_fixed_thousands_currency_and_scientific() {
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

        assert_eq!(
            text(
                1_234_567.891,
                NumberFormat::Fixed {
                    decimals: 2,
                    thousands: true
                }
            ),
            "1,234,567.89"
        );
        assert_eq!(
            text(
                -1_234.5,
                NumberFormat::Currency {
                    symbol: CurrencySymbol::Usd,
                    decimals: 2
                }
            ),
            "-$1,234.50"
        );
        assert_eq!(
            text(
                88_000.0,
                NumberFormat::Currency {
                    symbol: CurrencySymbol::Cny,
                    decimals: 0
                }
            ),
            "¥88,000"
        );
        assert_eq!(
            text(1234.5, NumberFormat::Scientific { decimals: 2 }),
            "1.23E+03"
        );
    }

    #[test]
    fn percent_datetime_and_text_function_codes() {
        assert_eq!(text(0.13, NumberFormat::Percent), "13%");
        assert_eq!(
            text(0.1234, NumberFormat::PercentDecimals { decimals: 2 }),
            "12.34%"
        );
        assert_eq!(
            text(46_345.5, NumberFormat::DateTime { seconds: true }),
            "2026-11-19 12:00:00"
        );
        assert_eq!(text(0.75, NumberFormat::Time { seconds: false }), "18:00");

        assert_eq!(
            parse_format_code("$#,##0.00"),
            Some(NumberFormat::Currency {
                symbol: CurrencySymbol::Usd,
                decimals: 2
            })
        );
        assert_eq!(
            format_excel_text(1234.567, "#,##0.00").as_deref(),
            Some("1,234.57")
        );
        assert_eq!(
            format_excel_text(-1234.5, "$#,##0.00;($#,##0.00)").as_deref(),
            Some("($1,234.50)")
        );
        assert_eq!(
            format_excel_text(46_345.0, "yyyy-mm-dd").as_deref(),
            Some("2026-11-19")
        );
    }

    #[test]
    fn dates_include_1900_leap_bug_and_round_trip() {
        assert_eq!(text(1.0, NumberFormat::Date), "1900-01-01");
        assert_eq!(text(59.0, NumberFormat::Date), "1900-02-28");
        assert_eq!(text(60.0, NumberFormat::Date), "1900-02-29");
        assert_eq!(text(61.0, NumberFormat::Date), "1900-03-01");
        assert_eq!(text(25_569.0, NumberFormat::Date), "1970-01-01");
        assert_eq!(text(45_351.0, NumberFormat::Date), "2024-02-29");
        assert_eq!(text(2_958_465.0, NumberFormat::Date), "9999-12-31");

        for (y, m, d, expected) in [
            (1900, 1, 1, 1.0),
            (1900, 2, 28, 59.0),
            (1900, 2, 29, 60.0),
            (1900, 3, 1, 61.0),
            (1970, 1, 1, 25_569.0),
            (2026, 11, 19, 46_345.0),
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
                NumberFormat::DateTime { seconds: true },
                NumberFormat::Scientific { decimals: 2 },
            ] {
                assert!(!text(value, format).is_empty());
            }
        }
    }
}
