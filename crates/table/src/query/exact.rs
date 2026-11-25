//! Numbers that people type, compared without rounding them.

use super::spec::NumberOp;

/// Larger than any number a column of integers can hold, whatever its decimals.
const BEYOND: u128 = 1 << 100;

/// A decimal number as it was written.
#[derive(Clone, Copy, Debug)]
pub(super) struct Threshold<'a> {
    negative: bool,
    /// The digits before the point, and after it.
    whole: &'a str,
    fraction: &'a str,
    /// How many places the point has to be moved to the right.
    exponent: i64,
    /// The floating-point number next to it.
    nearest: f64,
}

/// The whole numbers on both sides of a number: the same one twice when it is
/// whole itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Between {
    pub(super) floor: i128,
    pub(super) ceiling: i128,
}

fn all_digits(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_digit())
}

impl<'a> Threshold<'a> {
    /// Reads a number: digits with or without a point in them, with or without
    /// a sign before and an exponent after. `None` when it is anything else.
    pub(super) fn parse(text: &'a str) -> Option<Self> {
        let (negative, unsigned) = match text.as_bytes().first()? {
            b'-' => (true, text.get(1..)?),
            b'+' => (false, text.get(1..)?),
            _ => (false, text),
        };
        let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => {
                let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
                if digits.is_empty() || !all_digits(digits) {
                    return None;
                }
                // Far beyond every number there is, the exact one no longer matters.
                let size = digits.parse::<i64>().unwrap_or(i64::MAX).min(1 << 40);
                let exponent = if exponent.starts_with('-') {
                    0_i64.saturating_sub(size)
                } else {
                    size
                };
                (mantissa, exponent)
            }
            None => (unsigned, 0),
        };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if (whole.is_empty() && fraction.is_empty()) || !all_digits(whole) || !all_digits(fraction)
        {
            return None;
        }
        let nearest = reticula_core::nearest::parse(text.as_bytes())?;
        Some(Self {
            negative,
            whole,
            fraction,
            exponent,
            // There is one zero.
            nearest: if nearest == 0.0 { 0.0 } else { nearest },
        })
    }

    /// The floating-point number next to it, infinite when it is beyond all of them.
    pub(super) const fn nearest(&self) -> f64 {
        self.nearest
    }

    /// The whole numbers around this number times ten to the power of `scale`.
    #[inline(never)]
    pub(super) fn scaled(&self, scale: u8) -> Between {
        let digits = self.whole.bytes().chain(self.fraction.bytes());
        // How many of the digits come before the point.
        let point = i64::try_from(self.whole.len())
            .unwrap_or(i64::MAX)
            .saturating_add(self.exponent)
            .saturating_add(i64::from(scale));
        let mut size = 0_u128;
        let mut rest = false;
        let mut position = 0_i64;
        for digit in digits {
            let digit = digit.wrapping_sub(b'0');
            if position < point {
                size = size
                    .saturating_mul(10)
                    .saturating_add(u128::from(digit))
                    .min(BEYOND);
            } else {
                rest |= digit != 0;
            }
            position = position.saturating_add(1);
        }
        while position < point && size > 0 && size < BEYOND {
            size = size.saturating_mul(10).min(BEYOND);
            position = position.saturating_add(1);
        }
        let size = i128::try_from(size).unwrap_or(i128::MAX);
        let up = size.saturating_add(i128::from(rest));
        if self.negative {
            Between {
                floor: 0_i128.saturating_sub(up),
                ceiling: 0_i128.saturating_sub(size),
            }
        } else {
            Between {
                floor: size,
                ceiling: up,
            }
        }
    }
}

/// The whole numbers from `low` to `high`, or all others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Range<T> {
    pub(super) low: T,
    pub(super) high: T,
    pub(super) inside: bool,
}

impl Range<i64> {
    /// No number.
    pub(super) const NONE: Self = Self {
        low: 1,
        high: 0,
        inside: true,
    };

    /// The numbers from one to another, as far as there are such.
    pub(super) fn clamped(low: i128, high: i128) -> Self {
        if low > high || low > i128::from(i64::MAX) || high < i128::from(i64::MIN) {
            return Self::NONE;
        }
        Self {
            low: i64::try_from(low).unwrap_or(i64::MIN),
            high: i64::try_from(high).unwrap_or(i64::MAX),
            inside: true,
        }
    }
}

/// The integers of a column with so many decimals that an operator holds for.
pub(super) fn integers(
    op: NumberOp,
    a: &Threshold<'_>,
    b: Option<&Threshold<'_>>,
    scale: u8,
) -> Range<i64> {
    let least = i128::MIN;
    let most = i128::MAX;
    let at = a.scaled(scale);
    match op {
        NumberOp::Equal => Range::clamped(at.ceiling, at.floor),
        NumberOp::NotEqual => Range {
            inside: false,
            ..Range::clamped(at.ceiling, at.floor)
        },
        NumberOp::Greater => Range::clamped(at.floor.saturating_add(1), most),
        NumberOp::AtLeast => Range::clamped(at.ceiling, most),
        NumberOp::Less => Range::clamped(least, at.ceiling.saturating_sub(1)),
        NumberOp::AtMost => Range::clamped(least, at.floor),
        NumberOp::Between => {
            let to = b.map_or(at, |b| b.scaled(scale));
            // Whichever is the smaller one comes first.
            Range::clamped(at.ceiling.min(to.ceiling), at.floor.max(to.floor))
        }
    }
}

/// The floating-point numbers an operator holds for, as order keys.
///
/// A threshold stands for the floating-point number next to it. Numbers below
/// that one are below the threshold itself, and numbers above are above, so
/// this rounds nothing but says which number a threshold is equal to.
pub(super) fn floats(op: NumberOp, a: &Threshold<'_>, b: Option<&Threshold<'_>>) -> Range<u64> {
    let at = float_key(a.nearest().to_bits());
    let inside = true;
    let (least, most) = (u64::MIN, u64::MAX);
    match op {
        NumberOp::Equal => Range {
            low: at,
            high: at,
            inside,
        },
        NumberOp::NotEqual => Range {
            low: at,
            high: at,
            inside: false,
        },
        // No key of a number is the first or the last.
        NumberOp::Greater => Range {
            low: at.saturating_add(1),
            high: most,
            inside,
        },
        NumberOp::AtLeast => Range {
            low: at,
            high: most,
            inside,
        },
        NumberOp::Less => Range {
            low: least,
            high: at.saturating_sub(1),
            inside,
        },
        NumberOp::AtMost => Range {
            low: least,
            high: at,
            inside,
        },
        NumberOp::Between => {
            let to = b.map_or(at, |b| float_key(b.nearest().to_bits()));
            Range {
                low: at.min(to),
                high: at.max(to),
                inside,
            }
        }
    }
}

const SIGN: u64 = 1 << 63;

/// The bits of a floating-point number as an integer that compares like it.
pub(super) const fn float_key(bits: u64) -> u64 {
    bits ^ ((bits.cast_signed() >> 63).cast_unsigned() | SIGN)
}

/// The bits an order key was made from.
pub(super) const fn float_bits(key: u64) -> u64 {
    if key & SIGN == 0 { !key } else { key ^ SIGN }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::unwrap_used,
    reason = "tests"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;
    use std::format;

    use super::*;
    use reticula_core::rng::Rng;

    fn around(text: &str, scale: u8) -> (i128, i128) {
        let between = Threshold::parse(text).unwrap().scaled(scale);
        (between.floor, between.ceiling)
    }

    #[test]
    fn what_is_not_a_number_is_refused() {
        for text in [
            "", "-", "+", ".", "-.", "1e", "1e+", "e5", "1.2.3", "1,000", " 1", "1 ", "0x10",
            "inf", "NaN", "--1", "1e1.5", "１", "1_000", "1e 5",
        ] {
            assert!(Threshold::parse(text).is_none(), "{text:?}");
        }
        for text in ["0", "-0", "+5", ".5", "5.", "1e5", "1E-5", "-1.5e+3"] {
            assert!(Threshold::parse(text).is_some(), "{text:?}");
        }
    }

    #[test]
    fn nothing_is_rounded() {
        assert_eq!(around("0.1", 0), (0, 1));
        assert_eq!(around("-0.1", 0), (-1, 0));
        assert_eq!(around("0.1", 1), (1, 1));
        assert_eq!(around("0.1", 2), (10, 10));
        assert_eq!(around("0.10000000000000000000000000000000001", 2), (10, 11));
        assert_eq!(around("-12.345", 2), (-1_235, -1_234));
        assert_eq!(around("1e3", 0), (1_000, 1_000));
        assert_eq!(around("1.5e-1", 2), (15, 15));
        assert_eq!(around("123456e-3", 0), (123, 124));
        assert_eq!(
            around("9223372036854775807", 0),
            (i64::MAX.into(), i64::MAX.into())
        );
        assert_eq!(
            around("-9223372036854775808.5", 0),
            (-9_223_372_036_854_775_809, -9_223_372_036_854_775_808)
        );
        assert_eq!(around("0e999999999999999999999", 18), (0, 0));
        assert_eq!(around("1e-999999999999999999999", 18), (0, 1));
        assert_eq!(around("-000.000", 3), (0, 0));
        let (floor, ceiling) = around("1e999999999999999999999", 0);
        assert!(floor > i64::MAX.into() && ceiling > i64::MAX.into());
        let (floor, ceiling) = around(
            "-123456789012345678901234567890123456789012345678901234567890",
            0,
        );
        assert!(floor < i64::MIN.into() && ceiling < i64::MIN.into());
    }

    #[test]
    fn numbers_with_few_digits_agree_with_integers() {
        let mut rng = Rng::from_seed(4);
        for _ in 0..20_000 {
            let places = rng.next_below(6) as u32;
            let number = rng.next_below(2_000_000) as i128 - 1_000_000;
            let factor = 10_i128.pow(places);
            let (whole, rest) = (number.abs() / factor, number.abs() % factor);
            let sign = if number < 0 { "-" } else { "" };
            let text = if places == 0 {
                format!("{sign}{whole}")
            } else {
                format!("{sign}{whole}.{rest:0width$}", width = places as usize)
            };
            let scale = rng.next_below(8) as u32;
            let scaled = number * 10_i128.pow(scale);
            let expected = (scaled.div_euclid(factor), -(-scaled).div_euclid(factor));
            assert_eq!(around(&text, scale as u8), expected, "{text} {scale}");
            let shifted: String = format!("{text}e{scale}");
            assert_eq!(around(&shifted, 0), expected);
        }
    }

    #[test]
    fn order_keys_compare_like_the_numbers() {
        let numbers = [
            f64::MIN,
            -1e10,
            -1.5,
            -1.0,
            -f64::MIN_POSITIVE,
            -5e-324,
            0.0,
            5e-324,
            1.0,
            1.5,
            f64::MAX,
        ];
        for a in numbers {
            for b in numbers {
                let (x, y) = (float_key(a.to_bits()), float_key(b.to_bits()));
                assert_eq!(a.partial_cmp(&b), Some(x.cmp(&y)));
            }
            assert_eq!(float_bits(float_key(a.to_bits())), a.to_bits());
        }
        assert!(float_key(f64::INFINITY.to_bits()) < u64::MAX);
        assert!(float_key(f64::NEG_INFINITY.to_bits()) > 0);
    }

    #[test]
    fn a_threshold_equals_the_float_next_to_it() {
        let threshold = Threshold::parse("0.1").unwrap();
        let range = floats(NumberOp::Equal, &threshold, None);
        assert_eq!(range.low, float_key(0.1_f64.to_bits()));
        assert_eq!(range.low, range.high);
        // More digits than a float has, and still the same float.
        let digits = Threshold::parse("0.1000000000000000055511151231257827").unwrap();
        assert_eq!(floats(NumberOp::Equal, &digits, None), range);
        let next = Threshold::parse("0.10000000000000002").unwrap();
        assert_ne!(floats(NumberOp::Equal, &next, None), range);
        let huge = Threshold::parse("1e400").unwrap();
        assert!(float_key(f64::MAX.to_bits()) <= floats(NumberOp::Less, &huge, None).high);
        assert!(float_key(f64::MAX.to_bits()) < floats(NumberOp::AtLeast, &huge, None).low);
    }
}
