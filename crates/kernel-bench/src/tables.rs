//! Tables for tests from outside of the engine: written, imported, read back,
//! and asked questions made up at random.

use reticula_core::rng::Rng;
use reticula_table::csv;
use reticula_table::layout::PAGE_BYTES;
use reticula_table::parquet::{self, ParquetError};
use reticula_table::query::{
    ColumnSummary, Condition, NumberOp, Progress, QueryError, Session, SortKey, Spec, TimeOp,
    Value, ValueList,
};
use reticula_table::reader::{Opened, ReadError, Reader};
use reticula_table::source::{ByteRange, Source, SourceError};
use reticula_table::text::write_decimal;
use reticula_table::types::{CellValue, ColumnType};
use reticula_table::writer::{ColumnSpec, TableWriter};

/// A cell.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Blank,
    Int(i64),
    Float(f64),
    Text(String),
}

/// A column to be written.
pub struct Column {
    pub name: String,
    /// `None` when it is to be decided by the first value, which is a text.
    pub kind: Option<ColumnType>,
    pub dictionary: bool,
    pub cells: Vec<Cell>,
}

/// Writes columns as the importers do.
pub fn write(columns: &[Column], group_rows: u32) -> Vec<u8> {
    let specs = columns
        .iter()
        .map(|column| ColumnSpec {
            name: column.name.clone(),
            kind: column.kind,
            dictionary: column.dictionary,
        })
        .collect();
    let mut writer = TableWriter::new(specs, group_rows).expect("columns can be written");
    let mut file = Vec::new();
    let rows = columns.first().map_or(0, |column| column.cells.len());
    for row in 0..rows {
        for (builder, column) in writer.columns().iter_mut().zip(columns) {
            match &column.cells[row] {
                Cell::Blank => builder.push_blank(),
                Cell::Int(value) => builder.push_int(*value),
                Cell::Float(value) => builder.push_float(*value),
                Cell::Text(value) => {
                    builder.set_kind(ColumnType::Text);
                    builder.push_text(value);
                }
            }
        }
        if writer.check_group().expect("the texts are short") {
            writer.seal_group().expect("a group is full");
            while writer.step(&mut file).expect("memory takes it") {}
        }
    }
    writer.seal_group().expect("the last group may be short");
    while writer.step(&mut file).expect("memory takes it") {}
    writer.finish();
    while writer.step(&mut file).expect("memory takes it") {}
    file
}

/// Opens a file that is here completely.
///
/// # Errors
///
/// [`ReadError`] when it is not a table.
pub fn open(file: &[u8]) -> Result<Reader, ReadError> {
    let mut source = file;
    match Reader::open(&mut source, file.len() as u64)? {
        Opened::Ready(reader) => Ok(*reader),
        Opened::Need(_) => Err(ReadError::Missing),
    }
}

/// Imports delimited text. `piece` says how long the next piece is that is
/// given, and whether it is given although nobody asked.
///
/// # Errors
///
/// [`csv::ImportError`] when it cannot be imported.
pub fn import_csv(
    options: &csv::Options,
    file: &[u8],
    piece: &mut dyn FnMut() -> (usize, bool),
) -> Result<(Vec<u8>, csv::Report), csv::ImportError> {
    let head = &file[..file.len().min(csv::HEAD_BYTES)];
    let mut import = csv::Import::new(options, head)?;
    let mut sink = Vec::new();
    let mut given = 0;
    let mut next = piece();
    let mut idle = 0_u32;
    loop {
        let wanted = match import.step(&mut sink)? {
            csv::Step::NeedInput => true,
            csv::Step::Working => next.1 && given < file.len(),
            csv::Step::Restart => {
                sink.clear();
                given = 0;
                false
            }
            csv::Step::Done => break,
        };
        idle += 1;
        assert!(idle < 10_000_000, "the import does not come to an end");
        if wanted && given < file.len() {
            let end = file.len().min(given + next.0);
            if import.feed(&file[given..end]).is_ok() {
                given = end;
            }
            next = piece();
        } else if wanted {
            import.end_of_input();
        }
    }
    let report = import.report().expect("the import is done");
    Ok((sink, report))
}

/// Imports delimited text a mebibyte at a time.
///
/// # Errors
///
/// [`csv::ImportError`] when it cannot be imported.
pub fn import_csv_whole(
    options: &csv::Options,
    file: &[u8],
) -> Result<(Vec<u8>, csv::Report), csv::ImportError> {
    import_csv(options, file, &mut || (1 << 20, false))
}

/// Imports a Parquet file.
///
/// # Errors
///
/// [`ParquetError`] when it cannot be imported.
pub fn import_parquet<S: Source>(
    source: &mut S,
    length: u64,
    need: &mut dyn FnMut(&mut S, &[ByteRange]),
) -> Result<(Vec<u8>, parquet::Report), ParquetError> {
    let file = loop {
        match parquet::open(source, length)? {
            parquet::Opened::Need(range) => need(source, &[range]),
            parquet::Opened::Ready(file) => break file,
        }
    };
    let mut import = parquet::Import::new(file, &parquet::Options::default())?;
    let mut sink = Vec::new();
    let mut steps = 0_u32;
    loop {
        steps += 1;
        assert!(steps < 10_000_000, "the import does not come to an end");
        match import.step(source, &mut sink)? {
            parquet::Step::Working => {}
            parquet::Step::Need(ranges) => need(source, &ranges),
            parquet::Step::Restart => sink.clear(),
            parquet::Step::Done => break,
        }
    }
    Ok((sink, import.report()))
}

/// Makes the checksums of a file right, whose data ends at `data_len`.
pub fn reseal(file: &mut [u8], data_len: usize) {
    use sha2::{Digest, Sha256};
    let page_len = PAGE_BYTES as usize;
    let end = file.len() - 64;
    let pages = data_len / page_len;
    for page in 0..pages {
        let sum = reticula_table::hash::checksum(&file[page * page_len..(page + 1) * page_len]);
        let at = end - (pages - page) * 4;
        file[at..at + 4].copy_from_slice(&sum.to_le_bytes());
    }
    let sha256: [u8; 32] = Sha256::digest(&file[data_len..end]).into();
    file[end + 16..end + 48].copy_from_slice(&sha256);
}

/// A file of which some pieces are not here.
pub struct Pieces<'a> {
    file: &'a [u8],
    piece: u64,
    here: Vec<bool>,
}

impl<'a> Pieces<'a> {
    pub fn new(file: &'a [u8], piece: u64) -> Self {
        Self {
            file,
            piece,
            here: vec![false; file.len().div_ceil(piece as usize)],
        }
    }

    /// Fetches what is missing. All of it has to be in the file, and something
    /// of it must not be here: nobody is to wait for nothing.
    pub fn fetch(&mut self, ranges: &[ByteRange]) {
        let mut new = false;
        for range in ranges {
            for piece in range.at / self.piece..range.end().div_ceil(self.piece) {
                let here = &mut self.here[piece as usize];
                new |= !*here;
                *here = true;
            }
        }
        assert!(new, "asked for what is here already: {ranges:?}");
    }
}

impl Source for Pieces<'_> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        let end = at + into.len() as u64;
        if end > self.file.len() as u64 {
            return Err(SourceError::Failed);
        }
        for piece in at / self.piece..end.div_ceil(self.piece) {
            if !self.here[piece as usize] {
                return Err(SourceError::Missing);
            }
        }
        let mut file = self.file;
        file.read(at, into)
    }
}

/// Works on all jobs until they are done.
///
/// # Errors
///
/// [`QueryError`] of the first job that cannot be completed.
pub fn run<S: Source>(
    session: &mut Session,
    source: &mut S,
    budget: &mut dyn FnMut() -> u32,
    need: &mut dyn FnMut(&mut S, &[ByteRange]),
) -> Result<(), QueryError> {
    let mut steps = 0_u64;
    while session.running().is_some() {
        steps += 1;
        assert!(steps < 50_000_000, "the jobs do not come to an end");
        if let Progress::Need(ranges) = session.step(source, budget())? {
            need(source, &ranges);
        }
    }
    Ok(())
}

/// The rows of the view.
#[must_use]
pub fn rows_of(session: &Session) -> Vec<u32> {
    (0..session.view_len())
        .map(|position| session.view_row(position).expect("a position of the view"))
        .collect()
}

/// A table, and the file it is in.
pub struct Table<'a> {
    pub session: Session,
    file: &'a [u8],
}

/// What a step of a job may work on.
const BUDGET: u32 = 1 << 20;

impl<'a> Table<'a> {
    /// # Errors
    ///
    /// [`ReadError`] when it is not a table.
    pub fn open(file: &'a [u8]) -> Result<Self, ReadError> {
        Ok(Self {
            session: Session::new(open(file)?),
            file,
        })
    }

    fn run(&mut self) -> Result<(), QueryError> {
        let mut source = self.file;
        run(
            &mut self.session,
            &mut source,
            &mut || BUDGET,
            &mut |_, ranges| panic!("all of the file is here, but not {ranges:?}"),
        )
    }

    /// Shows a view.
    ///
    /// # Errors
    ///
    /// [`QueryError`] when it cannot be built.
    ///
    /// # Panics
    ///
    /// When this cannot be asked of the table.
    pub fn view(&mut self, spec: &Spec) -> Result<Vec<u32>, QueryError> {
        self.session
            .begin(spec.clone())
            .unwrap_or_else(|error| panic!("{error:?}: {spec:?}"));
        self.run()?;
        Ok(rows_of(&self.session))
    }

    /// # Errors
    ///
    /// [`QueryError`] when they cannot be worked out.
    pub fn summary(&mut self, columns: Vec<usize>) -> Result<Vec<ColumnSummary>, QueryError> {
        self.session
            .begin_summary(columns)
            .expect("the columns are there");
        self.run()?;
        Ok(self.session.take_summary().expect("the summary is done"))
    }

    /// # Errors
    ///
    /// [`QueryError`] when they cannot be listed.
    pub fn values(
        &mut self,
        column: usize,
        search: &str,
        limit: usize,
    ) -> Result<ValueList, QueryError> {
        self.session
            .begin_values(column, search.to_owned(), limit)
            .expect("the column is there");
        self.run()?;
        Ok(self.session.take_values().expect("the list is done"))
    }

    /// # Errors
    ///
    /// [`ReadError`] when it cannot be read.
    pub fn cell(&self, column: usize, row: u32) -> Result<Cell, ReadError> {
        let mut reader = open(self.file)?;
        read_cell(&mut reader, self.file, column, row)
    }
}

/// # Errors
///
/// [`ReadError`] when it cannot be read.
pub fn read_cell(
    reader: &mut Reader,
    file: &[u8],
    column: usize,
    row: u32,
) -> Result<Cell, ReadError> {
    let mut source = file;
    let mut text = String::new();
    Ok(
        match reader.cell(&mut source, column, row, usize::MAX, &mut text)? {
            CellValue::Blank => Cell::Blank,
            CellValue::Int(value) => Cell::Int(value),
            CellValue::Float(value) => Cell::Float(value),
            CellValue::Text => Cell::Text(text),
        },
    )
}

/// All cells of a table, by column.
///
/// # Errors
///
/// [`ReadError`] when they cannot be read.
pub fn read_all(file: &[u8]) -> Result<(Vec<ColumnType>, Vec<Vec<Cell>>), ReadError> {
    let mut reader = open(file)?;
    let kinds: Vec<ColumnType> = reader.columns().iter().map(|column| column.kind).collect();
    let rows = reader.rows();
    let columns = (0..kinds.len())
        .map(|column| {
            (0..rows)
                .map(|row| read_cell(&mut reader, file, column, row))
                .collect()
        })
        .collect::<Result<_, _>>()?;
    Ok((kinds, columns))
}

// Questions.

pub fn pick<'a, T>(rng: &mut Rng, from: &'a [T]) -> &'a T {
    &from[rng.next_below(from.len() as u64) as usize]
}

/// Where the values of questions come from.
pub trait Sampler {
    /// The types of the columns.
    fn kinds(&self) -> &[ColumnType];
    /// Some cell of a column. Blank when there is none.
    fn sample(&mut self, rng: &mut Rng, column: usize) -> Cell;
}

/// Cells in memory, by column.
pub struct InMemory<'a> {
    pub kinds: &'a [ColumnType],
    pub columns: &'a [Vec<Cell>],
}

impl Sampler for InMemory<'_> {
    fn kinds(&self) -> &[ColumnType] {
        self.kinds
    }

    fn sample(&mut self, rng: &mut Rng, column: usize) -> Cell {
        let cells = &self.columns[column];
        let row = rng.next_below(cells.len().max(1) as u64) as usize;
        cells.get(row).cloned().unwrap_or(Cell::Blank)
    }
}

/// How many decimals the integers of a column stand for.
#[must_use]
pub const fn scale_of(kind: ColumnType) -> u8 {
    match kind {
        ColumnType::Decimal { scale, .. } => scale,
        _ => 0,
    }
}

/// A number near one that is in the column, in one of many ways to write it.
pub fn some_threshold(rng: &mut Rng, from: &mut dyn Sampler, column: usize) -> String {
    let scale = scale_of(from.kinds()[column]);
    let base = match from.sample(rng, column) {
        Cell::Int(value) => {
            let mut text = String::new();
            write_decimal(i128::from(value), scale, true, &mut text);
            text
        }
        Cell::Float(value) => format!("{value}"),
        _ => format!("{}", rng.next_below(100)),
    };
    match rng.next_below(9) {
        0 if !base.contains('.') => format!("{base}.0"),
        1 if !base.contains('.') => format!("{base}.5"),
        2 if base.contains('.') => format!("{base}0001"),
        3 if !base.contains('.') => format!("{base}.00000000000000000000001"),
        4 if base.len() < 12 => format!("{base}e1"),
        5 => "-12.5e-1".to_owned(),
        6 => pick(
            rng,
            &["1e400", "-1e400", "0.1", "1e18", "99999999999999999999"],
        )
        .to_string(),
        _ => base,
    }
}

/// A value of the column, or one that is not in it.
pub fn some_value(rng: &mut Rng, from: &mut dyn Sampler, column: usize) -> Value {
    match from.sample(rng, column) {
        Cell::Blank => Value::Blank,
        Cell::Int(value) => Value::Int(value.saturating_add((rng.next_below(8) == 0).into())),
        Cell::Float(value) => Value::Float(value.to_bits()),
        Cell::Text(text) => Value::Text(match rng.next_below(3) {
            0 => text.to_uppercase(),
            1 => text.to_lowercase(),
            _ => text,
        }),
    }
}

/// Some of a text of the column, or of another one.
pub fn some_part(rng: &mut Rng, from: &mut dyn Sampler, column: usize) -> String {
    let text = match from.sample(rng, column) {
        Cell::Text(text) => text,
        _ => pick(rng, &["", "a", "North", "customer-0001", "Σ", "é"]).to_string(),
    };
    let chars: Vec<char> = text.chars().take(40).collect();
    let start = rng.next_below(chars.len() as u64 + 1) as usize;
    let end = start + rng.next_below((chars.len() - start) as u64 + 1) as usize;
    let part: String = match rng.next_below(3) {
        0 => chars[..end].iter().collect(),
        1 => chars[start..].iter().collect(),
        _ => chars[start..end].iter().collect(),
    };
    if rng.next_below(3) == 0 {
        part.to_uppercase()
    } else {
        part
    }
}

pub fn some_condition(rng: &mut Rng, from: &mut dyn Sampler, column: usize) -> Condition {
    let kind = from.kinds()[column];
    match rng.next_below(10) {
        0 => Condition::Blank,
        1 => Condition::NotBlank,
        2..=4 => {
            let count = *pick(rng, &[0, 1, 1, 2, 2, 5, 5, 40, 40, 400]);
            Condition::OneOf {
                values: (0..count).map(|_| some_value(rng, from, column)).collect(),
            }
        }
        _ if kind == ColumnType::Text => {
            let part = some_part(rng, from, column);
            match rng.next_below(3) {
                0 => Condition::TextContains(part),
                1 => Condition::TextStartsWith(part),
                _ => Condition::TextEndsWith(part),
            }
        }
        _ if kind == ColumnType::Boolean => Condition::OneOf {
            values: vec![Value::Int(rng.next_below(2) as i64)],
        },
        5 => Condition::Top {
            n: *pick(rng, &[0, 1, 2, 10, 100, 70_000, 10_000_000]),
            bottom: rng.next_below(2) == 0,
        },
        _ if kind.is_number() => {
            let op = *pick(
                rng,
                &[
                    NumberOp::Equal,
                    NumberOp::NotEqual,
                    NumberOp::Greater,
                    NumberOp::AtLeast,
                    NumberOp::Less,
                    NumberOp::AtMost,
                    NumberOp::Between,
                ],
            );
            Condition::Number {
                op,
                a: some_threshold(rng, from, column),
                b: (op == NumberOp::Between).then(|| some_threshold(rng, from, column)),
            }
        }
        _ => {
            let mut time = |rng: &mut Rng| match from.sample(rng, column) {
                Cell::Int(value) => value + rng.next_below(3) as i64 - 1,
                _ => *pick(rng, &[0, 19_000, 1_700_000_000_000_000]),
            };
            let op = *pick(
                rng,
                &[TimeOp::Before, TimeOp::After, TimeOp::Between, TimeOp::On],
            );
            Condition::Time {
                op,
                a: time(rng),
                b: (op == TimeOp::Between).then(|| time(rng)),
            }
        }
    }
}

pub fn some_filters(rng: &mut Rng, from: &mut dyn Sampler) -> Vec<(usize, Condition)> {
    let width = from.kinds().len() as u64;
    (0..*pick(rng, &[0, 1, 1, 1, 2, 3]))
        .map(|_| {
            let column = rng.next_below(width) as usize;
            (column, some_condition(rng, from, column))
        })
        .collect()
}

pub fn some_order(rng: &mut Rng, width: usize) -> Vec<SortKey> {
    (0..*pick(rng, &[0, 1, 1, 2, 3]))
        .map(|_| SortKey {
            column: rng.next_below(width as u64) as usize,
            descending: rng.next_below(2) == 0,
        })
        .collect()
}
