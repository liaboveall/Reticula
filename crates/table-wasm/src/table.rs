//! An open table: its pages, the texts of its cells, and questions about it.

use reticula_core::csv::write_escaped_field;
use reticula_core::viewport::{PAGE_COLS, PAGE_ROWS, PageFrame};
use reticula_table::query::{Job, Progress, QueryError, Session};
use reticula_table::reader::{Opened, ReadError, Reader};
use reticula_table::source::{AnySource, Source, SourceError};
use reticula_table::types::ColumnType;
use wasm_bindgen::prelude::*;

use crate::file::{JsFile, TableFile, from_offset, offset};
use crate::json::{Value, json};
use crate::outcome::{TableStep, failed};
use crate::pages::{Cells, Pages};
use crate::query::{key_of, spec};
use crate::remote::{Blocks, Taken};
use crate::wire::{
    TableColumn, TableColumnSummary, TableFigure, TableInfo, TableSummary, TableTrouble,
    TableValue, TableValues,
};

/// Cells that are copied in one step.
const COPY_CELLS: usize = 2_048;
/// Bytes of text that can be copied.
const MAX_COPY_BYTES: usize = 64 << 20;

/// Opens a file as everybody does who reads all of it.
pub fn open(source: &mut dyn Source, len: u64) -> Result<Opened, ReadError> {
    Reader::open(&mut AnySource(source), len)
}

/// Where the bytes of a table come from.
struct Bytes {
    /// Its file, if that is in this browser.
    file: Option<JsFile>,
    /// What of it has come from the server, where it is otherwise.
    blocks: Option<Blocks<JsFile>>,
}

impl Source for Bytes {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        let Some(blocks) = &mut self.blocks else {
            let file = self.file.as_mut().ok_or(SourceError::Failed)?;
            return file.read(at, into);
        };
        if let Some(file) = &mut self.file {
            if file.read(at, into).is_ok() {
                return Ok(());
            }
            // It was uploaded, and is on the server from now on.
            self.file = None;
        }
        blocks.read(at, into)
    }
}

/// A table that is open, or is being opened.
#[wasm_bindgen]
pub struct Table {
    bytes: Bytes,
    len: u64,
    session: Option<Session>,
    /// Whether what could not be read last will come.
    is_waiting: bool,
    pages: Pages,
    /// What was subscribed to, for the view there is.
    cells: Option<Cells>,
    frame: PageFrame,
    scratch: String,
    copy: Option<Copy>,
    share: f64,
}

struct Copy {
    first_row: u32,
    row: u32,
    end_row: u32,
    first_col: u32,
    end_col: u32,
    text: String,
    html: String,
}

#[wasm_bindgen]
impl Table {
    /// A table in a file of that length. It has to be opened.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(file: Option<TableFile>, len: f64) -> Self {
        Self {
            bytes: Bytes {
                file: file.map(JsFile),
                blocks: None,
            },
            len: from_offset(len),
            session: None,
            is_waiting: false,
            pages: Pages::default(),
            cells: None,
            frame: PageFrame {
                sheet: 0,
                first_row: 0,
                first_col: 0,
                epoch: 0,
            },
            scratch: String::new(),
            copy: None,
            share: 0.0,
        }
    }

    /// Has the table come from the server as far as there is no file of it, which
    /// from the second call on there is not. What has come is kept in a log, of
    /// which there are so many bytes, or in memory. Gives where the log ends.
    pub fn fetch(&mut self, log: Option<TableFile>, len: f64) -> f64 {
        if self.bytes.blocks.is_some() {
            self.bytes.file = None;
        }
        let blocks = self
            .bytes
            .blocks
            .get_or_insert_with(|| Blocks::new(self.len));
        if let Some(log) = log {
            blocks.attach(JsFile(log), from_offset(len));
        }
        offset(blocks.log_end())
    }

    /// Bytes of the table that are here.
    #[must_use]
    pub fn have(&self) -> f64 {
        match (&self.bytes.file, &self.bytes.blocks) {
            (None, Some(blocks)) => offset(blocks.have()),
            _ => offset(self.len),
        }
    }

    /// The requests to make, and those to give up: the number of each, and the
    /// offsets of its first byte and of the one after its last, which are equal
    /// for one to give up. There are so many at once unless something is waited for.
    pub fn plan(&mut self, requests: usize) -> Vec<f64> {
        match &mut self.bytes.blocks {
            Some(blocks) => blocks.plan(requests),
            None => Vec::new(),
        }
    }

    /// Takes in what has come, from an offset on. `Restart`: there is no room for
    /// the log, which is not used any more.
    pub fn take(&mut self, at: f64, bytes: &[u8]) -> TableStep {
        let taken = self
            .bytes
            .blocks
            .as_mut()
            .map(|blocks| blocks.take(from_offset(at), bytes));
        match taken {
            None | Some(Taken::Kept) => TableStep::Working,
            Some(Taken::LogFull) => {
                self.pages.arrived(|_| true);
                TableStep::Restart
            }
            Some(Taken::NoMemory) => failed(TableTrouble::Memory),
        }
    }

    /// A request has ended, whatever came of it.
    pub fn ended(&mut self, request: u32) {
        if let Some(blocks) = &mut self.bytes.blocks {
            blocks.ended(request);
        }
    }

    /// Lets go of the log. What is in it is not here any more.
    #[wasm_bindgen(js_name = leaveLog)]
    pub fn leave_log(&mut self) {
        if let Some(blocks) = &mut self.bytes.blocks {
            blocks.leave_log();
        }
        // They find out anew what they wait for.
        self.pages.arrived(|_| true);
    }

    /// Whether what could not be read last will come.
    #[must_use]
    #[wasm_bindgen(js_name = isWaiting)]
    pub fn is_waiting(&self) -> bool {
        self.is_waiting
    }

    /// Reads the directory of the file.
    pub fn open(&mut self) -> TableStep {
        match open(&mut self.bytes, self.len) {
            Ok(Opened::Ready(reader)) => {
                self.session = Some(Session::new(*reader));
                TableStep::Done
            }
            Ok(Opened::Need(range)) => {
                if let Some(blocks) = &mut self.bytes.blocks {
                    blocks.want_bytes(range.at, range.len);
                }
                self.unread(ReadError::Missing)
            }
            Err(error) => self.unread(error),
        }
    }

    /// What there is to a question that was answered, or otherwise what the table
    /// holds.
    #[must_use]
    #[wasm_bindgen(unchecked_return_type = "Json<TableSummary | TableValues | TableInfo>")]
    pub fn answer(&mut self, job: TableJob) -> String {
        match job {
            TableJob::Summary => self.take_summary(),
            TableJob::Values => self.take_values(),
            TableJob::None | TableJob::View => self.info(),
        }
    }

    fn info(&self) -> String {
        let reader = self.session.as_ref().map(Session::reader);
        let info = TableInfo {
            rows: reader.map_or(0, Reader::rows),
            columns: reader
                .into_iter()
                .flat_map(Reader::columns)
                .map(|column| TableColumn {
                    name: column.name.clone(),
                    kind: column.kind.into(),
                    blanks: u32::try_from(column.blanks).unwrap_or(u32::MAX),
                    dictionary: column.dictionary.map(|dictionary| dictionary.entries),
                })
                .collect(),
        };
        json(&info)
    }

    /// Sets how many bytes of memory this may take, about.
    #[wasm_bindgen(js_name = setMemoryLimit)]
    pub fn set_memory_limit(&mut self, bytes: f64) {
        let bytes = usize::try_from(from_offset(bytes)).unwrap_or(usize::MAX);
        if let Some(session) = &mut self.session {
            session.set_memory_limit(bytes);
        }
    }

    /// Bytes of memory this takes, about.
    #[must_use]
    pub fn memory(&self) -> usize {
        self.session.as_ref().map_or(0, Session::memory)
    }

    /// Says which cells pages are to be had of: the frozen columns, the first row
    /// and the number of rows, the same of columns, and the rows and columns on
    /// screen from the first to before the last. With `resend` all of them.
    pub fn subscribe(&mut self, sheet: u32, epoch: u32, resend: bool, cells: &[u32]) {
        if let [
            frozen_cols,
            first_row,
            rows,
            first_col,
            cols,
            seen_row,
            seen_row_end,
            seen_col,
            seen_col_end,
        ] = *cells
        {
            let cells = Cells {
                frozen_cols,
                first_row,
                rows,
                first_col,
                cols,
                seen_row,
                seen_row_end,
                seen_col,
                seen_col_end,
            };
            self.frame = PageFrame {
                sheet,
                epoch,
                ..self.frame
            };
            self.cells = Some(cells);
            self.hold(&cells, resend);
            if let Some(blocks) = &mut self.bytes.blocks {
                blocks.forget();
            }
        }
    }

    /// Ends the subscription. What is on screen when the view is shown from its
    /// start is still to be had. Gives the rows of the view.
    #[wasm_bindgen(js_name = toTop)]
    pub fn to_top(&mut self) -> u32 {
        self.pages.clear();
        if let Some(cells) = self.cells.take() {
            self.hold(&cells.at_top(), true);
        }
        self.view_rows()
    }

    /// The next page. None when there is none that can be had now, and an empty
    /// one when it never can: see `lastTrouble`.
    #[wasm_bindgen(js_name = nextPage)]
    pub fn next_page(&mut self) -> Option<Vec<u8>> {
        if let Some(blocks) = &mut self.bytes.blocks {
            self.pages.arrived(|block| blocks.has(block));
            blocks.take_missed();
        }
        while let Some(place) = self.pages.pop() {
            let frame = PageFrame {
                first_row: place.row.saturating_mul(PAGE_ROWS),
                first_col: place.col.saturating_mul(PAGE_COLS),
                ..self.frame
            };
            match self
                .session
                .as_mut()?
                .page(&mut self.bytes, frame, &mut self.scratch)
            {
                Ok(page) => return Some(page),
                Err(error) => {
                    if self.unread(error) == TableStep::Failed {
                        self.pages.clear();
                        return Some(Vec::new());
                    }
                    let missed = self.bytes.blocks.as_mut().map(Blocks::take_missed);
                    self.pages.wait(place, missed.unwrap_or_default());
                }
            }
        }
        None
    }

    /// The whole text of a cell. None when it cannot be had, or not yet.
    pub fn text(&mut self, row: u32, col: u32) -> Option<String> {
        let session = self.session.as_mut()?;
        match session.cell_text(&mut self.bytes, row, col as usize) {
            Ok(text) => Some(text),
            Err(error) => {
                self.unread(error);
                None
            }
        }
    }

    /// Starts to copy cells.
    #[wasm_bindgen(js_name = beginCopy)]
    pub fn begin_copy(&mut self, first_row: u32, rows: u32, first_col: u32, cols: u32) {
        self.copy = Some(Copy {
            first_row,
            row: first_row,
            end_row: first_row.saturating_add(rows).min(self.view_rows()),
            first_col,
            end_col: first_col.saturating_add(cols),
            text: String::new(),
            html: String::from("<table><tbody>"),
        });
    }

    /// Copies some more.
    #[wasm_bindgen(js_name = stepCopy)]
    pub fn step_copy(&mut self) -> TableStep {
        match (&mut self.copy, &mut self.session) {
            (Some(copy), Some(session)) => match copy.step(session, &mut self.bytes) {
                Ok(step) => {
                    self.is_waiting = false;
                    step
                }
                Err(error) => self.unread(error),
            },
            _ => TableStep::Done,
        }
    }

    /// What was copied: text with tabs, and once that was taken a table in HTML.
    #[must_use]
    #[wasm_bindgen(js_name = takeCopy)]
    pub fn take_copy(&mut self, html: bool) -> String {
        match &mut self.copy {
            Some(copy) if !html => std::mem::take(&mut copy.text),
            copy => copy.take().map_or_else(String::new, |copy| copy.html),
        }
    }

    /// Starts to answer a question, which is in JSON: for another view a
    /// `TableQuery`, for totals over the rows of the view the columns, and for the
    /// values of a column that contain a text the column, the text and how many.
    pub fn ask(&mut self, job: TableJob, question: &str) -> TableStep {
        let Some(session) = &mut self.session else {
            return failed(ReadError::Failed);
        };
        let asked = Value::read(question);
        let begun: Result<(), TableTrouble> = match job {
            TableJob::Summary => {
                let columns = asked.items().iter();
                let columns = columns.map(|(_, column)| column.number()).collect();
                session.begin_summary(columns).map_err(Into::into)
            }
            TableJob::Values => {
                let (column, limit) = (asked.field("column"), asked.field("limit"));
                let search = String::from(asked.of("search"));
                session
                    .begin_values(column.number(), search, limit.number())
                    .map_err(Into::into)
            }
            TableJob::None | TableJob::View => {
                let columns = session.reader().columns().iter();
                let kinds: Vec<ColumnType> = columns.map(|column| column.kind).collect();
                spec(&asked, &kinds).and_then(|spec| session.begin(spec).map_err(Into::into))
            }
        };
        begun.map_or_else(failed, |()| TableStep::Working)
    }

    /// Gives up something that was asked.
    pub fn cancel(&mut self, job: TableJob) {
        if let (Some(session), Some(job)) = (&mut self.session, job.into()) {
            session.cancel_job(job);
        }
    }

    /// What the next step will work on.
    #[must_use]
    pub fn job(&self) -> TableJob {
        match self.session.as_ref().and_then(Session::running) {
            None => TableJob::None,
            Some(Job::View) => TableJob::View,
            Some(Job::Values) => TableJob::Values,
            Some(Job::Summary) => TableJob::Summary,
        }
    }

    /// Works on that: about so many cells. After `Restart` it has to be asked again.
    pub fn step(&mut self, cells: u32) -> TableStep {
        let Some(session) = &mut self.session else {
            return TableStep::Done;
        };
        match session.step(&mut self.bytes, cells) {
            Ok(Progress::Working { done, total }) => {
                self.share = offset(done) / offset(total.max(1));
                TableStep::Working
            }
            Ok(Progress::Need(_)) => self.unread(ReadError::Missing),
            Ok(Progress::Done) => TableStep::Done,
            Err(QueryError::Read(error)) => match self.unread(error) {
                TableStep::Need => TableStep::Restart,
                step => step,
            },
            Err(error) => failed(error),
        }
    }

    /// How much of it is done, from 0 to 1.
    #[must_use]
    pub fn share(&self) -> f64 {
        self.share
    }

    fn take_summary(&mut self) -> String {
        let found = self.session.as_mut().and_then(Session::take_summary);
        let summary = TableSummary {
            columns: found
                .into_iter()
                .flatten()
                .map(|column| TableColumnSummary {
                    column: u32::try_from(column.column).unwrap_or(u32::MAX),
                    rows: u32::try_from(column.rows).unwrap_or(u32::MAX),
                    blanks: u32::try_from(column.blanks).unwrap_or(u32::MAX),
                    distinct: column
                        .distinct
                        .map(|count| u32::try_from(count).unwrap_or(u32::MAX)),
                    sum: column.sum.map(TableFigure::from),
                    mean: column.mean.map(TableFigure::from),
                    min: column.min.map(TableFigure::from),
                    max: column.max.map(TableFigure::from),
                })
                .collect(),
        };
        json(&summary)
    }

    fn take_values(&mut self) -> String {
        let found = self.session.as_mut().and_then(Session::take_values);
        let values = found.map_or_else(TableValues::default, |list| TableValues {
            values: list
                .values
                .into_iter()
                .map(|value| TableValue {
                    key: key_of(&value.value),
                    text: value.text,
                    count: u32::try_from(value.count).unwrap_or(u32::MAX),
                })
                .collect(),
            blanks: u32::try_from(list.blanks).unwrap_or(u32::MAX),
            complete: list.complete,
        });
        json(&values)
    }
}

impl Table {
    /// What it comes to that something cannot be read: what is missing of a file
    /// on the server comes, and what is damaged does so once more.
    fn unread(&mut self, error: ReadError) -> TableStep {
        if let Some(session) = &mut self.session {
            session.take_missing();
        }
        let is_here = self.bytes.file.is_some();
        self.is_waiting = match (&mut self.bytes.blocks, error) {
            (Some(_), ReadError::Missing) => true,
            (Some(blocks), ReadError::Corrupt(_)) => !is_here && blocks.mend(),
            _ => false,
        };
        if self.is_waiting {
            TableStep::Need
        } else {
            failed(error)
        }
    }

    /// Rows of the view.
    fn view_rows(&self) -> u32 {
        self.session.as_ref().map_or(0, Session::view_len)
    }

    fn hold(&mut self, cells: &Cells, resend: bool) {
        if let Some(session) = &self.session {
            let cols = u32::try_from(session.reader().columns().len()).unwrap_or(0);
            self.pages
                .subscribe(session.view_len(), cols, cells, resend);
        }
    }
}

impl Copy {
    fn step(&mut self, session: &mut Session, bytes: &mut Bytes) -> Result<TableStep, ReadError> {
        let mut cells = 0_usize;
        let mut line = String::new();
        let mut html = String::new();
        while self.row < self.end_row {
            if cells >= COPY_CELLS {
                return Ok(TableStep::Working);
            }
            line.clear();
            html.clear();
            let mut missing = false;
            for col in self.first_col..self.end_col {
                let cell = match session.cell_text(bytes, self.row, col as usize) {
                    Ok(text) => text,
                    Err(ReadError::Missing) => {
                        missing = true;
                        String::new()
                    }
                    Err(error) => return Err(error),
                };
                if col > self.first_col {
                    line.push('\t');
                }
                write_escaped_field(&cell, b'\t', &mut line);
                html.push_str("<td>");
                write_html(&cell, &mut html);
                html.push_str("</td>");
                cells = cells.saturating_add(1);
            }
            if missing {
                return Err(ReadError::Missing);
            }
            if self.row > self.first_row {
                self.text.push('\n');
            }
            self.text.push_str(&line);
            self.html.push_str("<tr>");
            self.html.push_str(&html);
            self.html.push_str("</tr>");
            if self.text.len() > MAX_COPY_BYTES {
                return Ok(failed(TableTrouble::Limit {
                    detail: "too much text to copy",
                }));
            }
            self.row = self.row.saturating_add(1);
        }
        self.html.push_str("</tbody></table>");
        Ok(TableStep::Done)
    }
}

fn write_html(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' => out.push_str("<br>"),
            _ => out.push(ch),
        }
    }
}

/// What can be asked of a table, one of each at a time.
#[wasm_bindgen]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableJob {
    /// Nothing.
    None,
    /// Another view.
    View,
    /// The values of a column.
    Values,
    /// Totals.
    Summary,
}

impl From<TableJob> for Option<Job> {
    fn from(job: TableJob) -> Self {
        match job {
            TableJob::None => None,
            TableJob::View => Some(Job::View),
            TableJob::Values => Some(Job::Values),
            TableJob::Summary => Some(Job::Summary),
        }
    }
}
