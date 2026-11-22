//! Streaming `.xlsx` incremental patcher (`17-m3a.md` §7 Path 2).
//!
//! Copies every untouched ZIP archive entry byte-for-byte via
//! `ZipWriter::raw_copy_file` (keeping charts, drawings, media, VBA macros,
//! merged cells, conditional formatting, pivot tables, themes, and shared
//! strings 100% intact), drops `xl/calcChain.xml` and forces
//! `fullCalcOnLoad="1"`, and streams only modified `xl/worksheets/sheetN.xml`
//! entries through `quick-xml` while preserving existing `s="..."` style
//! attributes and using `t="inlineStr"` for updated strings.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write as _};

use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use reticula_core::id::SheetId;
use reticula_core::ops::{FormulaMode, ScalarValue, Workbook};
use reticula_core::value::{CalcError, Value};
use reticula_formula::binder::{BoundExpr, BoundFormula};
use reticula_formula::engine::FormulaEngine;
use reticula_formula::spill::SpillRegistry;
use zip::ZipArchive;
use zip::write::SimpleFileOptions;

use crate::export::ensure_full_calc_on_load;
use crate::xlfn::add_xlfn_prefixes;
use crate::xml_scan::{a1_cell_ref, parse_a1_ref, scan_workbook_xml, validate_zip_limits};
use crate::{
    ChangedCell, EXCEL_MAX_COLS, EXCEL_MAX_ROWS, XlsxError, XlsxExportResult, XlsxExportWarning,
};

#[derive(Clone, Debug)]
enum CellPatch {
    Clear,
    Number(f64),
    InlineStr(String),
    Bool(bool),
    Error(CalcError),
    Formula {
        text: String,
        array_ref: Option<String>,
        cached: Option<ScalarValue>,
    },
}

fn find_sheet_by_name<'a>(
    wb: &'a Workbook<BoundFormula>,
    name: &str,
) -> Option<&'a reticula_core::sheet::Sheet> {
    wb.sheets()
        .iter()
        .find(|s| !s.is_deleted() && s.name() == name)
}

fn build_cell_patch(
    wb: &Workbook<BoundFormula>,
    spills: Option<&SpillRegistry>,
    sid: SheetId,
    sheet: &reticula_core::sheet::Sheet,
    row: u32,
    col: u32,
) -> CellPatch {
    let Some(row_raw) = sheet.row_axis().pos_to_id(row) else {
        return CellPatch::Clear;
    };
    let Some(col_raw) = sheet.col_axis().pos_to_id(col) else {
        return CellPatch::Clear;
    };
    let row_id = reticula_core::id::RowId(row_raw);
    let col_id = reticula_core::id::ColId(col_raw);
    let key = (sid, row_id, col_id);

    if let Some(formula) = wb.formulas().get(&key) {
        let raw = formula.unparse(wb, sid);
        let body = raw.strip_prefix('=').unwrap_or(&raw);
        let prefixed = add_xlfn_prefixes(body);
        let array_ref = if formula.mode == FormulaMode::Dynamic
            && let Some((sp_rows, sp_cols)) = spills.and_then(|reg| reg.spill_shape(key))
            && (sp_rows > 1 || sp_cols > 1)
        {
            let r2 = row
                .saturating_add(sp_rows.saturating_sub(1))
                .min(EXCEL_MAX_ROWS - 1);
            let c2 = col
                .saturating_add(sp_cols.saturating_sub(1))
                .min(EXCEL_MAX_COLS - 1);
            Some(format!("{}:{}", a1_cell_ref(row, col), a1_cell_ref(r2, c2)))
        } else {
            None
        };
        let cached = match &formula.root {
            BoundExpr::PreservedCached { cached, .. } => Some(cached.clone()),
            _ => None,
        };
        return CellPatch::Formula {
            text: prefixed,
            array_ref,
            cached,
        };
    }

    match sheet.value_by_id(row_id, col_id) {
        Value::Empty | Value::Array(_) => CellPatch::Clear,
        Value::Number(n) => {
            if n.is_finite() {
                CellPatch::Number(if n == 0.0 { 0.0 } else { n })
            } else {
                CellPatch::Error(CalcError::Num)
            }
        }
        Value::Text(idx) => CellPatch::InlineStr(sheet.strings().get(idx).to_owned()),
        Value::Bool(b) => CellPatch::Bool(b),
        Value::Error(e) => CellPatch::Error(e),
    }
}

/// Applies a streaming incremental patch over `orig_xlsx` for the listed `changed_cells`.
pub fn patch_xlsx(
    orig_xlsx: &[u8],
    engine: &FormulaEngine,
    changed_cells: &[ChangedCell],
) -> Result<XlsxExportResult, XlsxError> {
    patch_workbook_xlsx(
        orig_xlsx,
        engine.workbook(),
        Some(engine.spills()),
        changed_cells,
    )
}

/// Applies a streaming incremental patch over `orig_xlsx` from a raw [`Workbook`].
pub fn patch_workbook_xlsx(
    orig_xlsx: &[u8],
    wb: &Workbook<BoundFormula>,
    spills: Option<&SpillRegistry>,
    changed_cells: &[ChangedCell],
) -> Result<XlsxExportResult, XlsxError> {
    validate_zip_limits(orig_xlsx)?;
    let scanned = scan_workbook_xml(orig_xlsx)?;

    let mut path_by_sheet_name: BTreeMap<String, String> = BTreeMap::new();
    for s in &scanned.sheets {
        path_by_sheet_name.insert(s.name.clone(), s.zip_path.to_ascii_lowercase());
    }

    let mut patches_by_zip_path: BTreeMap<String, BTreeMap<u32, BTreeMap<u32, CellPatch>>> =
        BTreeMap::new();
    let mut truncated_count = 0_u32;

    for change in changed_cells {
        let Some(zip_path_lower) = path_by_sheet_name.get(&change.sheet_name) else {
            return Err(XlsxError::SheetNotFound(change.sheet_name.clone()));
        };
        let Some(sheet) = find_sheet_by_name(wb, &change.sheet_name) else {
            return Err(XlsxError::SheetNotFound(change.sheet_name.clone()));
        };
        if change.row >= EXCEL_MAX_ROWS || change.col >= EXCEL_MAX_COLS {
            truncated_count = truncated_count.saturating_add(1);
            continue;
        }
        let patch = build_cell_patch(wb, spills, sheet.id(), sheet, change.row, change.col);
        patches_by_zip_path
            .entry(zip_path_lower.clone())
            .or_default()
            .entry(change.row)
            .or_default()
            .insert(change.col, patch);
    }

    let mut reader_zip = ZipArchive::new(Cursor::new(orig_xlsx))
        .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
    let mut out_buf = Vec::with_capacity(orig_xlsx.len().saturating_add(4096));

    {
        let mut writer_zip = zip::ZipWriter::new(Cursor::new(&mut out_buf));
        let file_opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        for idx in 0..reader_zip.len() {
            let mut entry = reader_zip
                .by_index(idx)
                .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
            let orig_name = entry
                .name()
                .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?
                .into_owned();
            let lower = orig_name.to_ascii_lowercase();

            if lower == "xl/calcchain.xml" {
                continue;
            }
            if lower == "[content_types].xml" {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| XlsxError::Xml(e.to_string()))?;
                let rewritten = strip_calc_chain_override(&xml)?;
                writer_zip
                    .start_file(&orig_name, file_opts)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                writer_zip
                    .write_all(rewritten.as_bytes())
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                continue;
            }
            if lower == "xl/_rels/workbook.xml.rels" {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| XlsxError::Xml(e.to_string()))?;
                let rewritten = strip_calc_chain_rel(&xml)?;
                writer_zip
                    .start_file(&orig_name, file_opts)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                writer_zip
                    .write_all(rewritten.as_bytes())
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                continue;
            }
            if lower == "xl/workbook.xml" {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| XlsxError::Xml(e.to_string()))?;
                let rewritten = ensure_full_calc_on_load(&xml)?;
                writer_zip
                    .start_file(&orig_name, file_opts)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                writer_zip
                    .write_all(rewritten.as_bytes())
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                continue;
            }
            if let Some(sheet_patches) = patches_by_zip_path.get(&lower) {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| XlsxError::Xml(e.to_string()))?;
                let rewritten = patch_worksheet_xml(&xml, sheet_patches)?;
                writer_zip
                    .start_file(&orig_name, file_opts)
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                writer_zip
                    .write_all(rewritten.as_bytes())
                    .map_err(|e| XlsxError::Write(e.to_string()))?;
                continue;
            }

            writer_zip
                .raw_copy_file(entry)
                .map_err(|e| XlsxError::Write(e.to_string()))?;
        }

        writer_zip
            .finish()
            .map_err(|e| XlsxError::Write(e.to_string()))?;
    }

    let mut warnings = Vec::new();
    if truncated_count > 0 {
        warnings.push(XlsxExportWarning {
            code: String::from("EXCEL_BOUNDS_TRUNCATED"),
            message: format!(
                "Skipped {truncated_count} patched cell(s) exceeding Excel's 1,048,576 x 16,384 worksheet limits"
            ),
            count: truncated_count,
        });
    }

    Ok(XlsxExportResult {
        bytes: out_buf,
        warnings,
    })
}

fn attr_val(start: &BytesStart<'_>, key: &str) -> Option<String> {
    for attr in start.attributes().flatten() {
        if attr.key.as_ref() == key {
            return quick_xml::escape::unescape(attr.value.as_ref())
                .ok()
                .map(std::borrow::Cow::into_owned);
        }
    }
    None
}

fn strip_calc_chain_override(xml: &str) -> Result<String, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::with_capacity(xml.len()));
    let mut skip_end = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e))
                if e.local_name().as_ref() == "Override"
                    && attr_val(e, "PartName")
                        .is_some_and(|p| p.eq_ignore_ascii_case("/xl/calcChain.xml")) =>
            {
                skip_end = true;
            }
            Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == "Override"
                    && attr_val(e, "PartName")
                        .is_some_and(|p| p.eq_ignore_ascii_case("/xl/calcChain.xml")) => {}
            Ok(Event::End(ref e)) if skip_end && e.local_name().as_ref() == "Override" => {
                skip_end = false;
            }
            Ok(Event::Eof) => break,
            Ok(ev) if !skip_end => writer
                .write_event(ev)
                .map_err(|err| XlsxError::Xml(err.to_string()))?,
            Ok(_) => {}
            Err(e) => return Err(XlsxError::Xml(e.to_string())),
        }
    }
    String::from_utf8(writer.into_inner()).map_err(|e| XlsxError::Xml(e.to_string()))
}

fn strip_calc_chain_rel(xml: &str) -> Result<String, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::with_capacity(xml.len()));
    let mut skip_end = false;

    let is_calc_rel = |e: &BytesStart<'_>| {
        attr_val(e, "Target").is_some_and(|t| {
            let lower = t.to_ascii_lowercase();
            lower == "calcchain.xml" || lower == "/xl/calcchain.xml" || lower == "xl/calcchain.xml"
        })
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e))
                if e.local_name().as_ref() == "Relationship" && is_calc_rel(e) =>
            {
                skip_end = true;
            }
            Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == "Relationship" && is_calc_rel(e) => {}
            Ok(Event::End(ref e)) if skip_end && e.local_name().as_ref() == "Relationship" => {
                skip_end = false;
            }
            Ok(Event::Eof) => break,
            Ok(ev) if !skip_end => writer
                .write_event(ev)
                .map_err(|err| XlsxError::Xml(err.to_string()))?,
            Ok(_) => {}
            Err(e) => return Err(XlsxError::Xml(e.to_string())),
        }
    }
    String::from_utf8(writer.into_inner()).map_err(|e| XlsxError::Xml(e.to_string()))
}

fn write_patched_cell(
    writer: &mut Writer<Vec<u8>>,
    row: u32,
    col: u32,
    style_attr: Option<&str>,
    patch: &CellPatch,
) -> Result<(), XlsxError> {
    let map_err = |e: std::io::Error| XlsxError::Xml(e.to_string());
    let coord = a1_cell_ref(row, col);
    let mut c_start = BytesStart::new("c");
    c_start.push_attribute(("r", coord.as_str()));
    if let Some(s) = style_attr {
        c_start.push_attribute(("s", s));
    }

    match patch {
        CellPatch::Clear => {
            if style_attr.is_some() {
                writer.write_event(Event::Empty(c_start)).map_err(map_err)?;
            }
            return Ok(());
        }
        CellPatch::Number(n) => {
            writer.write_event(Event::Start(c_start)).map_err(map_err)?;
            writer
                .write_event(Event::Start(BytesStart::new("v")))
                .map_err(map_err)?;
            let text = format!("{n}");
            writer
                .write_event(Event::Text(BytesText::new(&text)))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("v")))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("c")))
                .map_err(map_err)?;
        }
        CellPatch::InlineStr(s) => {
            c_start.push_attribute(("t", "inlineStr"));
            writer.write_event(Event::Start(c_start)).map_err(map_err)?;
            writer
                .write_event(Event::Start(BytesStart::new("is")))
                .map_err(map_err)?;
            let mut t_start = BytesStart::new("t");
            t_start.push_attribute(("xml:space", "preserve"));
            writer.write_event(Event::Start(t_start)).map_err(map_err)?;
            writer
                .write_event(Event::Text(BytesText::new(s)))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("t")))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("is")))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("c")))
                .map_err(map_err)?;
        }
        CellPatch::Bool(b) => {
            c_start.push_attribute(("t", "b"));
            writer.write_event(Event::Start(c_start)).map_err(map_err)?;
            writer
                .write_event(Event::Start(BytesStart::new("v")))
                .map_err(map_err)?;
            writer
                .write_event(Event::Text(BytesText::new(if *b { "1" } else { "0" })))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("v")))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("c")))
                .map_err(map_err)?;
        }
        CellPatch::Error(e) => {
            c_start.push_attribute(("t", "e"));
            writer.write_event(Event::Start(c_start)).map_err(map_err)?;
            writer
                .write_event(Event::Start(BytesStart::new("v")))
                .map_err(map_err)?;
            writer
                .write_event(Event::Text(BytesText::new(e.as_str())))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("v")))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("c")))
                .map_err(map_err)?;
        }
        CellPatch::Formula {
            text,
            array_ref,
            cached,
        } => {
            if let Some(scalar) = cached {
                match scalar {
                    ScalarValue::Number(_) => {}
                    ScalarValue::Text(_) => c_start.push_attribute(("t", "str")),
                    ScalarValue::Bool(_) => c_start.push_attribute(("t", "b")),
                    ScalarValue::Error(_) => c_start.push_attribute(("t", "e")),
                }
            }
            writer.write_event(Event::Start(c_start)).map_err(map_err)?;
            let mut f_start = BytesStart::new("f");
            if let Some(aref) = array_ref {
                f_start.push_attribute(("t", "array"));
                f_start.push_attribute(("ref", aref.as_str()));
            }
            writer.write_event(Event::Start(f_start)).map_err(map_err)?;
            writer
                .write_event(Event::Text(BytesText::new(text)))
                .map_err(map_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("f")))
                .map_err(map_err)?;
            if let Some(scalar) = cached {
                writer
                    .write_event(Event::Start(BytesStart::new("v")))
                    .map_err(map_err)?;
                let v_str = match scalar {
                    ScalarValue::Number(n) => format!("{n}"),
                    ScalarValue::Text(s) => s.clone(),
                    ScalarValue::Bool(b) => String::from(if *b { "1" } else { "0" }),
                    ScalarValue::Error(e) => String::from(e.as_str()),
                };
                writer
                    .write_event(Event::Text(BytesText::new(&v_str)))
                    .map_err(map_err)?;
                writer
                    .write_event(Event::End(BytesEnd::new("v")))
                    .map_err(map_err)?;
            }
            writer
                .write_event(Event::End(BytesEnd::new("c")))
                .map_err(map_err)?;
        }
    }
    Ok(())
}

fn write_fresh_row(
    writer: &mut Writer<Vec<u8>>,
    row: u32,
    cols: &BTreeMap<u32, CellPatch>,
) -> Result<(), XlsxError> {
    let map_err = |e: std::io::Error| XlsxError::Xml(e.to_string());
    let has_content = cols.values().any(|p| !matches!(p, CellPatch::Clear));
    if !has_content {
        return Ok(());
    }
    let row_one = row.saturating_add(1).to_string();
    let mut r_start = BytesStart::new("row");
    r_start.push_attribute(("r", row_one.as_str()));
    writer.write_event(Event::Start(r_start)).map_err(map_err)?;
    for (&col, patch) in cols {
        write_patched_cell(writer, row, col, None, patch)?;
    }
    writer
        .write_event(Event::End(BytesEnd::new("row")))
        .map_err(map_err)?;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "streaming quick-xml worksheet row/cell patch state machine"
)]
fn patch_worksheet_xml(
    xml: &str,
    row_patches: &BTreeMap<u32, BTreeMap<u32, CellPatch>>,
) -> Result<String, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::with_capacity(xml.len().saturating_add(1024)));
    let map_err = |e: std::io::Error| XlsxError::Xml(e.to_string());

    let mut in_sheet_data = false;
    let pending_rows: Vec<(u32, &BTreeMap<u32, CellPatch>)> =
        row_patches.iter().map(|(&r, c)| (r, c)).collect();
    let mut next_row_idx = 0usize;

    let mut cur_row_zero: Option<u32> = None;
    let mut cur_row_cols: Vec<(u32, &CellPatch)> = Vec::new();
    let mut next_col_idx = 0usize;
    let mut skipping_cell = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) if !in_sheet_data && e.local_name().as_ref() == "sheetData" => {
                in_sheet_data = true;
                writer
                    .write_event(Event::Start(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::Empty(ref e)) if !in_sheet_data && e.local_name().as_ref() == "sheetData" => {
                writer
                    .write_event(Event::Start(BytesStart::new("sheetData")))
                    .map_err(map_err)?;
                while let Some(&(r, cols)) = pending_rows.get(next_row_idx) {
                    write_fresh_row(&mut writer, r, cols)?;
                    next_row_idx = next_row_idx.saturating_add(1);
                }
                writer
                    .write_event(Event::End(BytesEnd::new("sheetData")))
                    .map_err(map_err)?;
            }
            Ok(Event::End(ref e)) if in_sheet_data && e.local_name().as_ref() == "sheetData" => {
                while let Some(&(r, cols)) = pending_rows.get(next_row_idx) {
                    write_fresh_row(&mut writer, r, cols)?;
                    next_row_idx = next_row_idx.saturating_add(1);
                }
                in_sheet_data = false;
                writer
                    .write_event(Event::End(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::Start(ref e)) if in_sheet_data && e.local_name().as_ref() == "row" => {
                let r_zero = attr_val(e, "r")
                    .and_then(|s| s.parse::<u32>().ok())
                    .and_then(|r| r.checked_sub(1));
                if let Some(row_zero) = r_zero {
                    while let Some(&(pending_r, cols)) = pending_rows.get(next_row_idx)
                        && pending_r < row_zero
                    {
                        write_fresh_row(&mut writer, pending_r, cols)?;
                        next_row_idx = next_row_idx.saturating_add(1);
                    }
                    cur_row_zero = Some(row_zero);
                    cur_row_cols.clear();
                    next_col_idx = 0;
                    if let Some(&(pending_r, cols)) = pending_rows.get(next_row_idx)
                        && pending_r == row_zero
                    {
                        cur_row_cols.extend(cols.iter().map(|(&c, p)| (c, p)));
                        next_row_idx = next_row_idx.saturating_add(1);
                    }
                }
                writer
                    .write_event(Event::Start(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::Empty(ref e)) if in_sheet_data && e.local_name().as_ref() == "row" => {
                let r_zero = attr_val(e, "r")
                    .and_then(|s| s.parse::<u32>().ok())
                    .and_then(|r| r.checked_sub(1));
                if let Some(row_zero) = r_zero {
                    while let Some(&(pending_r, cols)) = pending_rows.get(next_row_idx)
                        && pending_r < row_zero
                    {
                        write_fresh_row(&mut writer, pending_r, cols)?;
                        next_row_idx = next_row_idx.saturating_add(1);
                    }
                    if let Some(&(pending_r, cols)) = pending_rows.get(next_row_idx)
                        && pending_r == row_zero
                    {
                        next_row_idx = next_row_idx.saturating_add(1);
                        let r_start = e.borrow().into_owned();
                        writer.write_event(Event::Start(r_start)).map_err(map_err)?;
                        for (&col, patch) in cols {
                            write_patched_cell(&mut writer, row_zero, col, None, patch)?;
                        }
                        writer
                            .write_event(Event::End(BytesEnd::new("row")))
                            .map_err(map_err)?;
                        continue;
                    }
                }
                writer
                    .write_event(Event::Empty(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::End(ref e)) if in_sheet_data && e.local_name().as_ref() == "row" => {
                if let Some(row_zero) = cur_row_zero.take() {
                    while let Some(&(col, patch)) = cur_row_cols.get(next_col_idx) {
                        write_patched_cell(&mut writer, row_zero, col, None, patch)?;
                        next_col_idx = next_col_idx.saturating_add(1);
                    }
                    cur_row_cols.clear();
                }
                writer
                    .write_event(Event::End(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::Start(ref e))
                if in_sheet_data && !skipping_cell && e.local_name().as_ref() == "c" =>
            {
                if let Some(row_zero) = cur_row_zero
                    && let Some((_, c_col)) = attr_val(e, "r").and_then(|s| parse_a1_ref(&s))
                {
                    while let Some(&(col, patch)) = cur_row_cols.get(next_col_idx)
                        && col < c_col
                    {
                        write_patched_cell(&mut writer, row_zero, col, None, patch)?;
                        next_col_idx = next_col_idx.saturating_add(1);
                    }
                    if let Some(&(col, patch)) = cur_row_cols.get(next_col_idx)
                        && col == c_col
                    {
                        let style_attr = attr_val(e, "s");
                        write_patched_cell(
                            &mut writer,
                            row_zero,
                            col,
                            style_attr.as_deref(),
                            patch,
                        )?;
                        next_col_idx = next_col_idx.saturating_add(1);
                        skipping_cell = true;
                        continue;
                    }
                }
                writer
                    .write_event(Event::Start(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::Empty(ref e))
                if in_sheet_data && !skipping_cell && e.local_name().as_ref() == "c" =>
            {
                if let Some(row_zero) = cur_row_zero
                    && let Some((_, c_col)) = attr_val(e, "r").and_then(|s| parse_a1_ref(&s))
                {
                    while let Some(&(col, patch)) = cur_row_cols.get(next_col_idx)
                        && col < c_col
                    {
                        write_patched_cell(&mut writer, row_zero, col, None, patch)?;
                        next_col_idx = next_col_idx.saturating_add(1);
                    }
                    if let Some(&(col, patch)) = cur_row_cols.get(next_col_idx)
                        && col == c_col
                    {
                        let style_attr = attr_val(e, "s");
                        write_patched_cell(
                            &mut writer,
                            row_zero,
                            col,
                            style_attr.as_deref(),
                            patch,
                        )?;
                        next_col_idx = next_col_idx.saturating_add(1);
                        continue;
                    }
                }
                writer
                    .write_event(Event::Empty(e.borrow().into_owned()))
                    .map_err(map_err)?;
            }
            Ok(Event::End(ref e)) if skipping_cell && e.local_name().as_ref() == "c" => {
                skipping_cell = false;
            }
            Ok(ev) if skipping_cell => {
                let _ = ev;
            }
            Ok(Event::Eof) => break,
            Ok(ev) => writer.write_event(ev).map_err(map_err)?,
            Err(e) => return Err(XlsxError::Xml(e.to_string())),
        }
    }

    String::from_utf8(writer.into_inner()).map_err(|e| XlsxError::Xml(e.to_string()))
}
