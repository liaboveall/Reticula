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

/// Parses a UTF-8 CSV/TSV byte buffer into a fresh [`Sheet`] with sheet name
/// `name` (stripping an optional UTF-8 BOM `\u{FEFF}`).
pub fn import_csv_sheet(
    id: SheetId,
    name: String,
    raw_bytes: &[u8],
    delimiter_override: Option<u8>,
) -> Result<Sheet, CsvError> {
    let body = raw_bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(raw_bytes);
    let text = core::str::from_utf8(body).map_err(|_| CsvError::InvalidUtf8)?;
    let delimiter = delimiter_override.unwrap_or_else(|| detect_delimiter(text));

    // Pass 1: count rows & max columns while validating quote balance, or parse
    // streaming row-by-row onto a standard sheet!
    let mut sheet = Sheet::standard(id, name);
    let mut row_idx: u32 = 0;
    let mut col_idx: u32 = 0;
    let mut max_cols_seen: u32 = 0;

    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    let mut field_scratch = String::new();

    while pos < len {
        // Parse one field starting at `pos`.
        field_scratch.clear();
        let field_slice: &str = if bytes.get(pos) == Some(&b'"') {
            pos = pos.saturating_add(1);
            let mut closed = false;
            while pos < len {
                // Scan until next `"` so multi-byte UTF-8 spans copy cleanly.
                let chunk_start = pos;
                while let Some(&b) = bytes.get(pos) {
                    if b == b'"' {
                        break;
                    }
                    pos = pos.saturating_add(1);
                }
                if let Some(piece) = text.get(chunk_start..pos) {
                    field_scratch.push_str(piece);
                }
                if bytes.get(pos) == Some(&b'"') {
                    if bytes.get(pos.saturating_add(1)) == Some(&b'"') {
                        field_scratch.push('"');
                        pos = pos.saturating_add(2);
                    } else {
                        pos = pos.saturating_add(1);
                        closed = true;
                        break;
                    }
                }
            }
            if !closed {
                return Err(CsvError::UnclosedQuote);
            }
            // Skip optional trailing spaces before delimiter/newline if any.
            while let Some(&b) = bytes.get(pos) {
                if b == delimiter || b == b'\r' || b == b'\n' {
                    break;
                }
                pos = pos.saturating_add(1);
            }
            field_scratch.as_str()
        } else {
            let start = pos;
            while let Some(&b) = bytes.get(pos) {
                if b == delimiter || b == b'\r' || b == b'\n' {
                    break;
                }
                pos = pos.saturating_add(1);
            }
            text.get(start..pos).unwrap_or("")
        };

        if row_idx >= MAX_ROW_AXIS_LEN || col_idx >= MAX_COL_AXIS_LEN {
            return Err(CsvError::DimensionsExceeded);
        }
        // Grow row axis beyond DEFAULT_BASE_ROWS (1,048,576) up to MAX_ROW_AXIS_LEN if needed.
        if row_idx >= sheet.rows() {
            let extra = sheet
                .extend_base_rows_to(row_idx.saturating_add(1))
                .ok_or(CsvError::DimensionsExceeded)?;
            let _ = extra;
        }

        if !field_slice.is_empty() {
            write_inferred_csv_cell(
                &mut sheet,
                RowId(u64::from(row_idx)),
                ColId(u64::from(col_idx)),
                field_slice,
            )?;
        }
        max_cols_seen = max_cols_seen.max(col_idx.saturating_add(1));

        // Check terminator after field.
        match bytes.get(pos).copied() {
            Some(b) if b == delimiter => {
                pos = pos.saturating_add(1);
                col_idx = col_idx.saturating_add(1);
                // If delimiter sits at very end of line or EOF, record the trailing empty cell.
                if pos == len || matches!(bytes.get(pos), Some(b'\r' | b'\n')) {
                    max_cols_seen = max_cols_seen.max(col_idx.saturating_add(1));
                }
            }
            Some(b'\r') => {
                pos = pos.saturating_add(1);
                if bytes.get(pos) == Some(&b'\n') {
                    pos = pos.saturating_add(1);
                }
                row_idx = row_idx.saturating_add(1);
                col_idx = 0;
                if row_idx.is_multiple_of(4096) && sheet.estimated_bytes() > MAX_WORKBOOK_BYTES {
                    return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
                }
            }
            Some(b'\n') => {
                pos = pos.saturating_add(1);
                row_idx = row_idx.saturating_add(1);
                col_idx = 0;
                if row_idx.is_multiple_of(4096) && sheet.estimated_bytes() > MAX_WORKBOOK_BYTES {
                    return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
                }
            }
            Some(_) => {
                pos = pos.saturating_add(1);
            }
            None => {}
        }
    }

    let _ = max_cols_seen;
    if sheet.estimated_bytes() > MAX_WORKBOOK_BYTES {
        return Err(CsvError::WorkbookLimit(OpError::MemoryLimitExceeded));
    }
    Ok(sheet)
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
