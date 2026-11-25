//! The floating-point number that is nearest to a decimal number.
//!
//! The standard library does this as well, with a table of powers of five
//! that is larger than everything here and compresses badly, which matters
//! in a module that is downloaded. The results are the same, bit for bit:
//! the nearest number, the one whose last bit is zero where two are as near.
//!
//! Nearly always the product of the digits and a power of ten is worked out
//! with about 100 bits, which settles what is nearest. Where it does not, whole
//! numbers of any length are compared.

use core::cmp::Ordering;

/// An unevaluated sum of two numbers, the second at most half a unit in the
/// last place of the first.
#[derive(Clone, Copy)]
struct Pair {
    high: f64,
    low: f64,
}

impl Pair {
    /// The sum, when the first number is not the smaller one.
    const fn sorted(a: f64, b: f64) -> Self {
        let high = a + b;
        Self {
            high,
            low: b - (high - a),
        }
    }

    /// The exact product.
    const fn product(a: f64, b: f64) -> Self {
        const fn halves(x: f64) -> (f64, f64) {
            let t = 134_217_729.0 * x;
            let high = t - (t - x);
            (high, x - high)
        }
        let high = a * b;
        let (a1, a0) = halves(a);
        let (b1, b0) = halves(b);
        Self {
            high,
            low: ((a1 * b1 - high) + a1 * b0 + a0 * b1) + a0 * b0,
        }
    }

    const fn times(self, other: Self) -> Self {
        let product = Self::product(self.high, other.high);
        let low = product.low + (self.high * other.low + self.low * other.high);
        Self::sorted(product.high, low)
    }

    const fn minus(self, other: Self) -> Self {
        let high = self.high - other.high;
        let back = high - self.high;
        let low = (self.high - (high - back)) - (other.high + back);
        Self::sorted(high, low + (self.low - other.low))
    }

    const fn over(self, other: Self) -> Self {
        let first = self.high / other.high;
        let rest = self.minus(other.times(Self {
            high: first,
            low: 0.0,
        }));
        let second = rest.high / other.high;
        let rest = rest.minus(other.times(Self {
            high: second,
            low: 0.0,
        }));
        let sum = Self::sorted(first, second);
        Self::sorted(sum.high, sum.low + rest.high / other.high)
    }

    /// A number that 53 bits need not hold.
    fn of(number: u64) -> Self {
        let upper = number.wrapping_shr(32) as f64 * 4_294_967_296.0;
        Self::sorted(upper, (number & 0xffff_ffff) as f64)
    }

    /// The nearest number, when that is certain.
    fn rounded(self) -> Option<f64> {
        // What was worked out is off by less than this, by far.
        let off = self.high * 1.262_177_448_353_619e-29; // 2^-96
        let same =
            self.high + (self.low + off) == self.high && self.high + (self.low - off) == self.high;
        same.then_some(self.high)
    }
}

/// The powers of ten that are floating-point numbers.
const EXACT: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Ten to the powers of 0, 23, 46 and so on.
const POWERS: [Pair; 12] = {
    let mut powers = [Pair {
        high: 1.0,
        low: 0.0,
    }; 12];
    let step = Pair::product(1e22, 10.0);
    let mut index = 1;
    while index < powers.len() {
        #[expect(clippy::indexing_slicing, reason = "evaluated by the compiler")]
        let before = powers[index.wrapping_sub(1)];
        #[expect(clippy::indexing_slicing, reason = "evaluated by the compiler")]
        {
            powers[index] = before.times(step);
        }
        index = index.wrapping_add(1);
    }
    powers
};

/// The largest whole number up to which all are floating-point numbers.
const ALL_EXACT: u64 = 1 << 53;

/// A number times ten to a power, when the nearest number is certain.
fn quickly(number: u64, power: i64) -> Option<f64> {
    let size = power.unsigned_abs();
    let steps = usize::try_from(size.checked_div(23)?).ok()?;
    let rest = usize::try_from(size.checked_rem(23)?).ok()?;
    let factor = *EXACT.get(rest)?;
    if steps == 0 && number <= ALL_EXACT {
        // With both of them exact, the one operation rounds correctly.
        let number = number as f64;
        return Some(if power < 0 {
            number / factor
        } else {
            number * factor
        });
    }
    // Beyond these powers the product might come too close to the largest and
    // the smallest numbers.
    let factor = POWERS.get(steps)?.times(Pair {
        high: factor,
        low: 0.0,
    });
    let number = Pair::of(number);
    if power < 0 {
        number.over(factor).rounded()
    } else {
        number.times(factor).rounded()
    }
}

/// Below this another digit fits into 64 bits.
const MOST_AT_ONCE: u64 = 1_000_000_000_000_000_000;

/// Digits that are looked at. No number halfway between two has more.
const MOST_DIGITS: usize = 800;

/// A whole number, its lowest 32 bits first. Of those that occur none is
/// longer than this: 800 digits, or a power of ten with 1,125, and the bits of
/// a power of two that the other side is divided by.
struct Big {
    limbs: [u32; 192],
    len: usize,
}

impl Big {
    fn new(number: u64) -> Self {
        let mut big = Self {
            limbs: [0; 192],
            len: 2,
        };
        for (limb, half) in big
            .limbs
            .iter_mut()
            .zip([number & 0xffff_ffff, number.wrapping_shr(32)])
        {
            *limb = u32::try_from(half).unwrap_or(0);
        }
        big
    }

    fn times_plus(&mut self, factor: u32, more: u32) {
        let mut carry = u64::from(more);
        for limb in self.limbs.iter_mut().take(self.len) {
            let word = u64::from(*limb)
                .wrapping_mul(u64::from(factor))
                .wrapping_add(carry);
            *limb = u32::try_from(word & 0xffff_ffff).unwrap_or(0);
            carry = word.wrapping_shr(32);
        }
        if carry > 0
            && let Some(limb) = self.limbs.get_mut(self.len)
        {
            *limb = u32::try_from(carry).unwrap_or(0);
            self.len = self.len.saturating_add(1);
        }
    }

    /// Multiplies with a power, `most` factors of which fit into 32 bits.
    fn times_power(&mut self, base: u32, most: u32, power: u64) {
        let mut rest = power;
        while rest > 0 {
            let some = u32::try_from(rest).unwrap_or(u32::MAX).min(most);
            self.times_plus(base.wrapping_pow(some), 0);
            rest = rest.saturating_sub(u64::from(some));
        }
    }

    fn compare(&self, other: &Self) -> Ordering {
        // Beyond their length both are zero.
        self.limbs.iter().rev().cmp(other.limbs.iter().rev())
    }
}

/// A decimal number: its digits, of which none but the first may be missing,
/// and the power of ten that belongs to the last of them.
struct Decimal<'a> {
    whole: &'a [u8],
    fraction: &'a [u8],
    power: i64,
}

impl Decimal<'_> {
    fn digits(&self) -> impl Iterator<Item = u8> + '_ {
        self.whole
            .iter()
            .chain(self.fraction)
            .skip_while(|digit| **digit == b'0')
            .map(|digit| digit.wrapping_sub(b'0'))
    }

    /// How this compares with a number of half units in the last place, or
    /// quarters or whatever `two` says.
    fn compare(&self, number: u64, two: i64) -> Ordering {
        let mut left = Big::new(0);
        let mut seen = 0_usize;
        let mut power = self.power;
        let (mut chunk, mut unit) = (0_u32, 1_u32);
        let mut rest = false;
        for digit in self.digits() {
            if seen < MOST_DIGITS {
                seen = seen.saturating_add(1);
                chunk = chunk.wrapping_mul(10).wrapping_add(u32::from(digit));
                unit = unit.wrapping_mul(10);
                if unit == 1_000_000_000 {
                    left.times_plus(unit, chunk);
                    (chunk, unit) = (0, 1);
                }
            } else {
                power = power.saturating_add(1);
                rest |= digit != 0;
            }
        }
        left.times_plus(unit, chunk);
        if rest {
            // More than what was looked at, by less than one of its last digit.
            left.times_plus(10, 1);
            power = power.saturating_sub(1);
        }
        let mut right = Big::new(number);
        let tens = if power < 0 { &mut right } else { &mut left };
        tens.times_power(10, 9, power.unsigned_abs());
        let twos = if two < 0 { &mut left } else { &mut right };
        twos.times_power(2, 31, two.unsigned_abs());
        left.compare(&right)
    }

    /// The nearest number, starting from one that is not far from it.
    #[cold]
    #[inline(never)]
    fn exactly(&self, near: f64) -> f64 {
        let mut value = if near.is_finite() { near } else { f64::MAX };
        for _ in 0..4_096 {
            let bits = value.to_bits();
            let fraction = bits & 0x000f_ffff_ffff_ffff;
            let exponent = i64::try_from(bits.wrapping_shr(52)).unwrap_or(0);
            let (mantissa, two) = if exponent == 0 {
                (fraction, -1_074)
            } else {
                (
                    fraction | 0x0010_0000_0000_0000,
                    exponent.wrapping_sub(1_075),
                )
            };
            let odd = mantissa & 1 == 1;
            // Halfway to the next number.
            let half = mantissa.wrapping_mul(2).wrapping_add(1);
            let above = self.compare(half, two.wrapping_sub(1));
            if above.is_gt() || (above.is_eq() && odd) {
                value = f64::from_bits(bits.wrapping_add(1));
                if value.is_infinite() {
                    return value;
                }
                continue;
            }
            if mantissa == 0 {
                return value;
            }
            // Halfway to the one before, which is closer at a power of two.
            let below = if fraction == 0 && exponent > 1 {
                let quarter = mantissa.wrapping_mul(4).wrapping_sub(1);
                self.compare(quarter, two.wrapping_sub(2))
            } else {
                let half = mantissa.wrapping_mul(2).wrapping_sub(1);
                self.compare(half, two.wrapping_sub(1))
            };
            if below.is_lt() || (below.is_eq() && odd) {
                value = f64::from_bits(bits.wrapping_sub(1));
                continue;
            }
            return value;
        }
        value
    }

    /// The nearest number, infinite when that is beyond all of them.
    fn nearest(&self) -> f64 {
        // The first digits as a number, and how many are left out.
        let mut number = 0_u64;
        let mut left_out = 0_i64;
        let mut rest = false;
        for part in [self.whole, self.fraction] {
            for byte in part {
                let digit = byte.wrapping_sub(b'0');
                if number < MOST_AT_ONCE {
                    number = number.wrapping_mul(10).wrapping_add(u64::from(digit));
                } else {
                    left_out = left_out.saturating_add(1);
                    rest |= digit != 0;
                }
            }
        }
        let power = self.power.saturating_add(left_out);
        match settle(number, rest, power) {
            Ok(value) => value,
            Err(near) => self.exactly(near),
        }
    }
}

/// The number nearest to so many tens to a power, or to somewhat more but less
/// than one more; or one that is not far from it.
fn settle(number: u64, more: bool, power: i64) -> Result<f64, f64> {
    let Some(size) = number.checked_ilog10() else {
        return Ok(0.0);
    };
    // The power of ten of the first digit.
    let first = power.saturating_add(i64::from(size));
    if first > 308 {
        return Ok(f64::INFINITY);
    }
    if first < -326 {
        return Ok(0.0);
    }
    if let Some(value) = quickly(number, power)
        && (!more || quickly(number.saturating_add(1), power) == Some(value))
    {
        return Ok(value);
    }
    Err(roughly(number, power))
}

/// Something near to a number times ten to a power.
#[cold]
#[inline(never)]
fn roughly(number: u64, power: i64) -> f64 {
    let mut near = number as f64;
    let mut size = power.unsigned_abs();
    while size > 0 {
        let some = size.min(22);
        let factor = EXACT.get(usize::try_from(some).unwrap_or(0)).copied();
        let factor = factor.unwrap_or(1.0);
        near = if power < 0 {
            near / factor
        } else {
            near * factor
        };
        size = size.saturating_sub(some);
    }
    near
}

/// The number that is nearest to what is written: digits, with or without a
/// point among them, with or without a sign before and an exponent after. It
/// is infinite when it is beyond the largest number there is.
///
/// `None` when anything else is written.
#[must_use]
pub fn parse(text: &[u8]) -> Option<f64> {
    let (negative, unsigned) = match text {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, text),
    };
    let mantissa_len = unsigned
        .iter()
        .position(|byte| matches!(byte, b'e' | b'E'))
        .unwrap_or(unsigned.len());
    let (mantissa, after) = unsigned.split_at_checked(mantissa_len)?;
    let exponent = match after {
        [] => 0,
        [_, b'-', digits @ ..] => small(digits)?.saturating_neg(),
        [_, b'+', digits @ ..] | [_, digits @ ..] => small(digits)?,
    };
    let point = mantissa.iter().position(|byte| *byte == b'.');
    let (whole, fraction) = match point {
        Some(at) => (mantissa.get(..at)?, mantissa.get(at.saturating_add(1)..)?),
        None => (mantissa, &[][..]),
    };
    let digits = |part: &[u8]| part.iter().all(u8::is_ascii_digit);
    if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) {
        return None;
    }
    let power = exponent.saturating_sub(i64::try_from(fraction.len()).unwrap_or(i64::MAX));
    let value = Decimal {
        whole,
        fraction,
        power,
    }
    .nearest();
    Some(if negative { -value } else { value })
}

/// A number of any length. What is far beyond every exponent is as good as that.
fn small(digits: &[u8]) -> Option<i64> {
    if digits.is_empty() {
        return None;
    }
    let mut value = 0_i64;
    for byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        let digit = i64::from(byte.wrapping_sub(b'0'));
        value = value.saturating_mul(10).saturating_add(digit).min(1 << 40);
    }
    Some(value)
}

/// The number nearest to `significand` times ten to the power of `power`.
#[must_use]
pub fn from_parts(significand: u128, power: i32) -> f64 {
    if let Ok(number) = u64::try_from(significand)
        && let Ok(value) = settle(number, false, i64::from(power))
    {
        return value;
    }
    digit_by_digit(significand, power)
}

#[cold]
#[inline(never)]
fn digit_by_digit(significand: u128, power: i32) -> f64 {
    // At most 39 digits.
    let mut buffer = [b'0'; 40];
    let mut rest = significand;
    for slot in buffer.iter_mut().rev() {
        let digit = u8::try_from(rest.checked_rem(10).unwrap_or(0)).unwrap_or(0);
        *slot = b'0'.wrapping_add(digit);
        rest = rest.checked_div(10).unwrap_or(0);
    }
    Decimal {
        whole: &buffer,
        fraction: &[],
        power: i64::from(power),
    }
    .nearest()
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::unwrap_used,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::format;
    use alloc::string::{String, ToString};

    use super::*;
    use crate::rng::Rng;

    fn check(text: &str) {
        let expected: f64 = text.parse().unwrap();
        let found = parse(text.as_bytes()).unwrap();
        assert_eq!(
            found.to_bits(),
            expected.to_bits(),
            "{text}: {found:e}, not {expected:e}"
        );
    }

    fn rounds(count: u64) -> u64 {
        if cfg!(debug_assertions) || cfg!(target_arch = "wasm32") {
            count / 20
        } else {
            count
        }
    }

    #[test]
    fn what_is_hard_to_read() {
        for text in [
            "0",
            "-0",
            "0.0",
            "1",
            "+1",
            "1.",
            ".5",
            "1e5",
            "1E+5",
            "1e-5",
            "00012.500",
            "0.1",
            "0.2",
            "0.3",
            "123456789012345678",
            "9007199254740993",
            "9007199254740991.5",
            "9007199254740992.5",
            "1.7976931348623157e308",
            "1.7976931348623158e308",
            "1.7976931348623159e308",
            "1.8e308",
            "1e309",
            "1e400",
            "1e-400",
            "1e99999999999",
            "1e-99999999999",
            "4.9406564584124654e-324",
            "2.4703282292062327e-324",
            "2.4703282292062328e-324",
            "2.4703282292062329e-324",
            "2.2250738585072011e-308",
            "2.2250738585072014e-308",
            "2.2250738585072012e-308",
            "6.9294956446009195e15",
            "8.533e+68",
            "4.1006e-184",
            "9.998e+307",
            "9.9538452227e-280",
            "6.47660115e-260",
            "7.4e+47",
            "5.92e+48",
            "7.35e+66",
            "8.32116e+55",
            "677715396045378.7",
            "0.000000000000000000000000000000000000000000000000000001",
            "100000000000000000000000000000000000000000000000000000000",
        ] {
            check(text);
            check(&format!("-{}", text.trim_start_matches(['+', '-'])));
        }
        for text in [
            "", "-", "+", ".", "e5", "1e", "1e+", "1.2.3", "1 ", " 1", "0x1", "inf", "nan", "1_0",
            "--1",
        ] {
            assert_eq!(parse(text.as_bytes()), None, "{text}");
        }
    }

    #[test]
    fn every_kind_of_number_comes_back() {
        let mut rng = Rng::from_seed(1);
        for _ in 0..rounds(2_000_000) {
            let value = f64::from_bits(rng.next_u64());
            if value.is_finite() {
                check(&format!("{value:e}"));
                check(&format!("{:.*e}", rng.next_below(20) as usize, value));
            }
        }
    }

    #[test]
    fn digits_and_powers_at_random() {
        let mut rng = Rng::from_seed(2);
        for _ in 0..rounds(1_000_000) {
            let digits = rng.next_u64() >> rng.next_below(64);
            let power = rng.next_below(700) as i64 - 350;
            check(&format!("{digits}e{power}"));
            let small = rng.next_below(60) as i64 - 30;
            check(&format!("{digits}e{small}"));
            let power = i32::try_from(power).unwrap();
            assert_eq!(
                from_parts(u128::from(digits), power).to_bits(),
                format!("{digits}e{power}")
                    .parse::<f64>()
                    .unwrap()
                    .to_bits()
            );
        }
    }

    #[test]
    fn halfway_between_two_numbers_and_next_to_it() {
        let mut rng = Rng::from_seed(3);
        for round in 0..rounds(20_000) {
            let bits = match round % 4 {
                0 => rng.next_u64() >> 1,
                1 => rng.next_u64() >> 12,              // without exponent
                2 => (rng.next_below(2_046) + 1) << 52, // powers of two
                _ => ((rng.next_below(600) + 800) << 52) | (rng.next_u64() >> 12),
            };
            let value = f64::from_bits(bits);
            let next = f64::from_bits(bits + 1);
            if !next.is_finite() {
                continue;
            }
            // All digits of the number in the middle.
            let exact = |x: f64| format!("{x:.1100}");
            let sum = add(&exact(value), &exact(next));
            let middle = halve(&sum);
            let cut = middle.trim_end_matches('0');
            check(cut);
            check(&format!("{cut}0000000000000000000000000000000001"));
            let last = cut.len() - 1;
            if cut.as_bytes().get(last).is_some_and(u8::is_ascii_digit) {
                check(&format!(
                    "{}4999999999999999999999999999999999",
                    cut.get(..last).unwrap()
                ));
            }
        }
    }

    /// The sum of two numbers with as many decimals.
    fn add(a: &str, b: &str) -> String {
        let width = a.len().max(b.len()) + 1;
        let (a, b) = (format!("{a:0>width$}"), format!("{b:0>width$}"));
        let mut carry = 0;
        let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        for (x, y) in a.bytes().rev().zip(b.bytes().rev()) {
            if x == b'.' {
                out.push(x);
                continue;
            }
            let sum = x - b'0' + (y - b'0') + carry;
            out.push(b'0' + sum % 10);
            carry = sum / 10;
        }
        out.reverse();
        String::from_utf8(out).unwrap()
    }

    /// Half of a number, with one more decimal.
    fn halve(text: &str) -> String {
        let mut rest = 0;
        let mut out = String::new();
        for byte in text.bytes().chain(*b"0") {
            if byte == b'.' {
                out.push('.');
                continue;
            }
            let digit = rest * 10 + (byte - b'0');
            out.push_str(&(digit / 2).to_string());
            rest = digit % 2;
        }
        out
    }

    /// `cargo test --release -p reticula-core -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    #[cfg(not(target_arch = "wasm32"))]
    fn how_long_it_takes() {
        let mut rng = Rng::from_seed(5);
        let texts: alloc::vec::Vec<String> = (0..1_000_000)
            .map(|_| ((rng.next_f64() - 0.5) * 1e18).to_string())
            .collect();
        for _ in 0..2 {
            let start = std::time::Instant::now();
            let here: f64 = texts.iter().filter_map(|text| parse(text.as_bytes())).sum();
            let ours = start.elapsed();
            let start = std::time::Instant::now();
            let there: f64 = texts
                .iter()
                .filter_map(|text| text.parse::<f64>().ok())
                .sum();
            std::println!(
                "{ours:?} here, {:?} there, {}",
                start.elapsed(),
                here - there
            );
        }
        let start = std::time::Instant::now();
        let sum: f64 = (0..1_000_000_u64)
            .map(|index| {
                from_parts(
                    u128::from(index * 900_000_007 + 100_000_000_000_000),
                    (index % 30) as i32 - 20,
                )
            })
            .sum();
        std::println!("{:?} for parts, {sum}", start.elapsed());
    }

    #[test]
    fn many_digits() {
        let mut rng = Rng::from_seed(4);
        for _ in 0..rounds(20_000) {
            let mut text = String::new();
            for _ in 0..=rng.next_below(1_200) {
                text.push(char::from(b'0' + u8::try_from(rng.next_below(10)).unwrap()));
            }
            let point = usize::try_from(rng.next_below(text.len() as u64 + 1)).unwrap();
            text.insert(point, '.');
            if text.len() > 1 {
                check(&format!("{text}e{}", rng.next_below(1_000) as i64 - 500));
            }
        }
    }
}
