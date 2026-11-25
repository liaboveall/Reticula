//! What crosses the boundary.

use reticula_table::csv::ImportError;
use reticula_table::parquet::{self, Damage, ParquetError};
use reticula_table::query::{Figure, QueryError, SpecError};
use reticula_table::reader::ReadError;
use reticula_table::types::ColumnType;
use reticula_table::writer::WriteError;
use serde::Serialize;
use tsify::Tsify;

/// What a column holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TableColumnType {
    Integer,
    /// `fixed`: every value is shown with all of its decimals.
    Decimal {
        scale: u8,
        fixed: bool,
    },
    Float,
    Date,
    DateTime,
    Boolean,
    Text,
}

impl From<ColumnType> for TableColumnType {
    fn from(kind: ColumnType) -> Self {
        match kind {
            ColumnType::Integer => Self::Integer,
            ColumnType::Decimal { scale, fixed } => Self::Decimal { scale, fixed },
            ColumnType::Float => Self::Float,
            ColumnType::Date => Self::Date,
            ColumnType::DateTime => Self::DateTime,
            ColumnType::Boolean => Self::Boolean,
            ColumnType::Text => Self::Text,
        }
    }
}

/// The type with a name that `TableColumnType` has.
pub fn type_named(name: &str, scale: u8) -> Option<ColumnType> {
    Some(match name {
        "integer" => ColumnType::Integer,
        "decimal" => ColumnType::Decimal {
            scale,
            fixed: false,
        },
        "float" => ColumnType::Float,
        "date" => ColumnType::Date,
        "dateTime" => ColumnType::DateTime,
        "boolean" => ColumnType::Boolean,
        "text" => ColumnType::Text,
        _ => return None,
    })
}

/// One column of an open table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
#[tsify(missing_as_null)]
pub struct TableColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: TableColumnType,
    /// Number of blank cells.
    pub blanks: u32,
    /// Number of texts of the dictionary, if it has one.
    pub dictionary: Option<u32>,
}

/// What an open table holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableInfo {
    pub rows: u32,
    pub columns: Vec<TableColumn>,
}

/// Why something could not be done. `detail` is for whoever looks into it. Rows
/// are those of a text file, counted from 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum TableTrouble {
    /// The file is damaged, or is none of a table.
    Corrupt { detail: &'static str },
    /// The file cannot be read or written.
    Io,
    /// There is not enough memory.
    Memory,
    /// A limit was reached.
    Limit { detail: &'static str },
    /// What was asked does not fit the column.
    Invalid { column: u32, detail: &'static str },
    /// A quote is never closed.
    UnclosedQuote { row: u64 },
    /// A field in quotes is too long.
    FieldTooLong { row: u64 },
    /// The text is not UTF-8.
    NotUtf8 { row: u64 },
    /// There are too many columns.
    TooManyColumns,
    /// There is nothing in the file, or nothing that can be taken.
    Empty,
    /// The file is not the one that was read before.
    Changed,
    /// The file is none of Parquet.
    NotParquet,
    /// The file is encrypted.
    Encrypted,
}

impl TableTrouble {
    pub fn invalid(column: usize, detail: &'static str) -> Self {
        Self::Invalid {
            column: u32::try_from(column).unwrap_or(u32::MAX),
            detail,
        }
    }
}

impl From<ReadError> for TableTrouble {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Corrupt(detail) => Self::Corrupt { detail },
            ReadError::Failed | ReadError::Missing => Self::Io,
            ReadError::OutOfMemory => Self::Memory,
        }
    }
}

impl From<QueryError> for TableTrouble {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::Read(error) => error.into(),
            QueryError::OutOfMemory => Self::Memory,
            QueryError::TooManyEqualPrefixes => Self::Limit {
                detail: "too many texts begin alike",
            },
        }
    }
}

impl From<SpecError> for TableTrouble {
    fn from(error: SpecError) -> Self {
        match error {
            SpecError::NoColumn { column } => Self::invalid(column, "no such column"),
            SpecError::DoesNotApply { column } => Self::invalid(column, "condition"),
            SpecError::NotANumber { column } => Self::invalid(column, "number"),
            SpecError::OneThreshold { column } => Self::invalid(column, "threshold"),
        }
    }
}

impl From<WriteError> for TableTrouble {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::Sink => Self::Io,
            WriteError::Limit("not enough memory") => Self::Memory,
            WriteError::Limit(detail) => Self::Limit { detail },
            WriteError::GroupTooLarge => Self::Limit {
                detail: "too much text in a group of rows",
            },
            WriteError::Uneven => Self::Corrupt {
                detail: "columns of different lengths",
            },
        }
    }
}

impl From<ImportError> for TableTrouble {
    fn from(error: ImportError) -> Self {
        match error {
            ImportError::UnclosedQuote { row } => Self::UnclosedQuote { row },
            ImportError::FieldTooLong { row } => Self::FieldTooLong { row },
            ImportError::NotUtf8 { row } => Self::NotUtf8 { row },
            ImportError::FirstRowTooLong => Self::Limit {
                detail: "the first row is too long",
            },
            ImportError::TooManyColumns => Self::TooManyColumns,
            ImportError::Empty => Self::Empty,
            ImportError::Changed => Self::Changed,
            ImportError::Refused => Self::Limit {
                detail: "too much text at once",
            },
            ImportError::Write(error) => error.into(),
        }
    }
}

impl From<ParquetError> for TableTrouble {
    fn from(error: ParquetError) -> Self {
        match error {
            ParquetError::NotParquet => Self::NotParquet,
            ParquetError::Encrypted => Self::Encrypted,
            ParquetError::NoColumns => Self::Empty,
            ParquetError::Limit(parquet::Limit::Columns) => Self::TooManyColumns,
            ParquetError::Limit(limit) => Self::Limit {
                detail: match limit {
                    parquet::Limit::Rows => "rows",
                    parquet::Limit::Columns => "columns",
                    parquet::Limit::Metadata => "metadata",
                    parquet::Limit::Page => "page",
                    parquet::Limit::Text => "text",
                },
            },
            ParquetError::Damaged { damage, .. } => Self::Corrupt {
                detail: match damage {
                    Damage::Metadata => "metadata",
                    Damage::Place => "place",
                    Damage::Count => "count",
                    Damage::Page => "page",
                    Damage::Checksum => "checksum",
                    Damage::Packed => "packed",
                    Damage::Levels => "levels",
                    Damage::Dictionary => "dictionary",
                    Damage::Encoding => "encoding",
                    Damage::Values => "values",
                },
            },
            ParquetError::Memory => Self::Memory,
            ParquetError::Source => Self::Io,
            ParquetError::Write(error) => error.into(),
        }
    }
}

/// What is asked of a table: which rows to show, and in which order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableQuery {
    /// All of them have to hold.
    pub filters: Vec<TableFilter>,
    /// The most important column first.
    pub order: Vec<TableSortKey>,
}

/// What the cells of a column have to be.
#[derive(Clone, Debug, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableFilter {
    pub column: u32,
    pub condition: TableCondition,
}

/// A column to order by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableSortKey {
    pub column: u32,
    /// Blank cells come last anyway.
    pub descending: bool,
}

/// What a cell has to be. Numbers are written in decimals of any length, dates
/// and times as cells show them.
#[derive(Clone, Debug, PartialEq, Eq, Tsify)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TableCondition {
    /// One of the values with these keys, see `TableValue`, or blank.
    OneOf {
        keys: Vec<String>,
        blank: bool,
    },
    Contains {
        text: String,
    },
    StartsWith {
        text: String,
    },
    EndsWith {
        text: String,
    },
    Number {
        op: TableNumberOp,
        a: String,
        /// The other threshold of `between`.
        #[tsify(optional)]
        b: Option<String>,
    },
    Time {
        op: TableTimeOp,
        a: String,
        /// The other one of `between`.
        #[tsify(optional)]
        b: Option<String>,
    },
    /// One of the `n` largest values, or with `bottom` one of the smallest.
    Top {
        n: u32,
        bottom: bool,
    },
    Blank,
    NotBlank,
}

/// How numbers are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum TableNumberOp {
    Equal,
    NotEqual,
    Greater,
    AtLeast,
    Less,
    AtMost,
    Between,
}

/// How dates and times are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum TableTimeOp {
    Before,
    After,
    Between,
    On,
}

/// Values of a column, in its order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableValues {
    pub values: Vec<TableValue>,
    /// Number of blank cells.
    pub blanks: u32,
    /// Whether these are all there are.
    pub complete: bool,
}

/// A value that occurs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableValue {
    /// What stands for it in a condition.
    pub key: String,
    /// What its cells show.
    pub text: String,
    /// In how many rows.
    pub count: u32,
}

/// Totals of columns over the rows of the view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableSummary {
    pub columns: Vec<TableColumnSummary>,
}

/// What there is to say of a column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
#[tsify(missing_as_null)]
pub struct TableColumnSummary {
    pub column: u32,
    pub rows: u32,
    pub blanks: u32,
    /// How many different texts there are, where that is known.
    pub distinct: Option<u32>,
    pub sum: Option<TableFigure>,
    pub mean: Option<TableFigure>,
    pub min: Option<TableFigure>,
    pub max: Option<TableFigure>,
}

/// A number or value, as cells show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct TableFigure {
    pub text: String,
    /// Whether it is exactly the figure, and not rounded.
    pub exact: bool,
}

impl From<Figure> for TableFigure {
    fn from(figure: Figure) -> Self {
        Self {
            text: figure.text,
            exact: figure.exact,
        }
    }
}

/// What became of a column of an imported file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct ImportedColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: TableColumnType,
    /// Blank cells, the ones for values that could not be taken too.
    pub blanks: u64,
    /// Values that are blank because they did not fit the type that was asked for.
    pub misfits: u64,
    /// Texts that were cut because they are too long.
    pub cut_texts: u64,
    /// Times that lost parts of a microsecond.
    pub cut_nanos: u64,
    /// Numbers that are blank because they were not finite.
    pub not_finite: u64,
    /// Dates and times that are blank because a table cannot hold them.
    pub out_of_range: u64,
}

/// Why a column of a file is not in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// Lists, maps or structures.
    Nested,
    /// Bytes that are not text, or not said to be.
    Binary,
    /// Packed or encoded in a way that cannot be read.
    Packing,
    /// Of a type that tables do not have.
    Type,
}

impl From<parquet::SkipReason> for SkipReason {
    fn from(reason: parquet::SkipReason) -> Self {
        use parquet::SkipReason as Reason;
        match reason {
            Reason::Nested => Self::Nested,
            Reason::Binary | Reason::NotText => Self::Binary,
            Reason::Codec(_) | Reason::Encoding(_) | Reason::External => Self::Packing,
            Reason::Interval
            | Reason::Bson
            | Reason::Variant
            | Reason::Geometry
            | Reason::UnknownType
            | Reason::InvalidType => Self::Type,
        }
    }
}

/// A column that is not in the table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SkippedColumn {
    pub name: String,
    pub reason: SkipReason,
}

impl From<&parquet::SkippedColumn> for SkippedColumn {
    fn from(column: &parquet::SkippedColumn) -> Self {
        Self {
            name: column.name.clone(),
            reason: column.reason.into(),
        }
    }
}

/// What became of an imported file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub columns: Vec<ImportedColumn>,
    pub skipped: Vec<SkippedColumn>,
    /// Fields that were dropped because their row has too many.
    pub extra_fields: u64,
    /// Rows that were filled up with blank cells.
    pub short_rows: u64,
    /// Lines that were skipped because there is nothing on them.
    pub empty_lines: u64,
}

/// What has to be given to `CsvSettings`: what is known about a text file beforehand.
#[derive(Clone, Debug, Default, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct CsvOptions {
    /// What separates the fields: one ASCII character. To be found out when not given.
    #[tsify(optional)]
    pub delimiter: Option<String>,
    /// Whether the first row holds the names of the columns. To be found out when not given.
    #[tsify(optional)]
    pub header: Option<bool>,
    /// Whether NA, N/A, NULL and NaN are blank cells.
    #[tsify(optional)]
    pub blank_markers: bool,
    /// What columns are to hold. Values that do not fit become blank cells.
    #[tsify(optional)]
    pub types: Vec<CsvColumnType>,
}

/// What a column is to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct CsvColumnType {
    pub column: u32,
    #[serde(rename = "type")]
    pub kind: TableColumnType,
}

/// What a text file seems to hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct CsvSniffed {
    pub delimiter: String,
    pub header: bool,
    pub columns: Vec<CsvSniffedColumn>,
    /// The texts of the first rows, without the one with the names.
    pub preview: Vec<Vec<String>>,
    /// Whether the text is UTF-8. When it is not, the rest is a guess.
    pub valid_utf8: bool,
    /// Bytes a row takes, on average.
    pub row_bytes: u32,
}

/// What a column of a text file seems to hold. No type when there is no value yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
#[tsify(missing_as_null)]
pub struct CsvSniffedColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<TableColumnType>,
}

/// What is in a Parquet file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct ParquetInfo {
    pub rows: u64,
    pub columns: Vec<ParquetColumn>,
    pub skipped: Vec<SkippedColumn>,
}

/// A column of a Parquet file that can be taken.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct ParquetColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: TableColumnType,
    /// Bytes it takes in the file.
    pub bytes: u64,
}
