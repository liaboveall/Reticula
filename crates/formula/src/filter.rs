//! AutoFilter criteria evaluation and O(log H) worker-local row projection
//! (`17-m3a.md` Step 4.1).
//!
//! Filtering is purely a viewport & `SUBTOTAL`/`AGGREGATE` visibility layer: it
//! never mutates [`Workbook`], emits WAL transactions, or changes `StateDigest`.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::id::{ColId, RowId};
use reticula_core::sheet::Sheet;
use reticula_core::value::{CalcError, Value};
use serde::{Deserialize, Serialize};

use crate::spill::SpillRegistry;
use crate::value_eval::EvalScalar;

/// Comparison operator used by [`ColFilterCriterion::NumberCompare`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterCmpOp {
    /// Equal to target value.
    Eq,
    /// Not equal to target value.
    Ne,
    /// Strictly greater than target value.
    Gt,
    /// Greater than or equal to target value.
    Ge,
    /// Strictly less than target value.
    Lt,
    /// Less than or equal to target value.
    Le,
}

/// Filter rule attached to one column inside a [`FilterTable`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ColFilterCriterion {
    /// Keep rows whose formatted display text matches `allowed` (case-insensitive)
    /// or is blank when `include_blanks` is `true`.
    Values {
        /// Allowed formatted display values.
        allowed: BTreeSet<String>,
        /// Whether blank cells remain visible.
        include_blanks: bool,
    },
    /// Keep non-blank cells whose formatted text contains `needle`.
    TextContains {
        /// Substring to match.
        needle: String,
        /// Whether comparison is case-sensitive.
        case_sensitive: bool,
    },
    /// Keep non-blank cells whose formatted text starts with `prefix`.
    TextStartsWith {
        /// Prefix to match.
        prefix: String,
        /// Whether comparison is case-sensitive.
        case_sensitive: bool,
    },
    /// Keep non-blank cells whose formatted text ends with `suffix`.
    TextEndsWith {
        /// Suffix to match.
        suffix: String,
        /// Whether comparison is case-sensitive.
        case_sensitive: bool,
    },
    /// Keep numeric cells satisfying `cell <op> value`.
    NumberCompare {
        /// Comparison operator.
        op: FilterCmpOp,
        /// Right-hand comparison target.
        value: f64,
    },
    /// Keep numeric cells inside `[min(min, max), max(min, max)]`.
    NumberBetween {
        /// Lower bound (inclusive).
        min: f64,
        /// Upper bound (inclusive).
        max: f64,
    },
    /// Keep numeric cells in the Top-N (`top == true`) or Bottom-N (`top == false`)
    /// ranked values within the filter column (ties included).
    TopN {
        /// Maximum rank count (`1..`).
        count: u32,
        /// `true` for Top-N, `false` for Bottom-N.
        top: bool,
    },
}

/// Anchored AutoFilter table bound to live row/column identities.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterTable {
    /// Header row holding filter dropdown buttons (always visible while filter exists).
    pub header_row: RowId,
    /// Inclusive last row of the filtered data region.
    pub last_row: RowId,
    /// Inclusive first column of the filter header bar.
    pub first_col: ColId,
    /// Inclusive last column of the filter header bar.
    pub last_col: ColId,
    /// Per-column active filter rules (combined with logical AND).
    pub criteria: BTreeMap<ColId, ColFilterCriterion>,
}

/// Resolved physical positions of a [`FilterTable`] on its live [`Sheet`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterBounds {
    /// 0-based physical header row index.
    pub header_row: u32,
    /// 0-based inclusive physical last row index.
    pub last_row: u32,
    /// 0-based inclusive first column index.
    pub first_col: u32,
    /// 0-based inclusive last column index.
    pub last_col: u32,
}

impl FilterTable {
    /// Resolves live 0-based physical bounds of this filter table on `sheet`,
    /// normalizing reversed anchors if columns/rows were reordered.
    #[must_use]
    pub fn resolve_bounds(&self, sheet: &Sheet) -> Option<FilterBounds> {
        let header_row = sheet.row_axis().id_to_pos(self.header_row.0)?;
        let raw_last_row = sheet.row_axis().id_to_pos(self.last_row.0)?;
        let c1 = sheet.col_axis().id_to_pos(self.first_col.0)?;
        let c2 = sheet.col_axis().id_to_pos(self.last_col.0)?;
        let last_row = raw_last_row.max(header_row);
        Some(FilterBounds {
            header_row,
            last_row,
            first_col: c1.min(c2),
            last_col: c1.max(c2),
        })
    }
}

/// O(log H) bidirectionally searchable visibility mapping over a sheet's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisibleRowProjection {
    total_rows: u32,
    visible_rows: u32,
    /// Sorted, disjoint, non-adjacent inclusive physical row intervals `[start, end]`.
    hidden_spans: Vec<(u32, u32)>,
    /// Cumulative hidden row count ending at each entry in `hidden_spans`.
    cum_hidden: Vec<u32>,
    /// Display row index `d` where each hidden span `[s, e]` sits between `d - 1` and `d`.
    display_skips: Vec<u32>,
    /// Live hidden row identities for `O(log H)` `SUBTOTAL`/`AGGREGATE` checks.
    hidden_row_ids: BTreeSet<RowId>,
}

impl VisibleRowProjection {
    /// Constructs an unfiltered identity projection for `total_rows`.
    #[must_use]
    pub const fn identity(total_rows: u32) -> Self {
        Self {
            total_rows,
            visible_rows: total_rows,
            hidden_spans: Vec::new(),
            cum_hidden: Vec::new(),
            display_skips: Vec::new(),
            hidden_row_ids: BTreeSet::new(),
        }
    }

    /// Rebuilds the projection from a set of still-relevant hidden [`RowId`]s.
    #[must_use]
    pub fn from_hidden_row_ids(sheet: &Sheet, hidden_row_ids: &BTreeSet<RowId>) -> Self {
        let total_rows = sheet.rows();
        if hidden_row_ids.is_empty() {
            return Self::identity(total_rows);
        }
        let mut live_ids = BTreeSet::new();
        let mut positions = Vec::with_capacity(hidden_row_ids.len());
        let row_axis = sheet.row_axis();
        for &rid in hidden_row_ids {
            if let Some(pos) = row_axis.id_to_pos(rid.0) {
                live_ids.insert(rid);
                positions.push(pos);
            }
        }
        Self::from_sorted_positions_and_ids(total_rows, &mut positions, live_ids)
    }

    /// Builds a projection directly from pre-collected hidden physical positions
    /// and their corresponding [`RowId`] set.
    #[must_use]
    pub fn from_hidden_positions(
        total_rows: u32,
        mut positions: Vec<u32>,
        hidden_row_ids: BTreeSet<RowId>,
    ) -> Self {
        Self::from_sorted_positions_and_ids(total_rows, &mut positions, hidden_row_ids)
    }

    fn from_sorted_positions_and_ids(
        total_rows: u32,
        positions: &mut [u32],
        hidden_row_ids: BTreeSet<RowId>,
    ) -> Self {
        if positions.is_empty() {
            return Self::identity(total_rows);
        }
        positions.sort_unstable();
        let mut hidden_spans: Vec<(u32, u32)> = Vec::new();
        for &pos in positions.iter() {
            if pos >= total_rows {
                continue;
            }
            if let Some((_, last_end)) = hidden_spans.last_mut()
                && pos <= last_end.saturating_add(1)
            {
                if pos > *last_end {
                    *last_end = pos;
                }
            } else {
                hidden_spans.push((pos, pos));
            }
        }
        Self::from_merged_spans(total_rows, hidden_spans, hidden_row_ids)
    }

    fn from_merged_spans(
        total_rows: u32,
        hidden_spans: Vec<(u32, u32)>,
        hidden_row_ids: BTreeSet<RowId>,
    ) -> Self {
        let mut cum_hidden = Vec::with_capacity(hidden_spans.len());
        let mut display_skips = Vec::with_capacity(hidden_spans.len());
        let mut total_hidden: u32 = 0;
        for &(start, end) in &hidden_spans {
            display_skips.push(start.saturating_sub(total_hidden));
            let span_len = end.saturating_sub(start).saturating_add(1);
            total_hidden = total_hidden.saturating_add(span_len);
            cum_hidden.push(total_hidden);
        }
        let visible_rows = total_rows.saturating_sub(total_hidden);
        Self {
            total_rows,
            visible_rows,
            hidden_spans,
            cum_hidden,
            display_skips,
            hidden_row_ids,
        }
    }

    /// Total physical rows on the underlying sheet.
    #[must_use]
    pub const fn total_rows(&self) -> u32 {
        self.total_rows
    }

    /// Number of visible rows exposed to the viewport.
    #[must_use]
    pub const fn visible_rows(&self) -> u32 {
        self.visible_rows
    }

    /// True if no rows are hidden.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hidden_spans.is_empty()
    }

    /// Inclusive physical `[start, end]` intervals of hidden rows.
    #[must_use]
    pub fn hidden_spans(&self) -> &[(u32, u32)] {
        &self.hidden_spans
    }

    /// Hidden row identities for rebuilding on structural edits.
    #[must_use]
    pub const fn hidden_row_ids(&self) -> &BTreeSet<RowId> {
        &self.hidden_row_ids
    }

    /// Returns `true` if `row_id` is currently hidden by AutoFilter.
    #[must_use]
    pub fn is_row_id_hidden(&self, row_id: RowId) -> bool {
        self.hidden_row_ids.contains(&row_id)
    }

    /// Maps a visible 0-based display row index `0..visible_rows` to its
    /// physical row index on the sheet in `O(log H)` time.
    #[must_use]
    pub fn display_to_physical(&self, display_row: u32) -> Option<u32> {
        if display_row >= self.visible_rows {
            return None;
        }
        if self.hidden_spans.is_empty() {
            return Some(display_row);
        }
        let k = self
            .display_skips
            .partition_point(|&skip| skip <= display_row);
        let hidden_before = k
            .checked_sub(1)
            .and_then(|idx| self.cum_hidden.get(idx).copied())
            .unwrap_or(0);
        Some(display_row.saturating_add(hidden_before))
    }

    /// Maps a physical 0-based row index `0..total_rows` to its visible display
    /// row index in `O(log H)` time, or `None` if that row is hidden.
    #[must_use]
    pub fn physical_to_display(&self, physical_row: u32) -> Option<u32> {
        if physical_row >= self.total_rows {
            return None;
        }
        if self.hidden_spans.is_empty() {
            return Some(physical_row);
        }
        let k = self
            .hidden_spans
            .partition_point(|&(start, _)| start <= physical_row);
        if let Some(prev_idx) = k.checked_sub(1) {
            if let Some(&(_, prev_end)) = self.hidden_spans.get(prev_idx)
                && physical_row <= prev_end
            {
                return None;
            }
            let hidden_before = self.cum_hidden.get(prev_idx).copied().unwrap_or(0);
            Some(physical_row.saturating_sub(hidden_before))
        } else {
            Some(physical_row)
        }
    }
}

/// Distinct formatted column values returned for the AutoFilter dropdown menu.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DistinctColumnValues {
    /// Sorted distinct non-blank `(display_text, row_count)` entries.
    pub values: Vec<(String, u32)>,
    /// Number of blank cells within the filter data rows (visible under other columns).
    pub blank_count: u32,
    /// `true` if distinct entry count hit `limit`.
    pub truncated: bool,
}

enum CompiledCriterion {
    Values {
        allowed_lower: BTreeSet<String>,
        include_blanks: bool,
    },
    TextContains {
        needle: String,
        case_sensitive: bool,
    },
    TextStartsWith {
        prefix: String,
        case_sensitive: bool,
    },
    TextEndsWith {
        suffix: String,
        case_sensitive: bool,
    },
    NumberCompare {
        op: FilterCmpOp,
        value: f64,
    },
    NumberBetween {
        lo: f64,
        hi: f64,
    },
    TopN {
        threshold: Option<f64>,
        top: bool,
    },
}

/// Evaluates `table` on `sheet` (considering active `spills` for dynamic-array
/// shadows) and produces a [`VisibleRowProjection`].
#[must_use]
pub fn evaluate_filter_table(
    sheet: &Sheet,
    table: &FilterTable,
    spills: Option<&SpillRegistry>,
) -> Option<VisibleRowProjection> {
    let bounds = table.resolve_bounds(sheet)?;
    let total_rows = sheet.rows();
    if table.criteria.is_empty() || bounds.header_row >= bounds.last_row {
        return Some(VisibleRowProjection::identity(total_rows));
    }

    let first_data_row = bounds.header_row.saturating_add(1);
    let data_row_count = bounds.last_row.saturating_sub(bounds.header_row);
    let data_rows = collect_live_rows(sheet, first_data_row, data_row_count);

    let mut compiled: Vec<(ColId, CompiledCriterion)> = Vec::with_capacity(table.criteria.len());
    for (&col_id, crit) in &table.criteria {
        if sheet.col_axis().id_to_pos(col_id.0).is_some() {
            compiled.push((
                col_id,
                compile_criterion(sheet, spills, col_id, &data_rows, crit),
            ));
        }
    }
    if compiled.is_empty() {
        return Some(VisibleRowProjection::identity(total_rows));
    }

    let mut hidden_positions = Vec::new();
    let mut hidden_row_ids = BTreeSet::new();
    let mut text_buf = String::new();
    for (idx, &row_id) in data_rows.iter().enumerate() {
        let phys_pos = first_data_row.saturating_add(u32::try_from(idx).unwrap_or(0));
        let mut visible = true;
        for (col_id, comp) in &compiled {
            if !cell_matches_compiled(sheet, spills, row_id, *col_id, comp, &mut text_buf) {
                visible = false;
                break;
            }
        }
        if !visible {
            hidden_positions.push(phys_pos);
            hidden_row_ids.insert(row_id);
        }
    }

    Some(VisibleRowProjection::from_hidden_positions(
        total_rows,
        hidden_positions,
        hidden_row_ids,
    ))
}

/// Scans `col` inside `table` to collect distinct formatted display values
/// (honoring active criteria on OTHER columns so cascading dropdowns reflect
/// visible peers), stopping at `limit` distinct non-blank values.
#[must_use]
pub fn distinct_column_values(
    sheet: &Sheet,
    table: &FilterTable,
    col: ColId,
    spills: Option<&SpillRegistry>,
    limit: usize,
) -> DistinctColumnValues {
    let Some(bounds) = table.resolve_bounds(sheet) else {
        return DistinctColumnValues::default();
    };
    if bounds.header_row >= bounds.last_row || sheet.col_axis().id_to_pos(col.0).is_none() {
        return DistinctColumnValues::default();
    }

    let first_data_row = bounds.header_row.saturating_add(1);
    let data_row_count = bounds.last_row.saturating_sub(bounds.header_row);
    let data_rows = collect_live_rows(sheet, first_data_row, data_row_count);

    let mut other_criteria: Vec<(ColId, CompiledCriterion)> = Vec::new();
    for (&cid, crit) in &table.criteria {
        if cid != col && sheet.col_axis().id_to_pos(cid.0).is_some() {
            other_criteria.push((cid, compile_criterion(sheet, spills, cid, &data_rows, crit)));
        }
    }

    let mut counts: BTreeMap<String, (String, u32)> = BTreeMap::new();
    let mut blank_count: u32 = 0;
    let mut truncated = false;
    let mut buf = String::new();

    for &row_id in &data_rows {
        let mut row_visible = true;
        for (cid, comp) in &other_criteria {
            if !cell_matches_compiled(sheet, spills, row_id, *cid, comp, &mut buf) {
                row_visible = false;
                break;
            }
        }
        if !row_visible {
            continue;
        }
        let is_blank = read_formatted_cell(sheet, spills, row_id, col, &mut buf);
        if is_blank || buf.is_empty() {
            blank_count = blank_count.saturating_add(1);
            continue;
        }
        let key = buf.to_lowercase();
        if let Some((_, count)) = counts.get_mut(&key) {
            *count = count.saturating_add(1);
        } else if counts.len() < limit {
            counts.insert(key, (buf.clone(), 1));
        } else {
            truncated = true;
        }
    }

    let mut values: Vec<(String, u32)> = counts.into_values().collect();
    values.sort_by_key(|a| a.0.to_lowercase());
    DistinctColumnValues {
        values,
        blank_count,
        truncated,
    }
}

fn collect_live_rows(sheet: &Sheet, first_pos: u32, count: u32) -> Vec<RowId> {
    let mut out = Vec::with_capacity((count as usize).min(65_536));
    if count == 0 {
        return out;
    }
    sheet
        .row_axis()
        .for_each_live_positions(first_pos, count, |run| {
            for raw in run.iter() {
                out.push(RowId(raw));
            }
        });
    out
}

fn compile_criterion(
    sheet: &Sheet,
    spills: Option<&SpillRegistry>,
    col_id: ColId,
    data_rows: &[RowId],
    crit: &ColFilterCriterion,
) -> CompiledCriterion {
    match crit {
        ColFilterCriterion::Values {
            allowed,
            include_blanks,
        } => {
            let mut allowed_lower = BTreeSet::new();
            for item in allowed {
                allowed_lower.insert(item.to_lowercase());
            }
            CompiledCriterion::Values {
                allowed_lower,
                include_blanks: *include_blanks,
            }
        }
        ColFilterCriterion::TextContains {
            needle,
            case_sensitive,
        } => CompiledCriterion::TextContains {
            needle: if *case_sensitive {
                needle.clone()
            } else {
                needle.to_lowercase()
            },
            case_sensitive: *case_sensitive,
        },
        ColFilterCriterion::TextStartsWith {
            prefix,
            case_sensitive,
        } => CompiledCriterion::TextStartsWith {
            prefix: if *case_sensitive {
                prefix.clone()
            } else {
                prefix.to_lowercase()
            },
            case_sensitive: *case_sensitive,
        },
        ColFilterCriterion::TextEndsWith {
            suffix,
            case_sensitive,
        } => CompiledCriterion::TextEndsWith {
            suffix: if *case_sensitive {
                suffix.clone()
            } else {
                suffix.to_lowercase()
            },
            case_sensitive: *case_sensitive,
        },
        ColFilterCriterion::NumberCompare { op, value } => CompiledCriterion::NumberCompare {
            op: *op,
            value: *value,
        },
        ColFilterCriterion::NumberBetween { min, max } => CompiledCriterion::NumberBetween {
            lo: min.min(*max),
            hi: min.max(*max),
        },
        ColFilterCriterion::TopN { count, top } => {
            let threshold = compute_top_n_threshold(sheet, spills, col_id, data_rows, *count, *top);
            CompiledCriterion::TopN {
                threshold,
                top: *top,
            }
        }
    }
}

fn compute_top_n_threshold(
    sheet: &Sheet,
    spills: Option<&SpillRegistry>,
    col_id: ColId,
    data_rows: &[RowId],
    count: u32,
    top: bool,
) -> Option<f64> {
    if count == 0 {
        return None;
    }
    let mut nums = Vec::new();
    for &row_id in data_rows {
        if let Some(n) = read_numeric_cell(sheet, spills, row_id, col_id) {
            nums.push(n);
        }
    }
    if nums.is_empty() {
        return None;
    }
    if top {
        nums.sort_by(|a, b| cmp_canon_f64(*b, *a));
    } else {
        nums.sort_by(|a, b| cmp_canon_f64(*a, *b));
    }
    let target_idx = (count as usize)
        .saturating_sub(1)
        .min(nums.len().saturating_sub(1));
    nums.get(target_idx).copied()
}

fn cmp_canon_f64(a: f64, b: f64) -> Ordering {
    let ca = if a == 0.0 { 0.0 } else { a };
    let cb = if b == 0.0 { 0.0 } else { b };
    ca.total_cmp(&cb)
}

fn read_numeric_cell(
    sheet: &Sheet,
    spills: Option<&SpillRegistry>,
    row_id: RowId,
    col_id: ColId,
) -> Option<f64> {
    match sheet.value_by_id(row_id, col_id) {
        Value::Number(n) if n.is_finite() => Some(n),
        Value::Empty => match spills.and_then(|sp| sp.shadow_value(sheet.id(), row_id, col_id)) {
            Some(EvalScalar::Number(n)) if n.is_finite() => Some(*n),
            _ => None,
        },
        _ => None,
    }
}

/// Writes the formatted display value of `(row_id, col_id)` into `out` and
/// returns `true` when the cell is logically blank (`Value::Empty`).
pub(crate) fn read_formatted_cell(
    sheet: &Sheet,
    spills: Option<&SpillRegistry>,
    row_id: RowId,
    col_id: ColId,
    out: &mut String,
) -> bool {
    out.clear();
    let val = sheet.value_by_id(row_id, col_id);
    if val != Value::Empty {
        sheet.format_value(val, row_id, col_id, out);
        return false;
    }
    if let Some(shadow) = spills.and_then(|sp| sp.shadow_value(sheet.id(), row_id, col_id)) {
        format_eval_scalar(sheet, row_id, col_id, shadow, out);
        return matches!(shadow, EvalScalar::Empty);
    }
    true
}

/// Formats an [`EvalScalar`] with `(row_id, col_id)`'s column/cell number format.
pub(crate) fn format_eval_scalar(
    sheet: &Sheet,
    row_id: RowId,
    col_id: ColId,
    scalar: &EvalScalar,
    out: &mut String,
) {
    match scalar {
        EvalScalar::Empty => {}
        EvalScalar::Number(n) => sheet.format_value(Value::number(*n), row_id, col_id, out),
        EvalScalar::Bool(b) => sheet.format_value(Value::Bool(*b), row_id, col_id, out),
        EvalScalar::Error(e) => out.push_str(e.as_str()),
        EvalScalar::Text(t) => out.push_str(t),
    }
}

fn cell_matches_compiled(
    sheet: &Sheet,
    spills: Option<&SpillRegistry>,
    row_id: RowId,
    col_id: ColId,
    comp: &CompiledCriterion,
    buf: &mut String,
) -> bool {
    match comp {
        CompiledCriterion::Values {
            allowed_lower,
            include_blanks,
        } => {
            let is_blank = read_formatted_cell(sheet, spills, row_id, col_id, buf);
            if is_blank || buf.is_empty() {
                *include_blanks
            } else {
                allowed_lower.contains(&buf.to_lowercase())
            }
        }
        CompiledCriterion::TextContains {
            needle,
            case_sensitive,
        } => {
            let is_blank = read_formatted_cell(sheet, spills, row_id, col_id, buf);
            if is_blank || buf.is_empty() {
                return needle.is_empty();
            }
            if *case_sensitive {
                buf.contains(needle.as_str())
            } else {
                buf.to_lowercase().contains(needle.as_str())
            }
        }
        CompiledCriterion::TextStartsWith {
            prefix,
            case_sensitive,
        } => {
            let is_blank = read_formatted_cell(sheet, spills, row_id, col_id, buf);
            if is_blank || buf.is_empty() {
                return prefix.is_empty();
            }
            if *case_sensitive {
                buf.starts_with(prefix.as_str())
            } else {
                buf.to_lowercase().starts_with(prefix.as_str())
            }
        }
        CompiledCriterion::TextEndsWith {
            suffix,
            case_sensitive,
        } => {
            let is_blank = read_formatted_cell(sheet, spills, row_id, col_id, buf);
            if is_blank || buf.is_empty() {
                return suffix.is_empty();
            }
            if *case_sensitive {
                buf.ends_with(suffix.as_str())
            } else {
                buf.to_lowercase().ends_with(suffix.as_str())
            }
        }
        CompiledCriterion::NumberCompare { op, value } => {
            let Some(cell_val) = read_numeric_cell(sheet, spills, row_id, col_id) else {
                return false;
            };
            if !value.is_finite() {
                return false;
            }
            let ord = cmp_canon_f64(cell_val, *value);
            match op {
                FilterCmpOp::Eq => ord == Ordering::Equal,
                FilterCmpOp::Ne => ord != Ordering::Equal,
                FilterCmpOp::Gt => ord == Ordering::Greater,
                FilterCmpOp::Ge => ord != Ordering::Less,
                FilterCmpOp::Lt => ord == Ordering::Less,
                FilterCmpOp::Le => ord != Ordering::Greater,
            }
        }
        CompiledCriterion::NumberBetween { lo, hi } => {
            let Some(cell_val) = read_numeric_cell(sheet, spills, row_id, col_id) else {
                return false;
            };
            if !lo.is_finite() || !hi.is_finite() {
                return false;
            }
            cmp_canon_f64(cell_val, *lo) != Ordering::Less
                && cmp_canon_f64(cell_val, *hi) != Ordering::Greater
        }
        CompiledCriterion::TopN { threshold, top } => {
            let (Some(thresh), Some(cell_val)) =
                (*threshold, read_numeric_cell(sheet, spills, row_id, col_id))
            else {
                return false;
            };
            if *top {
                cmp_canon_f64(cell_val, thresh) != Ordering::Less
            } else {
                cmp_canon_f64(cell_val, thresh) != Ordering::Greater
            }
        }
    }
}

/// Converts a stored [`Value`] (or error) into its error code.
#[must_use]
pub const fn value_error(v: Value) -> Option<CalcError> {
    match v {
        Value::Error(e) => Some(e),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]
mod tests {
    use alloc::collections::BTreeSet;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{
        CellEdit, CellInput, FormatPatch, Op, RawFormula, ScalarValue, Workbook,
    };

    use super::*;

    #[test]
    fn visible_row_projection_binary_searches_large_hidden_gaps() {
        let mut hidden = BTreeSet::new();
        let mut positions = Vec::new();
        // Keep header row 0, rows 500..=502, and final row 999_999.
        for pos in 1..500u32 {
            positions.push(pos);
            hidden.insert(RowId(u64::from(pos)));
        }
        for pos in 503..999_999u32 {
            positions.push(pos);
            hidden.insert(RowId(u64::from(pos)));
        }

        let proj = VisibleRowProjection::from_hidden_positions(1_000_000, positions, hidden);
        assert_eq!(proj.total_rows(), 1_000_000);
        assert_eq!(proj.visible_rows(), 5);
        assert_eq!(proj.hidden_spans(), &[(1, 499), (503, 999_998)]);

        let expected = [0u32, 500, 501, 502, 999_999];
        for (disp, &phys) in expected.iter().enumerate() {
            let d = u32::try_from(disp).unwrap();
            assert_eq!(proj.display_to_physical(d), Some(phys));
            assert_eq!(proj.physical_to_display(phys), Some(d));
        }
        assert_eq!(proj.display_to_physical(5), None);
        assert_eq!(proj.physical_to_display(1), None);
        assert_eq!(proj.physical_to_display(503), None);
        assert!(proj.is_row_id_hidden(RowId(250)));
        assert!(!proj.is_row_id_hidden(RowId(500)));
    }

    #[test]
    fn evaluate_filter_and_distinct_values_handle_top_n_blanks_and_multi_col() {
        let mut wb = Workbook::<RawFormula>::new();
        let edits = vec![
            CellEdit {
                row: RowId(0),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Text(String::from("Region"))),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(0),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Text(String::from("Sales"))),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(1),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Text(String::from("North"))),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(1),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(100.0)),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(2),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Text(String::from("South"))),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(2),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(300.0)),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(3),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Text(String::from("North"))),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(3),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(250.0)),
                format: FormatPatch::Keep,
            },
            // Row 4: blank region, 50 sales
            CellEdit {
                row: RowId(4),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(50.0)),
                format: FormatPatch::Keep,
            },
        ];
        wb.apply(Op::SetCells {
            sheet: SheetId(0),
            edits,
        })
        .unwrap();

        let sheet = wb.sheet(SheetId(0)).unwrap();
        let mut criteria = BTreeMap::new();
        criteria.insert(
            ColId(0),
            ColFilterCriterion::Values {
                allowed: [String::from("north")].into_iter().collect(),
                include_blanks: false,
            },
        );
        criteria.insert(
            ColId(1),
            ColFilterCriterion::TopN {
                count: 2,
                top: true,
            },
        );
        let table = FilterTable {
            header_row: RowId(0),
            last_row: RowId(4),
            first_col: ColId(0),
            last_col: ColId(1),
            criteria,
        };

        // Top 2 across column B are 300 (row 2, South) and 250 (row 3, North).
        // Combined with Region == North, only row 3 matches among rows 1..=4!
        let proj = evaluate_filter_table(sheet, &table, None).unwrap();
        assert_eq!(proj.hidden_spans(), &[(1, 2), (4, 4)]);
        assert_eq!(proj.display_to_physical(0), Some(0)); // header
        assert_eq!(proj.display_to_physical(1), Some(3)); // matched row

        // Cascading distinct values for Region while Sales=Top 2 is active shows North & South, no blanks.
        let distinct = distinct_column_values(sheet, &table, ColId(0), None, 100);
        assert_eq!(distinct.blank_count, 0);
        assert_eq!(
            distinct.values,
            vec![(String::from("North"), 1), (String::from("South"), 1)]
        );
    }
}
