//! Conditions, made ready to be asked of many cells.

use alloc::vec::Vec;

use super::arrange::{in_order, sort_by};
use super::exact::{self, Range, Threshold, float_key};
use super::fold;
use super::spec::{Condition, NumberOp, SpecError, TimeOp, Value};
use crate::calendar::DAY_MICROS;
use crate::types::ColumnType;

/// Added to an integer, makes it an unsigned one that compares like it.
const SIGN: u64 = 1 << 63;

/// What a text, folded, has to be.
#[derive(Debug)]
pub(super) enum TextTest {
    /// One of these, which are found by their place in the order.
    OneOf(Vec<Vec<u8>>, Vec<u32>),
    Contains(Vec<u8>),
    StartsWith(Vec<u8>),
    EndsWith(Vec<u8>),
}

impl TextTest {
    pub(super) fn matches(&self, folded: &[u8]) -> bool {
        match self {
            Self::OneOf(texts, order) => {
                let of = |place: &u32| texts.get(*place as usize).map_or(&[][..], |text| text);
                order
                    .binary_search_by(|place| of(place).cmp(folded))
                    .is_ok()
            }
            Self::Contains(part) => fold::contains(folded, part),
            Self::StartsWith(part) => folded.starts_with(part),
            Self::EndsWith(part) => folded.ends_with(part),
        }
    }
}

/// What a cell has to be.
#[derive(Debug)]
pub(super) enum Test {
    /// Anything.
    Any,
    Blank,
    NotBlank,
    /// An integer.
    Ints(Range<i64>),
    /// One of some integers, in order.
    IntSet {
        values: Vec<i64>,
        blank: bool,
    },
    /// A floating-point number, by its order key.
    Floats(Range<u64>),
    /// One of some floating-point numbers, by their order keys, in order.
    FloatSet {
        values: Vec<u64>,
        blank: bool,
    },
    Text {
        test: TextTest,
        blank: bool,
    },
    /// One of the first or last values of the column. This is looked up
    /// before any cell is asked.
    Top {
        n: u32,
        bottom: bool,
    },
}

/// A condition on a column, made ready.
#[derive(Debug)]
pub(super) struct Term {
    pub(super) column: usize,
    pub(super) test: Test,
}

impl Term {
    pub(super) fn new(
        column: usize,
        kind: ColumnType,
        condition: &Condition,
    ) -> Result<Self, SpecError> {
        let refused = SpecError::DoesNotApply { column };
        let only = |applies: bool| if applies { Ok(()) } else { Err(refused) };
        let text = |test: TextTest, part: &str| {
            only(kind == ColumnType::Text)?;
            // What nothing is asked of holds for blank cells too.
            Ok(if part.is_empty() {
                Test::Any
            } else {
                Test::Text { test, blank: false }
            })
        };
        let folded = |part: &str| part.to_lowercase().into_bytes();
        let test = match condition {
            Condition::OneOf { values } => one_of(kind, values).ok_or(refused)?,
            Condition::TextContains(part) => text(TextTest::Contains(folded(part)), part)?,
            Condition::TextStartsWith(part) => text(TextTest::StartsWith(folded(part)), part)?,
            Condition::TextEndsWith(part) => text(TextTest::EndsWith(folded(part)), part)?,
            Condition::Number { op, a, b } => {
                only(kind.is_number())?;
                let wrong = SpecError::NotANumber { column };
                let a = Threshold::parse(a).ok_or(wrong)?;
                let b = match (op, b) {
                    (NumberOp::Between, None) => return Err(SpecError::OneThreshold { column }),
                    (NumberOp::Between, Some(b)) => Some(Threshold::parse(b).ok_or(wrong)?),
                    _ => None,
                };
                match kind {
                    ColumnType::Float => Test::Floats(exact::floats(*op, &a, b.as_ref())),
                    ColumnType::Decimal { scale, .. } => {
                        Test::Ints(exact::integers(*op, &a, b.as_ref(), scale))
                    }
                    _ => Test::Ints(exact::integers(*op, &a, b.as_ref(), 0)),
                }
            }
            Condition::Time { op, a, b } => {
                let times = kind == ColumnType::DateTime;
                only(times || kind == ColumnType::Date)?;
                if *op == TimeOp::Between && b.is_none() {
                    return Err(SpecError::OneThreshold { column });
                }
                Test::Ints(time(*op, *a, *b, times))
            }
            Condition::Top { n, bottom } => {
                only(!matches!(kind, ColumnType::Text | ColumnType::Boolean))?;
                Test::Top {
                    n: *n,
                    bottom: *bottom,
                }
            }
            Condition::Blank => Test::Blank,
            Condition::NotBlank => Test::NotBlank,
        };
        Ok(Self { column, test })
    }
}

fn time(op: TimeOp, a: i64, b: Option<i64>, times: bool) -> Range<i64> {
    let at = i128::from(a);
    let next = |value: i128| value.saturating_add(1);
    let last = |value: i128| value.saturating_sub(1);
    match op {
        TimeOp::Before => Range::clamped(i128::MIN, last(at)),
        TimeOp::After => Range::clamped(next(at), i128::MAX),
        TimeOp::Between => {
            let to = b.map_or(at, i128::from);
            Range::clamped(at.min(to), at.max(to))
        }
        TimeOp::On if times => {
            let day = i128::from(DAY_MICROS);
            let from = at.saturating_sub(at.checked_rem_euclid(day).unwrap_or(0));
            Range::clamped(from, last(from.saturating_add(day)))
        }
        TimeOp::On => Range::clamped(at, at),
    }
}

/// `None` when one of the values is not of the column's type.
fn one_of(kind: ColumnType, values: &[Value]) -> Option<Test> {
    let mut blank = false;
    let mut numbers = Vec::new();
    let mut texts = Vec::new();
    for value in values {
        match value {
            Value::Blank => blank = true,
            Value::Int(value) if kind.is_integral() => numbers.push(value.cast_unsigned() ^ SIGN),
            Value::Float(bits) if kind == ColumnType::Float => {
                let value = f64::from_bits(*bits);
                if !value.is_nan() {
                    // There is one zero.
                    let value = if value == 0.0 { 0.0 } else { value };
                    numbers.push(float_key(value.to_bits()));
                }
            }
            Value::Text(text) if kind == ColumnType::Text => {
                if text.is_empty() {
                    blank = true;
                    continue;
                }
                texts.push(text.to_lowercase().into_bytes());
            }
            Value::Int(_) | Value::Float(_) | Value::Text(_) => return None,
        }
    }
    let numbers = in_order(&numbers);
    Some(match kind {
        ColumnType::Text => {
            let count = u32::try_from(texts.len()).unwrap_or(u32::MAX);
            let mut order: Vec<u32> = (0..count).collect();
            sort_by(&mut order, &mut Vec::new(), &mut |a, b| {
                texts.get(a as usize).cmp(&texts.get(b as usize))
            });
            Test::Text {
                test: TextTest::OneOf(texts, order),
                blank,
            }
        }
        ColumnType::Float => Test::FloatSet {
            values: numbers,
            blank,
        },
        _ => Test::IntSet {
            values: numbers
                .iter()
                .map(|key| (key ^ SIGN).cast_signed())
                .collect(),
            blank,
        },
    })
}
