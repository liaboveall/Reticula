//! Viewport pages: what the grid needs to draw a block of cells, as bytes.
//!
//! The grid draws on another thread and never touches the sheet. It says which
//! range of cells it wants; the engine covers that range with fixed-size pages
//! and sends each page as one self-contained packet, whose buffer changes
//! hands without being copied. A page carries the text every cell shows, so
//! the grid formats nothing, and the identifiers of its rows and columns, so
//! the grid can name a cell by identity when it sends a command.
//!
//! # Layout
//!
//! Version 1 (`13-m1a.md` 6.1). All integers are little-endian. A page of `R`
//! rows and `C` columns is laid out as:
//!
//! | bytes | content |
//! |---|---|
//! | [`HEADER_LEN`] | the header, see the `*_AT` constants |
//! | `R` × [`ID_LEN`] | the identifier of each row |
//! | `C` × [`ID_LEN`] | the identifier of each column |
//! | `R` × `C` × [`CELL_LEN`] | one record for each cell, row by row |
//! | [`TEXT_LEN_AT`] | the text of all cells, end to end, as UTF-8 |
//!
//! A cell record locates the cell's text within the page's text. The position
//! and the length are counted in UTF-16 code units, not bytes: the receiver
//! decodes the whole text once into a string of its own and cuts each cell out
//! of that, and its strings are indexed in those units. Each cell's display
//! text is capped at [`MAX_CELL_DISPLAY_UTF16`] UTF-16 code units (truncated on
//! a character boundary with [`FLAG_TRUNCATED`] set).

use alloc::string::String;
use alloc::vec::Vec;
use smallvec::SmallVec;

use crate::id::{ColId, RowId};
use crate::sheet::Sheet;
use crate::value::Value;

/// Version of the page layout (`13-m1a.md` 6.1).
pub const VERSION: u32 = 1;

/// Rows in a page. Pages at the bottom edge of a sheet have fewer.
pub const PAGE_ROWS: u32 = 64;
/// Columns in a page. Pages at the right edge of a sheet have fewer.
pub const PAGE_COLS: u32 = 16;

/// Length of the header.
pub const HEADER_LEN: usize = 32;
/// Header, `u32`: [`VERSION`].
pub const VERSION_AT: usize = 0;
/// Header, `u32`: the sheet the page belongs to (`SheetId`).
pub const SHEET_AT: usize = 4;
/// Header, `u32`: position of the page's first row.
pub const FIRST_ROW_AT: usize = 8;
/// Header, `u32`: position of the page's first column.
pub const FIRST_COL_AT: usize = 12;
/// Header, `u16`: number of rows.
pub const ROW_COUNT_AT: usize = 16;
/// Header, `u16`: number of columns.
pub const COL_COUNT_AT: usize = 18;
/// Header, `u32`: length of the text in bytes.
pub const TEXT_LEN_AT: usize = 20;
/// Header, `u32`: the subscription the page was sent for.
pub const EPOCH_AT: usize = 24;

/// Length of a row or column identifier, a `u64`.
pub const ID_LEN: usize = 8;

/// Length of a cell record.
pub const CELL_LEN: usize = 8;
/// Cell record, `u32`: where the cell's text starts, in UTF-16 code units
/// from the start of the page's text.
pub const CELL_TEXT_START_AT: usize = 0;
/// Cell record, `u16`: length of the cell's text in UTF-16 code units.
pub const CELL_TEXT_LEN_AT: usize = 4;
/// Cell record, `u8`: style of the cell. Always zero in this version.
pub const CELL_STYLE_AT: usize = 6;
/// Cell record, `u8`: flags, see the `FLAG_*` constants.
pub const CELL_FLAGS_AT: usize = 7;

/// Cell flag: the cell holds a number, which is shown right-aligned.
pub const FLAG_NUMBER: u8 = 1;
/// Cell flag: the cell holds a boolean (`TRUE` / `FALSE`), shown centered.
pub const FLAG_BOOL: u8 = 2;
/// Cell flag: the cell holds an error (`#DIV/0!`, `#REF!`, ...), shown centered
/// in error color.
pub const FLAG_ERROR: u8 = 4;
/// Cell flag: the cell's displayed value comes from a bound formula.
pub const FLAG_FORMULA: u8 = 8;
/// Cell flag: the cell's display text was truncated at [`MAX_CELL_DISPLAY_UTF16`]
/// UTF-16 code units.
pub const FLAG_TRUNCATED: u8 = 16;

/// Maximum UTF-16 code units written into a viewport page for a single cell
/// (`13-m1a.md` 2.4 & 6.1).
pub const MAX_CELL_DISPLAY_UTF16: u16 = 512;

/// Most page rows one subscription may span.
pub const MAX_SUBSCRIBED_PAGE_ROWS: u32 = 64;
/// Most page columns one subscription may span.
pub const MAX_SUBSCRIBED_PAGE_COLS: u32 = 16;

/// A page, by its position in the grid of pages that covers a sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Page {
    /// Index of the page from the top: rows `row * PAGE_ROWS` onwards.
    pub row: u32,
    /// Index of the page from the left: columns `col * PAGE_COLS` onwards.
    pub col: u32,
}

/// A rectangle of pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageRange {
    first_row: u32,
    end_row: u32,
    first_col: u32,
    end_col: u32,
}

impl PageRange {
    /// The pages that hold the cells of a range, as far as it lies inside a
    /// sheet of `sheet_rows` by `sheet_cols` cells.
    ///
    /// A subscription is a promise to keep sending, so its size is bounded:
    /// a range that needs more than [`MAX_SUBSCRIBED_PAGE_ROWS`] by
    /// [`MAX_SUBSCRIBED_PAGE_COLS`] pages is cut off at the far end.
    #[must_use]
    pub fn covering(
        (sheet_rows, sheet_cols): (u32, u32),
        (first_row, row_count): (u32, u32),
        (first_col, col_count): (u32, u32),
    ) -> Self {
        let (first_row, end_row) = pages_along(
            sheet_rows,
            first_row,
            row_count,
            PAGE_ROWS,
            MAX_SUBSCRIBED_PAGE_ROWS,
        );
        let (first_col, end_col) = pages_along(
            sheet_cols,
            first_col,
            col_count,
            PAGE_COLS,
            MAX_SUBSCRIBED_PAGE_COLS,
        );
        if first_row == end_row || first_col == end_col {
            return Self::default();
        }
        Self {
            first_row,
            end_row,
            first_col,
            end_col,
        }
    }

    /// Whether the range holds no page.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.first_row == self.end_row
    }

    /// Whether `page` is in the range.
    #[must_use]
    pub const fn contains(&self, page: Page) -> bool {
        self.first_row <= page.row
            && page.row < self.end_row
            && self.first_col <= page.col
            && page.col < self.end_col
    }

    /// The pages of the range, row by row.
    pub fn pages(self) -> impl Iterator<Item = Page> {
        (self.first_row..self.end_row)
            .flat_map(move |row| (self.first_col..self.end_col).map(move |col| Page { row, col }))
    }
}

/// The half-open range of pages along one axis that holds `count` cells from
/// `first`, clipped to `extent` cells and to `limit` pages.
fn pages_along(extent: u32, first: u32, count: u32, page: u32, limit: u32) -> (u32, u32) {
    let end = first.saturating_add(count).min(extent);
    if first >= end {
        return (0, 0);
    }
    let first_page = first.checked_div(page).unwrap_or(0);
    let last_page = end.saturating_sub(1).checked_div(page).unwrap_or(0);
    let end_page = last_page
        .saturating_add(1)
        .min(first_page.saturating_add(limit));
    (first_page, end_page)
}

/// Encodes one page of `sheet`, tagged with the subscription it answers,
/// without formula flags.
#[must_use]
pub fn encode_page(sheet: &Sheet, page: Page, epoch: u32, text: &mut String) -> Vec<u8> {
    encode_page_with_formulas(sheet, page, epoch, text, |_, _| false)
}

/// Encodes one page of `sheet`, marking cells for which `is_formula` returns
/// `true` with [`FLAG_FORMULA`].
///
/// `text` is scratch space, so that a run of pages reuses one allocation; its
/// previous content is discarded. A page outside the sheet is encoded with no
/// rows and no columns.
#[must_use]
pub fn encode_page_with_formulas<F>(
    sheet: &Sheet,
    page: Page,
    epoch: u32,
    text: &mut String,
    mut is_formula: F,
) -> Vec<u8>
where
    F: FnMut(RowId, ColId) -> bool,
{
    let first_row = page.row.saturating_mul(PAGE_ROWS);
    let first_col = page.col.saturating_mul(PAGE_COLS);
    let mut rows = sheet.rows().saturating_sub(first_row).min(PAGE_ROWS);
    let mut cols = sheet.cols().saturating_sub(first_col).min(PAGE_COLS);
    if rows == 0 || cols == 0 {
        (rows, cols) = (0, 0);
    }
    let end_row = first_row.saturating_add(rows);
    let end_col = first_col.saturating_add(cols);

    let cells = (rows as usize).saturating_mul(cols as usize);
    let fixed = HEADER_LEN
        .saturating_add(
            (rows as usize)
                .saturating_add(cols as usize)
                .saturating_mul(ID_LEN),
        )
        .saturating_add(cells.saturating_mul(CELL_LEN));
    // Most cells show a handful of characters; this guess avoids regrowing.
    let mut out = Vec::with_capacity(fixed.saturating_add(cells.saturating_mul(8)));

    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&sheet.id().0.to_le_bytes());
    out.extend_from_slice(&first_row.to_le_bytes());
    out.extend_from_slice(&first_col.to_le_bytes());
    out.extend_from_slice(&u16::try_from(rows).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&u16::try_from(cols).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // text length, filled in below
    out.extend_from_slice(&epoch.to_le_bytes());
    out.resize(HEADER_LEN, 0);

    let mut row_ids: SmallVec<[RowId; 64]> = SmallVec::new();
    for row in first_row..end_row {
        let id = sheet.row_id(row);
        row_ids.push(RowId(id));
        out.extend_from_slice(&id.to_le_bytes());
    }
    let mut col_ids: SmallVec<[ColId; 16]> = SmallVec::new();
    for col in first_col..end_col {
        let id = sheet.col_id(col);
        col_ids.push(ColId(id));
        out.extend_from_slice(&id.to_le_bytes());
    }

    text.clear();
    let mut position: u32 = 0;
    for (r_idx, &row_id) in row_ids.iter().enumerate() {
        for (c_idx, &col_id) in col_ids.iter().enumerate() {
            let start = text.len();
            let val = sheet.value_by_id(row_id, col_id);
            sheet.format_value(val, row_id, col_id, text);
            let (units, truncated) = clip_to_record(text, start);
            let mut flags = match val {
                Value::Number(_) => FLAG_NUMBER,
                Value::Bool(_) => FLAG_BOOL,
                Value::Error(_) | Value::Array(_) => FLAG_ERROR,
                Value::Empty | Value::Text(_) => 0,
            };
            if is_formula(row_id, col_id) {
                flags |= FLAG_FORMULA;
            }
            if truncated {
                flags |= FLAG_TRUNCATED;
            }
            let _ = (r_idx, c_idx);
            out.extend_from_slice(&position.to_le_bytes());
            out.extend_from_slice(&units.to_le_bytes());
            out.push(0); // style
            out.push(flags);
            position = position.saturating_add(u32::from(units));
        }
    }

    let text_len = u32::try_from(text.len()).unwrap_or(u32::MAX);
    if let Some(field) = out.get_mut(TEXT_LEN_AT..TEXT_LEN_AT.saturating_add(4)) {
        field.copy_from_slice(&text_len.to_le_bytes());
    }
    out.extend_from_slice(text.as_bytes());
    out
}

/// Measures the text appended to `text` from byte `start` in UTF-16 code
/// units. Text longer than [`MAX_CELL_DISPLAY_UTF16`] UTF-16 code units is cut
/// short on a character boundary and reported as truncated.
fn clip_to_record(text: &mut String, start: usize) -> (u16, bool) {
    let cap = usize::from(MAX_CELL_DISPLAY_UTF16);
    let piece = text.get(start..).unwrap_or("");
    if piece.is_ascii() {
        if piece.len() > cap {
            text.truncate(start.saturating_add(cap));
            return (MAX_CELL_DISPLAY_UTF16, true);
        }
        let units = u16::try_from(piece.len()).unwrap_or(MAX_CELL_DISPLAY_UTF16);
        return (units, false);
    }
    let mut units: u16 = 0;
    let mut cut = None;
    for (offset, character) in piece.char_indices() {
        let width: u16 = if character.len_utf16() == 2 { 2 } else { 1 };
        let next = units.saturating_add(width);
        if next > MAX_CELL_DISPLAY_UTF16 {
            cut = Some(start.saturating_add(offset));
            break;
        }
        units = next;
    }
    if let Some(cut) = cut {
        text.truncate(cut);
        (units, true)
    } else {
        (units, false)
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "the tests compute offsets into pages a few kilobytes long"
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;
    use alloc::vec::Vec;

    use super::*;
    use crate::format::NumberFormat;
    use crate::id::SheetId;
    use crate::sample;
    use crate::value::CalcError;

    /// A page read back the way the grid reads it.
    struct Decoded {
        sheet: u32,
        first_row: u32,
        first_col: u32,
        rows: usize,
        cols: usize,
        epoch: u32,
        row_ids: Vec<u64>,
        col_ids: Vec<u64>,
        /// Text and flags of each cell, row by row.
        cells: Vec<(String, u8)>,
    }

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    fn decode(bytes: &[u8]) -> Decoded {
        assert_eq!(u32_at(bytes, VERSION_AT), VERSION);
        let sheet = u32_at(bytes, SHEET_AT);
        let rows = usize::from(u16_at(bytes, ROW_COUNT_AT));
        let cols = usize::from(u16_at(bytes, COL_COUNT_AT));
        let text_len = u32_at(bytes, TEXT_LEN_AT) as usize;
        let row_ids_at = HEADER_LEN;
        let col_ids_at = row_ids_at + rows * ID_LEN;
        let cells_at = col_ids_at + cols * ID_LEN;
        let text_at = cells_at + rows * cols * CELL_LEN;
        assert_eq!(
            bytes.len(),
            text_at + text_len,
            "the page has no trailing bytes"
        );

        let text = core::str::from_utf8(&bytes[text_at..]).unwrap();
        let units: Vec<u16> = text.encode_utf16().collect();
        let cells = (0..rows * cols)
            .map(|index| {
                let record = cells_at + index * CELL_LEN;
                let start = u32_at(bytes, record + CELL_TEXT_START_AT) as usize;
                let len = usize::from(u16_at(bytes, record + CELL_TEXT_LEN_AT));
                assert_eq!(bytes[record + CELL_STYLE_AT], 0);
                let text = String::from_utf16(&units[start..start + len]).unwrap();
                (text, bytes[record + CELL_FLAGS_AT])
            })
            .collect();
        Decoded {
            sheet,
            first_row: u32_at(bytes, FIRST_ROW_AT),
            first_col: u32_at(bytes, FIRST_COL_AT),
            rows,
            cols,
            epoch: u32_at(bytes, EPOCH_AT),
            row_ids: (0..rows)
                .map(|row| u64_at(bytes, row_ids_at + row * ID_LEN))
                .collect(),
            col_ids: (0..cols)
                .map(|col| u64_at(bytes, col_ids_at + col * ID_LEN))
                .collect(),
            cells,
        }
    }

    fn shown(sheet: &Sheet, row: u32, col: u32) -> String {
        let mut out = String::new();
        sheet.write_text(row, col, &mut out);
        out
    }

    #[test]
    fn a_page_reads_back_as_the_cells_it_was_made_from() {
        let sheet = sample::generate(11, 150);
        let mut scratch = String::from("left over from the page before");
        for page in [
            Page { row: 0, col: 0 },
            Page { row: 1, col: 1 },
            Page { row: 2, col: 0 },
        ] {
            let decoded = decode(&encode_page(&sheet, page, 7, &mut scratch));
            assert_eq!(decoded.sheet, 0);
            assert_eq!(decoded.epoch, 7);
            assert_eq!(decoded.first_row, page.row * PAGE_ROWS);
            assert_eq!(decoded.first_col, page.col * PAGE_COLS);
            assert_eq!(
                decoded.rows as u32,
                (150 - decoded.first_row).min(PAGE_ROWS)
            );
            assert_eq!(decoded.cols as u32, (20 - decoded.first_col).min(PAGE_COLS));
            for (index, (text, flags)) in decoded.cells.iter().enumerate() {
                let row = decoded.first_row + (index / decoded.cols) as u32;
                let col = decoded.first_col + (index % decoded.cols) as u32;
                assert_eq!(*text, shown(&sheet, row, col), "row {row}, column {col}");
                let is_number = matches!(sheet.value(row, col), Value::Number(_));
                assert_eq!(*flags, if is_number { FLAG_NUMBER } else { 0 });
            }
            let rows = decoded.first_row..decoded.first_row + decoded.rows as u32;
            assert!(
                decoded
                    .row_ids
                    .iter()
                    .copied()
                    .eq(rows.map(|row| sheet.row_id(row)))
            );
            let cols = decoded.first_col..decoded.first_col + decoded.cols as u32;
            assert!(
                decoded
                    .col_ids
                    .iter()
                    .copied()
                    .eq(cols.map(|col| sheet.col_id(col)))
            );
        }
    }

    #[test]
    fn v1_flags_report_bool_error_formula_and_sheet_id() {
        let mut sheet = Sheet::with_axes(SheetId(3), String::from("Data"), 2, 4);
        assert!(sheet.set_value_by_id(RowId(0), ColId(0), Value::Bool(true)));
        assert!(sheet.set_value_by_id(RowId(0), ColId(1), Value::Error(CalcError::Div0)));
        assert!(sheet.set_value_by_id(RowId(0), ColId(2), Value::Number(42.0)));
        let txt = sheet.add_string("plain").unwrap();
        assert!(sheet.set_value_by_id(RowId(0), ColId(3), Value::Text(txt)));

        let bytes = encode_page_with_formulas(
            &sheet,
            Page { row: 0, col: 0 },
            9,
            &mut String::new(),
            |r, c| r == RowId(0) && (c == ColId(1) || c == ColId(2)),
        );
        let decoded = decode(&bytes);
        assert_eq!(decoded.sheet, 3);
        assert_eq!(decoded.cells[0], (String::from("TRUE"), FLAG_BOOL));
        assert_eq!(
            decoded.cells[1],
            (String::from("#DIV/0!"), FLAG_ERROR | FLAG_FORMULA)
        );
        assert_eq!(
            decoded.cells[2],
            (String::from("42"), FLAG_NUMBER | FLAG_FORMULA)
        );
        assert_eq!(decoded.cells[3], (String::from("plain"), 0));
    }

    #[test]
    fn text_positions_count_utf16_code_units() {
        let mut sheet = Sheet::new(&[NumberFormat::General; 3]);
        // Three bytes and one unit; four bytes and two units; one and one.
        let cells =
            ["华", "😀", "a", "", "𝒳-ray 华东"].map(|text| sheet.add_string(text).map(Value::Text));
        assert!(sheet.push_row(&[cells[0].unwrap(), cells[1].unwrap(), cells[2].unwrap()]));
        assert!(sheet.push_row(&[cells[3].unwrap(), Value::Number(-2.5), cells[4].unwrap()]));
        let bytes = encode_page(&sheet, Page { row: 0, col: 0 }, 0, &mut String::new());

        let decoded = decode(&bytes);
        let texts: Vec<&str> = decoded
            .cells
            .iter()
            .map(|(text, _)| text.as_str())
            .collect();
        assert_eq!(texts, ["华", "😀", "a", "", "-2.5", "𝒳-ray 华东"]);

        let cells_at = HEADER_LEN + 2 * ID_LEN + 3 * ID_LEN;
        let record = |index: usize| {
            let at = cells_at + index * CELL_LEN;
            (
                u32_at(&bytes, at + CELL_TEXT_START_AT),
                u16_at(&bytes, at + CELL_TEXT_LEN_AT),
            )
        };
        assert_eq!(
            [0, 1, 2, 3, 4, 5].map(record),
            [(0, 1), (1, 2), (3, 1), (4, 0), (4, 4), (8, 9)]
        );
    }

    #[test]
    fn a_page_outside_the_sheet_is_empty() {
        let sheet = sample::generate(3, 10);
        for page in [
            Page { row: 1, col: 0 },
            Page { row: 0, col: 2 },
            Page {
                row: u32::MAX,
                col: u32::MAX,
            },
        ] {
            let bytes = encode_page(&sheet, page, 1, &mut String::new());
            let decoded = decode(&bytes);
            assert_eq!((decoded.rows, decoded.cols), (0, 0));
            assert_eq!(bytes.len(), HEADER_LEN);
        }
        let empty = Sheet::new(&[]);
        assert_eq!(
            encode_page(&empty, Page { row: 0, col: 0 }, 1, &mut String::new()).len(),
            HEADER_LEN
        );
    }

    #[test]
    fn text_too_long_for_a_record_is_cut_at_512_utf16_units_and_marked_truncated() {
        let mut sheet = Sheet::new(&[NumberFormat::General; 2]);
        // One unit short of MAX_CELL_DISPLAY_UTF16 (511), then a surrogate-pair emoji (2 units).
        let mut long = "a".repeat(usize::from(MAX_CELL_DISPLAY_UTF16) - 1);
        long.push('😀');
        let ascii = "b".repeat(2_000);
        let cells =
            [long.as_str(), ascii.as_str()].map(|text| sheet.add_string(text).map(Value::Text));
        assert!(sheet.push_row(&[cells[0].unwrap(), cells[1].unwrap()]));

        let decoded = decode(&encode_page(
            &sheet,
            Page { row: 0, col: 0 },
            0,
            &mut String::new(),
        ));
        assert_eq!(
            decoded.cells[0].0.len(),
            usize::from(MAX_CELL_DISPLAY_UTF16) - 1
        );
        assert!(decoded.cells[0].0.bytes().all(|byte| byte == b'a'));
        assert_eq!(decoded.cells[0].1, FLAG_TRUNCATED);
        assert_eq!(
            decoded.cells[1].0.len(),
            usize::from(MAX_CELL_DISPLAY_UTF16)
        );
        assert_eq!(decoded.cells[1].1, FLAG_TRUNCATED);
    }

    fn pages(range: PageRange) -> Vec<(u32, u32)> {
        range.pages().map(|page| (page.row, page.col)).collect()
    }

    #[test]
    fn a_range_is_covered_by_the_pages_it_touches() {
        let sheet = (1_000_000, 20);
        // Rows 100 to 199 are in page rows 1 to 3; columns 0 to 11 in page column 0.
        let range = PageRange::covering(sheet, (100, 100), (0, 12));
        assert_eq!(pages(range), [(1, 0), (2, 0), (3, 0)]);
        assert!(range.contains(Page { row: 2, col: 0 }));
        assert!(!range.contains(Page { row: 0, col: 0 }));
        assert!(!range.contains(Page { row: 2, col: 1 }));

        // Exactly one page, up to its last cell.
        assert_eq!(
            pages(PageRange::covering(sheet, (64, 64), (16, 4))),
            [(1, 1)]
        );
        // One cell more crosses into the next page.
        assert_eq!(
            pages(PageRange::covering(sheet, (64, 65), (15, 2))),
            [(1, 0), (1, 1), (2, 0), (2, 1)]
        );
    }

    #[test]
    fn a_range_is_clipped_to_the_sheet() {
        let sheet = (130, 20);
        // The last page row is the third, and there are two page columns.
        assert_eq!(
            pages(PageRange::covering(sheet, (120, 1000), (10, 1000))),
            [(1, 0), (1, 1), (2, 0), (2, 1)]
        );
        assert_eq!(
            pages(PageRange::covering(sheet, (0, u32::MAX), (0, u32::MAX))).len(),
            6
        );
        // Nothing of it lies inside the sheet.
        for range in [
            PageRange::covering(sheet, (130, 10), (0, 10)),
            PageRange::covering(sheet, (0, 10), (20, 10)),
            PageRange::covering(sheet, (0, 0), (0, 10)),
            PageRange::covering(sheet, (u32::MAX, u32::MAX), (0, 10)),
            PageRange::covering((0, 0), (0, 10), (0, 10)),
        ] {
            assert!(range.is_empty());
            assert_eq!(range, PageRange::default());
            assert_eq!(pages(range), []);
        }
    }

    #[test]
    fn a_subscription_is_bounded() {
        let range = PageRange::covering((u32::MAX, u32::MAX), (640, u32::MAX), (0, u32::MAX));
        let pages = pages(range);
        assert_eq!(
            pages.len() as u32,
            MAX_SUBSCRIBED_PAGE_ROWS * MAX_SUBSCRIBED_PAGE_COLS
        );
        assert_eq!(pages.first(), Some(&(10, 0)));
        assert_eq!(
            pages.last(),
            Some(&(
                10 + MAX_SUBSCRIBED_PAGE_ROWS - 1,
                MAX_SUBSCRIBED_PAGE_COLS - 1
            ))
        );
    }
}
