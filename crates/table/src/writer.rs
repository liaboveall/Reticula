//! Writing a table.
//!
//! Rows are collected in memory until a group of them is full; the group is
//! then sealed and written one column at a time, so that no single call takes
//! long. What has been written is never touched again.

use alloc::string::String;
use alloc::vec::Vec;

use hashbrown::HashTable;
use sha2::{Digest, Sha256};

use crate::bits;
use crate::hash;
use crate::layout::{
    self, Block, Column, DictionaryPlace, Directory, Encoding, MAX_DICTIONARY_ENTRIES,
    MAX_DIRECTORY, MAX_FILE, MAX_GROUP_ROWS, MIN_GROUP_ROWS, PAGE, Trailer,
};
use crate::source::Sink;
use crate::types::{ColumnType, MAX_COLUMNS, MAX_NAME_BYTES, MAX_ROWS, MAX_TEXT_BYTES};

/// Bytes the values of a group may take in memory, about.
const GROUP_BUDGET: usize = 64 << 20;
/// Bytes of the texts of one column in one group.
const MAX_GROUP_TEXT: usize = 128 << 20;
/// Bytes all the dictionaries may take in memory together, about.
const DICTIONARY_BUDGET: usize = 128 << 20;
/// How much is collected before it is handed to the sink.
const FLUSH_AT: usize = 1 << 20;

/// Why a table cannot be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The sink took no more.
    Sink,
    /// A limit of the format, or of this machine, was reached.
    Limit(&'static str),
    /// The texts of a group are too long to collect: there have to be fewer
    /// rows in a group.
    GroupTooLarge,
    /// The columns of a group do not have the same number of rows.
    Uneven,
}

/// A column to be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    /// Its name.
    pub name: String,
    /// What it holds. `None` until a value decides; until then there can
    /// only be blank cells.
    pub kind: Option<ColumnType>,
    /// Whether a text column should have a dictionary.
    pub dictionary: bool,
}

/// How many rows a group should have, when a row takes about `row_bytes` bytes.
#[must_use]
pub fn group_rows_for(columns: usize, row_bytes: usize) -> u32 {
    let row = row_bytes.max(columns.saturating_mul(8)).max(1);
    let rows = GROUP_BUDGET.checked_div(row).unwrap_or(0);
    let rows = u32::try_from(rows).unwrap_or(u32::MAX);
    let fits = if rows == 0 {
        MIN_GROUP_ROWS
    } else {
        // The largest power of two that is not above.
        1_u32.wrapping_shl(rows.ilog2())
    };
    fits.clamp(MIN_GROUP_ROWS, MAX_GROUP_ROWS)
}

/// Where a text is in a [`DictionaryBuilder`].
#[derive(Clone, Copy, Debug)]
struct Entry {
    code: u32,
    /// Part of the text's hash. It tells most texts apart without looking at
    /// them, and is all that is needed when the table of entries grows.
    hash: u32,
}

impl Entry {
    /// What the entry is found by.
    const fn spread(hash: u32) -> u64 {
        (hash as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }
}

/// The distinct texts of a column, numbered as they come.
#[derive(Debug, Default)]
struct DictionaryBuilder {
    entries: HashTable<Entry>,
    ends: Vec<u32>,
    bytes: Vec<u8>,
    /// Bytes there were when the group began, and how many the last one added.
    before: usize,
    growth: usize,
    /// Whether many different texts came that cannot be told apart by their
    /// hashes. Finding any of them would take longer with every one.
    flooded: bool,
}

/// Texts with the same hash that are looked at to find one.
const MOST_ALIKE: u32 = 32;

impl DictionaryBuilder {
    fn text<'a>(ends: &[u32], bytes: &'a [u8], code: u32) -> &'a [u8] {
        let end = |code: u32| ends.get(code as usize).map(|end| *end as usize);
        let start = code.checked_sub(1).and_then(end).unwrap_or(0);
        end(code)
            .and_then(|end| bytes.get(start..end))
            .unwrap_or(&[])
    }

    /// The code of a text, which becomes a new entry when it is not one yet.
    fn code(&mut self, text: &[u8]) -> u32 {
        let hash = u32::try_from(hash::of_text(text).wrapping_shr(32)).unwrap_or(0);
        let spread = Entry::spread(hash);
        let (ends, bytes) = (&self.ends, &self.bytes);
        let mut alike = 0_u32;
        let found = self.entries.find(spread, |entry| {
            if entry.hash != hash {
                return false;
            }
            alike = alike.saturating_add(1);
            alike > MOST_ALIKE || Self::text(ends, bytes, entry.code) == text
        });
        match found {
            Some(entry) if alike <= MOST_ALIKE => return entry.code,
            _ => {}
        }
        let code = u32::try_from(self.ends.len()).unwrap_or(u32::MAX);
        // A quarter more at a time: twice as much would be a lot of memory
        // for what may be the last texts that fit.
        if self.bytes.capacity().saturating_sub(self.bytes.len()) < text.len() {
            let more = self.bytes.len().checked_div(4).unwrap_or(0);
            self.bytes.reserve_exact(text.len().max(more).max(4_096));
        }
        self.bytes.extend_from_slice(text);
        self.ends
            .push(u32::try_from(self.bytes.len()).unwrap_or(u32::MAX));
        if alike > MOST_ALIKE {
            // An entry of its own, whether or not there is one like it
            // already: two that are the same do no harm. It is not looked for
            // again, and neither is any text after this group.
            self.flooded = true;
        } else {
            self.entries
                .insert_unique(spread, Entry { code, hash }, |entry| {
                    Entry::spread(entry.hash)
                });
        }
        code
    }

    /// Bytes this will take in memory after another group like the last one, about.
    fn memory(&self) -> usize {
        self.bytes
            .len()
            .saturating_add(self.growth)
            .saturating_add(self.ends.len().saturating_mul(4))
            .saturating_add(self.entries.len().saturating_mul(10))
    }
}

/// The cells of one column, for the group that is being collected.
#[derive(Debug)]
pub struct ColumnBuilder {
    name: String,
    kind: Option<ColumnType>,
    rows: u32,
    nulls: Vec<u64>,
    blanks: u32,
    all_blanks: u64,
    /// Integers, or codes of the dictionary.
    ints: Vec<i64>,
    floats: Vec<f64>,
    ends: Vec<u32>,
    texts: Vec<u8>,
    dictionary: Option<DictionaryBuilder>,
    /// Whether new texts still go into the dictionary. Changes between groups only.
    dictionary_open: bool,
}

impl ColumnBuilder {
    fn new(spec: ColumnSpec) -> Self {
        Self {
            name: spec.name,
            kind: spec.kind,
            rows: 0,
            nulls: Vec::new(),
            blanks: 0,
            all_blanks: 0,
            ints: Vec::new(),
            floats: Vec::new(),
            ends: Vec::new(),
            texts: Vec::new(),
            dictionary: spec.dictionary.then(DictionaryBuilder::default),
            dictionary_open: spec.dictionary,
        }
    }

    /// What the column holds, once that is known.
    #[must_use]
    pub const fn kind(&self) -> Option<ColumnType> {
        self.kind
    }

    /// Decides what the column holds. Only while nothing but blank cells came.
    pub fn set_kind(&mut self, kind: ColumnType) {
        if self.kind.is_none() {
            self.kind = Some(kind);
        }
    }

    /// Says whether every value of a decimal column was written with all of
    /// its decimals.
    pub const fn set_fixed(&mut self, all: bool) {
        if let Some(ColumnType::Decimal { scale, .. }) = self.kind {
            self.kind = Some(ColumnType::Decimal { scale, fixed: all });
        }
    }

    /// Rows collected for this group.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.rows
    }

    /// Whether no row was collected for this group.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Bytes of text collected for this group.
    #[must_use]
    pub const fn text_bytes(&self) -> usize {
        self.texts.len()
    }

    /// The next cell is blank.
    pub fn push_blank(&mut self) {
        let word = (self.rows >> 6) as usize;
        if self.nulls.len() <= word {
            self.nulls.resize(word.saturating_add(1), 0);
        }
        if let Some(bits) = self.nulls.get_mut(word) {
            *bits |= 1_u64.wrapping_shl(self.rows & 63);
        }
        self.rows = self.rows.saturating_add(1);
        self.blanks = self.blanks.saturating_add(1);
        // Something has to stand in its place. The value before it costs
        // nothing; where there is none yet, the first value fills in.
        match self.kind {
            None => {}
            Some(ColumnType::Float) => {
                if let Some(last) = self.floats.last().copied() {
                    self.floats.push(last);
                }
            }
            Some(ColumnType::Text) if !self.dictionary_open => {
                let end = self.ends.last().copied().unwrap_or(0);
                self.ends.push(end);
            }
            Some(_) => {
                if let Some(last) = self.ints.last().copied() {
                    self.ints.push(last);
                }
            }
        }
    }

    fn push_integral(&mut self, value: i64) {
        if self.ints.len() < self.rows as usize {
            self.ints.resize(self.rows as usize, value);
        }
        self.ints.push(value);
        self.rows = self.rows.saturating_add(1);
    }

    /// The next cell is the value an integer stands for. A column that does
    /// not hold such takes it as a blank.
    pub fn push_int(&mut self, value: i64) {
        if self.kind.is_some_and(ColumnType::is_integral) {
            self.push_integral(value);
        } else {
            self.push_blank();
        }
    }

    /// The next cell is a floating-point number. One that is not finite, or
    /// that comes to a column of another type, is taken as a blank.
    pub fn push_float(&mut self, value: f64) {
        if self.kind != Some(ColumnType::Float) || !value.is_finite() {
            self.push_blank();
            return;
        }
        if self.floats.len() < self.rows as usize {
            self.floats.resize(self.rows as usize, value);
        }
        // There is one zero: its sign would otherwise make two of them.
        self.floats.push(if value == 0.0 { 0.0 } else { value });
        self.rows = self.rows.saturating_add(1);
    }

    /// The next cell is a text. The empty text is a blank, and so is any text
    /// that comes to a column of another type. What is over
    /// [`MAX_TEXT_BYTES`] is cut; gives whether it was.
    pub fn push_text(&mut self, text: &str) -> bool {
        let mut cut = text;
        while cut.len() > MAX_TEXT_BYTES {
            let mut end = MAX_TEXT_BYTES;
            while !text.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            cut = text.get(..end).unwrap_or("");
        }
        if self.kind != Some(ColumnType::Text) || cut.is_empty() {
            self.push_blank();
            return false;
        }
        if self.dictionary_open
            && let Some(dictionary) = &mut self.dictionary
        {
            let code = dictionary.code(cut.as_bytes());
            self.push_integral(i64::from(code));
        } else {
            if self.ends.len() < self.rows as usize {
                let end = self.ends.last().copied().unwrap_or(0);
                self.ends.resize(self.rows as usize, end);
            }
            self.texts.extend_from_slice(cut.as_bytes());
            self.ends
                .push(u32::try_from(self.texts.len()).unwrap_or(u32::MAX));
            self.rows = self.rows.saturating_add(1);
        }
        cut.len() < text.len()
    }

    /// The code of a text in the column's dictionary, for
    /// [`ColumnBuilder::push_code`]. `None` when texts do not go into it, or
    /// this one is not to be pushed as it is. Codes are good for as long as
    /// this gives any. A text that is asked for has to be pushed as well: every
    /// entry of a dictionary occurs.
    pub fn code_of(&mut self, text: &str) -> Option<u32> {
        if self.kind != Some(ColumnType::Text) || text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return None;
        }
        let dictionary = self.dictionary.as_mut().filter(|_| self.dictionary_open)?;
        Some(dictionary.code(text.as_bytes()))
    }

    /// The next cell is the text with that code. A code that cannot be one is
    /// taken as a blank.
    pub fn push_code(&mut self, code: u32) {
        let known = self
            .dictionary
            .as_ref()
            .is_some_and(|dictionary| (code as usize) < dictionary.ends.len());
        if known && self.dictionary_open && self.kind == Some(ColumnType::Text) {
            self.push_integral(i64::from(code));
        } else {
            self.push_blank();
        }
    }

    /// Whether the texts of this group are too many bytes to go on.
    fn too_much_text(&self) -> bool {
        let added = self.dictionary.as_ref().map_or(0, |dictionary| {
            dictionary.bytes.len().saturating_sub(dictionary.before)
        });
        self.texts.len().saturating_add(added) > MAX_GROUP_TEXT
    }

    fn dictionary_memory(&self) -> usize {
        self.dictionary
            .as_ref()
            .map_or(0, DictionaryBuilder::memory)
    }

    /// Whether the dictionary has no room for another group, or finds nothing anymore.
    fn dictionary_full(&self) -> bool {
        self.dictionary.as_ref().is_some_and(|dictionary| {
            let entries = dictionary.ends.len();
            dictionary.flooded
                || entries.saturating_add(MAX_GROUP_ROWS as usize) > MAX_DICTIONARY_ENTRIES as usize
        })
    }

    /// Writes the group, and starts the next one.
    fn write(&mut self, out: &mut Appender) -> Block {
        let rows = self.rows as usize;
        let mut block = Block {
            blanks: self.blanks,
            ..Block::BLANK
        };
        if self.blanks < self.rows {
            match self.kind {
                None => {}
                Some(ColumnType::Float) => {
                    let mut low = f64::INFINITY;
                    let mut high = f64::NEG_INFINITY;
                    block.encoding = Encoding::Raw;
                    block.at = out.at();
                    for value in &self.floats {
                        low = low.min(*value);
                        high = high.max(*value);
                        out.bytes(&value.to_bits().to_le_bytes());
                    }
                    block.low = low.to_bits();
                    block.high = high.to_bits();
                }
                Some(ColumnType::Text) if !self.dictionary_open => {
                    block.encoding = Encoding::Text;
                    let width = bits::width_of(u64::try_from(self.texts.len()).unwrap_or(0));
                    block.width = u8::try_from(width).unwrap_or(0);
                    block.at = out.at();
                    bits::pack(
                        self.ends.iter().map(|end| u64::from(*end)),
                        width,
                        out.buffer(),
                    );
                    out.bytes(&self.texts);
                }
                Some(_) => {
                    block.encoding = Encoding::Packed;
                    block.at = out.at();
                    // Codes are numbers of entries and nothing else.
                    let codes = self.kind == Some(ColumnType::Text);
                    write_packed(&self.ints, codes, &mut block, out);
                }
            }
            block.len = u32::try_from(out.at().saturating_sub(block.at)).unwrap_or(u32::MAX);
            if self.blanks > 0 {
                block.nulls_at = out.at();
                self.nulls.resize(rows.div_ceil(64), 0);
                for word in &self.nulls {
                    out.bytes(&word.to_le_bytes());
                }
            }
        }
        self.all_blanks = self.all_blanks.saturating_add(u64::from(self.blanks));
        if let Some(dictionary) = &mut self.dictionary {
            dictionary.growth = dictionary.bytes.len().saturating_sub(dictionary.before);
            dictionary.before = dictionary.bytes.len();
        }
        self.rows = 0;
        self.blanks = 0;
        self.nulls.clear();
        self.ints.clear();
        self.floats.clear();
        self.ends.clear();
        self.texts.clear();
        block
    }
}

/// Whether ten to that power divides the distance of every value from the smallest.
fn divides(values: &[i64], low: i64, factor: u64) -> bool {
    values
        .iter()
        .all(|value| value.wrapping_sub(low).cast_unsigned().checked_rem(factor) == Some(0))
}

fn write_packed(values: &[i64], codes: bool, block: &mut Block, out: &mut Appender) {
    let mut low = i64::MAX;
    let mut high = i64::MIN;
    for value in values {
        low = low.min(*value);
        high = high.max(*value);
    }
    let span = high.wrapping_sub(low).cast_unsigned();
    // Times of day in whole seconds, prices in whole units: the zeros at the
    // end need not be stored.
    let mut unit = 0_u8;
    if span > 0 && !codes {
        for candidate in [9_u8, 6, 4, 3, 2, 1] {
            if divides(values, low, 10_u64.saturating_pow(u32::from(candidate))) {
                unit = candidate;
                break;
            }
        }
    }
    let factor = 10_u64.saturating_pow(u32::from(unit));
    let width = bits::width_of(span.checked_div(factor).unwrap_or(0));
    block.unit = unit;
    block.width = u8::try_from(width).unwrap_or(0);
    block.low = low.cast_unsigned();
    block.high = high.cast_unsigned();
    if unit == 0 {
        bits::pack(
            values
                .iter()
                .map(|value| value.wrapping_sub(low).cast_unsigned()),
            width,
            out.buffer(),
        );
    } else {
        bits::pack(
            values.iter().map(|value| {
                value
                    .wrapping_sub(low)
                    .cast_unsigned()
                    .checked_div(factor)
                    .unwrap_or(0)
            }),
            width,
            out.buffer(),
        );
    }
}

/// The file so far: where it ends, and what has not been handed on yet.
#[derive(Debug)]
struct Appender {
    /// Bytes that were handed on. Whole pages.
    flushed: u64,
    pending: Vec<u8>,
    sums: Vec<u32>,
}

impl Appender {
    fn at(&self) -> u64 {
        self.flushed
            .saturating_add(u64::try_from(self.pending.len()).unwrap_or(u64::MAX))
    }

    const fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.pending
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// Hands on the whole pages that have come together.
    fn flush<S: Sink>(&mut self, sink: &mut S, at_least: usize) -> Result<(), WriteError> {
        if self.pending.len() < at_least.max(PAGE) {
            return Ok(());
        }
        let (pages, rest) = self.pending.as_chunks::<PAGE>();
        let rest = rest.len();
        for page in pages {
            self.sums.push(hash::checksum(page));
        }
        let whole = self.pending.len().saturating_sub(rest);
        sink.append(self.pending.get(..whole).unwrap_or(&[]))
            .map_err(|_| WriteError::Sink)?;
        self.pending.copy_within(whole.., 0);
        self.pending.truncate(rest);
        self.flushed = self
            .flushed
            .saturating_add(u64::try_from(whole).unwrap_or(u64::MAX));
        if self.flushed > MAX_FILE {
            return Err(WriteError::Limit("the file would be too long"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Rows are collected.
    Collecting,
    /// A group is being written; the next column to write.
    Group(usize),
    /// Dictionaries that were closed are being written: of which column, from
    /// which byte on.
    Closing(usize, usize),
    /// The other dictionaries are being written, likewise.
    Dictionary(usize, usize),
    /// Everything is written.
    Done,
}

/// What became of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Rows.
    pub rows: u32,
    /// Length of the file.
    pub bytes: u64,
}

/// Writes one table.
#[derive(Debug)]
pub struct TableWriter {
    columns: Vec<ColumnBuilder>,
    group_rows: u32,
    rows: u64,
    blocks: Vec<Block>,
    places: Vec<Option<DictionaryPlace>>,
    out: Appender,
    stage: Stage,
    finishing: bool,
    /// Rows of the group that is being written.
    sealed: u32,
    bytes: u64,
}

impl TableWriter {
    /// Starts a table. `group_rows` comes from [`group_rows_for`].
    ///
    /// # Errors
    ///
    /// [`WriteError::Limit`] when there are no columns or too many.
    pub fn new(specs: Vec<ColumnSpec>, group_rows: u32) -> Result<Self, WriteError> {
        if specs.is_empty() || specs.len() > MAX_COLUMNS {
            return Err(WriteError::Limit("too many columns, or none"));
        }
        let mut columns = Vec::new();
        for mut spec in specs {
            while spec.name.len() > MAX_NAME_BYTES {
                spec.name.pop();
            }
            columns.push(ColumnBuilder::new(spec));
        }
        let mut pending = Vec::with_capacity(FLUSH_AT);
        pending.extend_from_slice(&layout::header());
        Ok(Self {
            places: alloc::vec![None; columns.len()],
            columns,
            group_rows: 1_u32
                .wrapping_shl(group_rows.max(1).ilog2())
                .clamp(MIN_GROUP_ROWS, MAX_GROUP_ROWS),
            rows: 0,
            blocks: Vec::new(),
            out: Appender {
                flushed: 0,
                pending,
                sums: Vec::new(),
            },
            stage: Stage::Collecting,
            finishing: false,
            sealed: 0,
            bytes: 0,
        })
    }

    /// Rows of a full group.
    #[must_use]
    pub const fn group_rows(&self) -> u32 {
        self.group_rows
    }

    /// Rows that were sealed so far.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// Bytes of the file so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        if self.stage == Stage::Done {
            self.bytes
        } else {
            self.out.at()
        }
    }

    /// Number of columns.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.columns.len()
    }

    /// A column, to give it its next cells.
    pub fn column(&mut self, index: usize) -> Option<&mut ColumnBuilder> {
        self.columns.get_mut(index)
    }

    /// All columns, to give each its next cell.
    pub fn columns(&mut self) -> &mut [ColumnBuilder] {
        &mut self.columns
    }

    /// Whether a group is complete, or has so much text that it is not to grow.
    ///
    /// # Errors
    ///
    /// [`WriteError::GroupTooLarge`] when there is too much text.
    pub fn check_group(&self) -> Result<bool, WriteError> {
        let Some(first) = self.columns.first() else {
            return Ok(false);
        };
        if self.columns.iter().any(ColumnBuilder::too_much_text) {
            return Err(WriteError::GroupTooLarge);
        }
        Ok(first.rows >= self.group_rows)
    }

    /// Ends the group. All columns have to have the same number of rows, and
    /// only the last group of the table may be short.
    ///
    /// Then [`TableWriter::step`] has to be called until nothing is left to do.
    ///
    /// # Errors
    ///
    /// [`WriteError`] when the group cannot be written.
    pub fn seal_group(&mut self) -> Result<(), WriteError> {
        if self.stage != Stage::Collecting {
            return Err(WriteError::Uneven);
        }
        let rows = self.columns.first().map_or(0, ColumnBuilder::len);
        if rows == 0 {
            return Ok(());
        }
        let short = self
            .rows
            .checked_rem(u64::from(self.group_rows))
            .unwrap_or(0)
            != 0;
        if short || rows > self.group_rows || self.columns.iter().any(|column| column.rows != rows)
        {
            return Err(WriteError::Uneven);
        }
        if self.check_group().is_err() {
            return Err(WriteError::GroupTooLarge);
        }
        self.rows = self.rows.saturating_add(u64::from(rows));
        if self.rows > MAX_ROWS {
            return Err(WriteError::Limit("too many rows"));
        }
        let groups = u64::try_from(self.blocks.len()).unwrap_or(u64::MAX);
        if groups.saturating_mul(48) > MAX_DIRECTORY {
            return Err(WriteError::Limit("too many rows and columns"));
        }
        self.sealed = rows;
        self.stage = Stage::Group(0);
        Ok(())
    }

    /// Does the next piece of writing. Gives whether something is left to do.
    ///
    /// # Errors
    ///
    /// [`WriteError`] when it could not be done. The table is then of no use.
    pub fn step<S: Sink>(&mut self, sink: &mut S) -> Result<bool, WriteError> {
        match self.stage {
            Stage::Collecting if self.finishing => {
                if self.columns.iter().any(|column| !column.is_empty()) {
                    return Err(WriteError::Uneven);
                }
                self.stage = Stage::Dictionary(0, 0);
            }
            Stage::Collecting | Stage::Done => {}
            Stage::Group(index) => {
                if let Some(column) = self.columns.get_mut(index) {
                    if column.rows != self.sealed {
                        return Err(WriteError::Uneven);
                    }
                    let block = column.write(&mut self.out);
                    if block.len > layout::MAX_BLOCK {
                        return Err(WriteError::GroupTooLarge);
                    }
                    self.blocks
                        .try_reserve(1)
                        .map_err(|_| WriteError::Limit("not enough memory"))?;
                    self.blocks.push(block);
                    self.stage = Stage::Group(index.saturating_add(1));
                }
                if index.saturating_add(1) >= self.columns.len() {
                    self.close_dictionaries();
                    self.stage = self
                        .next_dictionary(0, true)
                        .map_or(Stage::Collecting, |next| Stage::Closing(next, 0));
                }
                self.out.flush(sink, FLUSH_AT)?;
            }
            Stage::Closing(index, from) => {
                self.stage = match self.write_dictionary(index, from, sink)? {
                    Some(from) => Stage::Closing(index, from),
                    None => self
                        .next_dictionary(index, true)
                        .map_or(Stage::Collecting, |next| Stage::Closing(next, 0)),
                };
            }
            Stage::Dictionary(index, from) => {
                if let Some(index) = self.next_dictionary(index, false) {
                    self.stage = match self.write_dictionary(index, from, sink)? {
                        Some(from) => Stage::Dictionary(index, from),
                        None => Stage::Dictionary(index, 0),
                    };
                } else {
                    self.write_directory(sink)?;
                }
            }
        }
        Ok(match self.stage {
            Stage::Collecting => self.finishing,
            Stage::Done => false,
            Stage::Group(_) | Stage::Closing(..) | Stage::Dictionary(..) => true,
        })
    }

    /// Between two groups, closes the dictionaries that have become too large.
    /// They are written at once, so that only open ones take memory.
    fn close_dictionaries(&mut self) {
        for column in &mut self.columns {
            if column.dictionary_open && column.dictionary_full() {
                column.dictionary_open = false;
            }
        }
        loop {
            let open = || {
                self.columns
                    .iter()
                    .filter(|column| column.dictionary_open)
                    .map(ColumnBuilder::dictionary_memory)
            };
            if open().fold(0, usize::saturating_add) <= DICTIONARY_BUDGET {
                break;
            }
            let largest = open().max().unwrap_or(0);
            let column = self
                .columns
                .iter_mut()
                .find(|column| column.dictionary_open && column.dictionary_memory() == largest);
            match column {
                Some(column) if largest > 0 => column.dictionary_open = false,
                _ => break,
            }
        }
    }

    /// The column whose dictionary is written next: from that one on, closed
    /// ones only, or all.
    fn next_dictionary(&self, from: usize, closed: bool) -> Option<usize> {
        self.columns
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, column)| {
                let some = column
                    .dictionary
                    .as_ref()
                    .is_some_and(|dictionary| !dictionary.ends.is_empty());
                some && !(closed && column.dictionary_open)
            })
            .map(|(index, _)| index)
    }

    /// Writes the next piece of a dictionary. Gives where the one after it begins.
    fn write_dictionary<S: Sink>(
        &mut self,
        index: usize,
        from: usize,
        sink: &mut S,
    ) -> Result<Option<usize>, WriteError> {
        let Some(dictionary) = self
            .columns
            .get_mut(index)
            .and_then(|column| column.dictionary.as_mut())
        else {
            return Ok(None);
        };
        if from == 0 {
            // What finds the texts is not needed anymore.
            dictionary.entries = HashTable::new();
            let bytes_len = u64::try_from(dictionary.bytes.len()).unwrap_or(u64::MAX);
            let width = bits::width_of(bytes_len);
            let ends_at = self.out.at();
            bits::pack(
                dictionary.ends.iter().map(|end| u64::from(*end)),
                width,
                self.out.buffer(),
            );
            let place = DictionaryPlace {
                entries: u32::try_from(dictionary.ends.len()).unwrap_or(u32::MAX),
                ends_width: u8::try_from(width).unwrap_or(0),
                ends_at,
                bytes_at: self.out.at(),
                bytes_len,
            };
            if let Some(slot) = self.places.get_mut(index) {
                *slot = Some(place);
            }
        }
        let to = from.saturating_add(FLUSH_AT).min(dictionary.bytes.len());
        self.out
            .bytes(dictionary.bytes.get(from..to).unwrap_or(&[]));
        let more = to < dictionary.bytes.len();
        if !more {
            *dictionary = DictionaryBuilder::default();
        }
        self.out.flush(sink, FLUSH_AT)?;
        Ok(more.then_some(to))
    }

    fn write_directory<S: Sink>(&mut self, sink: &mut S) -> Result<(), WriteError> {
        let pad = self.out.pending.len().next_multiple_of(PAGE);
        self.out.pending.resize(pad, 0);
        self.out.flush(sink, PAGE)?;
        let directory = Directory {
            group_rows: self.group_rows,
            rows: u32::try_from(self.rows).map_err(|_| WriteError::Limit("too many rows"))?,
            columns: self
                .columns
                .iter_mut()
                .zip(&self.places)
                .map(|(column, place)| Column {
                    name: core::mem::take(&mut column.name),
                    kind: column.kind.unwrap_or(ColumnType::Text),
                    blanks: column.all_blanks,
                    dictionary: *place,
                })
                .collect(),
            blocks: core::mem::take(&mut self.blocks),
            sums: core::mem::take(&mut self.out.sums),
            data_len: self.out.flushed,
        };
        if directory.encoded_len() > MAX_DIRECTORY {
            return Err(WriteError::Limit("too many rows and columns"));
        }
        let mut bytes = Vec::new();
        directory.encode(&mut bytes);
        let trailer = Trailer {
            directory_at: directory.data_len,
            directory_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            directory_sha256: Sha256::digest(&bytes).into(),
        };
        trailer.encode(&mut bytes);
        sink.append(&bytes).map_err(|_| WriteError::Sink)?;
        self.bytes = trailer.file_len();
        self.stage = Stage::Done;
        Ok(())
    }

    /// Says that there are no more rows. The last group has to be sealed, and
    /// [`TableWriter::step`] called until nothing is left to do.
    pub const fn finish(&mut self) {
        self.finishing = true;
    }

    /// What was written, once everything is.
    #[must_use]
    pub fn written(&self) -> Option<Written> {
        (self.stage == Stage::Done).then(|| Written {
            rows: u32::try_from(self.rows).unwrap_or(u32::MAX),
            bytes: self.bytes,
        })
    }
}
