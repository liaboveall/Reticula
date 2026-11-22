//! `.xlsx` importer combining `calamine` (shared formulas + values + 1904
//! epoch) with `quick-xml` (`xml_scan`) for dynamic-array metadata, styles,
//! cached formula results, freeze panes, and unsupported OOXML inventory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

use calamine::{CellErrorType, Data, Range, Reader as _, Xlsx, open_workbook_from_rs};
use reticula_core::id::{ColId, IdAllocator, RowId, SheetId};
use reticula_core::ops::{
    FormulaMode, MAX_CELL_TEXT_UTF16, MAX_NAMED_RANGE_COMMENT_CHARS, MAX_NAMED_RANGES, MAX_SHEETS,
    NamedRangeDef, NamedRangeKey, ScalarValue, Workbook, WorkbookCellKey,
    is_valid_named_range_name,
};
use reticula_core::sheet::{Sheet, is_valid_sheet_name};
use reticula_core::value::{CalcError, Value};
use reticula_formula::binder::{BoundExpr, BoundFormula, bind_expr};
use reticula_formula::engine::FormulaEngine;
use reticula_formula::parser::parse_formula;

use crate::xlfn::{contains_unknown_function, strip_xlfn_prefixes};
use crate::xml_scan::{ScannedCellMeta, ScannedSheet, scan_workbook_xml, validate_zip_limits};
use crate::{EXCEL_MAX_COLS, EXCEL_MAX_ROWS, XlsxError, XlsxImportReport, XlsxImportResult};

const DAYS_1900_TO_1904: f64 = 1462.0;

fn map_calamine_error(err: &CellErrorType) -> CalcError {
    match err {
        CellErrorType::Div0 => CalcError::Div0,
        CellErrorType::NA => CalcError::Na,
        CellErrorType::Name => CalcError::Name,
        CellErrorType::Null => CalcError::Null,
        CellErrorType::Num => CalcError::Num,
        CellErrorType::Ref => CalcError::Ref,
        CellErrorType::Value | CellErrorType::GettingData => CalcError::Value,
    }
}

fn adjust_serial_for_1904(val: f64, is_1904: bool, is_date_fmt: bool) -> f64 {
    if is_1904 && is_date_fmt && val.is_finite() {
        val + DAYS_1900_TO_1904
    } else {
        val
    }
}

fn canonical_number(n: f64) -> Value {
    if !n.is_finite() {
        Value::Error(CalcError::Num)
    } else if n == 0.0 {
        Value::Number(0.0)
    } else {
        Value::Number(n)
    }
}

fn clamp_cell_text(text: &str) -> String {
    let mut utf16_len = 0usize;
    let mut end_byte = 0usize;
    for ch in text.chars() {
        let units = ch.len_utf16();
        if utf16_len.saturating_add(units) > MAX_CELL_TEXT_UTF16 {
            break;
        }
        utf16_len = utf16_len.saturating_add(units);
        end_byte = end_byte.saturating_add(ch.len_utf8());
    }
    text.get(..end_byte).unwrap_or(text).to_owned()
}

fn insert_scalar_value(sheet: &mut Sheet, row_id: RowId, col_id: ColId, scalar: &ScalarValue) {
    let val = match scalar {
        ScalarValue::Number(n) => canonical_number(*n),
        ScalarValue::Text(s) => {
            let trimmed = clamp_cell_text(s);
            let Some(idx) = sheet.add_string(&trimmed) else {
                return;
            };
            Value::Text(idx)
        }
        ScalarValue::Bool(b) => Value::Bool(*b),
        ScalarValue::Error(e) => Value::Error(*e),
    };
    let _ = sheet.set_value_by_id(row_id, col_id, val);
}

fn sanitize_imported_sheet_name(raw: &str, idx: usize, used: &mut BTreeSet<String>) -> String {
    let base = if is_valid_sheet_name(raw) && !used.contains(raw) {
        raw.to_owned()
    } else {
        let mut cleaned: String = raw
            .chars()
            .filter(|c| !matches!(c, ':' | '\\' | '/' | '?' | '*' | '[' | ']'))
            .take(31)
            .collect();
        cleaned = cleaned.trim_matches('\'').to_owned();
        if cleaned.is_empty() || !is_valid_sheet_name(&cleaned) || used.contains(&cleaned) {
            cleaned = format!("Sheet{}", idx.saturating_add(1));
            let mut n = 1usize;
            while used.contains(&cleaned) {
                n = n.saturating_add(1);
                cleaned = format!("Sheet{}_{n}", idx.saturating_add(1));
            }
        }
        cleaned
    };
    used.insert(base.clone());
    base
}

fn resolve_raw_formula(
    meta: Option<&ScannedCellMeta>,
    calamine_formulas: &BTreeMap<(u32, u32), String>,
    coord: (u32, u32),
) -> Option<String> {
    if let Some(m) = meta
        && let Some(text) = &m.formula_text
        && !text.trim().is_empty()
    {
        return Some(text.trim().to_owned());
    }
    calamine_formulas
        .get(&coord)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn collect_calamine_formulas(
    wb: &mut Xlsx<Cursor<&[u8]>>,
    sheet_name: &str,
) -> BTreeMap<(u32, u32), String> {
    let mut map = BTreeMap::new();
    let Ok(range): Result<Range<String>, _> = wb.worksheet_formula(sheet_name) else {
        return map;
    };
    let Some((start_r, start_c)) = range.start() else {
        return map;
    };
    for (rel_r, row_slice) in range.rows().enumerate() {
        for (rel_c, f) in row_slice.iter().enumerate() {
            if !f.trim().is_empty() {
                let r = start_r.saturating_add(u32::try_from(rel_r).unwrap_or(u32::MAX));
                let c = start_c.saturating_add(u32::try_from(rel_c).unwrap_or(u32::MAX));
                map.insert((r, c), f.clone());
            }
        }
    }
    map
}

fn collect_sheet_metas(sheet_scan: &ScannedSheet) -> BTreeMap<(u32, u32), &ScannedCellMeta> {
    let mut out = BTreeMap::new();
    for m in &sheet_scan.cell_metas {
        out.insert((m.row, m.col), m);
    }
    out
}

/// Imports an `.xlsx` byte slice into a raw [`Workbook`] and structured
/// [`XlsxImportReport`] without instantiating [`FormulaEngine`] (used by
/// `reticula-xlsx-wasm` to defer recalculation cleanly to the main worker).
#[expect(
    clippy::too_many_lines,
    reason = "sequential workbook/styles/scalars/formulas/named-ranges pipeline"
)]
pub fn import_xlsx_workbook(
    bytes: &[u8],
) -> Result<(Workbook<BoundFormula>, XlsxImportReport), XlsxError> {
    validate_zip_limits(bytes)?;
    let scanned = scan_workbook_xml(bytes)?;
    if scanned.sheets.is_empty() {
        return Err(XlsxError::Xml(String::from("workbook has zero worksheets")));
    }
    if scanned.sheets.len() > MAX_SHEETS {
        return Err(XlsxError::Workbook(format!(
            "worksheet count {} exceeds maximum {MAX_SHEETS}",
            scanned.sheets.len()
        )));
    }

    let mut calamine_wb: Xlsx<Cursor<&[u8]>> =
        open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(bytes))
            .map_err(|e| XlsxError::Calamine(e.to_string()))?;
    let is_1904 = scanned.is_1904 || calamine_wb.has_1904_epoch();

    let mut report = XlsxImportReport {
        sheet_count: u32::try_from(scanned.sheets.len()).unwrap_or(u32::MAX),
        unsupported: scanned.unsupported.clone(),
        ..XlsxImportReport::default()
    };

    let mut used_names = BTreeSet::new();
    let mut sheets = Vec::with_capacity(scanned.sheets.len());
    let mut sheet_ids = Vec::with_capacity(scanned.sheets.len());
    let mut per_sheet_calamine_formulas = Vec::with_capacity(scanned.sheets.len());

    // First pass: construct sheets, apply layout & styles, collect expanded shared formulas.
    for (idx, scanned_sheet) in scanned.sheets.iter().enumerate() {
        let sheet_id = SheetId(u32::try_from(idx).unwrap_or(u32::MAX));
        let final_name = sanitize_imported_sheet_name(&scanned_sheet.name, idx, &mut used_names);
        let mut sheet = Sheet::standard(sheet_id, final_name);

        sheet.set_freeze(scanned_sheet.frozen_rows, scanned_sheet.frozen_cols);
        for (&col_pos, &px) in &scanned_sheet.col_widths_px {
            if col_pos < EXCEL_MAX_COLS {
                sheet.set_col_width_by_id(ColId(u64::from(col_pos)), Some(px));
            }
        }
        for (&row_pos, &px) in &scanned_sheet.row_heights_px {
            if row_pos < EXCEL_MAX_ROWS {
                sheet.set_row_height_by_id(RowId(u64::from(row_pos)), Some(px));
            }
        }
        for m in &scanned_sheet.cell_metas {
            if m.row >= EXCEL_MAX_ROWS || m.col >= EXCEL_MAX_COLS {
                continue;
            }
            if let Some(xf_idx) = m.xf_idx
                && let Some(xf) = usize::try_from(xf_idx)
                    .ok()
                    .and_then(|i| scanned.xfs.get(i))
            {
                let row_id = RowId(u64::from(m.row));
                let col_id = ColId(u64::from(m.col));
                if let Some(fmt) = xf.number_format {
                    sheet.set_cell_format_by_id(row_id, col_id, Some(fmt));
                }
                if !xf.style_patch.is_empty() {
                    sheet.set_cell_style_patch(row_id, col_id, xf.style_patch);
                }
            }
        }

        let cal_formulas = collect_calamine_formulas(&mut calamine_wb, &scanned_sheet.name);
        per_sheet_calamine_formulas.push(cal_formulas);
        sheet_ids.push(sheet_id);
        sheets.push(sheet);
    }

    // Build a temporary workbook so `bind_expr` can resolve sheet names while binding formulas.
    let next_sheet_id = u32::try_from(sheets.len()).unwrap_or(u32::MAX);
    let binding_wb: Workbook<BoundFormula> = Workbook::from_parts_with_names(
        sheets.clone(),
        BTreeMap::new(),
        BTreeMap::new(),
        IdAllocator::local(),
        next_sheet_id,
        None,
    )
    .map_err(|e| XlsxError::Workbook(format!("{e:?}")))?;

    // Second pass: bind formulas first so we know which dynamic-array spill
    // rectangles are executable (their static cached shadow cells must stay empty).
    let mut formulas: BTreeMap<WorkbookCellKey, BoundFormula> = BTreeMap::new();
    let mut per_sheet_formula_coords: Vec<BTreeSet<(u32, u32)>> =
        vec![BTreeSet::new(); sheets.len()];
    let mut per_sheet_shadow_skips: Vec<BTreeSet<(u32, u32)>> = vec![BTreeSet::new(); sheets.len()];

    for (idx, scanned_sheet) in scanned.sheets.iter().enumerate() {
        let sheet_id = *sheet_ids
            .get(idx)
            .ok_or_else(|| XlsxError::SheetNotFound(scanned_sheet.name.clone()))?;
        let cal_formulas = per_sheet_calamine_formulas
            .get(idx)
            .ok_or_else(|| XlsxError::SheetNotFound(scanned_sheet.name.clone()))?;
        let meta_map = collect_sheet_metas(scanned_sheet);

        let mut candidate_coords: BTreeSet<(u32, u32)> = cal_formulas.keys().copied().collect();
        for (&coord, m) in &meta_map {
            if m.formula_text.is_some() {
                candidate_coords.insert(coord);
            }
        }

        for coord @ (row, col) in candidate_coords {
            let meta = meta_map.get(&coord).copied();
            let Some(raw_no_eq) = resolve_raw_formula(meta, cal_formulas, coord) else {
                continue;
            };
            if row >= EXCEL_MAX_ROWS || col >= EXCEL_MAX_COLS {
                report.truncated_cells = report.truncated_cells.saturating_add(1);
                continue;
            }
            report.formula_count = report.formula_count.saturating_add(1);
            if let Some(fset) = per_sheet_formula_coords.get_mut(idx) {
                fset.insert(coord);
            }

            let is_xldapr_cm = meta
                .is_some_and(|m| m.cm_idx != 0 && scanned.xldapr_cm_indices.contains(&m.cm_idx));
            let is_array_tag = meta.is_some_and(|m| m.formula_type.as_deref() == Some("array"));
            let mode = if is_array_tag && !is_xldapr_cm && scanned.has_metadata_xldapr {
                FormulaMode::Legacy
            } else {
                FormulaMode::Dynamic
            };

            let raw_with_eq = if raw_no_eq.starts_with('=') {
                raw_no_eq.clone()
            } else {
                format!("={raw_no_eq}")
            };
            let stripped =
                strip_xlfn_prefixes(raw_with_eq.strip_prefix('=').unwrap_or(&raw_with_eq));
            let stripped_with_eq = format!("={stripped}");

            let bound = match parse_formula(&stripped_with_eq) {
                Ok(ast) if !contains_unknown_function(&ast) => {
                    if mode == FormulaMode::Dynamic
                        && (is_xldapr_cm || is_array_tag)
                        && let Some(m) = meta
                        && let Some((r1, c1, r2, c2)) = m.formula_ref
                        && let Some(skips) = per_sheet_shadow_skips.get_mut(idx)
                    {
                        for sr in r1..=r2 {
                            for sc in c1..=c2 {
                                if (sr, sc) != (row, col) {
                                    skips.insert((sr, sc));
                                }
                            }
                        }
                    }
                    BoundFormula {
                        mode,
                        root: bind_expr(&ast, &binding_wb, sheet_id),
                    }
                }
                Ok(ast) => {
                    if let Some(cached) = meta.and_then(|m| m.cached_value.clone()) {
                        report.preserved_formula_count =
                            report.preserved_formula_count.saturating_add(1);
                        BoundFormula {
                            mode,
                            root: BoundExpr::PreservedCached {
                                raw: raw_with_eq,
                                cached,
                            },
                        }
                    } else {
                        BoundFormula {
                            mode,
                            root: bind_expr(&ast, &binding_wb, sheet_id),
                        }
                    }
                }
                Err(_) => {
                    report.preserved_formula_count =
                        report.preserved_formula_count.saturating_add(1);
                    if let Some(cached) = meta.and_then(|m| m.cached_value.clone()) {
                        BoundFormula {
                            mode,
                            root: BoundExpr::PreservedCached {
                                raw: raw_with_eq,
                                cached,
                            },
                        }
                    } else {
                        BoundFormula {
                            mode,
                            root: BoundExpr::Opaque { raw: raw_with_eq },
                        }
                    }
                }
            };

            let key = (sheet_id, RowId(u64::from(row)), ColId(u64::from(col)));
            formulas.insert(key, bound);
        }
    }

    // Third pass: populate static scalar cell values on each sheet.
    for (idx, scanned_sheet) in scanned.sheets.iter().enumerate() {
        let Some(sheet) = sheets.get_mut(idx) else {
            continue;
        };
        let formula_coords = per_sheet_formula_coords
            .get(idx)
            .cloned()
            .unwrap_or_default();
        let shadow_skips = per_sheet_shadow_skips.get(idx).cloned().unwrap_or_default();
        let meta_map = collect_sheet_metas(scanned_sheet);

        let Ok(range) = calamine_wb.worksheet_range(&scanned_sheet.name) else {
            continue;
        };
        let Some((start_r, start_c)) = range.start() else {
            continue;
        };

        let mut written_static: BTreeSet<(u32, u32)> = BTreeSet::new();

        for (rel_r, row_slice) in range.rows().enumerate() {
            for (rel_c, data) in row_slice.iter().enumerate() {
                if matches!(data, Data::Empty) {
                    continue;
                }
                let row = start_r.saturating_add(u32::try_from(rel_r).unwrap_or(u32::MAX));
                let col = start_c.saturating_add(u32::try_from(rel_c).unwrap_or(u32::MAX));
                if row >= EXCEL_MAX_ROWS || col >= EXCEL_MAX_COLS {
                    report.truncated_cells = report.truncated_cells.saturating_add(1);
                    continue;
                }
                if shadow_skips.contains(&(row, col)) {
                    continue;
                }
                let is_formula = formula_coords.contains(&(row, col));

                let meta = meta_map.get(&(row, col)).copied();
                let is_date_fmt = meta
                    .and_then(|m| m.xf_idx)
                    .and_then(|xf| usize::try_from(xf).ok())
                    .and_then(|xf| scanned.xfs.get(xf))
                    .is_some_and(|xf| xf.is_date_fmt);

                let row_id = RowId(u64::from(row));
                let col_id = ColId(u64::from(col));
                match data {
                    Data::Empty => continue,
                    Data::Int(i) => {
                        let _ = sheet.set_value_by_id(row_id, col_id, canonical_number(*i as f64));
                    }
                    Data::Float(f) => {
                        let adj = adjust_serial_for_1904(*f, is_1904, is_date_fmt);
                        let _ = sheet.set_value_by_id(row_id, col_id, canonical_number(adj));
                    }
                    Data::DateTime(dt) => {
                        let adj = adjust_serial_for_1904(dt.as_f64(), is_1904, true);
                        let _ = sheet.set_value_by_id(row_id, col_id, canonical_number(adj));
                    }
                    Data::String(s) => {
                        let trimmed = clamp_cell_text(s);
                        if let Some(str_idx) = sheet.add_string(&trimmed) {
                            let _ = sheet.set_value_by_id(row_id, col_id, Value::Text(str_idx));
                        }
                    }
                    Data::Bool(b) => {
                        let _ = sheet.set_value_by_id(row_id, col_id, Value::Bool(*b));
                    }
                    Data::Error(e) => {
                        let resolved = match meta.and_then(|m| m.cached_value.as_ref()) {
                            Some(ScalarValue::Error(exact)) => *exact,
                            _ => map_calamine_error(e),
                        };
                        let _ = sheet.set_value_by_id(row_id, col_id, Value::Error(resolved));
                    }
                    Data::DateTimeIso(s) | Data::DurationIso(s) => {
                        let trimmed = clamp_cell_text(s);
                        if let Some(str_idx) = sheet.add_string(&trimmed) {
                            let _ = sheet.set_value_by_id(row_id, col_id, Value::Text(str_idx));
                        }
                    }
                }
                written_static.insert((row, col));
                if !is_formula {
                    report.cell_count = report.cell_count.saturating_add(1);
                }
            }
        }

        // Supplement cached values missed by calamine (e.g. `#SPILL!` / `#CALC!`).
        for m in &scanned_sheet.cell_metas {
            if m.row >= EXCEL_MAX_ROWS
                || m.col >= EXCEL_MAX_COLS
                || shadow_skips.contains(&(m.row, m.col))
                || written_static.contains(&(m.row, m.col))
            {
                continue;
            }
            if let Some(cached) = &m.cached_value {
                insert_scalar_value(
                    sheet,
                    RowId(u64::from(m.row)),
                    ColId(u64::from(m.col)),
                    cached,
                );
                if !formula_coords.contains(&(m.row, m.col)) {
                    report.cell_count = report.cell_count.saturating_add(1);
                }
            }
        }
    }

    // Fourth pass: bind workbook-level and sheet-local named ranges.
    let mut named_ranges: BTreeMap<NamedRangeKey, NamedRangeDef<BoundFormula>> = BTreeMap::new();
    for dn in &scanned.defined_names {
        if named_ranges.len() >= MAX_NAMED_RANGES {
            break;
        }
        if dn.name.starts_with("_xlnm.") || !is_valid_named_range_name(&dn.name) {
            continue;
        }
        let scope = dn.local_sheet_idx.and_then(|i| sheet_ids.get(i).copied());
        if dn.local_sheet_idx.is_some() && scope.is_none() {
            continue;
        }
        let Some(&default_sid) = sheet_ids.first() else {
            continue;
        };
        let host_sheet = scope.unwrap_or(default_sid);
        let body = dn.refers_to.strip_prefix('=').unwrap_or(&dn.refers_to);
        let stripped = strip_xlfn_prefixes(body);
        let with_eq = format!("={stripped}");
        let formula = match parse_formula(&with_eq) {
            Ok(ast) if !contains_unknown_function(&ast) => BoundFormula {
                mode: FormulaMode::Dynamic,
                root: bind_expr(&ast, &binding_wb, host_sheet),
            },
            _ => BoundFormula {
                mode: FormulaMode::Dynamic,
                root: BoundExpr::Opaque {
                    raw: format!("={body}"),
                },
            },
        };
        let comment = dn
            .comment
            .as_deref()
            .map(|c| c.chars().take(MAX_NAMED_RANGE_COMMENT_CHARS).collect());
        let key = NamedRangeKey::new(scope, &dn.name);
        named_ranges.insert(
            key,
            NamedRangeDef {
                name: dn.name.clone(),
                formula,
                comment,
                hidden: false,
            },
        );
    }

    let wb = Workbook::from_parts_with_names(
        sheets,
        formulas,
        named_ranges,
        IdAllocator::local(),
        next_sheet_id,
        None,
    )
    .map_err(|e| XlsxError::Workbook(format!("{e:?}")))?;

    Ok((wb, report))
}

/// Imports an `.xlsx` byte slice into a recalculated [`FormulaEngine`] and
/// structured [`XlsxImportReport`].
pub fn import_xlsx(bytes: &[u8]) -> Result<XlsxImportResult, XlsxError> {
    let (wb, report) = import_xlsx_workbook(bytes)?;
    let engine = FormulaEngine::from_workbook_recalc_all(wb);
    Ok(XlsxImportResult { engine, report })
}
