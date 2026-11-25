//! Values as they are written in delimited text.

use core::cmp::Ordering;

use reticula_core::nearest;

use crate::calendar::{self, DAY_MICROS};
use crate::types::{ColumnType, MAX_TEXT_BYTES};

/// The powers of ten that fit into 64 bits.
const POWERS: [u64; 20] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
];

/// The powers of ten that are floating-point numbers without rounding.
const EXACT_POWERS: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Most digits of a decimal number, before and after the point together.
pub(super) const DECIMAL_DIGITS: u32 = 18;
/// Ten to the power of [`DECIMAL_DIGITS`].
const DECIMAL_LIMIT: u64 = 1_000_000_000_000_000_000;
/// So many digits always survive the conversion to floating point.
const SAFE_DIGITS: u32 = 15;
/// The first number that has more digits than that.
pub(super) const SAFE_LIMIT: u64 = 1_000_000_000_000_000;
/// The whole numbers below are floating-point numbers without rounding.
const EXACT_LIMIT: u64 = 1 << 53;

const HIGH: u64 = 0x8080_8080_8080_8080;
const ZEROS: u64 = 0x3030_3030_3030_3030;
const POINTS: u64 = 0x2e2e_2e2e_2e2e_2e2e;

/// Ten to a power of at most 19.
pub(super) fn power(exponent: u32) -> u64 {
    POWERS.get(exponent as usize).copied().unwrap_or(u64::MAX)
}

/// How many digits a number has. Zero has none.
pub(super) const fn digit_count(number: u64) -> u32 {
    match number.checked_ilog10() {
        Some(log) => log.saturating_add(1),
        None => 0,
    }
}

/// The highest bit of every byte that is not zero.
pub(super) const fn nonzero(word: u64) -> u64 {
    ((word & !HIGH).wrapping_add(!HIGH) | word) & HIGH
}

/// The highest bit of every byte of a word of digits, less the digit zero,
/// that is not one. Right up to the first such byte.
const fn not_digits(word: u64) -> u64 {
    (word.wrapping_add(0x7676_7676_7676_7676) | word) & HIGH
}

/// At most eight bytes as a word, the first one lowest.
fn word_of(text: &[u8]) -> u64 {
    if let Some(all) = text.first_chunk::<8>() {
        return u64::from_le_bytes(*all);
    }
    let len = text.len();
    if let (Some(first), Some(last)) = (text.first_chunk::<4>(), text.last_chunk::<4>()) {
        let first = u64::from(u32::from_le_bytes(*first));
        let last = u64::from(u32::from_le_bytes(*last));
        return first | last.wrapping_shl(bits_of(len.wrapping_sub(4)));
    }
    let byte = |index: usize| {
        u64::from(text.get(index).copied().unwrap_or(0)).wrapping_shl(bits_of(index))
    };
    byte(0) | byte(len >> 1) | byte(len.saturating_sub(1))
}

/// Bits of so many bytes of a word.
fn bits_of(bytes: usize) -> u32 {
    u32::try_from(bytes & 7).unwrap_or(0) << 3
}

/// The number that one to eight digits make, which are the lowest bytes of a word.
fn eight(word: u64, len: usize) -> Option<u64> {
    // The bytes that are not used are shifted out, and zeros come in front.
    let digits = (word ^ ZEROS).wrapping_shl(bits_of(8_usize.wrapping_sub(len)));
    if not_digits(digits) != 0 {
        return None;
    }
    let pairs = digits.wrapping_mul(10).wrapping_add(digits >> 8);
    const LANES: u64 = 0x0000_00ff_0000_00ff;
    let high = (pairs & LANES).wrapping_mul(0x000f_4240_0000_0064);
    let low = (pairs >> 16 & LANES).wrapping_mul(0x0000_2710_0000_0001);
    Some(high.wrapping_add(low) >> 32)
}

/// The number that at most 19 digits make.
fn digits(text: &[u8]) -> Option<u64> {
    let (first, rest) = text.as_rchunks::<8>();
    if text.is_empty() || text.len() > 19 {
        return None;
    }
    let mut value = match first.len() {
        0 => 0,
        len => eight(word_of(first), len)?,
    };
    for chunk in rest {
        value = value
            .wrapping_mul(100_000_000)
            .wrapping_add(eight(u64::from_le_bytes(*chunk), 8)?);
    }
    Some(value)
}

fn signed(magnitude: u64, negative: bool) -> Option<i64> {
    if negative {
        0_i64.checked_sub_unsigned(magnitude)
    } else {
        i64::try_from(magnitude).ok()
    }
}

const fn minus(text: &[u8]) -> (bool, &[u8]) {
    match text {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, text),
    }
}

/// A whole number without zeros in front.
pub(super) fn integer(text: &[u8]) -> Option<i64> {
    let (negative, number) = minus(text);
    if number.first() == Some(&b'0') && (negative || number.len() > 1) {
        return None;
    }
    signed(digits(number)?, negative)
}

/// The digits of a number as one number, and how many come after the point.
fn unscaled(number: &[u8]) -> Option<(u64, u32)> {
    let len = number.len();
    if number.first() == Some(&b'0') && len > 1 && number.get(1) != Some(&b'.') {
        return None;
    }
    if len <= 8 {
        let word = word_of(number);
        let points = !nonzero(word ^ POINTS) & HIGH;
        let (mut digits, mut count, mut fraction) = (word, len, 0);
        if points != 0 {
            // The digits after the point move up to those before it.
            let shift = points.trailing_zeros() & 56;
            let before = 1_u64.wrapping_shl(shift).wrapping_sub(1);
            digits = (word & before) | (word >> 8 & !before);
            count = len.wrapping_sub(1);
            fraction = count.wrapping_sub((shift >> 3) as usize);
            if shift == 0 || fraction == 0 {
                return None;
            }
        }
        if count == 0 {
            return None;
        }
        return Some((eight(digits, count)?, u32::try_from(fraction).ok()?));
    }
    let Some(point) = number.iter().position(|byte| *byte == b'.') else {
        return Some((digits(number)?, 0));
    };
    let (whole, fraction) = number.split_at_checked(point)?;
    let fraction = fraction.get(1..)?;
    // With one digit more there could be too many for 64 bits.
    if len > 20 {
        return None;
    }
    let places = u32::try_from(fraction.len()).ok()?;
    let value = digits(whole)?
        .wrapping_mul(power(places))
        .wrapping_add(digits(fraction)?);
    Some((value, places))
}

/// A number times a power of ten. `None` when that has more digits than a
/// decimal number.
fn times_power(value: u64, exponent: u32) -> Option<u64> {
    let limit = power(DECIMAL_DIGITS.checked_sub(exponent)?);
    (value < limit).then(|| value.wrapping_mul(power(exponent)))
}

/// A number with or without decimals times ten to the power of `scale`, and
/// how many decimals it has. `None` when that has more digits than a decimal
/// number, or `scale` is too small.
pub(super) fn decimal(text: &[u8], scale: u32) -> Option<(i64, u32)> {
    let (negative, number) = minus(text);
    let (value, fraction) = unscaled(number)?;
    let scaled = times_power(value, scale.checked_sub(fraction)?)?;
    if negative && scaled == 0 {
        return None;
    }
    Some((signed(scaled, negative)?, fraction))
}

/// A number as it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Number {
    pub(super) negative: bool,
    /// Whether it is written the one way that shows it: without a plus sign,
    /// without zeros in front, and not as a zero with a minus sign.
    pub(super) plain: bool,
    /// Whether there is a point.
    pub(super) point: bool,
    /// Digits before the point, without zeros in front.
    pub(super) whole: u32,
    /// Digits after the point.
    pub(super) fraction: u32,
    /// Whether there is an exponent, and which.
    pub(super) exponent: Option<i64>,
    /// The digits as one number. Right when there are at most 19.
    pub(super) value: u64,
    /// Digits from the first one on that is not zero.
    pub(super) digits: u32,
    /// Those up to the last one that is not zero,
    pub(super) significant: u32,
    /// which as one number are this.
    pub(super) kept: u64,
}

impl Number {
    /// Reads any number, however it is written. `None` when it is not one.
    pub(super) fn read(text: &[u8]) -> Option<Self> {
        if text.len() > MAX_TEXT_BYTES {
            return None;
        }
        let (negative, plus, rest) = match text {
            [b'-', rest @ ..] => (true, false, rest),
            [b'+', rest @ ..] => (false, true, rest),
            _ => (false, false, text),
        };
        let mut number = Self {
            negative,
            plain: true,
            point: false,
            whole: 0,
            fraction: 0,
            exponent: None,
            value: 0,
            digits: 0,
            significant: 0,
            kept: 0,
        };
        // Digits since the start or the point.
        let mut run = 0_u32;
        let mut whole_run = 0_u32;
        let mut bytes = rest.iter();
        while let Some(byte) = bytes.next() {
            let digit = byte.wrapping_sub(b'0');
            if digit <= 9 {
                run = run.saturating_add(1);
                if digit > 0 || number.digits > 0 {
                    number.digits = number.digits.saturating_add(1);
                    if number.digits <= 19 {
                        number.value = number.value.wrapping_mul(10).wrapping_add(u64::from(digit));
                    }
                    if digit > 0 {
                        number.significant = number.digits;
                        number.kept = number.value;
                    }
                }
            } else if *byte == b'.' && !number.point && run > 0 {
                number.point = true;
                number.whole = number.digits;
                whole_run = run;
                run = 0;
            } else if matches!(*byte, b'e' | b'E') && run > 0 {
                number.exponent = Some(exponent(bytes.as_slice())?);
                break;
            } else {
                return None;
            }
        }
        if run == 0 {
            return None;
        }
        if number.point {
            number.fraction = run;
        } else {
            number.whole = number.digits;
            whole_run = run;
        }
        let padded = whole_run > 1 && rest.first() == Some(&b'0');
        let zero = number.significant == 0;
        number.plain = !plus && !padded && !(negative && zero);
        Some(number)
    }

    /// Whether the number comes back, digit for digit, from the
    /// floating-point number that is nearest to it. `text` is what it was
    /// read from.
    pub(super) fn is_lossless(&self, text: &[u8]) -> bool {
        if self.significant == 0 {
            return true;
        }
        // The powers of ten of the last digit that counts, and of the first.
        let last = self
            .exponent
            .unwrap_or(0)
            .saturating_sub(i64::from(self.fraction))
            .saturating_add(i64::from(self.digits.wrapping_sub(self.significant)));
        let first = last.saturating_add(i64::from(self.significant.wrapping_sub(1)));
        // Fewer digits survive in the numbers close to zero, and none beyond
        // the largest one.
        if self.significant <= SAFE_DIGITS && (-307..=307).contains(&first) {
            return true;
        }
        if self.significant > 17 {
            return false;
        }
        match (self.to_float(text), i32::try_from(last)) {
            (Some(value), Ok(last)) if value != 0.0 => is_shortest(self.kept, last, value),
            _ => false,
        }
    }

    /// What the number is. One that is not kept the way it is written is text.
    #[inline(never)]
    pub(super) fn class(&self, text: &[u8]) -> Class {
        if !self.plain {
            return Class::Text;
        }
        let lossless = self.is_lossless(text);
        if self.exponent.is_some() {
            Class::Exponent { lossless }
        } else if self.point {
            Class::Decimal {
                whole: self.whole,
                fraction: self.fraction,
                lossless,
            }
        } else {
            match self.to_kind(ColumnType::Integer) {
                Some(value) => Class::Integer { value, lossless },
                // 64 bits do not hold it.
                None => Class::Text,
            }
        }
    }

    /// The floating-point number that is nearest. `None` when that is infinite.
    #[inline(never)]
    pub(super) fn to_float(self, text: &[u8]) -> Option<f64> {
        // With both operands exact, the one operation rounds correctly.
        if self.digits <= 19 && self.value <= EXACT_LIMIT {
            let exponent = self
                .exponent
                .unwrap_or(0)
                .saturating_sub(i64::from(self.fraction));
            let power = usize::try_from(exponent.unsigned_abs())
                .ok()
                .and_then(|power| EXACT_POWERS.get(power));
            if let Some(power) = power {
                let value = self.value as f64;
                let value = if exponent < 0 {
                    value / power
                } else {
                    value * power
                };
                return Some(if self.negative { -value } else { value });
            }
        }
        parse_float(text)
    }

    /// The integer that stands for the number in a column of integers or
    /// decimals, when that can hold it. Zeros at the end do not matter.
    pub(super) fn to_kind(self, kind: ColumnType) -> Option<i64> {
        let (scale, limit) = match kind {
            ColumnType::Integer => (0, u64::MAX),
            ColumnType::Decimal { scale, .. } => (u32::from(scale), DECIMAL_LIMIT),
            _ => return None,
        };
        if self.digits > 19 || self.exponent.is_some() {
            return None;
        }
        let scaled = match scale.checked_sub(self.fraction) {
            Some(0) => self.value,
            Some(more) => times_power(self.value, more)?,
            None => {
                let less = power(self.fraction.saturating_sub(scale).min(19));
                if self.value.checked_rem(less) != Some(0) {
                    return None;
                }
                self.value.checked_div(less)?
            }
        };
        if scaled >= limit {
            return None;
        }
        signed(scaled, self.negative)
    }
}

fn exponent(text: &[u8]) -> Option<i64> {
    let (negative, digits) = match text {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0_i64;
    for byte in digits {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value.saturating_mul(10).saturating_add(i64::from(digit));
    }
    Some(if negative {
        value.saturating_neg()
    } else {
        value
    })
}

fn parse_float(text: &[u8]) -> Option<f64> {
    nearest::parse(text).filter(|value| value.is_finite())
}

/// Whether `digits` times ten to the power of `last` is what a floating-point
/// number is written as with as few digits as read back as that number, and
/// as close to it as so many digits can be. Where two are as close, either is
/// right. The number is the nearest one to it, and the last digit is no zero.
fn is_shortest(digits: u64, last: i32, value: f64) -> bool {
    let bits = value.abs().to_bits();
    let fraction = bits & (EXACT_LIMIT >> 1).wrapping_sub(1);
    let exponent = i32::try_from(bits >> 52).unwrap_or(0);
    let (mantissa, two) = if exponent == 0 {
        (fraction, -1_074)
    } else {
        (fraction | EXACT_LIMIT >> 1, exponent.wrapping_sub(1_075))
    };
    // How a number of tenths of the last digit compares with one of quarters
    // of the last bit.
    let compare = |tenths: u64, quarters: u64| {
        let mut left = Big::new(tenths);
        let mut right = Big::new(quarters);
        let ten = last.wrapping_sub(1);
        let two = two.wrapping_sub(2);
        let tens = if ten < 0 { &mut right } else { &mut left };
        tens.multiply(10, 9, ten.unsigned_abs());
        let twos = if two < 0 { &mut left } else { &mut right };
        twos.multiply(2, 31, two.unsigned_abs());
        left.compare(&right)
    };
    let quarters = mantissa.wrapping_mul(4);
    let tenths = digits.wrapping_mul(10);
    // What reads back as the number: what is nearer to it than to the next
    // one, and where it is even, what is as near. The next smaller one is only
    // half as far away at a power of two.
    let shared = mantissa & 1 == 0;
    let gap = if fraction == 0 && exponent > 1 { 1 } else { 2 };
    let is_above_low = |tenths: u64| {
        let order = compare(tenths, quarters.wrapping_sub(gap));
        order.is_gt() || (shared && order.is_eq())
    };
    let is_below_high = |tenths: u64| {
        let order = compare(tenths, quarters.wrapping_add(2));
        order.is_lt() || (shared && order.is_eq())
    };
    if compare(tenths.wrapping_add(5), quarters).is_lt()
        || (compare(tenths.wrapping_sub(5), quarters).is_gt()
            && is_above_low(tenths.wrapping_sub(10)))
    {
        return false;
    }
    if digits < 10 {
        return true;
    }
    // Nor do the two around it with one digit less read back.
    let below = tenths.wrapping_sub(tenths.checked_rem(100).unwrap_or(0));
    !is_above_low(below) && !is_below_high(below.wrapping_add(100))
}

fn low_half(word: u64) -> u32 {
    u32::try_from(word & 0xffff_ffff).unwrap_or(0)
}

/// A whole number of at most 1,280 bits, the lowest of them first.
struct Big {
    limbs: [u32; 40],
    len: usize,
}

impl Big {
    fn new(number: u64) -> Self {
        let mut big = Self {
            limbs: [0; 40],
            len: 2,
        };
        for (limb, half) in big.limbs.iter_mut().zip([number, number >> 32]) {
            *limb = low_half(half);
        }
        big
    }

    /// Multiplies with a power, `most` factors of which fit into 32 bits.
    #[inline(never)]
    fn multiply(&mut self, base: u32, most: u32, mut exponent: u32) {
        while exponent > 0 {
            let some = exponent.min(most);
            exponent = exponent.wrapping_sub(some);
            let factor = u64::from(base.wrapping_pow(some));
            let mut carry = 0_u64;
            for limb in self.limbs.iter_mut().take(self.len) {
                let word = u64::from(*limb).wrapping_mul(factor).wrapping_add(carry);
                *limb = low_half(word);
                carry = word >> 32;
            }
            if carry > 0
                && let Some(limb) = self.limbs.get_mut(self.len)
            {
                *limb = low_half(carry);
                self.len = self.len.wrapping_add(1);
            }
        }
    }

    fn compare(&self, other: &Self) -> Ordering {
        // Above their length both are zero.
        self.limbs.iter().rev().cmp(other.limbs.iter().rev())
    }
}

/// Year, month and day of `YYYY-MM-DD`.
fn dashed(text: &[u8; 10]) -> Option<[i64; 3]> {
    let [head @ .., d1, d0] = text;
    // Digits less the digit zero, and dashes less dashes.
    let word = u64::from_le_bytes(*head) ^ u64::from_le_bytes(*b"0000-00-");
    let last = u16::from_le_bytes([*d1, *d0]) ^ 0x3030;
    let wrong = (word.wrapping_add(0x7f76_767f_7676_7676) | word) & HIGH;
    let wrong_last = (last.wrapping_add(0x7676) | last) & 0x8080;
    if wrong != 0 || wrong_last != 0 {
        return None;
    }
    let [y3, y2, y1, y0, _, m1, m0, _] = word.to_le_bytes().map(i64::from);
    let [d1, d0] = last.to_le_bytes().map(i64::from);
    let two = |high: i64, low: i64| high.wrapping_mul(10).wrapping_add(low);
    let year = two(y3, y2).wrapping_mul(100).wrapping_add(two(y1, y0));
    Some([year, two(m1, m0), two(d1, d0)])
}

/// Year, month and day of `YYYY/M/D`.
#[cold]
#[inline(never)]
fn slashed(text: &[u8]) -> Option<[i64; 3]> {
    let mut parts = text.splitn(3, |byte| *byte == b'/');
    let mut date = [0; 3];
    for (slot, lens) in date.iter_mut().zip([4..=4, 1..=2, 1..=2]) {
        let part = parts.next()?;
        if !lens.contains(&part.len()) {
            return None;
        }
        *slot = i64::try_from(digits(part)?).ok()?;
    }
    Some(date)
}

/// A date, in days since 1970-01-01.
pub(super) fn date(text: &[u8]) -> Option<i64> {
    let [year, month, day] = match text.first_chunk::<10>() {
        Some(all) if text.len() == 10 && text.get(4) == Some(&b'-') => dashed(all)?,
        _ => slashed(text)?,
    };
    calendar::days_from_civil(year, month, day)
}

/// The time of day `hh:mm:ss`, in microseconds.
fn time_of_day(text: &[u8; 8]) -> Option<i64> {
    // Digits less the digit zero, and colons less colons.
    let word = u64::from_le_bytes(*text) ^ u64::from_le_bytes(*b"00:00:00");
    if (word.wrapping_add(0x7676_7f76_767f_7676) | word) & HIGH != 0 {
        return None;
    }
    let [h1, h0, _, m1, m0, _, s1, s0] = word.to_le_bytes().map(i64::from);
    let [hour, minute, second] =
        [(h1, h0), (m1, m0), (s1, s0)].map(|(high, low)| high.wrapping_mul(10).wrapping_add(low));
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds = hour
        .wrapping_mul(3_600)
        .wrapping_add(minute.wrapping_mul(60))
        .wrapping_add(second);
    Some(seconds.wrapping_mul(1_000_000))
}

/// The time of day `hh:mm[:ss[.f]]` with up to six decimals and perhaps a Z
/// after it, in microseconds.
fn time(text: &[u8]) -> Option<i64> {
    let text = text.strip_suffix(b"Z").unwrap_or(text);
    let mut whole = *b"00:00:00";
    let fraction = match text.len() {
        // Where there are no seconds, they are zero.
        len @ (5 | 8) => {
            whole.get_mut(..len)?.copy_from_slice(text);
            0
        }
        _ => {
            let (seconds, fraction) = text.split_first_chunk::<8>()?;
            whole = *seconds;
            let digits = digits(fraction.strip_prefix(b".")?)?;
            let more = 7_usize.checked_sub(fraction.len())?;
            let micros = digits.wrapping_mul(power(u32::try_from(more).ok()?));
            i64::try_from(micros).ok()?
        }
    };
    time_of_day(&whole)?.checked_add(fraction)
}

/// A date with a time of day, in microseconds since 1970-01-01 00:00.
pub(super) fn date_time(text: &[u8]) -> Option<i64> {
    // A date takes eight to ten bytes, and mostly the latter.
    let ends = |at: &usize| matches!(text.get(*at), Some(b' ' | b'T'));
    let (day, rest) = text.split_at_checked([10, 9, 8].into_iter().find(ends)?)?;
    let micros = time(rest.get(1..)?)?;
    start_of_day(day)?.checked_add(micros)
}

/// The microsecond a date begins with.
pub(super) fn start_of_day(text: &[u8]) -> Option<i64> {
    // There are not so many days that this could be too much.
    Some(date(text)?.wrapping_mul(DAY_MICROS))
}

/// A truth value.
pub(super) fn boolean(text: &[u8]) -> Option<i64> {
    const TRUE: u32 = u32::from_le_bytes(*b"true");
    const FALS: u32 = u32::from_le_bytes(*b"fals");
    // Makes letters small.
    let small = |letters: &[u8; 4]| u32::from_le_bytes(*letters) | 0x2020_2020;
    match text {
        [letters @ .., b'e' | b'E'] if text.len() == 5 => {
            let letters = letters.first_chunk::<4>()?;
            (small(letters) == FALS).then_some(0)
        }
        _ => {
            let letters = text.first_chunk::<4>().filter(|_| text.len() == 4)?;
            (small(letters) == TRUE).then_some(1)
        }
    }
}

/// The texts that stand for a blank cell, where such texts are asked for.
const BLANK_MARKERS: [&[u8]; 9] = [
    b"NA", b"na", b"N/A", b"n/a", b"NULL", b"null", b"NaN", b"nan", b"NAN",
];

/// Whether a text is one of those.
#[inline(never)]
pub(super) fn is_blank_marker(text: &[u8]) -> bool {
    BLANK_MARKERS.contains(&text)
}

/// What a text is, taken alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    /// A whole number, and whether it survives the conversion to floating point.
    Integer {
        value: i64,
        lossless: bool,
    },
    /// A number with a point and without an exponent: digits before the point
    /// without zeros in front, digits after it, and the same as above.
    Decimal {
        whole: u32,
        fraction: u32,
        lossless: bool,
    },
    /// A number with an exponent.
    Exponent {
        lossless: bool,
    },
    Date,
    DateTime,
    Boolean,
    Text,
}

/// Says what a text that is not empty is.
pub(super) fn classify(text: &[u8]) -> Class {
    match text.first() {
        Some(b'0'..=b'9') => {
            if matches!(text.get(4), Some(b'-' | b'/')) {
                if date(text).is_some() {
                    return Class::Date;
                }
                if date_time(text).is_some() {
                    return Class::DateTime;
                }
            }
            classify_number(text)
        }
        Some(b'-') => classify_number(text),
        Some(b't' | b'T' | b'f' | b'F') if boolean(text).is_some() => Class::Boolean,
        _ => Class::Text,
    }
}

fn classify_number(text: &[u8]) -> Class {
    // Most are whole numbers.
    if let Some(value) = integer(text) {
        return Class::Integer {
            value,
            lossless: value.unsigned_abs() < SAFE_LIMIT
                || Number::read(text).is_some_and(|number| number.is_lossless(text)),
        };
    }
    Number::read(text).map_or(Class::Text, |number| number.class(text))
}
