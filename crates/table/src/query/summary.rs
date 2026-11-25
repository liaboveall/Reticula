//! Totals of columns over the rows that are shown.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use super::QueryError;
use super::cells::{Cells, Part, each_word};
use super::context::{Context, Step};
use super::exact::{float_bits, float_key};
use super::memory::Large;
use super::ranks::{Ranking, Ranks};
use super::selection::{Selection, count, get, next_set, set};
use super::session::{Outcome, Work};
use super::sort::is_coded;
use crate::layout::{Block, Encoding};
use crate::text;
use crate::types::{CellValue, ColumnType};

/// A number or value, as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Figure {
    /// The text.
    pub text: String,
    /// Whether it is exactly the figure, and not rounded.
    pub exact: bool,
}

/// What there is to say of a column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSummary {
    /// The column.
    pub column: usize,
    /// Number of rows.
    pub rows: u64,
    /// How many of their cells are blank.
    pub blanks: u64,
    /// How many different texts there are, regardless of case. Of text columns
    /// that are in a dictionary completely.
    pub distinct: Option<u64>,
    /// The sum, of numbers.
    pub sum: Option<Figure>,
    /// The mean, of numbers.
    pub mean: Option<Figure>,
    /// The smallest value. Not of texts that are not in a dictionary.
    pub min: Option<Figure>,
    /// The largest value, likewise.
    pub max: Option<Figure>,
}

/// What is known of a column so far.
#[derive(Debug, Default)]
struct Totals {
    values: u64,
    sum: i128,
    /// The sum of floating-point numbers, and what it lacks.
    float: f64,
    lacks: f64,
    /// The smallest and largest value: integers, or the order keys of
    /// floating-point numbers.
    ints: Option<(i64, i64)>,
    floats: Option<(u64, u64)>,
}

impl Totals {
    fn add_range(&mut self, low: i64, high: i64) {
        let (a, b) = self.ints.unwrap_or((low, high));
        self.ints = Some((a.min(low), b.max(high)));
    }

    /// Adds a number and keeps what got lost in rounding.
    fn add_float(&mut self, number: f64) {
        let sum = self.float + number;
        let added = sum - self.float;
        self.lacks += (self.float - (sum - added)) + (number - added);
        self.float = sum;
    }
}

#[derive(Debug)]
enum Stage {
    Begin,
    Rank(Box<Ranking>),
    Seen,
    Scan(u32),
    /// The texts of the smallest and the largest rank.
    Texts,
}

/// Totals being worked out.
#[derive(Debug)]
pub(super) struct Summing {
    rows: Selection,
    columns: Vec<usize>,
    /// The column that is worked on, as a place in the list.
    at: usize,
    totals: Totals,
    ranks: Option<Rc<Ranks>>,
    /// The ranks that occur.
    seen: Large<u64>,
    stage: Stage,
    done: Vec<ColumnSummary>,
}

impl Summing {
    pub(super) fn new(cx: &Context<'_>, rows: Selection, columns: Vec<usize>) -> Self {
        Self {
            rows,
            columns,
            at: 0,
            totals: Totals::default(),
            ranks: None,
            seen: cx.empty(),
            stage: Stage::Begin,
            done: Vec::new(),
        }
    }

    /// Cells this works on in all, about.
    pub(super) fn size(rows: u32, columns: usize) -> u64 {
        u64::from(rows).saturating_mul(u64::try_from(columns).unwrap_or(1))
    }
}

impl Work for Summing {
    fn finish(&mut self) -> Outcome {
        Outcome::Summary(core::mem::take(&mut self.done))
    }

    fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let Some(column) = self.columns.get(self.at).copied() else {
            return Ok(Step::done(1));
        };
        match &mut self.stage {
            Stage::Begin => {
                self.totals = Totals::default();
                self.ranks = None;
                let directory = cx.reader.directory();
                let of = directory.columns.get(column);
                let coded =
                    of.is_some_and(|of| of.dictionary.is_some()) && is_coded(directory, column);
                self.stage = if !coded {
                    Stage::Scan(0)
                } else if let Some(ranks) = cx.caches.ranks(column) {
                    self.ranks = Some(ranks);
                    Stage::Seen
                } else {
                    Stage::Rank(Box::new(Ranking::new(cx, column, None)?))
                };
                Ok(Step::more(1))
            }
            Stage::Rank(ranking) => {
                let step = ranking.step(cx)?;
                if step.done {
                    let ranks = Rc::new(ranking.finish());
                    cx.caches.keep_ranks(column, &ranks);
                    self.ranks = Some(ranks);
                    self.stage = Stage::Seen;
                }
                Ok(step.part())
            }
            Stage::Seen => {
                let classes = self.ranks.as_ref().map_or(0, |ranks| ranks.classes);
                let words = (classes as usize).div_ceil(64);
                self.seen = cx.large(words)?;
                // Every entry of a dictionary occurs somewhere in the column.
                let all = self.rows.bits().is_none();
                let fill = if all { u64::MAX } else { 0 };
                self.seen.fill(words, fill, words)?;
                self.stage = if all { Stage::Texts } else { Stage::Scan(0) };
                Ok(Step::more(words))
            }
            Stage::Scan(group) => {
                let group = *group;
                if group >= cx.reader.directory().groups() {
                    if self.ranks.is_some() {
                        self.stage = Stage::Texts;
                    } else {
                        self.close(cx, column, None);
                    }
                    return Ok(Step::more(1));
                }
                let work = self.scan(cx, column, group)?;
                self.stage = Stage::Scan(group.saturating_add(1));
                Ok(Step::more(work))
            }
            Stage::Texts => {
                if !cx.dictionary(column)? {
                    return Ok(Step::more(65_536));
                }
                if self.rows.bits().is_none() {
                    let blanks = cx.reader.columns().get(column).map_or(0, |of| of.blanks);
                    self.totals.values = u64::from(self.rows.len()).saturating_sub(blanks);
                }
                let texts = self.texts(cx, column);
                self.close(cx, column, Some(texts));
                Ok(Step::more(65_536))
            }
        }
    }
}

impl Summing {
    /// Adds up a group. Gives how much work that was.
    fn scan(
        &mut self,
        cx: &mut Context<'_>,
        column: usize,
        group: u32,
    ) -> Result<usize, QueryError> {
        let directory = cx.reader.directory();
        let Some(record) = directory.block(column, group).copied() else {
            return Ok(1);
        };
        let kind = directory
            .columns
            .get(column)
            .map_or(ColumnType::Text, |of| of.kind);
        let selected = self.rows.of_group(directory, group);
        let picked = self.rows.count_in(directory, group);
        if picked == 0 || record.encoding == Encoding::Blank {
            return Ok(1);
        }
        let all = picked == directory.rows_in(group) as usize;
        let mut known = false;
        if all {
            let values = directory.rows_in(group).saturating_sub(record.blanks);
            if kind.is_integral() {
                self.totals
                    .add_range(record.low.cast_signed(), record.high.cast_signed());
            }
            // The directory knows all there is to say.
            if !kind.is_number() && self.ranks.is_none() {
                self.totals.values = self.totals.values.saturating_add(u64::from(values));
                return Ok(1);
            }
            known = true;
        } else if kind == ColumnType::Text && self.ranks.is_none() && record.blanks == 0 {
            self.totals.values = self
                .totals
                .values
                .saturating_add(u64::try_from(picked).unwrap_or(0));
            return Ok(1);
        }
        cx.load(column, group, selected, Part::ALL)?;
        let cells = &*cx.cells;
        let values = (picked as u64).saturating_sub(cells.blanks(selected));
        self.totals.values = self.totals.values.saturating_add(values);
        if values == 0 {
            return Ok(cells.rows());
        }
        let chunks = cells.words().chunks(64).enumerate();
        if let Some(ranks) = &self.ranks {
            let of = ranks.of_entries();
            let seen = &mut self.seen;
            for (at, words) in chunks {
                each_word(cells.values(selected, at), words, |word| {
                    let code = usize::try_from(record.low.wrapping_add(word));
                    if let Some(rank) = code.ok().and_then(|code| of.get(code)) {
                        set(seen, *rank as usize);
                    }
                });
            }
        } else if kind == ColumnType::Float {
            let totals = &mut self.totals;
            let (mut low, mut high) = totals.floats.unwrap_or((u64::MAX, 0));
            for (at, words) in chunks {
                each_word(cells.values(selected, at), words, |word| {
                    totals.add_float(f64::from_bits(word));
                    let key = float_key(word);
                    low = low.min(key);
                    high = high.max(key);
                });
            }
            totals.floats = Some((low, high));
        } else if kind != ColumnType::Text {
            let part = add_words(cells, selected);
            self.add_part(&record, part, values, known);
        }
        Ok(cells.rows())
    }

    /// Adds the packed numbers of a block.
    fn add_part(&mut self, record: &Block, part: Added, values: u64, known: bool) {
        let factor = record.factor();
        let base = i128::from(record.low.cast_signed());
        let low = base.saturating_mul(i128::from(values));
        let rest = i128::try_from(part.sum.saturating_mul(u128::from(factor))).unwrap_or(i128::MAX);
        self.totals.sum = self.totals.sum.saturating_add(low).saturating_add(rest);
        if !known {
            self.totals.add_range(
                record.value(part.low, factor),
                record.value(part.high, factor),
            );
        }
    }

    /// The texts of the smallest and the largest rank, and how many there are.
    fn texts(&self, cx: &Context<'_>, column: usize) -> (u64, Option<(String, String)>) {
        let Some(ranks) = &self.ranks else {
            return (0, None);
        };
        let classes = ranks.classes as usize;
        let seen = &*self.seen;
        let low = next_set(seen, 0, classes);
        if low >= classes {
            return (0, None);
        }
        let high = (low..classes).rev().find(|rank| get(seen, *rank));
        let spelled = |rank: usize| {
            let of = ranks.of_entries().iter();
            let code = of.enumerate().find(|(_, of)| **of as usize == rank);
            let dictionary = cx.reader.dictionary(column);
            match (code, dictionary) {
                (Some((code, _)), Some(dictionary)) => String::from(dictionary.text(code)),
                _ => String::new(),
            }
        };
        let distinct = if self.rows.bits().is_none() {
            classes
        } else {
            count(seen)
        };
        (
            u64::try_from(distinct).unwrap_or(0),
            Some((spelled(low), spelled(high.unwrap_or(low)))),
        )
    }

    /// All of a column is known.
    fn close(
        &mut self,
        cx: &Context<'_>,
        column: usize,
        texts: Option<(u64, Option<(String, String)>)>,
    ) {
        let of = cx.reader.columns().get(column);
        let kind = of.map_or(ColumnType::Text, |of| of.kind);
        let totals = &self.totals;
        let rows = u64::from(self.rows.len());
        let mut summary = ColumnSummary {
            column,
            rows,
            blanks: rows.saturating_sub(totals.values),
            distinct: None,
            sum: None,
            mean: None,
            min: None,
            max: None,
        };
        let exact = |text: String| Figure { text, exact: true };
        match kind {
            ColumnType::Text => {
                let nothing = of.is_some_and(|of| of.blanks == u64::from(cx.reader.rows()));
                match texts {
                    Some((distinct, ends)) => {
                        summary.distinct = Some(distinct);
                        if let Some((min, max)) = ends {
                            summary.min = Some(exact(min));
                            summary.max = Some(exact(max));
                        }
                    }
                    None if nothing => summary.distinct = Some(0),
                    None => {}
                }
            }
            ColumnType::Float => {
                let shown = |number: f64| {
                    let mut text = String::new();
                    text::write_value(kind, CellValue::Float(number), &mut text);
                    Figure { text, exact: false }
                };
                if let Some((low, high)) = totals.floats {
                    let number = |key: u64| f64::from_bits(float_bits(key));
                    summary.min = Some(shown(number(low)));
                    summary.max = Some(shown(number(high)));
                    let sum = totals.float + totals.lacks;
                    if sum.is_finite() {
                        summary.sum = Some(shown(sum));
                        summary.mean = Some(shown(sum / as_float(totals.values)));
                    }
                }
            }
            _ => {
                let shown = |value: i64| {
                    let mut text = String::new();
                    text::write_value(kind, CellValue::Int(value), &mut text);
                    Figure { text, exact: true }
                };
                if let Some((low, high)) = totals.ints {
                    summary.min = Some(shown(low));
                    summary.max = Some(shown(high));
                    if kind.is_number() {
                        let (scale, fixed) = match kind {
                            ColumnType::Decimal { scale, fixed } => (scale, fixed),
                            _ => (0, false),
                        };
                        let mut text = String::new();
                        text::write_decimal(totals.sum, scale, fixed, &mut text);
                        summary.sum = Some(exact(text));
                        summary.mean = Some(quotient(totals.sum, totals.values, scale));
                    }
                }
            }
        }
        self.done.push(summary);
        self.at = self.at.saturating_add(1);
        self.stage = Stage::Begin;
    }
}

fn as_float(number: u64) -> f64 {
    // Rows are at most 100,000,000.
    f64::from(u32::try_from(number).unwrap_or(u32::MAX))
}

/// The packed numbers of a block, added up.
#[derive(Clone, Copy, Debug)]
struct Added {
    sum: u128,
    low: u64,
    high: u64,
}

fn add_words(cells: &Cells, selected: Option<&[u64]>) -> Added {
    let mut part = Added {
        sum: 0,
        low: u64::MAX,
        high: 0,
    };
    for (at, words) in cells.words().chunks(64).enumerate() {
        // Halves of 64 numbers fit into 64 bits.
        let (mut lows, mut highs) = (0_u64, 0_u64);
        let (mut low, mut high) = (part.low, part.high);
        each_word(cells.values(selected, at), words, |word| {
            lows = lows.wrapping_add(word & 0xffff_ffff);
            highs = highs.wrapping_add(word >> 32);
            low = low.min(word);
            high = high.max(word);
        });
        part.low = low;
        part.high = high;
        part.sum = part
            .sum
            .wrapping_add(u128::from(lows))
            .wrapping_add(u128::from(highs).wrapping_shl(32));
    }
    part
}

/// Most digits of a mean, unless there are more before its point.
const DIGITS: u32 = 15;

/// A sum divided by a number of values, in as many digits as it takes or as
/// there are at most, rounded half to even.
fn quotient(sum: i128, values: u64, scale: u8) -> Figure {
    let scaled = 10_u128.checked_pow(u32::from(scale)).unwrap_or(1);
    let by = u128::from(values.max(1)).saturating_mul(scaled);
    let size = sum.unsigned_abs();
    let mut mantissa = size.checked_div(by).unwrap_or(0);
    let mut rest = size.checked_rem(by).unwrap_or(0);
    let mut places = 0_u8;
    let mut digits = mantissa
        .checked_ilog10()
        .map_or(0, |log| log.saturating_add(1));
    while rest != 0 && digits < DIGITS && places < 36 {
        rest = rest.saturating_mul(10);
        let digit = rest.checked_div(by).unwrap_or(0);
        rest = rest.checked_rem(by).unwrap_or(0);
        mantissa = mantissa.saturating_mul(10).saturating_add(digit);
        places = places.saturating_add(1);
        if mantissa != 0 {
            digits = digits.saturating_add(1);
        }
    }
    let twice = rest.saturating_mul(2);
    if twice > by || (twice == by && mantissa & 1 == 1) {
        mantissa = mantissa.saturating_add(1);
    }
    let mantissa = i128::try_from(mantissa).unwrap_or(i128::MAX);
    let mut text = String::new();
    let signed = if sum < 0 {
        0_i128.saturating_sub(mantissa)
    } else {
        mantissa
    };
    text::write_decimal(signed, places, false, &mut text);
    Figure {
        text,
        exact: rest == 0,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn means() {
        let mean = |sum, values, scale| {
            let figure = quotient(sum, values, scale);
            (figure.text, figure.exact)
        };
        let is = |text: &str, exact| (String::from(text), exact);
        assert_eq!(mean(10, 4, 0), is("2.5", true));
        assert_eq!(mean(-10, 4, 1), is("-0.25", true));
        assert_eq!(mean(1, 3, 0), is("0.333333333333333", false));
        assert_eq!(mean(2, 3, 0), is("0.666666666666667", false));
        assert_eq!(mean(0, 3, 5), is("0", true));
        assert_eq!(
            mean(1, 3, 18),
            is("0.000000000000000000333333333333333", false)
        );
        assert_eq!(
            mean(123_456_789_012_345_678_901, 2, 0),
            is("61728394506172839450", false)
        );
        assert_eq!(
            mean(1, 100_000_000, 18),
            is("0.00000000000000000000000001", true)
        );
        assert_eq!(
            mean(i128::from(i64::MIN), 1, 0),
            is("-9223372036854775808", true)
        );
    }
}
