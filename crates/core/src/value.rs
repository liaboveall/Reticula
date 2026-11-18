//! A cell value in eight bytes.
//!
//! A finite number is stored as its own IEEE 754 bit pattern (with `-0.0`
//! canonicalised to `+0.0`). Everything else — empty cells, interned strings,
//! booleans, formula errors and dynamic array handles — lives in the payload
//! of a quiet NaN prefix that arithmetic never produces, so a column block is
//! a plain array of 64-bit words with no side tag table.

use serde::{Deserialize, Serialize};

/// Formula and evaluation errors supported by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum CalcError {
    /// Division by zero (`#DIV/0!`).
    Div0 = 1,
    /// Wrong operand or argument type (`#VALUE!`).
    Value = 2,
    /// Deleted or out-of-sheet reference (`#REF!`).
    Ref = 3,
    /// Unknown function or identifier (`#NAME?`).
    Name = 4,
    /// Invalid numeric domain or overflow (`#NUM!`).
    Num = 5,
    /// Value not available (`#N/A`).
    Na = 6,
    /// Empty range intersection (`#NULL!`).
    Null = 7,
    /// Multi-cell dynamic array cannot spill (`#SPILL!`).
    Spill = 8,
    /// Evaluation limit or empty-array calculation error (`#CALC!`).
    Calc = 9,
    /// Circular reference (`#CYCLE!`).
    Cycle = 10,
}

impl CalcError {
    /// Canonical formula literal of the error.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Div0 => "#DIV/0!",
            Self::Value => "#VALUE!",
            Self::Ref => "#REF!",
            Self::Name => "#NAME?",
            Self::Num => "#NUM!",
            Self::Na => "#N/A",
            Self::Null => "#NULL!",
            Self::Spill => "#SPILL!",
            Self::Calc => "#CALC!",
            Self::Cycle => "#CYCLE!",
        }
    }

    /// Parses an ASCII case-insensitive error literal.
    #[must_use]
    pub fn parse_literal(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("#DIV/0!") {
            Some(Self::Div0)
        } else if text.eq_ignore_ascii_case("#VALUE!") {
            Some(Self::Value)
        } else if text.eq_ignore_ascii_case("#REF!") {
            Some(Self::Ref)
        } else if text.eq_ignore_ascii_case("#NAME?") {
            Some(Self::Name)
        } else if text.eq_ignore_ascii_case("#NUM!") {
            Some(Self::Num)
        } else if text.eq_ignore_ascii_case("#N/A") {
            Some(Self::Na)
        } else if text.eq_ignore_ascii_case("#NULL!") {
            Some(Self::Null)
        } else if text.eq_ignore_ascii_case("#SPILL!") {
            Some(Self::Spill)
        } else if text.eq_ignore_ascii_case("#CALC!") {
            Some(Self::Calc)
        } else if text.eq_ignore_ascii_case("#CYCLE!") {
            Some(Self::Cycle)
        } else {
            None
        }
    }

    const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Div0),
            2 => Some(Self::Value),
            3 => Some(Self::Ref),
            4 => Some(Self::Name),
            5 => Some(Self::Num),
            6 => Some(Self::Na),
            7 => Some(Self::Null),
            8 => Some(Self::Spill),
            9 => Some(Self::Calc),
            10 => Some(Self::Cycle),
            _ => None,
        }
    }
}

/// What a cell holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// Nothing.
    #[default]
    Empty,
    /// A finite IEEE 754 number (`-0.0` is canonicalised to `+0.0`).
    Number(f64),
    /// A string, by its index in the workbook's [`StringTable`].
    ///
    /// [`StringTable`]: crate::strings::StringTable
    Text(u32),
    /// A logical value (`TRUE` or `FALSE`).
    Bool(bool),
    /// A formula or domain error.
    Error(CalcError),
    /// Handle into an array value table.
    Array(u32),
}

/// Top bits of every boxed word: a quiet NaN with bit 50 set, which hardware
/// floating-point operations never emit as a canonical quiet NaN.
const BOX: u64 = 0x7FFC_0000_0000_0000;
const BOX_MASK: u64 = 0xFFFC_0000_0000_0000;
const TAG_SHIFT: u32 = 32;
const TAG_MASK: u64 = 0xF;
const TAG_EMPTY: u64 = 1;
const TAG_TEXT: u64 = 2;
const TAG_BOOL: u64 = 3;
const TAG_ERROR: u64 = 4;
const TAG_ARRAY: u64 = 5;
const PAYLOAD_MASK: u64 = 0xFFFF_FFFF;

impl Value {
    /// The word an empty cell is stored as.
    pub const EMPTY_BITS: u64 = BOX | (TAG_EMPTY << TAG_SHIFT);

    /// Constructs a canonical numeric value: `-0.0` becomes `+0.0`, while NaN
    /// and infinities become `#NUM!`.
    #[must_use]
    pub const fn number(value: f64) -> Self {
        if !value.is_finite() {
            return Self::Error(CalcError::Num);
        }
        if value == 0.0 {
            Self::Number(0.0)
        } else {
            Self::Number(value)
        }
    }

    /// Packs the value into 64 bits.
    ///
    /// `-0.0` is canonicalised to `+0.0`. A raw NaN inside [`Self::Number`] is
    /// turned into `#NUM!`; an infinity inside [`Self::Number`] is also turned
    /// into `#NUM!` so stored numbers are always finite IEEE doubles.
    #[must_use]
    pub const fn pack(self) -> u64 {
        match self {
            Self::Empty => Self::EMPTY_BITS,
            Self::Number(number) => {
                if !number.is_finite() {
                    BOX | (TAG_ERROR << TAG_SHIFT) | (CalcError::Num as u64)
                } else if number == 0.0 {
                    0.0_f64.to_bits()
                } else {
                    number.to_bits()
                }
            }
            Self::Text(id) => BOX | (TAG_TEXT << TAG_SHIFT) | (id as u64),
            Self::Bool(flag) => BOX | (TAG_BOOL << TAG_SHIFT) | u64_from_bool(flag),
            Self::Error(err) => BOX | (TAG_ERROR << TAG_SHIFT) | (err as u64),
            Self::Array(id) => BOX | (TAG_ARRAY << TAG_SHIFT) | (id as u64),
        }
    }

    /// Reads a packed value back, returning `None` if `bits` carries an
    /// unrecognized NaN or boxed tag (used by snapshot decoding to reject
    /// corrupted bit patterns).
    #[must_use]
    pub const fn try_unpack(bits: u64) -> Option<Self> {
        if bits & BOX_MASK != BOX {
            let number = f64::from_bits(bits);
            if !number.is_finite() {
                return None;
            }
            return Some(if number == 0.0 {
                Self::Number(0.0)
            } else {
                Self::Number(number)
            });
        }
        if (bits >> 48) != (BOX >> 48) {
            return None;
        }
        let tag = (bits >> TAG_SHIFT) & TAG_MASK;
        // High tag bits 36..48 must be zero.
        if (bits >> 36) & 0xFFF != 0 {
            return None;
        }
        let payload = low_half(bits & PAYLOAD_MASK);
        match tag {
            TAG_EMPTY if payload == 0 => Some(Self::Empty),
            TAG_TEXT => Some(Self::Text(payload)),
            TAG_BOOL => match payload {
                0 => Some(Self::Bool(false)),
                1 => Some(Self::Bool(true)),
                _ => None,
            },
            TAG_ERROR => match CalcError::from_code(payload) {
                Some(err) => Some(Self::Error(err)),
                None => None,
            },
            TAG_ARRAY => Some(Self::Array(payload)),
            _ => None,
        }
    }

    /// Reads a packed value back, treating any unrecognised bit pattern as
    /// [`Self::Empty`].
    #[must_use]
    pub const fn unpack(bits: u64) -> Self {
        match Self::try_unpack(bits) {
            Some(value) => value,
            None => Self::Empty,
        }
    }
}

const fn u64_from_bool(flag: bool) -> u64 {
    if flag { 1 } else { 0 }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the argument is masked to 32 bits"
)]
const fn low_half(masked: u64) -> u32 {
    masked as u32
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{CalcError, Value};

    #[test]
    fn values_survive_packing() {
        let values = [
            Value::Empty,
            Value::Number(0.0),
            Value::Number(1.5),
            Value::Number(-42.25),
            Value::Number(f64::MAX),
            Value::Number(f64::MIN_POSITIVE),
            Value::Number(5.0e-324), // subnormal preserved (D-027)
            Value::Text(0),
            Value::Text(u32::MAX),
            Value::Bool(false),
            Value::Bool(true),
            Value::Error(CalcError::Div0),
            Value::Error(CalcError::Ref),
            Value::Error(CalcError::Spill),
            Value::Error(CalcError::Cycle),
            Value::Array(0),
            Value::Array(99),
        ];
        for value in values {
            let unpacked = Value::unpack(value.pack());
            match (value, unpacked) {
                (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                _ => assert_eq!(value, unpacked),
            }
        }
        // Negative zero is canonicalised to +0.0 on storage (D-027).
        assert_eq!(Value::Number(-0.0).pack(), 0.0_f64.to_bits());
    }

    #[test]
    fn a_nan_is_never_stored_as_a_number() {
        assert_eq!(
            Value::unpack(Value::Number(f64::NAN).pack()),
            Value::Error(CalcError::Num)
        );
        assert_eq!(
            Value::unpack(Value::Number(f64::INFINITY).pack()),
            Value::Error(CalcError::Num)
        );
        assert_eq!(Value::try_unpack(f64::NAN.to_bits()), None);
        assert_eq!(Value::unpack(f64::NAN.to_bits()), Value::Empty);
        assert_eq!(Value::unpack(0xFFF8_0000_0000_0000), Value::Empty);
        // A foreign bit pattern cannot pass for text or error.
        assert_eq!(Value::try_unpack(0x7FF8_0002_0000_0007), None);
        assert_eq!(Value::try_unpack(0x7FFC_0004_0000_00FF), None);
    }

    #[test]
    fn error_literals_parse_case_insensitively() {
        for err in [
            CalcError::Div0,
            CalcError::Value,
            CalcError::Ref,
            CalcError::Name,
            CalcError::Num,
            CalcError::Na,
            CalcError::Null,
            CalcError::Spill,
            CalcError::Calc,
            CalcError::Cycle,
        ] {
            assert_eq!(CalcError::parse_literal(err.as_str()), Some(err));
        }
        assert_eq!(CalcError::parse_literal("#div/0!"), Some(CalcError::Div0));
        assert_eq!(CalcError::parse_literal("#n/a"), Some(CalcError::Na));
        assert_eq!(CalcError::parse_literal("#NOPE!"), None);
    }
}
