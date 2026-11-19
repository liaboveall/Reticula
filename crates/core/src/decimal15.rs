//! Excel-compatible 15-significant-digit decimal rounding and cancellation.
//!
//! Excel evaluates arithmetic in IEEE 754 binary64, yet rounds numbers to 15
//! significant decimal digits before relational comparisons, conditional
//! criteria and decimal rounding functions (`ROUND`, `ROUNDDOWN`, `ROUNDUP`,
//! `TRUNC`, `INT`, `FLOOR`, `CEILING`), and zeroes tiny cancellation residuals
//! on the outermost addition/subtraction and inside `SUM` / `AVERAGE`.
//!
//! All algorithms here are pure `no_std` integer and Dragon4/Eisel-Lemire
//! routines verified against 22,968 recorded Excel 16.0 (build 20711) cases.

use alloc::string::String;
use core::fmt::Write as _;

use crate::num;
use crate::value::CalcError;

///Smallest positive normal IEEE 754 `f64` (`2^-1022`).
const MIN_NORMAL: f64 = 2.225_073_858_507_201_4e-308;

/// Rounding direction for decimal place rounding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingMode {
    /// Halves away from zero (`ROUND`).
    HalfAwayFromZero,
    /// Toward zero (`ROUNDDOWN`, `TRUNC`).
    TowardZero,
    /// Away from zero (`ROUNDUP`, magnitude side of `FLOOR` / `CEILING`).
    AwayFromZero,
}

/// The 15 significant decimal digits of a finite non-zero `f64`:
/// `|x| ≈ digits * 10^(-scale)` with `10^14 <= digits < 10^15`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digits15 {
    /// Whether `x < 0`.
    pub negative: bool,
    /// 15-digit significand in `100_000_000_000_000 ..= 999_999_999_999_999`
    /// (or `1_000_000_000_000_000` when rounding `999_999_999_999_999.5` up,
    /// immediately normalised to `100_000_000_000_000` with `scale - 1`).
    pub digits: u64,
    /// Decimal fractional scale (`scale = 14 - exponent`).
    pub scale: i32,
}

/// Extracts the 15 significant decimal digits of a finite non-zero `x`,
/// rounding exact halfway cases away from zero.
#[must_use]
pub fn digits15(x: f64) -> Option<Digits15> {
    if !x.is_finite() || x == 0.0 {
        return None;
    }
    let negative = x.is_sign_negative();
    let abs_x = x.abs();

    // Format in scientific notation with 14 digits after the decimal point
    // (15 significant digits total). `core::fmt` rounds exact halfway ties to
    // even; we then promote an even tie to away-from-zero using exact integer
    // factorisation of `abs_x`.
    let mut buf = String::with_capacity(28);
    let _ = write!(buf, "{abs_x:.14e}");
    let (mant_str, exp_str) = buf.split_once('e')?;
    let exponent: i32 = exp_str.parse().ok()?;

    let mut digits: u64 = 0;
    for byte in mant_str.bytes() {
        if byte == b'.' {
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        digits = digits
            .saturating_mul(10)
            .saturating_add(u64::from(byte.saturating_sub(b'0')));
    }

    // If `digits` is even and `abs_x` equals the exact halfway value
    // `(2 * digits + 1) * 5 * 10^(exponent - 15)`, round away from zero.
    if digits & 1 == 0 && is_exact_halfway(abs_x, digits, exponent) {
        digits = digits.saturating_add(1);
    }

    let mut scale = 14_i32.saturating_sub(exponent);
    if digits >= 1_000_000_000_000_000 {
        digits /= 10;
        scale = scale.saturating_sub(1);
    }

    Some(Digits15 {
        negative,
        digits,
        scale,
    })
}

/// Rounds `x` to 15 significant decimal digits and returns the closest `f64`
/// (matching Excel's pre-comparison normalisation).
#[must_use]
pub fn round_to_15_sig_figs(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return if x == 0.0 { 0.0 } else { x };
    }
    let Some(d) = digits15(x) else {
        return x;
    };
    decimal_to_f64(d.negative, u128::from(d.digits), d.scale)
}

/// Compares two finite numbers under Excel's 15-significant-digit rule.
#[must_use]
pub fn cmp_excel_f64(a: f64, b: f64) -> Option<core::cmp::Ordering> {
    let ra = round_to_15_sig_figs(a);
    let rb = round_to_15_sig_figs(b);
    ra.partial_cmp(&rb)
}

/// Rounds `x` to `places` decimal places (`places` may be negative, e.g. `-2`
/// rounds to hundreds) after taking `x`'s 15-significant-digit decimal value.
#[must_use]
pub fn round_decimal(x: f64, places: i32, mode: RoundingMode) -> Option<f64> {
    if !x.is_finite() {
        return None;
    }
    if x == 0.0 {
        return Some(0.0);
    }
    let d = digits15(x)?;
    // Clamp extreme `places` values: for `|x| <= 1.8e308`, `places >= 325`
    // never drops any of the 15 digits, and `places <= -325` always drops all
    // digits with a tiny magnitude `< 10^-16` of a unit.
    if places >= d.scale {
        return Some(decimal_to_f64(d.negative, u128::from(d.digits), d.scale));
    }

    let drop_digits = i64::from(d.scale).saturating_sub(i64::from(places));
    if drop_digits > 36 {
        // `digits < 10^15`, so `digits / 10^drop_digits == 0` and
        // `2 * digits < 10^36 < 10^drop_digits`: HalfAwayFromZero and
        // TowardZero yield 0, while AwayFromZero yields `1 * 10^(-places)`.
        return match mode {
            RoundingMode::HalfAwayFromZero | RoundingMode::TowardZero => Some(0.0),
            RoundingMode::AwayFromZero => {
                let val = decimal_to_f64(d.negative, 1, places);
                if val.is_finite() { Some(val) } else { None }
            }
        };
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`1 <= drop_digits <= 36`, verified above"
    )]
    let exp = drop_digits as u32;
    let divisor = 10_u128.checked_pow(exp)?;
    let digits = u128::from(d.digits);
    let mut quotient = digits.checked_div(divisor)?;
    let remainder = digits.checked_rem(divisor)?;

    let round_up = match mode {
        RoundingMode::HalfAwayFromZero => remainder.saturating_mul(2) >= divisor,
        RoundingMode::TowardZero => false,
        RoundingMode::AwayFromZero => remainder > 0,
    };
    if round_up {
        quotient = quotient.saturating_add(1);
    }
    if quotient == 0 {
        return Some(0.0);
    }
    let result = decimal_to_f64(d.negative, quotient, places);
    if result.is_finite() {
        Some(result)
    } else {
        None
    }
}

/// Excel's `INT(x)`: below `10^15`, floor of the 15-digit value; from `10^15`
/// onwards, exact IEEE `floor(x)`.
#[must_use]
pub fn excel_int(x: f64) -> Option<f64> {
    if !x.is_finite() {
        return None;
    }
    if x.abs() >= 1.0e15 {
        return Some(canonical_zero(num::floor(x)));
    }
    let mode = if x < 0.0 {
        RoundingMode::AwayFromZero
    } else {
        RoundingMode::TowardZero
    };
    round_decimal(x, 0, mode)
}

/// Excel's `FLOOR(x, significance)` under D-027 15-significant-digit rules.
pub fn excel_floor(x: f64, significance: f64) -> Result<f64, CalcError> {
    if !x.is_finite() || !significance.is_finite() {
        return Err(CalcError::Num);
    }
    if x == 0.0 {
        return Ok(0.0);
    }
    if significance == 0.0 {
        return Err(CalcError::Div0);
    }
    if x > 0.0 && significance < 0.0 {
        return Err(CalcError::Num);
    }
    if significance == 1.0 && x.abs() < 1.0e15 {
        let mode = if x < 0.0 {
            RoundingMode::AwayFromZero
        } else {
            RoundingMode::TowardZero
        };
        return round_decimal(x, 0, mode).ok_or(CalcError::Num);
    }
    let rx = round_to_15_sig_figs(x);
    let rsig = round_to_15_sig_figs(significance);
    let ratio = round_to_15_sig_figs(rx / rsig);
    let res = num::floor(ratio) * significance;
    if res.is_finite() {
        Ok(canonical_zero(res))
    } else {
        Err(CalcError::Num)
    }
}

/// Excel's `CEILING(x, significance)` under D-027 15-significant-digit rules.
pub fn excel_ceiling(x: f64, significance: f64) -> Result<f64, CalcError> {
    if !x.is_finite() || !significance.is_finite() {
        return Err(CalcError::Num);
    }
    if x == 0.0 || significance == 0.0 {
        return Ok(0.0);
    }
    if x > 0.0 && significance < 0.0 {
        return Err(CalcError::Num);
    }
    if significance == 1.0 && x.abs() < 1.0e15 {
        let mode = if x > 0.0 {
            RoundingMode::AwayFromZero
        } else {
            RoundingMode::TowardZero
        };
        return round_decimal(x, 0, mode).ok_or(CalcError::Num);
    }
    let rx = round_to_15_sig_figs(x);
    let rsig = round_to_15_sig_figs(significance);
    let ratio = round_to_15_sig_figs(rx / rsig);
    let res = num::ceil(ratio) * significance;
    if res.is_finite() {
        Ok(canonical_zero(res))
    } else {
        Err(CalcError::Num)
    }
}

/// Excel's `MOD(a, b)`: exact IEEE remainder with the sign of `b` (`None` when
/// `b == 0` or inputs are non-finite).
#[must_use]
pub fn excel_mod(a: f64, b: f64) -> Option<f64> {
    if !a.is_finite() || !b.is_finite() || b == 0.0 {
        return None;
    }
    let mut rem = num::fmod(a, b);
    if !rem.is_finite() {
        return None;
    }
    if rem != 0.0 && (rem < 0.0) != (b < 0.0) {
        rem += b;
    }
    Some(canonical_zero(rem))
}

/// Excel's outermost addition/subtraction and `SUM`/`AVERAGE` tail-zeroing:
/// when `left != 0` and `|result| < 2^(exponent_of_left - 49)` and `|result|`
/// is not subnormal, returns `0.0`.
#[must_use]
pub fn snap_cancelled_zero(left: f64, result: f64) -> f64 {
    if !result.is_finite() || result == 0.0 || left == 0.0 || !left.is_finite() {
        return canonical_zero(result);
    }
    let abs_res = result.abs();
    if abs_res < MIN_NORMAL {
        return result;
    }
    if let Some(exp_left) = unbiased_bin_exp(left.abs()) {
        let threshold_exp = exp_left.saturating_sub(49);
        let threshold = num::exp2(f64::from(threshold_exp));
        if abs_res < threshold {
            return 0.0;
        }
    }
    canonical_zero(result)
}

const fn canonical_zero(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

/// Unbiased base-2 exponent `floor(log2(abs_x))` for positive finite `abs_x`.
fn unbiased_bin_exp(abs_x: f64) -> Option<i32> {
    if !abs_x.is_finite() || abs_x <= 0.0 {
        return None;
    }
    let bits = abs_x.to_bits();
    let raw_exp = ((bits >> 52) & 0x7FF) as i32;
    if raw_exp > 0 {
        Some(raw_exp.saturating_sub(1023))
    } else {
        // Subnormal: `bits` holds the 52-bit mantissa.
        let mant = bits & ((1_u64 << 52) - 1);
        if mant == 0 {
            None
        } else {
            let leading = mant.leading_zeros().cast_signed();
            // For `mant = 1 << 51`, leading_zeros is 12 and exponent is -1023.
            Some((-1022_i32).saturating_sub(leading.saturating_sub(11)))
        }
    }
}

/// Converts `(-1)^negative * significand * 10^(-scale)` into its correctly
/// rounded IEEE 754 `f64`.
fn decimal_to_f64(negative: bool, significand: u128, scale: i32) -> f64 {
    if significand == 0 {
        return 0.0;
    }
    let mut buf = String::with_capacity(48);
    let neg_scale = scale.saturating_neg();
    let _ = write!(buf, "{significand}e{neg_scale}");
    let mag = buf.parse::<f64>().unwrap_or(f64::INFINITY);
    if mag == 0.0 {
        0.0
    } else if negative {
        -mag
    } else {
        mag
    }
}

/// Exact number-theoretic test for `abs_x == (2 * digits + 1) * 5 * 10^(exponent - 15)`.
fn is_exact_halfway(abs_x: f64, digits: u64, exponent: i32) -> bool {
    let Some((odd_mant, bin_exp)) = decompose_odd(abs_x) else {
        return false;
    };
    let k = exponent.saturating_sub(15);
    if bin_exp != k {
        return false;
    }
    let two_d_plus_1 = digits.saturating_mul(2).saturating_add(1);
    let power_of_five = k.saturating_add(1);
    if power_of_five >= 0 {
        if power_of_five > 2 {
            return false;
        }
        #[expect(
            clippy::cast_sign_loss,
            reason = "`0 <= power_of_five <= 2`, checked above"
        )]
        let five_pow = 5_u64.pow(power_of_five as u32);
        two_d_plus_1.checked_mul(five_pow) == Some(odd_mant)
    } else {
        let q = power_of_five.saturating_neg();
        if q > 22 {
            return false;
        }
        #[expect(clippy::cast_sign_loss, reason = "`1 <= q <= 22`, checked above")]
        let five_pow = 5_u64.pow(q as u32);
        two_d_plus_1.checked_rem(five_pow) == Some(0)
            && two_d_plus_1.checked_div(five_pow) == Some(odd_mant)
    }
}

/// Decomposes positive finite `abs_x` into `(odd_mant, bin_exp)` such that
/// `abs_x == odd_mant * 2^bin_exp` with `odd_mant` odd.
fn decompose_odd(abs_x: f64) -> Option<(u64, i32)> {
    if !abs_x.is_finite() || abs_x <= 0.0 {
        return None;
    }
    let bits = abs_x.to_bits();
    let raw_exp = ((bits >> 52) & 0x7FF) as i32;
    let frac = bits & ((1_u64 << 52) - 1);
    let (mant53, exp) = if raw_exp == 0 {
        (frac, -1074_i32)
    } else {
        (frac | (1_u64 << 52), raw_exp.saturating_sub(1075))
    };
    if mant53 == 0 {
        return None;
    }
    let tz = mant53.trailing_zeros();
    #[expect(clippy::cast_possible_wrap, reason = "`tz <= 52`")]
    let odd_exp = exp.saturating_add(tz as i32);
    Some((mant53 >> tz, odd_exp))
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[cfg(not(target_arch = "wasm32"))]
    use alloc::format;

    use super::{
        RoundingMode, cmp_excel_f64, excel_int, excel_mod, round_decimal, snap_cancelled_zero,
    };

    #[test]
    fn classic_excel_15_digit_cases_hold() {
        // =0.1+0.2=0.3 is TRUE in Excel.
        assert_eq!(
            cmp_excel_f64(0.1 + 0.2, 0.3),
            Some(core::cmp::Ordering::Equal)
        );
        // =INT((0.1+0.7)*10) is 8 in Excel (IEEE is 7).
        assert_eq!(excel_int((0.1 + 0.7) * 10.0), Some(8.0));
        // =ROUND(1.005, 2) is 1.01 in Excel (IEEE binary is 1.00499999999999989 -> 15 digits is 1.005 -> 1.01).
        assert_eq!(
            round_decimal(1.005, 2, RoundingMode::HalfAwayFromZero),
            Some(1.01)
        );
        // Outermost subtraction cancellation: =0.1+0.2-0.3 is 0.
        assert_eq!(snap_cancelled_zero(0.1 + 0.2, (0.1 + 0.2) - 0.3), 0.0);
        // =MOD(1, 0.1) is the exact IEEE remainder ~0.09999999999999995, not 0.
        let Some(rem) = excel_mod(1.0, 0.1) else {
            panic!("mod");
        };
        assert!((rem - 0.099_999_999_999_999_95).abs() == 0.0);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn matches_all_22968_recorded_excel_cases() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../spikes/excel/results/numeric-excel-16.0.20711.tsv");
        let text = std::fs::read_to_string(&path).expect("Excel numeric TSV exists");

        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: std::vec::Vec<&str> = line.split('\t').collect();
            let kind = fields[1];
            if kind == "lit" || fields[4] != "=" {
                continue;
            }
            let a = f64::from_bits(u64::from_str_radix(fields[2], 16).expect("hex a"));
            let b = f64::from_bits(u64::from_str_radix(fields[3], 16).expect("hex b"));
            let res = &fields[5..];

            match kind {
                "cmp" => {
                    let ord = cmp_excel_f64(a, b).expect("finite");
                    assert_eq!(res[0] == "TRUE", ord.is_eq(), "case {}", fields[0]);
                    assert_eq!(res[1] == "TRUE", ord.is_ne(), "case {}", fields[0]);
                    assert_eq!(res[2] == "TRUE", ord.is_lt(), "case {}", fields[0]);
                    assert_eq!(res[3] == "TRUE", ord.is_gt(), "case {}", fields[0]);
                    assert_eq!(res[4] == "TRUE", ord.is_le(), "case {}", fields[0]);
                    assert_eq!(res[5] == "TRUE", ord.is_ge(), "case {}", fields[0]);
                    let snapped = snap_cancelled_zero(a, a - b);
                    assert_eq!(
                        format!("{:016X}", snapped.to_bits()),
                        res[6],
                        "snap case {}",
                        fields[0]
                    );
                }
                "rnd" => {
                    let places = b as i32;
                    let r_half =
                        round_decimal(a, places, RoundingMode::HalfAwayFromZero).expect("round");
                    let r_down =
                        round_decimal(a, places, RoundingMode::TowardZero).expect("rounddown");
                    let r_trunc =
                        round_decimal(a, places, RoundingMode::TowardZero).expect("trunc");
                    assert_eq!(
                        format!("{:016X}", r_half.to_bits()),
                        res[0],
                        "ROUND case {}",
                        fields[0]
                    );
                    assert_eq!(
                        format!("{:016X}", r_down.to_bits()),
                        res[2],
                        "ROUNDDOWN case {}",
                        fields[0]
                    );
                    assert_eq!(
                        format!("{:016X}", r_trunc.to_bits()),
                        res[3],
                        "TRUNC case {}",
                        fields[0]
                    );
                    let r_up =
                        round_decimal(a, places, RoundingMode::AwayFromZero).expect("roundup");
                    let excel_up = f64::from_bits(u64::from_str_radix(res[1], 16).expect("hex up"));
                    assert_eq!(
                        cmp_excel_f64(r_up, excel_up),
                        Some(core::cmp::Ordering::Equal),
                        "ROUNDUP 15-digit case {}",
                        fields[0]
                    );
                }
                "near" => {
                    let v_int = excel_int(a).expect("int");
                    let v_trunc = round_decimal(a, 0, RoundingMode::TowardZero).expect("trunc0");
                    let floor_mode = if a < 0.0 {
                        RoundingMode::AwayFromZero
                    } else {
                        RoundingMode::TowardZero
                    };
                    let ceil_mode = if a > 0.0 {
                        RoundingMode::AwayFromZero
                    } else {
                        RoundingMode::TowardZero
                    };
                    let v_floor = round_decimal(a, 0, floor_mode).expect("floor15");
                    let v_ceil = round_decimal(a, 0, ceil_mode).expect("ceil15");
                    assert_eq!(
                        format!("{:016X}", v_int.to_bits()),
                        res[0],
                        "INT case {}",
                        fields[0]
                    );
                    assert_eq!(
                        format!("{:016X}", v_trunc.to_bits()),
                        res[1],
                        "TRUNC(x) case {}",
                        fields[0]
                    );
                    assert_eq!(
                        format!("{:016X}", v_floor.to_bits()),
                        res[2],
                        "FLOOR(x,1) case {}",
                        fields[0]
                    );
                    assert_eq!(
                        format!("{:016X}", v_ceil.to_bits()),
                        res[3],
                        "CEILING(x,1) case {}",
                        fields[0]
                    );
                }
                "mod" if res[0].len() == 16 && res[0].bytes().all(|c| c.is_ascii_hexdigit()) => {
                    let m = excel_mod(a, b).expect("mod");
                    assert_eq!(
                        format!("{:016X}", m.to_bits()),
                        res[0],
                        "MOD case {}",
                        fields[0]
                    );
                }
                _ => {}
            }
        }
    }
}
