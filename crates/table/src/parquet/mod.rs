//! Importing Parquet files.
//!
//! [`open`] reads the metadata at the end of a file and says which columns can
//! be taken. [`Import`] reads them and writes a table, a piece of work at a
//! time. Only flat columns are taken, values that are lists, maps or
//! structures are left out and named.
//!
//! What a value becomes:
//!
//! | in the file | in the table |
//! |---|---|
//! | whole numbers, signed or not | numbers; unsigned ones above the largest signed one make the whole column text |
//! | decimals of at most 18 digits | decimals; longer ones text with all digits |
//! | floating-point numbers of 16, 32 and 64 bits | floating-point numbers; those that are not finite are blank |
//! | dates, timestamps, 96-bit timestamps | dates, dates with times; those outside the years 1 to 9999 are blank, parts of a microsecond are cut off towards the past |
//! | times of day, UUIDs | text |
//! | text, enumerations, JSON, bytes that are text | text |
//!
//! A number of 32 bits is converted exactly, so that 0.1 is shown as
//! 0.10000000149011612: the table does not know that there were fewer bits.
//!
//! A file is hostile until proven otherwise. Sizes are checked before memory
//! is taken for them, and what a page unpacks to is never longer than it says.

mod codec;
mod crc32;
#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "files for the tests"
)]
mod encode;
mod import;
mod meta;
mod runs;
#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "tests"
)]
mod samples;
mod snappy;
#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "measurements"
)]
mod speed;
mod stream;
#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests"
)]
mod tests;
mod thrift;
mod value;

use alloc::string::String;
use alloc::vec::Vec;

use crate::source::{ByteRange, Source, SourceError};
use crate::types::ColumnType;
use crate::writer::WriteError;

pub use import::{ColumnReport, Import, Options, Progress, Report, Step};
pub use value::SkipReason;

use meta::{Chunk, Fault};
use value::{Physical, Plan};

/// The first and the last bytes of a file.
const MAGIC: [u8; 4] = *b"PAR1";
/// The last bytes of a file whose metadata is encrypted.
const ENCRYPTED_MAGIC: [u8; 4] = *b"PARE";
/// Bytes after the metadata: its length, and [`MAGIC`].
const TRAILER: u64 = 8;
/// How much of the end of a file is read at first.
const TAIL: u64 = 64 << 10;
/// Longest metadata.
const MAX_METADATA: u64 = 64 << 20;

/// Why a file cannot be imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParquetError {
    /// It is not a Parquet file.
    NotParquet,
    /// It is encrypted.
    Encrypted,
    /// No column can be taken.
    NoColumns,
    /// Something is larger than is allowed.
    Limit(Limit),
    /// What is in the file contradicts itself.
    Damaged {
        /// The column, as [`ParquetFile::columns`] counts, if it is known.
        column: Option<u32>,
        /// What is damaged.
        damage: Damage,
    },
    /// There is not enough memory.
    Memory,
    /// The file could not be read.
    Source,
    /// The table could not be written.
    Write(WriteError),
}

/// What is too large.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// Too many rows.
    Rows,
    /// Too many columns were asked for.
    Columns,
    /// The metadata is too long.
    Metadata,
    /// A page is too long.
    Page,
    /// The texts of a few rows are too long.
    Text,
}

/// What is damaged in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
    /// The metadata.
    Metadata,
    /// Where something is said to lie.
    Place,
    /// The numbers of rows do not agree.
    Count,
    /// The start of a page.
    Page,
    /// The checksum of a page is another.
    Checksum,
    /// A page cannot be unpacked.
    Packed,
    /// What says which rows of a page are blank.
    Levels,
    /// A dictionary.
    Dictionary,
    /// The encoding of a page, which is not one for its column.
    Encoding,
    /// The values of a page.
    Values,
}

/// A column that can be taken.
#[derive(Clone, Debug)]
pub(crate) struct Column {
    name: String,
    kind: ColumnType,
    plan: Plan,
    physical: Physical,
    /// Whether there may be blanks.
    optional: bool,
    chunks: Vec<Chunk>,
    unreadable: Option<SkipReason>,
}

/// A column that can be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnInfo<'a> {
    /// Its name.
    pub name: &'a str,
    /// What it becomes, unless its values do not allow it.
    pub kind: ColumnType,
    /// Bytes it takes in the file.
    pub bytes: u64,
}

/// A column that is not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedColumn {
    /// Its name.
    pub name: String,
    /// Why.
    pub reason: SkipReason,
}

/// A file whose metadata was read.
#[derive(Clone, Debug)]
pub struct ParquetFile {
    rows: u64,
    columns: Vec<Column>,
    skipped: Vec<SkippedColumn>,
    /// The pages end here.
    data_end: u64,
}

impl ParquetFile {
    /// Number of rows.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// The columns that can be taken.
    pub fn columns(&self) -> impl ExactSizeIterator<Item = ColumnInfo<'_>> {
        self.columns.iter().map(|column| ColumnInfo {
            name: &column.name,
            kind: column.kind,
            bytes: column.bytes(),
        })
    }

    /// The columns that are left out.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedColumn] {
        &self.skipped
    }
}

impl Column {
    fn bytes(&self) -> u64 {
        self.chunks
            .iter()
            .fold(0, |bytes, chunk| bytes.saturating_add(chunk.len))
    }
}

/// What came of opening a file.
#[derive(Debug)]
pub enum Opened {
    /// These bytes are needed first.
    Need(ByteRange),
    /// The file is open.
    Ready(ParquetFile),
}

/// Reads the metadata of a file of that length.
///
/// # Errors
///
/// [`ParquetError`] when it is not a file that can be imported.
pub fn open<S: Source>(source: &mut S, file_len: u64) -> Result<Opened, ParquetError> {
    let data_end = file_len
        .checked_sub(TRAILER)
        .ok_or(ParquetError::NotParquet)?;
    let mut tail = ByteRange {
        at: file_len.saturating_sub(TAIL),
        len: file_len.min(TAIL),
    };
    let mut bytes = Vec::new();
    loop {
        let len = usize::try_from(tail.len).map_err(|_| ParquetError::Memory)?;
        bytes.clear();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| ParquetError::Memory)?;
        bytes.resize(len, 0);
        match source.read(tail.at, &mut bytes) {
            Ok(()) => {}
            Err(SourceError::Missing) => return Ok(Opened::Need(tail)),
            Err(SourceError::Failed) => return Err(ParquetError::Source),
        }
        let (before, trailer) = bytes
            .split_last_chunk::<8>()
            .ok_or(ParquetError::NotParquet)?;
        let [l0, l1, l2, l3, magic @ ..] = *trailer;
        if magic == ENCRYPTED_MAGIC {
            return Err(ParquetError::Encrypted);
        }
        if magic != MAGIC {
            return Err(ParquetError::NotParquet);
        }
        let len = u64::from(u32::from_le_bytes([l0, l1, l2, l3]));
        if len > MAX_METADATA {
            return Err(ParquetError::Limit(Limit::Metadata));
        }
        let start = data_end
            .checked_sub(len)
            .ok_or(Fault::Damaged(Damage::Place))?;
        if start >= tail.at {
            let offset = usize::try_from(start.saturating_sub(tail.at)).unwrap_or(usize::MAX);
            let metadata = before.get(offset..).unwrap_or_default();
            return Ok(Opened::Ready(meta::read_file(metadata, start)?));
        }
        tail = ByteRange {
            at: start,
            len: file_len.saturating_sub(start),
        };
    }
}
