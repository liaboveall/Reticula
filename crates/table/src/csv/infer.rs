//! Which type a column's values have in common.

use super::value::{Class, DECIMAL_DIGITS, classify, digit_count, is_blank_marker};
use crate::types::ColumnType;

/// What the values of a column were so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Seen {
    /// Whether there were numbers without an exponent.
    plain: bool,
    /// Whether there were numbers with one.
    exponents: bool,
    pub(super) dates: bool,
    pub(super) times: bool,
    pub(super) booleans: bool,
    text: bool,
    /// Whether one of the numbers does not survive the conversion to floating point.
    pub(super) lossy: bool,
    /// Of the numbers without an exponent: most digits before the point,
    whole: u32,
    /// fewest after it,
    least: u32,
    /// and most.
    most: u32,
}

impl Seen {
    /// Numbers without an exponent: the most digits before the point, and
    /// the fewest and the most after it.
    pub(super) fn numbers(&mut self, whole: u32, least: u32, most: u32) {
        self.least = if self.plain {
            self.least.min(least)
        } else {
            least
        };
        self.plain = true;
        self.whole = self.whole.max(whole);
        self.most = self.most.max(most);
    }

    /// Another field, which may be blank. `markers` says that some texts are.
    #[inline(never)]
    pub(super) fn look(&mut self, text: &[u8], markers: bool) {
        if !(text.is_empty() || markers && is_blank_marker(text)) {
            self.add(classify(text));
        }
    }

    /// Another value.
    pub(super) fn add(&mut self, class: Class) {
        match class {
            Class::Integer { value, lossless } => {
                self.numbers(digit_count(value.unsigned_abs()), 0, 0);
                self.lossy |= !lossless;
            }
            Class::Decimal {
                whole,
                fraction,
                lossless,
            } => {
                self.numbers(whole, fraction, fraction);
                self.lossy |= !lossless;
            }
            Class::Exponent { lossless } => {
                self.exponents = true;
                self.lossy |= !lossless;
            }
            Class::Date => self.dates = true,
            Class::DateTime => self.times = true,
            Class::Boolean => self.booleans = true,
            Class::Text => self.text = true,
        }
    }

    /// The narrowest type that holds all the values. `None` without values.
    #[inline(never)]
    pub(super) fn kind(&self) -> Option<ColumnType> {
        let numbers = self.plain || self.exponents;
        let days = self.dates || self.times;
        let groups = u8::from(numbers)
            .saturating_add(u8::from(days))
            .saturating_add(u8::from(self.booleans));
        if self.text || groups > 1 {
            return Some(ColumnType::Text);
        }
        if numbers {
            return Some(self.number());
        }
        if days {
            return Some(if self.times {
                ColumnType::DateTime
            } else {
                ColumnType::Date
            });
        }
        self.booleans.then_some(ColumnType::Boolean)
    }

    fn number(&self) -> ColumnType {
        if !self.exponents {
            if self.most == 0 {
                return ColumnType::Integer;
            }
            if self.whole.saturating_add(self.most) <= DECIMAL_DIGITS
                && let Ok(scale) = u8::try_from(self.most)
            {
                return ColumnType::Decimal {
                    scale,
                    fixed: self.is_fixed(),
                };
            }
        }
        if self.lossy {
            ColumnType::Text
        } else {
            ColumnType::Float
        }
    }

    /// Whether all numbers have the same number of decimals.
    pub(super) const fn is_fixed(&self) -> bool {
        self.least == self.most
    }

    /// The most decimals of a number.
    pub(super) const fn most_decimals(&self) -> u32 {
        self.most
    }

    /// Whether only text holds all the values. No further value changes that.
    pub(super) fn is_text(&self) -> bool {
        self.kind() == Some(ColumnType::Text)
    }
}
