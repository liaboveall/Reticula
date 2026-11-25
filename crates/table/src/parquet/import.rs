//! From the columns of a file to a table.
//!
//! A group of the table is collected column by column, and each column goes
//! on reading where the group before it ended: the groups of rows of the file
//! have other sizes, and so do its pages. That a column cannot become what its
//! type says may turn out late. Then the rest of the columns that this can
//! happen to are only looked at, up to the end, and everything starts again
//! knowing all of it.

use alloc::string::String;
use alloc::vec::Vec;

use super::stream::{Advance, Counts, Input, Out, Problem, Sample, Scratch, Stream};
use super::value::{Plan, SkipReason};
use super::{Column, Limit, ParquetError, ParquetFile, SkippedColumn};
use crate::layout::MIN_GROUP_ROWS;
use crate::source::{ByteRange, Sink, Source};
use crate::types::{ColumnType, MAX_COLUMNS};
use crate::writer::{ColumnSpec, TableWriter, WriteError, group_rows_for};

/// Bytes that may be handled in one step, about. A page is handled at once,
/// however long it is.
const STEP_WORK: usize = 1 << 20;
/// Bytes that the streams may hold together, about.
const STREAM_BUDGET: usize = 160 << 20;
/// Bytes that the streams read ahead together, at most.
const WINDOWS: usize = 32 << 20;
/// Least and most bytes that a stream reads at once.
const WINDOW: (usize, usize) = (64 << 10, 1 << 20);
/// Bytes of text of all columns that a group is meant to have at most.
const GROUP_TEXT: usize = 160 << 20;
/// Rows that are looked at to find out whether a dictionary is worth it, at
/// most.
const SAMPLE_ROWS: u64 = 65_536;

/// What is to be imported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The columns, as [`ParquetFile::columns`] counts; all when not given.
    pub columns: Option<Vec<usize>>,
}

/// What came of a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// There is more to do.
    Working,
    /// These bytes are missing. Once other bytes are, they may be forgotten.
    Need(Vec<ByteRange>),
    /// What was written is of no use. The next step expects an empty sink.
    Restart,
    /// The table is written.
    Done,
}

/// How far an import has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Rows that were written, or looked at.
    pub rows: u64,
    /// Rows there are.
    pub of: u64,
    /// How often it was started again.
    pub restarts: u32,
    /// Whether the rest is only looked at, and then everything starts again.
    pub checking: bool,
}

/// What became of a column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnReport {
    /// Its name.
    pub name: String,
    /// What it holds.
    pub kind: ColumnType,
    /// Blank cells, the ones for values that could not be taken too.
    pub blanks: u64,
    /// Times that lost parts of a microsecond.
    pub cut_nanos: u64,
    /// Numbers that were not finite.
    pub not_finite: u64,
    /// Dates and times that a table cannot hold.
    pub out_of_range: u64,
    /// Texts that were too long.
    pub cut_texts: u64,
}

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Rows.
    pub rows: u64,
    /// The columns of the table.
    pub columns: Vec<ColumnReport>,
    /// The columns that were left out, those not asked for excepted.
    pub skipped: Vec<SkippedColumn>,
}

/// A column that is imported.
#[derive(Debug)]
struct Taken {
    column: Column,
    /// Its place among the columns of the file.
    index: u32,
    stream: Stream,
    /// Whether a dictionary is worth it, once that is known.
    dictionary: Option<bool>,
    /// Why it is left out from now on.
    dropped: Option<SkipReason>,
    /// Rows that were given for the current group, or looked at.
    rows: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Finding out whether a dictionary is worth it, for which column.
    Sample(usize),
    /// Collecting a group, which column.
    Fill(usize),
    /// Writing.
    Write,
    /// Looking at the rest, of which column.
    Check(usize),
    Done,
}

/// What came of having a column go on.
enum Ran {
    /// It gave as many rows as it was asked for.
    Enough,
    Working,
    /// Bytes are missing, as the step says.
    Need(Step),
    Problem(Problem),
}

/// Imports one file.
#[derive(Debug)]
pub struct Import {
    rows: u64,
    data_end: u64,
    taken: Vec<Taken>,
    skipped: Vec<SkippedColumn>,
    scratch: Scratch,
    sample: Sample,
    writer: Option<TableWriter>,
    /// Most rows of a group.
    pub(super) group_rows: u32,
    /// Bytes that the streams may hold together, about.
    pub(super) stream_budget: usize,
    /// Bytes of text that a group is meant to have at most.
    pub(super) most_text: usize,
    /// Rows of the groups before the current one.
    written: u64,
    stage: Stage,
    restarts: u32,
}

impl Import {
    /// Starts to import.
    ///
    /// # Errors
    ///
    /// [`ParquetError`] when there is nothing to import, or too much.
    pub fn new(file: ParquetFile, options: &Options) -> Result<Self, ParquetError> {
        let count = options
            .columns
            .as_ref()
            .map_or(file.columns.len(), Vec::len);
        if count > MAX_COLUMNS {
            return Err(ParquetError::Limit(Limit::Columns));
        }
        let mut taken = Vec::new();
        for (index, column) in file.columns.into_iter().enumerate() {
            let wanted = options
                .columns
                .as_ref()
                .is_none_or(|columns| columns.contains(&index));
            if wanted {
                taken.push(Taken {
                    column,
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    stream: Stream::new(),
                    dictionary: None,
                    dropped: None,
                    rows: 0,
                });
            }
        }
        if taken.is_empty() {
            return Err(ParquetError::NoColumns);
        }
        let mut import = Self {
            rows: file.rows,
            data_end: file.data_end,
            group_rows: 0,
            stream_budget: STREAM_BUDGET,
            most_text: GROUP_TEXT,
            taken,
            skipped: file.skipped,
            scratch: Scratch::new(),
            sample: Sample::default(),
            writer: None,
            written: 0,
            stage: Stage::Sample(0),
            restarts: 0,
        };
        import.group_rows = import.group_rows_at_first();
        Ok(import)
    }

    /// How many rows a group should have, going by the size of the values.
    fn group_rows_at_first(&self) -> u32 {
        let rows = self.rows.max(1);
        let row_bytes = self
            .taken
            .iter()
            .filter(|taken| taken.dropped.is_none())
            .map(|taken| {
                let chunks = taken.column.chunks.iter();
                let unpacked = chunks.fold(0_u64, |sum, chunk| sum.saturating_add(chunk.unpacked));
                let each = unpacked.checked_div(rows).unwrap_or(0);
                usize::try_from(each).unwrap_or(usize::MAX).max(8)
            })
            .fold(0, usize::saturating_add);
        group_rows_for(self.taken.len(), row_bytes)
    }

    /// Does the next piece of work.
    ///
    /// # Errors
    ///
    /// [`ParquetError`] when the file cannot be imported after all.
    pub fn step<In: Source, To: Sink>(
        &mut self,
        source: &mut In,
        sink: &mut To,
    ) -> Result<Step, ParquetError> {
        match self.stage {
            Stage::Sample(index) => self.sample(index, source),
            Stage::Fill(index) => self.fill(index, source),
            Stage::Write => self.write(sink),
            Stage::Check(index) => self.check(index, source),
            Stage::Done => Ok(Step::Done),
        }
    }

    /// How far it has come.
    #[must_use]
    pub fn progress(&self) -> Progress {
        let checking = match self.stage {
            Stage::Check(index) => {
                Some(self.taken.get(index).map_or(self.rows, |taken| taken.rows))
            }
            _ => None,
        };
        Progress {
            rows: checking.unwrap_or(self.written),
            of: self.rows,
            restarts: self.restarts,
            checking: checking.is_some(),
        }
    }

    /// What it did, so far.
    #[must_use]
    pub fn report(&self) -> Report {
        let mut report = Report {
            rows: self.written,
            columns: Vec::new(),
            skipped: self.skipped.clone(),
        };
        for taken in &self.taken {
            let Counts {
                blanks,
                cut_nanos,
                not_finite,
                out_of_range,
                cut_texts,
            } = taken.stream.counts;
            report.columns.push(ColumnReport {
                name: taken.column.name.clone(),
                kind: taken.column.kind,
                blanks,
                cut_nanos,
                not_finite,
                out_of_range,
                cut_texts,
            });
        }
        report
    }

    fn window(&self) -> usize {
        let (least, most) = WINDOW;
        WINDOWS
            .checked_div(self.taken.len())
            .unwrap_or(most)
            .clamp(least, most)
    }

    /// Rows of the current group.
    fn target(&self) -> u64 {
        self.rows
            .saturating_sub(self.written)
            .min(u64::from(self.group_rows))
    }

    /// Everything starts again.
    fn restart(&mut self) -> Result<Step, ParquetError> {
        let mut index = 0;
        while let Some(taken) = self.taken.get_mut(index) {
            taken.rows = 0;
            taken.stream.restart();
            match taken.dropped {
                Some(reason) => {
                    let name = self.taken.remove(index).column.name;
                    self.skipped.push(SkippedColumn { name, reason });
                }
                None => index = index.saturating_add(1),
            }
        }
        if self.taken.is_empty() {
            return Err(ParquetError::NoColumns);
        }
        self.writer = None;
        self.written = 0;
        self.stage = Stage::Sample(0);
        self.restarts = self.restarts.saturating_add(1);
        Ok(Step::Restart)
    }

    /// Starts again with fewer rows in a group, going by the texts of the
    /// columns up to that one, which have that many rows.
    fn smaller_groups(&mut self, index: usize, rows: u64) -> Result<Step, ParquetError> {
        if self.group_rows <= MIN_GROUP_ROWS {
            return Err(ParquetError::Limit(Limit::Text));
        }
        let rows = usize::try_from(rows.max(1)).unwrap_or(1);
        let known = index.saturating_add(1).min(self.taken.len());
        let row_bytes = self
            .group_text()
            .div_ceil(rows)
            .saturating_mul(self.taken.len())
            .checked_div(known)
            .unwrap_or(0);
        self.group_rows =
            group_rows_for(self.taken.len(), row_bytes).min(self.group_rows.wrapping_shr(1));
        self.restart()
    }

    /// What a column that cannot go on as it was becomes.
    fn change(&mut self, index: usize, problem: Problem) {
        let Some(taken) = self.taken.get_mut(index) else {
            return;
        };
        match problem {
            Problem::TooWide => {
                taken.column.plan = taken.column.plan.as_text(taken.column.kind);
                taken.column.kind = ColumnType::Text;
                taken.dictionary = None;
            }
            Problem::NotText | Problem::TooMuchText => {
                taken.dropped = Some(SkipReason::NotText);
            }
        }
    }

    /// Has one column go on until it gave so many rows, or for a while.
    fn run<In: Source>(
        &mut self,
        index: usize,
        source: &mut In,
        until: u64,
    ) -> Result<Ran, ParquetError> {
        let window = self.window();
        let Some(taken) = self.taken.get_mut(index) else {
            return Ok(Ran::Enough);
        };
        let mut input = Input {
            source,
            column: &taken.column,
            data_end: self.data_end,
            window,
        };
        let mut out = match (self.stage, &mut self.writer) {
            (Stage::Sample(_), _) => Out::Sample(&mut self.sample),
            (Stage::Fill(_), Some(writer)) => match writer.column(index) {
                Some(cells) => Out::Cells(cells),
                None => Out::Nowhere,
            },
            _ => Out::Nowhere,
        };
        let mut work = 0;
        while taken.rows < until {
            let full = matches!(&out, Out::Sample(sample) if sample.is_full());
            if full {
                break;
            }
            if work >= STEP_WORK {
                return Ok(Ran::Working);
            }
            let left = until.saturating_sub(taken.rows);
            let advanced = taken
                .stream
                .advance(
                    &mut input,
                    &mut self.scratch,
                    usize::try_from(left).unwrap_or(usize::MAX),
                    &mut out,
                    &mut work,
                )
                .map_err(|fault| fault.at(Some(taken.index)))?;
            match advanced {
                Advance::Rows(rows) => {
                    let rows = u64::try_from(rows).unwrap_or(0);
                    taken.rows = taken.rows.saturating_add(rows);
                }
                Advance::Worked => {}
                Advance::Need(range) => {
                    return Ok(Ran::Need(Step::Need(alloc::vec![range])));
                }
                Advance::Problem(problem) => return Ok(Ran::Problem(problem)),
            }
        }
        Ok(Ran::Enough)
    }

    /// Looks at the first texts of the columns, one after another.
    fn sample<In: Source>(&mut self, index: usize, source: &mut In) -> Result<Step, ParquetError> {
        let Some(taken) = self.taken.get(index) else {
            return self.begin();
        };
        if taken.dictionary.is_none() && taken.column.plan.is_text() {
            match self.run(index, source, self.rows.min(SAMPLE_ROWS))? {
                // A problem will be found again when it counts.
                Ran::Enough | Ran::Problem(_) => {}
                Ran::Working => return Ok(Step::Working),
                Ran::Need(step) => return Ok(step),
            }
        }
        if let Some(taken) = self.taken.get_mut(index) {
            if taken.dictionary.is_none() {
                let worth = self.sample.repeats() || taken.stream.few_in_dictionary(&taken.column);
                taken.dictionary = Some(worth);
            }
            taken.stream.restart();
            taken.rows = 0;
        }
        self.sample = Sample::default();
        self.stage = Stage::Sample(index.saturating_add(1));
        Ok(Step::Working)
    }

    /// Starts the table.
    fn begin(&mut self) -> Result<Step, ParquetError> {
        let specs = self
            .taken
            .iter()
            .map(|taken| ColumnSpec {
                name: taken.column.name.clone(),
                kind: Some(taken.column.kind),
                dictionary: taken.dictionary == Some(true),
            })
            .collect();
        let writer = TableWriter::new(specs, self.group_rows).map_err(ParquetError::Write)?;
        self.group_rows = writer.group_rows();
        self.writer = Some(writer);
        self.stage = Stage::Fill(0);
        Ok(Step::Working)
    }

    /// Collects cells of one column for the current group.
    fn fill<In: Source>(&mut self, index: usize, source: &mut In) -> Result<Step, ParquetError> {
        let target = self.target();
        let (Some(taken), Some(writer)) = (self.taken.get_mut(index), &mut self.writer) else {
            return self.seal(target);
        };
        if taken.column.plan == Plan::Blank
            && let Some(cells) = writer.column(index)
        {
            for _ in 0..target {
                cells.push_blank();
            }
            taken.stream.counts.blanks = taken.stream.counts.blanks.saturating_add(target);
            taken.rows = target;
        }
        match self.run(index, source, target)? {
            Ran::Enough => {}
            Ran::Working => return Ok(Step::Working),
            Ran::Need(step) => return Ok(step),
            Ran::Problem(Problem::TooMuchText) => {
                let rows = self.taken.get(index).map_or(1, |taken| taken.rows);
                return self.smaller_groups(index, rows);
            }
            Ran::Problem(problem) => {
                self.change(index, problem);
                return Ok(self.begin_check());
            }
        }
        let text = self.group_text();
        // The smallest group may have more: it cannot be made smaller.
        let most = if self.group_rows > MIN_GROUP_ROWS {
            self.most_text
        } else {
            self.most_text.saturating_mul(3)
        };
        if text > most {
            return self.smaller_groups(index, target);
        }
        let held = self
            .taken
            .iter()
            .map(|taken| taken.stream.memory())
            .fold(0, usize::saturating_add);
        if held > self.stream_budget
            && let Some(taken) = self.taken.get_mut(index)
        {
            // The others are needed sooner.
            taken.stream.suspend();
        }
        self.stage = Stage::Fill(index.saturating_add(1));
        Ok(Step::Working)
    }

    /// Bytes of text that the group takes so far.
    fn group_text(&mut self) -> usize {
        let Some(writer) = &mut self.writer else {
            return 0;
        };
        self.taken
            .iter()
            .zip(writer.columns())
            .map(|(taken, cells)| taken.stream.text_in(cells))
            .fold(0, usize::saturating_add)
    }

    /// Ends the group.
    fn seal(&mut self, target: u64) -> Result<Step, ParquetError> {
        let Some(writer) = &mut self.writer else {
            return Err(ParquetError::Write(WriteError::Uneven));
        };
        match writer.seal_group() {
            Ok(()) => {}
            Err(WriteError::GroupTooLarge) => return self.smaller_groups(self.taken.len(), target),
            Err(error) => return Err(ParquetError::Write(error)),
        }
        self.written = self.written.saturating_add(target);
        if self.written >= self.rows {
            writer.finish();
        }
        for taken in &mut self.taken {
            taken.rows = 0;
            taken.stream.begin_group();
        }
        self.stage = Stage::Write;
        Ok(Step::Working)
    }

    fn write<To: Sink>(&mut self, sink: &mut To) -> Result<Step, ParquetError> {
        let target = u64::from(self.group_rows);
        let Some(writer) = &mut self.writer else {
            return Err(ParquetError::Write(WriteError::Uneven));
        };
        match writer.step(sink) {
            Ok(true) => Ok(Step::Working),
            Ok(false) if writer.written().is_some() => {
                self.stage = Stage::Done;
                Ok(Step::Done)
            }
            Ok(false) => {
                self.stage = Stage::Fill(0);
                Ok(Step::Working)
            }
            Err(WriteError::GroupTooLarge) => self.smaller_groups(self.taken.len(), target),
            Err(error) => Err(ParquetError::Write(error)),
        }
    }

    /// Starts to look at the rest.
    fn begin_check(&mut self) -> Step {
        self.writer = None;
        for taken in &mut self.taken {
            taken.rows = taken.rows.saturating_add(self.written);
        }
        self.stage = Stage::Check(0);
        Step::Working
    }

    /// Looks at the rest of the columns that may not be what their type says.
    fn check<In: Source>(&mut self, index: usize, source: &mut In) -> Result<Step, ParquetError> {
        let Some(taken) = self.taken.get(index) else {
            return self.restart();
        };
        if taken.column.plan.is_uncertain() && taken.dropped.is_none() {
            match self.run(index, source, self.rows)? {
                Ran::Enough => {}
                Ran::Working => return Ok(Step::Working),
                Ran::Need(step) => return Ok(step),
                Ran::Problem(problem) => self.change(index, problem),
            }
        }
        if let Some(taken) = self.taken.get_mut(index) {
            taken.stream.suspend();
        }
        self.stage = Stage::Check(index.saturating_add(1));
        Ok(Step::Working)
    }
}
