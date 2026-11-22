//! Multi-key Excel-ordered row sorting and header auto-detection (`17-m3a.md` Step 4.2).
//!
//! Emits atomic [`Op::PermuteContent`] operations preserving row/column axis
//! identities while keeping blank cells last in both ascending and descending order.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use reticula_core::id::{ColId, RowId};
use reticula_core::ops::Op;
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
use serde::{Deserialize, Serialize};

use crate::binder::BoundFormula;
use crate::spill::SpillRegistry;
use crate::value_eval::EvalScalar;

/// Single sort column specification relative to the selected sort range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortKey {
    /// 0-based column offset from `first_col` inside the sorted rectangle.
    pub col_offset: u32,
    /// `true` for ascending order, `false` for descending order.
    pub ascending: bool,
    /// Whether text comparisons distinguish ASCII/Unicode case.
    pub case_sensitive: bool,
}

#[derive(Clone, Debug)]
enum SortVal {
    Blank,
    Number(f64),
    Text { lower: String, raw: String },
    Bool(bool),
    Error(String),
}

impl SortVal {
    const fn type_rank(&self) -> u8 {
        match self {
            Self::Number(_) => 0,
            Self::Text { .. } => 1,
            Self::Bool(_) => 2,
            Self::Error(_) => 3,
            Self::Blank => 4,
        }
    }
}

/// Detects whether `first_row` acts as a header row for `first_row..=last_row`.
#[must_use]
pub fn detect_header_row(
    sheet: &Sheet,
    first_row: u32,
    last_row: u32,
    first_col: u32,
    last_col: u32,
) -> bool {
    let sheet_rows = sheet.rows();
    let sheet_cols = sheet.cols();
    if first_row >= sheet_rows || first_col >= sheet_cols || last_row <= first_row {
        return false;
    }
    let last_row = last_row.min(sheet_rows.saturating_sub(1));
    let last_col = last_col.min(sheet_cols.saturating_sub(1));
    if first_col > last_col {
        return false;
    }

    let col_ids = collect_live_cols(
        sheet,
        first_col,
        last_col.saturating_sub(first_col).saturating_add(1),
    );
    let header_rid = RowId(sheet.row_id(first_row));
    let mut header_has_text = false;
    let mut header_has_bold = false;
    for &cid in &col_ids {
        match sheet.value_by_id(header_rid, cid) {
            Value::Empty => {}
            Value::Text(_) => {
                let mut tmp = String::new();
                let _ = sheet.write_text_by_id(header_rid, cid, &mut tmp);
                if !tmp.is_empty() {
                    header_has_text = true;
                }
            }
            Value::Number(_) | Value::Bool(_) | Value::Error(_) | Value::Array(_) => return false,
        }
        if sheet.effective_style_by_id(header_rid, cid).bold {
            header_has_bold = true;
        }
    }
    if !header_has_text {
        return false;
    }
    if header_has_bold {
        return true;
    }

    let sample_end = last_row.min(first_row.saturating_add(4));
    for r in first_row.saturating_add(1)..=sample_end {
        let rid = RowId(sheet.row_id(r));
        for &cid in &col_ids {
            match sheet.value_by_id(rid, cid) {
                Value::Number(_) | Value::Bool(_) | Value::Error(_) | Value::Array(_) => {
                    return true;
                }
                Value::Empty | Value::Text(_) => {}
            }
        }
    }
    false
}

/// Plans a stable multi-key row permutation over `first_row..=last_row × first_col..=last_col`.
///
/// Returns `None` when fewer than 2 data rows exist, `keys` is empty, bounds
/// are out of range, or the range is already sorted in the requested order.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn plan_sort(
    sheet: &Sheet,
    first_row: u32,
    last_row: u32,
    first_col: u32,
    last_col: u32,
    has_header: bool,
    keys: &[SortKey],
    spills: Option<&SpillRegistry>,
) -> Option<Op<BoundFormula>> {
    if keys.is_empty() || sheet.rows() == 0 || sheet.cols() == 0 {
        return None;
    }
    let last_row = last_row.min(sheet.rows().saturating_sub(1));
    let last_col = last_col.min(sheet.cols().saturating_sub(1));
    if first_col > last_col || first_row > last_row {
        return None;
    }
    let data_start = if has_header {
        first_row.saturating_add(1)
    } else {
        first_row
    };
    if data_start >= last_row {
        return None;
    }

    let row_count = last_row.saturating_sub(data_start).saturating_add(1);
    let col_count = last_col.saturating_sub(first_col).saturating_add(1);
    let rows = collect_live_rows(sheet, data_start, row_count);
    let cols = collect_live_cols(sheet, first_col, col_count);
    if rows.len() < 2 || cols.is_empty() {
        return None;
    }

    let mut active_keys: Vec<(usize, SortKey)> = Vec::new();
    let mut key_columns: Vec<Vec<SortVal>> = Vec::new();
    for &key in keys {
        let col_idx = key.col_offset as usize;
        if let Some(&cid) = cols.get(col_idx) {
            let col_vals: Vec<SortVal> = rows
                .iter()
                .map(|&rid| read_sort_val(sheet, spills, rid, cid))
                .collect();
            let slot_idx = key_columns.len();
            key_columns.push(col_vals);
            active_keys.push((slot_idx, key));
        }
    }
    if active_keys.is_empty() {
        return None;
    }

    let mut indices: Vec<usize> = (0..rows.len()).collect();
    indices.sort_by(|&a, &b| {
        for &(slot_idx, key) in &active_keys {
            if let Some(col_vals) = key_columns.get(slot_idx)
                && let (Some(va), Some(vb)) = (col_vals.get(a), col_vals.get(b))
            {
                let ord = cmp_sort_val(va, vb, key.ascending, key.case_sensitive);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
        Ordering::Equal
    });

    if indices.iter().enumerate().all(|(dst, &src)| dst == src) {
        return None;
    }

    let mut row_perm = Vec::with_capacity(indices.len());
    for src in indices {
        row_perm.push(u32::try_from(src).ok()?);
    }

    Some(Op::PermuteContent {
        sheet: sheet.id(),
        rows,
        cols,
        row_perm,
    })
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

fn collect_live_cols(sheet: &Sheet, first_pos: u32, count: u32) -> Vec<ColId> {
    let mut out = Vec::with_capacity(count as usize);
    if count == 0 {
        return out;
    }
    sheet
        .col_axis()
        .for_each_live_positions(first_pos, count, |run| {
            for raw in run.iter() {
                out.push(ColId(raw));
            }
        });
    out
}

fn read_sort_val(
    sheet: &Sheet,
    spills: Option<&SpillRegistry>,
    row_id: RowId,
    col_id: ColId,
) -> SortVal {
    match sheet.value_by_id(row_id, col_id) {
        Value::Empty => match spills.and_then(|sp| sp.shadow_value(sheet.id(), row_id, col_id)) {
            Some(EvalScalar::Number(n)) if n.is_finite() => SortVal::Number(*n),
            Some(EvalScalar::Text(t)) => {
                if t.is_empty() {
                    SortVal::Blank
                } else {
                    SortVal::Text {
                        lower: t.to_lowercase(),
                        raw: t.clone(),
                    }
                }
            }
            Some(EvalScalar::Bool(b)) => SortVal::Bool(*b),
            Some(EvalScalar::Error(e)) => SortVal::Error(String::from(e.as_str())),
            _ => SortVal::Blank,
        },
        Value::Number(n) if n.is_finite() => SortVal::Number(n),
        Value::Number(_) => SortVal::Error(String::from("#NUM!")),
        Value::Text(_) => {
            let mut raw = String::new();
            let _ = sheet.write_text_by_id(row_id, col_id, &mut raw);
            if raw.is_empty() {
                SortVal::Blank
            } else {
                SortVal::Text {
                    lower: raw.to_lowercase(),
                    raw,
                }
            }
        }
        Value::Bool(b) => SortVal::Bool(b),
        Value::Error(e) => SortVal::Error(String::from(e.as_str())),
        Value::Array(_) => SortVal::Error(String::from("#VALUE!")),
    }
}

fn cmp_canon_f64(a: f64, b: f64) -> Ordering {
    let ca = if a == 0.0 { 0.0 } else { a };
    let cb = if b == 0.0 { 0.0 } else { b };
    ca.total_cmp(&cb)
}

fn cmp_sort_val(a: &SortVal, b: &SortVal, ascending: bool, case_sensitive: bool) -> Ordering {
    match (a, b) {
        (SortVal::Blank, SortVal::Blank) => return Ordering::Equal,
        (SortVal::Blank, _) => return Ordering::Greater,
        (_, SortVal::Blank) => return Ordering::Less,
        _ => {}
    }

    let rank_ord = a.type_rank().cmp(&b.type_rank());
    let ord = if rank_ord != Ordering::Equal {
        rank_ord
    } else {
        match (a, b) {
            (SortVal::Number(na), SortVal::Number(nb)) => cmp_canon_f64(*na, *nb),
            (SortVal::Text { lower: la, raw: ra }, SortVal::Text { lower: lb, raw: rb }) => {
                let c = la.cmp(lb);
                if c == Ordering::Equal && case_sensitive {
                    ra.cmp(rb)
                } else {
                    c
                }
            }
            (SortVal::Bool(ba), SortVal::Bool(bb)) => ba.cmp(bb),
            (SortVal::Error(ea), SortVal::Error(eb)) => ea.cmp(eb),
            _ => Ordering::Equal,
        }
    };

    if ascending { ord } else { ord.reverse() }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use alloc::vec;

    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue, Workbook};
    use reticula_core::value::{CalcError, Value};

    use super::*;
    use crate::engine::FormulaEngine;

    fn cell_text(sheet: &Sheet, row: u32, col: u32) -> String {
        let mut s = String::new();
        let _ = sheet.write_text(row, col, &mut s);
        s
    }

    #[test]
    fn sort_places_blanks_last_in_both_directions_and_orders_mixed_types() {
        let mut wb = Workbook::new();
        wb.apply(Op::SetCells {
            sheet: SheetId(0),
            edits: vec![
                CellEdit {
                    row: RowId(0),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Text(String::from("Header"))),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(1),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Error(CalcError::Na)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(2),
                    col: ColId(0),
                    input: CellInput::Empty,
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(3),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Bool(true)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(4),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Text(String::from("beta"))),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(5),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(10.0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(6),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Text(String::new())),
                    format: FormatPatch::Keep,
                },
            ],
        })
        .unwrap();

        let sheet = wb.sheet(SheetId(0)).unwrap();
        assert!(detect_header_row(sheet, 0, 6, 0, 0));

        // Ascending: 10 < "beta" < TRUE < #N/A < blanks (rows 2 and 6 keep relative order).
        let asc_op = plan_sort(
            sheet,
            0,
            6,
            0,
            0,
            true,
            &[SortKey {
                col_offset: 0,
                ascending: true,
                case_sensitive: false,
            }],
            None,
        )
        .unwrap();

        let mut engine = FormulaEngine::from_workbook_clean(wb.clone());
        let inv = engine.apply_op(asc_op).unwrap();
        let s = engine.workbook().sheet(SheetId(0)).unwrap();
        assert_eq!(cell_text(s, 0, 0), "Header");
        assert_eq!(s.value(1, 0), Value::number(10.0));
        assert_eq!(cell_text(s, 2, 0), "beta");
        assert_eq!(s.value(3, 0), Value::Bool(true));
        assert_eq!(s.value(4, 0), Value::Error(CalcError::Na));
        assert_eq!(s.value(5, 0), Value::Empty);
        assert!(matches!(s.value(6, 0), Value::Text(_)));
        assert_eq!(cell_text(s, 6, 0), "");

        engine.apply_inverse(inv).unwrap();
        assert_eq!(engine.workbook().state_digest(), wb.state_digest());

        // Descending: #N/A > TRUE > "beta" > 10, AND blanks stay LAST!
        let desc_op = plan_sort(
            engine.workbook().sheet(SheetId(0)).unwrap(),
            0,
            6,
            0,
            0,
            true,
            &[SortKey {
                col_offset: 0,
                ascending: false,
                case_sensitive: false,
            }],
            None,
        )
        .unwrap();
        engine.apply_op(desc_op).unwrap();
        let s2 = engine.workbook().sheet(SheetId(0)).unwrap();
        assert_eq!(s2.value(1, 0), Value::Error(CalcError::Na));
        assert_eq!(s2.value(2, 0), Value::Bool(true));
        assert_eq!(cell_text(s2, 3, 0), "beta");
        assert_eq!(s2.value(4, 0), Value::number(10.0));
        assert_eq!(s2.value(5, 0), Value::Empty);
        assert!(matches!(s2.value(6, 0), Value::Text(_)));
    }
}
