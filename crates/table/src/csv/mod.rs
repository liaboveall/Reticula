//! Importing delimited text.
//!
//! A file is read once from its start to its end while the table is written.
//! Its first mebibyte tells how the fields are delimited, whether the first
//! row holds the names of the columns, and what the columns seem to hold; see
//! [`sniff`]. What they really hold is decided by every row. A value that its
//! column cannot hold turns the rest of the pass into one that only looks at
//! the types, after which the file is read a second time, from its start,
//! with the right ones. So do rows that are longer than they seemed, of which
//! fewer have to go into a group. [`Import`] does all this in short steps.
//!
//! The text is UTF-8. Rows end with a line feed, a carriage return or both. A
//! field that begins with a quote goes on to the next quote that is not
//! doubled; whatever follows that up to the delimiter belongs to it as well.
//! Other quotes mean nothing. Rows with too few fields are filled up with
//! blank cells, fields beyond the last column are dropped, and lines with
//! nothing on them are skipped, unless there is only one column. Nothing is
//! trimmed: spaces are part of a text, and make a number one.

mod column;
mod import;
mod infer;
mod sniff;
mod split;
mod value;

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::string_slice,
    clippy::unwrap_used,
    reason = "tests of small files"
)]
mod tests;

use alloc::vec::Vec;

pub use import::{ColumnReport, Import, Progress, Report, Step};
pub use sniff::{Sniffed, SniffedColumn, sniff};

use crate::types::ColumnType;
use crate::writer::WriteError;

/// How much of the start of a file is looked at to find out what it holds.
const SAMPLE_BYTES: usize = 1 << 20;
/// How much of the start of a file has to be given for that. The last byte
/// only says that the file goes on.
pub const HEAD_BYTES: usize = SAMPLE_BYTES + 1;
/// Most bytes that can wait to be parsed, see [`Import::feed`].
pub const MAX_WAITING: usize = 8 << 20;

/// What is known about a file beforehand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// What separates the fields: an ASCII character other than a quote, a
    /// line feed, a carriage return or NUL. `None` to find out.
    pub delimiter: Option<u8>,
    /// Whether the first row holds the names of the columns. `None` to find out.
    pub header: Option<bool>,
    /// Whether NA, N/A, NULL and NaN are blank cells, also all in lower or
    /// all in upper case.
    pub blank_markers: bool,
    /// What these columns are to hold. Values that they cannot hold then
    /// become blank cells. Whether all decimals are shown is decided by the
    /// values nevertheless.
    pub kinds: Vec<(usize, ColumnType)>,
}

/// Why a file cannot be imported. Rows are counted from 1, with the header
/// and with the lines that have nothing on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The quote at the start of a field in that row is never closed.
    UnclosedQuote {
        /// The row the field starts in.
        row: u64,
    },
    /// A field in quotes is longer than a text can be.
    FieldTooLong {
        /// The row the field starts in.
        row: u64,
    },
    /// The text is not UTF-8.
    NotUtf8 {
        /// The row it stops being so in.
        row: u64,
    },
    /// The first row does not end in the first mebibyte.
    FirstRowTooLong,
    /// The first rows have more fields than a table can have columns.
    TooManyColumns,
    /// There are no rows.
    Empty,
    /// The file that was read the second time is not the one that was read first.
    Changed,
    /// More was given than can wait to be parsed, or something was given
    /// after the end.
    Refused,
    /// The table cannot be written.
    Write(WriteError),
}
