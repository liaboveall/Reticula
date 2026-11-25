//! A table that is being looked at.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::viewport::{PAGE_ROWS, PageFrame};

use super::QueryError;
use super::build::{Building, View};
use super::cells::Cells;
use super::context::{Caches, Context, Limits, Step};
use super::filter::Filtering;
use super::memory::Meter;
use super::predicate::Term;
use super::selection::Selection;
use super::spec::{Spec, SpecError};
use super::summary::{ColumnSummary, Summing};
use super::values::{Listing, ValueList};
use crate::page;
use crate::reader::{ReadError, Reader};
use crate::source::{AnySource, ByteRange, Source};
use crate::text;

/// Bytes of buffers for cells that are kept for the next job.
const CELLS_KEPT: usize = 16 << 20;

/// What a job is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    /// A new view, see [`Session::begin`].
    View,
    /// The values of a column, see [`Session::begin_values`].
    Values,
    /// Totals, see [`Session::begin_summary`].
    Summary,
}

/// All of them, in the order they are worked on.
const JOBS: [Job; 3] = [Job::View, Job::Values, Job::Summary];

/// How far a job is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// There is more to do.
    Working {
        /// How much was done, about, in cells.
        done: u64,
        /// How much there is to do in all, about. It is never less.
        total: u64,
    },
    /// These bytes have to be fetched before it can go on.
    Need(Vec<ByteRange>),
    /// The job that was worked on is complete, or there was none.
    Done,
}

/// Work on something that was asked for.
pub(super) trait Work {
    /// Does the next piece.
    fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, QueryError>;

    /// What it came to, when the last step is done.
    fn finish(&mut self) -> Outcome;
}

/// What work came to.
pub(super) enum Outcome {
    View(Spec, Selection, View),
    Values(ValueList),
    Summary(Vec<ColumnSummary>),
}

/// What was asked for.
enum Request {
    View,
    Values {
        column: usize,
        search: String,
        limit: usize,
    },
    Summary(Vec<usize>),
}

/// Something that was asked for, and the work on it once that has begun.
///
/// The kinds of work are not named here: what is done with all of them, and
/// with a session as a whole, is there once, and in this crate.
struct Asked {
    request: Request,
    work: Option<Box<dyn Work>>,
    done: u64,
    total: u64,
}

/// A table, the rows of it that are shown, and the jobs that are under way.
///
/// There is one job of each kind at most. They are worked on one after
/// another, a new view first; the others are about the view that is there
/// when they begin, and begin again when another one is asked for.
pub struct Session {
    reader: Reader,
    meter: Meter,
    cells: Cells,
    caches: Caches,
    pub(super) limits: Limits,
    /// What the view shows, the rows it has, and their order.
    spec: Spec,
    rows: Selection,
    view: View,
    /// What was asked for, a place for every kind of job.
    asked: [Option<Asked>; 3],
    values: Option<ValueList>,
    summary: Option<Vec<ColumnSummary>>,
}

impl core::fmt::Debug for Session {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Session")
    }
}

impl Session {
    /// Looks at a table. All of its rows are shown, as they are in the file.
    #[must_use]
    pub fn new(reader: Reader) -> Self {
        let meter = Meter::new();
        meter.set_outside(reader.memory());
        Self {
            rows: Selection::all(reader.rows()),
            cells: Cells::new(meter.no_charge()),
            reader,
            meter,
            caches: Caches::default(),
            limits: Limits::DEFAULT,
            spec: Spec::default(),
            view: View::File,
            asked: [const { None }; 3],
            values: None,
            summary: None,
        }
    }

    /// The table.
    #[must_use]
    pub const fn reader(&self) -> &Reader {
        &self.reader
    }

    /// Sets how many bytes this may use in all, about. What is in use already
    /// is kept.
    pub fn set_memory_limit(&mut self, bytes: usize) {
        self.meter.set_limit(bytes);
    }

    /// Bytes in use, about.
    #[must_use]
    pub fn memory(&self) -> usize {
        self.meter.set_outside(self.reader.memory());
        self.meter.used()
    }

    /// What the view shows.
    #[must_use]
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Number of rows of the view.
    #[must_use]
    pub const fn view_len(&self) -> u32 {
        self.rows.len()
    }

    /// The row of the file at a position of the view.
    #[must_use]
    pub fn view_row(&self, position: u32) -> Option<u32> {
        match &self.view {
            View::File => (position < self.rows.len()).then_some(position),
            View::Rows(rows) => rows.get(position as usize).copied(),
        }
    }

    /// The cells of some rows of the view, from `frame.first_row` on, for the
    /// screen. See [`page::encode`].
    ///
    /// # Errors
    ///
    /// [`ReadError`] when not all of the cells can be read. What is missing can
    /// be had from [`Session::take_missing`].
    pub fn page<S: Source>(
        &mut self,
        source: &mut S,
        frame: PageFrame,
        scratch: &mut String,
    ) -> Result<Vec<u8>, ReadError> {
        self.encode(AnySource(source), frame, scratch)
    }

    fn encode(
        &mut self,
        mut source: AnySource<'_>,
        frame: PageFrame,
        scratch: &mut String,
    ) -> Result<Vec<u8>, ReadError> {
        let end = frame
            .first_row
            .saturating_add(PAGE_ROWS)
            .min(self.view_len());
        let rows: Vec<u32> = (frame.first_row..end)
            .filter_map(|position| self.view_row(position))
            .collect();
        page::encode(&mut self.reader, &mut source, frame, &rows, scratch)
    }

    /// The whole content of a cell as text.
    ///
    /// # Errors
    ///
    /// [`ReadError`] when it cannot be read. What is missing can be had from
    /// [`Session::take_missing`].
    pub fn cell_text<S: Source>(
        &mut self,
        source: &mut S,
        position: u32,
        column: usize,
    ) -> Result<String, ReadError> {
        self.text_of(AnySource(source), position, column)
    }

    fn text_of(
        &mut self,
        mut source: AnySource<'_>,
        position: u32,
        column: usize,
    ) -> Result<String, ReadError> {
        let mut out = String::new();
        let of = self.reader.columns().get(column);
        if let (Some(row), Some(kind)) = (self.view_row(position), of.map(|of| of.kind)) {
            let value = self
                .reader
                .cell(&mut source, column, row, usize::MAX, &mut out)?;
            text::write_exact(kind, value, &mut out);
        }
        Ok(out)
    }

    /// The bytes that pages and cells were missing since the last call.
    pub fn take_missing(&mut self) -> Vec<ByteRange> {
        self.reader.take_missing()
    }

    /// Starts to build another view. The one there is stays until the new one
    /// is complete. A view that is being built is given up, and so is what
    /// was worked out of a summary or of a list of values: those begin again
    /// when the view is there.
    ///
    /// # Errors
    ///
    /// [`SpecError`] when this cannot be asked of the table. Then nothing changes.
    pub fn begin(&mut self, spec: Spec) -> Result<(), SpecError> {
        let terms = self.terms(&spec, None)?;
        let mut spec = spec;
        let mut order = Vec::new();
        for key in spec.order {
            if key.column >= self.reader.columns().len() {
                return Err(SpecError::NoColumn { column: key.column });
            }
            // Rows that are equal in a column once are so another time.
            if order
                .iter()
                .all(|known: &super::SortKey| known.column != key.column)
            {
                order.push(key);
            }
        }
        spec.order = order;
        self.cancel_job(Job::View);
        for asked in self.asked.iter_mut().flatten() {
            // What was worked out so far does not hold any more.
            asked.work = None;
            asked.done = 0;
        }
        self.values = None;
        self.summary = None;
        let same = (spec.filters == self.spec.filters).then(|| self.rows.clone());
        let building = Building::new(spec, terms, same, self.reader.rows());
        let size = building.size();
        self.ask(Job::View, Request::View, size);
        if let Some(asked) = self.asked_for(Job::View) {
            asked.work = Some(Box::new(building));
        }
        Ok(())
    }

    /// Notes what was asked for, instead of what was before.
    fn ask(&mut self, job: Job, request: Request, total: u64) {
        self.cancel_job(job);
        let asked = Asked {
            request,
            work: None,
            done: 0,
            total,
        };
        if let Some(place) = self.asked.get_mut(job as usize) {
            *place = Some(asked);
        }
    }

    fn asked_for(&mut self, job: Job) -> Option<&mut Asked> {
        self.asked.get_mut(job as usize)?.as_mut()
    }

    /// The conditions, made ready, but for those on one column.
    fn terms(&self, spec: &Spec, except: Option<usize>) -> Result<Vec<Term>, SpecError> {
        let mut terms = Vec::new();
        for (column, condition) in &spec.filters {
            if except != Some(*column) {
                let of = self.reader.columns().get(*column);
                let kind = of.ok_or(SpecError::NoColumn { column: *column })?.kind;
                terms.push(Term::new(*column, kind, condition)?);
            }
        }
        Ok(terms)
    }

    /// Starts to add up columns over the rows of the view.
    ///
    /// # Errors
    ///
    /// [`SpecError`] when there is no such column. Then nothing changes.
    pub fn begin_summary(&mut self, columns: Vec<usize>) -> Result<(), SpecError> {
        for column in &columns {
            self.check_column(*column)?;
        }
        let size = Summing::size(self.rows.len(), columns.len());
        self.ask(Job::Summary, Request::Summary(columns), size);
        Ok(())
    }

    /// Starts to list the values of a column that contain a text, as it is
    /// shown and regardless of case. They are counted over the rows that the
    /// conditions of the view on other columns hold for. There are `limit` of
    /// them at most, the most frequent ones, in the order of the column.
    ///
    /// # Errors
    ///
    /// [`SpecError`] when there is no such column. Then nothing changes.
    pub fn begin_values(
        &mut self,
        column: usize,
        search: String,
        limit: usize,
    ) -> Result<(), SpecError> {
        self.check_column(column)?;
        let rows = self.reader.rows();
        let size = u64::from(rows).saturating_add(Filtering::size(rows, self.spec.filters.len()));
        let request = Request::Values {
            column,
            search,
            limit,
        };
        self.ask(Job::Values, request, size);
        Ok(())
    }

    const fn check_column(&self, column: usize) -> Result<(), SpecError> {
        if column < self.reader.directory().columns.len() {
            Ok(())
        } else {
            Err(SpecError::NoColumn { column })
        }
    }

    /// The totals that were asked for, once they are complete.
    pub fn take_summary(&mut self) -> Option<Vec<ColumnSummary>> {
        self.summary.take()
    }

    /// The values that were asked for, once they are complete.
    pub fn take_values(&mut self) -> Option<ValueList> {
        self.values.take()
    }

    /// Which job the next step will work on.
    #[must_use]
    pub fn running(&self) -> Option<Job> {
        JOBS.into_iter()
            .zip(&self.asked)
            .find_map(|(job, asked)| asked.as_ref().map(|_| job))
    }

    /// Gives up all jobs. The view stays as it is.
    pub fn cancel(&mut self) {
        for job in JOBS {
            self.cancel_job(job);
        }
    }

    /// Gives up a job.
    pub fn cancel_job(&mut self, job: Job) {
        if let Some(asked) = self.asked.get_mut(job as usize) {
            *asked = None;
        }
        self.clean_up();
    }

    /// Lets go of what only a job needed.
    fn clean_up(&mut self) {
        self.caches.abandon(&mut self.reader);
        if self.cells.charged() > CELLS_KEPT {
            self.cells = Cells::new(self.meter.no_charge());
        }
        self.meter.set_outside(self.reader.memory());
    }

    /// Works on the jobs, until about so many cells were handled, a job is
    /// complete, or bytes are missing. At least one piece of work is done,
    /// which is never larger than a group of rows of one column.
    ///
    /// # Errors
    ///
    /// [`QueryError`] when the job cannot be completed. It is given up then;
    /// the view stays as it is, and so do the other jobs.
    pub fn step<S: Source>(&mut self, source: &mut S, budget: u32) -> Result<Progress, QueryError> {
        self.steps(source, budget)
    }

    fn steps(&mut self, source: &mut dyn Source, budget: u32) -> Result<Progress, QueryError> {
        let mut left = u64::from(budget.max(1));
        while let Some(job) = self.running() {
            match self.piece(source, job) {
                Ok(step) if step.done => {
                    self.finish(job);
                    return Ok(Progress::Done);
                }
                Ok(step) => {
                    left = left.saturating_sub(step.cost);
                    if left == 0 {
                        return Ok(self.progress(job));
                    }
                }
                Err(QueryError::Read(ReadError::Missing)) => {
                    let missing = self.reader.take_missing();
                    if !missing.is_empty() {
                        return Ok(Progress::Need(missing));
                    }
                    self.cancel_job(job);
                    return Err(QueryError::Read(ReadError::Failed));
                }
                Err(error) => {
                    self.cancel_job(job);
                    return Err(error);
                }
            }
        }
        Ok(Progress::Done)
    }

    fn progress(&mut self, job: Job) -> Progress {
        self.asked_for(job)
            .map_or(Progress::Done, |asked| Progress::Working {
                done: asked.done,
                total: asked.total.max(asked.done),
            })
    }

    fn piece(&mut self, source: &mut dyn Source, job: Job) -> Result<Step, QueryError> {
        let Some(Some(mut asked)) = self.asked.get_mut(job as usize).map(Option::take) else {
            return Ok(Step::done(1));
        };
        let step = self.work_on(source, &mut asked);
        if let Some(place) = self.asked.get_mut(job as usize) {
            *place = Some(asked);
        }
        step
    }

    fn work_on(&mut self, source: &mut dyn Source, asked: &mut Asked) -> Result<Step, QueryError> {
        let rows = self.rows.clone();
        // The rows that the conditions on all other columns hold for: as far as
        // they are known, or what is to be found out. The conditions were made
        // ready once, so they can be again.
        let others = match asked {
            Asked {
                request: Request::Values { column, .. },
                work: None,
                ..
            } => self.terms(&self.spec, Some(*column)).unwrap_or_default(),
            _ => Vec::new(),
        };
        let all = self.spec.filters.len();
        let mut cx = Context {
            reader: &mut self.reader,
            source: AnySource(source),
            cells: &mut self.cells,
            caches: &mut self.caches,
            meter: &self.meter,
            limits: self.limits,
        };
        cx.count_reader();
        let work = match (&mut asked.work, &asked.request) {
            (Some(work), _) => work,
            (None, Request::View) => return Ok(Step::done(1)),
            (
                place @ None,
                Request::Values {
                    column,
                    search,
                    limit,
                },
            ) => {
                let (rows, filter) = if others.len() == all {
                    (rows, None)
                } else if others.is_empty() {
                    (Selection::all(cx.reader.rows()), None)
                } else {
                    (rows, Some(Filtering::new(&mut cx, others)?))
                };
                let listing = Listing::new(&cx, *column, search, *limit, rows, filter);
                place.insert(Box::new(listing))
            }
            (place @ None, Request::Summary(columns)) => {
                place.insert(Box::new(Summing::new(&cx, rows, columns.clone())))
            }
        };
        let step = work.step(&mut cx)?;
        asked.done = asked.done.saturating_add(step.cost);
        Ok(step)
    }

    /// Takes what a job came to.
    fn finish(&mut self, job: Job) {
        let asked = self.asked.get_mut(job as usize).and_then(Option::take);
        match asked
            .and_then(|asked| asked.work)
            .map(|mut work| work.finish())
        {
            Some(Outcome::View(spec, rows, view)) => {
                (self.spec, self.rows, self.view) = (spec, rows, view);
            }
            Some(Outcome::Values(values)) => self.values = Some(values),
            Some(Outcome::Summary(summary)) => self.summary = Some(summary),
            None => {}
        }
        self.clean_up();
    }
}
