//! Queries that change nothing, answered alike by the live engine and by the
//! history player.

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::Workbook;
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_formula::search::{self, FindOptions};
use reticula_formula::value_eval::EvalScalar;

use crate::clipboard::{InternalClipboard, RectPos, copy_range};
use crate::{
    EngineEvent, JumpDirection, WireCellRect, WireCellStyle, WireSearchMatch, WireSpillAnchor,
    WireValueKind,
};

pub fn map_display_rect_to_physical(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    rect: RectPos,
) -> Option<RectPos> {
    if engine.filter_state(sheet_id).is_none() {
        return Some(rect);
    }
    let pr1 = engine.display_to_physical_row(sheet_id, rect.first_row)?;
    let last_disp = rect
        .first_row
        .saturating_add(rect.row_count.saturating_sub(1));
    let pr2 = engine.display_to_physical_row(sheet_id, last_disp)?;
    Some(RectPos {
        first_row: pr1,
        row_count: pr2.saturating_sub(pr1).saturating_add(1),
        first_col: rect.first_col,
        col_count: rect.col_count,
    })
}

/// The rows shown as `first` and the `count - 1` after it, as `(first, count)`
/// runs of adjacent rows of the sheet. Hidden rows between them are left out.
/// `None` when not all of them are on the sheet.
pub fn physical_row_runs(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    first: u32,
    count: u32,
) -> Option<Vec<(u32, u32)>> {
    if engine.filter_state(sheet_id).is_none() {
        return Some(vec![(first, count)]);
    }
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for display in first..first.saturating_add(count) {
        let row = engine.display_to_physical_row(sheet_id, display)?;
        match runs.last_mut() {
            Some((start, len)) if start.saturating_add(*len) == row => *len += 1,
            _ => runs.push((row, 1)),
        }
    }
    Some(runs)
}

pub fn nearest_visible_display_row(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    physical_row: u32,
) -> u32 {
    if engine.filter_state(sheet_id).is_none() {
        return physical_row;
    }
    if let Some(d) = engine.physical_to_display_row(sheet_id, physical_row) {
        return d;
    }
    let Some(sheet) = engine.workbook().sheet(sheet_id) else {
        return 0;
    };
    let mut r = physical_row;
    while r > 0 {
        r = r.saturating_sub(1);
        if let Some(d) = engine.physical_to_display_row(sheet_id, r) {
            return d;
        }
    }
    let max_r = sheet.rows().saturating_sub(1);
    for candidate in physical_row..=max_r {
        if let Some(d) = engine.physical_to_display_row(sheet_id, candidate) {
            return d;
        }
    }
    0
}

pub fn jump_edge(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    row: u32,
    col: u32,
    dir: JumpDirection,
    select: bool,
) -> EngineEvent {
    let Some(sheet) = engine.workbook().sheet(sheet_id) else {
        return EngineEvent::EdgeJumped {
            sheet: sheet_id.0,
            row,
            col,
            select,
        };
    };
    let rows = sheet.rows();
    let cols = sheet.cols();
    if rows == 0 || cols == 0 {
        return EngineEvent::EdgeJumped {
            sheet: sheet_id.0,
            row: 0,
            col: 0,
            select,
        };
    }
    let phys_row = engine.display_to_physical_row(sheet_id, row).unwrap_or(row);
    let r0 = phys_row.min(rows.saturating_sub(1));
    let c0 = col.min(cols.saturating_sub(1));
    let (nr, nc) = compute_jump_target(engine.workbook(), sheet, sheet_id, r0, c0, dir);
    EngineEvent::EdgeJumped {
        sheet: sheet_id.0,
        row: nearest_visible_display_row(engine, sheet_id, nr),
        col: nc,
        select,
    }
}

pub fn jump_used_end(engine: &FormulaEngine, sheet_id: SheetId, select: bool) -> EngineEvent {
    let (mut max_r, mut max_c) = engine
        .workbook()
        .sheet(sheet_id)
        .and_then(Sheet::used_extent)
        .unwrap_or((0, 0));
    if let Some(sheet) = engine.workbook().sheet(sheet_id) {
        let start = (sheet_id, RowId(0), ColId(0));
        let end = (sheet_id, RowId(u64::MAX), ColId(u64::MAX));
        for ((_, r_id, c_id), _) in engine.workbook().formulas().range(start..=end) {
            if let (Some(rp), Some(cp)) = (
                sheet.row_axis().id_to_pos(r_id.0),
                sheet.col_axis().id_to_pos(c_id.0),
            ) {
                max_r = max_r.max(rp);
                max_c = max_c.max(cp);
            }
        }
    }
    EngineEvent::EdgeJumped {
        sheet: sheet_id.0,
        row: nearest_visible_display_row(engine, sheet_id, max_r),
        col: max_c,
        select,
    }
}

/// Copies `rect`. The second half of the result is what a later paste inside
/// the same workbook needs; the caller decides whether to keep it.
pub fn copy_selection(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    rect: RectPos,
    cut: bool,
) -> Result<(EngineEvent, InternalClipboard), String> {
    let phys_rect = map_display_rect_to_physical(engine, sheet_id, rect)
        .ok_or_else(|| String::from("Copy selection falls outside visible rows."))?;
    let (payload, internal) = copy_range(engine.workbook(), sheet_id, phys_rect, cut)?;
    let event = EngineEvent::ClipboardCopied {
        sheet: sheet_id.0,
        first_row: rect.first_row,
        row_count: rect.row_count,
        first_col: rect.first_col,
        col_count: rect.col_count,
        cut,
        tsv: payload.tsv,
        html: payload.html,
        fingerprint: payload.fingerprint,
    };
    Ok((event, internal))
}

fn search_match_to_wire(engine: &FormulaEngine, m: search::SearchMatch) -> WireSearchMatch {
    let row = nearest_visible_display_row(engine, m.sheet, m.row_pos);
    WireSearchMatch {
        sheet: m.sheet.0,
        sheet_name: m.sheet_name,
        row,
        physical_row: m.row_pos,
        col: m.col_pos,
        row_id: m.row_id.0,
        col_id: m.col_id.0,
        text: m.text,
        is_formula: m.is_formula,
    }
}

pub fn find_all(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    opts: &FindOptions,
    max_results: usize,
) -> EngineEvent {
    match search::find_all(engine, sheet_id, opts, max_results) {
        Ok(res) => {
            let total = u32::try_from(res.matches.len()).unwrap_or(u32::MAX);
            let matches = res
                .matches
                .into_iter()
                .map(|m| search_match_to_wire(engine, m))
                .collect();
            EngineEvent::SearchResults {
                matches,
                total,
                truncated: res.truncated,
            }
        }
        Err(err) => EngineEvent::Rejected {
            reason: err.to_string(),
        },
    }
}

pub fn find_next(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    from_row: u32,
    from_col: u32,
    backwards: bool,
    opts: &FindOptions,
) -> EngineEvent {
    let phys_row = engine
        .display_to_physical_row(sheet_id, from_row)
        .unwrap_or(from_row);
    match search::find_next(engine, sheet_id, phys_row, from_col, backwards, opts) {
        Ok(found) => EngineEvent::SearchNavigated {
            found: found.map(|m| search_match_to_wire(engine, m)),
        },
        Err(err) => EngineEvent::Rejected {
            reason: err.to_string(),
        },
    }
}

pub fn inspect_cell(
    engine: &FormulaEngine,
    sheet_id: SheetId,
    row_id: RowId,
    col_id: ColId,
) -> Option<EngineEvent> {
    let info = engine.inspect_cell(sheet_id, row_id, col_id)?;
    let sheet = engine.workbook().sheet(sheet_id)?;
    let format_code = String::from(sheet.effective_format_by_id(row_id, col_id).code_str());
    let style = WireCellStyle::from(sheet.effective_style_by_id(row_id, col_id));
    let key = (sheet_id, row_id, col_id);
    let anchor_opt = if engine.spills().active_spill(key).is_some() {
        Some(key)
    } else {
        engine.spills().anchor_of_cell(key)
    };
    let is_spill_shadow = anchor_opt.is_some_and(|a| a != key);
    let spill_range = anchor_opt.and_then(|anchor| {
        let entry = engine.spills().active_spill(anchor)?;
        let pr1 = sheet.row_axis().id_to_pos(anchor.1.0)?;
        let c1 = sheet.col_axis().id_to_pos(anchor.2.0)?;
        let pr2 = entry
            .rows
            .last()
            .and_then(|r| sheet.row_axis().id_to_pos(r.0))
            .unwrap_or(pr1);
        let c2 = entry
            .cols
            .last()
            .and_then(|c| sheet.col_axis().id_to_pos(c.0))
            .unwrap_or(c1);
        let d1 = engine.physical_to_display_row(sheet_id, pr1).unwrap_or(pr1);
        let d2 = engine.physical_to_display_row(sheet_id, pr2).unwrap_or(pr2);
        Some(WireCellRect {
            first_row: d1.min(d2),
            last_row: d1.max(d2),
            first_col: c1.min(c2),
            last_col: c1.max(c2),
        })
    });
    Some(EngineEvent::CellInspected {
        sheet: sheet_id.0,
        row_id: row_id.0,
        col_id: col_id.0,
        address: info.address,
        raw: info.raw,
        display: info.display,
        is_formula: info.is_formula,
        error: info.error,
        format_code,
        style,
        spill_range,
        is_spill_shadow,
        spill_anchor: anchor_opt.filter(|_| is_spill_shadow).and_then(
            |(anchor_sheet, anchor_row, anchor_col)| {
                let anchor = engine.inspect_cell(anchor_sheet, anchor_row, anchor_col)?;
                Some(WireSpillAnchor {
                    address: anchor.address,
                    formula: anchor.raw,
                })
            },
        ),
        value_kind: value_kind(engine, sheet, row_id, col_id),
    })
}

fn value_kind(engine: &FormulaEngine, sheet: &Sheet, row: RowId, col: ColId) -> WireValueKind {
    match sheet.value_by_id(row, col) {
        Value::Number(_) => WireValueKind::Number,
        Value::Bool(_) => WireValueKind::Boolean,
        Value::Text(_) => WireValueKind::Text,
        Value::Error(_) | Value::Array(_) => WireValueKind::Error,
        Value::Empty => match engine.spills().shadow_value(sheet.id(), row, col) {
            Some(EvalScalar::Number(_)) => WireValueKind::Number,
            Some(EvalScalar::Bool(_)) => WireValueKind::Boolean,
            Some(EvalScalar::Text(_)) => WireValueKind::Text,
            Some(EvalScalar::Error(_)) => WireValueKind::Error,
            Some(EvalScalar::Empty) | None => WireValueKind::Empty,
        },
    }
}

fn compute_jump_target(
    wb: &Workbook<BoundFormula>,
    sheet: &Sheet,
    sheet_id: SheetId,
    r0: u32,
    c0: u32,
    dir: JumpDirection,
) -> (u32, u32) {
    let rows = sheet.rows();
    let cols = sheet.cols();
    let is_occ = |r: u32, c: u32| -> bool {
        let (Some(r_id), Some(c_id)) =
            (sheet.row_axis().pos_to_id(r), sheet.col_axis().pos_to_id(c))
        else {
            return false;
        };
        !matches!(sheet.value_by_id(RowId(r_id), ColId(c_id)), Value::Empty)
            || wb.formula(sheet_id, RowId(r_id), ColId(c_id)).is_some()
    };

    // O(1) fast path when jumping vertically along a completely empty column!
    if matches!(dir, JumpDirection::Up | JumpDirection::Down)
        && let Some(c_id) = sheet.col_axis().pos_to_id(c0)
    {
        let col_empty = sheet
            .col_slots()
            .get(c_id)
            .and_then(|slot| sheet.raw_columns().get(slot as usize))
            .is_none_or(|col| col.blocks().is_empty());
        if col_empty && !is_occ(r0, c0) {
            // Check if any formula exists in `c_id` (rare if cached value is Empty).
            let has_any_formula = wb.formulas().col_has_any(sheet_id, ColId(c_id));
            if !has_any_formula {
                return match dir {
                    JumpDirection::Up => (0, c0),
                    JumpDirection::Down => (rows.saturating_sub(1), c0),
                    _ => (r0, c0),
                };
            }
        }
    }

    match dir {
        JumpDirection::Down => {
            if r0.saturating_add(1) >= rows {
                return (r0, c0);
            }
            if is_occ(r0, c0) && is_occ(r0 + 1, c0) {
                let mut r = r0 + 1;
                while r + 1 < rows && is_occ(r + 1, c0) {
                    r += 1;
                }
                (r, c0)
            } else {
                let mut r = r0 + 1;
                while r < rows {
                    if is_occ(r, c0) {
                        return (r, c0);
                    }
                    r += 1;
                }
                (rows - 1, c0)
            }
        }
        JumpDirection::Up => {
            if r0 == 0 {
                return (0, c0);
            }
            if is_occ(r0, c0) && is_occ(r0 - 1, c0) {
                let mut r = r0 - 1;
                while r > 0 && is_occ(r - 1, c0) {
                    r -= 1;
                }
                (r, c0)
            } else {
                let mut r = r0 - 1;
                loop {
                    if is_occ(r, c0) {
                        return (r, c0);
                    }
                    if r == 0 {
                        return (0, c0);
                    }
                    r -= 1;
                }
            }
        }
        JumpDirection::Right => {
            if c0.saturating_add(1) >= cols {
                return (r0, c0);
            }
            if is_occ(r0, c0) && is_occ(r0, c0 + 1) {
                let mut c = c0 + 1;
                while c + 1 < cols && is_occ(r0, c + 1) {
                    c += 1;
                }
                (r0, c)
            } else {
                let mut c = c0 + 1;
                while c < cols {
                    if is_occ(r0, c) {
                        return (r0, c);
                    }
                    c += 1;
                }
                (r0, cols - 1)
            }
        }
        JumpDirection::Left => {
            if c0 == 0 {
                return (r0, 0);
            }
            if is_occ(r0, c0) && is_occ(r0, c0 - 1) {
                let mut c = c0 - 1;
                while c > 0 && is_occ(r0, c - 1) {
                    c -= 1;
                }
                (r0, c)
            } else {
                let mut c = c0 - 1;
                loop {
                    if is_occ(r0, c) {
                        return (r0, c);
                    }
                    if c == 0 {
                        return (r0, 0);
                    }
                    c -= 1;
                }
            }
        }
    }
}
