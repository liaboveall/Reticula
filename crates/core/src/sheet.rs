//! A worksheet: identity-addressed axes, slotted Copy-on-Write columns, row/col
//! dimensions, freeze panes, formatting and styles (`14-m1b.md` 5.1 & 5.4).
//!
//! Positions (`0..rows()`, `0..cols()`) are derived from `row_axis` and
//! `col_axis`. Cell values are stored against `(row_slot, col_slot)` while
//! formats, styles and pixel sizes are keyed by stable `RowId` / `ColId` so
//! inserting, deleting or moving rows/columns never moves cell data.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::axis::{
    Axis, AxisRun, DEFAULT_BASE_COLS, DEFAULT_BASE_ROWS, MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN,
};
use crate::format::{NumberFormat, write_number};
use crate::id::{ColId, IdRun, RowId, SheetId, is_base_id};
use crate::store::{Column, SlotMap};
use crate::strings::StringTable;
use crate::style::{CellStyle, StyleId, StylePatch, StyleTable};
use crate::value::Value;

/// Minimum explicit row height in CSS pixels.
pub const MIN_ROW_HEIGHT: u16 = 16;
/// Maximum explicit row height in CSS pixels.
pub const MAX_ROW_HEIGHT: u16 = 600;
/// Default row height in CSS pixels.
pub const DEFAULT_ROW_HEIGHT: u16 = 24;

/// Minimum explicit column width in CSS pixels.
pub const MIN_COL_WIDTH: u16 = 24;
/// Maximum explicit column width in CSS pixels.
pub const MAX_COL_WIDTH: u16 = 1_200;
/// Default column width in CSS pixels.
pub const DEFAULT_COL_WIDTH: u16 = 112;

/// Maximum frozen rows or frozen columns on a single sheet.
pub const MAX_FREEZE_COUNT: u32 = 32;

/// Maximum UTF-16 code units in a worksheet tab name (Excel parity).
pub const MAX_SHEET_NAME_UTF16: usize = 31;

type CellFormatMap = BTreeMap<(RowId, ColId), NumberFormat>;
type RowFormatMap = BTreeMap<RowId, NumberFormat>;
type RowStyleMap = BTreeMap<RowId, StylePatch>;
type ColStyleMap = BTreeMap<ColId, StylePatch>;
type CellStyleMap = BTreeMap<(RowId, ColId), StylePatch>;

/// Auxiliary layout, tombstone and style state of a [`Sheet`], grouped for
/// clean snapshot reconstruction.
#[derive(Clone, Debug, Default)]
pub struct SheetExtras {
    /// Deletion counter (`0` = live sheet; `> 0` = tombstoned per E20).
    pub deleted_count: u32,
    /// Number of frozen rows pinned at top (`0..=MAX_FREEZE_COUNT`).
    pub frozen_rows: u32,
    /// Number of frozen columns pinned at left (`0..=MAX_FREEZE_COUNT`).
    pub frozen_cols: u32,
    /// Explicit custom row heights in CSS pixels.
    pub row_heights: BTreeMap<RowId, u16>,
    /// Explicit custom column widths in CSS pixels.
    pub col_widths: BTreeMap<ColId, u16>,
    /// Explicit whole-row number formats.
    pub row_formats: RowFormatMap,
    /// Whole-column visual style patches.
    pub col_styles: ColStyleMap,
    /// Whole-row visual style patches.
    pub row_styles: RowStyleMap,
    /// Per-cell visual style patches.
    pub cell_styles: CellStyleMap,
    /// Deduplicated resolved style table (`StyleId(0)` = default).
    pub styles: StyleTable,
}

/// One worksheet of a workbook.
#[derive(Clone, Debug)]
pub struct Sheet {
    id: SheetId,
    name: String,
    deleted_count: u32,
    frozen_rows: u32,
    frozen_cols: u32,
    row_axis: Axis,
    col_axis: Axis,
    row_slots: SlotMap,
    col_slots: SlotMap,
    columns: Vec<Column>,
    col_generations: Vec<u64>,
    col_formats: Vec<NumberFormat>,
    row_formats: RowFormatMap,
    cell_formats: CellFormatMap,
    row_heights: BTreeMap<RowId, u16>,
    col_widths: BTreeMap<ColId, u16>,
    col_styles: ColStyleMap,
    row_styles: RowStyleMap,
    cell_styles: CellStyleMap,
    styles: StyleTable,
    strings: StringTable,
}

impl Default for Sheet {
    fn default() -> Self {
        Self::with_axes(SheetId(0), String::from("Sheet1"), 0, 0)
    }
}

impl Sheet {
    /// Creates an empty growable sheet with one column per `NumberFormat`
    /// (convenient for incremental row appending in tests).
    #[must_use]
    pub fn new(formats: &[NumberFormat]) -> Self {
        let base_cols = u32::try_from(formats.len()).unwrap_or(u32::MAX);
        Self {
            id: SheetId(0),
            name: String::from("Sheet1"),
            deleted_count: 0,
            frozen_rows: 0,
            frozen_cols: 0,
            row_axis: Axis::empty_with_px(MAX_ROW_AXIS_LEN, DEFAULT_ROW_HEIGHT),
            col_axis: Axis::with_base_px(base_cols, MAX_COL_AXIS_LEN, DEFAULT_COL_WIDTH),
            row_slots: SlotMap::with_base(MAX_ROW_AXIS_LEN),
            col_slots: SlotMap::with_base(base_cols.max(DEFAULT_BASE_COLS)),
            columns: formats.iter().map(|_| Column::new()).collect(),
            col_generations: vec![1; formats.len()],
            col_formats: formats.iter().map(|f| f.normalize()).collect(),
            row_formats: RowFormatMap::default(),
            cell_formats: CellFormatMap::default(),
            row_heights: BTreeMap::new(),
            col_widths: BTreeMap::new(),
            col_styles: ColStyleMap::default(),
            row_styles: RowStyleMap::default(),
            cell_styles: CellStyleMap::default(),
            styles: StyleTable::new(),
            strings: StringTable::new(),
        }
    }

    /// Creates a worksheet with pre-allocated base axes of `base_rows` by
    /// `base_cols` live entries.
    #[must_use]
    pub fn with_axes(id: SheetId, name: String, base_rows: u32, base_cols: u32) -> Self {
        Self {
            id,
            name,
            deleted_count: 0,
            frozen_rows: 0,
            frozen_cols: 0,
            row_axis: Axis::with_base_px(base_rows, MAX_ROW_AXIS_LEN, DEFAULT_ROW_HEIGHT),
            col_axis: Axis::with_base_px(base_cols, MAX_COL_AXIS_LEN, DEFAULT_COL_WIDTH),
            row_slots: SlotMap::with_base(base_rows.max(DEFAULT_BASE_ROWS)),
            col_slots: SlotMap::with_base(base_cols.max(DEFAULT_BASE_COLS)),
            columns: Vec::new(),
            col_generations: Vec::new(),
            col_formats: Vec::new(),
            row_formats: RowFormatMap::default(),
            cell_formats: CellFormatMap::default(),
            row_heights: BTreeMap::new(),
            col_widths: BTreeMap::new(),
            col_styles: ColStyleMap::default(),
            row_styles: RowStyleMap::default(),
            cell_styles: CellStyleMap::default(),
            styles: StyleTable::new(),
            strings: StringTable::new(),
        }
    }

    /// Creates a standard empty spreadsheet (`1,048,576` rows × `16,384` cols).
    #[must_use]
    pub fn standard(id: SheetId, name: String) -> Self {
        Self::with_axes(id, name, DEFAULT_BASE_ROWS, DEFAULT_BASE_COLS)
    }

    /// Creates a pre-sized sheet for `rows` base rows and `formats` columns.
    #[must_use]
    pub fn with_base_formats(rows: u32, formats: &[NumberFormat], strings: StringTable) -> Self {
        let base_cols = u32::try_from(formats.len()).unwrap_or(u32::MAX);
        Self {
            id: SheetId(0),
            name: String::from("Sheet1"),
            deleted_count: 0,
            frozen_rows: 0,
            frozen_cols: 0,
            row_axis: Axis::with_base_px(rows, MAX_ROW_AXIS_LEN, DEFAULT_ROW_HEIGHT),
            col_axis: Axis::with_base_px(base_cols, MAX_COL_AXIS_LEN, DEFAULT_COL_WIDTH),
            row_slots: SlotMap::with_base(rows.max(DEFAULT_BASE_ROWS)),
            col_slots: SlotMap::with_base(base_cols.max(DEFAULT_BASE_COLS)),
            columns: formats
                .iter()
                .map(|_| Column::with_slot_capacity(rows))
                .collect(),
            col_generations: vec![1; formats.len()],
            col_formats: formats.iter().map(|f| f.normalize()).collect(),
            row_formats: RowFormatMap::default(),
            cell_formats: CellFormatMap::default(),
            row_heights: BTreeMap::new(),
            col_widths: BTreeMap::new(),
            col_styles: ColStyleMap::default(),
            row_styles: RowStyleMap::default(),
            cell_styles: CellStyleMap::default(),
            styles: StyleTable::new(),
            strings,
        }
    }

    /// Extends a fresh base-run sheet up to `target_rows` (`<= MAX_ROW_AXIS_LEN`)
    /// during CSV import.
    pub fn extend_base_rows_to(&mut self, target_rows: u32) -> Option<u32> {
        if target_rows > MAX_ROW_AXIS_LEN {
            return None;
        }
        let cur = self.row_axis.total_len();
        if target_rows <= cur {
            return Some(cur);
        }
        let grow = target_rows.checked_sub(cur)?;
        let anchor = if cur == 0 {
            None
        } else {
            self.row_axis
                .last_run()
                .and_then(|r| r.ids.id_at(r.ids.len.saturating_sub(1)))
        };
        let run = IdRun::new(u64::from(cur), grow)?;
        self.row_axis.insert_after(anchor, run).ok()?;
        Some(target_rows)
    }

    /// Stable identifier of this worksheet.
    #[must_use]
    pub const fn id(&self) -> SheetId {
        self.id
    }

    /// Display name of this worksheet.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sets the display name of this worksheet.
    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    /// Tombstone deletion count (`0` means visible/live; `> 0` means deleted).
    #[must_use]
    pub const fn deleted_count(&self) -> u32 {
        self.deleted_count
    }

    /// Whether this worksheet is currently tombstoned (E20).
    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        self.deleted_count > 0
    }

    /// Increments sheet deletion count.
    pub fn increment_deleted(&mut self) -> bool {
        if self.deleted_count == u32::MAX {
            return false;
        }
        self.deleted_count = self.deleted_count.saturating_add(1);
        true
    }

    /// Decrements sheet deletion count (returns `false` if already `0`).
    pub fn decrement_deleted(&mut self) -> bool {
        if self.deleted_count == 0 {
            return false;
        }
        self.deleted_count = self.deleted_count.saturating_sub(1);
        true
    }

    /// Frozen row count at the top of the sheet.
    #[must_use]
    pub const fn frozen_rows(&self) -> u32 {
        self.frozen_rows
    }

    /// Frozen column count at the left of the sheet.
    #[must_use]
    pub const fn frozen_cols(&self) -> u32 {
        self.frozen_cols
    }

    /// Updates `(frozen_rows, frozen_cols)`.
    pub fn set_freeze(&mut self, frozen_rows: u32, frozen_cols: u32) {
        self.frozen_rows = frozen_rows;
        self.frozen_cols = frozen_cols;
    }

    /// Explicit custom row heights (`RowId -> px`).
    #[must_use]
    pub const fn row_heights(&self) -> &BTreeMap<RowId, u16> {
        &self.row_heights
    }

    /// Explicit custom column widths (`ColId -> px`).
    #[must_use]
    pub const fn col_widths(&self) -> &BTreeMap<ColId, u16> {
        &self.col_widths
    }

    /// Reads custom row height for `row` (`None` means [`DEFAULT_ROW_HEIGHT`]).
    #[must_use]
    pub fn row_height_by_id(&self, row: RowId) -> Option<u16> {
        self.row_heights.get(&row).copied()
    }

    /// Sets or clears custom row height (`Some(DEFAULT_ROW_HEIGHT)` normalises
    /// to `None` so default heights cost zero map entries).
    pub fn set_row_height_by_id(&mut self, row: RowId, height: Option<u16>) {
        match height {
            None | Some(DEFAULT_ROW_HEIGHT) => {
                self.row_heights.remove(&row);
                let _ = self.row_axis.set_entry_px(row.0, None);
            }
            Some(px) => {
                self.row_heights.insert(row, px);
                let _ = self.row_axis.set_entry_px(row.0, Some(px));
            }
        }
    }

    /// Reads custom column width for `col` (`None` means [`DEFAULT_COL_WIDTH`]).
    #[must_use]
    pub fn col_width_by_id(&self, col: ColId) -> Option<u16> {
        self.col_widths.get(&col).copied()
    }

    /// Sets or clears custom column width (`Some(DEFAULT_COL_WIDTH)` normalises
    /// to `None`).
    pub fn set_col_width_by_id(&mut self, col: ColId, width: Option<u16>) {
        match width {
            None | Some(DEFAULT_COL_WIDTH) => {
                self.col_widths.remove(&col);
                let _ = self.col_axis.set_entry_px(col.0, None);
            }
            Some(px) => {
                self.col_widths.insert(col, px);
                let _ = self.col_axis.set_entry_px(col.0, Some(px));
            }
        }
    }

    /// Total pixel height summed across all live rows (`O(1)`).
    #[must_use]
    pub const fn total_height_px(&self) -> u64 {
        self.row_axis.total_pixels()
    }

    /// Total pixel width summed across all live columns (`O(1)`).
    #[must_use]
    pub const fn total_width_px(&self) -> u64 {
        self.col_axis.total_pixels()
    }

    /// Pixel Y-offset of 0-based visible row `row_pos` (`O(log R)`).
    #[must_use]
    pub fn row_pixel_offset(&self, row_pos: u32) -> Option<u64> {
        self.row_axis.pixel_offset_of_pos(row_pos)
    }

    /// Pixel X-offset of 0-based visible column `col_pos` (`O(log C)`).
    #[must_use]
    pub fn col_pixel_offset(&self, col_pos: u32) -> Option<u64> {
        self.col_axis.pixel_offset_of_pos(col_pos)
    }

    /// Resolves Y pixel coordinate to `(row_pos, row_top_px, row_height_px)` (`O(log R)`).
    #[must_use]
    pub fn row_at_pixel(&self, y_px: u64) -> Option<(u32, u64, u16)> {
        self.row_axis.pos_at_pixel(y_px)
    }

    /// Resolves X pixel coordinate to `(col_pos, col_left_px, col_width_px)` (`O(log C)`).
    #[must_use]
    pub fn col_at_pixel(&self, x_px: u64) -> Option<(u32, u64, u16)> {
        self.col_axis.pos_at_pixel(x_px)
    }

    /// Structural generation counter of the row axis (`O(1)`).
    #[must_use]
    pub const fn row_axis_generation(&self) -> u64 {
        self.row_axis.generation()
    }

    /// Structural generation counter of the column axis (`O(1)`).
    #[must_use]
    pub const fn col_axis_generation(&self) -> u64 {
        self.col_axis.generation()
    }

    /// Per-column write generation counter (`1` if never modified).
    #[must_use]
    pub fn col_generation(&self, col: ColId) -> u64 {
        self.col_slots
            .get(col.0)
            .and_then(|slot| self.col_generations.get(slot as usize))
            .copied()
            .unwrap_or(1)
    }

    /// Bumps the write generation counter for `col` (used by bulk block writes).
    pub fn bump_col_generation(&mut self, col: ColId) {
        if let Some(slot) = self.col_slots.get(col.0) {
            let idx = slot as usize;
            if idx >= self.col_generations.len() {
                self.col_generations.resize(idx.saturating_add(1), 1);
            }
            if let Some(g) = self.col_generations.get_mut(idx) {
                *g = g.wrapping_add(1);
            }
        }
    }

    /// Sparse `(visible_row_pos, height_px)` list in ascending position order,
    /// sent to the main thread for `AxisMetrics`.
    #[must_use]
    pub fn visible_custom_row_heights(&self) -> Vec<(u32, u16)> {
        let mut list: Vec<(u32, u16)> = self
            .row_heights
            .iter()
            .filter_map(|(&row_id, &px)| self.row_axis.id_to_pos(row_id.0).map(|pos| (pos, px)))
            .collect();
        list.sort_unstable_by_key(|&(pos, _)| pos);
        list
    }

    /// Sparse `(visible_col_pos, width_px)` list in ascending position order.
    #[must_use]
    pub fn visible_custom_col_widths(&self) -> Vec<(u32, u16)> {
        let mut list: Vec<(u32, u16)> = self
            .col_widths
            .iter()
            .filter_map(|(&col_id, &px)| self.col_axis.id_to_pos(col_id.0).map(|pos| (pos, px)))
            .collect();
        list.sort_unstable_by_key(|&(pos, _)| pos);
        list
    }

    /// Row axis.
    #[must_use]
    pub const fn row_axis(&self) -> &Axis {
        &self.row_axis
    }

    /// Mutable row axis.
    pub const fn row_axis_mut(&mut self) -> &mut Axis {
        &mut self.row_axis
    }

    /// Column axis.
    #[must_use]
    pub const fn col_axis(&self) -> &Axis {
        &self.col_axis
    }

    /// Mutable column axis.
    pub const fn col_axis_mut(&mut self) -> &mut Axis {
        &mut self.col_axis
    }

    /// Row slot table.
    #[must_use]
    pub const fn row_slots(&self) -> &SlotMap {
        &self.row_slots
    }

    /// Column slot table.
    #[must_use]
    pub const fn col_slots(&self) -> &SlotMap {
        &self.col_slots
    }

    /// Column storage slice (indexed by column slot).
    #[must_use]
    pub fn raw_columns(&self) -> &[Column] {
        &self.columns
    }

    /// Default column number formats (indexed by column slot).
    #[must_use]
    pub fn col_formats(&self) -> &[NumberFormat] {
        &self.col_formats
    }

    /// Reads the explicit default number format of `col` (`General` if unset).
    #[must_use]
    pub fn col_format_by_id(&self, col: ColId) -> NumberFormat {
        self.col_slots
            .get(col.0)
            .and_then(|slot| self.col_formats.get(slot as usize))
            .copied()
            .unwrap_or_default()
    }

    /// Sets the default number format of a column by identity.
    pub fn set_col_format_by_id(&mut self, col: ColId, format: NumberFormat) -> bool {
        let format = format.normalize();
        let Some(col_slot) = self.col_slots.get_or_alloc(col.0) else {
            return false;
        };
        let idx = col_slot as usize;
        if idx >= self.col_formats.len() {
            if format == NumberFormat::General {
                return true;
            }
            self.col_formats
                .resize(idx.saturating_add(1), NumberFormat::General);
        }
        if let Some(slot) = self.col_formats.get_mut(idx) {
            *slot = format;
        }
        true
    }

    /// Whole-row number format overrides (`RowId -> NumberFormat`).
    #[must_use]
    pub const fn row_formats(&self) -> &RowFormatMap {
        &self.row_formats
    }

    /// Reads whole-row number format override for `row`.
    #[must_use]
    pub fn row_format_by_id(&self, row: RowId) -> Option<NumberFormat> {
        self.row_formats.get(&row).copied()
    }

    /// Sets or clears whole-row number format override.
    pub fn set_row_format_by_id(&mut self, row: RowId, format: Option<NumberFormat>) {
        match format {
            None => {
                self.row_formats.remove(&row);
            }
            Some(fmt) => {
                self.row_formats.insert(row, fmt.normalize());
            }
        }
    }

    /// Whole-column style overrides.
    #[must_use]
    pub const fn col_styles(&self) -> &ColStyleMap {
        &self.col_styles
    }

    /// Whole-row style overrides.
    #[must_use]
    pub const fn row_styles(&self) -> &RowStyleMap {
        &self.row_styles
    }

    /// Per-cell style overrides.
    #[must_use]
    pub const fn cell_styles(&self) -> &CellStyleMap {
        &self.cell_styles
    }

    /// Interned resolved style table of this sheet.
    #[must_use]
    pub const fn styles(&self) -> &StyleTable {
        &self.styles
    }

    /// Mutable style table.
    pub const fn styles_mut(&mut self) -> &mut StyleTable {
        &mut self.styles
    }

    /// Reads whole-column style patch for `col` (default empty if unset).
    #[must_use]
    pub fn col_style_patch(&self, col: ColId) -> StylePatch {
        self.col_styles.get(&col).copied().unwrap_or_default()
    }

    /// Replaces whole-column style patch for `col` (removing map entry if empty),
    /// and interns the standalone resolved style.
    pub fn set_col_style_patch(&mut self, col: ColId, patch: StylePatch) {
        if patch.is_empty() {
            self.col_styles.remove(&col);
        } else {
            self.col_styles.insert(col, patch);
            let _ = self.styles.intern(patch.apply_to(CellStyle::default()));
        }
    }

    /// Reads whole-row style patch for `row`.
    #[must_use]
    pub fn row_style_patch(&self, row: RowId) -> StylePatch {
        self.row_styles.get(&row).copied().unwrap_or_default()
    }

    /// Replaces whole-row style patch for `row`.
    pub fn set_row_style_patch(&mut self, row: RowId, patch: StylePatch) {
        if patch.is_empty() {
            self.row_styles.remove(&row);
        } else {
            self.row_styles.insert(row, patch);
            let _ = self.styles.intern(patch.apply_to(CellStyle::default()));
        }
    }

    /// Reads explicit per-cell style patch at `(row, col)`.
    #[must_use]
    pub fn cell_style_patch(&self, row: RowId, col: ColId) -> StylePatch {
        self.cell_styles
            .get(&(row, col))
            .copied()
            .unwrap_or_default()
    }

    /// Replaces explicit per-cell style patch at `(row, col)` and interns the
    /// cell's effective resolved [`CellStyle`].
    pub fn set_cell_style_patch(&mut self, row: RowId, col: ColId, patch: StylePatch) {
        if patch.is_empty() {
            self.cell_styles.remove(&(row, col));
        } else {
            self.cell_styles.insert((row, col), patch);
        }
        let resolved = self.effective_style_by_id(row, col);
        let _ = self.styles.intern(resolved);
    }

    /// Resolves `cell > row > col > default` visual style for `(row, col)`.
    #[must_use]
    pub fn effective_style_by_id(&self, row: RowId, col: ColId) -> CellStyle {
        let mut style = CellStyle::default();
        if let Some(&cp) = self.col_styles.get(&col) {
            style = cp.apply_to(style);
        }
        if let Some(&rp) = self.row_styles.get(&row) {
            style = rp.apply_to(style);
        }
        if let Some(&cell_p) = self.cell_styles.get(&(row, col)) {
            style = cell_p.apply_to(style);
        }
        style
    }

    /// Ensures every cell in `rows × cols` has its resolved [`CellStyle`]
    /// interned in [`Self::styles`] prior to immutable viewport page encoding.
    pub fn prepare_viewport_styles(&mut self, rows: &[RowId], cols: &[ColId]) {
        if self.col_styles.is_empty() && self.row_styles.is_empty() && self.cell_styles.is_empty() {
            return;
        }
        for &r in rows {
            let r_patch = self.row_styles.get(&r).copied();
            for &c in cols {
                let c_patch = self.col_styles.get(&c).copied();
                let cell_patch = self.cell_styles.get(&(r, c)).copied();
                if r_patch.is_none() && c_patch.is_none() && cell_patch.is_none() {
                    continue;
                }
                let resolved = self.effective_style_by_id(r, c);
                let _ = self.styles.intern(resolved);
            }
        }
    }

    /// Looks up the interned [`StyleId`] of `(row, col)` (`StyleId(0)` if
    /// unstyled).
    #[must_use]
    pub fn effective_style_id(&self, row: RowId, col: ColId) -> StyleId {
        if self.col_styles.is_empty() && self.row_styles.is_empty() && self.cell_styles.is_empty() {
            return StyleId(0);
        }
        let style = self.effective_style_by_id(row, col);
        self.styles.lookup(&style).unwrap_or(StyleId(0))
    }

    /// Number of visible rows on the row axis.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.row_axis.live_len()
    }

    /// Number of visible columns on the column axis.
    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.col_axis.live_len()
    }

    /// Interned strings of this sheet.
    #[must_use]
    pub const fn strings(&self) -> &StringTable {
        &self.strings
    }

    /// Mutable interned string pool.
    pub const fn strings_mut(&mut self) -> &mut StringTable {
        &mut self.strings
    }

    /// Interns `text` and returns its string index.
    pub fn add_string(&mut self, text: &str) -> Option<u32> {
        self.strings.intern(text)
    }

    /// Approximate heap bytes owned by this sheet.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let cols_mem = self
            .columns
            .iter()
            .fold(0usize, |acc, col| acc.saturating_add(col.estimated_bytes()));
        let cell_fmt_mem = self
            .cell_formats
            .len()
            .saturating_mul(size_of::<((RowId, ColId), NumberFormat)>().saturating_mul(2));
        let style_maps_mem = self
            .cell_styles
            .len()
            .saturating_add(self.row_styles.len())
            .saturating_add(self.col_styles.len())
            .saturating_mul(size_of::<((RowId, ColId), StylePatch)>().saturating_mul(2));
        let size_maps_mem = self
            .row_heights
            .len()
            .saturating_add(self.col_widths.len())
            .saturating_mul(24);
        self.row_axis
            .estimated_bytes()
            .saturating_add(self.col_axis.estimated_bytes())
            .saturating_add(self.row_slots.estimated_bytes())
            .saturating_add(self.col_slots.estimated_bytes())
            .saturating_add(cols_mem)
            .saturating_add(cell_fmt_mem)
            .saturating_add(style_maps_mem)
            .saturating_add(size_maps_mem)
            .saturating_add(self.styles.estimated_bytes())
            .saturating_add(self.strings.estimated_bytes())
    }

    /// Appends one base row at the end of a freshly constructed growable sheet.
    pub fn push_row(&mut self, values: &[Value]) -> bool {
        let next_id = u64::from(self.row_axis.total_len());
        let Some(new_total) = self.row_axis.total_len().checked_add(1) else {
            return false;
        };
        if new_total > self.row_axis.max_entries() {
            return false;
        }
        let anchor = if next_id == 0 {
            None
        } else {
            self.row_axis
                .last_run()
                .and_then(|run| run.ids.id_at(run.ids.len.saturating_sub(1)))
        };
        let Some(id_run) = IdRun::new(next_id, 1) else {
            return false;
        };
        if self.row_axis.insert_after(anchor, id_run).is_err() {
            return false;
        }
        let Some(row_slot) = self.row_slots.get_or_alloc(next_id) else {
            return false;
        };
        for (index, column) in self.columns.iter_mut().enumerate() {
            let val = values.get(index).copied().unwrap_or(Value::Empty);
            column.set(row_slot, val);
            if val != Value::Empty {
                if index >= self.col_generations.len() {
                    self.col_generations.resize(index.saturating_add(1), 1);
                }
                if let Some(g) = self.col_generations.get_mut(index) {
                    *g = g.wrapping_add(1);
                }
            }
        }
        true
    }

    /// Fast direct write to a base `(row_slot, col_slot)` during bulk sample
    /// generation when `with_base_formats` has already initialised the axes.
    pub fn set_base_slot(&mut self, row_slot: u32, col_slot: usize, value: Value) {
        if let Some(column) = self.columns.get_mut(col_slot) {
            column.set(row_slot, value);
            if col_slot >= self.col_generations.len() {
                self.col_generations.resize(col_slot.saturating_add(1), 1);
            }
            if let Some(g) = self.col_generations.get_mut(col_slot) {
                *g = g.wrapping_add(1);
            }
        }
    }

    /// Reads the value of `(row, col)` by stable identity.
    #[must_use]
    pub fn value_by_id(&self, row: RowId, col: ColId) -> Value {
        let (Some(row_slot), Some(col_slot)) =
            (self.row_slots.get(row.0), self.col_slots.get(col.0))
        else {
            return Value::Empty;
        };
        self.columns
            .get(col_slot as usize)
            .map_or(Value::Empty, |column| column.get(row_slot))
    }

    /// Writes `value` into `(row, col)` by stable identity, allocating storage
    /// slots on first write.
    pub fn set_value_by_id(&mut self, row: RowId, col: ColId, value: Value) -> bool {
        let (Some(row_slot), Some(col_slot)) = (
            self.row_slots.get_or_alloc(row.0),
            self.col_slots.get_or_alloc(col.0),
        ) else {
            return false;
        };
        let col_idx = col_slot as usize;
        if value == Value::Empty && col_idx >= self.columns.len() {
            return true;
        }
        if col_idx >= self.columns.len() {
            self.columns
                .resize_with(col_idx.saturating_add(1), Column::new);
            self.col_generations.resize(col_idx.saturating_add(1), 1);
        } else if col_idx >= self.col_generations.len() {
            self.col_generations.resize(col_idx.saturating_add(1), 1);
        }
        if let Some(g) = self.col_generations.get_mut(col_idx) {
            *g = g.wrapping_add(1);
        }
        self.columns
            .get_mut(col_idx)
            .is_some_and(|column| column.set(row_slot, value))
    }

    /// Reads contiguous packed 64-bit cell words for `[first_row .. first_row + out.len())`
    /// in column `col` (`O(blocks)` when rows sit in base identity slots).
    pub fn read_row_id_span_packed(&self, first_row: RowId, col: ColId, out: &mut [u64]) {
        if out.is_empty() {
            return;
        }
        let len_u64 = u64::try_from(out.len()).unwrap_or(u64::MAX);
        let Some(col_slot) = self.col_slots.get(col.0) else {
            out.fill(Value::EMPTY_BITS);
            return;
        };
        let Some(column) = self.columns.get(col_slot as usize) else {
            out.fill(Value::EMPTY_BITS);
            return;
        };
        if is_base_id(first_row.0)
            && let Some(end_id) = first_row.0.checked_add(len_u64)
            && end_id <= u64::from(self.row_slots.base_len())
            && let Ok(start_slot) = u32::try_from(first_row.0)
        {
            column.read_slot_span_packed(start_slot, out);
            return;
        }
        for (i, dst) in out.iter_mut().enumerate() {
            let off = u64::try_from(i).unwrap_or(u64::MAX);
            let row = RowId(first_row.0.saturating_add(off));
            *dst = self
                .row_slots
                .get(row.0)
                .map_or(Value::EMPTY_BITS, |rs| column.get_packed(rs));
        }
    }

    /// Writes contiguous packed 64-bit cell words from `src` into `[first_row .. first_row + src.len())`
    /// in column `col`, bumping `col_generation(col)` whenever any cell changed.
    pub fn write_row_id_span_packed(
        &mut self,
        first_row: RowId,
        col: ColId,
        src: &[u64],
        mut on_changed: impl FnMut(u32),
    ) -> u32 {
        if src.is_empty() {
            return 0;
        }
        let len_u64 = u64::try_from(src.len()).unwrap_or(u64::MAX);
        if is_base_id(first_row.0)
            && let Some(end_id) = first_row.0.checked_add(len_u64)
            && end_id <= u64::from(self.row_slots.base_len())
            && let Ok(start_slot) = u32::try_from(first_row.0)
        {
            if self.col_slots.get(col.0).is_none() && src.iter().all(|&w| w == Value::EMPTY_BITS) {
                return 0;
            }
            let Some(col_slot) = self.col_slots.get_or_alloc(col.0) else {
                return 0;
            };
            let col_idx = col_slot as usize;
            if col_idx >= self.columns.len() {
                self.columns
                    .resize_with(col_idx.saturating_add(1), Column::new);
            }
            if col_idx >= self.col_generations.len() {
                self.col_generations.resize(col_idx.saturating_add(1), 1);
            }
            let changed = self.columns.get_mut(col_idx).map_or(0, |column| {
                column.write_slot_span_packed(start_slot, src, &mut on_changed)
            });
            if changed > 0
                && let Some(g) = self.col_generations.get_mut(col_idx)
            {
                *g = g.wrapping_add(1);
            }
            return changed;
        }
        let mut changed = 0_u32;
        for (i, &w) in src.iter().enumerate() {
            let off = u64::try_from(i).unwrap_or(u64::MAX);
            let row = RowId(first_row.0.saturating_add(off));
            let old_w = self.value_by_id(row, col).pack();
            if old_w != w {
                let _ = self.set_value_by_id(row, col, Value::unpack(w));
                changed = changed.saturating_add(1);
                if let Ok(rel) = u32::try_from(i) {
                    on_changed(rel);
                }
            }
        }
        changed
    }

    /// Explicit per-cell number format at `(row, col)`, if any.
    #[must_use]
    pub fn cell_format_by_id(&self, row: RowId, col: ColId) -> Option<NumberFormat> {
        self.cell_formats.get(&(row, col)).copied()
    }

    /// Sets or clears the explicit per-cell number format at `(row, col)`.
    pub fn set_cell_format_by_id(
        &mut self,
        row: RowId,
        col: ColId,
        format: Option<NumberFormat>,
    ) -> bool {
        match format {
            None => {
                self.cell_formats.remove(&(row, col));
                true
            }
            Some(fmt) => {
                self.cell_formats.insert((row, col), fmt.normalize());
                true
            }
        }
    }

    /// Effective number format at `(row, col)` (`cell > row > col > General`).
    #[must_use]
    pub fn effective_format_by_id(&self, row: RowId, col: ColId) -> NumberFormat {
        if let Some(fmt) = self.cell_format_by_id(row, col) {
            return fmt;
        }
        if let Some(fmt) = self.row_format_by_id(row) {
            return fmt;
        }
        self.col_format_by_id(col)
    }

    /// Reads the value at visible `(row_pos, col_pos)`; [`Value::Empty`] outside
    /// the visible sheet.
    #[must_use]
    pub fn value(&self, row_pos: u32, col_pos: u32) -> Value {
        let (Some(row_id), Some(col_id)) = (
            self.row_axis.pos_to_id(row_pos),
            self.col_axis.pos_to_id(col_pos),
        ) else {
            return Value::Empty;
        };
        self.value_by_id(RowId(row_id), ColId(col_id))
    }

    /// Column storage for the column currently at visible position `col_pos`.
    #[must_use]
    pub fn column(&self, col_pos: u32) -> Option<&Column> {
        let col_id = self.col_axis.pos_to_id(col_pos)?;
        let slot = self.col_slots.get(col_id)?;
        self.columns.get(slot as usize)
    }

    /// Appends the formatted display text of `(row, col)` (by identity) to
    /// `out` and returns the cell's value.
    pub fn write_text_by_id(&self, row: RowId, col: ColId, out: &mut String) -> Value {
        let value = self.value_by_id(row, col);
        self.format_value(value, row, col, out);
        value
    }

    /// Appends the formatted display text of visible `(row_pos, col_pos)` to
    /// `out` and returns the cell's value.
    pub fn write_text(&self, row_pos: u32, col_pos: u32, out: &mut String) -> Value {
        let (Some(row_id), Some(col_id)) = (
            self.row_axis.pos_to_id(row_pos),
            self.col_axis.pos_to_id(col_pos),
        ) else {
            return Value::Empty;
        };
        self.write_text_by_id(RowId(row_id), ColId(col_id), out)
    }

    /// Formats an arbitrary `value` using `(row, col)`'s effective format.
    pub fn format_value(&self, value: Value, row: RowId, col: ColId, out: &mut String) {
        match value {
            Value::Empty => {}
            Value::Number(number) => {
                let format = self.effective_format_by_id(row, col);
                write_number(number, format, out);
            }
            Value::Text(id) => out.push_str(self.strings.get(id)),
            Value::Bool(true) => out.push_str("TRUE"),
            Value::Bool(false) => out.push_str("FALSE"),
            Value::Error(err) => out.push_str(err.as_str()),
            Value::Array(_) => out.push_str("#SPILL!"),
        }
    }

    /// Stable row identifier at 0-based visible position `row_pos` (`u64::MAX`
    /// if `row_pos` is out of bounds).
    #[must_use]
    pub fn row_id(&self, row_pos: u32) -> u64 {
        self.row_axis.pos_to_id(row_pos).unwrap_or(u64::MAX)
    }

    /// Stable column identifier at 0-based visible position `col_pos`
    /// (`u64::MAX` if `col_pos` is out of bounds).
    #[must_use]
    pub fn col_id(&self, col_pos: u32) -> u64 {
        self.col_axis.pos_to_id(col_pos).unwrap_or(u64::MAX)
    }

    /// Computes `(max_visible_row, max_visible_col)` bounding all non-empty
    /// visible cells on this sheet, returning `None` if every visible cell is
    /// empty (used by `Ctrl+End` and CSV export).
    #[must_use]
    pub fn used_extent(&self) -> Option<(u32, u32)> {
        let mut max_r: Option<u32> = None;
        let mut max_c: Option<u32> = None;
        for (col_slot_idx, column) in self.columns.iter().enumerate() {
            if column.is_empty() {
                continue;
            }
            let Ok(col_slot) = u32::try_from(col_slot_idx) else {
                continue;
            };
            let Some(col_id) = self.col_slots.slot_to_id(col_slot) else {
                continue;
            };
            let Some(col_pos) = self.col_axis.id_to_pos(col_id) else {
                continue;
            };
            let mut col_has_live = false;
            column.for_each_non_empty(|row_slot, _| {
                if let Some(row_id) = self.row_slots.slot_to_id(row_slot)
                    && let Some(row_pos) = self.row_axis.id_to_pos(row_id)
                {
                    col_has_live = true;
                    max_r = Some(max_r.map_or(row_pos, |cur| cur.max(row_pos)));
                }
            });
            if col_has_live {
                max_c = Some(max_c.map_or(col_pos, |cur| cur.max(col_pos)));
            }
        }
        match (max_r, max_c) {
            (Some(r), Some(c)) => Some((r, c)),
            _ => None,
        }
    }

    /// Visits every non-empty live cell stored in `self`, yielding
    /// `(row_pos, col_pos, row_id, col_id, value)` in sparse storage order.
    pub fn for_each_non_empty_cell(&self, mut visit: impl FnMut(u32, u32, RowId, ColId, Value)) {
        for (col_slot_idx, column) in self.columns.iter().enumerate() {
            if column.is_empty() {
                continue;
            }
            let Ok(col_slot) = u32::try_from(col_slot_idx) else {
                continue;
            };
            let Some(col_id) = self.col_slots.slot_to_id(col_slot) else {
                continue;
            };
            let Some(col_pos) = self.col_axis.id_to_pos(col_id) else {
                continue;
            };
            column.for_each_non_empty(|row_slot, val| {
                if let Some(row_id) = self.row_slots.slot_to_id(row_slot)
                    && let Some(row_pos) = self.row_axis.id_to_pos(row_id)
                {
                    visit(row_pos, col_pos, RowId(row_id), ColId(col_id), val);
                }
            });
        }
    }

    /// Computes Excel-compatible `Ctrl+Arrow` jump from `(row_pos, col_pos)`.
    /// `(dr, dc)` is one of `(-1, 0)`, `(1, 0)`, `(0, -1)`, `(0, 1)`.
    #[must_use]
    pub fn jump_edge(&self, row_pos: u32, col_pos: u32, dr: i8, dc: i8) -> (u32, u32) {
        if self.rows() == 0 || self.cols() == 0 {
            return (0, 0);
        }
        let r = row_pos.min(self.rows().saturating_sub(1));
        let c = col_pos.min(self.cols().saturating_sub(1));
        if dr != 0 {
            let forward = dr > 0;
            let new_r = match self.column(c) {
                Some(col) => col.jump_vertical(&self.row_axis, &self.row_slots, r, forward),
                None => {
                    if forward {
                        self.rows().saturating_sub(1)
                    } else {
                        0
                    }
                }
            };
            return (new_r, c);
        }
        if dc != 0 {
            let forward = dc > 0;
            let last_c = self.cols().saturating_sub(1);
            if (forward && c >= last_c) || (!forward && c == 0) {
                return (r, c);
            }
            let Some(row_id) = self.row_axis.pos_to_id(r) else {
                return (r, c);
            };
            let is_filled = |cp: u32| -> bool {
                self.value_by_id(RowId(row_id), ColId(self.col_id(cp))) != Value::Empty
            };
            let next_c = if forward {
                c.saturating_add(1)
            } else {
                c.saturating_sub(1)
            };
            if is_filled(c) && is_filled(next_c) {
                if forward {
                    let mut cur = next_c;
                    while cur < last_c && is_filled(cur.saturating_add(1)) {
                        cur = cur.saturating_add(1);
                    }
                    return (r, cur);
                }
                let mut cur = next_c;
                while cur > 0 && is_filled(cur.saturating_sub(1)) {
                    cur = cur.saturating_sub(1);
                }
                return (r, cur);
            }
            if forward {
                let mut cur = next_c;
                while cur <= last_c {
                    if is_filled(cur) {
                        return (r, cur);
                    }
                    if cur == last_c {
                        break;
                    }
                    cur = cur.saturating_add(1);
                }
                return (r, last_c);
            }
            let mut cur = next_c;
            loop {
                if is_filled(cur) {
                    return (r, cur);
                }
                if cur == 0 {
                    return (r, 0);
                }
                cur = cur.saturating_sub(1);
            }
        }
        (r, c)
    }

    /// Compacts [`Self::strings`] by removing unreferenced entries and
    /// remapping all [`Value::Text`] indices in deterministic slot order
    /// (`14-m1b.md` 5.5).
    pub fn compact_strings(&mut self) {
        let total = self.strings.len();
        if total == 0 {
            return;
        }
        let mut used = vec![false; total];
        let mut used_count = 0usize;
        for col in &self.columns {
            col.for_each_non_empty(|_, val| {
                if let Value::Text(id) = val
                    && let Some(flag) = used.get_mut(id as usize)
                    && !*flag
                {
                    *flag = true;
                    used_count = used_count.saturating_add(1);
                }
            });
        }
        if used_count == total {
            return;
        }
        let mut new_table = StringTable::with_capacity(used_count, self.strings.raw_bytes().len());
        let mut remap = vec![0_u32; total];
        for (old_idx, &is_used) in used.iter().enumerate() {
            if is_used
                && let Ok(old_id) = u32::try_from(old_idx)
                && let Some(new_id) = new_table.intern(self.strings.get(old_id))
                && let Some(slot) = remap.get_mut(old_idx)
            {
                *slot = new_id;
            }
        }
        for col in &mut self.columns {
            col.remap_strings(&remap);
        }
        self.strings = new_table;
    }

    /// Purges tombstoned rows and columns on this sheet, clearing stored cell
    /// values, formats, styles and custom dimensions on purged identities.
    pub fn purge_axis_tombstones(&mut self) -> (Vec<RowId>, Vec<ColId>) {
        let purged_rows: Vec<RowId> = self
            .row_axis
            .purge_tombstones()
            .into_iter()
            .map(RowId)
            .collect();
        let purged_cols: Vec<ColId> = self
            .col_axis
            .purge_tombstones()
            .into_iter()
            .map(ColId)
            .collect();

        for &r in &purged_rows {
            self.row_heights.remove(&r);
            self.row_formats.remove(&r);
            self.row_styles.remove(&r);
            if let Some(r_slot) = self.row_slots.get(r.0) {
                for col in &mut self.columns {
                    col.clear_slot(r_slot);
                }
            }
        }
        for &c in &purged_cols {
            self.col_widths.remove(&c);
            self.col_styles.remove(&c);
            if let Some(c_slot) = self.col_slots.get(c.0) {
                if let Some(col) = self.columns.get_mut(c_slot as usize) {
                    *col = Column::new();
                }
                if let Some(g) = self.col_generations.get_mut(c_slot as usize) {
                    *g = g.wrapping_add(1);
                }
                if let Some(fmt_slot) = self.col_formats.get_mut(c_slot as usize) {
                    *fmt_slot = NumberFormat::General;
                }
            }
        }
        if !purged_rows.is_empty() || !purged_cols.is_empty() {
            self.cell_formats.retain(|&(r, c), _| {
                self.row_axis.contains_id(r.0) && self.col_axis.contains_id(c.0)
            });
            self.cell_styles.retain(|&(r, c), _| {
                self.row_axis.contains_id(r.0) && self.col_axis.contains_id(c.0)
            });
        }
        (purged_rows, purged_cols)
    }

    /// Restores components decoded from an `RTCLSNAP` v0 snapshot.
    #[expect(
        clippy::too_many_arguments,
        reason = "snapshot reconstruction passes each persisted section"
    )]
    pub fn from_parts(
        id: SheetId,
        name: String,
        row_runs: Vec<AxisRun>,
        col_runs: Vec<AxisRun>,
        row_slots: SlotMap,
        col_slots: SlotMap,
        columns: Vec<Column>,
        col_formats: Vec<NumberFormat>,
        cell_formats: CellFormatMap,
        strings: StringTable,
    ) -> Option<Self> {
        Self::from_parts_with_extras(
            id,
            name,
            row_runs,
            col_runs,
            row_slots,
            col_slots,
            columns,
            col_formats,
            cell_formats,
            strings,
            SheetExtras::default(),
        )
    }

    /// Restores components decoded from an `RTCLSNAP` v1 snapshot, validating
    /// layout, style and format bounds.
    #[expect(
        clippy::too_many_arguments,
        reason = "snapshot reconstruction passes each persisted section"
    )]
    pub fn from_parts_with_extras(
        id: SheetId,
        name: String,
        row_runs: Vec<AxisRun>,
        col_runs: Vec<AxisRun>,
        row_slots: SlotMap,
        col_slots: SlotMap,
        columns: Vec<Column>,
        col_formats: Vec<NumberFormat>,
        cell_formats: CellFormatMap,
        strings: StringTable,
        extras: SheetExtras,
    ) -> Option<Self> {
        if extras.frozen_rows > MAX_FREEZE_COUNT || extras.frozen_cols > MAX_FREEZE_COUNT {
            return None;
        }
        if !col_formats.iter().all(|f| f.is_valid())
            || !cell_formats.values().all(|f| f.is_valid())
            || !extras.row_formats.values().all(|f| f.is_valid())
        {
            return None;
        }
        if !extras
            .row_heights
            .values()
            .all(|&h| (MIN_ROW_HEIGHT..=MAX_ROW_HEIGHT).contains(&h))
            || !extras
                .col_widths
                .values()
                .all(|&w| (MIN_COL_WIDTH..=MAX_COL_WIDTH).contains(&w))
        {
            return None;
        }
        if !extras.col_styles.values().all(StylePatch::is_valid)
            || !extras.row_styles.values().all(StylePatch::is_valid)
            || !extras.cell_styles.values().all(StylePatch::is_valid)
        {
            return None;
        }
        let mut row_axis =
            Axis::from_runs_with_px(row_runs, MAX_ROW_AXIS_LEN, DEFAULT_ROW_HEIGHT).ok()?;
        let mut col_axis =
            Axis::from_runs_with_px(col_runs, MAX_COL_AXIS_LEN, DEFAULT_COL_WIDTH).ok()?;
        for (&r, &px) in &extras.row_heights {
            row_axis.set_entry_px(r.0, Some(px)).ok()?;
        }
        for &r in extras.row_formats.keys().chain(extras.row_styles.keys()) {
            if !row_axis.contains_id(r.0) {
                return None;
            }
        }
        for (&c, &px) in &extras.col_widths {
            col_axis.set_entry_px(c.0, Some(px)).ok()?;
        }
        for &c in extras.col_styles.keys() {
            if !col_axis.contains_id(c.0) {
                return None;
            }
        }
        for &(r, c) in extras.cell_styles.keys() {
            if !row_axis.contains_id(r.0) || !col_axis.contains_id(c.0) {
                return None;
            }
        }

        let col_gen_len = columns.len();
        Some(Self {
            id,
            name,
            deleted_count: extras.deleted_count,
            frozen_rows: extras.frozen_rows,
            frozen_cols: extras.frozen_cols,
            row_axis,
            col_axis,
            row_slots,
            col_slots,
            columns,
            col_generations: vec![1; col_gen_len],
            col_formats: col_formats
                .into_iter()
                .map(NumberFormat::normalize)
                .collect(),
            row_formats: extras
                .row_formats
                .into_iter()
                .map(|(k, v)| (k, v.normalize()))
                .collect(),
            cell_formats: cell_formats
                .into_iter()
                .map(|(k, v)| (k, v.normalize()))
                .collect(),
            row_heights: extras.row_heights,
            col_widths: extras.col_widths,
            col_styles: extras.col_styles,
            row_styles: extras.row_styles,
            cell_styles: extras.cell_styles,
            styles: extras.styles,
            strings,
        })
    }

    /// Per-cell format overrides `(row, col) -> NumberFormat`, in deterministic
    /// order for snapshot encoding.
    #[must_use]
    pub const fn cell_formats(&self) -> &BTreeMap<(RowId, ColId), NumberFormat> {
        &self.cell_formats
    }
}

/// Validates basic worksheet tab name rules (non-empty, `<= 31` UTF-16 units,
/// no forbidden Excel characters `: \ / ? * [ ]`).
#[must_use]
pub fn is_valid_sheet_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    if name.encode_utf16().count() > MAX_SHEET_NAME_UTF16 {
        return false;
    }
    !name
        .chars()
        .any(|c| matches!(c, ':' | '\\' | '/' | '?' | '*' | '[' | ']'))
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::Sheet;
    use crate::format::NumberFormat;
    use crate::id::{ColId, IdAllocator, RowId, SheetId};
    use crate::value::{CalcError, Value};

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
        assert!(sheet.push_row(&[
            Value::Number(7.0),
            Value::Bool(true),
            Value::Error(CalcError::Div0),
        ]));
        assert_eq!((sheet.rows(), sheet.cols()), (2, 3));

        assert_eq!(text(&sheet, 0, 0), "合计");
        assert_eq!(text(&sheet, 0, 1), "12.50");
        assert_eq!(text(&sheet, 0, 2), "13%");
        assert_eq!(text(&sheet, 1, 0), "7");
        assert_eq!(text(&sheet, 1, 1), "TRUE");
        assert_eq!(text(&sheet, 1, 2), "#DIV/0!");
    }

    #[test]
    fn inserting_a_row_at_top_shifts_positions_without_moving_data() {
        let mut sheet = Sheet::new(&[NumberFormat::General]);
        assert!(sheet.push_row(&[Value::Number(10.0)]));
        assert!(sheet.push_row(&[Value::Number(20.0)]));

        let mut alloc = IdAllocator::local();
        let Some(inserted) = alloc.alloc_run(1) else {
            panic!("alloc");
        };
        assert_eq!(sheet.row_axis_mut().insert_after(None, inserted), Ok(()));
        assert!(sheet.set_value_by_id(RowId(inserted.first_id), ColId(0), Value::Number(99.0)));

        assert_eq!(sheet.rows(), 3);
        assert_eq!(sheet.row_id(0), inserted.first_id);
        assert_eq!(sheet.row_id(1), 0);
        assert_eq!(sheet.row_id(2), 1);
        assert_eq!(sheet.value(0, 0), Value::Number(99.0));
        assert_eq!(sheet.value(1, 0), Value::Number(10.0));
        assert_eq!(sheet.value(2, 0), Value::Number(20.0));
    }

    #[test]
    fn string_compaction_reclaims_unreferenced_entries_and_remaps_cells() {
        let mut sheet = Sheet::new(&[NumberFormat::General]);
        let s0 = sheet.add_string("dropped").unwrap_or(0);
        let s1 = sheet.add_string("kept-a").unwrap_or(0);
        let s2 = sheet.add_string("kept-b").unwrap_or(0);
        let _ = s0;
        assert!(sheet.push_row(&[Value::Text(s2)]));
        assert!(sheet.push_row(&[Value::Text(s1)]));
        assert_eq!(sheet.strings().len(), 3);

        sheet.compact_strings();
        assert_eq!(sheet.strings().len(), 2);
        assert_eq!(text(&sheet, 0, 0), "kept-b");
        assert_eq!(text(&sheet, 1, 0), "kept-a");
    }

    #[test]
    fn span_read_write_bumps_col_generation_only_on_change() {
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), 5000, 3);
        let col = ColId(1);
        let g0 = sheet.col_generation(col);

        let vals = [Value::Number(10.0).pack(), Value::Number(20.0).pack()];
        let mut changes = Vec::new();
        let n = sheet.write_row_id_span_packed(RowId(5), col, &vals, |o| changes.push(o));
        assert_eq!(n, 2);
        assert_eq!(changes, vec![0, 1]);
        let g1 = sheet.col_generation(col);
        assert!(g1 > g0);

        let n2 = sheet.write_row_id_span_packed(RowId(5), col, &vals, |_| {});
        assert_eq!(n2, 0);
        assert_eq!(sheet.col_generation(col), g1);

        let mut read_back = [0_u64; 4];
        sheet.read_row_id_span_packed(RowId(4), col, &mut read_back);
        assert_eq!(
            read_back,
            [
                Value::EMPTY_BITS,
                Value::Number(10.0).pack(),
                Value::Number(20.0).pack(),
                Value::EMPTY_BITS,
            ]
        );
    }
}
