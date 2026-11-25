//! The order of texts, as numbers.
//!
//! The entries of a column's dictionary, and the texts of selected rows that
//! are not in it, are put in order together, and each gets the number of
//! smaller different texts as its rank. Texts are compared in lower case, byte
//! by byte; equal ones get the same rank.
//!
//! First all of them are sorted by their first eight bytes. Those that are
//! still equal and go on are then sorted by the next eight bytes, and so on.
//! Entries of the dictionary are in memory, but the other texts have to be read
//! again for that. Once they are few enough, what is left of them is kept in
//! memory. When that takes long, equal texts are looked for, by their SHA-256,
//! and only one of each is sorted. There is a limit to how often texts are read
//! again, and a table that has too many different texts that begin alike is
//! refused rather than sorted for minutes.

use alloc::vec::Vec;

use sha2::{Digest, Sha256};

use super::QueryError;
use super::arrange::sort_by;
use super::cells::Part;
use super::context::{Context, Step};
use super::fold::{chunk, fold, goes_on};
use super::memory::Large;
use super::radix::{Counts, Radix};
use super::selection::{Selection, clear, get, next_set, place, set};
use crate::layout::Encoding;

/// Items whose keys are compared in one go.
const COMPARED: usize = 65_536;

/// The ranks of the texts of a column.
#[derive(Debug)]
pub(super) struct Ranks {
    /// The rank of every entry of the dictionary, then of every other text.
    items: Large<u32>,
    /// Number of ranks.
    pub(super) classes: u32,
    /// Number of entries.
    pub(super) entries: usize,
    /// The first of the other texts of each group, when there are such.
    bases: Vec<u32>,
}

impl Ranks {
    /// The ranks of the entries, and of the texts of the rows from the first
    /// one of a group on.
    pub(super) fn of_entries(&self) -> &[u32] {
        self.items.get(..self.entries).unwrap_or(&[])
    }

    pub(super) fn of_group(&self, group: u32) -> &[u32] {
        let base = self.bases.get(group as usize).copied().unwrap_or(u32::MAX);
        self.items.get(base as usize..).unwrap_or(&[])
    }

    pub(super) fn bytes(&self) -> usize {
        self.items.len().saturating_mul(4)
    }
}

/// The rest of some texts.
#[derive(Debug)]
struct Loaded {
    /// Which items they are of, and how many are before each 64 items.
    bits: Large<u64>,
    before: Large<u32>,
    ends: Large<u32>,
    bytes: Large<u8>,
    /// Where in the texts these begin.
    depth: usize,
}

impl Loaded {
    fn text(&self, item: u32) -> &[u8] {
        let (word, bit) = place(item as usize);
        let before = self.before.get(word).copied().unwrap_or(0);
        let within = self.bits.get(word).copied().unwrap_or(0) & bits_below(bit);
        let slot = (before as usize).saturating_add(within.count_ones() as usize);
        let end = |slot: usize| self.ends.get(slot).map(|end| *end as usize);
        let start = slot.checked_sub(1).and_then(end).unwrap_or(0);
        end(slot)
            .and_then(|end| self.bytes.get(start..end))
            .unwrap_or(&[])
    }

    fn chunk(&self, item: u32, depth: usize) -> u64 {
        chunk(self.text(item), depth.saturating_sub(self.depth))
    }
}

const fn bits_below(bit: u32) -> u64 {
    1_u64.wrapping_shl(bit).wrapping_sub(1)
}

/// Many items that begin alike, being sorted.
#[derive(Debug)]
struct Run {
    start: usize,
    end: usize,
    phase: Phase,
}

#[derive(Debug)]
enum Phase {
    Count(usize, Counts),
    Sort(Radix),
    Back(usize),
    Split(usize),
}

/// What is done with one text after another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
    /// Their first eight bytes become their keys.
    First,
    /// The next eight bytes of those that are open do.
    Next,
    /// What is left of those that are open is kept.
    Keep,
    /// The first or the second half of their digest becomes the key of those
    /// that are open.
    Digest(bool),
}

/// Which half of their digests texts are told apart by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Uniting {
    No,
    First,
    Second,
    Done,
}

/// Half as many bits of the SHA-256 of a text as nobody finds two texts with.
/// The key always goes on.
fn digest(text: &[u8], second: bool) -> u64 {
    let digest: [u8; 32] = Sha256::digest(text).into();
    let (parts, _) = digest.as_chunks::<8>();
    let part = parts.get(usize::from(second)).copied().unwrap_or_default();
    u64::from_be_bytes(part) | 1
}

#[derive(Debug)]
enum Stage {
    /// The entries, then the texts of the rows.
    Entries(Pass, usize),
    /// Of which group, from which row on, and which item that is.
    Rows(Pass, u32, usize, usize),
    /// Room for everything else.
    Fill,
    /// Those that are equal to others and go on: either what is left of them is kept, or the
    /// next eight bytes are read. After that they are sorted.
    Decide,
    KeepBits(usize),
    NextKept(usize),
    Runs(usize),
    Run(Run),
    /// The same for the last ones, as far as it takes: from where on, and the
    /// runs to be sorted first.
    Rest(usize, Vec<(usize, usize, usize)>),
    /// Of equal texts one stays, see [`Ranking::unite`].
    Unite(usize, usize),
    Number(usize, u32),
    Twins(usize, u32),
    Done(u32),
}

/// Ranks being worked out.
#[derive(Debug)]
pub(super) struct Ranking {
    column: usize,
    rows: Option<Selection>,
    entries: usize,
    bases: Vec<u32>,
    keys: Large<u64>,
    order: Large<u32>,
    other: Large<u32>,
    /// The places in the order where a text differs from the one before.
    starts: Large<u64>,
    /// The items that are equal to another one so far and go on.
    open: Large<u64>,
    open_count: usize,
    /// The same of the round that is going on.
    still_open: usize,
    depth: usize,
    /// Texts of the last pass, and their bytes.
    texts: u64,
    text_bytes: u64,
    kept: Option<Loaded>,
    /// How many may be open for it to be tried to keep them, and whether there
    /// was too little memory for it.
    keep_below: usize,
    short: bool,
    rounds: u32,
    uniting: Uniting,
    /// The places in the order where a text differed from the one before until
    /// equal ones were looked for.
    runs: Large<u64>,
    /// Items that are not in the order any more, and those they are equal to.
    twins: Large<(u32, u32)>,
    stage: Stage,
    scratch: Vec<u8>,
    merged: Vec<u32>,
}

impl Ranking {
    /// Of the entries of a dictionary, and of the texts of the rows that are
    /// not in it.
    pub(super) fn new(
        cx: &mut Context<'_>,
        column: usize,
        rows: Option<Selection>,
    ) -> Result<Self, QueryError> {
        let entries = cx.place(column).map_or(0, |place| place.entries as usize);
        let directory = cx.reader.directory();
        let mut texts = 0_usize;
        if let Some(rows) = &rows {
            for group in 0..directory.groups() {
                if let Some(block) = directory.block(column, group)
                    && block.encoding == Encoding::Text
                {
                    let values = directory.rows_in(group).saturating_sub(block.blanks);
                    texts =
                        texts.saturating_add(rows.count_in(directory, group).min(values as usize));
                }
            }
        }
        Ok(Self {
            column,
            rows,
            entries,
            bases: Vec::new(),
            keys: cx.large(entries.saturating_add(texts))?,
            order: cx.empty(),
            other: cx.empty(),
            starts: cx.empty(),
            open: cx.empty(),
            open_count: 0,
            still_open: 0,
            depth: 0,
            texts: 0,
            text_bytes: 0,
            kept: None,
            keep_below: usize::MAX,
            short: false,
            rounds: 0,
            uniting: Uniting::No,
            runs: cx.empty(),
            twins: cx.empty(),
            stage: Stage::Entries(Pass::First, 0),
            scratch: Vec::new(),
            merged: Vec::new(),
        })
    }

    /// The ranks, when the last step is done.
    pub(super) fn finish(&mut self) -> Ranks {
        Ranks {
            items: self.other.take(),
            classes: match self.stage {
                Stage::Done(classes) => classes,
                _ => 0,
            },
            entries: self.entries,
            bases: core::mem::take(&mut self.bases),
        }
    }

    pub(super) fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let slice = cx.limits.slice;
        match &mut self.stage {
            Stage::Entries(pass, from) => {
                let (pass, from) = (*pass, *from);
                self.pass_entries(cx, pass, from)
            }
            Stage::Rows(pass, group, row, item) => {
                let (pass, group, at) = (*pass, *group, (*row, *item));
                self.pass_rows(cx, pass, group, at)
            }
            Stage::Fill => self.fill(cx),
            Stage::Decide => {
                self.still_open = 0;
                self.stage = self.decide(cx)?;
                Ok(Step::more(1))
            }
            Stage::KeepBits(at) => {
                let at = *at;
                self.keep_bits(at, slice >> 2)
            }
            Stage::NextKept(from) => {
                let from = *from;
                Ok(self.next_kept(from, slice))
            }
            Stage::Runs(at) => {
                let at = *at;
                Ok(self.sort_runs(cx.limits.run, at, slice))
            }
            Stage::Run(_) => Ok(self.sort_run(slice)),
            Stage::Rest(..) => Ok(self.sort_rest(slice)),
            Stage::Number(at, rank) => {
                let (at, rank) = (*at, *rank);
                Ok(self.number(at, rank, slice))
            }
            Stage::Unite(at, out) => {
                let (at, out) = (*at, *out);
                self.unite(cx, at, out)
            }
            Stage::Twins(at, classes) => {
                let (from, classes) = (*at, *classes);
                let to = from.saturating_add(slice).min(self.twins.len());
                for (item, twin) in self.twins.get(from..to).unwrap_or(&[]) {
                    let rank = self.other.get(*twin as usize).copied().unwrap_or(0);
                    if let Some(slot) = self.other.get_mut(*item as usize) {
                        *slot = rank;
                    }
                }
                self.stage = if to < self.twins.len() {
                    Stage::Twins(to, classes)
                } else {
                    Stage::Done(classes)
                };
                Ok(Step::more(to.saturating_sub(from)))
            }
            Stage::Done(_) => Ok(Step::done(1)),
        }
    }

    /// Does with the next entries what is done with them.
    fn pass_entries(
        &mut self,
        cx: &mut Context<'_>,
        pass: Pass,
        from: usize,
    ) -> Result<Step, QueryError> {
        if self.entries > 0 && !cx.dictionary(self.column)? {
            return Ok(Step::more(COMPARED));
        }
        let stop = from.saturating_add(cx.limits.slice).min(self.entries);
        let mut at = from;
        let mut left = cx.limits.entries;
        let mut bytes = 0_usize;
        let mut room = true;
        let mut scratch = core::mem::take(&mut self.scratch);
        if let Some(dictionary) = cx.reader.dictionary(self.column) {
            while left > 0 && room && bytes < cx.limits.part {
                if pass != Pass::First {
                    at = next_set(&self.open, at, stop);
                }
                if at >= stop {
                    break;
                }
                let text = fold(dictionary.bytes_of(at), &mut scratch)?;
                bytes = bytes.saturating_add(text.len());
                room = self.take_text(pass, at, text, cx.limits.texts)?;
                at = at.saturating_add(1);
                left = left.saturating_sub(1);
            }
        }
        self.scratch = scratch;
        self.stage = if !room {
            self.cannot_keep()
        } else if at < self.entries {
            Stage::Entries(pass, at)
        } else {
            Stage::Rows(pass, 0, 0, 0)
        };
        Ok(Step::more(at.saturating_sub(from)))
    }

    /// Does with the texts of the rows of the next group that has some what is
    /// done with them.
    fn pass_rows(
        &mut self,
        cx: &mut Context<'_>,
        pass: Pass,
        group: u32,
        (row, item): (usize, usize),
    ) -> Result<Step, QueryError> {
        let directory = cx.reader.directory();
        let groups = directory.groups();
        let rows = self.rows.clone();
        let found = rows.as_ref().and_then(|rows| {
            (group..groups).find(|group| {
                directory
                    .block(self.column, *group)
                    .is_some_and(|block| block.encoding == Encoding::Text)
                    && (pass == Pass::First || self.is_open(*group))
                    && rows.count_in(directory, *group) > 0
            })
        });
        let next = found.map_or(groups, |found| found.saturating_add(1));
        let mut rest = None;
        let mut room = true;
        let mut work = 1;
        if let (Some(found), Some(rows)) = (found, &rows) {
            let part = Part::texts(row);
            let selected = rows.of_group(directory, found);
            let all = directory.rows_in(found) as usize;
            let end = cx.load(self.column, found, selected, part)?;
            self.note_bases(pass, next)?;
            let mut item = if row == 0 { self.base(found) } else { item };
            let mut scratch = core::mem::take(&mut self.scratch);
            let cells = &*cx.cells;
            work = cells.rows();
            let selected = rows.of_group(cx.reader.directory(), found);
            let limit = cx.limits.texts;
            cells.each(selected, &mut |cell, blank| {
                if !blank {
                    if room && (pass == Pass::First || get(&self.open, item)) {
                        let text = fold(cells.text(cell), &mut scratch)?;
                        room = self.take_text(pass, item, text, limit)?;
                    }
                    item = item.saturating_add(1);
                }
                Ok(())
            })?;
            self.scratch = scratch;
            rest = (end < all).then_some((found, end, item));
        } else {
            self.note_bases(pass, next)?;
        }
        self.stage = if !room {
            self.cannot_keep()
        } else if let Some((group, row, item)) = rest {
            Stage::Rows(pass, group, row, item)
        } else if next < groups {
            Stage::Rows(pass, next, 0, 0)
        } else {
            match pass {
                Pass::First => Stage::Fill,
                Pass::Next => self.begin_runs(),
                Pass::Keep => self.refine(cx.limits.run),
                Pass::Digest(_) => Stage::Runs(0),
            }
        };
        Ok(Step::more(work))
    }

    /// Notes the first items of the groups before one. Those without texts
    /// outside of the dictionary have none.
    fn note_bases(&mut self, pass: Pass, next: u32) -> Result<(), QueryError> {
        if pass == Pass::First {
            let base = u32::try_from(self.keys.len()).map_err(|_| QueryError::OutOfMemory)?;
            self.bases.resize(next as usize, base);
        }
        Ok(())
    }

    /// The first item of a group.
    fn base(&self, group: u32) -> usize {
        self.bases
            .get(group as usize)
            .map_or(self.keys.len(), |base| *base as usize)
    }

    /// Whether some of the texts of a group are open.
    fn is_open(&self, group: u32) -> bool {
        let to = self.base(group.saturating_add(1));
        next_set(&self.open, self.base(group), to) < to
    }

    /// Gives whether there was room for the text.
    fn take_text(
        &mut self,
        pass: Pass,
        item: usize,
        text: &[u8],
        limit: usize,
    ) -> Result<bool, QueryError> {
        self.texts = self.texts.saturating_add(1);
        self.text_bytes = self.text_bytes.saturating_add(text.len() as u64);
        let key = match (pass, &mut self.kept) {
            (Pass::First, _) => {
                self.keys.push(chunk(text, 0))?;
                return Ok(true);
            }
            (Pass::Next, _) => chunk(text, self.depth),
            (Pass::Digest(second), _) => digest(text, second),
            (Pass::Keep, Some(kept)) => {
                let rest = text.get(kept.depth..).unwrap_or(&[]);
                let end = kept.bytes.len().saturating_add(rest.len());
                if end > limit {
                    return Ok(false);
                }
                if kept.bytes.extend_from_slice(rest).is_err() {
                    self.short = true;
                    return Ok(false);
                }
                let end = u32::try_from(end).map_err(|_| QueryError::OutOfMemory)?;
                kept.ends.push(end)?;
                return Ok(true);
            }
            (Pass::Keep, None) => return Ok(false),
        };
        if let Some(slot) = self.keys.get_mut(item) {
            *slot = key;
        }
        Ok(true)
    }

    fn fill(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let items = self.keys.len();
        let words = items.div_ceil(64);
        if self.order.is_empty() && items > 0 {
            self.order = cx.large(items)?;
            self.other = cx.large(items)?;
            self.starts = cx.large(words)?;
            self.open = cx.large(words)?;
        }
        let most = cx.limits.slice.saturating_mul(2);
        let ready = self.order.fill_counting(items, most)?
            && self.other.fill(items, 0, most)?
            && self.starts.fill(words, 0, most)?
            && self.open.fill(words, 0, most)?;
        if ready {
            self.stage = if items == 0 {
                Stage::Number(0, 0)
            } else {
                // All of them are equal so far.
                self.depth = 8;
                self.run(0, items)
            };
        }
        Ok(Step::more(most))
    }

    fn key_at(&self, place: usize) -> u64 {
        self.order
            .get(place)
            .and_then(|item| self.keys.get(*item as usize))
            .copied()
            .unwrap_or(0)
    }

    /// Finds out which of the sorted items of a run, from one place to another,
    /// are equal to their neighbours and go on.
    fn split(&mut self, start: usize, from: usize, to: usize) {
        let mut last = from.checked_sub(1).filter(|last| *last >= start);
        let mut last_key = last.map_or(0, |last| self.key_at(last));
        for place in from..to {
            let item = self.order.get(place).copied().unwrap_or(0) as usize;
            let key = self.keys.get(item).copied().unwrap_or(0);
            clear(&mut self.open, item);
            match last {
                Some(before) if key == last_key => {
                    if goes_on(key) {
                        let before = self.order.get(before).copied().unwrap_or(0);
                        self.open_both(before as usize, item);
                    }
                }
                _ => set(&mut self.starts, place),
            }
            last = Some(place);
            last_key = key;
        }
    }

    fn decide(&mut self, cx: &mut Context<'_>) -> Result<Stage, QueryError> {
        let united = matches!(self.uniting, Uniting::No | Uniting::Done);
        if self.open_count == 0 && united {
            self.kept = None;
            return Ok(Stage::Number(0, 0));
        }
        if self.kept.is_some() {
            return Ok(self.refine(cx.limits.run));
        }
        match self.uniting {
            Uniting::First if self.open_count > 0 => {
                self.uniting = Uniting::Second;
                return Ok(self.pass(Pass::Digest(true)));
            }
            Uniting::First | Uniting::Second => {
                self.uniting = Uniting::Done;
                return Ok(Stage::Unite(0, 0));
            }
            _ => {}
        }
        // Those that are left are about as long as those of the last pass.
        let each = self.text_bytes.checked_div(self.texts).unwrap_or(0);
        let bytes = (self.open_count as u64).saturating_mul(each);
        let last = self.rounds >= cx.limits.rounds;
        let fits = last || bytes <= cx.limits.texts as u64;
        if self.open_count <= self.keep_below && fits {
            let words = self.open.len();
            let room = cx.large(words).and_then(|bits| {
                Ok(Loaded {
                    bits,
                    before: cx.large(words)?,
                    ends: cx.large(self.open_count)?,
                    bytes: cx.empty(),
                    depth: self.depth,
                })
            });
            if let Ok(mut room) = room {
                room.bytes.expect(bytes.min(cx.limits.texts as u64));
                self.kept = Some(room);
                return Ok(Stage::KeepBits(0));
            }
            self.short = true;
            return Ok(self.cannot_keep());
        }
        if self.uniting == Uniting::No && (last || self.rounds >= cx.limits.unite) {
            self.uniting = Uniting::First;
            self.runs = cx.large(self.starts.len())?;
            self.runs.extend_from_slice(&self.starts)?;
            return Ok(self.pass(Pass::Digest(false)));
        }
        if last {
            return Err(if self.short {
                QueryError::OutOfMemory
            } else {
                QueryError::TooManyEqualPrefixes
            });
        }
        self.rounds = self.rounds.saturating_add(1);
        Ok(self.pass(Pass::Next))
    }

    /// Goes through the texts that are open once more.
    const fn pass(&mut self, pass: Pass) -> Stage {
        self.texts = 0;
        self.text_bytes = 0;
        Stage::Entries(pass, 0)
    }

    /// There is no room for what is left of the texts. It is tried again when
    /// there are far fewer.
    fn cannot_keep(&mut self) -> Stage {
        self.kept = None;
        self.keep_below = self.open_count >> 1;
        Stage::Decide
    }

    /// Takes all but one of each that is equal out of the order. Until now the
    /// items that were equal so far were sorted by their digests.
    fn unite(&mut self, cx: &mut Context<'_>, at: usize, out: usize) -> Result<Step, QueryError> {
        let to = at.saturating_add(cx.limits.slice).min(self.order.len());
        let mut out = out;
        for place in at..to {
            let item = self.order.get(place).copied().unwrap_or(0);
            clear(&mut self.open, item as usize);
            let last = out.checked_sub(1).and_then(|last| self.order.get(last));
            let last = last.copied().unwrap_or(0);
            if !get(&self.starts, place) {
                self.twins.push((item, last))?;
                continue;
            }
            if get(&self.runs, place) {
                set(&mut self.starts, out);
            } else {
                clear(&mut self.starts, out);
                self.open_both(last as usize, item as usize);
            }
            if let Some(slot) = self.order.get_mut(out) {
                *slot = item;
            }
            out = out.saturating_add(1);
        }
        self.stage = if to < self.order.len() {
            Stage::Unite(to, out)
        } else {
            self.order.truncate(out);
            self.runs.take();
            self.open_count = self.still_open;
            Stage::Decide
        };
        Ok(Step::more(to.saturating_sub(at)))
    }

    /// Notes that an item is equal so far to the one before it.
    fn open_both(&mut self, before: usize, item: usize) {
        if !get(&self.open, before) {
            set(&mut self.open, before);
            self.still_open = self.still_open.saturating_add(1);
        }
        set(&mut self.open, item);
        self.still_open = self.still_open.saturating_add(1);
    }

    /// What is left of the texts is in memory: how they are told apart.
    fn refine(&self, run: usize) -> Stage {
        if self.open_count <= run {
            Stage::Rest(0, Vec::new())
        } else {
            Stage::NextKept(0)
        }
    }

    fn keep_bits(&mut self, at: usize, slice: usize) -> Result<Step, QueryError> {
        let to = at.saturating_add(slice.max(1)).min(self.open.len());
        if let Some(kept) = &mut self.kept {
            let mut before = match at.checked_sub(1) {
                None => 0,
                Some(last) => kept
                    .before
                    .get(last)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(kept.bits.get(last).copied().unwrap_or(0).count_ones()),
            };
            for word in self.open.get(at..to).unwrap_or(&[]) {
                kept.bits.push(*word)?;
                kept.before.push(before)?;
                before = before.saturating_add(word.count_ones());
            }
        }
        self.stage = if to < self.open.len() {
            Stage::KeepBits(to)
        } else {
            Stage::Entries(Pass::Keep, 0)
        };
        Ok(Step::more(to.saturating_sub(at)))
    }

    fn next_kept(&mut self, from: usize, slice: usize) -> Step {
        let items = self.keys.len();
        let to = from.saturating_add(slice).min(items);
        let mut at = from;
        let mut work = 0_usize;
        if let Some(kept) = &self.kept {
            while work < COMPARED {
                at = next_set(&self.open, at, to);
                if at >= to {
                    break;
                }
                if let (Ok(item), Some(key)) = (u32::try_from(at), self.keys.get_mut(at)) {
                    *key = kept.chunk(item, self.depth);
                }
                at = at.saturating_add(1);
                work = work.saturating_add(1);
            }
        }
        self.stage = if at < items {
            Stage::NextKept(at)
        } else {
            self.begin_runs()
        };
        Step::more(work)
    }

    const fn begin_runs(&mut self) -> Stage {
        self.depth = self.depth.saturating_add(8);
        Stage::Runs(0)
    }

    /// Sorts many items that begin alike, from one place to another.
    fn run(&self, start: usize, end: usize) -> Stage {
        Stage::Run(Run {
            start,
            end,
            phase: Phase::Count(start, Counts::new(64)),
        })
    }

    /// The end of the run that begins at a place.
    fn run_end(&self, start: usize) -> usize {
        next_set(&self.starts, start.saturating_add(1), self.order.len())
    }

    fn is_open_at(&self, place: usize) -> bool {
        self.order
            .get(place)
            .is_some_and(|item| get(&self.open, *item as usize))
    }

    /// Sorts the next items that begin alike, as far as there are not too many.
    fn sort_runs(&mut self, limit: usize, from: usize, slice: usize) -> Step {
        let stop = from.saturating_add(slice).min(self.order.len());
        let mut at = from;
        let mut work = 0_usize;
        while at < stop && work < COMPARED {
            let end = self.run_end(at);
            let len = end.saturating_sub(at);
            if len > 1 && self.is_open_at(at) {
                if len > limit {
                    self.stage = self.run(at, end);
                    return Step::more(work);
                }
                work = work.saturating_add(len);
                self.sort_keys(at, end);
                self.split(at, at, end);
            }
            at = end;
        }
        self.after_runs(at);
        Step::more(at.saturating_sub(from).max(work))
    }

    /// Sorts the items from one place to another by their keys.
    fn sort_keys(&mut self, start: usize, end: usize) {
        let keys = &*self.keys;
        let of = |item: u32| keys.get(item as usize).copied().unwrap_or(0);
        if let Some(items) = self.order.get_mut(start..end) {
            sort_by(items, &mut self.merged, &mut |a, b| of(a).cmp(&of(b)));
        }
    }

    fn after_runs(&mut self, at: usize) {
        self.stage = if at < self.order.len() {
            Stage::Runs(at)
        } else {
            self.open_count = self.still_open;
            Stage::Decide
        };
    }

    fn sort_run(&mut self, slice: usize) -> Step {
        let Stage::Run(run) = &mut self.stage else {
            return Step::more(0);
        };
        let (start, end) = (run.start, run.end);
        match &mut run.phase {
            Phase::Count(at, counts) => {
                let to = at.saturating_add(slice.min(COMPARED)).min(end);
                let items = self.order.get(*at..to).unwrap_or(&[]);
                let key = |item: &u32| self.keys.get(*item as usize).copied().unwrap_or(0);
                let keys: Vec<u64> = items.iter().map(key).collect();
                counts.add(&keys);
                let work = items.len();
                *at = to;
                if to >= end {
                    let counts = counts.take();
                    let len = end.saturating_sub(start);
                    run.phase = Phase::Sort(Radix::new(start, len, counts));
                }
                Step::more(work)
            }
            Phase::Sort(radix) => {
                let (work, done) = radix.step(&self.keys, &mut self.order, &mut self.other, slice);
                if done {
                    run.phase = if radix.in_other() {
                        Phase::Back(start)
                    } else {
                        Phase::Split(start)
                    };
                }
                Step::more(work)
            }
            Phase::Back(at) => {
                let from = *at;
                let to = from.saturating_add(slice).min(end);
                if let (Some(to), Some(from)) =
                    (self.order.get_mut(from..to), self.other.get(from..to))
                {
                    to.copy_from_slice(from);
                }
                *at = to;
                if to >= end {
                    run.phase = Phase::Split(start);
                }
                Step::more(to.saturating_sub(from))
            }
            Phase::Split(at) => {
                let from = *at;
                let to = from.saturating_add(slice).min(end);
                *at = to;
                self.split(start, from, to);
                if to >= end {
                    self.after_runs(end);
                }
                Step::more(to.saturating_sub(from))
            }
        }
    }

    /// Sorts the last items, whose texts are all in memory.
    fn sort_rest(&mut self, slice: usize) -> Step {
        let Stage::Rest(at, stack) = &mut self.stage else {
            return Step::more(0);
        };
        let (mut at, mut stack) = (*at, core::mem::take(stack));
        let items = self.order.len();
        let (mut work, mut passed) = (0_usize, 0_usize);
        while work < COMPARED && passed < slice {
            let Some((start, end, depth)) = stack.pop() else {
                if at >= items {
                    break;
                }
                let end = self.run_end(at);
                if end.saturating_sub(at) > 1 && self.is_open_at(at) {
                    stack.push((at, end, self.depth));
                }
                passed = passed.saturating_add(end.saturating_sub(at));
                at = end;
                continue;
            };
            work = work.saturating_add(end.saturating_sub(start));
            if let Some(kept) = &self.kept {
                for item in self.order.get(start..end).unwrap_or(&[]) {
                    if let Some(key) = self.keys.get_mut(*item as usize) {
                        *key = kept.chunk(*item, depth);
                    }
                }
            }
            self.sort_keys(start, end);
            let mut from = start;
            for place in start.saturating_add(1)..=end {
                let key = self.key_at(place.saturating_sub(1));
                if place < end && self.key_at(place) == key {
                    continue;
                }
                if place < end {
                    set(&mut self.starts, place);
                }
                if goes_on(key) && place.saturating_sub(from) > 1 {
                    stack.push((from, place, depth.saturating_add(8)));
                }
                from = place;
            }
        }
        self.stage = if at >= items && stack.is_empty() {
            self.kept = None;
            Stage::Number(0, 0)
        } else {
            Stage::Rest(at, stack)
        };
        Step::more(work.max(passed))
    }

    fn number(&mut self, at: usize, rank: u32, slice: usize) -> Step {
        let to = at.saturating_add(slice).min(self.order.len());
        let mut rank = rank;
        for place in at..to {
            if place > 0 && get(&self.starts, place) {
                rank = rank.saturating_add(1);
            }
            let item = self.order.get(place).copied().unwrap_or(0);
            if let Some(slot) = self.other.get_mut(item as usize) {
                *slot = rank;
            }
        }
        let work = to.saturating_sub(at);
        if to < self.order.len() {
            self.stage = Stage::Number(to, rank);
            return Step::more(work);
        }
        let some = u32::from(!self.order.is_empty());
        self.stage = Stage::Twins(0, rank.saturating_add(some));
        Step::more(work)
    }
}
