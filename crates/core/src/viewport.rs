//! Viewport pages v2: binary cell, style, and left-overflow packets (`14-m1b.md` 6.2).
//!
//! # Layout (Version 2)
//!
//! All integers are little-endian and naturally aligned. A page of `R` rows,
//! `C` columns, and `K` left-overflow candidates (`0..=R`, always `0` when
//! `first_col == 0`) is laid out as:
//!
//! | bytes | content |
//! |---|---|
//! | [`HEADER_LEN`] (`32`) | header (`VERSION = 2`, `sheet`, `first_row`, `first_col`, `rows`, `cols`, `text_len`, `epoch`, `overflow_count`, `_pad`) |
//! | `R` × [`ID_LEN`] (`8`) | stable `RowId` of each row |
//! | `C` × [`ID_LEN`] (`8`) | stable `ColId` of each column |
//! | `R` × `C` × [`CELL_LEN`] (`12`) | cell records `[text_start: u32, text_len: u16, style_id: u16, flags: u16, _pad: u16]` |
//! | `K` × [`OVERFLOW_LEN`] (`12`) | left-overflow records `[text_start: u32, text_len: u16, row_offset: u16, origin_col: u16, style_id: u16]` |
//! | `text_len` | UTF-8 string pool referenced by cell and overflow UTF-16 spans |

use alloc::string::String;
use alloc::vec::Vec;
use smallvec::SmallVec;

use crate::id::{ColId, RowId};
use crate::sheet::Sheet;
use crate::style::HorizontalAlign;
use crate::value::{CalcError, Value};

/// Version of the viewport page binary layout (`14-m1b.md` 6.2).
pub const VERSION: u32 = 2;

/// Rows in a full page. Pages at the bottom edge of a sheet have fewer.
pub const PAGE_ROWS: u32 = 64;
/// Columns in a full page. Pages at the right edge of a sheet have fewer.
pub const PAGE_COLS: u32 = 16;

/// Byte length of the page header.
pub const HEADER_LEN: usize = 32;
/// Header, `u32`: [`VERSION`].
pub const VERSION_AT: usize = 0;
/// Header, `u32`: worksheet identifier (`SheetId`).
pub const SHEET_AT: usize = 4;
/// Header, `u32`: 0-based visible row position of the page's first row.
pub const FIRST_ROW_AT: usize = 8;
/// Header, `u32`: 0-based visible column position of the page's first column.
pub const FIRST_COL_AT: usize = 12;
/// Header, `u16`: number of rows in this page (`0..=64`).
pub const ROW_COUNT_AT: usize = 16;
/// Header, `u16`: number of columns in this page (`0..=16`).
pub const COL_COUNT_AT: usize = 18;
/// Header, `u32`: byte length of the trailing UTF-8 text pool.
pub const TEXT_LEN_AT: usize = 20;
/// Header, `u32`: subscription epoch.
pub const EPOCH_AT: usize = 24;
/// Header, `u16`: count `K` of trailing left-overflow candidate records.
pub const OVERFLOW_COUNT_AT: usize = 28;

/// Byte length of a `RowId` or `ColId` (`u64`).
pub const ID_LEN: usize = 8;

/// Byte length of one cell record (naturally 4-byte aligned, `14-m1b.md` 6.2).
pub const CELL_LEN: usize = 12;
/// Cell record, `u32`: UTF-16 code unit start offset in the page's text pool.
pub const CELL_TEXT_START_AT: usize = 0;
/// Cell record, `u16`: UTF-16 code unit length (`0..=512`).
pub const CELL_TEXT_LEN_AT: usize = 4;
/// Cell record, `u16`: `StyleId` index into `SheetInfo.styles` (`0` = default).
pub const CELL_STYLE_AT: usize = 6;
/// Cell record, `u16`: bit flags (`FLAG_*`).
pub const CELL_FLAGS_AT: usize = 8;

/// Byte length of one left-overflow candidate record (naturally 4-byte aligned).
pub const OVERFLOW_LEN: usize = 12;
/// Overflow record, `u32`: UTF-16 code unit start offset in the page's text pool.
pub const OVERFLOW_TEXT_START_AT: usize = 0;
/// Overflow record, `u16`: UTF-16 code unit length (`1..=512`).
pub const OVERFLOW_TEXT_LEN_AT: usize = 4;
/// Overflow record, `u16`: row index within this page (`0..rows`).
pub const OVERFLOW_ROW_OFFSET_AT: usize = 6;
/// Overflow record, `u16`: 0-based visible column position `< first_col` where the text originates.
pub const OVERFLOW_ORIGIN_COL_AT: usize = 8;
/// Overflow record, `u16`: `StyleId` of the originating cell.
pub const OVERFLOW_STYLE_AT: usize = 10;

/// Maximum columns scanned to the left of `first_col` for overflowing text (`14-m1b.md` 6.2).
pub const MAX_LEFT_OVERFLOW_LOOKBACK: u32 = 16;

/// Cell flag: numeric value (right-aligned when style alignment is `Auto`).
pub const FLAG_NUMBER: u16 = 1;
/// Cell flag: boolean value (`TRUE` / `FALSE`, centered when `Auto`).
pub const FLAG_BOOL: u16 = 2;
/// Cell flag: error value (`#DIV/0!`, `#REF!`, ..., centered in error color).
pub const FLAG_ERROR: u16 = 4;
/// Cell flag: cell carries a bound formula.
pub const FLAG_FORMULA: u16 = 8;
/// Cell flag: display text was clipped at [`MAX_CELL_DISPLAY_UTF16`] UTF-16 units.
pub const FLAG_TRUNCATED: u16 = 16;
/// Cell flag: cell value comes from a dynamic array spill shadow (not an anchor).
pub const FLAG_SPILL: u16 = 32;

/// Maximum UTF-16 code units written into a viewport page for a single cell.
pub const MAX_CELL_DISPLAY_UTF16: u16 = 512;

/// Most page rows one subscription may span.
pub const MAX_SUBSCRIBED_PAGE_ROWS: u32 = 64;
/// Most page columns one subscription may span.
pub const MAX_SUBSCRIBED_PAGE_COLS: u32 = 16;

/// Coordinates of one `64 × 16` page in the sheet's page grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Page {
    /// Page row (`row * PAGE_ROWS`).
    pub row: u32,
    /// Page column (`col * PAGE_COLS`).
    pub col: u32,
}

/// Half-open rectangle of pages `[first_row..end_row) × [first_col..end_col)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageRange {
    first_row: u32,
    end_row: u32,
    first_col: u32,
    end_col: u32,
}

impl PageRange {
    /// Pages covering `[first_row .. first_row + row_count)` by
    /// `[first_col .. first_col + col_count)`, clipped to `sheet_rows × sheet_cols`
    /// and bounded to [`MAX_SUBSCRIBED_PAGE_ROWS`] × [`MAX_SUBSCRIBED_PAGE_COLS`].
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

    /// First page row index.
    #[must_use]
    pub const fn first_row(&self) -> u32 {
        self.first_row
    }

    /// Exclusive end page row index.
    #[must_use]
    pub const fn end_row(&self) -> u32 {
        self.end_row
    }

    /// First page column index.
    #[must_use]
    pub const fn first_col(&self) -> u32 {
        self.first_col
    }

    /// Exclusive end page column index.
    #[must_use]
    pub const fn end_col(&self) -> u32 {
        self.end_col
    }

    /// Whether the range holds zero pages.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.first_row == self.end_row
    }

    /// Whether `page` lies inside this rectangle.
    #[must_use]
    pub const fn contains(&self, page: Page) -> bool {
        self.first_row <= page.row
            && page.row < self.end_row
            && self.first_col <= page.col
            && page.col < self.end_col
    }

    /// Iterates pages row-major.
    pub fn pages(self) -> impl Iterator<Item = Page> {
        (self.first_row..self.end_row)
            .flat_map(move |row| (self.first_col..self.end_col).map(move |col| Page { row, col }))
    }
}

/// Returns `true` when `page` belongs either to the main `subscribed` viewport
/// or to the pinned freeze strips (`0..frozen_rows` along `subscribed` cols,
/// `0..frozen_cols` along `subscribed` rows, and their top-left corner;
/// `14-m1b.md` 2.5).
#[must_use]
pub fn is_page_wanted_with_freeze(
    subscribed: PageRange,
    frozen_rows: u32,
    frozen_cols: u32,
    page: Page,
) -> bool {
    if subscribed.is_empty() {
        return false;
    }
    if subscribed.contains(page) {
        return true;
    }
    let pinned_row_pages = if frozen_rows == 0 {
        0
    } else {
        frozen_rows
            .saturating_sub(1)
            .checked_div(PAGE_ROWS)
            .unwrap_or(0)
            .saturating_add(1)
    };
    let pinned_col_pages = if frozen_cols == 0 {
        0
    } else {
        frozen_cols
            .saturating_sub(1)
            .checked_div(PAGE_COLS)
            .unwrap_or(0)
            .saturating_add(1)
    };
    let in_top_strip = page.row < pinned_row_pages
        && subscribed.first_col <= page.col
        && page.col < subscribed.end_col;
    let in_left_strip = page.col < pinned_col_pages
        && subscribed.first_row <= page.row
        && page.row < subscribed.end_row;
    let in_corner = page.row < pinned_row_pages && page.col < pinned_col_pages;
    in_top_strip || in_left_strip || in_corner
}

/// Collects all unique pages wanted by `subscribed` together with pinned freeze
/// strips (pinned corner & top/left strips first so frozen headers paint
/// immediately, followed by main viewport pages).
#[must_use]
pub fn collect_wanted_pages_with_freeze(
    subscribed: PageRange,
    frozen_rows: u32,
    frozen_cols: u32,
) -> Vec<Page> {
    if subscribed.is_empty() {
        return Vec::new();
    }
    let pinned_r = if frozen_rows == 0 {
        0
    } else {
        frozen_rows
            .saturating_sub(1)
            .checked_div(PAGE_ROWS)
            .unwrap_or(0)
            .saturating_add(1)
    };
    let pinned_c = if frozen_cols == 0 {
        0
    } else {
        frozen_cols
            .saturating_sub(1)
            .checked_div(PAGE_COLS)
            .unwrap_or(0)
            .saturating_add(1)
    };

    let mut out = Vec::new();
    let mut push_unique = |p: Page| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    for r in 0..pinned_r {
        for c in 0..pinned_c {
            push_unique(Page { row: r, col: c });
        }
        for c in subscribed.first_col..subscribed.end_col {
            push_unique(Page { row: r, col: c });
        }
    }
    for r in subscribed.first_row..subscribed.end_row {
        for c in 0..pinned_c {
            push_unique(Page { row: r, col: c });
        }
    }
    for p in subscribed.pages() {
        push_unique(p);
    }
    out
}

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

/// Ensures resolved styles for all cells (and left-overflow origin cells) on
/// `page` are interned in `sheet.styles_mut()` prior to calling [`encode_page_with_formulas`].
pub fn prepare_page_styles(sheet: &mut Sheet, page: Page) {
    if sheet.col_styles().is_empty()
        && sheet.row_styles().is_empty()
        && sheet.cell_styles().is_empty()
    {
        return;
    }
    let first_row = page.row.saturating_mul(PAGE_ROWS);
    let first_col = page.col.saturating_mul(PAGE_COLS);
    let rows = sheet.rows().saturating_sub(first_row).min(PAGE_ROWS);
    let cols = sheet.cols().saturating_sub(first_col).min(PAGE_COLS);
    if rows == 0 || cols == 0 {
        return;
    }
    let lookback_start = first_col.saturating_sub(MAX_LEFT_OVERFLOW_LOOKBACK);
    let end_row = first_row.saturating_add(rows);
    let end_col = first_col.saturating_add(cols);

    let mut row_ids: SmallVec<[RowId; 64]> = SmallVec::new();
    for r in first_row..end_row {
        row_ids.push(RowId(sheet.row_id(r)));
    }
    let mut col_ids: SmallVec<[ColId; 32]> = SmallVec::new();
    for c in lookback_start..end_col {
        col_ids.push(ColId(sheet.col_id(c)));
    }
    sheet.prepare_viewport_styles(&row_ids, &col_ids);
}

/// Borrowed value override for a single viewport cell (used so spilled values,
/// including spilled strings, render without mutating [`Sheet::strings`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayValue<'a> {
    /// Numeric value formatted with the cell's resolved number format.
    Number(f64),
    /// Boolean value (`TRUE` / `FALSE`).
    Bool(bool),
    /// Formula error value (`#SPILL!`, `#DIV/0!`, ...).
    Error(CalcError),
    /// Borrowed string slice.
    Text(&'a str),
    /// Explicit empty cell override.
    Empty,
}

impl OverlayValue<'_> {
    const fn is_empty(self) -> bool {
        matches!(self, Self::Empty)
    }

    const fn is_text(self) -> bool {
        matches!(self, Self::Text(_))
    }
}

/// Cell overlay supplied when encoding a viewport page (e.g., formula flags
/// and materialized dynamic-array spill shadow values).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportCellOverlay<'a> {
    /// Optional value override (used for spill shadow cells whose sheet slot is `Empty`).
    pub value: Option<OverlayValue<'a>>,
    /// Additional bit flags (`FLAG_FORMULA`, `FLAG_SPILL`) ORed into the cell's flags.
    pub extra_flags: u16,
}

/// Encodes one v2 page of `sheet`, without formula flags or spill overlays.
#[must_use]
pub fn encode_page(sheet: &Sheet, page: Page, epoch: u32, text: &mut String) -> Vec<u8> {
    encode_page_with_overlay(sheet, page, epoch, text, |_, _| {
        ViewportCellOverlay::default()
    })
}

/// Encodes one v2 page of `sheet`, marking cells where `is_formula` returns
/// `true` with [`FLAG_FORMULA`], recording each cell's `StyleId`, and appending
/// left-overflow text candidates when `first_col > 0` (`14-m1b.md` 6.2).
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
    encode_page_with_overlay(sheet, page, epoch, text, |r, c| ViewportCellOverlay {
        value: None,
        extra_flags: if is_formula(r, c) { FLAG_FORMULA } else { 0 },
    })
}

fn format_overlay_or_sheet_value(
    sheet: &Sheet,
    row_id: RowId,
    col_id: ColId,
    override_val: Option<OverlayValue<'_>>,
    text: &mut String,
) -> (u16, bool, bool) {
    match override_val {
        Some(OverlayValue::Empty) => (0, true, false),
        Some(OverlayValue::Number(n)) => {
            let v = Value::number(n);
            sheet.format_value(v, row_id, col_id, text);
            let f = if matches!(v, Value::Number(_)) {
                FLAG_NUMBER
            } else {
                FLAG_ERROR
            };
            (f, false, false)
        }
        Some(OverlayValue::Bool(b)) => {
            sheet.format_value(Value::Bool(b), row_id, col_id, text);
            (FLAG_BOOL, false, false)
        }
        Some(OverlayValue::Error(e)) => {
            text.push_str(e.as_str());
            (FLAG_ERROR, false, false)
        }
        Some(OverlayValue::Text(s)) => {
            text.push_str(s);
            (0, false, true)
        }
        None => {
            let val = sheet.value_by_id(row_id, col_id);
            sheet.format_value(val, row_id, col_id, text);
            let flags = match val {
                Value::Number(_) => FLAG_NUMBER,
                Value::Bool(_) => FLAG_BOOL,
                Value::Error(_) | Value::Array(_) => FLAG_ERROR,
                Value::Empty | Value::Text(_) => 0,
            };
            (flags, val == Value::Empty, matches!(val, Value::Text(_)))
        }
    }
}

/// Encodes one v2 page of `sheet` with per-cell value/flag overlays (e.g.
/// [`FLAG_FORMULA`] and materialized [`FLAG_SPILL`] shadow values), recording
/// each cell's `StyleId` and appending left-overflow text candidates when
/// `first_col > 0` (`14-m1b.md` 6.2, `17-m3a.md`).
#[must_use]
pub fn encode_page_with_overlay<'a, F>(
    sheet: &Sheet,
    page: Page,
    epoch: u32,
    text: &mut String,
    mut overlay: F,
) -> Vec<u8>
where
    F: FnMut(RowId, ColId) -> ViewportCellOverlay<'a>,
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
    let mut out = Vec::with_capacity(fixed.saturating_add(cells.saturating_mul(8)));

    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&sheet.id().0.to_le_bytes());
    out.extend_from_slice(&first_row.to_le_bytes());
    out.extend_from_slice(&first_col.to_le_bytes());
    out.extend_from_slice(&u16::try_from(rows).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&u16::try_from(cols).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // text_len, patched below
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // overflow_count, patched below
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
    for &row_id in &row_ids {
        for &col_id in &col_ids {
            let start = text.len();
            let cell_overlay = overlay(row_id, col_id);
            let (mut flags, _, _) =
                format_overlay_or_sheet_value(sheet, row_id, col_id, cell_overlay.value, text);
            let (units, truncated) = clip_to_record(text, start);
            flags |= cell_overlay.extra_flags;
            if truncated {
                flags |= FLAG_TRUNCATED;
            }
            let style_id = sheet.effective_style_id(row_id, col_id).0;
            out.extend_from_slice(&position.to_le_bytes());
            out.extend_from_slice(&units.to_le_bytes());
            out.extend_from_slice(&style_id.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&0_u16.to_le_bytes());
            position = position.saturating_add(u32::from(units));
        }
    }

    // Left-overflow candidate scan (`first_col > 0`, up to 16 columns back).
    let mut overflow_count: u16 = 0;
    if first_col > 0
        && let Some(&first_page_col_id) = col_ids.first()
    {
        let min_scan_col = first_col.saturating_sub(MAX_LEFT_OVERFLOW_LOOKBACK);
        let mut lookback_col_ids: SmallVec<[(u32, ColId); 16]> = SmallVec::new();
        let mut c = first_col;
        while c > min_scan_col {
            c = c.saturating_sub(1);
            lookback_col_ids.push((c, ColId(sheet.col_id(c))));
        }

        for (r_idx, &row_id) in row_ids.iter().enumerate() {
            let first_is_empty = overlay(row_id, first_page_col_id).value.map_or_else(
                || sheet.value_by_id(row_id, first_page_col_id) == Value::Empty,
                OverlayValue::is_empty,
            );
            if !first_is_empty {
                continue;
            }
            for &(origin_col_pos, origin_col_id) in &lookback_col_ids {
                let ov_val = overlay(row_id, origin_col_id).value;
                let (is_empty, is_text) = match ov_val {
                    Some(ov) => (ov.is_empty(), ov.is_text()),
                    None => {
                        let v = sheet.value_by_id(row_id, origin_col_id);
                        (v == Value::Empty, matches!(v, Value::Text(_)))
                    }
                };
                if is_empty {
                    continue;
                }
                let eff_style = sheet.effective_style_by_id(row_id, origin_col_id);
                let is_left_text = if is_text {
                    matches!(
                        eff_style.align,
                        HorizontalAlign::Auto | HorizontalAlign::Left
                    )
                } else {
                    eff_style.align == HorizontalAlign::Left
                };
                if is_left_text {
                    let start = text.len();
                    let _ =
                        format_overlay_or_sheet_value(sheet, row_id, origin_col_id, ov_val, text);
                    let (units, _) = clip_to_record(text, start);
                    if units > 0
                        && let (Ok(r_off), Ok(orig_c)) =
                            (u16::try_from(r_idx), u16::try_from(origin_col_pos))
                    {
                        let style_id = sheet.effective_style_id(row_id, origin_col_id).0;
                        out.extend_from_slice(&position.to_le_bytes());
                        out.extend_from_slice(&units.to_le_bytes());
                        out.extend_from_slice(&r_off.to_le_bytes());
                        out.extend_from_slice(&orig_c.to_le_bytes());
                        out.extend_from_slice(&style_id.to_le_bytes());
                        position = position.saturating_add(u32::from(units));
                        overflow_count = overflow_count.saturating_add(1);
                    }
                }
                break;
            }
        }
    }

    let text_len = u32::try_from(text.len()).unwrap_or(u32::MAX);
    if let Some(field) = out.get_mut(TEXT_LEN_AT..TEXT_LEN_AT.saturating_add(4)) {
        field.copy_from_slice(&text_len.to_le_bytes());
    }
    if let Some(field) = out.get_mut(OVERFLOW_COUNT_AT..OVERFLOW_COUNT_AT.saturating_add(2)) {
        field.copy_from_slice(&overflow_count.to_le_bytes());
    }
    out.extend_from_slice(text.as_bytes());
    out
}

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
    clippy::indexing_slicing,
    clippy::unwrap_used,
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
    use crate::style::StylePatch;
    use crate::value::CalcError;

    struct Decoded {
        sheet: u32,
        first_row: u32,
        first_col: u32,
        rows: usize,
        cols: usize,
        epoch: u32,
        row_ids: Vec<u64>,
        col_ids: Vec<u64>,
        cells: Vec<(String, u16, u16)>,
        overflows: Vec<(u16, u16, u16, String)>,
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
        let overflow_count = usize::from(u16_at(bytes, OVERFLOW_COUNT_AT));
        let row_ids_at = HEADER_LEN;
        let col_ids_at = row_ids_at + rows * ID_LEN;
        let cells_at = col_ids_at + cols * ID_LEN;
        let overflows_at = cells_at + rows * cols * CELL_LEN;
        let text_at = overflows_at + overflow_count * OVERFLOW_LEN;
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
                let style = u16_at(bytes, record + CELL_STYLE_AT);
                let flags = u16_at(bytes, record + CELL_FLAGS_AT);
                let s = String::from_utf16(&units[start..start + len]).unwrap();
                (s, style, flags)
            })
            .collect();
        let overflows = (0..overflow_count)
            .map(|index| {
                let record = overflows_at + index * OVERFLOW_LEN;
                let start = u32_at(bytes, record + OVERFLOW_TEXT_START_AT) as usize;
                let len = usize::from(u16_at(bytes, record + OVERFLOW_TEXT_LEN_AT));
                let row_off = u16_at(bytes, record + OVERFLOW_ROW_OFFSET_AT);
                let orig_col = u16_at(bytes, record + OVERFLOW_ORIGIN_COL_AT);
                let style = u16_at(bytes, record + OVERFLOW_STYLE_AT);
                let s = String::from_utf16(&units[start..start + len]).unwrap();
                (row_off, orig_col, style, s)
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
            overflows,
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
            for (index, (text, style, flags)) in decoded.cells.iter().enumerate() {
                let row = decoded.first_row + (index / decoded.cols) as u32;
                let col = decoded.first_col + (index % decoded.cols) as u32;
                assert_eq!(*text, shown(&sheet, row, col), "row {row}, column {col}");
                assert_eq!(*style, 0);
                let is_number = matches!(sheet.value(row, col), Value::Number(_));
                assert_eq!(*flags, if is_number { FLAG_NUMBER } else { 0 });
            }
            let rows = decoded.first_row..decoded.first_row + decoded.rows as u32;
            assert!(
                decoded
                    .row_ids
                    .iter()
                    .copied()
                    .eq(rows.map(|r| sheet.row_id(r)))
            );
            let cols = decoded.first_col..decoded.first_col + decoded.cols as u32;
            assert!(
                decoded
                    .col_ids
                    .iter()
                    .copied()
                    .eq(cols.map(|c| sheet.col_id(c)))
            );
        }
    }

    #[test]
    fn v2_flags_styles_and_left_overflow_records_work() {
        let mut sheet = Sheet::with_axes(SheetId(3), String::from("Data"), 3, 20);
        assert!(sheet.set_value_by_id(RowId(0), ColId(0), Value::Bool(true)));
        assert!(sheet.set_value_by_id(RowId(0), ColId(1), Value::Error(CalcError::Div0)));
        assert!(sheet.set_value_by_id(RowId(0), ColId(2), Value::Number(42.0)));
        sheet.set_cell_style_patch(
            RowId(0),
            ColId(2),
            StylePatch {
                bold: Some(true),
                ..StylePatch::default()
            },
        );
        let long_txt = sheet
            .add_string("Long overflowing text across page boundary")
            .unwrap();
        assert!(sheet.set_value_by_id(RowId(1), ColId(14), Value::Text(long_txt)));
        assert!(sheet.set_value_by_id(RowId(2), ColId(14), Value::Text(long_txt)));
        assert!(sheet.set_value_by_id(RowId(2), ColId(15), Value::Number(9.0)));

        let mut scratch = String::new();
        prepare_page_styles(&mut sheet, Page { row: 0, col: 0 });
        let p0 = decode(&encode_page_with_formulas(
            &sheet,
            Page { row: 0, col: 0 },
            9,
            &mut scratch,
            |r, c| r == RowId(0) && (c == ColId(1) || c == ColId(2)),
        ));
        assert_eq!(p0.sheet, 3);
        assert_eq!(p0.cells[0], (String::from("TRUE"), 0, FLAG_BOOL));
        assert_eq!(
            p0.cells[1],
            (String::from("#DIV/0!"), 0, FLAG_ERROR | FLAG_FORMULA)
        );
        assert_eq!(
            p0.cells[2],
            (String::from("42"), 1, FLAG_NUMBER | FLAG_FORMULA)
        );
        assert!(p0.overflows.is_empty());

        prepare_page_styles(&mut sheet, Page { row: 0, col: 1 });
        let p1 = decode(&encode_page(
            &sheet,
            Page { row: 0, col: 1 },
            9,
            &mut scratch,
        ));
        assert_eq!(p1.overflows.len(), 1);
        assert_eq!(
            p1.overflows[0],
            (
                1,
                14,
                0,
                String::from("Long overflowing text across page boundary")
            )
        );
    }

    #[test]
    fn text_positions_count_utf16_code_units() {
        let mut sheet = Sheet::new(&[NumberFormat::General; 3]);
        let cells =
            ["华", "😀", "a", "", "𝒳-ray 华东"].map(|text| sheet.add_string(text).map(Value::Text));
        assert!(sheet.push_row(&[cells[0].unwrap(), cells[1].unwrap(), cells[2].unwrap()]));
        assert!(sheet.push_row(&[cells[3].unwrap(), Value::Number(-2.5), cells[4].unwrap()]));
        let bytes = encode_page(&sheet, Page { row: 0, col: 0 }, 0, &mut String::new());

        let decoded = decode(&bytes);
        let texts: Vec<&str> = decoded
            .cells
            .iter()
            .map(|(text, _, _)| text.as_str())
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
        assert_eq!(decoded.cells[0].2, FLAG_TRUNCATED);
        assert_eq!(
            decoded.cells[1].0.len(),
            usize::from(MAX_CELL_DISPLAY_UTF16)
        );
        assert_eq!(decoded.cells[1].2, FLAG_TRUNCATED);
    }

    fn pages(range: PageRange) -> Vec<(u32, u32)> {
        range.pages().map(|page| (page.row, page.col)).collect()
    }

    #[test]
    fn a_range_is_covered_by_the_pages_it_touches_and_freeze_pins_strips() {
        let sheet = (1_000_000, 40);
        let range = PageRange::covering(sheet, (5000, 64), (16, 16));
        assert_eq!(pages(range), [(78, 1), (79, 1)]);

        let wanted = collect_wanted_pages_with_freeze(range, 1, 2);
        let coords: Vec<(u32, u32)> = wanted.iter().map(|p| (p.row, p.col)).collect();
        assert_eq!(coords, [(0, 0), (0, 1), (78, 0), (79, 0), (78, 1), (79, 1)]);
        assert!(is_page_wanted_with_freeze(
            range,
            1,
            2,
            Page { row: 0, col: 1 }
        ));
        assert!(!is_page_wanted_with_freeze(
            range,
            1,
            2,
            Page { row: 5, col: 0 }
        ));
    }

    #[test]
    fn a_range_is_clipped_to_the_sheet() {
        let sheet = (130, 20);
        assert_eq!(
            pages(PageRange::covering(sheet, (120, 1000), (10, 1000))),
            [(1, 0), (1, 1), (2, 0), (2, 1)]
        );
        assert_eq!(
            pages(PageRange::covering(sheet, (0, u32::MAX), (0, u32::MAX))).len(),
            6
        );
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

    #[test]
    fn overlay_renders_spilled_numbers_text_and_errors_without_sheet_mutation() {
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("S1"), 3, 20);
        assert!(sheet.set_value_by_id(RowId(0), ColId(0), Value::Number(10.0)));
        let mut scratch = String::new();
        let bytes = encode_page_with_overlay(
            &sheet,
            Page { row: 0, col: 0 },
            1,
            &mut scratch,
            |r, c| match (r.0, c.0) {
                (0, 0) => ViewportCellOverlay {
                    value: None,
                    extra_flags: FLAG_FORMULA,
                },
                (0, 1) => ViewportCellOverlay {
                    value: Some(OverlayValue::Number(20.0)),
                    extra_flags: FLAG_SPILL,
                },
                (1, 0) => ViewportCellOverlay {
                    value: Some(OverlayValue::Text("spilled text")),
                    extra_flags: FLAG_SPILL,
                },
                (1, 1) => ViewportCellOverlay {
                    value: Some(OverlayValue::Error(CalcError::Div0)),
                    extra_flags: FLAG_SPILL,
                },
                _ => ViewportCellOverlay::default(),
            },
        );
        let decoded = decode(&bytes);
        assert_eq!(
            decoded.cells[0],
            (String::from("10"), 0, FLAG_NUMBER | FLAG_FORMULA)
        );
        assert_eq!(
            decoded.cells[1],
            (String::from("20"), 0, FLAG_NUMBER | FLAG_SPILL)
        );
        assert_eq!(decoded.cols, 16);
        assert_eq!(
            decoded.cells[decoded.cols],
            (String::from("spilled text"), 0, FLAG_SPILL)
        );
        assert_eq!(
            decoded.cells[decoded.cols + 1],
            (String::from("#DIV/0!"), 0, FLAG_ERROR | FLAG_SPILL)
        );
    }
}
