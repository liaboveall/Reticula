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
pub const MAX_LEFT_OVERFLOW_LOOKBACK: u32 = 64;

/// Page columns to the right of a changed cell whose overflow records can
/// depend on it: the cell may start, stop or cover text that runs into them.
pub const OVERFLOW_PAGE_COLS: u32 = MAX_LEFT_OVERFLOW_LOOKBACK.div_ceil(PAGE_COLS);

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

/// Most page rows one subscription may span, unless more than that are visible.
pub const MAX_SUBSCRIBED_PAGE_ROWS: u32 = 64;
/// Most page columns one subscription may span, unless more than that are visible.
pub const MAX_SUBSCRIBED_PAGE_COLS: u32 = 64;
/// Most pages one subscription may hold besides the pinned strips. The pages
/// with visible cells are held however many they are; the margin around them
/// gets what they leave of this.
pub const MAX_SUBSCRIBED_PAGES: u32 = 512;
/// Most pages along one axis that count as visible. No display shows this many
/// (a cell is at least one device pixel), so it only bounds nonsensical input.
pub const MAX_VISIBLE_PAGES_PER_AXIS: u32 = 256;

/// Coordinates of one `64 × 16` page in the sheet's page grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Page {
    /// Page row (`row * PAGE_ROWS`).
    pub row: u32,
    /// Page column (`col * PAGE_COLS`).
    pub col: u32,
}

/// Half-open rectangle of cells `[first_row..end_row) × [first_col..end_col)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellSpan {
    /// Position of the first row.
    pub first_row: u32,
    /// Position after the last row.
    pub end_row: u32,
    /// Position of the first column.
    pub first_col: u32,
    /// Position after the last column.
    pub end_col: u32,
}

impl CellSpan {
    /// Whether the span holds no cell.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.first_row >= self.end_row || self.first_col >= self.end_col
    }
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
    /// `[first_col .. first_col + col_count)`, clipped to `sheet_rows × sheet_cols`.
    ///
    /// Every page with cells of `visible` is in the result. Around them there
    /// are at most [`MAX_SUBSCRIBED_PAGE_ROWS`] × [`MAX_SUBSCRIBED_PAGE_COLS`]
    /// and [`MAX_SUBSCRIBED_PAGES`] pages; what is over is cut off the sides
    /// that are furthest from the visible cells. An empty `visible` keeps the
    /// start of the range.
    #[must_use]
    pub fn covering(
        (sheet_rows, sheet_cols): (u32, u32),
        (first_row, row_count): (u32, u32),
        (first_col, col_count): (u32, u32),
        visible: CellSpan,
    ) -> Self {
        let visible = if visible.is_empty() {
            CellSpan::default()
        } else {
            visible
        };
        let along_rows = AxisPages::new(
            sheet_rows,
            PAGE_ROWS,
            (first_row, first_row.saturating_add(row_count)),
            (visible.first_row, visible.end_row),
            MAX_SUBSCRIBED_PAGE_ROWS,
        );
        let along_cols = AxisPages::new(
            sheet_cols,
            PAGE_COLS,
            (first_col, first_col.saturating_add(col_count)),
            (visible.first_col, visible.end_col),
            MAX_SUBSCRIBED_PAGE_COLS,
        );
        let (Some(mut rows), Some(mut cols)) = (along_rows, along_cols) else {
            return Self::default();
        };
        let limit = rows
            .visible_len()
            .saturating_mul(cols.visible_len())
            .max(MAX_SUBSCRIBED_PAGES);
        while rows.len().saturating_mul(cols.len()) > limit {
            let furthest = rows.furthest().max(cols.furthest());
            if !cols.cut_at(furthest) && !rows.cut_at(furthest) {
                break;
            }
        }
        Self {
            first_row: rows.first,
            end_row: rows.end,
            first_col: cols.first,
            end_col: cols.end,
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
    let pinned_row_pages = frozen_rows.div_ceil(PAGE_ROWS);
    let pinned_col_pages = frozen_cols.div_ceil(PAGE_COLS);
    let in_top_strip = page.row < pinned_row_pages
        && subscribed.first_col <= page.col
        && page.col < subscribed.end_col;
    let in_left_strip = page.col < pinned_col_pages
        && subscribed.first_row <= page.row
        && page.row < subscribed.end_row;
    let in_corner = page.row < pinned_row_pages && page.col < pinned_col_pages;
    in_top_strip || in_left_strip || in_corner
}

/// Collects the pages wanted by `subscribed` together with the pinned freeze
/// strips, each once, in the order to send them: the pinned pages that show
/// something, so that frozen headers paint immediately, then the pages with
/// cells of `visible`, then the others in rings around those.
///
/// An empty `visible` says nothing about what is on screen: then all pinned
/// pages come first, and the others row by row.
#[must_use]
pub fn collect_wanted_pages_with_freeze(
    subscribed: PageRange,
    visible: CellSpan,
    frozen_rows: u32,
    frozen_cols: u32,
) -> Vec<Page> {
    if subscribed.is_empty() {
        return Vec::new();
    }
    let pinned_rows = frozen_rows.div_ceil(PAGE_ROWS);
    let pinned_cols = frozen_cols.div_ceil(PAGE_COLS);
    let rows = (0..pinned_rows).chain(subscribed.first_row.max(pinned_rows)..subscribed.end_row);
    let cols = (0..pinned_cols).chain(subscribed.first_col.max(pinned_cols)..subscribed.end_col);
    let mut out: Vec<Page> = rows
        .flat_map(|row| cols.clone().map(move |col| Page { row, col }))
        .collect();

    let seen = if visible.is_empty() {
        subscribed
    } else {
        PageRange {
            first_row: page_of(visible.first_row, PAGE_ROWS),
            end_row: visible.end_row.div_ceil(PAGE_ROWS),
            first_col: page_of(visible.first_col, PAGE_COLS),
            end_col: visible.end_col.div_ceil(PAGE_COLS),
        }
    };
    let rank = |page: Page| -> u32 {
        let below = distance(page.row, seen.first_row, seen.end_row);
        let beside = distance(page.col, seen.first_col, seen.end_col);
        // A pinned page does not move along its pinned axis, so what shows of
        // it depends on the other axis only.
        let pinned = match (page.row < pinned_rows, page.col < pinned_cols) {
            (true, true) => Some(0),
            (true, false) => Some(beside),
            (false, true) => Some(below),
            (false, false) => None,
        };
        match pinned {
            Some(0) => 0,
            _ => pinned
                .unwrap_or(u32::MAX)
                .min(below.max(beside))
                .saturating_add(1),
        }
    };
    out.sort_unstable_by_key(|&page| (rank(page), page.row, page.col));
    out
}

const fn page_of(position: u32, page: u32) -> u32 {
    match position.checked_div(page) {
        Some(index) => index,
        None => 0,
    }
}

/// How many pages `index` is away from `first..end`.
const fn distance(index: u32, first: u32, end: u32) -> u32 {
    if index < first {
        first.saturating_sub(index)
    } else {
        index.saturating_sub(end.saturating_sub(1))
    }
}

/// Pages along one axis: the wanted `first..end`, and the part of it that has
/// to stay because it is visible.
struct AxisPages {
    first: u32,
    end: u32,
    visible_first: u32,
    visible_end: u32,
}

impl AxisPages {
    /// `None` when nothing of `wanted` is on the axis.
    fn new(
        extent: u32,
        page: u32,
        wanted: (u32, u32),
        visible: (u32, u32),
        limit: u32,
    ) -> Option<Self> {
        let along = |(first, end): (u32, u32)| {
            let end = end.min(extent);
            (first < end).then(|| (page_of(first, page), end.div_ceil(page)))
        };
        let (first, end) = along(wanted)?;
        let (visible_first, visible_end) =
            along(visible).unwrap_or_else(|| (first, first.saturating_add(1)));
        let visible_end = visible_end.min(visible_first.saturating_add(MAX_VISIBLE_PAGES_PER_AXIS));
        let mut pages = Self {
            first: first.min(visible_first),
            end: end.max(visible_end),
            visible_first,
            visible_end,
        };
        pages.trim_to(limit);
        Some(pages)
    }

    const fn len(&self) -> u32 {
        self.end.saturating_sub(self.first)
    }

    const fn visible_len(&self) -> u32 {
        self.visible_end.saturating_sub(self.visible_first)
    }

    const fn before(&self) -> u32 {
        self.visible_first.saturating_sub(self.first)
    }

    const fn after(&self) -> u32 {
        self.end.saturating_sub(self.visible_end)
    }

    /// Pages of margin on the side that has more of it.
    fn furthest(&self) -> u32 {
        self.before().max(self.after())
    }

    /// Cuts the larger margin until there are `limit` pages, or none to cut.
    fn trim_to(&mut self, limit: u32) {
        let room = limit.saturating_sub(self.visible_len());
        if self.before().saturating_add(self.after()) <= room {
            return;
        }
        let half = room / 2;
        let rest = room.saturating_sub(half);
        let (before, after) = if self.before() <= half {
            (self.before(), room.saturating_sub(self.before()))
        } else if self.after() <= rest {
            (room.saturating_sub(self.after()), self.after())
        } else {
            (half, rest)
        };
        self.first = self.visible_first.saturating_sub(before);
        self.end = self.visible_end.saturating_add(after);
    }

    /// Cuts one page off a side with `distance` pages of margin, if there is one.
    const fn cut_at(&mut self, distance: u32) -> bool {
        if distance == 0 {
            return false;
        }
        if self.before() == distance {
            self.first = self.first.saturating_add(1);
            return true;
        }
        if self.after() == distance {
            self.end = self.end.saturating_sub(1);
            return true;
        }
        false
    }
}

/// Ensures resolved styles for all cells (and left-overflow origin cells) on
/// `page` are interned in `sheet.styles_mut()` prior to calling [`encode_page_with_formulas`].
pub fn prepare_page_styles(sheet: &mut Sheet, page: Page) {
    let total = sheet.rows();
    prepare_projected_page_styles(sheet, page, total, false, Some);
}

/// Projection-aware variant of [`prepare_page_styles`] mapping display rows
/// `0..visible_rows` to physical sheet row indices via `map_display_row`.
/// `spilled` tells whether cells the sheet has no value for may show one.
pub fn prepare_projected_page_styles<R>(
    sheet: &mut Sheet,
    page: Page,
    visible_rows: u32,
    spilled: bool,
    mut map_display_row: R,
) where
    R: FnMut(u32) -> Option<u32>,
{
    if sheet.col_styles().is_empty()
        && sheet.row_styles().is_empty()
        && sheet.cell_styles().is_empty()
    {
        return;
    }
    let first_row = page.row.saturating_mul(PAGE_ROWS);
    let first_col = page.col.saturating_mul(PAGE_COLS);
    let rows = visible_rows
        .min(sheet.rows())
        .saturating_sub(first_row)
        .min(PAGE_ROWS);
    let cols = sheet.cols().saturating_sub(first_col).min(PAGE_COLS);
    if rows == 0 || cols == 0 {
        return;
    }
    let lookback_start = first_col.saturating_sub(MAX_LEFT_OVERFLOW_LOOKBACK);
    let end_row = first_row.saturating_add(rows);
    let end_col = first_col.saturating_add(cols);

    let mut row_ids: SmallVec<[RowId; 64]> = SmallVec::new();
    for r in first_row..end_row {
        if let Some(phys) = map_display_row(r)
            && phys < sheet.rows()
        {
            row_ids.push(RowId(sheet.row_id(phys)));
        }
    }
    let col_ids: SmallVec<[ColId; 16]> = (first_col..end_col)
        .map(|c| ColId(sheet.col_id(c)))
        .collect();
    sheet.prepare_viewport_styles(&row_ids, &col_ids);

    // Of the cells to the left only the one text overflows from is drawn: the
    // first that shows anything, or one before it that a value is spilled into.
    let Some(&first_page_col) = col_ids.first() else {
        return;
    };
    let lookback: SmallVec<[ColId; 64]> = (lookback_start..first_col)
        .rev()
        .map(|c| ColId(sheet.col_id(c)))
        .filter(|&col| spilled || column_has_storage(sheet, col))
        .collect();
    let mut scratch = String::new();
    for &row in &row_ids {
        if shows_anything(sheet, row, first_page_col, &mut scratch) {
            continue;
        }
        for &col in &lookback {
            let shown = shows_anything(sheet, row, col, &mut scratch);
            if spilled || shown {
                sheet.prepare_viewport_styles(&[row], &[col]);
            }
            if shown {
                break;
            }
        }
    }
}

/// Whether a cell of the sheet itself has anything to draw as text.
fn shows_anything(sheet: &Sheet, row: RowId, col: ColId, scratch: &mut String) -> bool {
    let value = sheet.value_by_id(row, col);
    if value == Value::Empty {
        return false;
    }
    scratch.clear();
    sheet.format_value(value, row, col, scratch);
    !scratch.is_empty()
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
    encode_page_with_formulas(sheet, page, epoch, text, |_, _| false)
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
    encode(
        sheet,
        page,
        epoch,
        sheet.rows(),
        Some,
        text,
        |r, c| ViewportCellOverlay {
            value: None,
            extra_flags: if is_formula(r, c) { FLAG_FORMULA } else { 0 },
        },
        false,
    )
}

/// Writes what a cell shows. Returns its flags and whether it holds text.
fn format_overlay_or_sheet_value(
    sheet: &Sheet,
    row_id: RowId,
    col_id: ColId,
    override_val: Option<OverlayValue<'_>>,
    text: &mut String,
) -> (u16, bool) {
    match override_val {
        Some(OverlayValue::Empty) => (0, false),
        Some(OverlayValue::Number(n)) => {
            let v = Value::number(n);
            sheet.format_value(v, row_id, col_id, text);
            let f = if matches!(v, Value::Number(_)) {
                FLAG_NUMBER
            } else {
                FLAG_ERROR
            };
            (f, false)
        }
        Some(OverlayValue::Bool(b)) => {
            sheet.format_value(Value::Bool(b), row_id, col_id, text);
            (FLAG_BOOL, false)
        }
        Some(OverlayValue::Error(e)) => {
            text.push_str(e.as_str());
            (FLAG_ERROR, false)
        }
        Some(OverlayValue::Text(s)) => {
            text.push_str(s);
            (0, true)
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
            (flags, matches!(val, Value::Text(_)))
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
    overlay: F,
) -> Vec<u8>
where
    F: FnMut(RowId, ColId) -> ViewportCellOverlay<'a>,
{
    encode_projected_page_with_overlay(sheet, page, epoch, sheet.rows(), Some, text, overlay)
}

/// Projection-aware variant of [`encode_page_with_overlay`] that maps visible
/// display row indices `0..visible_rows` to physical sheet row indices via
/// `map_display_row` without altering the v2 binary viewport page format.
#[must_use]
pub fn encode_projected_page_with_overlay<'a, R, F>(
    sheet: &Sheet,
    page: Page,
    epoch: u32,
    visible_rows: u32,
    map_display_row: R,
    text: &mut String,
    overlay: F,
) -> Vec<u8>
where
    R: FnMut(u32) -> Option<u32>,
    F: FnMut(RowId, ColId) -> ViewportCellOverlay<'a>,
{
    encode(
        sheet,
        page,
        epoch,
        visible_rows,
        map_display_row,
        text,
        overlay,
        true,
    )
}

/// `overlay_has_values` tells whether `overlay` ever supplies a value. Without,
/// the cells to the left of the page are read from the sheet alone, which is
/// what makes looking far back cheap.
#[expect(
    clippy::too_many_arguments,
    reason = "the one encoder behind the public entry points"
)]
fn encode<'a, R, F>(
    sheet: &Sheet,
    page: Page,
    epoch: u32,
    visible_rows: u32,
    mut map_display_row: R,
    text: &mut String,
    mut overlay: F,
    overlay_has_values: bool,
) -> Vec<u8>
where
    R: FnMut(u32) -> Option<u32>,
    F: FnMut(RowId, ColId) -> ViewportCellOverlay<'a>,
{
    let first_row = page.row.saturating_mul(PAGE_ROWS);
    let first_col = page.col.saturating_mul(PAGE_COLS);
    let raw_rows = visible_rows
        .min(sheet.rows())
        .saturating_sub(first_row)
        .min(PAGE_ROWS);
    let mut cols = sheet.cols().saturating_sub(first_col).min(PAGE_COLS);

    let mut row_ids: SmallVec<[RowId; 64]> = SmallVec::new();
    if raw_rows > 0 && cols > 0 {
        let end_row = first_row.saturating_add(raw_rows);
        for d in first_row..end_row {
            if let Some(phys) = map_display_row(d)
                && phys < sheet.rows()
            {
                row_ids.push(RowId(sheet.row_id(phys)));
            }
        }
    }
    let rows = u32::try_from(row_ids.len()).unwrap_or(0);
    if rows == 0 || cols == 0 {
        row_ids.clear();
        cols = 0;
    }
    let end_col = first_col.saturating_add(cols);

    let mut col_ids: SmallVec<[ColId; 16]> = SmallVec::new();
    for col in first_col..end_col {
        col_ids.push(ColId(sheet.col_id(col)));
    }
    let mut writer = PageWriter::new(
        PageFrame {
            sheet: sheet.id().0,
            first_row,
            first_col,
            epoch,
        },
        row_ids.iter().map(|id| id.0),
        col_ids.iter().map(|id| id.0),
        text,
    );

    // A cell may hold a value that shows nothing, such as an empty string. Text
    // runs on over it like over any other cell with nothing to draw.
    let mut first_cell_blank: SmallVec<[bool; 64]> = SmallVec::new();
    for &row_id in &row_ids {
        for (index, &col_id) in col_ids.iter().enumerate() {
            let cell_overlay = overlay(row_id, col_id);
            let (flags, _) = format_overlay_or_sheet_value(
                sheet,
                row_id,
                col_id,
                cell_overlay.value,
                writer.text(),
            );
            let style_id = sheet.effective_style_id(row_id, col_id).0;
            let units = writer.cell(style_id, flags | cell_overlay.extra_flags);
            if index == 0 {
                first_cell_blank.push(units == 0);
            }
        }
    }

    // Left-overflow candidate scan (`first_col > 0`).
    if first_col > 0 {
        let min_scan_col = first_col.saturating_sub(MAX_LEFT_OVERFLOW_LOOKBACK);
        let mut lookback_col_ids: SmallVec<[(u32, ColId); 64]> = SmallVec::new();
        let mut c = first_col;
        while c > min_scan_col {
            c = c.saturating_sub(1);
            let id = ColId(sheet.col_id(c));
            if overlay_has_values || column_has_storage(sheet, id) {
                lookback_col_ids.push((c, id));
            }
        }

        for (r_idx, (&row_id, &blank)) in row_ids.iter().zip(&first_cell_blank).enumerate() {
            if !blank {
                continue;
            }
            for &(origin_col_pos, origin_col_id) in &lookback_col_ids {
                let ov_val = if overlay_has_values {
                    overlay(row_id, origin_col_id).value
                } else {
                    None
                };
                let is_empty = ov_val.map_or_else(
                    || sheet.value_by_id(row_id, origin_col_id) == Value::Empty,
                    OverlayValue::is_empty,
                );
                if is_empty {
                    continue;
                }
                let (_, is_text) = format_overlay_or_sheet_value(
                    sheet,
                    row_id,
                    origin_col_id,
                    ov_val,
                    writer.text(),
                );
                if writer.is_text_empty() {
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
                if is_left_text
                    && let (Ok(r_off), Ok(orig_c)) =
                        (u16::try_from(r_idx), u16::try_from(origin_col_pos))
                {
                    let style_id = sheet.effective_style_id(row_id, origin_col_id).0;
                    writer.overflow(r_off, orig_c, style_id);
                } else {
                    writer.drop_text();
                }
                break;
            }
        }
    }

    writer.finish()
}

/// Where a page lies and which subscription it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFrame {
    /// Identifier of the sheet, or of whatever else the cells belong to.
    pub sheet: u32,
    /// Position of the first row.
    pub first_row: u32,
    /// Position of the first column.
    pub first_col: u32,
    /// Subscription epoch.
    pub epoch: u32,
}

/// Writes one v2 page, whatever its cells come from: the header and the
/// identifiers first, then each cell in row-major order, then the overflow
/// records, then the text.
///
/// What a cell shows is appended to [`PageWriter::text`]; the call that
/// follows closes it.
#[derive(Debug)]
pub struct PageWriter<'t> {
    out: Vec<u8>,
    text: &'t mut String,
    /// Where the text of the next record starts in `text`.
    start: usize,
    /// The same in UTF-16 code units.
    position: u32,
    overflow_count: u16,
}

impl<'t> PageWriter<'t> {
    /// Starts a page of at most [`PAGE_ROWS`] by [`PAGE_COLS`] cells.
    ///
    /// `text` is scratch space: it is emptied, and left holding the page's text.
    pub fn new<R, C>(frame: PageFrame, row_ids: R, col_ids: C, text: &'t mut String) -> Self
    where
        R: ExactSizeIterator<Item = u64>,
        C: ExactSizeIterator<Item = u64>,
    {
        let (rows, cols) = (row_ids.len(), col_ids.len());
        let cells = rows.saturating_mul(cols);
        let fixed = HEADER_LEN
            .saturating_add(rows.saturating_add(cols).saturating_mul(ID_LEN))
            .saturating_add(cells.saturating_mul(CELL_LEN));
        let mut out = Vec::with_capacity(fixed.saturating_add(cells.saturating_mul(8)));

        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&frame.sheet.to_le_bytes());
        out.extend_from_slice(&frame.first_row.to_le_bytes());
        out.extend_from_slice(&frame.first_col.to_le_bytes());
        out.extend_from_slice(&u16::try_from(rows).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&u16::try_from(cols).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&0_u32.to_le_bytes()); // text_len, patched by `finish`
        out.extend_from_slice(&frame.epoch.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes()); // overflow_count, patched by `finish`
        out.resize(HEADER_LEN, 0);
        for id in row_ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
        for id in col_ids {
            out.extend_from_slice(&id.to_le_bytes());
        }

        text.clear();
        Self {
            out,
            text,
            start: 0,
            position: 0,
            overflow_count: 0,
        }
    }

    /// Where the text of the next record is appended.
    pub const fn text(&mut self) -> &mut String {
        self.text
    }

    /// Whether nothing was written for the next record.
    #[must_use]
    pub fn is_text_empty(&self) -> bool {
        self.text.len() == self.start
    }

    /// Takes back what was written for the next record.
    pub fn drop_text(&mut self) {
        self.text.truncate(self.start);
    }

    /// Closes the next cell. Returns the length of its text in UTF-16 code units.
    pub fn cell(&mut self, style: u16, flags: u16) -> u16 {
        let (units, truncated) = clip_to_record(self.text, self.start);
        let flags = if truncated {
            flags | FLAG_TRUNCATED
        } else {
            flags
        };
        self.out.extend_from_slice(&self.position.to_le_bytes());
        self.out.extend_from_slice(&units.to_le_bytes());
        self.out.extend_from_slice(&style.to_le_bytes());
        self.out.extend_from_slice(&flags.to_le_bytes());
        self.out.extend_from_slice(&0_u16.to_le_bytes());
        self.advance(units);
        units
    }

    /// Closes an overflow record, after all the cells.
    pub fn overflow(&mut self, row_offset: u16, origin_col: u16, style: u16) {
        let (units, _) = clip_to_record(self.text, self.start);
        self.out.extend_from_slice(&self.position.to_le_bytes());
        self.out.extend_from_slice(&units.to_le_bytes());
        self.out.extend_from_slice(&row_offset.to_le_bytes());
        self.out.extend_from_slice(&origin_col.to_le_bytes());
        self.out.extend_from_slice(&style.to_le_bytes());
        self.advance(units);
        self.overflow_count = self.overflow_count.saturating_add(1);
    }

    fn advance(&mut self, units: u16) {
        self.position = self.position.saturating_add(u32::from(units));
        self.start = self.text.len();
    }

    /// The finished page.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.drop_text();
        let text_len = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
        if let Some(field) = self.out.get_mut(TEXT_LEN_AT..TEXT_LEN_AT.saturating_add(4)) {
            field.copy_from_slice(&text_len.to_le_bytes());
        }
        if let Some(field) = self
            .out
            .get_mut(OVERFLOW_COUNT_AT..OVERFLOW_COUNT_AT.saturating_add(2))
        {
            field.copy_from_slice(&self.overflow_count.to_le_bytes());
        }
        self.out.extend_from_slice(self.text.as_bytes());
        self.out
    }
}

fn column_has_storage(sheet: &Sheet, col: ColId) -> bool {
    sheet
        .col_slots()
        .get(col.0)
        .and_then(|slot| sheet.raw_columns().get(slot as usize))
        .is_some_and(|column| !column.blocks().is_empty())
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
        let range = PageRange::covering(sheet, (5000, 64), (16, 16), CellSpan::default());
        assert_eq!(pages(range), [(78, 1), (79, 1)]);

        let wanted = collect_wanted_pages_with_freeze(range, span((5000, 5064), (16, 32)), 1, 2);
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
            pages(PageRange::covering(
                sheet,
                (120, 1000),
                (10, 1000),
                CellSpan::default()
            )),
            [(1, 0), (1, 1), (2, 0), (2, 1)]
        );
        let all = span((0, u32::MAX), (0, u32::MAX));
        for visible in [CellSpan::default(), all] {
            assert_eq!(
                pages(PageRange::covering(
                    sheet,
                    (0, u32::MAX),
                    (0, u32::MAX),
                    visible
                ))
                .len(),
                6
            );
        }
        for range in [
            PageRange::covering(sheet, (130, 10), (0, 10), all),
            PageRange::covering(sheet, (0, 10), (20, 10), all),
            PageRange::covering(sheet, (0, 0), (0, 10), all),
            PageRange::covering(sheet, (u32::MAX, u32::MAX), (0, 10), all),
            PageRange::covering((0, 0), (0, 10), (0, 10), all),
        ] {
            assert!(range.is_empty());
            assert_eq!(range, PageRange::default());
            assert_eq!(pages(range), []);
        }
    }

    fn span((first_row, end_row): (u32, u32), (first_col, end_col): (u32, u32)) -> CellSpan {
        CellSpan {
            first_row,
            end_row,
            first_col,
            end_col,
        }
    }

    /// `(first_row, end_row, first_col, end_col)` of the pages around `visible`
    /// with a margin of so many multiples of it on every side.
    fn around(
        sheet: (u32, u32),
        visible: CellSpan,
        (times_rows, times_cols): (u32, u32),
    ) -> (u32, u32, u32, u32) {
        let rows = (visible.end_row - visible.first_row) * times_rows;
        let cols = (visible.end_col - visible.first_col) * times_cols;
        let first_row = visible.first_row.saturating_sub(rows);
        let first_col = visible.first_col.saturating_sub(cols);
        let range = PageRange::covering(
            sheet,
            (first_row, visible.end_row + rows - first_row),
            (first_col, visible.end_col + cols - first_col),
            visible,
        );
        (
            range.first_row(),
            range.end_row(),
            range.first_col(),
            range.end_col(),
        )
    }

    #[test]
    fn a_subscription_is_bounded() {
        let any = (u32::MAX, u32::MAX);
        let range = PageRange::covering(any, (640, u32::MAX), (0, u32::MAX), CellSpan::default());
        let held = pages(range);
        assert!(held.len() as u32 <= MAX_SUBSCRIBED_PAGES);
        assert_eq!(held.first(), Some(&(10, 0)));

        // A single row of cells leaves the whole budget to one axis.
        let range = PageRange::covering(any, (0, 1), (0, u32::MAX), CellSpan::default());
        assert_eq!(pages(range).len() as u32, MAX_SUBSCRIBED_PAGE_COLS);

        // What claims to be visible is bounded too.
        let all = span((0, u32::MAX), (0, u32::MAX));
        let range = PageRange::covering(any, (0, u32::MAX), (0, u32::MAX), all);
        assert_eq!(range.end_row(), MAX_VISIBLE_PAGES_PER_AXIS);
        assert_eq!(range.end_col(), MAX_VISIBLE_PAGES_PER_AXIS);
    }

    #[test]
    fn visible_pages_are_held_and_the_margin_gets_what_they_leave() {
        // 3840 × 2160 at a quarter of the size: columns of 6 px, rows of 4 px.
        let sheet = (1_000_000, 16_384);
        let visible = span((100_000, 100_540), (1_000, 1_640));
        // Page rows 1562 to 1570 and page columns 62 to 102 are visible: 369
        // pages, which leave room for a row of pages on both sides.
        assert_eq!(around(sheet, visible, (0, 0)), (1562, 1571, 62, 103));
        assert_eq!(around(sheet, visible, (2, 1)), (1561, 1572, 61, 104));

        // More visible pages than the budget: all of them, and no margin.
        let visible = span((6_400, 6_400 + 64 * 20), (1_600, 1_600 + 16 * 40));
        assert_eq!(around(sheet, visible, (2, 1)), (100, 120, 100, 140));

        // More visible columns than the limit along one axis.
        let visible = span((64, 128), (0, 16 * 70));
        assert_eq!(around(sheet, visible, (2, 1)), (0, 4, 0, 70));
    }

    #[test]
    fn the_margin_is_cut_where_it_is_furthest_from_what_is_visible() {
        let sheet = (1_000_000, 16_384);
        // 40 visible page columns: the margin asks for 40 more on both sides,
        // and the limit leaves 24 for both, shared between left and right.
        let visible = span((64, 128), (16 * 100, 16 * 140));
        let (_, _, first_col, end_col) = around(sheet, visible, (0, 1));
        assert_eq!((first_col, end_col), (100 - 12, 140 + 12));
        // At the left edge there is nothing to hold there, so the right gets it.
        let visible = span((64, 128), (0, 16 * 40));
        assert_eq!(around(sheet, visible, (0, 1)), (1, 2, 0, 64));
        // And the other way round at the right edge.
        let visible = span((64, 128), (16_384 - 16 * 40, 16_384));
        assert_eq!(around(sheet, visible, (0, 1)), (1, 2, 960, 1024));
    }

    #[test]
    fn a_margin_within_the_limits_is_held_as_asked() {
        let sheet = (1_000_000, 16_384);
        for visible in [
            span((0, 45), (0, 17)),
            span((5_000, 5_045), (30, 47)),
            span((999_955, 1_000_000), (16_367, 16_384)),
        ] {
            let held = around(sheet, visible, (2, 1));
            let rows = (visible.end_row - visible.first_row) * 2;
            let cols = visible.end_col - visible.first_col;
            assert_eq!(
                held,
                (
                    visible.first_row.saturating_sub(rows) / PAGE_ROWS,
                    (visible.end_row + rows).min(sheet.0).div_ceil(PAGE_ROWS),
                    visible.first_col.saturating_sub(cols) / PAGE_COLS,
                    (visible.end_col + cols).min(sheet.1).div_ceil(PAGE_COLS),
                )
            );
        }
    }

    #[test]
    fn wanted_pages_come_once_pinned_then_visible_then_in_rings() {
        let sheet = (1_000_000, 16_384);
        let visible = span((640, 700), (160, 190));
        let range = PageRange::covering(sheet, (560, 240), (130, 100), visible);
        assert_eq!((range.first_row(), range.end_row()), (8, 13));
        assert_eq!((range.first_col(), range.end_col()), (8, 15));
        let wanted = collect_wanted_pages_with_freeze(range, visible, 3, 20);
        let coords: Vec<(u32, u32)> = wanted.iter().map(|p| (p.row, p.col)).collect();

        // The corner, the strip above the visible columns and the strip to the
        // left of the visible rows: what is drawn pinned comes first.
        assert_eq!(
            coords[..6],
            [(0, 0), (0, 1), (0, 10), (0, 11), (10, 0), (10, 1)]
        );
        // Then what is visible of the rest.
        assert_eq!(coords[6..8], [(10, 10), (10, 11)]);
        // Then the first ring around it, pinned pages included.
        let ring: Vec<(u32, u32)> = coords[8..24].to_vec();
        assert!(ring.contains(&(0, 9)) && ring.contains(&(0, 12)));
        assert!(ring.contains(&(9, 0)) && ring.contains(&(11, 1)));
        assert!(ring.contains(&(9, 9)) && ring.contains(&(11, 12)));
        assert!(!ring.contains(&(10, 8)) && !ring.contains(&(12, 10)));

        let mut unique = coords.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), coords.len());
        // (1 pinned + 5) page rows by (2 pinned + 7) page columns.
        assert_eq!(coords.len(), 6 * 9);
        assert!(
            wanted
                .iter()
                .all(|&page| is_page_wanted_with_freeze(range, 3, 20, page))
        );
    }

    #[test]
    fn strips_that_overlap_the_range_are_not_collected_twice() {
        let visible = span((0, 100), (0, 30));
        let range = PageRange::covering((1_000, 64), (0, 128), (0, 40), visible);
        let wanted = collect_wanted_pages_with_freeze(range, visible, 32, 32);
        let mut coords: Vec<(u32, u32)> = wanted.iter().map(|p| (p.row, p.col)).collect();
        coords.sort_unstable();
        assert_eq!(coords, [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)]);
    }

    #[test]
    fn overflow_is_found_four_page_columns_back() {
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("S1"), 3, 200);
        let text = sheet.add_string("A heading that runs on and on").unwrap();
        assert!(sheet.set_value_by_id(RowId(1), ColId(0), Value::Text(text)));
        assert!(sheet.set_value_by_id(RowId(2), ColId(70), Value::Text(text)));
        let mut scratch = String::new();
        let mut origins = |col: u32| -> Vec<(u16, u16)> {
            decode(&encode_page(&sheet, Page { row: 0, col }, 1, &mut scratch))
                .overflows
                .iter()
                .map(|overflow| (overflow.0, overflow.1))
                .collect()
        };
        for col in 1..=OVERFLOW_PAGE_COLS {
            assert_eq!(origins(col).first(), Some(&(1, 0)), "page column {col}");
        }
        assert_eq!(origins(OVERFLOW_PAGE_COLS + 1), [(2, 70)]);
        // Column 70 is in page column 4, so it reaches page column 8.
        assert_eq!(origins(8), [(2, 70)]);
        assert_eq!(origins(9), []);
    }

    #[test]
    fn text_runs_on_over_cells_that_show_nothing() {
        let italic = StylePatch {
            italic: Some(true),
            ..StylePatch::default()
        };
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("S1"), 4, 60);
        let text = sheet.add_string("A heading that runs on and on").unwrap();
        let nothing = sheet.add_string("").unwrap();
        for row in 0..4 {
            assert!(sheet.set_value_by_id(RowId(row), ColId(12), Value::Text(text)));
        }
        // Before the page, at its start, and both; in the last row something is in the way.
        assert!(sheet.set_value_by_id(RowId(0), ColId(14), Value::Text(nothing)));
        assert!(sheet.set_value_by_id(RowId(1), ColId(16), Value::Text(nothing)));
        assert!(sheet.set_value_by_id(RowId(2), ColId(15), Value::Text(nothing)));
        assert!(sheet.set_value_by_id(RowId(2), ColId(16), Value::Text(nothing)));
        assert!(sheet.set_value_by_id(RowId(3), ColId(14), Value::Text(nothing)));
        assert!(sheet.set_value_by_id(RowId(3), ColId(15), Value::Number(5.0)));
        sheet.set_col_style_patch(ColId(12), italic);
        sheet.set_row_style_patch(
            RowId(2),
            StylePatch {
                bold: Some(true),
                ..StylePatch::default()
            },
        );

        let page = Page { row: 0, col: 1 };
        let interned = sheet.styles().entries().len();
        prepare_page_styles(&mut sheet, page);
        assert_eq!(sheet.styles().entries().len(), interned + 1, "bold italic");
        let decoded = decode(&encode_page(&sheet, page, 1, &mut String::new()));
        let origins: Vec<(u16, u16)> = decoded
            .overflows
            .iter()
            .map(|overflow| (overflow.0, overflow.1))
            .collect();
        assert_eq!(origins, [(0, 12), (1, 12), (2, 12)]);
        assert!(
            decoded
                .overflows
                .iter()
                .all(|overflow| overflow.3 == "A heading that runs on and on")
        );
    }

    #[test]
    fn styles_of_cells_text_may_overflow_from_are_interned() {
        let bold = StylePatch {
            bold: Some(true),
            ..StylePatch::default()
        };
        let italic = StylePatch {
            italic: Some(true),
            ..StylePatch::default()
        };
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("S1"), 3, 200);
        let text = sheet.add_string("A heading that runs on and on").unwrap();
        assert!(sheet.set_value_by_id(RowId(1), ColId(0), Value::Text(text)));
        // Where the bold rows cross an italic column, the combination is only
        // interned on demand. One of those cells has text, and one nothing.
        sheet.set_col_style_patch(ColId(0), italic);
        sheet.set_row_style_patch(RowId(1), bold);
        sheet.set_row_style_patch(
            RowId(2),
            StylePatch {
                strikethrough: Some(true),
                ..bold
            },
        );
        let interned = sheet.styles().entries().len();

        let page = Page { row: 0, col: 4 };
        prepare_page_styles(&mut sheet, page);
        assert_eq!(sheet.styles().entries().len(), interned + 1);
        let decoded = decode(&encode_page(&sheet, page, 1, &mut String::new()));
        let styles: Vec<u16> = decoded
            .overflows
            .iter()
            .map(|overflow| overflow.2)
            .collect();
        assert_eq!(styles, [interned as u16]);

        // The empty one counts too when a value may be spilled into it.
        let rows = sheet.rows();
        prepare_projected_page_styles(&mut sheet, page, rows, true, Some);
        assert_eq!(sheet.styles().entries().len(), interned + 2);
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
