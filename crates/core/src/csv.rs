//! Pure `no_std` RFC 4180 CSV / TSV reader & writer (`14-m1b.md` 2.6 & 5.6).
//!
//! CSV import constructs a fresh [`Sheet`] directly using [`infer_csv_cell`]
//! (scalar-only E30 classification: empty, boolean, strict ISO date, percent,
//! finite decimal, or literal text — strings beginning with `'='` or `'\''` are
//! stored verbatim as text rather than parsed as formulas).

use alloc::string::String;

use crate::axis::{MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN};
use crate::id::{ColId, RowId, SheetId};
use crate::input::infer_input_scalar_only;
use crate::ops::{MAX_WORKBOOK_BYTES, OpError, ScalarValue};
use crate::sheet::Sheet;
use crate::value::Value;

/// Errors returned when importing a CSV/TSV byte buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvError {
    /// Input bytes were not valid UTF-8.
    InvalidUtf8,
    /// Unclosed quoted field at EOF.
    UnclosedQuote,
    /// Row or column count exceeded [`MAX_ROW_AXIS_LEN`] or [`MAX_COL_AXIS_LEN`].
    DimensionsExceeded,
    /// A cell exceeded [`crate::ops::MAX_CELL_TEXT_UTF16`] or workbook memory
    /// exceeded [`MAX_WORKBOOK_BYTES`].
    WorkbookLimit(OpError),
}

impl From<OpError> for CsvError {
    fn from(err: OpError) -> Self {
        Self::WorkbookLimit(err)
    }
}

/// Auto-detects delimiter (`'\t'` if the first non-empty unquoted line has
/// more tabs than commas, otherwise `','`).
#[must_use]
pub fn detect_delimiter(text: &str) -> u8 {
    let first_line = text.lines().next().unwrap_or("");
    let mut commas = 0usize;
    let mut tabs = 0usize;
    let mut in_quotes = false;
    for b in first_line.bytes() {
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            if b == b',' {
                commas = commas.saturating_add(1);
            } else if b == b'\t' {
                tabs = tabs.saturating_add(1);
            }
        }
    }
    if tabs > commas { b'\t' } else { b',' }
}

/// Incremental RFC 4180 CSV/TSV parser supporting row-batched progress updates
/// (`14-m1b.md` 5.6: report progress every `65,536` rows).
#[derive(Debug)]
pub struct CsvReader {
    raw: alloc::vec::Vec<u8>,
    text_offset: usize,
    delimiter: u8,
    sheet: Sheet,
    pos: usize,
    row_idx: u32,
    col_idx: u32,
    field_scratch: String,
}

impl CsvReader {
    /// Validates UTF-8 (stripping an optional UTF-8 BOM `\u{FEFF}`) and prepares
    /// incremental row parsing into a fresh [`Sheet`].
    pub fn new(
        id: SheetId,
        name: String,
        raw: alloc::vec::Vec<u8>,
        delimiter_override: Option<u8>,
    ) -> Result<Self, CsvError> {
        let text_offset = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else {
            0
        };
        let body = raw.get(text_offset..).unwrap_or(&[]);
        let text = core::str::from_utf8(body).map_err(|_| CsvError::InvalidUtf8)?;
        let delimiter = delimiter_override.unwrap_or_else(|| detect_delimiter(text));
        Ok(Self {
            raw,
            text_offset,
            delimiter,
            sheet: Sheet::standard(id, name),
            pos: 0,
            row_idx: 0,
            col_idx: 0,
            field_scratch: String::new(),
        })
    }

    /// Rows completed so far.
    #[must_use]
    pub const fn rows_parsed(&self) -> u32 {
        self.row_idx
    }

    /// Byte progress percentage (`0..=100`).
    #[must_use]
    pub fn progress_percent(&self) -> u8 {
        let body_len = self.raw.len().saturating_sub(self.text_offset);
        if body_len == 0 || self.pos >= body_len {
            return 100;
        }
        let pct = self
            .pos
            .saturating_mul(100)
            .checked_div(body_len)
            .unwrap_or(100);
        u8::try_from(pct.min(99)).unwrap_or(99)
    }

    /// Parses up to `max_rows` complete rows (`>= 1`), returning `Ok(true)` once
    /// the entire input has been consumed or `Ok(false)` when yielding between
    /// batches.
    pub fn parse_rows(&mut self, max_rows: u32) -> Result<bool, CsvError> {
        let body = self.raw.get(self.text_offset..).unwrap_or(&[]);
        let text = core::str::from_utf8(body).map_err(|_| CsvError::InvalidUtf8)?;
        let bytes = text.as_bytes();
        let len = bytes.len();
        let delimiter = self.delimiter;
        let stop_row = self.row_idx.saturating_add(max_rows.max(1));

        while self.pos < len && self.row_idx < stop_row {
            self.field_scratch.clear();
            let field_slice: &str = if bytes.get(self.pos) == Some(&b'"') {
                self.pos = self.pos.saturating_add(1);
                let mut closed = false;
                while self.pos < len {
                    let chunk_start = self.pos;
                    while let Some(&b) = bytes.get(self.pos) {
                        if b == b'"' {
                            break;
                        }
                        self.pos = self.pos.saturating_add(1);
                    }
                    if let Some(piece) = text.get(chunk_start..self.pos) {
                        self.field_scratch.push_str(piece);
                    }
                    if bytes.get(self.pos) == Some(&b'"') {
                        if bytes.get(self.pos.saturating_add(1)) == Some(&b'"') {
                            self.field_scratch.push('"');
                            self.pos = self.pos.saturating_add(2);
                        } else {
                            self.pos = self.pos.saturating_add(1);
                            closed = true;
                            break;
                        }
                    }
                }
                if !closed {
                    return Err(CsvError::UnclosedQuote);
                }
                while let Some(&b) = bytes.get(self.pos) {
                    if b == delimiter || b == b'\r' || b == b'\n' {
                        break;
                    }
                    self.pos = self.pos.saturating_add(1);
                }
                self.field_scratch.as_str()
            } else {
                let start = self.pos;
                while let Some(&b) = bytes.get(self.pos) {
                    if b == delimiter || b == b'\r' || b == b'\n' {
                        break;
                    }
                    self.pos = self.pos.saturating_add(1);
                }
                text.get(start..self.pos).unwrap_or("")
            };

            if self.row_idx >= MAX_ROW_AXIS_LEN || self.col_idx >= MAX_COL_AXIS_LEN {
                return Err(CsvError::DimensionsExceeded);
            }
            if self.row_idx >= self.sheet.rows() {
                let _ = self
                    .sheet
                    .extend_base_rows_to(self.row_idx.saturating_add(1))
                    .ok_or(CsvError::DimensionsExceeded)?;
            }

            if !field_slice.is_empty() {
                write_inferred_csv_cell(
                    &mut self.sheet,
                    RowId(u64::from(self.row_idx)),
                    ColId(u64::from(self.col_idx)),
                    field_slice,
                )?;
            }

            match bytes.get(self.pos).copied() {
                Some(b) if b == delimiter => {
                    self.pos = self.pos.saturating_add(1);
                    self.col_idx = self.col_idx.saturating_add(1);
                }
                Some(b'\r') => {
                    self.pos = self.pos.saturating_add(1);
                    if bytes.get(self.pos) == Some(&b'\n') {
                        self.pos = self.pos.saturating_add(1);
                    }
                    self.row_idx = self.row_idx.saturating_add(1);
                    self.col_idx = 0;
                    if self.row_idx.is_multiple_of(4096)
                        && self.sheet.estimated_bytes() > MAX_WORKBOOK_BYTES
                    {
                        return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
                    }
                }
                Some(b'\n') => {
                    self.pos = self.pos.saturating_add(1);
                    self.row_idx = self.row_idx.saturating_add(1);
                    self.col_idx = 0;
                    if self.row_idx.is_multiple_of(4096)
                        && self.sheet.estimated_bytes() > MAX_WORKBOOK_BYTES
                    {
                        return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
                    }
                }
                Some(_) => {
                    self.pos = self.pos.saturating_add(1);
                }
                None => {
                    if self.col_idx > 0 || !field_slice.is_empty() {
                        self.row_idx = self.row_idx.saturating_add(1);
                        self.col_idx = 0;
                    }
                }
            }
        }

        if self.pos >= len {
            if self.sheet.estimated_bytes() > MAX_WORKBOOK_BYTES {
                return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Consumes the reader and returns the populated [`Sheet`].
    pub fn finish(self) -> Result<Sheet, CsvError> {
        if self.sheet.estimated_bytes() > MAX_WORKBOOK_BYTES {
            return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
        }
        Ok(self.sheet)
    }
}

/// Parses a UTF-8 CSV/TSV byte buffer into a fresh [`Sheet`] with sheet name
/// `name` (stripping an optional UTF-8 BOM `\u{FEFF}`).
pub fn import_csv_sheet(
    id: SheetId,
    name: String,
    raw_bytes: &[u8],
    delimiter_override: Option<u8>,
) -> Result<Sheet, CsvError> {
    let mut reader = CsvReader::new(id, name, raw_bytes.to_vec(), delimiter_override)?;
    while !reader.parse_rows(65_536)? {}
    reader.finish()
}

fn write_inferred_csv_cell(
    sheet: &mut Sheet,
    row: RowId,
    col: ColId,
    raw: &str,
) -> Result<(), CsvError> {
    let Some((scalar, suggested_fmt)) = infer_input_scalar_only(raw)? else {
        return Ok(());
    };
    let val = match scalar {
        ScalarValue::Number(n) => Value::number(n),
        ScalarValue::Bool(b) => Value::Bool(b),
        ScalarValue::Error(e) => Value::Error(e),
        ScalarValue::Text(s) => {
            let sid = sheet
                .add_string(&s)
                .ok_or(CsvError::WorkbookLimit(OpError::StringPoolExhausted))?;
            Value::Text(sid)
        }
    };
    if !sheet.set_value_by_id(row, col, val) {
        return Err(CsvError::DimensionsExceeded);
    }
    if let Some(fmt) = suggested_fmt {
        sheet.set_cell_format_by_id(row, col, Some(fmt));
    }
    Ok(())
}

/// Exports the non-empty bounding box `(0..=max_row, 0..=max_col)` of `sheet` as
/// RFC 4180 UTF-8 CSV (`delimiter` is typically `b','` or `b'\t'`).
#[must_use]
pub fn export_sheet_delimited(sheet: &Sheet, delimiter: u8) -> String {
    let Some((max_row, max_col)) = sheet.used_extent() else {
        return String::new();
    };
    let delim_ch = char::from(delimiter);
    let mut out = String::new();
    let mut cell_buf = String::new();

    for r in 0..=max_row {
        for c in 0..=max_col {
            if c > 0 {
                out.push(delim_ch);
            }
            cell_buf.clear();
            sheet.write_text(r, c, &mut cell_buf);
            write_escaped_field(&cell_buf, delimiter, &mut out);
        }
        out.push_str("\r\n");
    }
    out
}

/// Escapes a single CSV/TSV field per RFC 4180.
pub fn write_escaped_field(field: &str, delimiter: u8, out: &mut String) {
    let needs_quotes = field
        .bytes()
        .any(|b| b == delimiter || b == b'"' || b == b'\r' || b == b'\n');
    if !needs_quotes {
        out.push_str(field);
        return;
    }
    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use super::{export_sheet_delimited, import_csv_sheet};
    use crate::format::NumberFormat;
    use crate::id::{ColId, RowId, SheetId};
    use crate::value::Value;

    #[test]
    fn csv_round_trips_quotes_newlines_dates_and_keeps_equals_as_text() {
        let raw = "\u{FEFF}Item,Date,Rate,Note\r\n\
                   \"Widget \"\"A\"\"\",2026-11-19,12.5%,\"Line 1\nLine 2\"\r\n\
                   =SUM(A1:A5),'00123,true,42.5\r\n";
        let sheet = import_csv_sheet(
            SheetId(0),
            String::from("Imported"),
            raw.as_bytes(),
            Some(b','),
        )
        .expect("import");

        // `=SUM(A1:A5)` and `'00123` stay literal text in CSV import!
        let mut buf = String::new();
        sheet.write_text(2, 0, &mut buf);
        assert_eq!(buf, "=SUM(A1:A5)");
        buf.clear();
        sheet.write_text(2, 1, &mut buf);
        assert_eq!(buf, "'00123");
        assert_eq!(sheet.value(2, 2), Value::Bool(true));
        assert_eq!(sheet.value(2, 3), Value::Number(42.5));
        assert_eq!(
            sheet.effective_format_by_id(RowId(1), ColId(1)),
            NumberFormat::Date
        );

        let exported = export_sheet_delimited(&sheet, b',');
        assert!(exported.contains("\"Widget \"\"A\"\"\",2026-11-19,13%,\"Line 1\nLine 2\"\r\n"));
        assert!(exported.contains("=SUM(A1:A5),'00123,TRUE,42.5\r\n"));
    }
}
