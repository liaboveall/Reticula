//! Putting rows in order.
//!
//! Columns are taken from the least important one to the most important one,
//! and rows are sorted by each, which keeps the order there was among equal
//! rows. The values of a column become unsigned integers of as few bits as
//! they need ([`Space`], [`Ranks`]); those of more than 32 bits are made and
//! sorted by in two halves, so that keys never take more than four bytes a
//! row. Blank cells are moved to the end before, whatever the direction.
//!
//! Only the selected rows are sorted: they are numbered from 0, and become
//! rows of the file again at the end.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;

use super::QueryError;
use super::cells::Part;
use super::context::{Context, Step};
use super::memory::Large;
use super::radix::{Counts, Radix};
use super::ranks::{Ranking, Ranks};
use super::selection::{Selection, get, place, set};
use super::space::{Scale, Space};
use super::spec::SortKey;
use crate::bits;
use crate::layout::{Block, Directory, Encoding};
use crate::types::ColumnType;

/// Bits of a key that are sorted by together.
const HALF: u32 = 32;

/// How the values of a column become keys.
#[derive(Clone, Debug)]
enum Keys {
    Space(Space),
    Ranks(Rc<Ranks>),
    /// There are only blank cells.
    None,
}

/// The column that rows are being sorted by.
#[derive(Debug)]
struct Working {
    key: SortKey,
    keys: Keys,
    span: u64,
    /// The bits that are sorted by now, the first one and how many, and those after.
    half: (u32, u32),
    next: Option<(u32, u32)>,
    blanks: bool,
    /// Cells that are not blank.
    values: usize,
    counts: Counts,
    /// Whether the keys so far were in order.
    ordered: bool,
    last: u32,
}

#[derive(Debug)]
enum Stage {
    Plan,
    Rank(SortKey, Box<Ranking>),
    Room,
    Fill,
    Keys(u32, usize),
    Partition(usize, usize, usize),
    Sort(Radix),
    Rows(u32),
    Map(usize),
}

/// The order that comes out.
#[derive(Debug)]
pub(super) enum Sorted {
    /// That of the file, of all rows.
    Same,
    Rows(Large<u32>),
}

/// Rows being sorted.
#[derive(Debug)]
pub(super) struct Sorting {
    rows: Selection,
    /// The columns that are left, the least important one last.
    left: Vec<SortKey>,
    working: Option<Working>,
    order: Large<u32>,
    other: Large<u32>,
    keys: Large<u32>,
    blanks: Large<u64>,
    /// Whether the rows are still in the order of the file.
    untouched: bool,
    stage: Stage,
    nulls: Vec<u64>,
    present: Vec<u32>,
}

impl Sorting {
    pub(super) fn new(cx: &Context<'_>, rows: Selection, keys: &[SortKey]) -> Self {
        Self {
            rows,
            left: keys.to_vec(),
            working: None,
            order: cx.empty(),
            other: cx.empty(),
            keys: cx.empty(),
            blanks: cx.empty(),
            untouched: true,
            stage: Stage::Plan,
            nulls: Vec::new(),
            present: Vec::new(),
        }
    }

    /// Cells this works on in all, about.
    pub(super) fn size(rows: u32, keys: usize) -> u64 {
        u64::from(rows)
            .saturating_mul(5)
            .saturating_mul(u64::try_from(keys).unwrap_or(1))
    }

    fn len(&self) -> usize {
        self.rows.len() as usize
    }

    /// The order, when the last step is done.
    pub(super) fn finish(&mut self) -> Sorted {
        if self.untouched && self.rows.bits().is_none() {
            Sorted::Same
        } else {
            Sorted::Rows(self.order.take())
        }
    }

    pub(super) fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let slice = cx.limits.slice;
        match &mut self.stage {
            Stage::Plan => self.plan(cx),
            Stage::Rank(key, ranking) => {
                let key = *key;
                let step = ranking.step(cx)?;
                if step.done {
                    let ranks = Rc::new(ranking.finish());
                    if is_coded(cx.reader.directory(), key.column) {
                        cx.caches.keep_ranks(key.column, &ranks);
                    }
                    self.begin(cx, key, Keys::Ranks(ranks));
                }
                Ok(step.part())
            }
            Stage::Room => self.make_room(cx),
            Stage::Fill => self.fill(slice),
            Stage::Keys(group, at) => {
                let (group, at) = (*group, *at);
                self.make_keys(cx, group, at)
            }
            Stage::Partition(at, front, back) => {
                let from = (*at, *front, *back);
                Ok(self.partition(from, slice))
            }
            Stage::Sort(radix) => {
                let (work, done) = radix.step(&self.keys, &mut self.order, &mut self.other, slice);
                if work > 0 {
                    self.untouched = false;
                }
                if done {
                    let moved = radix.in_other();
                    self.after_half(moved);
                }
                Ok(Step::more(work))
            }
            Stage::Rows(group) => {
                let group = *group;
                self.list_rows(cx, group)
            }
            Stage::Map(at) => {
                let from = *at;
                let to = from.saturating_add(slice).min(self.order.len());
                for slot in self.order.get_mut(from..to).unwrap_or(&mut []) {
                    *slot = self.other.get(*slot as usize).copied().unwrap_or(0);
                }
                *at = to;
                let work = to.saturating_sub(from);
                Ok(if to >= self.order.len() {
                    Step::done(work)
                } else {
                    Step::more(work)
                })
            }
        }
    }

    fn plan(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let Some(key) = self.left.pop() else {
            return self.end(cx);
        };
        if self.rows.len() < 2 {
            self.left.clear();
            return Ok(Step::more(1));
        }
        let directory = cx.reader.directory();
        let kind = directory
            .columns
            .get(key.column)
            .map_or(ColumnType::Text, |column| column.kind);
        if kind != ColumnType::Text {
            let rows = &self.rows;
            let space = Space::of(directory, key.column, &|group| {
                rows.count_in(directory, group) > 0
            });
            self.begin(cx, key, space.map_or(Keys::None, Keys::Space));
            return Ok(Step::more(1));
        }
        let pure = is_coded(directory, key.column);
        if pure {
            if cx.place(key.column).is_none() {
                self.begin(cx, key, Keys::None);
                return Ok(Step::more(1));
            }
            if let Some(ranks) = cx.caches.ranks(key.column) {
                self.begin(cx, key, Keys::Ranks(ranks));
                return Ok(Step::more(1));
            }
        }
        // Ranking takes room as well.
        self.other = cx.empty();
        self.keys = cx.empty();
        let rows = (!pure).then(|| self.rows.clone());
        let ranking = Ranking::new(cx, key.column, rows)?;
        self.stage = Stage::Rank(key, Box::new(ranking));
        Ok(Step::more(1))
    }

    /// Begins to sort by a column.
    fn begin(&mut self, cx: &Context<'_>, key: SortKey, keys: Keys) {
        let directory = cx.reader.directory();
        let blanks = (0..directory.groups()).any(|group| {
            directory
                .block(key.column, group)
                .is_none_or(|block| block.blanks > 0)
                && self.rows.count_in(directory, group) > 0
        });
        let span = match &keys {
            Keys::Space(space) => space.span(),
            Keys::Ranks(ranks) => u64::from(ranks.classes.saturating_sub(1)),
            Keys::None => 0,
        };
        if matches!(keys, Keys::None) || (span == 0 && !blanks) {
            // All cells are equal.
            self.stage = Stage::Plan;
            return;
        }
        let width = bits::width_of(span);
        let (low, high) = if width <= HALF {
            (width, 0)
        } else {
            // As few digits as the whole has.
            let digits = width.div_ceil(11).div_ceil(2);
            let low = digits.saturating_mul(11).min(HALF);
            (low, width.saturating_sub(low))
        };
        self.working = Some(Working {
            key,
            keys,
            span,
            half: (0, low),
            next: (high > 0).then_some((low, high)),
            blanks,
            values: 0,
            counts: Counts::new(low),
            ordered: true,
            last: 0,
        });
        self.stage = Stage::Room;
    }

    fn make_room(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let len = self.len();
        if self.order.is_empty() {
            self.order = cx.large(len)?;
        }
        if self.keys.is_empty() {
            self.other = cx.large(len)?;
            self.keys = cx.large(len)?;
        }
        let blanks = self.working.as_ref().is_some_and(|working| working.blanks);
        self.blanks = cx.large(if blanks { len.div_ceil(64) } else { 0 })?;
        self.stage = Stage::Fill;
        Ok(Step::more(1))
    }

    fn fill(&mut self, slice: usize) -> Result<Step, QueryError> {
        let len = self.len();
        let words = match &self.working {
            Some(working) if working.blanks => len.div_ceil(64),
            _ => 0,
        };
        let most = slice.saturating_mul(2);
        let ready = self.order.fill_counting(len, most)?
            && self.other.fill(len, 0, most)?
            && self.keys.fill(len, 0, most)?
            && self.blanks.fill(words, 0, most)?;
        if ready {
            self.stage = Stage::Keys(0, 0);
        }
        Ok(Step::more(most))
    }

    fn make_keys(
        &mut self,
        cx: &mut Context<'_>,
        group: u32,
        at: usize,
    ) -> Result<Step, QueryError> {
        let Some(working) = &mut self.working else {
            self.stage = Stage::Plan;
            return Ok(Step::more(1));
        };
        let directory = cx.reader.directory();
        if group >= directory.groups() {
            return Ok(self.keys_made());
        }
        let next = group.saturating_add(1);
        let count = self.rows.count_in(directory, group);
        let to = at.saturating_add(count);
        let first = working.half.0 == 0;
        let selected = self.rows.of_group(directory, group);
        let out = self.keys.get_mut(at..to).unwrap_or(&mut []);
        let record = directory
            .block(working.key.column, group)
            .map_or(Block::BLANK, |block| *block);
        if count == 0 {
            self.stage = Stage::Keys(next, to);
            return Ok(Step::more(1));
        }
        if record.encoding == Encoding::Blank {
            if first {
                for place in at..to {
                    set(&mut self.blanks, place);
                }
            }
            self.stage = Stage::Keys(next, to);
            return Ok(Step::more(count >> 4));
        }
        // The ranks of texts are known already, and which of them are blank is
        // in the cells.
        let any = record.blanks > 0;
        if any || record.encoding != Encoding::Text {
            cx.load(working.key.column, group, selected, Part::ALL)?;
        }
        let cells = &*cx.cells;
        // Which of the selected cells are blank.
        let nulls = &mut self.nulls;
        nulls.clear();
        if any {
            if selected.is_none() || cells.is_compact() {
                nulls.extend_from_slice(cells.nulls());
            } else {
                nulls.resize(count.div_ceil(64), 0);
                let mut place = 0_usize;
                cells.each(selected, &mut |_, blank| {
                    if blank {
                        set(nulls, place);
                    }
                    place = place.saturating_add(1);
                    Ok(())
                })?;
            }
        }
        let (shift, width) = working.half;
        let mask = u32::try_from(bits::mask(width)).unwrap_or(u32::MAX);
        let span = working.span;
        let down = working.key.descending;
        let part = |key: u64| {
            let key = if down { span.wrapping_sub(key) } else { key };
            low_half(key.wrapping_shr(shift)) & mask
        };
        let contiguous = selected.is_none() || cells.is_compact();
        let picked = selected.filter(|_| !contiguous);
        match &working.keys {
            Keys::Space(space) => match space.scale(&record) {
                Scale::Integral { offset, times } => {
                    write_keys(cells.words(), picked, out, |word| {
                        part(offset.wrapping_add(word.wrapping_mul(times)))
                    });
                }
                scale @ Scale::Float { .. } => {
                    write_keys(cells.words(), picked, out, |word| part(scale.key(word)));
                }
            },
            Keys::Ranks(ranks) if record.encoding == Encoding::Packed => {
                let of = ranks.of_entries();
                let low = record.low;
                write_keys(cells.words(), picked, out, |word| {
                    let code = usize::try_from(low.wrapping_add(word)).unwrap_or(usize::MAX);
                    part(u64::from(of.get(code).copied().unwrap_or(0)))
                });
            }
            Keys::Ranks(ranks) => {
                // The texts of the group have their ranks one after another.
                let mut of = ranks.of_group(group).iter();
                for (place, slot) in out.iter_mut().enumerate() {
                    if !get(nulls, place) {
                        *slot = part(u64::from(of.next().copied().unwrap_or(0)));
                    }
                }
            }
            Keys::None => {}
        }
        if any {
            let present = &mut self.present;
            present.clear();
            for (chunk, nulls) in out.chunks(64).zip(nulls.iter()) {
                let mut left = !nulls;
                while left != 0 {
                    let bit = left.trailing_zeros() as usize;
                    left &= left.wrapping_sub(1);
                    let Some(key) = chunk.get(bit) else {
                        break;
                    };
                    present.push(*key);
                }
            }
            working.add(present);
            if first {
                or_bits(&mut self.blanks, at, nulls);
            }
        } else {
            working.add(out);
        }
        self.stage = Stage::Keys(next, to);
        Ok(Step::more(count))
    }

    /// All keys are made: what is to be done with them.
    fn keys_made(&mut self) -> Step {
        let len = self.len();
        let Some(working) = &mut self.working else {
            self.stage = Stage::Plan;
            return Step::more(1);
        };
        let counts = working.counts.take();
        let first = working.half.0 == 0;
        if first && working.values < len {
            working.counts = counts;
            self.stage = Stage::Partition(0, 0, working.values);
            return Step::more(1);
        }
        let nothing = first && working.next.is_none() && self.untouched && working.ordered;
        if nothing {
            self.working = None;
            self.stage = Stage::Plan;
        } else {
            self.stage = Stage::Sort(Radix::new(0, working.values, counts));
        }
        Step::more(1)
    }

    /// Moves the rows whose cells are blank to the end.
    fn partition(&mut self, (at, front, back): (usize, usize, usize), slice: usize) -> Step {
        let (mut front, mut back) = (front, back);
        let len = self.len();
        let to = at.saturating_add(slice).min(len);
        for item in self.order.get(at..to).unwrap_or(&[]) {
            let place = if get(&self.blanks, *item as usize) {
                let place = back;
                back = back.saturating_add(1);
                place
            } else {
                let place = front;
                front = front.saturating_add(1);
                place
            };
            if let Some(slot) = self.other.get_mut(place) {
                *slot = *item;
            }
        }
        self.stage = Stage::Partition(to, front, back);
        if to >= len {
            // They may have been there already.
            let trailing = self.untouched
                && self
                    .order
                    .iter()
                    .zip(self.other.iter())
                    .all(|(before, after)| before == after);
            core::mem::swap(&mut self.order, &mut self.other);
            if let Some(working) = &mut self.working {
                let counts = working.counts.take();
                let nothing = trailing && working.next.is_none() && working.ordered;
                if nothing {
                    self.working = None;
                    self.stage = Stage::Plan;
                } else {
                    self.untouched &= trailing;
                    self.stage = Stage::Sort(Radix::new(0, working.values, counts));
                }
            }
        }
        Step::more(to.saturating_sub(at))
    }

    /// Rows are sorted by one half of the keys.
    fn after_half(&mut self, in_other: bool) {
        let Some(working) = &mut self.working else {
            self.stage = Stage::Plan;
            return;
        };
        if in_other {
            // The blank ones were not moved along.
            let from = working.values;
            if let (Some(to), Some(tail)) = (self.other.get_mut(from..), self.order.get(from..)) {
                to.copy_from_slice(tail);
            }
            core::mem::swap(&mut self.order, &mut self.other);
        }
        if let Some((shift, width)) = working.next.take() {
            working.half = (shift, width);
            working.counts = Counts::new(width);
            self.stage = Stage::Keys(0, 0);
        } else {
            self.working = None;
            self.stage = Stage::Plan;
        }
    }

    /// Sorted by all columns: what is left is to make rows of the numbers.
    fn end(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        self.keys = cx.empty();
        self.blanks = cx.empty();
        if self.rows.bits().is_none() {
            self.other = cx.empty();
            return Ok(Step::done(1));
        }
        if self.other.is_empty() {
            self.other = cx.large(self.len())?;
        }
        self.other.clear();
        self.stage = Stage::Rows(0);
        Ok(Step::more(1))
    }

    fn list_rows(&mut self, cx: &mut Context<'_>, group: u32) -> Result<Step, QueryError> {
        let directory = cx.reader.directory();
        let mut work = 0_usize;
        let mut group = group;
        while work < cx.limits.slice && group < directory.groups() {
            work = work.saturating_add(64).saturating_add(append_rows(
                &self.rows,
                directory,
                group,
                &mut self.other,
            )?);
            group = group.saturating_add(1);
        }
        if group < directory.groups() {
            self.stage = Stage::Rows(group);
        } else if self.untouched {
            self.order = self.other.take();
            return Ok(Step::done(work));
        } else {
            self.stage = Stage::Map(0);
        }
        Ok(Step::more(work))
    }
}

impl Working {
    /// Counts the digits of keys that come next.
    fn add(&mut self, keys: &[u32]) {
        self.counts.add(keys);
        if self.half.0 == 0 {
            self.values = self.values.saturating_add(keys.len());
        }
        if self.ordered {
            let mut last = self.last;
            let mut ordered = true;
            for key in keys {
                ordered &= last <= *key;
                last = *key;
            }
            self.last = last;
            self.ordered = ordered;
        }
    }
}

/// Adds the selected rows of a group to a list. Gives how many there are.
pub(super) fn append_rows(
    rows: &Selection,
    directory: &Directory,
    group: u32,
    out: &mut Large<u32>,
) -> Result<usize, QueryError> {
    let Some(words) = rows.of_group(directory, group) else {
        return Ok(0);
    };
    let before = out.len();
    let mut first = group.saturating_mul(directory.group_rows);
    for word in words {
        let mut left = *word;
        while left != 0 {
            out.push(first.saturating_add(left.trailing_zeros()))?;
            left &= left.wrapping_sub(1);
        }
        first = first.saturating_add(64);
    }
    Ok(out.len().saturating_sub(before))
}

/// Writes the keys of cells: of all of them, or of those that are picked.
fn write_keys(words: &[u64], picked: Option<&[u64]>, out: &mut [u32], key: impl Fn(u64) -> u32) {
    let Some(picked) = picked else {
        for (slot, word) in out.iter_mut().zip(words) {
            *slot = key(*word);
        }
        return;
    };
    let mut slots = out.iter_mut();
    for (words, picked) in words.chunks(64).zip(picked) {
        let mut left = *picked;
        while left != 0 {
            let bit = left.trailing_zeros() as usize;
            left &= left.wrapping_sub(1);
            if let (Some(word), Some(slot)) = (words.get(bit), slots.next()) {
                *slot = key(*word);
            }
        }
    }
}

/// Sets bits from a place on.
fn or_bits(bits: &mut [u64], at: usize, more: &[u64]) {
    let (word, shift) = place(at);
    for (index, value) in (word..).zip(more) {
        if let Some(slot) = bits.get_mut(index) {
            *slot |= value.wrapping_shl(shift);
        }
        if shift > 0
            && let Some(slot) = bits.get_mut(index.saturating_add(1))
        {
            *slot |= value.wrapping_shr(u64::BITS.wrapping_sub(shift));
        }
    }
}

fn low_half(value: u64) -> u32 {
    u32::try_from(value & 0xffff_ffff).unwrap_or(0)
}

/// Whether no text of a column is outside of its dictionary.
pub(super) fn is_coded(directory: &Directory, column: usize) -> bool {
    (0..directory.groups()).all(|group| {
        directory
            .block(column, group)
            .is_none_or(|block| block.encoding != Encoding::Text)
    })
}
