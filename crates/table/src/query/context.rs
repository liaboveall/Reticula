//! What the pieces of all jobs work with.

use alloc::rc::Rc;
use alloc::vec::Vec;

use super::QueryError;
use super::cells::{Cells, Part, is_coded};
use super::memory::{Large, Meter};
use super::ranks::Ranks;
use super::selection::{Selection, count};
use super::spec::SortKey;
use crate::layout::{DictionaryPlace, Encoding};
use crate::reader::{ReadError, Reader};
use crate::source::AnySource;

/// Bytes of dictionaries that are kept for the next job.
const DICTIONARIES_KEPT: usize = 64 << 20;
/// Bytes of ranks that are kept for the next job.
const RANKS_KEPT: usize = 24 << 20;

/// Sizes at which work is cut, or another way of doing it is taken. Tests set
/// them so that small tables go all ways.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// Items that are handled in one go, where that is as simple as can be.
    pub(super) slice: usize,
    /// Entries of a dictionary that are handled in one go.
    pub(super) entries: usize,
    /// Bytes of texts that are handled in one go, and texts that are entered
    /// in an index.
    pub(super) part: usize,
    pub(super) texts_at_once: usize,
    /// Items that are sorted in one go, by comparing their keys.
    pub(super) run: usize,
    /// Bytes of texts that may be kept to tell apart those that begin alike.
    pub(super) texts: usize,
    /// How often a column may be read to tell apart texts that begin alike.
    pub(super) rounds: u32,
    /// After how many times equal texts are looked for.
    pub(super) unite: u32,
    /// Bytes of a block for each selected cell, above which the cells are
    /// read one by one.
    pub(super) sparse: usize,
    /// Rows for each selected row, above which only those are sorted.
    pub(super) few: u32,
    /// Largest span of a column whose values are counted in an array.
    pub(super) direct: u64,
    /// Most different values that are counted otherwise, and bytes of their texts.
    pub(super) distinct: usize,
    pub(super) listed: usize,
}

impl Limits {
    pub(super) const DEFAULT: Self = Self {
        slice: 262_144,
        entries: 4_096,
        part: 4 << 20,
        texts_at_once: 16_384,
        run: 32_768,
        texts: 64 << 20,
        rounds: 16,
        unite: 4,
        sparse: 16_384,
        few: 4,
        direct: 1 << 24,
        distinct: 1 << 20,
        listed: 64 << 20,
    };
}

/// What a piece of work came to: about how many cells it was, and whether it
/// was the last.
#[derive(Clone, Copy, Debug)]
pub(super) struct Step {
    pub(super) cost: u64,
    pub(super) done: bool,
}

impl Step {
    pub(super) const fn more(cells: usize) -> Self {
        Self {
            cost: if cells == 0 { 1 } else { cells as u64 },
            done: false,
        }
    }

    pub(super) const fn done(cells: usize) -> Self {
        Self {
            done: true,
            ..Self::more(cells)
        }
    }

    /// The same piece of a larger job.
    pub(super) const fn part(self) -> Self {
        Self {
            done: false,
            ..self
        }
    }
}

/// An order of rows that was worked out, to be used again.
#[derive(Debug)]
pub(super) struct KeptOrder {
    pub(super) keys: Vec<SortKey>,
    pub(super) of: Selection,
    /// The rows, unless their order is that of the file.
    pub(super) rows: Option<Rc<Large<u32>>>,
}

/// What is kept from one job to the next.
#[derive(Debug, Default)]
pub(super) struct Caches {
    ranks: Vec<(usize, Rc<Ranks>)>,
    /// The columns whose dictionaries are in memory, the latest one last.
    dictionaries: Vec<usize>,
    loading: Option<usize>,
    pub(super) order: Option<KeptOrder>,
}

impl Caches {
    pub(super) fn ranks(&self, column: usize) -> Option<Rc<Ranks>> {
        self.ranks
            .iter()
            .find(|(of, _)| *of == column)
            .map(|(_, ranks)| Rc::clone(ranks))
    }

    pub(super) fn keep_ranks(&mut self, column: usize, ranks: &Rc<Ranks>) {
        self.ranks.retain(|(of, _)| *of != column);
        self.ranks.push((column, Rc::clone(ranks)));
        let mut bytes = self
            .ranks
            .iter()
            .map(|(_, ranks)| ranks.bytes())
            .fold(0, usize::saturating_add);
        while bytes > RANKS_KEPT && self.ranks.len() > 1 {
            let (_, oldest) = self.ranks.remove(0);
            bytes = bytes.saturating_sub(oldest.bytes());
        }
    }

    /// Lets go of everything that is not in use.
    fn release(&mut self, reader: &mut Reader) {
        self.ranks.clear();
        self.order = None;
        self.drop_dictionaries(reader, 0);
    }

    /// Lets go of all dictionaries but so many of the latest ones.
    fn drop_dictionaries(&mut self, reader: &mut Reader, keep: usize) {
        let first = self.dictionaries.len().saturating_sub(keep);
        for column in self.dictionaries.drain(..first) {
            reader.drop_dictionary(column);
        }
    }

    /// Lets go of a dictionary that was not loaded completely.
    pub(super) fn abandon(&mut self, reader: &mut Reader) {
        if let Some(column) = self.loading.take()
            && reader.dictionary(column).is_none()
        {
            reader.drop_dictionary(column);
        }
    }
}

/// What a piece of work is done with.
pub(super) struct Context<'a> {
    pub(super) reader: &'a mut Reader,
    pub(super) source: AnySource<'a>,
    pub(super) cells: &'a mut Cells,
    pub(super) caches: &'a mut Caches,
    pub(super) meter: &'a Meter,
    pub(super) limits: Limits,
}

/// Bytes a dictionary takes in memory.
fn size(place: &DictionaryPlace) -> usize {
    usize::try_from(place.bytes_len)
        .unwrap_or(usize::MAX)
        .saturating_add((place.entries as usize).saturating_mul(4))
}

impl Context<'_> {
    /// Counts what the reader uses now.
    pub(super) fn count_reader(&self) {
        self.meter.set_outside(self.reader.memory());
    }

    fn release(&mut self) {
        self.caches.release(self.reader);
        self.count_reader();
    }

    /// An empty buffer.
    pub(super) fn empty<T: Copy>(&self) -> Large<T> {
        Large::empty(self.meter)
    }

    /// Makes sure that so many bytes may be used. What is kept for later is let
    /// go when they may not.
    #[inline(never)]
    fn make_room(&mut self, bytes: usize) -> Result<(), QueryError> {
        if !self.meter.fits(bytes) {
            self.release();
            if !self.meter.fits(bytes) {
                return Err(QueryError::OutOfMemory);
            }
        }
        Ok(())
    }

    /// An empty buffer with room for so many items. What is kept for later
    /// makes room for it when there is none.
    pub(super) fn large<T: Copy>(&mut self, len: usize) -> Result<Large<T>, QueryError> {
        self.make_room(len.saturating_mul(size_of::<T>()))?;
        let mut large = self.empty();
        if large.reserve(len).is_err() {
            self.release();
            large.reserve(len)?;
        }
        Ok(large)
    }

    /// Where the dictionary of a column lies, if it has one.
    pub(super) fn place(&self, column: usize) -> Option<DictionaryPlace> {
        self.reader.columns().get(column)?.dictionary
    }

    /// Loads the next piece of a dictionary. Gives whether all of it is here.
    #[inline(never)]
    pub(super) fn dictionary(&mut self, column: usize) -> Result<bool, QueryError> {
        if self.reader.dictionary(column).is_some() {
            return Ok(true);
        }
        let Some(place) = self.place(column) else {
            return Ok(true);
        };
        if self.caches.loading != Some(column) {
            self.caches.abandon(self.reader);
            let mut bytes = size(&place);
            let mut keep = 0_usize;
            for of in self.caches.dictionaries.iter().rev() {
                bytes = bytes.saturating_add(self.place(*of).as_ref().map_or(0, size));
                if bytes > DICTIONARIES_KEPT {
                    break;
                }
                keep = keep.saturating_add(1);
            }
            self.caches.drop_dictionaries(self.reader, keep);
            self.count_reader();
            // Its ends are packed and unpacked on the way.
            let ends = usize::try_from(place.ends_len()).unwrap_or(usize::MAX);
            let unpacked = (place.entries as usize).saturating_mul(8);
            self.make_room(size(&place).saturating_add(ends).saturating_add(unpacked))?;
            self.caches.loading = Some(column);
        }
        let ready = self.reader.load_dictionary(&mut self.source, column);
        self.count_reader();
        let ready = ready?;
        if ready {
            self.caches.loading = None;
            self.caches.dictionaries.retain(|of| *of != column);
            self.caches.dictionaries.push(column);
        }
        Ok(ready)
    }

    /// Loads a part of the cells of a group into `cells`: the selected ones one
    /// by one when there are so few that this is cheaper, or when the block is
    /// too long to read all of it in one go. Gives the first row of the group
    /// that is not in the part.
    #[inline(never)]
    pub(super) fn load(
        &mut self,
        column: usize,
        group: u32,
        selected: Option<&[u64]>,
        part: Part,
    ) -> Result<usize, QueryError> {
        let directory = self.reader.directory();
        let record = directory
            .block(column, group)
            .copied()
            .ok_or(ReadError::Corrupt("no such block"))?;
        let at = (column, group);
        let few = selected.is_some_and(|selected| {
            record.encoding != Encoding::Blank
                && !is_coded(directory, column, &record)
                && count(selected).saturating_mul(self.limits.sparse) < record.len as usize
        });
        let long = record.encoding == Encoding::Text && record.len as usize > self.limits.part;
        if few || long {
            let (part, most) = ((selected, part), self.limits.part);
            let (reader, source) = (&mut *self.reader, &mut self.source);
            return self.cells.gather(reader, source, at, &record, part, most);
        }
        let bytes = Cells::size(&record, directory.rows_in(group) as usize);
        if let Some(more) = bytes.checked_sub(self.cells.charged()) {
            self.make_room(more)?;
            self.cells.count_bytes(bytes)?;
        }
        self.cells
            .load(self.reader, &mut self.source, at, &record, part)
    }
}
