//! Cross-target determinism self-check.
//!
//! [`digest`] runs a fixed workload through the parts of the kernel whose
//! results could plausibly differ between targets and folds everything into
//! one number. The expected value is recorded in the repository; tests compare
//! against it on every supported native target, under WebAssembly in Node, and
//! in each browser engine.
//!
//! The workload grows with the kernel. Changing it changes the expected value,
//! which is updated in the same commit.

use alloc::string::String;

use crate::digest::Digest64;
use crate::format::{NumberFormat, write_number};
use crate::num;
use crate::rng::Rng;
use crate::sample;
use crate::viewport::{self, Page};

const SEED: u64 = 0x5EED_0000_0000_0001;

/// Runs the self-check workload and returns its digest.
#[must_use]
pub fn digest() -> u64 {
    let mut digest = Digest64::new();
    generator_stream(&mut digest);
    math_functions(&mut digest);
    accumulation(&mut digest);
    conversions(&mut digest);
    formatting(&mut digest);
    sample_sheet(&mut digest);
    digest.finish()
}

/// Formats a digest the way it is recorded: 16 lowercase hexadecimal digits.
#[must_use]
pub fn to_hex(digest: u64) -> [u8; 16] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = [b'0'; 16];
    let mut rest = digest;
    for slot in text.iter_mut().rev() {
        let nibble = usize::try_from(rest & 0xF).unwrap_or(0);
        *slot = DIGITS.get(nibble).copied().unwrap_or(b'0');
        rest >>= 4;
    }
    text
}

fn generator_stream(digest: &mut Digest64) {
    let mut rng = Rng::from_seed(SEED);
    for _ in 0..4096 {
        digest.write_u64(rng.next_u64());
    }
    for _ in 0..4096 {
        digest.write_f64(rng.next_f64());
    }
    for bound in [1, 2, 6, 10, 1000, 1 << 33, u64::MAX] {
        for _ in 0..64 {
            digest.write_u64(rng.next_below(bound));
        }
    }
}

fn math_functions(digest: &mut Digest64) {
    let mut rng = Rng::from_seed(SEED ^ 0x0001_0000);
    for _ in 0..4096 {
        let unit = rng.next_f64();
        let signed = unit * 2.0 - 1.0;
        let positive = unit * 1.0e6 + f64::MIN_POSITIVE;
        let wide = signed * 700.0;
        let angle = signed * 1.0e6;
        let other = rng.next_f64() * 2.0 - 1.0;

        for value in [
            num::floor(wide),
            num::ceil(wide),
            num::trunc(wide),
            num::round(wide),
            num::round_ties_even(wide),
            num::sqrt(positive),
            num::cbrt(wide),
            num::exp(wide),
            num::exp2(wide),
            num::exp_m1(signed),
            num::ln(positive),
            num::ln_1p(unit),
            num::log2(positive),
            num::log10(positive),
            num::sin(signed),
            num::cos(signed),
            num::tan(signed),
            num::sin(angle),
            num::cos(angle),
            num::tan(angle),
            num::asin(signed),
            num::acos(signed),
            num::atan(wide),
            num::sinh(signed * 20.0),
            num::cosh(signed * 20.0),
            num::tanh(signed * 20.0),
            num::asinh(wide),
            num::acosh(positive + 1.0),
            num::atanh(signed),
            num::pow(positive, other),
            num::pow(unit, wide),
            num::atan2(signed, other),
            num::hypot(wide, angle),
            num::fmod(angle, unit + 0.1),
            // Out-of-domain arguments: the results are NaN or infinite on every target.
            num::sqrt(-positive),
            num::ln(-positive),
            num::asin(wide),
        ] {
            digest.write_f64(value);
        }
    }
}

/// Catches a compiler or target that reorders a sum or contracts `a * b + c`
/// into a fused multiply-add.
fn accumulation(digest: &mut Digest64) {
    let mut rng = Rng::from_seed(SEED ^ 0x0002_0000);
    let mut sum = 0.0;
    for index in 0..65_536_u32 {
        let magnitude = num::exp2(num::floor(rng.next_f64() * 80.0) - 40.0);
        sum += (rng.next_f64() - 0.5) * magnitude;
        if index % 1024 == 0 {
            digest.write_f64(sum);
        }
    }
    digest.write_f64(sum);

    for _ in 0..4096 {
        let a = rng.next_f64() * 3.0 + 1.0;
        let b = rng.next_f64() * 3.0 + 1.0;
        let c = -(a * b);
        digest.write_f64(a * b + c);
        digest.write_f64(a / b);
        digest.write_f64(a - b * num::floor(a / b));
    }
}

fn conversions(digest: &mut Digest64) {
    let mut rng = Rng::from_seed(SEED ^ 0x0003_0000);
    for _ in 0..4096 {
        let bits = rng.next_u64();
        let scaled = (rng.next_f64() - 0.5) * 1.0e19;
        digest.write_f64(bits as f64);
        digest.write_f64(bits.cast_signed() as f64);
        digest.write_u64(saturating_i64(scaled).cast_unsigned());
        digest.write_u64(u64::from(saturating_u32(scaled)));
        digest.write_f64(f64::from_bits(bits));
    }
}

/// Numbers as text, over magnitudes from a millionth to beyond what is printed
/// digit by digit.
fn formatting(digest: &mut Digest64) {
    let mut rng = Rng::from_seed(SEED ^ 0x0004_0000);
    let mut text = String::new();
    for _ in 0..4096 {
        let unit = rng.next_f64();
        let magnitude = num::pow(10.0, num::floor(rng.next_f64() * 24.0) - 6.0);
        let value = (unit - 0.5) * magnitude;
        for (number, format) in [
            (value, NumberFormat::General),
            (num::round(value), NumberFormat::General),
            (value, NumberFormat::Fixed2),
            (unit, NumberFormat::Percent),
            (unit * 80_000.0, NumberFormat::Date),
        ] {
            text.clear();
            write_number(number, format, &mut text);
            digest.write_bytes(text.as_bytes());
        }
    }
}

/// The seeded sample: every stored value, every string, and two viewport pages.
fn sample_sheet(digest: &mut Digest64) {
    const ROWS: u32 = 3000;
    let sheet = sample::generate(SEED ^ 0x0005_0000, ROWS);
    for col in 0..sheet.cols() {
        for row in 0..sheet.rows() {
            digest.write_u64(sheet.value(row, col).pack());
        }
    }
    let strings = sheet.strings();
    for id in 0..u32::try_from(strings.len()).unwrap_or(u32::MAX) {
        digest.write_bytes(strings.get(id).as_bytes());
    }
    let mut text = String::new();
    // The first page, and the last one, which is cut short on both sides.
    let last = Page {
        row: ROWS
            .saturating_sub(1)
            .checked_div(viewport::PAGE_ROWS)
            .unwrap_or(0),
        col: sheet
            .cols()
            .saturating_sub(1)
            .checked_div(viewport::PAGE_COLS)
            .unwrap_or(0),
    };
    for page in [Page { row: 0, col: 0 }, last] {
        digest.write_bytes(&viewport::encode_page(&sheet, page, 1, &mut text));
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "float-to-int `as` saturates; that is what is checked"
)]
fn saturating_i64(value: f64) -> i64 {
    value as i64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float-to-int `as` saturates; that is what is checked"
)]
fn saturating_u32(value: f64) -> u32 {
    value as u32
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{digest, to_hex};

    #[test]
    fn hex_is_fixed_width_lowercase() {
        assert_eq!(&to_hex(0), b"0000000000000000");
        assert_eq!(&to_hex(0x0123_4567_89AB_CDEF), b"0123456789abcdef");
        assert_eq!(&to_hex(u64::MAX), b"ffffffffffffffff");
    }

    #[test]
    fn digest_is_repeatable() {
        assert_eq!(digest(), digest());
    }
}
