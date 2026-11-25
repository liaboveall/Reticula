//! The values of a column, for choosing among them.
//!
//! Values are counted over the rows that the conditions on all other columns
//! hold for. Of more values than are asked for, the most frequent are given,
//! in the order of the column. Texts are the same regardless of case and are
//! spelled like the first of them: the first in the dictionary, or else the
//! first that was counted.
//!
//! There is a limit to how many different values are counted. Those that come
//! first in the file are, and the list then says that it is not complete.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use super::QueryError;
use super::arrange::{Index, keep_first, sort_by};
use super::cells::{Cells, Part, each_word};
use super::context::{Context, Step};
use super::filter::Filtering;
use super::fold::{contains, fold};
use super::memory::Large;
use super::ranks::{Ranking, Ranks};
use super::selection::Selection;
use super::session::{Outcome, Work};
use super::sort::is_coded;
use super::space::{Scale, Space};
use super::spec::Value;
use crate::hash;
use crate::layout::Encoding;
use crate::text;
use crate::types::ColumnType;

/// Most values of a list.
const LONGEST: usize = 65_536;
/// Longest text that is listed, in bytes.
const LONGEST_TEXT: usize = 4_096;
/// Values that are looked through in one go.
const LOOKED: usize = 16_384;

/// A value that occurs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedValue {
    /// The value, as a condition takes it.
    pub value: Value,
    /// What its cells show.
    pub text: String,
    /// In how many rows.
    pub count: u64,
}

/// Values of a column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueList {
    /// The column.
    pub column: usize,
    /// The values, in the order of the column.
    pub values: Vec<ListedValue>,
    /// Number of blank cells.
    pub blanks: u64,
    /// Whether these are all the values that were searched for.
    pub complete: bool,
}

/// A text that was counted: where its folded form lies, and how it is spelled
/// when that is another one.
#[derive(Clone, Copy, Debug)]
struct Counted {
    at: usize,
    folded: u32,
    spelled: u32,
}

/// What the numbers of the values are.
#[derive(Debug)]
enum Numbers {
    /// There are no values.
    None,
    /// Their keys.
    Keys(Space),
    /// Those of the keys that were entered.
    Entered(Space),
    /// Their ranks.
    Ranks(Rc<Ranks>),
    /// Those of the texts that were entered.
    Texts,
}

/// The values that were counted, by number.
#[derive(Debug)]
struct Counting {
    numbers: Numbers,
    /// How often each occurs.
    counts: Large<u32>,
    index: Index,
    /// The keys that were entered.
    keys: Large<u64>,
    /// The texts that were entered, and their bytes.
    texts: Large<Counted>,
    bytes: Large<u8>,
    /// The first entry of the dictionary that has each rank.
    first: Large<u32>,
}

#[derive(Debug)]
enum Stage {
    Filter(Box<Filtering>),
    Begin,
    Rank(Box<Ranking>),
    /// The array that values, or codes, are counted in, and the index.
    Room(usize, Large<u32>, usize),
    /// The codes, if there are such, and the other texts after them: the group
    /// that is next, and the row in it.
    Scan(u32, usize),
    /// Codes that were counted become texts.
    Codes(usize),
    First(usize),
    Look(usize),
    Last,
}

/// Values being counted.
#[derive(Debug)]
pub(super) struct Listing {
    column: usize,
    kind: ColumnType,
    search: Vec<u8>,
    limit: usize,
    rows: Selection,
    counting: Counting,
    /// How often each code occurs, of a column that has texts besides.
    codes: Large<u32>,
    blanks: u64,
    complete: bool,
    /// Whether texts outside of the dictionary are counted by now.
    others: bool,
    /// The numbers of the most frequent values.
    found: Vec<u32>,
    stage: Stage,
    list: Vec<ListedValue>,
    scratch: Vec<u8>,
    shown: String,
}

impl Counting {
    fn key(&self, number: u32) -> u64 {
        match self.numbers {
            Numbers::Entered(_) => self.keys.get(number as usize).copied().unwrap_or(0),
            _ => u64::from(number),
        }
    }

    fn folded(&self, number: usize) -> &[u8] {
        let Some(entry) = self.texts.get(number) else {
            return &[];
        };
        let end = entry.at.saturating_add(entry.folded as usize);
        self.bytes.get(entry.at..end).unwrap_or(&[])
    }

    fn spelled(&self, number: usize) -> &[u8] {
        match self.texts.get(number) {
            Some(entry) if entry.spelled > 0 => {
                let from = entry.at.saturating_add(entry.folded as usize);
                let end = from.saturating_add(entry.spelled as usize);
                self.bytes.get(from..end).unwrap_or(&[])
            }
            _ => self.folded(number),
        }
    }

    /// How two values compare.
    fn compare(&self, a: u32, b: u32) -> Ordering {
        match self.numbers {
            Numbers::Texts => self.folded(a as usize).cmp(self.folded(b as usize)),
            _ => self.key(a).cmp(&self.key(b)),
        }
    }

    /// Which of two values is listed rather than the other.
    fn prefer(&self, a: u32, b: u32) -> Ordering {
        let count = |number: u32| self.counts.get(number as usize).copied().unwrap_or(0);
        count(b).cmp(&count(a)).then_with(|| self.compare(a, b))
    }

    /// Writes what a value shows. Gives its number in the column otherwise.
    fn show(&self, cx: &Context<'_>, column: usize, number: u32, shown: &mut String) {
        shown.clear();
        match &self.numbers {
            Numbers::None => {}
            Numbers::Keys(space) | Numbers::Entered(space) => {
                let kind = cx.reader.columns().get(column).map(|of| of.kind);
                let cell = space.cell(self.key(number));
                text::write_value(kind.unwrap_or(ColumnType::Text), cell, shown);
            }
            Numbers::Ranks(_) => {
                let code = self.first.get(number as usize).copied();
                if let Some(dictionary) = cx.reader.dictionary(column) {
                    shown.push_str(dictionary.text(code.unwrap_or(u32::MAX) as usize));
                }
            }
            Numbers::Texts => {
                let text = core::str::from_utf8(self.spelled(number as usize));
                shown.push_str(text.unwrap_or(""));
            }
        }
    }

    /// Counts a key. Gives whether there was room for it.
    fn add_key(&mut self, key: u64, most: usize) -> Result<bool, QueryError> {
        let keys = &self.keys;
        let hash = reticula_core::rng::mix64(key);
        match self.index.find(hash, &mut |at| keys.get(at) == Some(&key)) {
            Ok(at) => bump(&mut self.counts, at, 1),
            Err(Some(slot)) if keys.len() < most => {
                self.index.enter(slot, keys.len());
                self.keys.push(key)?;
                self.counts.push(1)?;
            }
            Err(_) => return Ok(false),
        }
        Ok(true)
    }

    /// Counts a text. Gives whether there was room for it.
    fn add_text(
        &mut self,
        folded: &[u8],
        spelled: &[u8],
        count: u32,
        (most, most_bytes): (usize, usize),
    ) -> Result<bool, QueryError> {
        let hash = hash::of_text(folded);
        let found = self.index.find(hash, &mut |at| self.folded(at) == folded);
        let slot = match found {
            Ok(at) => {
                bump(&mut self.counts, at, count);
                return Ok(true);
            }
            Err(slot) => slot,
        };
        let Some(slot) = slot else {
            return Ok(false);
        };
        let spelled = if spelled == folded { &[] } else { spelled };
        let size = folded.len().saturating_add(spelled.len());
        if self.texts.len() >= most
            || folded.len().max(spelled.len()) > LONGEST_TEXT
            || self.bytes.len().saturating_add(size) > most_bytes
        {
            return Ok(false);
        }
        let length = |text: &[u8]| u32::try_from(text.len()).unwrap_or(u32::MAX);
        let entry = Counted {
            at: self.bytes.len(),
            folded: length(folded),
            spelled: length(spelled),
        };
        self.index.enter(slot, self.texts.len());
        self.bytes.extend_from_slice(folded)?;
        self.bytes.extend_from_slice(spelled)?;
        self.texts.push(entry)?;
        self.counts.push(count)?;
        Ok(true)
    }
}

impl Listing {
    /// Over some rows, or over those that a filter that is yet to be done keeps.
    pub(super) fn new(
        cx: &Context<'_>,
        column: usize,
        search: &str,
        limit: usize,
        rows: Selection,
        filter: Option<Filtering>,
    ) -> Self {
        let of = cx.reader.columns().get(column);
        Self {
            column,
            kind: of.map_or(ColumnType::Text, |of| of.kind),
            search: search.to_lowercase().into_bytes(),
            limit: limit.min(LONGEST),
            rows,
            counting: Counting {
                numbers: Numbers::None,
                counts: cx.empty(),
                index: Index::new(cx.empty()),
                keys: cx.empty(),
                texts: cx.empty(),
                bytes: cx.empty(),
                first: cx.empty(),
            },
            codes: cx.empty(),
            blanks: 0,
            complete: true,
            others: false,
            found: Vec::new(),
            stage: filter.map_or(Stage::Begin, |filter| Stage::Filter(Box::new(filter))),
            list: Vec::new(),
            scratch: Vec::new(),
            shown: String::new(),
        }
    }
}

impl Work for Listing {
    fn finish(&mut self) -> Outcome {
        Outcome::Values(ValueList {
            column: self.column,
            values: core::mem::take(&mut self.list),
            blanks: self.blanks,
            complete: self.complete,
        })
    }

    fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        match &mut self.stage {
            Stage::Filter(filter) => {
                let step = filter.step(cx)?;
                if step.done {
                    self.rows = filter.finish();
                    self.stage = Stage::Begin;
                }
                Ok(step.part())
            }
            Stage::Begin => self.begin(cx),
            Stage::Rank(ranking) => {
                let step = ranking.step(cx)?;
                if step.done {
                    let ranks = Rc::new(ranking.finish());
                    cx.caches.keep_ranks(self.column, &ranks);
                    self.stage = self.count_ranks(cx, ranks)?;
                }
                Ok(step.part())
            }
            Stage::Room(len, slots, slot_count) => {
                let most = cx.limits.slice.saturating_mul(2);
                let array = if matches!(self.counting.numbers, Numbers::Texts) {
                    &mut self.codes
                } else {
                    &mut self.counting.counts
                };
                if array.fill(*len, 0, most)? && slots.fill(*slot_count, 0, most)? {
                    self.counting.index = Index::new(slots.take());
                    self.stage = Stage::Scan(0, 0);
                }
                Ok(Step::more(most))
            }
            Stage::Scan(group, row) => {
                let (group, row) = (*group, *row);
                if group >= cx.reader.directory().groups() {
                    self.stage = match &self.counting.numbers {
                        Numbers::Ranks(_) => Stage::First(0),
                        Numbers::Texts if !self.others => Stage::Codes(0),
                        _ => Stage::Look(0),
                    };
                    return Ok(Step::more(1));
                }
                let (work, next) = self.scan(cx, group, row)?;
                self.stage = if next == 0 {
                    Stage::Scan(group.saturating_add(1), 0)
                } else {
                    Stage::Scan(group, next)
                };
                Ok(Step::more(work))
            }
            Stage::Codes(from) => {
                let from = *from;
                self.add_codes(cx, from)
            }
            Stage::First(from) => {
                let from = *from;
                self.find_first(cx, from)
            }
            Stage::Look(from) => {
                let from = *from;
                self.look(cx, from)
            }
            Stage::Last => {
                if !self.has_texts(cx)? {
                    return Ok(Step::more(65_536));
                }
                self.write(cx);
                Ok(Step::done(LOOKED))
            }
        }
    }
}

impl Listing {
    fn begin(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let directory = cx.reader.directory();
        let entries = cx.place(self.column).map(|place| place.entries as usize);
        let (mut len, mut indexed) = (0, true);
        self.counting.numbers = if self.kind != ColumnType::Text {
            let rows = &self.rows;
            let space = Space::of(directory, self.column, &|group| {
                rows.count_in(directory, group) > 0
            });
            match space {
                None => {
                    indexed = false;
                    Numbers::None
                }
                Some(space) if space.span() < cx.limits.direct && space.is_integral() => {
                    indexed = false;
                    len = usize::try_from(space.span())
                        .unwrap_or(usize::MAX)
                        .saturating_add(1);
                    Numbers::Keys(space)
                }
                Some(space) => Numbers::Entered(space),
            }
        } else if entries.is_some() && is_coded(directory, self.column) {
            self.stage = match cx.caches.ranks(self.column) {
                Some(ranks) => self.count_ranks(cx, ranks)?,
                None => Stage::Rank(Box::new(Ranking::new(cx, self.column, None)?)),
            };
            return Ok(Step::more(1));
        } else {
            len = entries.unwrap_or(0);
            Numbers::Texts
        };
        self.stage = self.make_room(cx, len, indexed)?;
        Ok(Step::more(1))
    }

    /// Room to count so many numbers, and an index of the values if there is to be one.
    fn make_room(
        &mut self,
        cx: &mut Context<'_>,
        len: usize,
        indexed: bool,
    ) -> Result<Stage, QueryError> {
        let array = cx.large(len)?;
        if matches!(self.counting.numbers, Numbers::Texts) {
            let directory = cx.reader.directory();
            let blocks = (0..directory.groups())
                .filter_map(|group| directory.block(self.column, group))
                .filter(|block| block.encoding == Encoding::Text);
            let (texts, bytes) = blocks.fold((0_u64, 0_u64), |(texts, bytes), block| {
                let more = directory.group_rows.saturating_sub(block.blanks);
                let len = u64::from(block.len);
                (texts.saturating_add(more.into()), bytes.saturating_add(len))
            });
            // As many as are entered, if they are about as long as the others.
            let each = bytes.checked_div(texts).unwrap_or(0);
            let bytes = bytes.min(each.saturating_mul(cx.limits.distinct as u64));
            self.counting
                .bytes
                .expect(bytes.min(cx.limits.listed as u64));
            self.codes = array;
        } else {
            self.counting.counts = array;
        }
        let values = cx.limits.distinct.min(self.rows.len() as usize);
        let slots = if indexed { Index::slots(values) } else { 0 };
        Ok(Stage::Room(len, cx.large(slots)?, slots))
    }

    fn count_ranks(&mut self, cx: &mut Context<'_>, ranks: Rc<Ranks>) -> Result<Stage, QueryError> {
        let classes = ranks.classes as usize;
        self.counting.numbers = Numbers::Ranks(ranks);
        self.make_room(cx, classes, false)
    }

    /// Counts the cells of a group from a row on. Gives how many there are, and
    /// the row to go on from, or zero at the end of the group.
    fn scan(
        &mut self,
        cx: &mut Context<'_>,
        group: u32,
        row: usize,
    ) -> Result<(usize, usize), QueryError> {
        let directory = cx.reader.directory();
        let Some(record) = directory.block(self.column, group).copied() else {
            return Ok((1, 0));
        };
        let picked = self.rows.count_in(directory, group);
        let outside = record.encoding == Encoding::Text;
        // Entries of the dictionary come first.
        if picked == 0 || outside != self.others {
            return Ok((1, 0));
        }
        if record.encoding == Encoding::Blank {
            self.blanks = self.blanks.saturating_add(picked as u64);
            return Ok((1, 0));
        }
        let selected = self.rows.of_group(directory, group);
        let rows = directory.rows_in(group) as usize;
        let mut part = Part::texts(row);
        if outside {
            part.most = cx.limits.texts_at_once;
        }
        let end = cx.load(self.column, group, selected, part)?;
        let cells = &*cx.cells;
        self.blanks = self.blanks.saturating_add(cells.blanks(selected));
        let limits = (cx.limits.distinct, cx.limits.listed);
        let counting = &mut self.counting;
        let mut room = true;
        match &counting.numbers {
            Numbers::None => {}
            Numbers::Keys(space) => {
                if let Scale::Integral { offset, times } = space.scale(&record) {
                    let by = (offset, times, None);
                    count_words(cells, selected, by, &mut counting.counts);
                }
            }
            Numbers::Ranks(ranks) => {
                let by = (record.low, 1, Some(ranks.of_entries()));
                count_words(cells, selected, by, &mut counting.counts);
            }
            Numbers::Texts if !outside => {
                count_words(cells, selected, (record.low, 1, None), &mut self.codes);
            }
            Numbers::Entered(space) => {
                let scale = space.scale(&record);
                let words = cells.words();
                cells.each(selected, &mut |cell, blank| {
                    if !blank {
                        let key = scale.key(words.get(cell).copied().unwrap_or(0));
                        room &= counting.add_key(key, limits.0)?;
                    }
                    Ok(())
                })?;
            }
            Numbers::Texts => {
                let scratch = &mut self.scratch;
                cells.each(selected, &mut |cell, blank| {
                    if !blank {
                        let spelled = cells.text(cell);
                        let folded = fold(spelled, scratch)?;
                        room &= counting.add_text(folded, spelled, 1, limits)?;
                    }
                    Ok(())
                })?;
            }
        }
        self.complete &= room;
        let next = if end < rows { end } else { 0 };
        Ok((cells.rows().min(end.saturating_sub(row)), next))
    }

    /// Entries of the dictionary are counted like the other texts, of which
    /// they are the first.
    fn add_codes(&mut self, cx: &mut Context<'_>, from: usize) -> Result<Step, QueryError> {
        if !cx.dictionary(self.column)? {
            return Ok(Step::more(65_536));
        }
        let stop = from.saturating_add(cx.limits.entries).min(self.codes.len());
        let mut to = from;
        let mut bytes = 0_usize;
        while to < stop && bytes < cx.limits.part {
            let count = self.codes.get(to).copied().unwrap_or(0);
            if let Some(dictionary) = cx.reader.dictionary(self.column)
                && count > 0
            {
                let limits = (cx.limits.distinct, cx.limits.listed);
                let spelled = dictionary.bytes_of(to);
                bytes = bytes.saturating_add(spelled.len());
                let folded = fold(spelled, &mut self.scratch)?;
                self.complete &= self.counting.add_text(folded, spelled, count, limits)?;
            }
            to = to.saturating_add(1);
        }
        if to < self.codes.len() {
            self.stage = Stage::Codes(to);
        } else {
            self.others = true;
            self.stage = Stage::Scan(0, 0);
        }
        Ok(Step::more(to.saturating_sub(from)))
    }

    fn find_first(&mut self, cx: &mut Context<'_>, from: usize) -> Result<Step, QueryError> {
        let counting = &mut self.counting;
        self.stage = Stage::Look(0);
        let Numbers::Ranks(ranks) = &counting.numbers else {
            return Ok(Step::more(1));
        };
        let classes = ranks.classes as usize;
        if counting.first.len() < classes {
            if counting.first.is_empty() {
                counting.first = cx.large(classes)?;
            }
            counting.first.fill(classes, u32::MAX, cx.limits.slice)?;
            self.stage = Stage::First(0);
            return Ok(Step::more(cx.limits.slice));
        }
        let of = ranks.of_entries();
        let to = from.saturating_add(cx.limits.slice).min(of.len());
        for (code, rank) in (from..).zip(of.get(from..to).unwrap_or(&[])) {
            if let Some(first) = counting.first.get_mut(*rank as usize) {
                *first = (*first).min(u32::try_from(code).unwrap_or(u32::MAX));
            }
        }
        if to < of.len() {
            self.stage = Stage::First(to);
        }
        Ok(Step::more(to.saturating_sub(from)))
    }

    /// Loads the next piece of the dictionary, if it is asked for texts. Gives
    /// whether it is here.
    fn has_texts(&self, cx: &mut Context<'_>) -> Result<bool, QueryError> {
        match &self.counting.numbers {
            Numbers::Ranks(_) => cx.dictionary(self.column),
            _ => Ok(true),
        }
    }

    /// Looks for the values that contain what is searched for, and that occur
    /// most often.
    fn look(&mut self, cx: &mut Context<'_>, from: usize) -> Result<Step, QueryError> {
        if !self.search.is_empty() && !self.has_texts(cx)? {
            return Ok(Step::more(65_536));
        }
        let counting = &self.counting;
        let all = counting.counts.len();
        // Those that do not occur are passed quickly.
        let stop = from.saturating_add(cx.limits.slice).min(all);
        let mut left = LOOKED;
        let mut at = from;
        while at < stop && left > 0 {
            let count = counting.counts.get(at).copied().unwrap_or(0);
            let number = u32::try_from(at).unwrap_or(u32::MAX);
            at = at.saturating_add(1);
            if count == 0 {
                continue;
            }
            left = left.saturating_sub(1);
            if !self.search.is_empty() {
                counting.show(cx, self.column, number, &mut self.shown);
                // Long texts count for as many as are handled in the same time.
                left = left.saturating_sub(self.shown.len() >> 8);
                let folded = fold(self.shown.as_bytes(), &mut self.scratch)?;
                if !contains(folded, &self.search) {
                    continue;
                }
            }
            let mut prefer = |a, b| counting.prefer(a, b);
            if keep_first(&mut self.found, self.limit, number, &mut prefer) {
                self.complete = false;
            }
        }
        self.stage = if at < all {
            Stage::Look(at)
        } else {
            Stage::Last
        };
        Ok(Step::more(LOOKED))
    }

    /// Writes the list of the values that were found.
    fn write(&mut self, cx: &Context<'_>) {
        let counting = &self.counting;
        sort_by(&mut self.found, &mut Vec::new(), &mut |a, b| {
            counting.compare(a, b)
        });
        for number in &self.found {
            counting.show(cx, self.column, *number, &mut self.shown);
            if self.shown.len() > LONGEST_TEXT {
                self.complete = false;
                continue;
            }
            let count = counting.counts.get(*number as usize).copied();
            let text = self.shown.clone();
            self.list.push(ListedValue {
                value: match &counting.numbers {
                    Numbers::Keys(space) | Numbers::Entered(space) => {
                        space.value(counting.key(*number))
                    }
                    _ => Value::Text(text.clone()),
                },
                text,
                count: u64::from(count.unwrap_or(0)),
            });
        }
    }
}

/// Counts the numbers of the selected cells: what their words are times one
/// number and plus another, or what is at that place in a list.
#[inline(never)]
fn count_words(
    cells: &Cells,
    selected: Option<&[u64]>,
    (offset, times, through): (u64, u64, Option<&[u32]>),
    counts: &mut [u32],
) {
    let number = |word: u64| {
        usize::try_from(offset.wrapping_add(word.wrapping_mul(times))).unwrap_or(usize::MAX)
    };
    for (at, words) in cells.words().chunks(64).enumerate() {
        let mask = cells.values(selected, at);
        match through {
            None => each_word(mask, words, |word| bump(counts, number(word), 1)),
            Some(through) => each_word(mask, words, |word| {
                if let Some(number) = through.get(number(word)) {
                    bump(counts, *number as usize, 1);
                }
            }),
        }
    }
}

fn bump(counts: &mut [u32], at: usize, by: u32) {
    if let Some(count) = counts.get_mut(at) {
        *count = count.saturating_add(by);
    }
}
