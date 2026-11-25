//! Finding the rows that all conditions hold for.
//!
//! The conditions are taken one after another, those that need the least to be
//! read first. Each goes through the column a group at a time, and leaves out the
//! groups that have no rows left, and those whose smallest and largest values
//! answer for all of their cells. Conditions on texts are asked of every entry
//! of the dictionary first, and then of the codes.

use alloc::vec::Vec;

use super::QueryError;
use super::cells::{Cells, Part, is_coded};
use super::context::{Context, Step};
use super::exact::{Range, float_key};
use super::fold::fold;
use super::memory::Large;
use super::predicate::{Term, Test, TextTest};
use super::selection::{Selection, count, get, group_range, next_set, set, tail};
use super::top::TopSearch;
use crate::layout::{Block, Directory, Encoding};

#[derive(Debug)]
enum Stage {
    Begin,
    Top(TopSearch),
    /// The entries the condition holds for, so far.
    Entries(Large<u64>, usize),
    /// The group that is next, and the row in it.
    Scan(Option<Large<u64>>, u32, usize),
    Count(usize, usize),
}

/// Rows being filtered.
#[derive(Debug)]
pub(super) struct Filtering {
    /// The conditions that are left.
    left: Vec<Term>,
    term: Option<Term>,
    /// Whether the condition is the first one.
    first: bool,
    bits: Large<u64>,
    stage: Stage,
    out: Vec<u64>,
    scratch: Vec<u8>,
}

/// How much there is to do for a condition: whether something has to be found
/// out before any cell is asked, and how many bytes have to be read.
fn effort(directory: &Directory, term: &Term) -> (bool, u64) {
    let asked = !matches!(term.test, Test::Text { .. } | Test::Top { .. });
    let all = matches!(term.test, Test::Top { .. });
    let bytes = (0..directory.groups())
        .filter(|group| all || matches!(judge(directory, term, None, *group), Verdict::Read))
        .filter_map(|group| directory.block(term.column, group))
        .map(|block| u64::from(block.len))
        .fold(0, u64::saturating_add);
    (!asked, bytes)
}

impl Filtering {
    pub(super) fn new(cx: &mut Context<'_>, terms: Vec<Term>) -> Result<Self, QueryError> {
        let words = (cx.reader.rows() as usize).div_ceil(64);
        Ok(Self {
            left: terms,
            term: None,
            first: true,
            bits: cx.large(words)?,
            stage: Stage::Begin,
            out: Vec::new(),
            scratch: Vec::new(),
        })
    }

    /// Cells this works on in all, about.
    pub(super) fn size(rows: u32, terms: usize) -> u64 {
        u64::from(rows).saturating_mul(u64::try_from(terms).unwrap_or(1))
    }

    /// The rows, when the last step is done.
    pub(super) fn finish(&mut self) -> Selection {
        let count = match self.stage {
            Stage::Count(_, count) => count,
            _ => 0,
        };
        Selection::some(self.bits.take(), u32::try_from(count).unwrap_or(u32::MAX))
    }

    pub(super) fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        match &mut self.stage {
            Stage::Begin => self.begin(cx),
            Stage::Top(search) => {
                let step = search.step(cx)?;
                if step.done {
                    if let Some(term) = &mut self.term {
                        term.test = search.finish();
                    }
                    self.stage = Stage::Scan(None, 0, 0);
                }
                Ok(step.part())
            }
            Stage::Entries(_, from) => {
                let from = *from;
                self.ask_entries(cx, from)
            }
            Stage::Scan(_, group, row) => {
                let at = (*group, *row);
                self.scan(cx, at)
            }
            Stage::Count(at, sum) => {
                let to = at.saturating_add(65_536).min(self.bits.len());
                let words = self.bits.get(*at..to).unwrap_or(&[]);
                *sum = sum.saturating_add(count(words));
                let work = words.len();
                *at = to;
                Ok(if to >= self.bits.len() {
                    Step::done(work)
                } else {
                    Step::more(work)
                })
            }
        }
    }

    fn begin(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        // Those that need the least come first, and the first of them.
        let directory = cx.reader.directory();
        let efforts = self.left.iter().map(|term| effort(directory, term));
        let least = efforts.enumerate().min_by_key(|(_, effort)| *effort);
        self.term = least.map(|(at, _)| self.left.remove(at));
        let Some(term) = &self.term else {
            self.stage = Stage::Count(0, 0);
            return Ok(Step::more(1));
        };
        self.stage = match &term.test {
            Test::Top { n, bottom } => Stage::Top(TopSearch::new(cx, term.column, *n, *bottom)?),
            Test::Text { .. } => match cx.place(term.column) {
                None => Stage::Scan(None, 0, 0),
                Some(place) => {
                    let words = (place.entries as usize).div_ceil(64);
                    let mut hits = cx.large(words)?;
                    hits.fill(words, 0, words)?;
                    Stage::Entries(hits, 0)
                }
            },
            _ => Stage::Scan(None, 0, 0),
        };
        Ok(Step::more(1))
    }

    fn ask_entries(&mut self, cx: &mut Context<'_>, from: usize) -> Result<Step, QueryError> {
        let Some(term) = &self.term else {
            self.stage = Stage::Begin;
            return Ok(Step::more(1));
        };
        if !cx.dictionary(term.column)? {
            return Ok(Step::more(65_536));
        }
        let Stage::Entries(hits, at) = &mut self.stage else {
            return Ok(Step::more(1));
        };
        let dictionary = cx.reader.dictionary(term.column);
        let entries = dictionary.map_or(0, crate::reader::Dictionary::len);
        let mut to = from;
        if let (Some(dictionary), Test::Text { test, .. }) = (dictionary, &term.test) {
            let end = from.saturating_add(cx.limits.entries).min(entries);
            let mut bytes = 0_usize;
            while to < end && bytes < cx.limits.part {
                let text = fold(dictionary.bytes_of(to), &mut self.scratch)?;
                bytes = bytes.saturating_add(text.len());
                if test.matches(text) {
                    set(hits, to);
                }
                to = to.saturating_add(1);
            }
        } else {
            to = entries;
        }
        *at = to;
        if to >= entries {
            let stage = core::mem::replace(&mut self.stage, Stage::Begin);
            if let Stage::Entries(hits, _) = stage {
                self.stage = Stage::Scan(Some(hits), 0, 0);
            }
        }
        Ok(Step::more(to.saturating_sub(from)))
    }

    fn scan(
        &mut self,
        cx: &mut Context<'_>,
        (group, row): (u32, usize),
    ) -> Result<Step, QueryError> {
        let (Some(term), Stage::Scan(hits, at, next)) = (&self.term, &mut self.stage) else {
            self.stage = Stage::Begin;
            return Ok(Step::more(1));
        };
        let hits = hits.as_deref();
        let directory = cx.reader.directory();
        let groups = directory.groups();
        if group >= groups {
            self.first = false;
            self.stage = Stage::Begin;
            return Ok(Step::more(1));
        }
        let rows = directory.rows_in(group) as usize;
        let (from, to) = group_range(directory, group);
        let left = if self.first {
            None
        } else {
            Some(self.bits.get(from..to).unwrap_or(&[]))
        };
        let wanted = left.is_none_or(|left| left.iter().any(|word| *word != 0));
        let verdict = if wanted {
            judge(directory, term, hits, group)
        } else {
            Verdict::None
        };
        let mut work = rows >> 6;
        match verdict {
            Verdict::None | Verdict::All if self.first => {
                let all = matches!(verdict, Verdict::All);
                for word in 0..to.saturating_sub(from) {
                    self.bits.push(if all { tail(rows, word) } else { 0 })?;
                }
            }
            Verdict::None => self.bits.get_mut(from..to).unwrap_or(&mut []).fill(0),
            Verdict::All => {}
            Verdict::Read => {
                let part = if matches!(term.test, Test::Text { .. }) {
                    Part::texts(row)
                } else {
                    Part::ALL
                };
                let end = cx.load(term.column, group, left, part)?;
                let cells = &*cx.cells;
                let coded = is_coded(cx.reader.directory(), term.column, cells.record());
                let hits = hits.filter(|_| coded);
                work = cells.rows();
                self.out.clear();
                self.out.resize(cells.rows().div_ceil(64), 0);
                evaluate(term, hits, cells, left, &mut self.out, &mut self.scratch)?;
                if self.first && row == 0 {
                    // Those that it does not hold for are taken away.
                    for word in 0..to.saturating_sub(from) {
                        self.bits.push(tail(rows, word))?;
                    }
                }
                let left = self.bits.get_mut(from..to).unwrap_or(&mut []);
                if cells.is_compact() {
                    keep_among(left, &self.out, row, end);
                } else {
                    for (left, out) in left.iter_mut().zip(&self.out) {
                        *left &= out;
                    }
                }
                if end < rows {
                    *next = end;
                    return Ok(Step::more(work));
                }
            }
        }
        *at = group.saturating_add(1);
        *next = 0;
        Ok(Step::more(work))
    }
}

/// Keeps those of the set bits, from one to another, whose numbers among them
/// are set in `keep`.
fn keep_among(bits: &mut [u64], keep: &[u64], from: usize, to: usize) {
    let mut number = 0_usize;
    for (at, word) in bits.iter_mut().enumerate() {
        let mut left = *word & tail(to, at) & !tail(from, at);
        while left != 0 {
            let bit = left.isolate_lowest_one();
            left &= left.wrapping_sub(1);
            if !get(keep, number) {
                *word &= !bit;
            }
            number = number.saturating_add(1);
        }
    }
}

/// What is known of a group before it is read.
enum Verdict {
    /// The condition holds for no cell, for all of them, or for some.
    None,
    All,
    Read,
}

fn judge(directory: &Directory, term: &Term, hits: Option<&[u64]>, group: u32) -> Verdict {
    let Some(block) = directory.block(term.column, group) else {
        return Verdict::None;
    };
    let takes_blanks = match &term.test {
        Test::Any | Test::Blank => true,
        Test::NotBlank | Test::Ints(_) | Test::Floats(_) | Test::Top { .. } => false,
        Test::IntSet { blank, .. } | Test::FloatSet { blank, .. } | Test::Text { blank, .. } => {
            *blank
        }
    };
    if block.encoding == Encoding::Blank {
        return if takes_blanks {
            Verdict::All
        } else {
            Verdict::None
        };
    }
    // Whether it holds for every value of the block, or for none.
    let values = match &term.test {
        Test::Any | Test::NotBlank => Some(true),
        Test::Blank | Test::Top { .. } => Some(false),
        Test::Ints(range) => {
            let (low, high) = (block.low.cast_signed(), block.high.cast_signed());
            judge_range(range, low, high)
        }
        Test::IntSet { values, .. } => {
            let (low, high) = (block.low.cast_signed(), block.high.cast_signed());
            judge_set(values, low, high)
        }
        Test::Floats(range) => judge_range(range, float_key(block.low), float_key(block.high)),
        Test::FloatSet { values, .. } => {
            judge_set(values, float_key(block.low), float_key(block.high))
        }
        Test::Text { .. } => hits
            .filter(|_| is_coded(directory, term.column, block))
            .and_then(|hits| judge_codes(hits, block)),
    };
    match values {
        Some(all) if block.blanks == 0 || all == takes_blanks => {
            if all {
                Verdict::All
            } else {
                Verdict::None
            }
        }
        _ => Verdict::Read,
    }
}

fn judge_range<T: Ord + Copy>(range: &Range<T>, low: T, high: T) -> Option<bool> {
    if range.low > range.high || high < range.low || low > range.high {
        return Some(!range.inside);
    }
    (range.low <= low && high <= range.high).then_some(range.inside)
}

fn judge_set<T: Ord + Copy>(values: &[T], low: T, high: T) -> Option<bool> {
    let from = values.partition_point(|value| *value < low);
    let first = values.get(from);
    if first.is_none_or(|first| *first > high) {
        return Some(false);
    }
    (low == high).then_some(true)
}

fn judge_codes(hits: &[u64], block: &Block) -> Option<bool> {
    let code = |bits: u64| usize::try_from(bits).unwrap_or(usize::MAX);
    let (low, high) = (code(block.low), code(block.high));
    let end = high.saturating_add(1);
    if end > hits.len().saturating_mul(64) || low > high {
        return None;
    }
    let found = next_set(hits, low, end);
    if found >= end {
        return Some(false);
    }
    // Looking for one it does not hold for costs about as much as reading the
    // block, unless there are few.
    let few = end.saturating_sub(low) <= 64;
    (few && (low..end).all(|code| get(hits, code))).then_some(true)
}

/// Asks the cells that were loaded. The bits are set where it holds.
fn evaluate(
    term: &Term,
    hits: Option<&[u64]>,
    cells: &Cells,
    selected: Option<&[u64]>,
    out: &mut [u64],
    scratch: &mut Vec<u8>,
) -> Result<(), QueryError> {
    let blanks = match &term.test {
        Test::Any => {
            out.fill(u64::MAX);
            true
        }
        Test::Blank => true,
        Test::NotBlank => {
            out.fill(u64::MAX);
            false
        }
        // It was looked up.
        Test::Top { .. } => false,
        Test::Ints(range) => {
            mark_range(range, cells, out);
            false
        }
        Test::IntSet { values, blank } => {
            mark_set(values, cells, out);
            *blank
        }
        Test::Floats(range) => {
            let Range { low, high, inside } = *range;
            let span = high.wrapping_sub(low);
            if low <= high {
                mark(cells.words(), out, |word| {
                    (float_key(word).wrapping_sub(low) <= span) == inside
                });
            } else if !inside {
                out.fill(u64::MAX);
            }
            false
        }
        Test::FloatSet { values, blank } => {
            mark_found(cells, values, true, out);
            *blank
        }
        Test::Text { test, blank } => {
            match hits {
                Some(hits) => mark_bits(cells, cells.record().low, hits, out),
                None => mark_texts(test, cells, selected, out, scratch)?,
            }
            *blank
        }
    };
    let rows = cells.rows();
    for (word, (out, nulls)) in out.iter_mut().zip(cells.nulls()).enumerate() {
        if blanks {
            *out |= nulls;
        } else {
            *out &= !nulls;
        }
        *out &= tail(rows, word);
    }
    Ok(())
}

/// Sets the bits of the words something holds for.
fn mark(words: &[u64], out: &mut [u64], holds: impl Fn(u64) -> bool) {
    for (words, slot) in words.chunks(64).zip(out) {
        let mut bits = 0_u64;
        for (bit, word) in (0_u32..).zip(words) {
            bits |= u64::from(holds(*word)).wrapping_shl(bit);
        }
        *slot = bits;
    }
}

/// The values of a block from one to another as the numbers that are packed.
fn packed_range(block: &Block, low: i64, high: i64) -> Option<(u64, u64)> {
    let factor = block.factor();
    let from = low.max(block.low.cast_signed());
    if from > high {
        return None;
    }
    let distance = |value: i64| value.cast_unsigned().wrapping_sub(block.low);
    let first = distance(from).div_ceil(factor.max(1));
    let last = distance(high).checked_div(factor).unwrap_or(0);
    (first <= last).then_some((first, last))
}

fn mark_range(range: &Range<i64>, cells: &Cells, out: &mut [u64]) {
    let inside = range.inside;
    match packed_range(cells.record(), range.low, range.high) {
        None => {
            if !inside {
                out.fill(u64::MAX);
            }
        }
        Some(range) => mark_packed(cells, range, inside, out),
    }
}

/// Sets the bits of the cells whose numbers lie in a range, or do not.
fn mark_packed(cells: &Cells, (first, last): (u64, u64), inside: bool, out: &mut [u64]) {
    let span = last.wrapping_sub(first);
    mark(cells.words(), out, |word| {
        (word.wrapping_sub(first) <= span) == inside
    });
}

fn mark_set(values: &[i64], cells: &Cells, out: &mut [u64]) {
    let block = cells.record();
    let (low, high) = (block.low.cast_signed(), block.high.cast_signed());
    let from = values.partition_point(|value| *value < low);
    let to = values.partition_point(|value| *value <= high);
    let factor = block.factor();
    let packed: Vec<u64> = values
        .get(from..to.max(from))
        .unwrap_or(&[])
        .iter()
        .map(|value| value.cast_unsigned().wrapping_sub(block.low))
        .filter(|distance| distance.checked_rem(factor) == Some(0))
        .filter_map(|distance| distance.checked_div(factor))
        .collect();
    if let [only] = packed.as_slice() {
        mark_packed(cells, (*only, *only), true, out);
        return;
    }
    // Which numbers are wanted, when a bit for each takes less than the block.
    let span = packed.last().copied().unwrap_or(0);
    let span = usize::try_from(span).unwrap_or(usize::MAX);
    if span >> 6 < cells.rows() {
        let mut wanted = alloc::vec![0_u64; (span >> 6).saturating_add(1)];
        for number in &packed {
            set(&mut wanted, usize::try_from(*number).unwrap_or(usize::MAX));
        }
        mark_bits(cells, 0, &wanted, out);
    } else {
        mark_found(cells, &packed, false, out);
    }
}

/// Sets the bits of the cells whose words, or the order keys of which, are
/// among some numbers in order.
fn mark_found(cells: &Cells, numbers: &[u64], floats: bool, out: &mut [u64]) {
    mark(cells.words(), out, |word| {
        let number = if floats { float_key(word) } else { word };
        numbers.binary_search(&number).is_ok()
    });
}

/// Sets the bits of the cells whose words, with a number added, are the places
/// of set bits.
fn mark_bits(cells: &Cells, offset: u64, bits: &[u64], out: &mut [u64]) {
    mark(cells.words(), out, |word| {
        get(
            bits,
            usize::try_from(offset.wrapping_add(word)).unwrap_or(usize::MAX),
        )
    });
}

fn mark_texts(
    test: &TextTest,
    cells: &Cells,
    selected: Option<&[u64]>,
    out: &mut [u64],
    scratch: &mut Vec<u8>,
) -> Result<(), QueryError> {
    cells.each(selected, &mut |cell, blank| {
        if !blank && test.matches(fold(cells.text(cell), scratch)?) {
            set(out, cell);
        }
        Ok(())
    })
}
