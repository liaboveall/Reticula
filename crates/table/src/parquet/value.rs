//! What values become: from how a file stores them to the cells of a table.

use alloc::string::String;
use core::fmt::Write;

use crate::calendar::{DAY_MICROS, FIRST_DAY, LAST_DAY};
use crate::types::{ColumnType, MAX_SCALE};

/// Longest number of a decimal column, in bytes: 308 digits.
pub(super) const MAX_DECIMAL_BYTES: usize = 128;
/// Most digits of a decimal column.
pub(super) const MAX_PRECISION: i64 = 308;
/// 1970-01-01 as a Julian day.
const EPOCH_JULIAN: i64 = 2_440_588;
const FIRST_MICROS: i64 = FIRST_DAY.saturating_mul(DAY_MICROS);
const LAST_MICROS: i64 = LAST_DAY
    .saturating_add(1)
    .saturating_mul(DAY_MICROS)
    .saturating_sub(1);

/// The value is as it was.
pub(super) const KEPT: u8 = 0;
/// Nanoseconds were cut off it.
pub(super) const CUT_NANOS: u8 = 1;
/// It is a blank: no date or time a table can hold.
pub(super) const OUT_OF_RANGE: u8 = 2;
/// It is a blank: not a finite number.
pub(super) const NOT_FINITE: u8 = 4;
/// The column has to become text to hold it.
pub(super) const TOO_WIDE: u8 = 8;
/// The value is a blank.
pub(super) const BLANK: u8 = OUT_OF_RANGE | NOT_FINITE;

/// How a file stores the values of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Physical {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    Bytes,
    Fixed(u32),
}

impl Physical {
    /// Whether the values are numbers of at most 64 bits, not bytes.
    pub(super) const fn is_number(self) -> bool {
        matches!(
            self,
            Self::Boolean | Self::Int32 | Self::Int64 | Self::Float | Self::Double
        )
    }

    /// Bytes of a value, where all have the same number.
    pub(super) const fn width(self) -> Option<usize> {
        match self {
            Self::Boolean | Self::Bytes => None,
            Self::Int32 | Self::Float => Some(4),
            Self::Int64 | Self::Double => Some(8),
            Self::Int96 => Some(12),
            Self::Fixed(len) => Some(len as usize),
        }
    }
}

/// What a column says its values mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Logical {
    None,
    Text,
    Decimal {
        scale: i64,
        precision: i64,
    },
    Date,
    /// Units of a second.
    Time(i64),
    /// Units of a second.
    Timestamp(i64),
    Integer {
        signed: bool,
    },
    Null,
    Bson,
    Uuid,
    Half,
    Interval,
    Variant,
    Geometry,
    /// Nothing known here, or nothing for a column that is not nested.
    Other,
}

/// How the values of a column become cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// Every cell is blank.
    Blank,
    Int,
    Unsigned32,
    Unsigned64,
    Day,
    /// Units of a second.
    Timestamp(i64),
    Float,
    Half,
    Int96,
    /// The number is in big-endian bytes.
    DecimalBytes,
    /// Bytes as they are.
    Text,
    Uuid,
    /// Big-endian bytes, shown with so many decimals.
    DecimalBytesText(u32),
    /// A number, shown with so many decimals.
    DecimalText(u32),
    Unsigned64Text,
    /// Units of a second.
    TimeText(i64),
}

impl Plan {
    /// Whether the cells are texts.
    pub(super) const fn is_text(self) -> bool {
        matches!(
            self,
            Self::Text
                | Self::Uuid
                | Self::DecimalBytesText(_)
                | Self::DecimalText(_)
                | Self::Unsigned64Text
                | Self::TimeText(_)
        )
    }

    /// Whether the cells are floating-point numbers.
    pub(super) const fn is_float(self) -> bool {
        matches!(self, Self::Float | Self::Half)
    }

    /// Whether only the values tell whether the column can be taken this way.
    pub(super) const fn is_uncertain(self) -> bool {
        matches!(self, Self::Unsigned64 | Self::DecimalBytes | Self::Text)
    }

    /// The plan for a column whose values do not all fit this one.
    pub(super) const fn as_text(self, kind: ColumnType) -> Self {
        match (self, kind) {
            (Self::Unsigned64, _) => Self::Unsigned64Text,
            (Self::DecimalBytes, ColumnType::Decimal { scale, .. }) => {
                Self::DecimalBytesText(scale as u32)
            }
            (other, _) => other,
        }
    }
}

/// Why a column is not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Its values are lists, maps or structures.
    Nested,
    /// Its values are bytes that are not said to be text.
    Binary,
    /// Its values are said to be text, or could be, and one of them is not
    /// valid UTF-8.
    NotText,
    /// It is packed in a way that cannot be read; the number of the codec.
    Codec(i32),
    /// It is encoded in a way that cannot be read; the number of the encoding.
    Encoding(i32),
    /// Its values are intervals of time.
    Interval,
    /// Its values are BSON documents.
    Bson,
    /// Its values are variants.
    Variant,
    /// Its values are shapes.
    Geometry,
    /// Its type is not known here.
    UnknownType,
    /// What its type says contradicts itself.
    InvalidType,
    /// Its values lie in another file.
    External,
}

/// How a column is to be read, and what it becomes.
pub(super) fn plan(physical: Physical, logical: Logical) -> Result<(ColumnType, Plan), SkipReason> {
    let reason = match logical {
        Logical::Bson => SkipReason::Bson,
        Logical::Interval => SkipReason::Interval,
        Logical::Variant => SkipReason::Variant,
        Logical::Geometry => SkipReason::Geometry,
        Logical::Other => SkipReason::UnknownType,
        Logical::None if matches!(physical, Physical::Fixed(_)) => SkipReason::Binary,
        _ => SkipReason::InvalidType,
    };
    let planned = match (physical, logical) {
        (_, Logical::Null) => Some((ColumnType::Text, Plan::Blank)),
        (Physical::Boolean, Logical::None) => Some((ColumnType::Boolean, Plan::Int)),
        (Physical::Int32 | Physical::Int64, Logical::None)
        | (Physical::Int32 | Physical::Int64, Logical::Integer { signed: true }) => {
            Some((ColumnType::Integer, Plan::Int))
        }
        (Physical::Int32, Logical::Integer { signed: false }) => {
            Some((ColumnType::Integer, Plan::Unsigned32))
        }
        (Physical::Int64, Logical::Integer { signed: false }) => {
            Some((ColumnType::Integer, Plan::Unsigned64))
        }
        (Physical::Int32, Logical::Date) => Some((ColumnType::Date, Plan::Day)),
        (Physical::Int32, Logical::Time(units @ 1_000))
        | (Physical::Int64, Logical::Time(units)) => {
            Some((ColumnType::Text, Plan::TimeText(units)))
        }
        (Physical::Int64, Logical::Timestamp(units)) => {
            Some((ColumnType::DateTime, Plan::Timestamp(units)))
        }
        (Physical::Int96, Logical::None) => Some((ColumnType::DateTime, Plan::Int96)),
        (Physical::Float | Physical::Double, Logical::None) => {
            Some((ColumnType::Float, Plan::Float))
        }
        (Physical::Fixed(2), Logical::Half) => Some((ColumnType::Float, Plan::Half)),
        (Physical::Fixed(16), Logical::Uuid) => Some((ColumnType::Text, Plan::Uuid)),
        (Physical::Bytes, Logical::None | Logical::Text)
        | (Physical::Fixed(1..), Logical::Text) => Some((ColumnType::Text, Plan::Text)),
        (_, Logical::Decimal { scale, precision }) => decimal(physical, scale, precision),
        _ => None,
    };
    planned.ok_or(reason)
}

fn decimal(physical: Physical, scale: i64, precision: i64) -> Option<(ColumnType, Plan)> {
    if !(1..=MAX_PRECISION).contains(&precision) || !(0..=precision).contains(&scale) {
        return None;
    }
    let decimals = u32::try_from(scale).ok()?;
    let in_bytes = match physical {
        Physical::Int32 | Physical::Int64 => false,
        Physical::Bytes => true,
        Physical::Fixed(len) => {
            if !(1..=MAX_DECIMAL_BYTES).contains(&(len as usize)) {
                return None;
            }
            true
        }
        _ => return None,
    };
    if precision <= i64::from(MAX_SCALE) {
        let kind = ColumnType::Decimal {
            scale: u8::try_from(scale).ok()?,
            fixed: true,
        };
        let plan = if in_bytes {
            Plan::DecimalBytes
        } else {
            Plan::Int
        };
        return Some((kind, plan));
    }
    let plan = if in_bytes {
        Plan::DecimalBytesText(decimals)
    } else {
        Plan::DecimalText(decimals)
    };
    Some((ColumnType::Text, plan))
}

/// The low 32 bits as a signed number.
pub(super) fn sign_extended(value: u64) -> i64 {
    i64::from(low_half(value).cast_signed())
}

/// Turns a value as it is stored, a number or bytes, into the cell it becomes,
/// and says what happened to it. A text is written to `text`.
///
/// Not part of the loop around it, which would become one loop for each plan.
#[inline(never)]
pub(super) fn convert(
    plan: Plan,
    physical: Physical,
    value: u64,
    bytes: &[u8],
    text: &mut String,
) -> (u64, u8) {
    let signed = if physical == Physical::Int32 {
        sign_extended(value)
    } else {
        value.cast_signed()
    };
    let kept = |happened| (0, happened);
    match plan {
        Plan::Unsigned64 if signed < 0 => kept(TOO_WIDE),
        Plan::Unsigned64 => (value, KEPT),
        Plan::Day => {
            let known = (FIRST_DAY..=LAST_DAY).contains(&signed);
            (
                signed.cast_unsigned(),
                if known { KEPT } else { OUT_OF_RANGE },
            )
        }
        Plan::Timestamp(1_000) => moment(signed.checked_mul(1_000), true),
        Plan::Timestamp(1_000_000) => moment(Some(signed), true),
        Plan::Timestamp(_) => moment(
            Some(signed.div_euclid(1_000)),
            signed.rem_euclid(1_000) == 0,
        ),
        Plan::Float if physical == Physical::Float => {
            finite(f64::from(f32::from_bits(low_half(value))))
        }
        Plan::Float => finite(f64::from_bits(value)),
        Plan::DecimalBytes => match small(bytes) {
            Some(value) => (value.cast_unsigned(), KEPT),
            None => kept(TOO_WIDE),
        },
        Plan::Half => match bytes.first_chunk() {
            Some(half) => finite(from_half(u16::from_le_bytes(*half))),
            None => kept(NOT_FINITE),
        },
        Plan::Int96 => {
            let Some((nanos, rest)) = bytes.split_first_chunk::<8>() else {
                return kept(OUT_OF_RANGE);
            };
            let julian = rest.first_chunk().map_or(0, |day| u32::from_le_bytes(*day));
            let nanos = i64::from_le_bytes(*nanos);
            let days = i64::from(julian).wrapping_sub(EPOCH_JULIAN);
            let micros = days
                .checked_mul(DAY_MICROS)
                .and_then(|micros| micros.checked_add(nanos.div_euclid(1_000)));
            moment(micros, nanos.rem_euclid(1_000) == 0)
        }
        Plan::Unsigned64Text => {
            let _ = write!(text, "{value}");
            kept(KEPT)
        }
        Plan::DecimalText(scale) => kept(write_decimal(&signed.to_be_bytes(), scale, text)),
        Plan::DecimalBytesText(scale) => kept(write_decimal(bytes, scale, text)),
        Plan::TimeText(units) => kept(write_time(signed, units, text)),
        Plan::Uuid => {
            for (index, byte) in bytes.iter().enumerate() {
                if matches!(index, 4 | 6 | 8 | 10) {
                    text.push('-');
                }
                let _ = write!(text, "{byte:02x}");
            }
            kept(KEPT)
        }
        Plan::Blank | Plan::Int | Plan::Unsigned32 | Plan::Text => (value, KEPT),
    }
}

fn low_half(value: u64) -> u32 {
    u32::try_from(value & 0xffff_ffff).unwrap_or(0)
}

/// A moment in microseconds, and whether that is all of it.
fn moment(micros: Option<i64>, whole: bool) -> (u64, u8) {
    let micros = micros.unwrap_or(i64::MAX);
    let happened = if !(FIRST_MICROS..=LAST_MICROS).contains(&micros) {
        OUT_OF_RANGE
    } else if whole {
        KEPT
    } else {
        CUT_NANOS
    };
    (micros.cast_unsigned(), happened)
}

fn finite(number: f64) -> (u64, u8) {
    (
        number.to_bits(),
        if number.is_finite() { KEPT } else { NOT_FINITE },
    )
}

/// A number of 16 bits: a sign, five bits of exponent, ten of fraction.
fn from_half(half: u16) -> f64 {
    let sign = u64::from(half >> 15) << 63;
    let exponent = u64::from((half >> 10) & 0x1f);
    let fraction = u64::from(half & 0x3ff);
    let magnitude = match exponent {
        // Multiples of two to the power of -24.
        0 => f64::from(half & 0x3ff) * f64::from_bits(0x3e70_0000_0000_0000),
        31 => {
            if fraction == 0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => f64::from_bits((exponent.wrapping_add(1_008) << 52) | (fraction << 42)),
    };
    f64::from_bits(magnitude.to_bits() | sign)
}

/// Leaves out the bytes at the start that only repeat the sign.
fn without_sign_bytes(bytes: &[u8]) -> &[u8] {
    let mut rest = bytes;
    while let [first, after @ ..] = rest
        && let [second, ..] = after
        && ((*first == 0 && *second < 0x80) || (*first == 0xff && *second >= 0x80))
    {
        rest = after;
    }
    rest
}

/// A big-endian number, when it has at most 64 bits.
fn small(bytes: &[u8]) -> Option<i64> {
    let bytes = without_sign_bytes(bytes);
    if bytes.len() > 8 {
        return None;
    }
    let negative = bytes.first().is_some_and(|first| *first >= 0x80);
    let mut value = if negative { -1_i64 } else { 0 };
    for byte in bytes {
        value = value.wrapping_shl(8) | i64::from(*byte);
    }
    Some(value)
}

/// Writes a big-endian number with so many decimals, all of its digits.
fn write_decimal(bytes: &[u8], scale: u32, out: &mut String) -> u8 {
    let bytes = without_sign_bytes(bytes);
    if bytes.len() > MAX_DECIMAL_BYTES {
        return OUT_OF_RANGE;
    }
    let negative = bytes.first().is_some_and(|first| *first >= 0x80);
    // Its magnitude, in parts of 32 bits, the lowest first.
    let mut parts = [0_u32; MAX_DECIMAL_BYTES / 4];
    let mut carry = negative;
    for (slot, four) in parts.iter_mut().zip(bytes.rchunks(4)) {
        let mut part = if negative { u32::MAX } else { 0 };
        for byte in four {
            part = part.wrapping_shl(8) | u32::from(*byte);
        }
        if negative {
            (part, carry) = (!part).overflowing_add(u32::from(carry));
        }
        *slot = part;
    }
    let mut used = parts.as_mut_slice();
    // Nine digits at a time, the lowest first.
    let mut digits = [0_u8; 324];
    let mut slots = digits.iter_mut();
    loop {
        while let [rest @ .., 0] = used {
            used = rest;
        }
        if used.is_empty() {
            break;
        }
        let mut rest = 0_u64;
        for part in used.iter_mut().rev() {
            let value = rest.wrapping_shl(32) | u64::from(*part);
            *part = u32::try_from(value.checked_div(1_000_000_000).unwrap_or(0)).unwrap_or(0);
            rest = value.checked_rem(1_000_000_000).unwrap_or(0);
        }
        for slot in slots.by_ref().take(9) {
            *slot = u8::try_from(rest.checked_rem(10).unwrap_or(0)).unwrap_or(0);
            rest = rest.checked_div(10).unwrap_or(0);
        }
    }
    let significant = digits
        .iter()
        .rposition(|digit| *digit != 0)
        .map_or(0, |last| last.saturating_add(1));
    let scale = scale as usize;
    let shown = significant.max(scale.saturating_add(1)).min(digits.len());
    if negative {
        out.push('-');
    }
    for (place, digit) in digits.iter().enumerate().take(shown).rev() {
        out.push(char::from(b'0'.wrapping_add(*digit)));
        if place == scale && scale > 0 {
            out.push('.');
        }
    }
    KEPT
}

/// Writes a time of day, which is text, with as many decimals of a second as
/// it has, in threes.
fn write_time(value: i64, units: i64, out: &mut String) -> u8 {
    let seconds = value.div_euclid(units);
    let fraction = value.rem_euclid(units);
    if !(0..=86_400).contains(&seconds) || (seconds == 86_400 && fraction > 0) {
        return OUT_OF_RANGE;
    }
    let (minutes, second) = (seconds.div_euclid(60), seconds.rem_euclid(60));
    let (hour, minute) = (minutes.div_euclid(60), minutes.rem_euclid(60));
    let _ = write!(out, "{hour:02}:{minute:02}:{second:02}");
    let nanos = fraction.wrapping_mul(1_000_000_000_i64.checked_div(units).unwrap_or(1));
    if nanos.rem_euclid(1_000_000) != 0 {
        if nanos.rem_euclid(1_000) == 0 {
            let _ = write!(out, ".{:06}", nanos.div_euclid(1_000));
        } else {
            let _ = write!(out, ".{nanos:09}");
        }
    } else if nanos != 0 {
        let _ = write!(out, ".{:03}", nanos.div_euclid(1_000_000));
    }
    KEPT
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use reticula_core::rng::Rng;
    use std::format;

    fn decimal(bytes: &[u8], scale: u32) -> String {
        let mut out = String::new();
        assert_eq!(write_decimal(bytes, scale, &mut out), KEPT);
        out
    }

    #[test]
    fn digits_of_numbers_of_any_length() {
        let mut rng = Rng::from_seed(12);
        for _ in 0..5_000 {
            let high = u128::from(rng.next_u64()) << 64;
            let value = (high | u128::from(rng.next_u64())).cast_signed() >> rng.next_below(128);
            let scale = rng.next_below(40) as usize;
            let digits = format!("{:0>width$}", value.unsigned_abs(), width = scale + 1);
            let (whole, rest) = digits.split_at(digits.len() - scale);
            let sign = if value < 0 { "-" } else { "" };
            let point = if scale > 0 { "." } else { "" };
            // Bytes that only repeat the sign change nothing.
            let sign_byte = if value < 0 { 0xff } else { 0 };
            let mut bytes: Vec<u8> = Vec::new();
            bytes.resize(rng.next_below(200) as usize, sign_byte);
            bytes.extend_from_slice(&value.to_be_bytes());
            assert_eq!(
                decimal(&bytes, scale as u32),
                format!("{sign}{whole}{point}{rest}")
            );
            assert_eq!(small(&bytes), i64::try_from(value).ok());
        }
    }

    #[test]
    fn the_longest_numbers() {
        assert_eq!(decimal(&[], 0), "0");
        assert_eq!(decimal(&[], 2), "0.00");
        assert_eq!(decimal(&[0xff], 3), "-0.001");
        assert_eq!(decimal(&[0xff], 308), format!("-0.{:0>308}", 1));
        // Two to the power of 1,023 has 308 digits, and so has its negative.
        let mut bytes = [0_u8; MAX_DECIMAL_BYTES];
        bytes[0] = 0x80;
        let least = decimal(&bytes, 0);
        assert_eq!(least.len(), 309);
        assert!(least.starts_with("-898846567431157953864652595394512366"));
        assert!(least.ends_with("39858152417678164812112068608"));
        bytes[0] = 0x7f;
        for byte in &mut bytes[1..] {
            *byte = 0xff;
        }
        let most = decimal(&bytes, 308);
        assert_eq!(most.len(), 310);
        assert!(most.starts_with("0.898846567431157953864652595394512366"));
        let mut long = Vec::from([1]);
        long.extend_from_slice(&bytes);
        assert_eq!(write_decimal(&long, 0, &mut String::new()), OUT_OF_RANGE);
        // As many digits as 128 bytes can have.
        assert_eq!(
            i128::from(i64::MIN).to_string(),
            decimal(&i64::MIN.to_be_bytes(), 0)
        );
    }

    #[test]
    fn numbers_of_sixteen_bits() {
        assert_eq!(from_half(0x3c00), 1.0);
        assert_eq!(from_half(0xc000), -2.0);
        assert_eq!(from_half(0x7bff), 65_504.0);
        assert_eq!(from_half(0x0400), 2.0_f64 / 32_768.0);
        assert_eq!(from_half(0x0001), 1.0 / 16_777_216.0);
        assert_eq!(from_half(0x8000).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(from_half(0xfc00), f64::NEG_INFINITY);
        assert!(from_half(0x7e00).is_nan());
        // They come in order.
        for half in 0..0x7c00_u16 {
            assert!(from_half(half) < from_half(half + 1));
            assert_eq!(from_half(half | 0x8000), -from_half(half));
        }
    }

    #[test]
    fn times_of_day() {
        let time = |value, units| {
            let mut out = String::new();
            let happened = write_time(value, units, &mut out);
            (happened == KEPT).then_some(out)
        };
        let shown = |text: &str| Some(text.to_string());
        assert_eq!(time(0, 1_000), shown("00:00:00"));
        assert_eq!(time(86_399_999, 1_000), shown("23:59:59.999"));
        assert_eq!(time(86_400_000, 1_000), shown("24:00:00"));
        assert_eq!(time(86_400_001, 1_000), None);
        assert_eq!(time(-1, 1_000), None);
        assert_eq!(time(1_500_000, 1_000_000), shown("00:00:01.500"));
        assert_eq!(time(1_500_001, 1_000_000), shown("00:00:01.500001"));
        assert_eq!(time(1, 1_000_000_000), shown("00:00:00.000000001"));
        assert_eq!(time(i64::MAX, 1_000), None);
        assert_eq!(time(i64::MIN, 1_000_000_000), None);
    }

    #[test]
    fn long_decimals_in_numbers_become_text() {
        let means = |scale, precision| Logical::Decimal { scale, precision };
        assert_eq!(
            plan(Physical::Int64, means(3, 19)),
            Ok((ColumnType::Text, Plan::DecimalText(3)))
        );
        let mut text = String::new();
        let minus_five = (-5_i64).cast_unsigned();
        let converted = convert(
            Plan::DecimalText(3),
            Physical::Int64,
            minus_five,
            &[],
            &mut text,
        );
        assert_eq!((converted.1, text.as_str()), (KEPT, "-0.005"));
    }
}
