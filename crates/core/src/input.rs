//! User input type inference (semantic checklist E30).
//!
//! Raw text submitted from a cell editor or formula bar is classified at the
//! authoring boundary using fixed `en-US` number punctuation and strict ISO
//! `YYYY-MM-DD` dates (`1900-01-01` through `9999-12-31`, preserving historical
//! `1900-02-29` = serial `60`).

use alloc::string::String;
use core::fmt::Write as _;

use crate::format::{NumberFormat, ymd_to_serial};
use crate::ops::{
    FormulaMode, MAX_CELL_TEXT_UTF16, MAX_FORMULA_CHARS, OpError, RawFormula, ScalarValue,
};
use crate::value::CalcError;

/// Result of classifying a raw user input string under E30.
#[derive(Clone, Debug, PartialEq)]
pub enum InferredInput {
    /// Empty string clears the cell's value/formula.
    Empty,
    /// Scalar constant with an optional implicit number format to apply when
    /// the target cell has no explicit per-cell format yet.
    Scalar {
        /// Parsed scalar value.
        value: ScalarValue,
        /// Suggested format (`Date` or `Percent`), if inferred from syntax.
        suggested_format: Option<NumberFormat>,
    },
    /// Formula starting with `'='` (authored formulas always use
    /// [`FormulaMode::Dynamic`]).
    Formula(RawFormula),
}

/// Infers the typed cell input for `raw` according to E30, returning
/// [`OpError`] if text or formula length limits are exceeded.
pub fn infer_input(raw: &str) -> Result<InferredInput, OpError> {
    // 1. Empty string -> Empty.
    if raw.is_empty() {
        return Ok(InferredInput::Empty);
    }

    // 2. Leading '=' -> Formula (Dynamic mode).
    if raw.starts_with('=') {
        if raw.chars().count() > MAX_FORMULA_CHARS {
            return Err(OpError::FormulaTooLong);
        }
        return Ok(InferredInput::Formula(RawFormula {
            source: String::from(raw),
            mode: FormulaMode::Dynamic,
        }));
    }

    // 3. Leading single quote '\'' -> forced Text of everything after '\''.
    if let Some(forced_text) = raw.strip_prefix('\'') {
        check_utf16_limit(forced_text)?;
        return Ok(InferredInput::Scalar {
            value: ScalarValue::Text(String::from(forced_text)),
            suggested_format: None,
        });
    }

    // 4. Exact (no surrounding whitespace) case-insensitive TRUE / FALSE.
    if !has_surrounding_whitespace(raw) {
        if raw.eq_ignore_ascii_case("TRUE") {
            return Ok(InferredInput::Scalar {
                value: ScalarValue::Bool(true),
                suggested_format: None,
            });
        }
        if raw.eq_ignore_ascii_case("FALSE") {
            return Ok(InferredInput::Scalar {
                value: ScalarValue::Bool(false),
                suggested_format: None,
            });
        }

        // 5. Exact case-insensitive standard error literal.
        if let Some(err) = CalcError::parse_literal(raw) {
            return Ok(InferredInput::Scalar {
                value: ScalarValue::Error(err),
                suggested_format: None,
            });
        }
    }

    let trimmed = raw.trim_matches(' ');

    // 6. Strict ISO date `YYYY-MM-DD` after trimming ASCII spaces.
    if let Some(serial) = parse_iso_date(trimmed) {
        return Ok(InferredInput::Scalar {
            value: ScalarValue::Number(serial),
            suggested_format: Some(NumberFormat::Date),
        });
    }

    // 7. Percentage `<decimal>%` with exact decimal-exponent shift (-2) so
    // there is no intermediate double rounding.
    if let Some(prefix) = trimmed.strip_suffix('%')
        && let Some(pct) = parse_decimal_shifted(prefix, -2)
    {
        return Ok(InferredInput::Scalar {
            value: ScalarValue::Number(pct),
            suggested_format: Some(NumberFormat::Percent),
        });
    }

    // 8. Finite `en-US` decimal or scientific number (overflow to infinity
    // falls through to Text per E30).
    if let Some(num) = parse_decimal_shifted(trimmed, 0) {
        return Ok(InferredInput::Scalar {
            value: ScalarValue::Number(num),
            suggested_format: None,
        });
    }

    // 9. Everything else stays literal Text (original untrimmed `raw`).
    check_utf16_limit(raw)?;
    Ok(InferredInput::Scalar {
        value: ScalarValue::Text(String::from(raw)),
        suggested_format: None,
    })
}

fn check_utf16_limit(text: &str) -> Result<(), OpError> {
    if text.encode_utf16().count() > MAX_CELL_TEXT_UTF16 {
        Err(OpError::TextTooLong)
    } else {
        Ok(())
    }
}

fn has_surrounding_whitespace(text: &str) -> bool {
    let first = text.chars().next();
    let last = text.chars().next_back();
    first.is_some_and(char::is_whitespace) || last.is_some_and(char::is_whitespace)
}

/// Parses strict `YYYY-MM-DD` (`4-2-2` ASCII digits) into a 1900-system serial.
fn parse_iso_date(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let year = parse_fixed_digits(bytes.get(0..4)?)?;
    let month = u8::try_from(parse_fixed_digits(bytes.get(5..7)?)?).ok()?;
    let day = u8::try_from(parse_fixed_digits(bytes.get(8..10)?)?).ok()?;
    ymd_to_serial(year, month, day)
}

fn parse_fixed_digits(bytes: &[u8]) -> Option<i32> {
    let mut acc: i32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        let digit = i32::from(b.saturating_sub(b'0'));
        acc = acc.checked_mul(10)?.checked_add(digit)?;
    }
    Some(acc)
}

/// Validates strict `en-US` decimal / scientific syntax (`[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?`)
/// and converts it to a finite `f64` with `extra_exp10` added to the decimal
/// exponent before a single IEEE-754 parse (avoiding `NaN`/`inf` keywords and
/// avoiding `x / 100.0` double rounding).
fn parse_decimal_shifted(text: &str, extra_exp10: i32) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut idx = 0usize;
    let mut negative = false;
    if let Some(&first) = bytes.first()
        && (first == b'+' || first == b'-')
    {
        negative = first == b'-';
        idx = 1;
    }

    let mut digits = String::new();
    let mut int_digits: usize = 0;
    while let Some(&b) = bytes.get(idx) {
        if b.is_ascii_digit() {
            if !(digits.is_empty() && b == b'0') {
                digits.push(char::from(b));
            }
            int_digits = int_digits.saturating_add(1);
            idx = idx.saturating_add(1);
        } else {
            break;
        }
    }

    let mut frac_digits: usize = 0;
    if bytes.get(idx) == Some(&b'.') {
        idx = idx.saturating_add(1);
        while let Some(&b) = bytes.get(idx) {
            if b.is_ascii_digit() {
                if !(digits.is_empty() && b == b'0') {
                    digits.push(char::from(b));
                }
                frac_digits = frac_digits.saturating_add(1);
                idx = idx.saturating_add(1);
            } else {
                break;
            }
        }
    }

    if int_digits == 0 && frac_digits == 0 {
        return None;
    }

    let mut exp10: i64 = 0;
    if let Some(&e_ch) = bytes.get(idx)
        && (e_ch == b'e' || e_ch == b'E')
    {
        idx = idx.saturating_add(1);
        let mut exp_neg = false;
        if let Some(&sign) = bytes.get(idx)
            && (sign == b'+' || sign == b'-')
        {
            exp_neg = sign == b'-';
            idx = idx.saturating_add(1);
        }
        let exp_start = idx;
        while let Some(&b) = bytes.get(idx) {
            if b.is_ascii_digit() {
                let d = i64::from(b.saturating_sub(b'0'));
                exp10 = exp10.saturating_mul(10).saturating_add(d).min(1_000_000);
                idx = idx.saturating_add(1);
            } else {
                return None;
            }
        }
        if idx == exp_start {
            return None;
        }
        if exp_neg {
            exp10 = exp10.saturating_neg();
        }
    }

    if idx != bytes.len() {
        return None;
    }

    if digits.is_empty() {
        return Some(0.0);
    }

    let frac_i64 = i64::try_from(frac_digits).ok()?;
    let adjusted_exp = exp10
        .checked_sub(frac_i64)?
        .checked_add(i64::from(extra_exp10))?;

    let mut canonical = String::with_capacity(digits.len().saturating_add(16));
    if negative {
        canonical.push('-');
    }
    canonical.push_str(&digits);
    let _ = write!(canonical, "e{adjusted_exp}");

    let parsed = canonical.parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some(if parsed == 0.0 { 0.0 } else { parsed })
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use super::{InferredInput, infer_input};
    use crate::format::NumberFormat;
    use crate::ops::{FormulaMode, OpError, RawFormula, ScalarValue};
    use crate::value::CalcError;

    #[test]
    fn e30_rules_classify_inputs_in_documented_order() {
        assert_eq!(infer_input("").unwrap(), InferredInput::Empty);

        assert_eq!(
            infer_input("=SUM(A1:A10)").unwrap(),
            InferredInput::Formula(RawFormula {
                source: String::from("=SUM(A1:A10)"),
                mode: FormulaMode::Dynamic,
            })
        );

        // Forced text strips only the leading apostrophe.
        assert_eq!(
            infer_input("'=1+1").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Text(String::from("=1+1")),
                suggested_format: None,
            }
        );
        assert_eq!(
            infer_input("'00123").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Text(String::from("00123")),
                suggested_format: None,
            }
        );

        // Booleans require no surrounding whitespace.
        assert_eq!(
            infer_input("true").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Bool(true),
                suggested_format: None,
            }
        );
        assert_eq!(
            infer_input(" false").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Text(String::from(" false")),
                suggested_format: None,
            }
        );

        // Error literals.
        assert_eq!(
            infer_input("#div/0!").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Error(CalcError::Div0),
                suggested_format: None,
            }
        );
        assert_eq!(
            infer_input("#SPILL!").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Error(CalcError::Spill),
                suggested_format: None,
            }
        );

        // Strict ISO dates, including 1900-02-29 = 60.
        assert_eq!(
            infer_input(" 1900-02-29 ").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Number(60.0),
                suggested_format: Some(NumberFormat::Date),
            }
        );
        assert_eq!(
            infer_input("2026-11-18").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Number(46_344.0),
                suggested_format: Some(NumberFormat::Date),
            }
        );
        // Non-ISO or invalid dates stay text.
        assert_eq!(
            infer_input("2026-2-5").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Text(String::from("2026-2-5")),
                suggested_format: None,
            }
        );
        assert_eq!(
            infer_input("2023-02-29").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Text(String::from("2023-02-29")),
                suggested_format: None,
            }
        );

        // Percentages shift the decimal exponent without double rounding.
        assert_eq!(
            infer_input("5%").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Number(0.05),
                suggested_format: Some(NumberFormat::Percent),
            }
        );
        assert_eq!(
            infer_input(" -12.34% ").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Number(-0.1234),
                suggested_format: Some(NumberFormat::Percent),
            }
        );

        // Numbers vs keywords vs overflow.
        assert_eq!(
            infer_input(" -123.45 ").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Number(-123.45),
                suggested_format: None,
            }
        );
        assert_eq!(
            infer_input("-0.0").unwrap(),
            InferredInput::Scalar {
                value: ScalarValue::Number(0.0),
                suggested_format: None,
            }
        );
        for non_num in ["NaN", "inf", "Infinity", "-inf", "1e400", "1,234"] {
            assert_eq!(
                infer_input(non_num).unwrap(),
                InferredInput::Scalar {
                    value: ScalarValue::Text(String::from(non_num)),
                    suggested_format: None,
                }
            );
        }

        // Length limits are rejected cleanly.
        let huge_text = "a".repeat(32_768);
        assert_eq!(infer_input(&huge_text), Err(OpError::TextTooLong));
        let huge_formula = alloc::format!("={}", "1+".repeat(4_100));
        assert_eq!(infer_input(&huge_formula), Err(OpError::FormulaTooLong));
    }
}
