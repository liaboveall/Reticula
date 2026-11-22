//! `.xlsx` generator backed by `rust_xlsxwriter` with full formatting,
//! dynamic array spills, `_xlfn.` restoration, named ranges, Excel limit
//! truncation warnings, and optional `<v>` cache stripping for Step 6
//! differential recalculation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Cursor, Read, Write as _};

use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::events::{BytesStart, Event};
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{FormulaMode, ScalarValue, Workbook};
use reticula_core::sheet::{DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT, Sheet};
use reticula_core::style::{BorderWeight, CellStyle, ColorValue, HorizontalAlign};
use reticula_core::value::{CalcError, Value};
use reticula_formula::binder::{BoundExpr, BoundFormula};
use reticula_formula::engine::FormulaEngine;
use reticula_formula::spill::SpillRegistry;
use rust_xlsxwriter::{
    Color, Format, FormatAlign, FormatBorder, Formula, Workbook as WriterWorkbook, Worksheet,
};
use zip::ZipArchive;
use zip::write::SimpleFileOptions;

use crate::xlfn::add_xlfn_prefixes;
use crate::{
    EXCEL_MAX_COLS, EXCEL_MAX_ROWS, ExportOptions, XlsxError, XlsxExportResult, XlsxExportWarning,
};

const PALETTE_LIGHT_RGB: [u32; 8] = [
    0x0F172A, 0x2563EB, 0x16A34A, 0xD97706, 0xDC2626, 0x7C3AED, 0x0891B2, 0x64748B,
];

fn color_to_xl(c: ColorValue) -> Color {
    match c {
        ColorValue::Rgb(rgb) => Color::RGB(rgb & 0x00FF_FFFF),
        ColorValue::Palette(slot) => {
            let rgb = PALETTE_LIGHT_RGB
                .get(usize::from(slot))
                .copied()
                .unwrap_or(0x0F172A);
            Color::RGB(rgb)
        }
    }
}

fn border_to_xl(w: BorderWeight) -> FormatBorder {
    match w {
        BorderWeight::None => FormatBorder::None,
        BorderWeight::Thin => FormatBorder::Thin,
        BorderWeight::Medium => FormatBorder::Medium,
    }
}

/// Maps a kernel [`NumberFormat`] to an OOXML Excel format string.
#[must_use]
pub(crate) fn number_format_to_excel(fmt: NumberFormat) -> Option<String> {
    let norm = fmt.normalize();
    match norm {
        NumberFormat::General => None,
        NumberFormat::Fixed2 => Some(String::from("0.00")),
        NumberFormat::Percent => Some(String::from("0%")),
        NumberFormat::Date => Some(String::from("yyyy-mm-dd")),
        NumberFormat::Integer => Some(String::from("0")),
        NumberFormat::Fixed {
            decimals,
            thousands,
        } => {
            let prefix = if thousands { "#,##0" } else { "0" };
            if decimals == 0 {
                Some(String::from(prefix))
            } else {
                Some(format!("{prefix}.{}", "0".repeat(usize::from(decimals))))
            }
        }
        NumberFormat::Currency { symbol, decimals } => {
            let frac = if decimals == 0 {
                String::new()
            } else {
                format!(".{}", "0".repeat(usize::from(decimals)))
            };
            Some(format!("{}#,##0{frac}", symbol.as_str()))
        }
        NumberFormat::PercentDecimals { decimals } => {
            if decimals == 0 {
                Some(String::from("0%"))
            } else {
                Some(format!("0.{}%", "0".repeat(usize::from(decimals))))
            }
        }
        NumberFormat::Scientific { decimals } => {
            let d = decimals.max(1);
            Some(format!("0.{}E+00", "0".repeat(usize::from(d))))
        }
        NumberFormat::DateSlash => Some(String::from("yyyy/mm/dd")),
        NumberFormat::DateUs => Some(String::from("mm/dd/yyyy")),
        NumberFormat::DateYearMonth => Some(String::from("yyyy-mm")),
        NumberFormat::DateTime { seconds } => Some(if seconds {
            String::from("yyyy-mm-dd hh:mm:ss")
        } else {
            String::from("yyyy-mm-dd hh:mm")
        }),
        NumberFormat::Time { seconds } => Some(if seconds {
            String::from("hh:mm:ss")
        } else {
            String::from("hh:mm")
        }),
        NumberFormat::PlainText => Some(String::from("@")),
    }
}

fn build_xl_format(num_fmt: NumberFormat, style: CellStyle) -> Format {
    let mut fmt = Format::new();
    if let Some(code) = number_format_to_excel(num_fmt.normalize()) {
        fmt = fmt.set_num_format(code);
    }
    if style.bold {
        fmt = fmt.set_bold();
    }
    if style.italic {
        fmt = fmt.set_italic();
    }
    if style.strikethrough {
        fmt = fmt.set_font_strikethrough();
    }
    match style.align {
        HorizontalAlign::Auto => {}
        HorizontalAlign::Left => fmt = fmt.set_align(FormatAlign::Left),
        HorizontalAlign::Center => fmt = fmt.set_align(FormatAlign::Center),
        HorizontalAlign::Right => fmt = fmt.set_align(FormatAlign::Right),
    }
    if let Some(c) = style.text_color {
        fmt = fmt.set_font_color(color_to_xl(c));
    }
    if let Some(c) = style.fill_color {
        fmt = fmt.set_background_color(color_to_xl(c));
    }
    if style.borders.top != BorderWeight::None {
        fmt = fmt.set_border_top(border_to_xl(style.borders.top));
    }
    if style.borders.right != BorderWeight::None {
        fmt = fmt.set_border_right(border_to_xl(style.borders.right));
    }
    if style.borders.bottom != BorderWeight::None {
        fmt = fmt.set_border_bottom(border_to_xl(style.borders.bottom));
    }
    if style.borders.left != BorderWeight::None {
        fmt = fmt.set_border_left(border_to_xl(style.borders.left));
    }
    fmt
}

fn is_default_cell_appearance(num_fmt: NumberFormat, style: CellStyle) -> bool {
    num_fmt.normalize() == NumberFormat::General && style == CellStyle::default()
}

fn cached_scalar_to_result_str(
    sheet: &Sheet,
    row_id: RowId,
    col_id: ColId,
    formula: &BoundFormula,
) -> Option<String> {
    if let BoundExpr::PreservedCached { cached, .. } = &formula.root {
        return Some(match cached {
            ScalarValue::Number(n) => {
                if !n.is_finite() {
                    String::from("#NUM!")
                } else if *n == 0.0 {
                    String::from("0")
                } else {
                    format!("{n}")
                }
            }
            ScalarValue::Text(s) => s.clone(),
            ScalarValue::Bool(b) => String::from(if *b { "TRUE" } else { "FALSE" }),
            ScalarValue::Error(e) => String::from(e.as_str()),
        });
    }
    match sheet.value_by_id(row_id, col_id) {
        Value::Empty | Value::Array(_) => None,
        Value::Number(n) => Some(if !n.is_finite() {
            String::from("#NUM!")
        } else if n == 0.0 {
            String::from("0")
        } else {
            format!("{n}")
        }),
        Value::Text(idx) => Some(sheet.strings().get(idx).to_owned()),
        Value::Bool(b) => Some(String::from(if b { "TRUE" } else { "FALSE" })),
        Value::Error(e) => Some(String::from(e.as_str())),
    }
}

fn format_cache_lookup(
    cache: &mut HashMap<(NumberFormat, CellStyle), Format>,
    num_fmt: NumberFormat,
    style: CellStyle,
) -> &Format {
    let key = (num_fmt.normalize(), style);
    cache
        .entry(key)
        .or_insert_with(|| build_xl_format(key.0, key.1))
}

fn write_scalar_cell(
    ws: &mut Worksheet,
    row: u32,
    col: u16,
    sheet: &Sheet,
    val: Value,
    fmt: Option<&Format>,
) -> Result<(), XlsxError> {
    let map_err = |e: rust_xlsxwriter::XlsxError| XlsxError::Write(e.to_string());
    match val {
        Value::Empty | Value::Array(_) => {
            if let Some(f) = fmt {
                ws.write_blank(row, col, f).map_err(map_err)?;
            }
        }
        Value::Number(n) => {
            if !n.is_finite() {
                let f_obj =
                    Formula::new(CalcError::Num.as_str()).set_result(CalcError::Num.as_str());
                match fmt {
                    Some(f) => ws
                        .write_formula_with_format(row, col, f_obj, f)
                        .map_err(map_err)?,
                    None => ws.write_formula(row, col, f_obj).map_err(map_err)?,
                };
            } else {
                let clean = if n == 0.0 { 0.0 } else { n };
                match fmt {
                    Some(f) => ws
                        .write_number_with_format(row, col, clean, f)
                        .map_err(map_err)?,
                    None => ws.write_number(row, col, clean).map_err(map_err)?,
                };
            }
        }
        Value::Text(idx) => {
            let text = sheet.strings().get(idx);
            match fmt {
                Some(f) => ws
                    .write_string_with_format(row, col, text, f)
                    .map_err(map_err)?,
                None => ws.write_string(row, col, text).map_err(map_err)?,
            };
        }
        Value::Bool(b) => {
            match fmt {
                Some(f) => ws
                    .write_boolean_with_format(row, col, b, f)
                    .map_err(map_err)?,
                None => ws.write_boolean(row, col, b).map_err(map_err)?,
            };
        }
        Value::Error(e) => {
            let f_obj = Formula::new(e.as_str()).set_result(e.as_str());
            match fmt {
                Some(f) => ws
                    .write_formula_with_format(row, col, f_obj, f)
                    .map_err(map_err)?,
                None => ws.write_formula(row, col, f_obj).map_err(map_err)?,
            };
        }
    }
    Ok(())
}

fn is_dynamic_array_fn_name(name: &str) -> bool {
    matches!(
        name,
        "SEQUENCE"
            | "FILTER"
            | "SORT"
            | "SORTBY"
            | "UNIQUE"
            | "RANDARRAY"
            | "HSTACK"
            | "VSTACK"
            | "TOCOL"
            | "TOROW"
            | "WRAPROWS"
            | "WRAPCOLS"
            | "TAKE"
            | "DROP"
            | "CHOOSEROWS"
            | "CHOOSECOLS"
            | "EXPAND"
    )
}

fn expr_uses_dynamic_array_fn(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Call { name, args } => {
            is_dynamic_array_fn_name(name) || args.iter().any(expr_uses_dynamic_array_fn)
        }
        BoundExpr::SpillRef(inner)
        | BoundExpr::Paren(inner)
        | BoundExpr::Unary { expr: inner, .. } => expr_uses_dynamic_array_fn(inner),
        BoundExpr::Binary { left, right, .. } => {
            expr_uses_dynamic_array_fn(left) || expr_uses_dynamic_array_fn(right)
        }
        BoundExpr::Literal(_)
        | BoundExpr::ArrayLiteral { .. }
        | BoundExpr::Reference(_)
        | BoundExpr::Name(_)
        | BoundExpr::ScopedName { .. }
        | BoundExpr::Opaque { .. }
        | BoundExpr::PreservedCached { .. }
        | BoundExpr::MissingArg => false,
    }
}

/// Exports `engine` using explicit [`ExportOptions`].
pub fn export_xlsx_with_options(
    engine: &FormulaEngine,
    options: ExportOptions,
) -> Result<XlsxExportResult, XlsxError> {
    export_workbook_with_options(engine.workbook(), Some(engine.spills()), options)
}

/// Exports a raw [`Workbook`] (with optional materialized [`SpillRegistry`])
/// without requiring a [`FormulaEngine`] instance.
#[expect(
    clippy::too_many_lines,
    reason = "sequential sheet, cell, formula, style, named-range, and post-pass export"
)]
pub fn export_workbook_with_options(
    wb: &Workbook<BoundFormula>,
    spills: Option<&SpillRegistry>,
    options: ExportOptions,
) -> Result<XlsxExportResult, XlsxError> {
    let mut writer = WriterWorkbook::new();
    let mut format_cache: HashMap<(NumberFormat, CellStyle), Format> = HashMap::new();
    let mut warnings = Vec::new();
    let mut truncated_count = 0_u32;

    let mut live_sheets: Vec<&Sheet> = Vec::new();
    let mut sheet_names_by_id: BTreeMap<SheetId, String> = BTreeMap::new();
    for s in wb.sheets() {
        if !s.is_deleted() {
            live_sheets.push(s);
            sheet_names_by_id.insert(s.id(), s.name().to_owned());
        }
    }

    type SheetFormulaList<'a> = Vec<(u32, u32, RowId, ColId, &'a BoundFormula)>;
    let mut per_sheet_formulas: BTreeMap<SheetId, SheetFormulaList<'_>> = BTreeMap::new();
    for (&(sid, row_id, col_id), formula) in wb.formulas() {
        let Some(sheet) = wb.sheet(sid) else {
            continue;
        };
        if sheet.is_deleted() {
            continue;
        }
        let Some(r_pos) = sheet.row_axis().id_to_pos(row_id.0) else {
            continue;
        };
        let Some(c_pos) = sheet.col_axis().id_to_pos(col_id.0) else {
            continue;
        };
        if r_pos >= EXCEL_MAX_ROWS || c_pos >= EXCEL_MAX_COLS {
            truncated_count = truncated_count.saturating_add(1);
            continue;
        }
        per_sheet_formulas
            .entry(sid)
            .or_default()
            .push((r_pos, c_pos, row_id, col_id, formula));
    }

    for sheet in &live_sheets {
        let sid = sheet.id();
        let ws = writer.add_worksheet();
        ws.set_name(sheet.name())
            .map_err(|e| XlsxError::Write(e.to_string()))?;

        if sheet.frozen_rows() > 0 || sheet.frozen_cols() > 0 {
            let fr = sheet.frozen_rows();
            let fc = u16::try_from(sheet.frozen_cols()).unwrap_or(u16::MAX);
            ws.set_freeze_panes(fr, fc)
                .map_err(|e| XlsxError::Write(e.to_string()))?;
        }

        for (r_pos, px) in sheet.visible_custom_row_heights() {
            if r_pos < EXCEL_MAX_ROWS && px != DEFAULT_ROW_HEIGHT {
                ws.set_row_height_pixels(r_pos, u32::from(px))
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
            }
        }
        for (c_pos, px) in sheet.visible_custom_col_widths() {
            if c_pos < EXCEL_MAX_COLS
                && px != DEFAULT_COL_WIDTH
                && let Ok(col_u16) = u16::try_from(c_pos)
            {
                ws.set_column_width_pixels(col_u16, u32::from(px))
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
            }
        }

        let mut formula_coords: BTreeMap<(u32, u32), ()> = BTreeMap::new();
        let mut written_coords: BTreeMap<(u32, u32), ()> = BTreeMap::new();

        if let Some(sheet_formulas) = per_sheet_formulas.get(&sid) {
            for &(r_pos, c_pos, row_id, col_id, formula) in sheet_formulas {
                formula_coords.insert((r_pos, c_pos), ());
                written_coords.insert((r_pos, c_pos), ());

                let raw = formula.unparse(wb, sid);
                let body = raw.strip_prefix('=').unwrap_or(&raw);
                let prefixed = add_xlfn_prefixes(body);
                let mut f_obj = Formula::new(&prefixed);
                if !options.omit_cached_values
                    && let Some(res) = cached_scalar_to_result_str(sheet, row_id, col_id, formula)
                {
                    f_obj = f_obj.set_result(res);
                }

                let num_fmt = sheet.effective_format_by_id(row_id, col_id);
                let cell_style = sheet.effective_style_by_id(row_id, col_id);
                let has_fmt = !is_default_cell_appearance(num_fmt, cell_style);
                let col_u16 = u16::try_from(c_pos).unwrap_or(u16::MAX);

                let spill_shape = if formula.mode == FormulaMode::Dynamic {
                    spills.and_then(|reg| reg.spill_shape((sid, row_id, col_id)))
                } else {
                    None
                };

                if let Some((sp_rows, sp_cols)) = spill_shape
                    && (sp_rows > 1 || sp_cols > 1)
                {
                    let last_r = r_pos
                        .saturating_add(sp_rows.saturating_sub(1))
                        .min(EXCEL_MAX_ROWS - 1);
                    let last_c = c_pos
                        .saturating_add(sp_cols.saturating_sub(1))
                        .min(EXCEL_MAX_COLS - 1);
                    let last_c_u16 = u16::try_from(last_c).unwrap_or(u16::MAX);
                    if has_fmt {
                        let xl_fmt = format_cache_lookup(&mut format_cache, num_fmt, cell_style);
                        ws.write_dynamic_array_formula_with_format(
                            r_pos, col_u16, last_r, last_c_u16, f_obj, xl_fmt,
                        )
                        .map_err(|e| XlsxError::Write(e.to_string()))?;
                    } else {
                        ws.write_dynamic_array_formula(r_pos, col_u16, last_r, last_c_u16, f_obj)
                            .map_err(|e| XlsxError::Write(e.to_string()))?;
                    }
                } else if spills.is_none()
                    && formula.mode == FormulaMode::Dynamic
                    && expr_uses_dynamic_array_fn(&formula.root)
                {
                    if has_fmt {
                        let xl_fmt = format_cache_lookup(&mut format_cache, num_fmt, cell_style);
                        ws.write_dynamic_array_formula_with_format(
                            r_pos, col_u16, r_pos, col_u16, f_obj, xl_fmt,
                        )
                        .map_err(|e| XlsxError::Write(e.to_string()))?;
                    } else {
                        ws.write_dynamic_array_formula(r_pos, col_u16, r_pos, col_u16, f_obj)
                            .map_err(|e| XlsxError::Write(e.to_string()))?;
                    }
                } else if has_fmt {
                    let xl_fmt = format_cache_lookup(&mut format_cache, num_fmt, cell_style);
                    ws.write_formula_with_format(r_pos, col_u16, f_obj, xl_fmt)
                        .map_err(|e| XlsxError::Write(e.to_string()))?;
                } else {
                    ws.write_formula(r_pos, col_u16, f_obj)
                        .map_err(|e| XlsxError::Write(e.to_string()))?;
                }
            }
        }

        let mut cell_write_err: Option<XlsxError> = None;
        sheet.for_each_non_empty_cell(|r_pos, c_pos, row_id, col_id, val| {
            if cell_write_err.is_some() {
                return;
            }
            if r_pos >= EXCEL_MAX_ROWS || c_pos >= EXCEL_MAX_COLS {
                truncated_count = truncated_count.saturating_add(1);
                return;
            }
            if formula_coords.contains_key(&(r_pos, c_pos))
                || spills.is_some_and(|reg| reg.anchor_of_cell((sid, row_id, col_id)).is_some())
            {
                return;
            }
            written_coords.insert((r_pos, c_pos), ());
            let num_fmt = sheet.effective_format_by_id(row_id, col_id);
            let cell_style = sheet.effective_style_by_id(row_id, col_id);
            let col_u16 = u16::try_from(c_pos).unwrap_or(u16::MAX);
            if !is_default_cell_appearance(num_fmt, cell_style) {
                let xl_fmt = format_cache_lookup(&mut format_cache, num_fmt, cell_style);
                if let Err(e) = write_scalar_cell(ws, r_pos, col_u16, sheet, val, Some(xl_fmt)) {
                    cell_write_err = Some(e);
                }
            } else if let Err(e) = write_scalar_cell(ws, r_pos, col_u16, sheet, val, None) {
                cell_write_err = Some(e);
            }
        });
        if let Some(err) = cell_write_err {
            return Err(err);
        }

        let mut blank_styled: BTreeSet<(RowId, ColId)> = BTreeSet::new();
        for &(r, c) in sheet.cell_styles().keys() {
            blank_styled.insert((r, c));
        }
        for &(r, c) in sheet.cell_formats().keys() {
            blank_styled.insert((r, c));
        }
        for (row_id, col_id) in blank_styled {
            let Some(r_pos) = sheet.row_axis().id_to_pos(row_id.0) else {
                continue;
            };
            let Some(c_pos) = sheet.col_axis().id_to_pos(col_id.0) else {
                continue;
            };
            if r_pos >= EXCEL_MAX_ROWS
                || c_pos >= EXCEL_MAX_COLS
                || written_coords.contains_key(&(r_pos, c_pos))
            {
                continue;
            }
            let num_fmt = sheet.effective_format_by_id(row_id, col_id);
            let cell_style = sheet.effective_style_by_id(row_id, col_id);
            if !is_default_cell_appearance(num_fmt, cell_style)
                && let Ok(col_u16) = u16::try_from(c_pos)
            {
                let xl_fmt = format_cache_lookup(&mut format_cache, num_fmt, cell_style);
                ws.write_blank(r_pos, col_u16, xl_fmt)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
            }
        }
    }

    if let Some(first_sheet) = live_sheets.first() {
        let default_sid = first_sheet.id();
        for (key, def) in wb.named_ranges() {
            if def.name.starts_with("_xlnm.") {
                continue;
            }
            let host_sid = key.scope.unwrap_or(default_sid);
            let raw = def.formula.unparse(wb, host_sid);
            let body = raw.strip_prefix('=').unwrap_or(&raw);
            let prefixed = format!("={}", add_xlfn_prefixes(body));
            let defined_key = if let Some(scope_sid) = key.scope {
                let Some(s_name) = sheet_names_by_id.get(&scope_sid) else {
                    continue;
                };
                let escaped = s_name.replace('\'', "''");
                format!("'{escaped}'!{}", def.name)
            } else {
                def.name.clone()
            };
            writer
                .define_name(&defined_key, &prefixed)
                .map_err(|e| XlsxError::Write(e.to_string()))?;
        }
    }

    let mut bytes = writer
        .save_to_buffer()
        .map_err(|e| XlsxError::Write(e.to_string()))?;

    if options.omit_cached_values || options.full_calc_on_load {
        bytes = post_process_export_zip(&bytes, options)?;
    }

    if truncated_count > 0 {
        warnings.push(XlsxExportWarning {
            code: String::from("EXCEL_BOUNDS_TRUNCATED"),
            message: format!(
                "Skipped {truncated_count} cell(s) exceeding Excel's 1,048,576 x 16,384 worksheet limits"
            ),
            count: truncated_count,
        });
    }

    Ok(XlsxExportResult { bytes, warnings })
}

fn post_process_export_zip(bytes: &[u8], options: ExportOptions) -> Result<Vec<u8>, XlsxError> {
    let mut reader_zip = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
    let mut out_buf = Vec::with_capacity(bytes.len());
    {
        let mut writer_zip = zip::ZipWriter::new(Cursor::new(&mut out_buf));
        let file_opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        for idx in 0..reader_zip.len() {
            let mut entry = reader_zip
                .by_index(idx)
                .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
            let name = entry
                .name()
                .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?
                .into_owned();
            if options.omit_cached_values
                && name.starts_with("xl/worksheets/sheet")
                && name.ends_with(".xml")
            {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| XlsxError::Xml(e.to_string()))?;
                let cleaned = strip_formula_cached_values(&xml)?;
                writer_zip
                    .start_file(&name, file_opts)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                writer_zip
                    .write_all(cleaned.as_bytes())
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
            } else if (options.full_calc_on_load || options.omit_cached_values)
                && name == "xl/workbook.xml"
            {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| XlsxError::Xml(e.to_string()))?;
                let updated = ensure_full_calc_on_load(&xml)?;
                writer_zip
                    .start_file(&name, file_opts)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                writer_zip
                    .write_all(updated.as_bytes())
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
            } else {
                writer_zip
                    .raw_copy_file(entry)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
            }
        }
        writer_zip
            .finish()
            .map_err(|e| XlsxError::Write(e.to_string()))?;
    }
    Ok(out_buf)
}

/// Removes cached `<v>...</v>` nodes from formula cells and drops stale `t`
/// result-type attributes on `<c>` so Calc / Excel always recomputes on load.
pub(crate) fn strip_formula_cached_values(xml: &str) -> Result<String, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::with_capacity(xml.len()));
    let mut cell_events: Option<(BytesStart<'static>, Vec<Event<'static>>)> = None;
    let mut in_v = false;
    let mut cell_has_f = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "c" => {
                cell_events = Some((e.into_owned(), Vec::new()));
                in_v = false;
                cell_has_f = false;
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == "c" => {
                if let Some((orig_start, inner)) = cell_events.take() {
                    let tag_name = orig_start.name();
                    let mut start_elem = BytesStart::new(tag_name.as_ref());
                    for attr in orig_start.attributes().flatten() {
                        if cell_has_f && attr.key.as_ref() == "t" {
                            continue;
                        }
                        start_elem.push_attribute(attr);
                    }
                    writer
                        .write_event(Event::Start(start_elem))
                        .map_err(|err| XlsxError::Xml(err.to_string()))?;
                    for ev in inner {
                        writer
                            .write_event(ev)
                            .map_err(|err| XlsxError::Xml(err.to_string()))?;
                    }
                    writer
                        .write_event(Event::End(e.into_owned()))
                        .map_err(|err| XlsxError::Xml(err.to_string()))?;
                }
                in_v = false;
                cell_has_f = false;
            }
            Ok(ev) if cell_events.is_some() => {
                if let Some((_, ref mut inner)) = cell_events {
                    match &ev {
                        Event::Start(s) | Event::Empty(s) if s.local_name().as_ref() == "f" => {
                            cell_has_f = true;
                            inner.push(ev.into_owned());
                        }
                        Event::Start(s) if s.local_name().as_ref() == "v" && cell_has_f => {
                            in_v = true;
                        }
                        Event::Empty(s) if s.local_name().as_ref() == "v" && cell_has_f => {}
                        Event::End(s) if s.local_name().as_ref() == "v" && in_v => {
                            in_v = false;
                        }
                        _ if in_v => {}
                        _ => inner.push(ev.into_owned()),
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(ev) => {
                writer
                    .write_event(ev)
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
            }
            Err(e) => return Err(XlsxError::Xml(e.to_string())),
        }
    }

    String::from_utf8(writer.into_inner()).map_err(|e| XlsxError::Xml(e.to_string()))
}

/// Ensures `<calcPr ... fullCalcOnLoad="1"/>` is set in `xl/workbook.xml`.
pub(crate) fn ensure_full_calc_on_load(xml: &str) -> Result<String, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::with_capacity(xml.len().saturating_add(64)));
    let mut saw_calc_pr = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "calcPr" => {
                saw_calc_pr = true;
                let mut rebuilt = BytesStart::new("calcPr");
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() != "fullCalcOnLoad" {
                        rebuilt.push_attribute(attr);
                    }
                }
                rebuilt.push_attribute(("fullCalcOnLoad", "1"));
                writer
                    .write_event(Event::Start(rebuilt))
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
            }
            Ok(Event::Empty(e)) if e.local_name().as_ref() == "calcPr" => {
                saw_calc_pr = true;
                let mut rebuilt = BytesStart::new("calcPr");
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() != "fullCalcOnLoad" {
                        rebuilt.push_attribute(attr);
                    }
                }
                rebuilt.push_attribute(("fullCalcOnLoad", "1"));
                writer
                    .write_event(Event::Empty(rebuilt))
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == "workbook" => {
                if !saw_calc_pr {
                    let mut calc_pr = BytesStart::new("calcPr");
                    calc_pr.push_attribute(("fullCalcOnLoad", "1"));
                    writer
                        .write_event(Event::Empty(calc_pr))
                        .map_err(|err| XlsxError::Xml(err.to_string()))?;
                }
                writer
                    .write_event(Event::End(e))
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
            }
            Ok(Event::Eof) => break,
            Ok(ev) => writer
                .write_event(ev)
                .map_err(|err| XlsxError::Xml(err.to_string()))?,
            Err(e) => return Err(XlsxError::Xml(e.to_string())),
        }
    }

    String::from_utf8(writer.into_inner()).map_err(|e| XlsxError::Xml(e.to_string()))
}
