//! A cell value in eight bytes.
//!
//! A number is stored as its own bit pattern. Everything else lives in the
//! payload of a NaN that arithmetic never produces, so a column is a plain
//! array of 64-bit words with no per-cell tag.
//!
//! This is the first cut: numbers, text and empty cells. Booleans and error
//! values join when the formula engine needs them.

/// What a cell holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// Nothing.
    Empty,
    /// A finite number or an infinity. Never a NaN: see [`Value::pack`].
    Number(f64),
    /// A string, by its index in the workbook's [`StringTable`].
    ///
    /// [`StringTable`]: crate::strings::StringTable
    Text(u32),
}

/// Top bits of every boxed word: a quiet NaN with one more bit set, which no
/// arithmetic result has.
const BOX: u64 = 0x7FFC_0000_0000_0000;
const BOX_MASK: u64 = 0xFFFC_0000_0000_0000;
const TAG_SHIFT: u32 = 32;
const TAG_EMPTY: u64 = 1;
const TAG_TEXT: u64 = 2;
const PAYLOAD_MASK: u64 = 0xFFFF_FFFF;

impl Value {
    /// The word an empty cell is stored as.
    pub const EMPTY_BITS: u64 = BOX | (TAG_EMPTY << TAG_SHIFT);

    /// Packs the value into 64 bits.
    ///
    /// A NaN is stored as an empty cell. A computed NaN is an error, not a
    /// number, and its bit pattern must never be mistaken for a tag; callers
    /// turn it into an error value before storing once those exist.
    #[must_use]
    pub const fn pack(self) -> u64 {
        match self {
            Self::Number(number) if !number.is_nan() => number.to_bits(),
            Self::Text(id) => BOX | (TAG_TEXT << TAG_SHIFT) | id as u64,
            Self::Empty | Self::Number(_) => Self::EMPTY_BITS,
        }
    }

    /// Reads a packed value back.
    #[must_use]
    pub const fn unpack(bits: u64) -> Self {
        if bits & BOX_MASK != BOX {
            let number = f64::from_bits(bits);
            // A NaN outside the boxed range was not written by `pack`.
            return if number.is_nan() {
                Self::Empty
            } else {
                Self::Number(number)
            };
        }
        if (bits >> TAG_SHIFT) & 0xF == TAG_TEXT {
            Self::Text(low_half(bits & PAYLOAD_MASK))
        } else {
            Self::Empty
        }
    }
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

    use super::Value;

    #[test]
    fn values_survive_packing() {
        let values = [
            Value::Empty,
            Value::Number(0.0),
            Value::Number(-0.0),
            Value::Number(1.5),
            Value::Number(f64::MAX),
            Value::Number(f64::MIN_POSITIVE),
            Value::Number(f64::INFINITY),
            Value::Number(f64::NEG_INFINITY),
            Value::Text(0),
            Value::Text(u32::MAX),
        ];
        for value in values {
            let unpacked = Value::unpack(value.pack());
            match (value, unpacked) {
                (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_bits(), b.to_bits()),
                _ => assert_eq!(value, unpacked),
            }
        }
    }

    #[test]
    fn a_nan_is_never_stored_as_a_number() {
        assert_eq!(Value::Number(f64::NAN).pack(), Value::EMPTY_BITS);
        assert_eq!(Value::unpack(f64::NAN.to_bits()), Value::Empty);
        assert_eq!(Value::unpack(0xFFF8_0000_0000_0000), Value::Empty);
        // A foreign bit pattern cannot pass for text.
        assert_eq!(Value::unpack(0x7FF8_0002_0000_0007), Value::Empty);
    }
}
