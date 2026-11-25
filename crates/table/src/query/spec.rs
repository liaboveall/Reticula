//! What can be asked of a table.

use alloc::string::String;
use alloc::vec::Vec;

/// A value of a column, as it is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Nothing.
    Blank,
    /// The integer that stands for a value, see [`ColumnType`](crate::types::ColumnType).
    Int(i64),
    /// The bits of a floating-point number.
    Float(u64),
    /// A text, in whatever case. The empty text is a blank.
    Text(String),
}

/// How numbers are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberOp {
    /// The number is the threshold.
    Equal,
    /// The number is not the threshold.
    NotEqual,
    /// It is larger.
    Greater,
    /// It is not smaller.
    AtLeast,
    /// It is smaller.
    Less,
    /// It is not larger.
    AtMost,
    /// It is one of two thresholds or lies between them, whichever is larger.
    Between,
}

/// How dates and times are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeOp {
    /// It is earlier.
    Before,
    /// It is later.
    After,
    /// It is one of two, or lies between them, whichever is later.
    Between,
    /// It lies on the same day.
    On,
}

/// What a cell has to be for its row to be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// One of these values. For columns of any type; the values have to be of
    /// that type, or blank.
    OneOf {
        /// The values.
        values: Vec<Value>,
    },
    /// A text that contains this one. For text columns.
    TextContains(String),
    /// A text that begins with this one. For text columns.
    TextStartsWith(String),
    /// A text that ends with this one. For text columns.
    TextEndsWith(String),
    /// A number that compares in some way. For columns of numbers.
    Number {
        /// How.
        op: NumberOp,
        /// The threshold, a decimal number.
        a: String,
        /// The other threshold of [`NumberOp::Between`].
        b: Option<String>,
    },
    /// A date or time that compares in some way. For columns of such.
    Time {
        /// How.
        op: TimeOp,
        /// Days of a column of dates, microseconds of a column of times.
        a: i64,
        /// The other one of [`TimeOp::Between`].
        b: Option<i64>,
    },
    /// One of the largest or smallest values of the column, or equal to one of
    /// them. For columns of numbers, dates and times.
    Top {
        /// How many of the values there are.
        n: u32,
        /// Whether they are the smallest.
        bottom: bool,
    },
    /// Nothing. For columns of any type.
    Blank,
    /// Something. For columns of any type.
    NotBlank,
}

/// A column to order by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    /// The column.
    pub column: usize,
    /// Whether the largest value comes first. Blank cells come last anyway.
    pub descending: bool,
}

/// Which rows to show, and in which order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    /// Columns and what their cells have to be. All of it has to hold.
    pub filters: Vec<(usize, Condition)>,
    /// What to order by, the most important column first. Rows that are equal
    /// stay in the order of the file.
    pub order: Vec<SortKey>,
}

/// Why something cannot be asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecError {
    /// There is no such column.
    NoColumn {
        /// Its number.
        column: usize,
    },
    /// The condition is not for columns of this type.
    DoesNotApply {
        /// The column.
        column: usize,
    },
    /// The threshold is not a decimal number.
    NotANumber {
        /// The column.
        column: usize,
    },
    /// A comparison with two thresholds has only one.
    OneThreshold {
        /// The column.
        column: usize,
    },
}
