//! A sheet: columns of cells, and what it takes to show them.
//!
//! This is the first cut: rows are appended and cells are read by position.
//! Rows and columns have no identity of their own yet, so the identifier of a
//! row is its position. That changes when the axis arrives; the identifiers
//! already travel with every viewport page so that nothing downstream has to.

use alloc::string::String;
use alloc::vec::Vec;

use crate::format::{NumberFormat, write_number};
use crate::store::Column;
use crate::strings::StringTable;
use crate::value::Value;

/// One sheet of a workbook.
#[derive(Clone, Debug, Default)]
pub struct Sheet {
    columns: Vec<Column>,
    formats: Vec<NumberFormat>,
    strings: StringTable,
    rows: u32,
}

impl Sheet {
    /// Creates an empty sheet with one column for each number format.
    #[must_use]
    pub fn new(formats: &[NumberFormat]) -> Self {
        Self {
            columns: formats.iter().map(|_| Column::new()).collect(),
            formats: formats.to_vec(),
            strings: StringTable::new(),
            rows: 0,
        }
    }

    /// Number of rows.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    /// Number of columns.
    #[must_use]
    pub fn cols(&self) -> u32 {
        u32::try_from(self.columns.len()).unwrap_or(u32::MAX)
    }

    /// The strings that text cells refer to.
    #[must_use]
    pub const fn strings(&self) -> &StringTable {
        &self.strings
    }

    /// Stores a string and returns the index a text cell refers to it by, or
    /// `None` when the string table is full.
    pub fn add_string(&mut self, text: &str) -> Option<u32> {
        self.strings.push(text)
    }

    /// Appends a row. Columns beyond the end of `values` get an empty cell.
    /// Returns `false`, leaving the sheet unchanged, once it holds as many
    /// rows as can be numbered.
    pub fn push_row(&mut self, values: &[Value]) -> bool {
        let Some(rows) = self.rows.checked_add(1) else {
            return false;
        };
        for (index, column) in self.columns.iter_mut().enumerate() {
            // Every column has `self.rows` rows, so this cannot fail part-way.
            column.push(values.get(index).copied().unwrap_or(Value::Empty));
        }
        self.rows = rows;
        true
    }

    /// The value of a cell; empty outside the sheet.
    #[must_use]
    pub fn value(&self, row: u32, col: u32) -> Value {
        self.column(col)
            .map_or(Value::Empty, |column| column.get(row))
    }

    /// A column by position.
    #[must_use]
    pub fn column(&self, col: u32) -> Option<&Column> {
        self.columns.get(col as usize)
    }

    /// Appends the text a cell shows to `out` and returns the cell's value.
    /// An empty cell appends nothing.
    pub fn write_text(&self, row: u32, col: u32, out: &mut String) -> Value {
        let value = self.value(row, col);
        match value {
            Value::Empty => {}
            Value::Number(number) => {
                let format = self.formats.get(col as usize).copied().unwrap_or_default();
                write_number(number, format, out);
            }
            Value::Text(id) => out.push_str(self.strings.get(id)),
        }
        value
    }

    /// The identifier of the row at a position.
    #[must_use]
    pub const fn row_id(&self, row: u32) -> u64 {
        row as u64
    }

    /// The identifier of the column at a position.
    #[must_use]
    pub const fn col_id(&self, col: u32) -> u64 {
        col as u64
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use super::Sheet;
    use crate::format::NumberFormat;
    use crate::value::Value;

    fn text(sheet: &Sheet, row: u32, col: u32) -> String {
        let mut out = String::new();
        sheet.write_text(row, col, &mut out);
        out
    }

    #[test]
    fn cells_are_shown_in_their_column_format() {
        let mut sheet = Sheet::new(&[
            NumberFormat::General,
            NumberFormat::Fixed2,
            NumberFormat::Percent,
        ]);
        let label = sheet.add_string("合计").map_or(Value::Empty, Value::Text);
        assert!(sheet.push_row(&[label, Value::Number(12.5), Value::Number(0.13)]));
        assert!(sheet.push_row(&[Value::Number(7.0)]));
        assert_eq!((sheet.rows(), sheet.cols()), (2, 3));

        assert_eq!(text(&sheet, 0, 0), "合计");
        assert_eq!(text(&sheet, 0, 1), "12.50");
        assert_eq!(text(&sheet, 0, 2), "13%");
        assert_eq!(text(&sheet, 1, 0), "7");
        // A short row leaves the remaining cells empty.
        assert_eq!(sheet.value(1, 1), Value::Empty);
        assert_eq!(text(&sheet, 1, 2), "");
    }

    #[test]
    fn cells_outside_the_sheet_are_empty() {
        let mut sheet = Sheet::new(&[NumberFormat::General]);
        assert!(sheet.push_row(&[Value::Number(1.0)]));
        assert_eq!(sheet.value(1, 0), Value::Empty);
        assert_eq!(sheet.value(0, 1), Value::Empty);
        assert_eq!(sheet.value(u32::MAX, u32::MAX), Value::Empty);
        assert_eq!(text(&sheet, 5, 5), "");
    }
}
