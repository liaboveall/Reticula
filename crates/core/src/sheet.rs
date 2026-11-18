//! A worksheet: identity-addressed axes, slotted columns and formatting.
//!
//! Positions (`0..rows()`, `0..cols()`) are derived from `row_axis` and
//! `col_axis`. Cell values and explicit formats are stored against `(row_slot,
//! col_slot)` so inserting or deleting rows on the axis never moves cell data.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::axis::{
    Axis, AxisRun, DEFAULT_BASE_COLS, DEFAULT_BASE_ROWS, MAX_COL_AXIS_LEN, MAX_ROW_AXIS_LEN,
};
use crate::format::{NumberFormat, write_number};
use crate::id::{ColId, IdRun, RowId, SheetId};
use crate::store::{Column, SlotMap};
use crate::strings::StringTable;
use crate::value::Value;

type CellFormatMap = BTreeMap<(RowId, ColId), NumberFormat>;

/// One worksheet of a workbook.
#[derive(Clone, Debug)]
pub struct Sheet {
    id: SheetId,
    name: String,
    row_axis: Axis,
    col_axis: Axis,
    row_slots: SlotMap,
    col_slots: SlotMap,
    columns: Vec<Column>,
    col_formats: Vec<NumberFormat>,
    cell_formats: CellFormatMap,
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
            row_axis: Axis::empty(MAX_ROW_AXIS_LEN),
            col_axis: Axis::with_base(base_cols, MAX_COL_AXIS_LEN),
            row_slots: SlotMap::with_base(MAX_ROW_AXIS_LEN),
            col_slots: SlotMap::with_base(base_cols.max(DEFAULT_BASE_COLS)),
            columns: formats.iter().map(|_| Column::new()).collect(),
            col_formats: formats.to_vec(),
            cell_formats: CellFormatMap::default(),
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
            row_axis: Axis::with_base(base_rows, MAX_ROW_AXIS_LEN),
            col_axis: Axis::with_base(base_cols, MAX_COL_AXIS_LEN),
            row_slots: SlotMap::with_base(base_rows.max(DEFAULT_BASE_ROWS)),
            col_slots: SlotMap::with_base(base_cols.max(DEFAULT_BASE_COLS)),
            columns: Vec::new(),
            col_formats: Vec::new(),
            cell_formats: CellFormatMap::default(),
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
            row_axis: Axis::with_base(rows, MAX_ROW_AXIS_LEN),
            col_axis: Axis::with_base(base_cols, MAX_COL_AXIS_LEN),
            row_slots: SlotMap::with_base(rows.max(DEFAULT_BASE_ROWS)),
            col_slots: SlotMap::with_base(base_cols.max(DEFAULT_BASE_COLS)),
            columns: formats
                .iter()
                .map(|_| Column::with_slot_capacity(rows))
                .collect(),
            col_formats: formats.to_vec(),
            cell_formats: CellFormatMap::default(),
            strings,
        }
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

    /// Sets the default number format of a column by identity.
    pub fn set_col_format_by_id(&mut self, col: ColId, format: NumberFormat) -> bool {
        let Some(col_slot) = self.col_slots.get_or_alloc(col.0) else {
            return false;
        };
        let idx = col_slot as usize;
        if idx >= self.col_formats.len() {
            self.col_formats
                .resize(idx.saturating_add(1), NumberFormat::General);
        }
        if let Some(slot) = self.col_formats.get_mut(idx) {
            *slot = format;
        }
        true
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
        self.row_axis
            .estimated_bytes()
            .saturating_add(self.col_axis.estimated_bytes())
            .saturating_add(self.row_slots.estimated_bytes())
            .saturating_add(self.col_slots.estimated_bytes())
            .saturating_add(cols_mem)
            .saturating_add(cell_fmt_mem)
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
                .runs()
                .last()
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
        }
        true
    }

    /// Fast direct write to a base `(row_slot, col_slot)` during bulk sample
    /// generation when `with_base_formats` has already initialised the axes.
    pub fn set_base_slot(&mut self, row_slot: u32, col_slot: usize, value: Value) {
        if let Some(column) = self.columns.get_mut(col_slot) {
            column.set(row_slot, value);
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
        }
        self.columns
            .get_mut(col_idx)
            .is_some_and(|column| column.set(row_slot, value))
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
                self.cell_formats.insert((row, col), fmt);
                true
            }
        }
    }

    /// Effective number format at `(row, col)` (cell override, falling back to
    /// column default, then [`NumberFormat::General`]).
    #[must_use]
    pub fn effective_format_by_id(&self, row: RowId, col: ColId) -> NumberFormat {
        if let Some(fmt) = self.cell_format_by_id(row, col) {
            return fmt;
        }
        self.col_slots
            .get(col.0)
            .and_then(|slot| self.col_formats.get(slot as usize))
            .copied()
            .unwrap_or_default()
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

    /// Restores components decoded from a snapshot.
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
        let row_axis = Axis::from_runs(row_runs, MAX_ROW_AXIS_LEN).ok()?;
        let col_axis = Axis::from_runs(col_runs, MAX_COL_AXIS_LEN).ok()?;
        Some(Self {
            id,
            name,
            row_axis,
            col_axis,
            row_slots,
            col_slots,
            columns,
            col_formats,
            cell_formats,
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

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;

    use super::Sheet;
    use crate::format::NumberFormat;
    use crate::id::{ColId, IdAllocator, RowId};
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
            Value::Error(CalcError::Div0)
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
    fn cells_outside_the_sheet_are_empty() {
        let mut sheet = Sheet::new(&[NumberFormat::General]);
        assert!(sheet.push_row(&[Value::Number(1.0)]));
        assert_eq!(sheet.value(1, 0), Value::Empty);
        assert_eq!(sheet.value(0, 1), Value::Empty);
        assert_eq!(sheet.value(u32::MAX, u32::MAX), Value::Empty);
        assert_eq!(text(&sheet, 5, 5), "");
    }
}
