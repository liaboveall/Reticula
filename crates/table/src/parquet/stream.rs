//! The values of one column, page after page.
//!
//! A stream holds the bytes it read ahead, one unpacked page and the
//! dictionary of its chunk. It gives as many rows as it is asked for, whatever
//! the sizes of the pages and of the chunks are. Several streams that hold too
//! much together can be told to let go of everything, and then read their
//! page again when they go on.
//!
//! Values go through [`Scratch::values`] in batches: numbers as their bits,
//! texts as the place where they are, see [`span`]. A dictionary holds the
//! same, so that the values of a page are looked up in it whatever they are.

use alloc::string::String;
use alloc::vec::Vec;

use super::codec::{Codec, Unpacker};
use super::meta::{
    BIT_PACKED, BYTE_STREAM_SPLIT, Chunk, DELTA_BINARY_PACKED, DELTA_BYTE_ARRAY,
    DELTA_LENGTH_BYTE_ARRAY, Fault, PLAIN, PLAIN_DICTIONARY, PageHeader, PageKind, RLE,
    RLE_DICTIONARY, read_page_header,
};
use super::runs::{Delta, Hybrid, unpack};
use super::thrift::ThriftError;
use super::value::{
    self, BLANK, CUT_NANOS, KEPT, NOT_FINITE, OUT_OF_RANGE, Physical, Plan, TOO_WIDE,
};
use super::{Column, Damage, Limit};
use crate::hash;
use crate::source::{ByteRange, Source, SourceError};
use crate::types::MAX_TEXT_BYTES;
use crate::writer::ColumnBuilder;

/// Rows that are done at once.
pub(super) const BATCH: usize = 1_024;
/// Rows that are done at once when each value is put together of two parts,
/// and so has to be copied.
const SMALL_BATCH: usize = 128;
/// So many equal levels are worth a batch of their own.
const LONG_RUN: usize = 64;
/// Longest page, unpacked or not.
const MAX_PAGE: usize = 256 << 20;
/// Longest start of a page. It may hold the smallest and the largest value.
const MAX_HEADER: usize = 16 << 20;
/// How much of a start of a page is read at first.
const HEADER: usize = 256;
/// Longest dictionary, unpacked, and most entries of one. Dictionaries of a
/// hundred megabytes occur, of long texts that come again; and dictionaries
/// with more entries than there are rows, of categories.
const MAX_DICTIONARY: usize = 128 << 20;
const MAX_ENTRIES: usize = 1 << 22;
/// Entries of a dictionary that are read at once.
const SLICE: usize = 16;
/// Longest page without any rows.
const MAX_EMPTY: usize = 4 << 10;
/// By how many bytes a chunk may be longer than it says. Some writers have
/// forgotten the header of the dictionary.
const FORGOTTEN: u64 = 100;
/// Most bytes of text a group may take of one column.
const MAX_GROUP_TEXT: usize = 128 << 20;
/// Texts that are looked at to find out whether a dictionary is worth it, and
/// how many bytes of them.
const SAMPLE_TEXTS: usize = 16_384;
const SAMPLE_PLACES: usize = 1 << 16;
const SAMPLE_BYTES: usize = 16 << 20;
/// Most bytes of a text that are looked at: what is needed to know that it is
/// too long, and where to cut it.
const MAX_LOOK: usize = MAX_TEXT_BYTES.wrapping_add(4);

/// The code of a text that has none yet.
const NO_CODE: u32 = u32::MAX;

/// What a file does not say in its values but in the way it holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Problem {
    /// Values do not fit the plan, and have to become text.
    TooWide,
    /// Values are not text.
    NotText,
    /// The texts of a group take too much memory.
    TooMuchText,
}

/// What came of asking a stream for rows.
#[derive(Debug)]
pub(super) enum Advance {
    /// So many rows were given.
    Rows(usize),
    /// Something else was done.
    Worked,
    /// Bytes are missing.
    Need(ByteRange),
    Problem(Problem),
}

/// What happened to the values of a column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) blanks: u64,
    pub(super) cut_nanos: u64,
    pub(super) not_finite: u64,
    pub(super) out_of_range: u64,
    pub(super) cut_texts: u64,
}

/// How many of the first texts of a column are different.
#[derive(Debug, Default)]
pub(super) struct Sample {
    /// The hashes of the texts, each near the place it names. 0 is no hash.
    seen: Vec<u64>,
    different: usize,
    /// Texts that were looked at, and their bytes.
    texts: usize,
    bytes: usize,
}

impl Sample {
    /// Whether enough was looked at.
    pub(super) const fn is_full(&self) -> bool {
        self.texts >= SAMPLE_TEXTS || self.bytes >= SAMPLE_BYTES
    }

    /// Whether the texts repeat so often that a dictionary is worth it.
    pub(super) fn repeats(&self) -> bool {
        self.different.saturating_mul(2) <= self.texts
    }

    fn look_at(&mut self, text: &str) {
        self.texts = self.texts.saturating_add(1);
        self.bytes = self.bytes.saturating_add(text.len());
        // More places than texts, so there is a free one.
        self.seen.resize(SAMPLE_PLACES, 0);
        let hash = hash::of_text(text.as_bytes()).max(1);
        let first = usize::try_from(hash & 0xffff_ffff).unwrap_or(0);
        for step in 0..SAMPLE_PLACES {
            let place = first.wrapping_add(step) & SAMPLE_PLACES.wrapping_sub(1);
            match self.seen.get_mut(place) {
                Some(slot) if *slot == 0 => {
                    *slot = hash;
                    self.different = self.different.saturating_add(1);
                    return;
                }
                Some(slot) if *slot == hash => return,
                _ => {}
            }
        }
    }
}

/// Where values go.
#[derive(Debug)]
pub(super) enum Out<'a> {
    Cells(&'a mut ColumnBuilder),
    Sample(&'a mut Sample),
    /// Nowhere: they are only looked at.
    Nowhere,
}

/// What is needed for a while, by one stream at a time.
#[derive(Debug)]
pub(super) struct Scratch {
    pub(super) unpacker: Unpacker,
    levels: Vec<u64>,
    /// Values of a batch, and what happened to each.
    values: Vec<u64>,
    status: Vec<u8>,
    lens: Vec<u64>,
    /// Bytes that were put together, and texts that were written.
    bytes: Vec<u8>,
    text: String,
}

impl Scratch {
    pub(super) fn new() -> Self {
        let numbers = alloc::vec![0; BATCH];
        Self {
            unpacker: Unpacker::default(),
            levels: numbers.clone(),
            values: numbers.clone(),
            lens: numbers,
            status: alloc::vec![0; BATCH],
            bytes: Vec::new(),
            text: String::new(),
        }
    }
}

/// The place of a text: where it starts and where it ends.
fn span(from: usize, to: usize) -> u64 {
    let half = |at| u64::try_from(at).unwrap_or(0) & 0xffff_ffff;
    half(from) << 32 | half(to)
}

/// The bytes at a place.
fn at_span(bytes: &[u8], span: u64) -> Option<&[u8]> {
    let from = usize::try_from(span >> 32).ok()?;
    let to = usize::try_from(span & 0xffff_ffff).ok()?;
    bytes.get(from..to)
}

/// What a stream reads from, and what it may read.
#[derive(Debug)]
pub(super) struct Input<'a, S> {
    pub(super) source: &'a mut S,
    pub(super) column: &'a Column,
    /// The pages of the file end here.
    pub(super) data_end: u64,
    /// How much is read at once.
    pub(super) window: usize,
}

#[derive(Clone, Copy, Debug)]
enum Levels {
    /// Every row has a value.
    All,
    /// No row has one.
    None,
    Runs(Hybrid),
    /// One bit for each row, the highest bits of a byte first; the next bit.
    Bits(u64),
}

/// How the values of a page are encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Coding {
    /// There are none.
    #[default]
    None,
    /// One after another.
    Plain,
    /// Places in the dictionary.
    Codes,
    /// Runs of numbers of one bit.
    Runs,
    /// Differences of numbers.
    Differences,
    /// Lengths, then the texts.
    Lengths,
    /// How much is taken from the text before, the lengths of the rest, then
    /// the rests.
    Parts,
    /// All first bytes, all second ones and so on.
    Split,
}

/// The values of a page that were not read.
#[derive(Debug, Default)]
struct Values {
    coding: Coding,
    /// The byte, or the bit, at which the next value starts.
    at: usize,
    bit: u64,
    runs: Hybrid,
    deltas: [Delta; 2],
    /// The text before the next one.
    before: Vec<u8>,
    /// Of split values: how many there are, and which is next.
    count: usize,
    next: usize,
}

/// The values a page may name, as they come out.
#[derive(Debug, Default)]
struct Dictionary {
    values: Vec<u64>,
    status: Vec<u8>,
    /// Everything that happened to them.
    seen: u8,
    /// The page it came from, where the texts are. Texts that had to be
    /// written are in `text` instead.
    page: Vec<u8>,
    text: String,
    /// What of the page was not read yet.
    rest: Values,
    left: usize,
    /// The codes the texts have in the table, as they are first used.
    codes: Vec<u32>,
}

/// Where to go on after everything was let go.
#[derive(Clone, Copy, Debug)]
struct Resume {
    page_at: u64,
    left: u64,
    skip: usize,
    dictionary: bool,
}

/// Where a stream is.
#[derive(Debug)]
pub(super) struct Stream {
    /// The chunk after the current one.
    next_chunk: usize,
    /// Values of the chunk in pages that were not begun.
    left: u64,
    /// Offset of the page that comes next.
    at: u64,
    /// Offset of the current page.
    page_at: u64,
    raw: Vec<u8>,
    raw_at: u64,
    page: Vec<u8>,
    /// Rows of the page that were not given, and that were.
    page_left: usize,
    page_done: usize,
    levels: Levels,
    values: Values,
    dictionary: Dictionary,
    /// Where the dictionary of the current chunk is; 0 when there is none.
    dictionary_at: u64,
    /// Rows that were given before, and have to be left out.
    skip: usize,
    resume: Option<Resume>,
    /// Whether the codes of the dictionary are known to be good in this group.
    /// The table may stop taking texts into its dictionary between two.
    codes_good: bool,
    /// Entries of the table's dictionary that came from here, and how many
    /// bytes of text they brought in this group.
    entries: u32,
    pub(super) gained: usize,
    pub(super) counts: Counts,
}

impl Stream {
    #[inline(never)]
    pub(super) fn new() -> Self {
        Self {
            next_chunk: 0,
            left: 0,
            at: 0,
            page_at: 0,
            raw: Vec::new(),
            raw_at: 0,
            page: Vec::new(),
            page_left: 0,
            page_done: 0,
            levels: Levels::All,
            values: Values::default(),
            dictionary: Dictionary::default(),
            dictionary_at: 0,
            skip: 0,
            resume: None,
            codes_good: false,
            entries: 0,
            gained: 0,
            counts: Counts::default(),
        }
    }

    /// Bytes that are held.
    pub(super) fn memory(&self) -> usize {
        let dictionary = &self.dictionary;
        self.raw
            .capacity()
            .saturating_add(self.page.capacity())
            .saturating_add(dictionary.values.capacity().saturating_mul(13))
            .saturating_add(dictionary.page.capacity())
            .saturating_add(dictionary.text.capacity())
    }

    /// Whether the dictionary of the chunk that is being read has few entries
    /// for the values there are.
    pub(super) fn few_in_dictionary(&self, column: &Column) -> bool {
        let entries = u64::try_from(self.dictionary.values.len()).unwrap_or(u64::MAX);
        let values = self.chunk(column).map_or(0, |chunk| chunk.values);
        self.dictionary_at > 0 && entries.saturating_mul(2) <= values
    }

    /// The chunk that is being read.
    fn chunk(&self, column: &Column) -> Option<Chunk> {
        let index = self.next_chunk.checked_sub(1)?;
        column.chunks.get(index).copied()
    }

    /// Where the chunk that is being read ends, as the metadata says.
    fn chunk_end<S>(&self, input: &Input<'_, S>) -> u64 {
        self.chunk(input.column).map_or(0, |chunk| {
            chunk.start.saturating_add(chunk.len).min(input.data_end)
        })
    }

    /// Lets go of everything that can be read again.
    pub(super) fn suspend(&mut self) {
        if self.resume.is_none() && self.next_chunk > 0 {
            let mut resume = Resume {
                page_at: self.at,
                left: self.left,
                skip: 0,
                dictionary: false,
            };
            if self.page_left > 0 {
                let given = self.page_left.saturating_add(self.page_done);
                resume.page_at = self.page_at;
                resume.left = self.left.saturating_add(u64::try_from(given).unwrap_or(0));
                // What is still to be left out was given before, too.
                resume.skip = self.page_done.saturating_add(self.skip);
            }
            self.resume = Some(resume);
        }
        if let Some(resume) = &mut self.resume {
            resume.dictionary = self.dictionary_at > 0;
        }
        let fresh = Self {
            next_chunk: self.next_chunk,
            dictionary_at: self.dictionary_at,
            resume: self.resume,
            entries: self.entries,
            gained: self.gained,
            counts: self.counts,
            ..Self::new()
        };
        *self = fresh;
    }

    /// Bytes of text that the cells of the group take.
    pub(super) const fn text_in(&self, cells: &ColumnBuilder) -> usize {
        self.gained.saturating_add(cells.text_bytes())
    }

    /// Begins with another group.
    pub(super) fn begin_group(&mut self) {
        self.gained = 0;
        self.codes_good = false;
    }

    /// Begins at the start again.
    #[inline(never)]
    pub(super) fn restart(&mut self) {
        *self = Self::new();
    }

    /// Gives at most `most` rows, or does something that has to be done
    /// before. `work` grows by about the number of bytes that were handled.
    pub(super) fn advance<S: Source>(
        &mut self,
        input: &mut Input<'_, S>,
        scratch: &mut Scratch,
        most: usize,
        out: &mut Out<'_>,
        work: &mut usize,
    ) -> Result<Advance, Fault> {
        *work = work.saturating_add(32);
        if self.dictionary.left > 0 {
            self.read_entries(input.column, scratch, work)
                .ok_or(Fault::Damaged(Damage::Dictionary))?;
            return Ok(Advance::Worked);
        }
        if self.page_left == 0 {
            return Ok(match self.next_page(input, scratch, work)? {
                Some(range) => Advance::Need(range),
                None => Advance::Worked,
            });
        }
        let skipping = self.skip > 0;
        let wanted = if skipping { self.skip } else { most };
        let small = self.values.coding == Coding::Parts;
        let batch = if small { SMALL_BATCH } else { BATCH };
        let most = wanted.min(self.page_left).min(batch);
        let mut nowhere = Out::Nowhere;
        let out = if skipping { &mut nowhere } else { out };
        let (rows, problem) = self
            .rows(input.column, scratch, most, out, work)
            .ok_or(Fault::Damaged(Damage::Values))?;
        self.page_left = self.page_left.saturating_sub(rows);
        self.page_done = self.page_done.saturating_add(rows);
        if let Some(problem) = problem {
            return Ok(Advance::Problem(problem));
        }
        if skipping {
            self.skip = self.skip.saturating_sub(rows);
            return Ok(Advance::Worked);
        }
        Ok(Advance::Rows(rows))
    }

    /// Has `len` bytes from `at` on in `raw`. Gives what is missing.
    #[inline(never)]
    fn ensure<S: Source>(
        &mut self,
        input: &mut Input<'_, S>,
        at: u64,
        len: usize,
    ) -> Result<Option<ByteRange>, Fault> {
        let end = u64::try_from(len)
            .ok()
            .and_then(|len| at.checked_add(len))
            .filter(|end| *end <= input.data_end)
            .ok_or(Fault::Damaged(Damage::Place))?;
        let held = self
            .raw_at
            .saturating_add(u64::try_from(self.raw.len()).unwrap_or(0));
        if at >= self.raw_at && end <= held {
            return Ok(None);
        }
        // What is there already goes to the front.
        if at >= self.raw_at && at < held {
            let used = usize::try_from(at.saturating_sub(self.raw_at)).unwrap_or(usize::MAX);
            self.raw.drain(..used.min(self.raw.len()));
        } else {
            self.raw.clear();
        }
        self.raw_at = at;
        let window = u64::try_from(input.window).unwrap_or(0);
        let until = at
            .saturating_add(window)
            .min(self.chunk_end(input))
            .max(end);
        let have = self.raw.len();
        let range = ByteRange {
            at: at.saturating_add(u64::try_from(have).unwrap_or(0)),
            len: until
                .saturating_sub(at)
                .saturating_sub(u64::try_from(have).unwrap_or(0)),
        };
        let more = usize::try_from(range.len).map_err(|_| Fault::Memory)?;
        fill_up(&mut self.raw, have.saturating_add(more))?;
        match input
            .source
            .read(range.at, self.raw.get_mut(have..).unwrap_or_default())
        {
            Ok(()) => Ok(None),
            Err(error) => {
                self.raw.truncate(have);
                match error {
                    SourceError::Missing => Ok(Some(range)),
                    SourceError::Failed => Err(Fault::Source),
                }
            }
        }
    }

    /// Reads the start of the page at `at`. Gives how long it is besides, or
    /// what is missing.
    fn header<S: Source>(
        &mut self,
        input: &mut Input<'_, S>,
    ) -> Result<Result<(PageHeader, usize), ByteRange>, Fault> {
        // Not more than the chunk has, unless that does not do.
        let left = self.chunk_end(input).saturating_sub(self.at).max(1);
        let mut wanted = usize::try_from(left).unwrap_or(HEADER).min(HEADER);
        loop {
            let most = input.data_end.saturating_sub(self.at);
            let most = usize::try_from(most).unwrap_or(usize::MAX).min(MAX_HEADER);
            let len = wanted.min(most);
            if len == 0 {
                return Err(Fault::Damaged(Damage::Place));
            }
            if let Some(range) = self.ensure(input, self.at, len)? {
                return Ok(Err(range));
            }
            let offset = usize::try_from(self.at.saturating_sub(self.raw_at)).unwrap_or(usize::MAX);
            let bytes = self.raw.get(offset..).unwrap_or_default();
            match read_page_header(bytes) {
                Ok(found) => return Ok(Ok(found)),
                Err(ThriftError::Ends) if bytes.len() < most => {
                    wanted = bytes.len().saturating_mul(2).max(HEADER);
                }
                Err(_) => return Err(Fault::Damaged(Damage::Page)),
            }
        }
    }

    /// Reads the next page. Gives what is missing.
    fn next_page<S: Source>(
        &mut self,
        input: &mut Input<'_, S>,
        scratch: &mut Scratch,
        work: &mut usize,
    ) -> Result<Option<ByteRange>, Fault> {
        if let Some(resume) = self.resume {
            self.left = resume.left;
            self.at = if resume.dictionary {
                self.dictionary_at
            } else {
                resume.page_at
            };
        }
        while self.left == 0 {
            let chunk = input
                .column
                .chunks
                .get(self.next_chunk)
                .ok_or(Fault::Damaged(Damage::Count))?;
            self.next_chunk = self.next_chunk.saturating_add(1);
            self.left = chunk.values;
            self.at = chunk.start;
            self.dictionary_at = 0;
            self.resume = None;
        }
        // What comes after a chunk is another column.
        let chunk_end = self.chunk_end(input);
        if self.at >= chunk_end {
            return Err(Fault::Damaged(Damage::Place));
        }
        let codec = self
            .chunk(input.column)
            .map_or(Codec::Plain, |chunk| chunk.codec);
        let (header, header_len) = match self.header(input)? {
            Ok(found) => found,
            Err(range) => return Ok(Some(range)),
        };
        let dictionary = header.kind == PageKind::Dictionary;
        let most = if dictionary { MAX_DICTIONARY } else { MAX_PAGE };
        if header.packed_len.max(header.len) > most || (dictionary && header.values > MAX_ENTRIES) {
            return Err(Fault::Limit(Limit::Page));
        }
        if header.values == 0 && header.len > MAX_EMPTY {
            return Err(Fault::Damaged(Damage::Page));
        }
        // The header is asked for with the rest: it is read again when the rest
        // has come.
        let page_len = header_len.saturating_add(header.packed_len);
        let next = self
            .at
            .saturating_add(u64::try_from(page_len).unwrap_or(u64::MAX));
        if next > chunk_end.saturating_add(FORGOTTEN) {
            return Err(Fault::Damaged(Damage::Place));
        }
        if header.kind == PageKind::Other {
            self.at = next;
            if let Some(resume) = &mut self.resume {
                resume.page_at = next;
            }
            return Ok(None);
        }
        if let Some(range) = self.ensure(input, self.at, page_len)? {
            return Ok(Some(range));
        }
        let start = usize::try_from(self.at.saturating_sub(self.raw_at)).unwrap_or(usize::MAX);
        let body = self
            .raw
            .get(start.saturating_add(header_len)..start.saturating_add(page_len))
            .ok_or(Fault::Damaged(Damage::Place))?;
        *work = work
            .saturating_add(header.packed_len)
            .saturating_add(header.len);
        if let Some(checksum) = header.checksum
            && scratch.unpacker.checksum(body) != checksum
        {
            return Err(Fault::Damaged(Damage::Checksum));
        }
        // What lies before the packed part of the page.
        let levels = header.repetition.saturating_add(header.definition);
        self.page_at = self.at;
        self.at = next;
        self.page.clear();
        fill_up(&mut self.page, header.len)?;
        let (before, rest) = body
            .split_at_checked(levels)
            .ok_or(Fault::Damaged(Damage::Levels))?;
        let (to_before, to_rest) = self
            .page
            .split_at_mut_checked(levels)
            .ok_or(Fault::Damaged(Damage::Levels))?;
        to_before.copy_from_slice(before);
        let codec = if header.packed { codec } else { Codec::Plain };
        scratch
            .unpacker
            .unpack(codec, rest, to_rest)
            .ok_or(Fault::Damaged(Damage::Packed))?;
        match header.kind {
            PageKind::Other => {}
            PageKind::Dictionary => {
                // There is one at most, which is read again after a while.
                let again = self
                    .resume
                    .as_mut()
                    .is_some_and(|resume| core::mem::take(&mut resume.dictionary));
                if self.dictionary_at > 0 && !again {
                    return Err(Fault::Damaged(Damage::Dictionary));
                }
                self.begin_dictionary(input.column, &header)
                    .ok_or(Fault::Damaged(Damage::Dictionary))?;
                self.dictionary_at = self.page_at;
            }
            PageKind::Data | PageKind::DataV2 => {
                let values = u64::try_from(header.values).unwrap_or(u64::MAX);
                self.left = self
                    .left
                    .checked_sub(values)
                    .ok_or(Fault::Damaged(Damage::Count))?;
                self.begin(input.column, &header)?;
                self.page_left = header.values;
                self.page_done = 0;
                if let Some(resume) = self.resume.take() {
                    if resume.dictionary || resume.skip > header.values {
                        return Err(Fault::Damaged(Damage::Dictionary));
                    }
                    self.skip = resume.skip;
                }
            }
        }
        Ok(None)
    }

    /// Takes the page that was read as a dictionary.
    fn begin_dictionary(&mut self, column: &Column, header: &PageHeader) -> Option<()> {
        // A value takes some bytes, so there cannot be more than that.
        let least = match column.physical {
            Physical::Boolean => return None,
            Physical::Bytes => 4,
            other => other.width()?,
        };
        let count = header.values;
        if count.checked_mul(least)? > self.page.len()
            || !matches!(header.encoding, PLAIN | PLAIN_DICTIONARY)
        {
            return None;
        }
        let mut dictionary = Dictionary {
            page: core::mem::take(&mut self.page),
            left: count,
            ..Dictionary::default()
        };
        dictionary.rest.coding = Coding::Plain;
        dictionary.values.try_reserve_exact(count).ok()?;
        dictionary.status.try_reserve_exact(count).ok()?;
        if column.plan.is_text() {
            dictionary.codes.try_reserve_exact(count).ok()?;
        }
        self.dictionary = dictionary;
        Some(())
    }

    /// Reads some entries of the dictionary.
    fn read_entries(
        &mut self,
        column: &Column,
        scratch: &mut Scratch,
        work: &mut usize,
    ) -> Option<()> {
        let dictionary = &mut self.dictionary;
        for _ in 0..SLICE {
            let batch = dictionary.left.min(BATCH);
            let (seen, place) = decode(
                column,
                &dictionary.page,
                &mut dictionary.rest,
                batch,
                scratch,
            )?;
            // Texts that were written go behind those before them.
            let offset = match place {
                Place::Written => dictionary.text.len(),
                _ => 0,
            };
            for (value, status) in scratch.values.iter().zip(&scratch.status).take(batch) {
                let happened = if seen == KEPT { KEPT } else { *status };
                dictionary
                    .values
                    .push(value.wrapping_add(span(offset, offset)));
                dictionary.status.push(happened);
                if column.plan.is_text() {
                    dictionary.codes.push(NO_CODE);
                }
            }
            if matches!(place, Place::Written) {
                dictionary.text.push_str(&scratch.text);
            }
            dictionary.seen |= seen;
            dictionary.left = dictionary.left.saturating_sub(batch);
            *work = work.saturating_add(batch.saturating_mul(16));
        }
        Some(())
    }

    /// Finds out where the levels and the values of a page lie.
    fn begin(&mut self, column: &Column, header: &PageHeader) -> Result<(), Fault> {
        let end = self.page.len();
        let mut at = 0_usize;
        self.levels = Levels::All;
        match header.kind {
            PageKind::DataV2 => {
                at = header.repetition.saturating_add(header.definition);
                if column.optional {
                    self.levels = if header.definition > 0 {
                        Levels::Runs(Hybrid::new(header.repetition, at, 1))
                    } else if header.blanks == 0 {
                        Levels::All
                    } else if header.blanks == header.values {
                        Levels::None
                    } else {
                        return Err(Fault::Damaged(Damage::Levels));
                    };
                }
            }
            _ if column.optional => match header.levels {
                RLE => {
                    let len = length(&self.page, 0).ok_or(Fault::Damaged(Damage::Levels))?;
                    at = len.checked_add(4).ok_or(Fault::Damaged(Damage::Levels))?;
                    self.levels = Levels::Runs(Hybrid::new(4, at, 1));
                }
                BIT_PACKED => {
                    at = header.values.div_ceil(8);
                    self.levels = Levels::Bits(0);
                }
                _ => return Err(Fault::Damaged(Damage::Levels)),
            },
            _ => {}
        }
        if at > end {
            return Err(Fault::Damaged(Damage::Levels));
        }
        // Where every row is blank, there may be nothing at all.
        self.values = if at == end {
            Values::default()
        } else {
            self.begin_values(column, header, at)
                .ok_or(Fault::Damaged(Damage::Encoding))?
        };
        Ok(())
    }

    fn begin_values(&self, column: &Column, header: &PageHeader, at: usize) -> Option<Values> {
        let page = self.page.as_slice();
        let end = page.len();
        let physical = column.physical;
        let coding = match (header.encoding, physical) {
            (PLAIN, _) => Coding::Plain,
            (PLAIN_DICTIONARY | RLE_DICTIONARY, _) if self.dictionary_at > 0 => Coding::Codes,
            (RLE, Physical::Boolean) => Coding::Runs,
            (DELTA_BINARY_PACKED, Physical::Int32 | Physical::Int64) => Coding::Differences,
            (DELTA_LENGTH_BYTE_ARRAY, Physical::Bytes) => Coding::Lengths,
            (DELTA_BYTE_ARRAY, Physical::Bytes | Physical::Fixed(_)) => Coding::Parts,
            (BYTE_STREAM_SPLIT, Physical::Boolean | Physical::Int96 | Physical::Bytes) => {
                return None;
            }
            (BYTE_STREAM_SPLIT, _) => Coding::Split,
            _ => return None,
        };
        let mut values = Values {
            coding,
            at,
            bit: u64::try_from(at).ok()?.saturating_mul(8),
            ..Values::default()
        };
        let mut deltas = 0;
        match coding {
            Coding::Codes => {
                let width = u32::from(*page.get(at)?);
                if width > 32 {
                    return None;
                }
                values.runs = Hybrid::new(at.saturating_add(1), end, width);
            }
            Coding::Runs => {
                let start = at.saturating_add(4);
                let stop = start.saturating_add(length(page, at)?);
                if stop > end {
                    return None;
                }
                values.runs = Hybrid::new(start, stop, 1);
            }
            Coding::Differences | Coding::Lengths => deltas = 1,
            Coding::Parts => deltas = 2,
            Coding::Split => {
                let (len, width) = (end.saturating_sub(at), physical.width()?);
                if len.checked_rem(width)? != 0 {
                    return None;
                }
                values.count = len.checked_div(width)?;
            }
            Coding::None | Coding::Plain => {}
        }
        for delta in values.deltas.iter_mut().take(deltas) {
            *delta = Delta::new(page, values.at, end, header.values)?;
            values.at = delta.after(page)?;
        }
        Some(values)
    }

    /// Gives at most `most` rows. Gives how many they were.
    fn rows(
        &mut self,
        column: &Column,
        scratch: &mut Scratch,
        most: usize,
        out: &mut Out<'_>,
        work: &mut usize,
    ) -> Option<(usize, Option<Problem>)> {
        let (rows, count) = read_levels(&mut self.levels, &self.page, most, scratch)?;
        *work = work.saturating_add(rows.saturating_mul(8));
        let mut known = None;
        let mut place = Place::Page;
        let mut seen = KEPT;
        if count > 0 {
            if self.values.coding == Coding::Codes {
                let cells = scratch.values.get_mut(..count)?;
                self.values.runs.read(&self.page, cells)?;
                let slots = scratch.status.iter_mut().zip(&mut scratch.lens);
                for (cell, (happened, index)) in cells.iter_mut().zip(slots) {
                    *index = *cell;
                    let code = usize::try_from(*cell).ok()?;
                    *cell = *self.dictionary.values.get(code)?;
                    *happened = *self.dictionary.status.get(code)?;
                }
                seen = self.dictionary.seen;
                if column.plan != Plan::Text {
                    known = Some(self.dictionary.text.as_str());
                }
            } else {
                (seen, place) = decode(column, &self.page, &mut self.values, count, scratch)?;
                if matches!(place, Place::Written) {
                    known = Some(scratch.text.as_str());
                }
            }
        }
        let values = scratch.values.get(..count)?;
        let statuses = scratch.status.get(..count)?;
        let is_text = column.plan.is_text();
        if seen & TOO_WIDE != 0 && statuses.iter().any(|status| status & TOO_WIDE != 0) {
            return Some((rows, Some(Problem::TooWide)));
        }
        let is_float = column.plan.is_float();
        if !is_text {
            let Out::Cells(cells) = out else {
                return Some((rows, None));
            };
            if count == rows && seen == KEPT && !is_float {
                for value in values {
                    cells.push_int(value.cast_signed());
                }
                return Some((rows, None));
            }
        }
        let bytes = if self.values.coding == Coding::Codes {
            self.dictionary.page.as_slice()
        } else if matches!(place, Place::Page) {
            &self.page
        } else {
            &scratch.bytes
        };
        // A text of a dictionary is looked for in the table only once.
        let coded = is_text && self.values.coding == Coding::Codes;
        let mut values = values.iter().zip(statuses).zip(&scratch.lens);
        for row in 0..rows {
            let here = count == rows || scratch.levels.get(row).is_some_and(|level| *level != 0);
            let mut happened = KEPT;
            let mut value = 0;
            let mut text = "";
            let mut entry = None;
            if here {
                let ((next, status), index) = values.next()?;
                if coded && let Out::Cells(cells) = out {
                    entry = usize::try_from(*index)
                        .ok()
                        .and_then(|index| self.dictionary.codes.get_mut(index));
                    if self.codes_good
                        && let Some(code) = entry.as_deref()
                        && *code != NO_CODE
                    {
                        cells.push_code(*code);
                        continue;
                    }
                }
                value = *next;
                if seen != KEPT {
                    happened = *status;
                }
                if is_text {
                    text = match known {
                        Some(known) => {
                            let (from, to) = (value >> 32, value & 0xffff_ffff);
                            known.get(usize::try_from(from).ok()?..usize::try_from(to).ok()?)?
                        }
                        None => match whole_text(at_span(bytes, value)?) {
                            Some(text) => text,
                            None => return Some((rows, Some(Problem::NotText))),
                        },
                    };
                    *work = work.saturating_add(text.len());
                }
            }
            match out {
                Out::Nowhere => {}
                Out::Sample(sample) => {
                    if here {
                        sample.look_at(text);
                    }
                }
                Out::Cells(cells) => {
                    if happened != KEPT {
                        self.counts.note(happened);
                    }
                    if !here || happened & BLANK != 0 || (is_text && text.is_empty()) {
                        self.counts.blanks = self.counts.blanks.saturating_add(1);
                        cells.push_blank();
                    } else if is_text {
                        if let Some(code) = cells.code_of(text) {
                            // Codes are given one after another.
                            if code >= self.entries {
                                self.entries = code.saturating_add(1);
                                self.gained = self.gained.saturating_add(text.len());
                            }
                            if let Some(entry) = entry {
                                *entry = code;
                                self.codes_good = true;
                            }
                            cells.push_code(code);
                        } else if cells.push_text(text) {
                            self.counts.cut_texts = self.counts.cut_texts.saturating_add(1);
                            // It may be in the dictionary now.
                            self.gained = self.gained.saturating_add(text.len());
                        }
                        if self.text_in(cells) > MAX_GROUP_TEXT {
                            return Some((rows, Some(Problem::TooMuchText)));
                        }
                    } else if is_float {
                        cells.push_float(f64::from_bits(value));
                    } else {
                        cells.push_int(value.cast_signed());
                    }
                }
            }
        }
        Some((rows, None))
    }
}

impl Counts {
    /// Counts what happened to a value.
    fn note(&mut self, happened: u8) {
        let one = |flag: u8| u64::from(happened & flag != 0);
        self.cut_nanos = self.cut_nanos.saturating_add(one(CUT_NANOS));
        self.not_finite = self.not_finite.saturating_add(one(NOT_FINITE));
        self.out_of_range = self.out_of_range.saturating_add(one(OUT_OF_RANGE));
    }
}

/// The text that bytes are. Of one that is too long for a cell, only so much
/// as shows it.
fn whole_text(bytes: &[u8]) -> Option<&str> {
    if bytes.len() <= MAX_TEXT_BYTES {
        return core::str::from_utf8(bytes).ok();
    }
    let start = bytes.get(..MAX_LOOK).unwrap_or(bytes);
    match core::str::from_utf8(start) {
        Ok(text) => Some(text),
        // Cut within a character, after the last one that can be shown.
        Err(error) if error.error_len().is_none() && start.len() == MAX_LOOK => start
            .get(..error.valid_up_to())
            .and_then(|valid| core::str::from_utf8(valid).ok()),
        Err(_) => None,
    }
}

/// Reads levels: for how many rows, and how many of them have values. When
/// only some do, `Scratch::levels` says which.
fn read_levels(
    levels: &mut Levels,
    page: &[u8],
    most: usize,
    scratch: &mut Scratch,
) -> Option<(usize, usize)> {
    let slots = scratch.levels.get_mut(..most)?;
    match levels {
        Levels::All => return Some((most, most)),
        Levels::None => slots.fill(0),
        Levels::Runs(runs) => {
            if let Some((level, rows)) = runs.same(page, LONG_RUN, most)? {
                slots.fill(0);
                return Some((rows, if level == 0 { 0 } else { rows }));
            }
            runs.read(page, slots)?;
        }
        Levels::Bits(bit) => {
            for slot in slots.iter_mut() {
                let byte = page.get(usize::try_from(*bit >> 3).ok()?)?;
                let shift = u32::try_from(*bit & 7).ok()?;
                *slot = u64::from(byte.wrapping_shl(shift) >> 7);
                *bit = bit.saturating_add(1);
            }
        }
    }
    let count: u64 = slots.iter().sum();
    Some((most, usize::try_from(count).ok()?))
}

/// Makes a buffer as long as that.
#[inline(never)]
fn fill_up(buffer: &mut Vec<u8>, len: usize) -> Result<(), Fault> {
    let more = len.saturating_sub(buffer.len());
    buffer.try_reserve_exact(more).map_err(|_| Fault::Memory)?;
    buffer.resize(len, 0);
    Ok(())
}

/// A length of four bytes.
fn length(bytes: &[u8], at: usize) -> Option<usize> {
    let four = bytes.get(at..)?.first_chunk::<4>()?;
    usize::try_from(u32::from_le_bytes(*four)).ok()
}

/// Where the texts of a batch are.
#[derive(Clone, Copy)]
enum Place {
    Page,
    /// Put together in `Scratch::bytes`.
    Copied,
    /// Written to `Scratch::text`.
    Written,
}

/// Reads values that are not in a dictionary, as what they become. Gives what
/// happened to them, and where they are if they are texts. When nothing
/// happened, their status means nothing.
fn decode(
    column: &Column,
    page: &[u8],
    values: &mut Values,
    count: usize,
    scratch: &mut Scratch,
) -> Option<(u8, Place)> {
    let (plan, physical) = (column.plan, column.physical);
    let mut place = Place::Page;
    if physical.is_number() {
        let out = scratch.values.get_mut(..count)?;
        decode_numbers(physical, page, values, out)?;
        if plan == Plan::Int && physical == Physical::Int32 {
            for value in out {
                *value = value::sign_extended(*value).cast_unsigned();
            }
        }
    } else {
        place = decode_spans(physical, page, values, count, scratch)?;
    }
    if matches!(plan, Plan::Int | Plan::Unsigned32 | Plan::Text) {
        return Some((KEPT, place));
    }
    let base = match place {
        Place::Page => page,
        _ => scratch.bytes.as_slice(),
    };
    scratch.text.clear();
    let mut seen = KEPT;
    let slots = scratch.values.get_mut(..count)?.iter_mut();
    for (value, status) in slots.zip(&mut scratch.status) {
        let from = scratch.text.len();
        let bytes = if physical.is_number() {
            &[]
        } else {
            at_span(base, *value)?
        };
        (*value, *status) = value::convert(plan, physical, *value, bytes, &mut scratch.text);
        if plan.is_text() {
            *value = span(from, scratch.text.len());
            place = Place::Written;
        }
        seen |= *status;
    }
    Some((seen, place))
}

/// Reads numbers as they are stored.
fn decode_numbers(
    physical: Physical,
    page: &[u8],
    values: &mut Values,
    out: &mut [u64],
) -> Option<()> {
    let [delta, _] = &mut values.deltas;
    match values.coding {
        Coding::Plain => {
            let width = match physical.width() {
                Some(4) => 32,
                Some(_) => 64,
                None => 1,
            };
            unpack(page, values.bit, width, out)?;
            let count = u64::try_from(out.len()).unwrap_or(0);
            let bits = count.saturating_mul(u64::from(width));
            values.bit = values.bit.saturating_add(bits);
        }
        Coding::Runs => values.runs.read(page, out)?,
        Coding::Differences => {
            delta.read(page, out)?;
            if physical == Physical::Int32 {
                for slot in out {
                    *slot &= 0xffff_ffff;
                }
            }
        }
        Coding::Split => {
            let width = physical.width()?;
            for slot in out {
                if values.next >= values.count {
                    return None;
                }
                let mut from = values.at.saturating_add(values.next);
                let mut value = 0;
                for shift in (0_u32..).step_by(8).take(width) {
                    value |= u64::from(*page.get(from)?).wrapping_shl(shift);
                    from = from.saturating_add(values.count);
                }
                *slot = value;
                values.next = values.next.saturating_add(1);
            }
        }
        _ => return None,
    }
    Some(())
}

/// Finds out where the next values lie that are bytes.
fn decode_spans(
    physical: Physical,
    page: &[u8],
    values: &mut Values,
    count: usize,
    scratch: &mut Scratch,
) -> Option<Place> {
    let spans = scratch.values.get_mut(..count)?;
    let lens = scratch.lens.get_mut(..count)?;
    let width = physical.width();
    let Values { at, before, .. } = values;
    let bytes = &mut scratch.bytes;
    bytes.clear();
    let [first, second] = &mut values.deltas;
    match values.coding {
        Coding::Plain => {
            for slot in spans {
                let mut from = *at;
                let len = match width {
                    Some(width) => width,
                    None => {
                        from = from.saturating_add(4);
                        length(page, *at)?
                    }
                };
                *at = from.saturating_add(len);
                *slot = span(from, *at);
            }
            (*at <= page.len()).then_some(Place::Page)
        }
        Coding::Lengths => {
            first.read(page, lens)?;
            for (slot, len) in spans.iter_mut().zip(lens) {
                let from = *at;
                *at = at.saturating_add(usize::try_from(*len).unwrap_or(usize::MAX));
                *slot = span(from, *at);
                if *at > page.len() {
                    return None;
                }
            }
            Some(Place::Page)
        }
        Coding::Parts => {
            first.read(page, spans)?;
            second.read(page, lens)?;
            for (slot, len) in spans.iter_mut().zip(lens) {
                let share = usize::try_from(*slot).unwrap_or(usize::MAX);
                let end = at.saturating_add(usize::try_from(*len).unwrap_or(usize::MAX));
                let rest = page.get(*at..end)?;
                *at = end;
                if share > before.len() {
                    return None;
                }
                before.truncate(share);
                before.try_reserve(rest.len()).ok()?;
                before.extend_from_slice(rest);
                if width.is_some_and(|width| width != before.len()) {
                    return None;
                }
                let from = bytes.len();
                bytes.extend_from_slice(before.get(..MAX_LOOK).unwrap_or(before));
                *slot = span(from, bytes.len());
            }
            Some(Place::Copied)
        }
        Coding::Split => {
            for slot in spans {
                if values.next >= values.count {
                    return None;
                }
                let begin = bytes.len();
                let mut from = at.saturating_add(values.next);
                for _ in 0..width? {
                    bytes.push(*page.get(from)?);
                    from = from.saturating_add(values.count);
                }
                *slot = span(begin, bytes.len());
                values.next = values.next.saturating_add(1);
            }
            Some(Place::Copied)
        }
        _ => None,
    }
}
