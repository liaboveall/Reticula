//! Pages of cells for the screen, in the same format as a sheet's.

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::viewport::{
    FLAG_BOOL, FLAG_NUMBER, MAX_CELL_DISPLAY_UTF16, PAGE_COLS, PAGE_ROWS, PageFrame, PageWriter,
};

use crate::reader::{ReadError, Reader};
use crate::source::Source;
use crate::text;
use crate::types::ColumnType;

/// A cell shows no more than this many bytes of its text: every character
/// takes at most three bytes for each of its UTF-16 code units, and one
/// character more than is shown, which has at most four, makes the cell count
/// as cut.
const SHOWN_BYTES: usize = (MAX_CELL_DISPLAY_UTF16 as usize) * 3 + 4;

/// Encodes the cells of some rows, at most [`PAGE_ROWS`], in the columns from
/// `frame.first_col` on, at most [`PAGE_COLS`].
///
/// `rows` are rows of the file, in the order they are shown. `scratch` is
/// space to write texts in.
///
/// # Errors
///
/// [`ReadError`] when not all of the cells can be read. What is missing is
/// noted for all of them, not only for the first one. Where a text lies is
/// known only once its code is here, so that there can be more than one round.
pub fn encode<S: Source>(
    reader: &mut Reader,
    source: &mut S,
    frame: PageFrame,
    rows: &[u32],
    scratch: &mut String,
) -> Result<Vec<u8>, ReadError> {
    let rows = rows
        .get(..rows.len().min(PAGE_ROWS as usize))
        .unwrap_or(&[]);
    let first = frame.first_col as usize;
    let kinds: Vec<ColumnType> = reader
        .columns()
        .iter()
        .skip(first)
        .take(PAGE_COLS as usize)
        .map(|column| column.kind)
        .collect();
    let rows = if kinds.is_empty() { &[] } else { rows };
    let cols = if rows.is_empty() { 0 } else { kinds.len() };
    let end = first.saturating_add(cols);
    let ids: Vec<u64> = (first..end)
        .map(|column| u64::try_from(column).unwrap_or(0))
        .collect();
    let mut writer = PageWriter::new(
        frame,
        rows.iter().map(|row| u64::from(*row)),
        ids.into_iter(),
        scratch,
    );
    let mut missing = false;
    for row in rows {
        for (column, kind) in (first..end).zip(&kinds) {
            match reader.cell(source, column, *row, SHOWN_BYTES, writer.text()) {
                Ok(value) => text::write_value(*kind, value, writer.text()),
                Err(ReadError::Missing) => missing = true,
                Err(error) => return Err(error),
            }
            let blank = writer.is_text_empty();
            writer.cell(0, if blank { 0 } else { flags(*kind) });
        }
    }
    if missing {
        Err(ReadError::Missing)
    } else {
        Ok(writer.finish())
    }
}

const fn flags(kind: ColumnType) -> u16 {
    match kind {
        ColumnType::Boolean => FLAG_BOOL,
        ColumnType::Text => 0,
        // Dates and times are numbers where sheets are concerned, and stand
        // at the right like them.
        ColumnType::Integer
        | ColumnType::Decimal { .. }
        | ColumnType::Float
        | ColumnType::Date
        | ColumnType::DateTime => FLAG_NUMBER,
    }
}
