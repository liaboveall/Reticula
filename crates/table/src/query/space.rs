//! The values of a column as small unsigned integers that compare like them.

use super::exact::{float_bits, float_key};
use super::spec::Value;
use crate::bits;
use crate::layout::{Block, Directory, Encoding};
use crate::types::{CellValue, ColumnType};

/// Where the values of a column lie. The key of a value is its distance from
/// the smallest one, in steps of a power of ten that all distances have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Space {
    Integral { low: i64, factor: u64, span: u64 },
    Float { low: u64, span: u64 },
}

/// How the words of a block become keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scale {
    Integral { offset: u64, times: u64 },
    Float { low: u64 },
}

impl Scale {
    pub(super) const fn key(self, word: u64) -> u64 {
        match self {
            Self::Integral { offset, times } => offset.wrapping_add(word.wrapping_mul(times)),
            Self::Float { low } => float_key(word).wrapping_sub(low),
        }
    }
}

/// The blocks of a column that have values, of the groups that are used.
fn blocks<'a>(
    directory: &'a Directory,
    column: usize,
    used: &'a dyn Fn(u32) -> bool,
) -> impl Iterator<Item = &'a Block> {
    (0..directory.groups())
        .filter(move |group| used(*group))
        .filter_map(move |group| directory.block(column, group))
        .filter(|block| block.encoding != Encoding::Blank)
}

impl Space {
    /// Of a column of numbers, dates, times or truth values. `None` when there
    /// are no values.
    pub(super) fn of(
        directory: &Directory,
        column: usize,
        used: &dyn Fn(u32) -> bool,
    ) -> Option<Self> {
        let kind = directory.columns.get(column)?.kind;
        if kind == ColumnType::Text {
            return None;
        }
        if kind == ColumnType::Float {
            let mut range: Option<(u64, u64)> = None;
            for block in blocks(directory, column, used) {
                let (low, high) = (float_key(block.low), float_key(block.high));
                let (a, b) = range.unwrap_or((low, high));
                range = Some((a.min(low), b.max(high)));
            }
            return range.map(|(low, high)| Self::Float {
                low,
                span: high.saturating_sub(low),
            });
        }
        let mut range: Option<(i64, i64)> = None;
        // The values of a block are a multiple of this apart, unless there is
        // only one.
        let mut unit = 18_u8;
        for block in blocks(directory, column, used) {
            let (low, high) = (block.low.cast_signed(), block.high.cast_signed());
            let (a, b) = range.unwrap_or((low, high));
            range = Some((a.min(low), b.max(high)));
            if block.width > 0 {
                unit = unit.min(block.unit);
            }
        }
        let (low, high) = range?;
        for block in blocks(directory, column, used) {
            let apart = block.low.wrapping_sub(low.cast_unsigned());
            while unit > 0 && apart.checked_rem(power(unit)) != Some(0) {
                unit = unit.saturating_sub(1);
            }
        }
        let factor = power(unit);
        let span = high
            .wrapping_sub(low)
            .cast_unsigned()
            .checked_div(factor)
            .unwrap_or(0);
        Some(Self::Integral { low, factor, span })
    }

    /// The largest key.
    pub(super) const fn span(&self) -> u64 {
        match self {
            Self::Integral { span, .. } | Self::Float { span, .. } => *span,
        }
    }

    pub(super) const fn is_integral(&self) -> bool {
        matches!(self, Self::Integral { .. })
    }

    /// Bits of the largest key.
    pub(super) const fn width(&self) -> u32 {
        bits::width_of(self.span())
    }

    pub(super) fn scale(&self, block: &Block) -> Scale {
        match *self {
            Self::Integral { low, factor, .. } => Scale::Integral {
                offset: block
                    .low
                    .wrapping_sub(low.cast_unsigned())
                    .checked_div(factor)
                    .unwrap_or(0),
                times: block.factor().checked_div(factor).unwrap_or(0),
            },
            Self::Float { low, .. } => Scale::Float { low },
        }
    }

    /// The stored value a key stands for: an integer, or the order key of a
    /// floating-point number.
    pub(super) const fn stored(&self, key: u64) -> u64 {
        match *self {
            Self::Integral { low, factor, .. } => {
                low.cast_unsigned().wrapping_add(key.wrapping_mul(factor))
            }
            Self::Float { low, .. } => low.wrapping_add(key),
        }
    }

    /// The value a key stands for.
    pub(super) const fn cell(&self, key: u64) -> CellValue {
        match self {
            Self::Integral { .. } => CellValue::Int(self.stored(key).cast_signed()),
            Self::Float { .. } => CellValue::Float(f64::from_bits(float_bits(self.stored(key)))),
        }
    }

    /// The value a key stands for.
    pub(super) const fn value(&self, key: u64) -> Value {
        match self {
            Self::Integral { .. } => Value::Int(self.stored(key).cast_signed()),
            Self::Float { .. } => Value::Float(float_bits(self.stored(key))),
        }
    }

    /// The smallest and the largest key of a block.
    pub(super) fn keys_of(&self, block: &Block) -> (u64, u64) {
        match *self {
            Self::Integral { low, factor, .. } => {
                let key = |value: u64| {
                    value
                        .wrapping_sub(low.cast_unsigned())
                        .checked_div(factor)
                        .unwrap_or(0)
                };
                (key(block.low), key(block.high))
            }
            Self::Float { low, .. } => (
                float_key(block.low).wrapping_sub(low),
                float_key(block.high).wrapping_sub(low),
            ),
        }
    }
}

const fn power(unit: u8) -> u64 {
    10_u64.saturating_pow(unit as u32)
}
