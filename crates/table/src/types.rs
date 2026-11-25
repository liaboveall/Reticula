//! What a column holds.

/// Most rows of a table.
pub const MAX_ROWS: u64 = 100_000_000;
/// Most columns of a table.
pub const MAX_COLUMNS: usize = 1_024;
/// Longest text of a cell, in bytes. Importers cut what is longer and say so.
pub const MAX_TEXT_BYTES: usize = 1 << 20;
/// Longest name of a column, in bytes.
pub const MAX_NAME_BYTES: usize = 1_024;
/// Most decimals of a [`ColumnType::Decimal`].
pub const MAX_SCALE: u8 = 18;

/// The type of a column.
///
/// Every type but [`ColumnType::Float`] and [`ColumnType::Text`] is stored
/// as an integer, named in its description; comparing the integers compares
/// the values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColumnType {
    /// Whole numbers: the number.
    Integer,
    /// Numbers with a fixed number of decimals: the number times ten to the
    /// power of `scale`.
    Decimal {
        /// Number of decimals, at most [`MAX_SCALE`].
        scale: u8,
        /// Whether every value was written with exactly that many decimals,
        /// and so is shown with them.
        fixed: bool,
    },
    /// Binary floating-point numbers, all finite.
    Float,
    /// Dates: days since 1970-01-01.
    Date,
    /// Dates with a time of day: microseconds since 1970-01-01 00:00.
    DateTime,
    /// Truth values: 0 or 1.
    Boolean,
    /// Text.
    Text,
}

impl ColumnType {
    /// Whether the values are stored as integers.
    #[must_use]
    pub const fn is_integral(self) -> bool {
        !matches!(self, Self::Float | Self::Text)
    }

    /// Whether the values are numbers, which can be added up.
    #[must_use]
    pub const fn is_number(self) -> bool {
        matches!(self, Self::Integer | Self::Decimal { .. } | Self::Float)
    }
}

/// One cell. The text of a text cell is wherever the reader was told to put it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellValue {
    /// Nothing.
    Blank,
    /// The integer that stands for the value, see [`ColumnType`].
    Int(i64),
    /// A floating-point number.
    Float(f64),
    /// Text.
    Text,
}
