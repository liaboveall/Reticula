//! Finding where the largest or smallest values of a column begin.
//!
//! The keys of the values are counted by their first sixteen bits, which tells
//! what those of the value that is looked for are; then those that have them
//! are counted by the next ones, until all bits are known. The column is read
//! at most four times, and the blocks whose values cannot be meant are left out.

use super::QueryError;
use super::cells::{Part, each_word};
use super::context::{Context, Step};
use super::exact::Range;
use super::memory::Large;
use super::predicate::Test;
use super::space::Space;
use crate::bits;
use crate::layout::Encoding;

const BITS: u32 = 16;
const BUCKETS: usize = 1 << BITS;

#[derive(Debug)]
pub(super) struct TopSearch {
    column: usize,
    bottom: bool,
    space: Option<Space>,
    /// How many values come before the one that is looked for, among those
    /// that begin with the bits that are known.
    before: u64,
    /// The bits that are known, and how many are not.
    known: u64,
    unknown: u32,
    /// The bits that are found next are above these.
    below: u32,
    counts: Large<u32>,
    group: u32,
}

impl TopSearch {
    pub(super) fn new(
        cx: &mut Context<'_>,
        column: usize,
        n: u32,
        bottom: bool,
    ) -> Result<Self, QueryError> {
        let directory = cx.reader.directory();
        let blanks = directory.columns.get(column).map_or(0, |of| of.blanks);
        let values = u64::from(directory.rows).saturating_sub(blanks);
        let space = Space::of(directory, column, &|_| true).filter(|_| n > 0 && values > 0);
        let unknown = space.as_ref().map_or(0, Space::width);
        let mut counts = cx.large(BUCKETS)?;
        counts.fill(BUCKETS, 0, BUCKETS)?;
        Ok(Self {
            column,
            bottom,
            space,
            before: u64::from(n).min(values).saturating_sub(1),
            known: 0,
            unknown,
            below: unknown.saturating_sub(BITS),
            counts,
            group: 0,
        })
    }

    /// Reads the next group.
    pub(super) fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError> {
        let Some(space) = self.space.filter(|_| self.unknown > 0) else {
            return Ok(Step::done(1));
        };
        let directory = cx.reader.directory();
        if self.group >= directory.groups() {
            self.narrow();
            return Ok(Step::more(BUCKETS));
        }
        let group = self.group;
        let wanted = directory.block(self.column, group).is_some_and(|block| {
            let (low, high) = space.keys_of(block);
            let begins = |key: u64| shifted(key, self.unknown);
            block.encoding != Encoding::Blank
                && begins(low) <= self.known
                && self.known <= begins(high)
        });
        if !wanted {
            self.group = group.saturating_add(1);
            return Ok(Step::more(1));
        }
        cx.load(self.column, group, None, Part::ALL)?;
        self.group = group.saturating_add(1);
        let cells = &*cx.cells;
        let scale = space.scale(cells.record());
        let (known, unknown, below) = (self.known, self.unknown, self.below);
        let mask = bits::mask(unknown.saturating_sub(below));
        let counts: &mut [u32] = &mut self.counts;
        let mut count = |word: u64| {
            let key = scale.key(word);
            if shifted(key, unknown) == known {
                let bucket = usize::try_from(shifted(key, below) & mask).unwrap_or(usize::MAX);
                if let Some(count) = counts.get_mut(bucket) {
                    *count = count.wrapping_add(1);
                }
            }
        };
        for (at, words) in cells.words().chunks(64).enumerate() {
            each_word(cells.values(None, at), words, &mut count);
        }
        Ok(Step::more(cells.rows()))
    }

    /// All values are counted: finds the next bits.
    fn narrow(&mut self) {
        let width = self.unknown.saturating_sub(self.below);
        let buckets = 1_usize.wrapping_shl(width);
        let mut before = self.before;
        let mut found = 0_usize;
        for number in 0..buckets {
            let bucket = if self.bottom {
                number
            } else {
                buckets.saturating_sub(number).saturating_sub(1)
            };
            let count = u64::from(self.counts.get(bucket).copied().unwrap_or(0));
            if count > 0 {
                found = bucket;
            }
            match before.checked_sub(count) {
                Some(rest) => before = rest,
                None => break,
            }
        }
        self.before = before;
        self.known = self
            .known
            .wrapping_shl(width)
            .wrapping_add(u64::try_from(found).unwrap_or(0));
        self.unknown = self.below;
        self.below = self.below.saturating_sub(BITS);
        self.group = 0;
        let counts: &mut [u32] = &mut self.counts;
        counts.fill(0);
    }

    /// What is asked of the cells, when the last step is done.
    pub(super) fn finish(&self) -> Test {
        let Some(space) = &self.space else {
            return Test::Ints(Range::NONE);
        };
        let value = space.stored(self.known);
        let inside = true;
        match space {
            Space::Integral { .. } => {
                let value = value.cast_signed();
                let (low, high) = if self.bottom {
                    (i64::MIN, value)
                } else {
                    (value, i64::MAX)
                };
                Test::Ints(Range { low, high, inside })
            }
            Space::Float { .. } => {
                let (low, high) = if self.bottom {
                    (u64::MIN, value)
                } else {
                    (value, u64::MAX)
                };
                Test::Floats(Range { low, high, inside })
            }
        }
    }
}

/// A key without its last bits.
fn shifted(key: u64, bits: u32) -> u64 {
    key.checked_shr(bits).unwrap_or(0)
}
