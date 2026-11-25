//! Writing a table from delimited text.

use alloc::string::String;
use alloc::vec::Vec;

use super::column::{Column, Fields, specs};
use super::infer::Seen;
use super::sniff::{Head, Sample, valid_start};
use super::split::{Reached, SplitError, Splitter};
use super::{HEAD_BYTES, ImportError, MAX_WAITING, Options};
use crate::layout::{MAX_GROUP_ROWS, MIN_GROUP_ROWS};
use crate::source::Sink;
use crate::types::{ColumnType, MAX_COLUMNS, MAX_ROWS, MAX_SCALE};
use crate::writer::{TableWriter, WriteError, Written, group_rows_for};

/// How much is done at a time. Always the same, but in tests.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// Most bytes that one step parses.
    pub(super) step_bytes: u64,
    /// About as many fields does one step parse at most.
    pub(super) step_fields: usize,
    /// Most bytes of the file that the rows of one group are read from, about.
    /// What a group takes in memory is in proportion to that.
    pub(super) group_bytes: u64,
}

const LIMITS: Limits = Limits {
    step_bytes: 256 << 10,
    step_fields: 128 << 10,
    group_bytes: 128 << 20,
};

/// How many sizes of groups there are.
const GROUP_SIZES: usize = (MAX_GROUP_ROWS / MIN_GROUP_ROWS).ilog2() as usize + 1;

/// How much of the file groups of one size are read from.
#[derive(Clone, Copy, Debug, Default)]
struct Extent {
    /// Which group is being read.
    group: u64,
    /// Where in the file it begins at the earliest.
    start: u64,
    /// Bytes of the longest group, or a little more.
    longest: u64,
}

/// What a step came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// More of the file has to be given, or that it has ended.
    NeedInput,
    /// There is more to do.
    Working,
    /// What was written is to be thrown away, and the file to be given again
    /// from its start.
    Restart,
    /// The table is written.
    Done,
}

/// How far an import is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Rows that were read, in this pass.
    pub rows: u64,
    /// Bytes of the file that were read, in this pass.
    pub bytes: u64,
    /// The pass, from 1 on.
    pub pass: u32,
}

/// What became of a column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnReport {
    /// Its name.
    pub name: String,
    /// What it holds.
    pub kind: ColumnType,
    /// Blank cells, the misfits among them.
    pub blanks: u64,
    /// Values that are blank because they did not fit the type that was asked for.
    pub misfits: u64,
    /// Texts that were cut because they are too long.
    pub cut_texts: u64,
}

/// What became of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Rows of the table.
    pub rows: u64,
    /// Bytes of the table's file.
    pub bytes: u64,
    /// The columns.
    pub columns: Vec<ColumnReport>,
    /// Fields that were dropped because their row has too many.
    pub extra_fields: u64,
    /// Rows that were filled up with blank cells.
    pub short_rows: u64,
    /// Lines that were skipped because there is nothing on them.
    pub empty_lines: u64,
    /// How often the file was read.
    pub passes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Reading,
    /// A group is being written, or, after the last one, the rest.
    Writing {
        last: bool,
    },
    Done,
}

/// Imports one file.
#[derive(Debug)]
pub struct Import {
    delimiter: u8,
    header: bool,
    markers: bool,
    columns: Vec<Column>,
    group_rows: u32,
    pub(super) limits: Limits,
    /// What was given and not parsed yet.
    text: String,
    /// The start of a character whose other bytes have not come.
    half: Vec<u8>,
    /// Whether what comes after the text is not UTF-8.
    bad: bool,
    /// Whether the end of the file was given.
    ended: bool,
    /// Whether nothing was parsed.
    fresh: bool,
    splitter: Splitter,
    /// `None` once the file has to be read again, and is read on to see the types.
    writer: Option<TableWriter>,
    stage: Stage,
    pass: u32,
    /// Whether the types come from all of the file.
    sure: bool,
    /// Bytes of the file before `text`.
    before: u64,
    /// How long groups of every size are, from the smallest on, and how far
    /// into the file that was found out.
    extents: Vec<Extent>,
    measured: u64,
    /// Whether there have to be fewer rows in a group, whatever that says.
    shrink: bool,
    written: Option<Written>,
    failed: Option<ImportError>,
}

impl Import {
    /// Starts an import. `head` is the first [`HEAD_BYTES`] of the file, or
    /// the whole of it when it is shorter.
    ///
    /// # Errors
    ///
    /// [`ImportError`] when that is enough to know that there is no table in
    /// the file.
    pub fn new(options: &Options, head: &[u8]) -> Result<Self, ImportError> {
        let head = Head::new(head, head.len() < HEAD_BYTES);
        let sample = Sample::new(head.text, head.is_whole && head.is_valid, options);
        if !head.is_valid {
            let row = sample.last_row();
            return Err(ImportError::NotUtf8 { row });
        }
        if sample.len() == 0 {
            return Err(match sample.trouble {
                Some(error) => split_error(error, sample.last_row()),
                None if head.is_whole => ImportError::Empty,
                None => ImportError::FirstRowTooLong,
            });
        }
        if sample.width > MAX_COLUMNS {
            return Err(ImportError::TooManyColumns);
        }
        let mut columns = Vec::new();
        for index in 0..sample.width {
            let is_taken = |name: &str| columns.iter().any(|column: &Column| column.name == name);
            let name = sample.name(index, &is_taken);
            let mut forced = None;
            for (column, kind) in &options.kinds {
                if *column == index {
                    forced = Some(*kind);
                }
            }
            if let Some(ColumnType::Decimal { scale, fixed }) = &mut forced {
                *scale = (*scale).min(MAX_SCALE);
                *fixed = false;
            }
            columns.push(Column {
                name,
                forced,
                kind: forced.or_else(|| sample.kind(index)),
                dictionary: sample.repeats(index),
                seen: Seen::default(),
                blanks: 0,
                misfits: 0,
                cut_texts: 0,
            });
        }
        let mut import = Self {
            delimiter: sample.delimiter,
            header: sample.header,
            markers: options.blank_markers,
            splitter: Splitter::new(sample.delimiter, 0, false),
            group_rows: group_rows_for(columns.len(), sample.row_bytes()),
            columns,
            limits: LIMITS,
            text: String::new(),
            half: Vec::new(),
            bad: false,
            ended: false,
            fresh: true,
            writer: None,
            stage: Stage::Reading,
            pass: 0,
            sure: false,
            before: 0,
            extents: Vec::new(),
            measured: 0,
            shrink: false,
            written: None,
            failed: None,
        };
        import.restart()?;
        Ok(import)
    }

    /// Gives the next bytes of the file, which are copied.
    ///
    /// # Errors
    ///
    /// [`ImportError::Refused`] when more than [`MAX_WAITING`] would wait, or
    /// the file has ended. The import can go on.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), ImportError> {
        if self.ended || self.stage == Stage::Done {
            return Err(ImportError::Refused);
        }
        self.forget();
        if self.text.len().saturating_add(bytes.len()) > MAX_WAITING {
            return Err(ImportError::Refused);
        }
        self.text
            .try_reserve(bytes.len())
            .map_err(|_| ImportError::Write(WriteError::Limit("not enough memory")))?;
        // Rarely, the last piece ended within a character.
        let mut joined = core::mem::take(&mut self.half);
        let bytes = if joined.is_empty() {
            bytes
        } else {
            joined.extend_from_slice(bytes);
            &joined
        };
        if !self.bad {
            let (valid, cut) = valid_start(bytes);
            self.text.push_str(valid);
            self.bad = !cut;
            if cut {
                let rest = bytes.get(valid.len()..).unwrap_or(&[]);
                self.half.extend_from_slice(rest);
            }
        }
        Ok(())
    }

    /// Says that the file has ended.
    pub fn end_of_input(&mut self) {
        self.ended = true;
        self.bad |= !self.half.is_empty();
    }

    /// Parses the next piece, or writes one.
    ///
    /// # Errors
    ///
    /// [`ImportError`] when the file cannot be imported. Every further step
    /// gives the same.
    pub fn step<S: Sink>(&mut self, sink: &mut S) -> Result<Step, ImportError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let step = match self.stage {
            Stage::Done => Ok(Step::Done),
            Stage::Reading => self.read(),
            Stage::Writing { last } => self.write(sink, last),
        };
        if let Err(error) = step {
            self.failed = Some(error);
        }
        step
    }

    /// How far it is.
    #[must_use]
    pub fn progress(&self) -> Progress {
        Progress {
            rows: self.splitter.rows,
            bytes: self.at(),
            pass: self.pass,
        }
    }

    /// What the columns are taken to hold. `None` while that is not known.
    #[must_use]
    pub fn kinds(&self) -> Vec<Option<ColumnType>> {
        self.columns.iter().map(|column| column.kind).collect()
    }

    /// What became of the file, once the table is written.
    #[must_use]
    pub fn report(&self) -> Option<Report> {
        let written = self.written?;
        let mut columns = Vec::new();
        for column in &self.columns {
            columns.push(ColumnReport {
                name: column.name.clone(),
                kind: column.kind.unwrap_or(ColumnType::Text),
                blanks: column.blanks,
                misfits: column.misfits,
                cut_texts: column.cut_texts,
            });
        }
        Some(Report {
            rows: u64::from(written.rows),
            bytes: written.bytes,
            columns,
            extra_fields: self.splitter.extra_fields,
            short_rows: self.splitter.short_rows,
            empty_lines: self.splitter.empty_lines,
            passes: self.pass,
        })
    }

    /// Bytes of the file that were read.
    fn at(&self) -> u64 {
        let here = self.splitter.looked_at();
        self.before
            .saturating_add(u64::try_from(here).unwrap_or(u64::MAX))
    }

    /// Makes room where the text was parsed.
    fn forget(&mut self) {
        self.splitter.clear();
        let gone = self.splitter.forget();
        if gone >= self.text.len() {
            self.text.clear();
        } else if self.text.is_char_boundary(gone) {
            self.text.drain(..gone);
        }
        self.before = self
            .before
            .saturating_add(u64::try_from(gone).unwrap_or(u64::MAX));
    }

    fn read(&mut self) -> Result<Step, ImportError> {
        if self.fresh && !self.text.is_empty() {
            self.fresh = false;
            if self.text.starts_with('\u{feff}') {
                self.splitter.skip(3);
            }
        }
        // A step ends at a multiple of its length in the file, whatever
        // pieces that came in, or a little after it, between two characters.
        let all = self.text.len();
        let step = self.limits.step_bytes;
        let into = self.at().checked_rem(step).unwrap_or(0);
        let stop = self.at().saturating_add(step.saturating_sub(into));
        let mut end = usize::try_from(stop.saturating_sub(self.before)).unwrap_or(all);
        while !self.text.is_char_boundary(end) && end < all {
            end = end.saturating_add(1);
        }
        let end = end.min(all);
        let last = end == all && self.ended && !self.bad;
        let sealed = self.writer.as_ref().map(TableWriter::rows);
        let rows = match sealed {
            Some(sealed) => u64::from(self.group_rows)
                .saturating_add(sealed)
                .saturating_sub(self.splitter.rows),
            None => u64::MAX,
        };
        self.splitter.clear();
        let reached = self
            .splitter
            .run(&self.text, end, last, rows, self.limits.step_fields)
            .map_err(|error| split_error(error, self.splitter.number))?;
        self.take_fields()?;
        if self.splitter.rows > MAX_ROWS {
            return Err(ImportError::Write(WriteError::Limit("too many rows")));
        }
        if self.at() >= stop {
            self.measure();
        }
        let limit = self.limits.group_bytes;
        if self.longest(MIN_GROUP_ROWS) > limit {
            return Err(ROWS_TOO_LONG);
        }
        if self.writer.is_some() {
            if self.longest(self.group_rows) > limit {
                return Ok(self.make_groups_smaller());
            }
            let came = self.splitter.rows.saturating_sub(sealed.unwrap_or(0));
            if came >= u64::from(self.group_rows) {
                return self.seal(false);
            }
        }
        if reached == Reached::Limit || end < all {
            Ok(Step::Working)
        } else if self.bad {
            Err(ImportError::NotUtf8 {
                row: self.splitter.number,
            })
        } else if last {
            self.end_of_rows()
        } else {
            Ok(Step::NeedInput)
        }
    }

    /// Finds out how long the groups of every size are. This is done at
    /// certain places in the file only, and where a group begins and ends
    /// between two of them, it is taken to be longer.
    #[inline(never)]
    fn measure(&mut self) {
        let at = self.at();
        for (extent, shift) in self.extents.iter_mut().zip(MIN_GROUP_ROWS.ilog2()..) {
            extent.longest = extent.longest.max(at.saturating_sub(extent.start));
            let group = self.splitter.rows.wrapping_shr(shift);
            if group > extent.group {
                extent.group = group;
                extent.start = self.measured;
            }
        }
        self.measured = at;
    }

    /// Bytes of the file that the longest group of so many rows is read from.
    /// 0 for a size that groups cannot have.
    #[inline(never)]
    fn longest(&self, rows: u32) -> u64 {
        let index = rows.ilog2().checked_sub(MIN_GROUP_ROWS.ilog2());
        let extent = index.and_then(|index| self.extents.get(index as usize));
        extent.map_or(0, |extent| extent.longest)
    }

    /// Reads the cells of the fields that were cut.
    fn take_fields(&mut self) -> Result<(), ImportError> {
        let fields = Fields {
            input: &self.text,
            joined: &self.splitter.joined,
            all: &self.splitter.fields,
            first: self.splitter.first,
            width: self.columns.len(),
            markers: self.markers,
        };
        if fields.all.is_empty() {
            return Ok(());
        }
        let mut surprised = false;
        match &mut self.writer {
            Some(writer) => {
                let pairs = self.columns.iter_mut().zip(writer.columns());
                for (index, (column, builder)) in pairs.enumerate() {
                    surprised |= !column.read(&fields, index, builder);
                }
            }
            None => {
                for (index, column) in self.columns.iter_mut().enumerate() {
                    column.look(&fields, index);
                }
            }
        }
        if surprised {
            if self.sure {
                return Err(ImportError::Changed);
            }
            self.writer = None;
        }
        Ok(())
    }

    /// All rows were read.
    fn end_of_rows(&mut self) -> Result<Step, ImportError> {
        let is_right = self.columns.iter().all(Column::is_right);
        if self.writer.is_none() || !is_right {
            if self.sure && !(self.shrink && is_right) {
                return Err(ImportError::Changed);
            }
            self.measure();
            let mut rows = self.group_rows >> u32::from(self.shrink);
            while self.longest(rows) > self.limits.group_bytes {
                rows >>= 1;
            }
            if rows < MIN_GROUP_ROWS {
                return Err(ROWS_TOO_LONG);
            }
            // What was found out once should have been enough.
            self.group_rows = if self.sure { MIN_GROUP_ROWS } else { rows };
            self.sure = true;
            for column in &mut self.columns {
                let kind = column.seen.kind();
                column.restart(kind);
            }
            return self.restart();
        }
        if let Some(writer) = &mut self.writer {
            for (column, builder) in self.columns.iter_mut().zip(writer.columns()) {
                builder.set_fixed(column.is_fixed());
                column.kind = builder.kind();
            }
        }
        self.seal(true)
    }

    fn seal(&mut self, last: bool) -> Result<Step, ImportError> {
        let Some(writer) = &mut self.writer else {
            return Ok(Step::Working);
        };
        match writer.seal_group() {
            Ok(()) => {}
            Err(WriteError::GroupTooLarge) => return Ok(self.make_groups_smaller()),
            Err(error) => return Err(ImportError::Write(error)),
        }
        if last {
            writer.finish();
        }
        self.stage = Stage::Writing { last };
        Ok(Step::Working)
    }

    fn write<S: Sink>(&mut self, sink: &mut S, last: bool) -> Result<Step, ImportError> {
        let Some(writer) = &mut self.writer else {
            return Ok(Step::Working);
        };
        match writer.step(sink) {
            Ok(true) => Ok(Step::Working),
            Ok(false) if last => {
                self.written = writer.written();
                self.writer = None;
                self.stage = Stage::Done;
                Ok(Step::Done)
            }
            Ok(false) => {
                self.stage = Stage::Reading;
                Ok(Step::Working)
            }
            Err(WriteError::GroupTooLarge) => Ok(self.make_groups_smaller()),
            Err(error) => Err(ImportError::Write(error)),
        }
    }

    /// The rows are longer than they seemed, so fewer of them go into a group.
    /// The rest of the file is looked at to see how long they get.
    fn make_groups_smaller(&mut self) -> Step {
        self.shrink = true;
        self.writer = None;
        self.stage = Stage::Reading;
        Step::Working
    }

    /// Starts with the first pass, or another one.
    fn restart(&mut self) -> Result<Step, ImportError> {
        let writer =
            TableWriter::new(specs(&self.columns), self.group_rows).map_err(ImportError::Write)?;
        self.writer = Some(writer);
        self.splitter = Splitter::new(self.delimiter, self.columns.len(), self.header);
        self.text.clear();
        self.half.clear();
        self.bad = false;
        self.ended = false;
        self.fresh = true;
        self.stage = Stage::Reading;
        self.pass = self.pass.saturating_add(1);
        self.before = 0;
        self.extents.clear();
        self.extents.resize(GROUP_SIZES, Extent::default());
        self.measured = 0;
        self.shrink = false;
        Ok(Step::Restart)
    }
}

/// The rows are too long even for the smallest groups.
const ROWS_TOO_LONG: ImportError = ImportError::Write(WriteError::Limit("the rows are too long"));

const fn split_error(error: SplitError, row: u64) -> ImportError {
    match error {
        SplitError::Unclosed => ImportError::UnclosedQuote { row },
        SplitError::TooLong => ImportError::FieldTooLong { row },
    }
}
