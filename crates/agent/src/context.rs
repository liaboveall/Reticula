//! SpreadsheetLLM-inspired heuristic context compression (`SheetCompressor`)
//! for `workbook_overview` and RLE-compressed `read_range` (`18-m4.md` §2.5, §6.2).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
use reticula_formula::address::{CellAddr, parse_cell_addr, write_col_letters};
use reticula_formula::engine::FormulaEngine;

/// Hard byte ceiling for `workbook_overview` tool output (`12 KiB`).
pub const OVERVIEW_BYTE_BUDGET: usize = 12 * 1024;
/// Hard byte ceiling for `read_range` tool output (`16 KiB`).
pub const READ_RANGE_BYTE_BUDGET: usize = 16 * 1024;

/// Parses an A1 coordinate or rectangular range (`"A1"`, `"A1:D20"`, `"Sheet2!B2:C10"`).
/// Returns `(optional_sheet_name, r0, c0, r1, c1)` in 0-based inclusive coordinates.
pub fn parse_a1_range(raw: &str) -> Result<(Option<String>, u32, u32, u32, u32), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty range string".to_owned());
    }
    let (sheet_part, range_part) = if let Some((s, r)) = trimmed.rsplit_once('!') {
        let cleaned = s.trim();
        let unquoted = if cleaned.starts_with('\'') && cleaned.ends_with('\'') && cleaned.len() >= 2
        {
            cleaned[1..cleaned.len() - 1].replace("''", "'")
        } else {
            cleaned.to_owned()
        };
        (Some(unquoted), r.trim())
    } else {
        (None, trimmed)
    };

    let (start, end) = if let Some((a, b)) = range_part.split_once(':') {
        let ca =
            parse_cell_addr(a.trim()).ok_or_else(|| format!("invalid start cell address `{a}`"))?;
        let cb =
            parse_cell_addr(b.trim()).ok_or_else(|| format!("invalid end cell address `{b}`"))?;
        (ca, cb)
    } else {
        let ca = parse_cell_addr(range_part)
            .ok_or_else(|| format!("invalid cell address `{range_part}`"))?;
        (ca, ca)
    };

    let r0 = start.row.min(end.row);
    let r1 = start.row.max(end.row);
    let c0 = start.col.min(end.col);
    let c1 = start.col.max(end.col);
    Ok((sheet_part, r0, c0, r1, c1))
}

/// Formats a 0-based `(row, col)` as canonical uppercase A1 (`"B3"`).
pub fn a1_of(row: u32, col: u32) -> String {
    let mut s = String::new();
    CellAddr {
        col,
        row,
        abs_col: false,
        abs_row: false,
    }
    .write_a1(&mut s);
    s
}

/// Resolves a worksheet by case-insensitive display name, defaulting to the first
/// live sheet when `sheet_name` is `None`.
pub fn resolve_sheet_id(
    engine: &FormulaEngine,
    sheet_name: Option<&str>,
) -> Result<SheetId, String> {
    let wb = engine.workbook();
    let Some(name) = sheet_name.map(str::trim).filter(|s| !s.is_empty()) else {
        return wb
            .live_sheets()
            .next()
            .map(Sheet::id)
            .ok_or_else(|| "workbook has no live worksheets".to_owned());
    };
    wb.live_sheets()
        .find(|s| s.name().eq_ignore_ascii_case(name))
        .map(Sheet::id)
        .ok_or_else(|| format!("worksheet `{name}` not found"))
}

/// Produces a compressed structural & statistical overview of the workbook
/// (`<= 12 KiB`), highlighting header rows, subtotal/summary anchors, non-empty
/// block boundaries, column data types, numeric ranges, named ranges, and errors.
pub fn build_workbook_overview(
    engine: &FormulaEngine,
    sheet_filter: Option<&str>,
    max_cols: u32,
) -> String {
    let wb = engine.workbook();
    let max_cols = max_cols.clamp(1, 64);
    let mut out = String::new();

    let live_sheets: Vec<&Sheet> = wb.live_sheets().collect();
    let _ = writeln!(out, "Workbook sheets ({}):", live_sheets.len());
    for s in &live_sheets {
        let extent = s
            .used_extent()
            .map(|(r, c)| format!("A1:{}", a1_of(r, c)))
            .unwrap_or_else(|| "<empty>".to_owned());
        let _ = writeln!(out, "- '{}' (used: {})", s.name(), extent);
    }

    if !wb.named_ranges().is_empty() {
        let _ = writeln!(out, "\nNamed ranges ({}):", wb.named_ranges().len());
        for (key, def) in wb.named_ranges().iter().take(24) {
            let scope = key
                .scope
                .and_then(|sid| wb.sheet(sid))
                .map_or("Workbook", Sheet::name);
            let expr = def.formula.unparse(wb, key.scope.unwrap_or(SheetId(0)));
            let _ = writeln!(out, "- {scope}!{} = {expr}", def.name);
        }
    }

    for sheet in live_sheets {
        if let Some(filter) = sheet_filter.map(str::trim).filter(|s| !s.is_empty())
            && !sheet.name().eq_ignore_ascii_case(filter)
        {
            continue;
        }
        if out.len() >= OVERVIEW_BYTE_BUDGET.saturating_sub(256) {
            let _ = writeln!(out, "\n... [overview truncated at 12 KiB budget]");
            break;
        }
        write_sheet_summary(engine, sheet, max_cols, &mut out);
    }

    truncate_utf8(&mut out, OVERVIEW_BYTE_BUDGET);
    out
}

fn write_sheet_summary(engine: &FormulaEngine, sheet: &Sheet, max_cols: u32, out: &mut String) {
    let wb = engine.workbook();
    let sid = sheet.id();
    let _ = writeln!(out, "\n=== Sheet '{}' ===", sheet.name());

    let Some((max_r, max_c)) = sheet.used_extent() else {
        let _ = writeln!(out, "(empty worksheet)");
        return;
    };

    let mut populated = 0_u64;
    let mut formula_count = 0_u64;
    let mut error_count = 0_u64;
    let mut non_empty_rows: BTreeSet<u32> = BTreeSet::new();
    let mut summary_rows: BTreeSet<u32> = BTreeSet::new();

    #[derive(Default)]
    struct ColStats {
        header: Option<String>,
        num_count: u64,
        text_count: u64,
        bool_count: u64,
        err_count: u64,
        formula_count: u64,
        min_num: Option<f64>,
        max_num: Option<f64>,
        sample_formula: Option<String>,
    }

    let mut cols: BTreeMap<u32, ColStats> = BTreeMap::new();

    sheet.for_each_non_empty_cell(|r_pos, c_pos, row_id, col_id, val| {
        populated += 1;
        non_empty_rows.insert(r_pos);
        let has_formula = wb.formula(sid, row_id, col_id).is_some();
        if has_formula {
            formula_count += 1;
        }
        if matches!(val, Value::Error(_) | Value::Array(_)) {
            error_count += 1;
        }

        if c_pos < max_cols {
            let st = cols.entry(c_pos).or_default();
            if has_formula {
                st.formula_count += 1;
                if st.sample_formula.is_none()
                    && let Some(f) = wb.formula(sid, row_id, col_id)
                {
                    let unparsed = f.unparse(wb, sid);
                    let upper = unparsed.to_ascii_uppercase();
                    if upper.contains("SUM(")
                        || upper.contains("SUBTOTAL(")
                        || upper.contains("AVERAGE(")
                    {
                        summary_rows.insert(r_pos);
                    }
                    st.sample_formula = Some(unparsed);
                }
            }
            match val {
                Value::Number(n) => {
                    st.num_count += 1;
                    st.min_num = Some(st.min_num.map_or(n, |cur| cur.min(n)));
                    st.max_num = Some(st.max_num.map_or(n, |cur| cur.max(n)));
                }
                Value::Text(id) => {
                    st.text_count += 1;
                    let txt = sheet.strings().get(id);
                    if st.header.is_none() && r_pos <= 2 && !txt.trim().is_empty() {
                        st.header = Some(txt.trim().chars().take(32).collect());
                    }
                    let lower = txt.to_ascii_lowercase();
                    if lower.contains("total")
                        || lower.contains("subtotal")
                        || lower.contains("summary")
                    {
                        summary_rows.insert(r_pos);
                    }
                }
                Value::Bool(_) => st.bool_count += 1,
                Value::Error(_) | Value::Array(_) => st.err_count += 1,
                Value::Empty => {}
            }
        }
    });

    let _ = writeln!(
        out,
        "Used range: A1:{} | populated: {populated} | formulas: {formula_count} | errors: {error_count}",
        a1_of(max_r, max_c)
    );

    // Identify structural anchor rows (first non-empty row, block boundaries, summary rows, last row).
    let mut anchors: BTreeSet<u32> = BTreeSet::new();
    let mut prev_r: Option<u32> = None;
    for &r in &non_empty_rows {
        if prev_r.is_none() || prev_r.is_some_and(|p| r > p.saturating_add(1)) {
            anchors.insert(r);
            if let Some(p) = prev_r {
                anchors.insert(p);
            }
        }
        prev_r = Some(r);
    }
    if let Some(last) = prev_r {
        anchors.insert(last);
    }
    anchors.extend(summary_rows.iter().copied());

    if !anchors.is_empty() {
        let mut anchor_labels = Vec::new();
        for &r in anchors.iter().take(12) {
            let tag = if summary_rows.contains(&r) {
                format!("row {}(summary)", r + 1)
            } else {
                format!("row {}", r + 1)
            };
            anchor_labels.push(tag);
        }
        let _ = writeln!(out, "Anchor rows: {}", anchor_labels.join(", "));
    }

    let _ = writeln!(out, "Columns (showing up to {max_cols}):");
    for (c_pos, st) in cols {
        let mut col_name = String::new();
        write_col_letters(c_pos, &mut col_name);
        let hdr = st.header.as_deref().unwrap_or("-");
        let mut parts = Vec::new();
        if st.num_count > 0 {
            let range = match (st.min_num, st.max_num) {
                (Some(lo), Some(hi)) => format!("num:{}(min={lo:.4},max={hi:.4})", st.num_count),
                _ => format!("num:{}", st.num_count),
            };
            parts.push(range);
        }
        if st.text_count > 0 {
            parts.push(format!("text:{}", st.text_count));
        }
        if st.bool_count > 0 {
            parts.push(format!("bool:{}", st.bool_count));
        }
        if st.formula_count > 0 {
            let sample = st.sample_formula.as_deref().unwrap_or("");
            parts.push(format!("formulas:{} e.g. `{sample}`", st.formula_count));
        }
        if st.err_count > 0 {
            parts.push(format!("errors:{}", st.err_count));
        }
        let _ = writeln!(out, "  {col_name} ({hdr}): {}", parts.join(", "));
        if out.len() >= OVERVIEW_BYTE_BUDGET.saturating_sub(128) {
            break;
        }
    }
}

/// Output of [`read_range_compressed`]: worksheet ID, clamped `(r0, c0, r1, c1)`
/// rectangle, and RLE-compressed textual payload.
pub type CompressedRangeResult = (SheetId, (u32, u32, u32, u32), String);

/// Reads a rectangular range with blank-row and identical-row RLE compression,
/// bounded strictly to `16 KiB` with an explicit continuation coordinate hint.
pub fn read_range_compressed(
    engine: &FormulaEngine,
    sheet_name: Option<&str>,
    range_a1: &str,
    include_formulas: bool,
    max_rows: u32,
) -> Result<CompressedRangeResult, String> {
    let (parsed_sheet, r0, c0, r1, c1) = parse_a1_range(range_a1)?;
    let sid = resolve_sheet_id(engine, parsed_sheet.as_deref().or(sheet_name))?;
    let wb = engine.workbook();
    let sheet = wb
        .live_sheet(sid)
        .ok_or_else(|| "target worksheet not found".to_owned())?;

    let cap_rows = max_rows.clamp(1, 500);
    let r_end = r1
        .min(sheet.rows().saturating_sub(1))
        .min(r0.saturating_add(cap_rows).saturating_sub(1));
    let c_end = c1
        .min(sheet.cols().saturating_sub(1))
        .min(c0.saturating_add(63));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Range '{}'!{}:{}:",
        sheet.name(),
        a1_of(r0, c0),
        a1_of(r_end, c_end)
    );

    let mut blank_run_start: Option<u32> = None;
    let mut last_row_sig: Option<(String, u32, u32)> = None; // (rendered_body, first_r, repeat_count)
    let mut stopped_at: Option<u32> = None;

    let flush_blanks = |start: &mut Option<u32>, end_exclusive: u32, buf: &mut String| {
        if let Some(s) = start.take() {
            let count = end_exclusive.saturating_sub(s);
            if count == 1 {
                let _ = writeln!(buf, "Row {}: <empty>", s + 1);
            } else if count > 1 {
                let _ = writeln!(
                    buf,
                    "Rows {}..{}: <empty ({count} rows)>",
                    s + 1,
                    end_exclusive
                );
            }
        }
    };

    let flush_repeat = |sig: &mut Option<(String, u32, u32)>, buf: &mut String| {
        if let Some((body, first_r, count)) = sig.take() {
            if count == 1 {
                let _ = writeln!(buf, "Row {}: {body}", first_r + 1);
            } else {
                let _ = writeln!(
                    buf,
                    "Row {} (repeated x{count} through row {}): {body}",
                    first_r + 1,
                    first_r + count
                );
            }
        }
    };

    for r in r0..=r_end {
        let Some(row_raw) = sheet.row_axis().pos_to_id(r) else {
            continue;
        };
        let row_id = RowId(row_raw);
        let mut cells_rendered = Vec::new();
        for c in c0..=c_end {
            let Some(col_raw) = sheet.col_axis().pos_to_id(c) else {
                continue;
            };
            let col_id = ColId(col_raw);
            let inspected = engine.inspect_cell(sid, row_id, col_id);
            let Some(info) = inspected else {
                continue;
            };
            if info.display.is_empty() && !info.is_formula {
                continue;
            }
            let mut col_letters = String::new();
            write_col_letters(c, &mut col_letters);
            if include_formulas && info.is_formula {
                cells_rendered.push(format!("{col_letters}={} [={}]", info.display, info.raw));
            } else {
                cells_rendered.push(format!("{col_letters}={}", info.display));
            }
        }

        if cells_rendered.is_empty() {
            flush_repeat(&mut last_row_sig, &mut out);
            if blank_run_start.is_none() {
                blank_run_start = Some(r);
            }
            continue;
        }

        flush_blanks(&mut blank_run_start, r, &mut out);
        let body = cells_rendered.join(" | ");
        match &mut last_row_sig {
            Some((prev_body, _, count)) if *prev_body == body => {
                *count += 1;
            }
            _ => {
                flush_repeat(&mut last_row_sig, &mut out);
                last_row_sig = Some((body, r, 1));
            }
        }

        if out.len() >= READ_RANGE_BYTE_BUDGET.saturating_sub(256) {
            flush_repeat(&mut last_row_sig, &mut out);
            stopped_at = Some(r);
            break;
        }
    }

    flush_blanks(&mut blank_run_start, r_end.saturating_add(1), &mut out);
    flush_repeat(&mut last_row_sig, &mut out);

    let next_row = stopped_at
        .map(|r| r.saturating_add(1))
        .unwrap_or(r_end.saturating_add(1));
    if next_row <= r1 {
        let _ = writeln!(
            out,
            "... truncated; call read_range with range '{}:{}' to continue.",
            a1_of(next_row, c0),
            a1_of(r1, c1)
        );
    }

    truncate_utf8(&mut out, READ_RANGE_BYTE_BUDGET);
    Ok((sid, (r0, c0, r_end, c_end), out))
}

fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}
