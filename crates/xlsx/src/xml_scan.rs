//! Bounded streaming OOXML ZIP validation and `quick-xml` metadata extraction
//! (`workbook.xml`, `styles.xml`, `sharedStrings.xml`, `metadata.xml`, and
//! `xl/worksheets/*.xml`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use reticula_core::format::{NumberFormat, parse_format_code};
use reticula_core::ops::ScalarValue;
use reticula_core::sheet::{
    MAX_COL_WIDTH, MAX_FREEZE_COUNT, MAX_ROW_HEIGHT, MIN_COL_WIDTH, MIN_ROW_HEIGHT,
};
use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_core::value::CalcError;
use zip::ZipArchive;

use crate::{
    MAX_XLSX_INPUT_BYTES, MAX_XLSX_UNCOMPRESSED_BYTES, UnsupportedKind, XlsxError,
    XlsxUnsupportedFeature,
};

const MAX_TOTAL_ARCHIVE_READ: u64 = 512 * 1024 * 1024;

/// Converts 0-based `(row, col)` into an Excel A1 reference string (e.g. `(0, 26) -> "AA1"`).
#[must_use]
pub fn a1_cell_ref(row: u32, col: u32) -> String {
    let mut col_buf = Vec::with_capacity(4);
    let mut n = col.saturating_add(1);
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        col_buf.push(b'A' + rem);
        n = (n - 1) / 26;
    }
    col_buf.reverse();
    let col_str = String::from_utf8(col_buf).unwrap_or_default();
    let row_one = row.saturating_add(1);
    format!("{col_str}{row_one}")
}

/// Parses an A1 cell coordinate like `"B12"` or `"$B$12"` into 0-based `(row, col)`.
#[must_use]
pub fn parse_a1_ref(s: &str) -> Option<(u32, u32)> {
    let bytes = s.trim().as_bytes();
    let mut i = 0usize;
    if bytes.get(i) == Some(&b'$') {
        i = i.saturating_add(1);
    }
    let col_start = i;
    while i < bytes.len() && bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
        i = i.saturating_add(1);
    }
    let col_len = i.saturating_sub(col_start);
    if col_len == 0 || col_len > 3 {
        return None;
    }
    let mut col_one: u32 = 0;
    for &b in bytes.get(col_start..i)? {
        let val = u32::from(b.to_ascii_uppercase().checked_sub(b'A')? + 1);
        col_one = col_one.checked_mul(26)?.checked_add(val)?;
    }
    if bytes.get(i) == Some(&b'$') {
        i = i.saturating_add(1);
    }
    if i >= bytes.len() {
        return None;
    }
    let mut row_one: u32 = 0;
    for &b in bytes.get(i..)? {
        if !b.is_ascii_digit() {
            return None;
        }
        row_one = row_one.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    if row_one == 0 || col_one == 0 {
        return None;
    }
    Some((row_one - 1, col_one - 1))
}

/// Parses `"A1:C5"` (or single `"A1"`) into normalized inclusive `(r1, c1, r2, c2)`.
#[must_use]
pub fn parse_a1_range(s: &str) -> Option<(u32, u32, u32, u32)> {
    let trimmed = s.trim();
    if let Some((left, right)) = trimmed.split_once(':') {
        let (r1, c1) = parse_a1_ref(left)?;
        let (r2, c2) = parse_a1_ref(right)?;
        Some((r1.min(r2), c1.min(c2), r1.max(r2), c1.max(c2)))
    } else {
        let (r, c) = parse_a1_ref(trimmed)?;
        Some((r, c, r, c))
    }
}

fn is_counted_xml_path(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "xl/workbook.xml"
        || lower == "xl/sharedstrings.xml"
        || lower == "xl/styles.xml"
        || lower == "xl/metadata.xml"
        || (lower.starts_with("xl/worksheets/") && lower.ends_with(".xml"))
}

/// Enforces raw compressed (`<= 64 MiB`) and cumulative uncompressed XML (`<= 256 MiB`) bounds.
pub fn validate_zip_limits(bytes: &[u8]) -> Result<(), XlsxError> {
    if bytes.len() > MAX_XLSX_INPUT_BYTES {
        return Err(XlsxError::InputTooLarge { bytes: bytes.len() });
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;

    let mut xml_total: u64 = 0;
    let mut all_total: u64 = 0;

    for idx in 0..archive.len() {
        let entry = archive
            .by_index(idx)
            .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
        let counted = {
            let entry_name = entry
                .name()
                .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
            is_counted_xml_path(entry_name.as_ref())
        };
        if counted {
            let rem = MAX_XLSX_UNCOMPRESSED_BYTES.saturating_sub(xml_total);
            if entry.size() > rem {
                return Err(XlsxError::UncompressedTooLarge {
                    bytes: xml_total.saturating_add(entry.size()),
                });
            }
            let copied =
                std::io::copy(&mut entry.take(rem.saturating_add(1)), &mut std::io::sink())
                    .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
            xml_total = xml_total.saturating_add(copied);
            if xml_total > MAX_XLSX_UNCOMPRESSED_BYTES {
                return Err(XlsxError::UncompressedTooLarge { bytes: xml_total });
            }
            all_total = all_total.saturating_add(copied);
        } else {
            let rem = MAX_TOTAL_ARCHIVE_READ.saturating_sub(all_total);
            let copied =
                std::io::copy(&mut entry.take(rem.saturating_add(1)), &mut std::io::sink())
                    .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
            all_total = all_total.saturating_add(copied);
            if all_total > MAX_TOTAL_ARCHIVE_READ {
                return Err(XlsxError::UncompressedTooLarge { bytes: all_total });
            }
        }
    }

    Ok(())
}

/// Decoded style entry indexed by OOXML `xf` id (`<cellXfs>`).
#[derive(Clone, Debug, Default)]
pub struct ScannedXf {
    pub number_format: Option<NumberFormat>,
    pub is_date_fmt: bool,
    pub style_patch: StylePatch,
}

/// Extracted formula/cache metadata for a single OOXML `<c>` element.
#[derive(Clone, Debug)]
pub struct ScannedCellMeta {
    pub row: u32,
    pub col: u32,
    pub xf_idx: Option<u32>,
    pub cm_idx: u32,
    pub formula_text: Option<String>,
    pub formula_type: Option<String>,
    pub formula_ref: Option<(u32, u32, u32, u32)>,
    pub cached_value: Option<ScalarValue>,
}

/// Extracted worksheet metadata from `xl/worksheets/*.xml`.
#[derive(Clone, Debug, Default)]
pub struct ScannedSheet {
    pub name: String,
    pub zip_path: String,
    pub frozen_rows: u32,
    pub frozen_cols: u32,
    pub col_widths_px: BTreeMap<u32, u16>,
    pub row_heights_px: BTreeMap<u32, u16>,
    pub cell_metas: Vec<ScannedCellMeta>,
    pub merged_cells_count: u32,
    pub conditional_fmt_count: u32,
    pub drawing_count: u32,
}

/// Extracted OOXML `<definedName>` entry.
#[derive(Clone, Debug)]
pub struct ScannedDefinedName {
    pub name: String,
    pub local_sheet_idx: Option<usize>,
    pub refers_to: String,
    pub comment: Option<String>,
}

/// Workbook-wide XML scan output.
#[derive(Clone, Debug, Default)]
pub struct ScannedWorkbook {
    pub is_1904: bool,
    pub has_metadata_xldapr: bool,
    pub xldapr_cm_indices: BTreeSet<u32>,
    pub shared_strings: Vec<String>,
    pub xfs: Vec<ScannedXf>,
    pub sheets: Vec<ScannedSheet>,
    pub defined_names: Vec<ScannedDefinedName>,
    pub unsupported: Vec<XlsxUnsupportedFeature>,
}

/// Reads all bounded metadata and formula/style XML from an `.xlsx` archive.
pub fn scan_workbook_xml(bytes: &[u8]) -> Result<ScannedWorkbook, XlsxError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;

    let mut out = ScannedWorkbook::default();
    let mut charts = 0_u32;
    let mut drawings = 0_u32;
    let mut macros = 0_u32;
    let mut pivots = 0_u32;
    let mut comments = 0_u32;
    let mut external_links = 0_u32;

    for idx in 0..archive.len() {
        let entry = archive
            .by_index(idx)
            .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
        let lower = entry
            .name()
            .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?
            .to_ascii_lowercase();
        if lower == "xl/vbaproject.bin" {
            macros = macros.saturating_add(1);
        } else if lower.starts_with("xl/charts/") && lower.ends_with(".xml") {
            charts = charts.saturating_add(1);
        } else if (lower.starts_with("xl/drawings/") && lower.ends_with(".xml"))
            || lower.starts_with("xl/media/")
        {
            drawings = drawings.saturating_add(1);
        } else if lower.starts_with("xl/pivottables/") && lower.ends_with(".xml") {
            pivots = pivots.saturating_add(1);
        } else if (lower.starts_with("xl/comments") || lower.starts_with("xl/threadedcomments/"))
            && lower.ends_with(".xml")
        {
            comments = comments.saturating_add(1);
        } else if lower == "xl/connections.xml"
            || (lower.starts_with("xl/externallinks/") && lower.ends_with(".xml"))
        {
            external_links = external_links.saturating_add(1);
        }
    }

    let rels = if let Some(xml) = read_optional_entry(&mut archive, "xl/_rels/workbook.xml.rels")? {
        parse_workbook_rels(&xml)?
    } else {
        BTreeMap::new()
    };

    if let Some(xml) = read_optional_entry(&mut archive, "xl/metadata.xml")? {
        out.xldapr_cm_indices = parse_metadata_xldapr(&xml)?;
        out.has_metadata_xldapr = !out.xldapr_cm_indices.is_empty();
    }

    if let Some(xml) = read_optional_entry(&mut archive, "xl/sharedStrings.xml")? {
        out.shared_strings = parse_shared_strings(&xml)?;
    }

    if let Some(xml) = read_optional_entry(&mut archive, "xl/styles.xml")? {
        out.xfs = parse_styles(&xml)?;
    }

    let wb_xml = read_optional_entry(&mut archive, "xl/workbook.xml")?
        .ok_or_else(|| XlsxError::Xml(String::from("missing xl/workbook.xml")))?;
    let wb_parsed = parse_workbook(&wb_xml, &rels)?;
    out.is_1904 = wb_parsed.is_1904;
    out.defined_names = wb_parsed.defined_names;

    for (name, zip_path) in wb_parsed.sheet_targets {
        let Some(sheet_xml) = read_optional_entry(&mut archive, &zip_path)? else {
            return Err(XlsxError::Xml(format!(
                "missing worksheet entry {zip_path}"
            )));
        };
        let sheet =
            parse_worksheet_xml(&name, &zip_path, &sheet_xml, &out.shared_strings, &out.xfs)?;
        if sheet.merged_cells_count > 0 {
            out.unsupported.push(XlsxUnsupportedFeature {
                kind: UnsupportedKind::MergedCells,
                sheet: Some(name.clone()),
                count: sheet.merged_cells_count,
                detail: format!("{} merged range(s) on `{name}`", sheet.merged_cells_count),
            });
        }
        if sheet.conditional_fmt_count > 0 {
            out.unsupported.push(XlsxUnsupportedFeature {
                kind: UnsupportedKind::ConditionalFormatting,
                sheet: Some(name.clone()),
                count: sheet.conditional_fmt_count,
                detail: format!(
                    "{} conditional formatting block(s) on `{name}`",
                    sheet.conditional_fmt_count
                ),
            });
        }
        if sheet.drawing_count > 0 {
            out.unsupported.push(XlsxUnsupportedFeature {
                kind: UnsupportedKind::DrawingOrImage,
                sheet: Some(name.clone()),
                count: sheet.drawing_count,
                detail: format!(
                    "{} drawing/shape reference(s) on `{name}`",
                    sheet.drawing_count
                ),
            });
        }
        out.sheets.push(sheet);
    }

    if macros > 0 {
        out.unsupported.push(XlsxUnsupportedFeature {
            kind: UnsupportedKind::VbaMacro,
            sheet: None,
            count: macros,
            detail: String::from(
                "VBA macro project (xl/vbaProject.bin) is not executed or re-emitted on fresh export",
            ),
        });
    }
    if charts > 0 {
        out.unsupported.push(XlsxUnsupportedFeature {
            kind: UnsupportedKind::Chart,
            sheet: None,
            count: charts,
            detail: format!("{charts} embedded chart XML part(s)"),
        });
    }
    if drawings > 0 {
        out.unsupported.push(XlsxUnsupportedFeature {
            kind: UnsupportedKind::DrawingOrImage,
            sheet: None,
            count: drawings,
            detail: format!("{drawings} drawing/media archive asset(s)"),
        });
    }
    if pivots > 0 {
        out.unsupported.push(XlsxUnsupportedFeature {
            kind: UnsupportedKind::PivotTable,
            sheet: None,
            count: pivots,
            detail: format!("{pivots} pivot table definition(s)"),
        });
    }
    if comments > 0 {
        out.unsupported.push(XlsxUnsupportedFeature {
            kind: UnsupportedKind::Comment,
            sheet: None,
            count: comments,
            detail: format!("{comments} worksheet comment stream(s)"),
        });
    }
    if external_links > 0 {
        out.unsupported.push(XlsxUnsupportedFeature {
            kind: UnsupportedKind::ExternalConnection,
            sheet: None,
            count: external_links,
            detail: format!("{external_links} external workbook/data connection part(s)"),
        });
    }

    Ok(out)
}

fn read_optional_entry(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    target: &str,
) -> Result<Option<String>, XlsxError> {
    let exact = archive.by_name(target).is_ok();
    let resolved_name: Option<String> = if exact {
        Some(target.to_owned())
    } else {
        let lower_target = target.to_ascii_lowercase();
        let mut found = None;
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index(i)
                && let Ok(ename) = entry.name()
                && ename.to_ascii_lowercase() == lower_target
            {
                found = Some(ename.into_owned());
                break;
            }
        }
        found
    };
    let Some(name) = resolved_name else {
        return Ok(None);
    };
    let entry = archive
        .by_name(&name)
        .map_err(|e| XlsxError::InvalidArchive(e.to_string()))?;
    let mut s = String::new();
    entry
        .take(MAX_XLSX_UNCOMPRESSED_BYTES.saturating_add(1))
        .read_to_string(&mut s)
        .map_err(|e| XlsxError::Xml(format!("failed reading {name} as UTF-8: {e}")))?;
    Ok(Some(s))
}

fn attr_str(start: &BytesStart<'_>, key: &str) -> Option<String> {
    for attr in start.attributes().flatten() {
        if attr.key.as_ref() == key {
            return quick_xml::escape::unescape(attr.value.as_ref())
                .ok()
                .map(std::borrow::Cow::into_owned);
        }
    }
    None
}

fn attr_local_str(start: &BytesStart<'_>, local: &str) -> Option<String> {
    for attr in start.attributes().flatten() {
        if attr.key.local_name().as_ref() == local {
            return quick_xml::escape::unescape(attr.value.as_ref())
                .ok()
                .map(std::borrow::Cow::into_owned);
        }
    }
    None
}

fn normalize_xl_target(base_dir: &str, target: &str) -> String {
    let trimmed = target.replace('\\', "/");
    if let Some(abs) = trimmed.strip_prefix('/') {
        return abs.to_owned();
    }
    let mut parts: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for seg in trimmed.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn parse_workbook_rels(xml: &str) -> Result<BTreeMap<String, String>, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut map = BTreeMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == "Relationship" =>
            {
                if let (Some(id), Some(target)) = (attr_str(e, "Id"), attr_str(e, "Target")) {
                    map.insert(id, normalize_xl_target("xl", &target));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(XlsxError::Xml(format!("workbook.xml.rels: {e}"))),
            _ => {}
        }
    }
    Ok(map)
}

struct ParsedWorkbookXml {
    is_1904: bool,
    sheet_targets: Vec<(String, String)>,
    defined_names: Vec<ScannedDefinedName>,
}

fn parse_workbook(
    xml: &str,
    rels: &BTreeMap<String, String>,
) -> Result<ParsedWorkbookXml, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut is_1904 = false;
    let mut sheet_targets = Vec::new();
    let mut defined_names = Vec::new();
    let mut in_defined_name: Option<(String, Option<usize>, Option<String>)> = None;
    let mut name_text = String::new();
    let mut fallback_sheet_idx = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "workbookPr" => {
                    if let Some(v) = attr_str(e, "date1904") {
                        is_1904 = v == "1" || v.eq_ignore_ascii_case("true");
                    }
                }
                "sheet" => {
                    fallback_sheet_idx = fallback_sheet_idx.saturating_add(1);
                    let name =
                        attr_str(e, "name").unwrap_or_else(|| format!("Sheet{fallback_sheet_idx}"));
                    let rid = attr_local_str(e, "id");
                    let zip_path = rid
                        .and_then(|id| rels.get(&id).cloned())
                        .unwrap_or_else(|| format!("xl/worksheets/sheet{fallback_sheet_idx}.xml"));
                    sheet_targets.push((name, zip_path));
                }
                "definedName" => {
                    let name = attr_str(e, "name").unwrap_or_default();
                    let local_sheet_idx =
                        attr_str(e, "localSheetId").and_then(|s| s.parse::<usize>().ok());
                    let comment = attr_str(e, "comment");
                    let hidden = attr_str(e, "hidden")
                        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
                    if !hidden && !name.is_empty() {
                        in_defined_name = Some((name, local_sheet_idx, comment));
                        name_text.clear();
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(ref e)) => match e.local_name().as_ref() {
                "workbookPr" => {
                    if let Some(v) = attr_str(e, "date1904") {
                        is_1904 = v == "1" || v.eq_ignore_ascii_case("true");
                    }
                }
                "sheet" => {
                    fallback_sheet_idx = fallback_sheet_idx.saturating_add(1);
                    let name =
                        attr_str(e, "name").unwrap_or_else(|| format!("Sheet{fallback_sheet_idx}"));
                    let rid = attr_local_str(e, "id");
                    let zip_path = rid
                        .and_then(|id| rels.get(&id).cloned())
                        .unwrap_or_else(|| format!("xl/worksheets/sheet{fallback_sheet_idx}.xml"));
                    sheet_targets.push((name, zip_path));
                }
                _ => {}
            },
            Ok(Event::Text(e)) if in_defined_name.is_some() => {
                let content = e.xml10_content();
                let decoded = quick_xml::escape::unescape(&content)
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
                name_text.push_str(&decoded);
            }
            Ok(Event::CData(e)) if in_defined_name.is_some() => {
                name_text.push_str(e.as_ref());
            }
            Ok(Event::End(ref e)) if e.local_name().as_ref() == "definedName" => {
                if let Some((name, local_sheet_idx, comment)) = in_defined_name.take() {
                    let trimmed = name_text.trim().to_owned();
                    if !trimmed.is_empty() {
                        defined_names.push(ScannedDefinedName {
                            name,
                            local_sheet_idx,
                            refers_to: trimmed,
                            comment,
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(XlsxError::Xml(format!("workbook.xml: {e}"))),
            _ => {}
        }
    }

    Ok(ParsedWorkbookXml {
        is_1904,
        sheet_targets,
        defined_names,
    })
}

fn parse_metadata_xldapr(xml: &str) -> Result<BTreeSet<u32>, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut xldapr_types = BTreeSet::new();
    let mut current_type_idx = 0_u32;
    let mut in_cell_metadata = false;
    let mut current_bk_idx = 0_u32;
    let mut in_bk = false;
    let mut bk_has_xldapr = false;
    let mut out = BTreeSet::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "metadataType" => {
                    current_type_idx = current_type_idx.saturating_add(1);
                    if attr_str(e, "name").is_some_and(|n| n.eq_ignore_ascii_case("XLDAPR")) {
                        xldapr_types.insert(current_type_idx);
                    }
                }
                "cellMetadata" => in_cell_metadata = true,
                "bk" if in_cell_metadata => {
                    in_bk = true;
                    current_bk_idx = current_bk_idx.saturating_add(1);
                    bk_has_xldapr = false;
                }
                "rc" if in_bk => {
                    if let Some(t_val) = attr_str(e, "t").and_then(|s| s.parse::<u32>().ok())
                        && xldapr_types.contains(&t_val)
                    {
                        bk_has_xldapr = true;
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(ref e)) => match e.local_name().as_ref() {
                "metadataType" => {
                    current_type_idx = current_type_idx.saturating_add(1);
                    if attr_str(e, "name").is_some_and(|n| n.eq_ignore_ascii_case("XLDAPR")) {
                        xldapr_types.insert(current_type_idx);
                    }
                }
                "rc" if in_bk => {
                    if let Some(t_val) = attr_str(e, "t").and_then(|s| s.parse::<u32>().ok())
                        && xldapr_types.contains(&t_val)
                    {
                        bk_has_xldapr = true;
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                "cellMetadata" => in_cell_metadata = false,
                "bk" if in_bk => {
                    in_bk = false;
                    if bk_has_xldapr {
                        out.insert(current_bk_idx);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(XlsxError::Xml(format!("metadata.xml: {e}"))),
            _ => {}
        }
    }

    Ok(out)
}

fn parse_shared_strings(xml: &str) -> Result<Vec<String>, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_si = false;
    let mut in_t = false;
    let mut rph_depth = 0_u32;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "si" => {
                    in_si = true;
                    current.clear();
                }
                "rPh" if in_si => rph_depth = rph_depth.saturating_add(1),
                "t" if in_si && rph_depth == 0 => in_t = true,
                _ => {}
            },
            Ok(Event::Text(e)) if in_t && rph_depth == 0 => {
                let content = e.xml10_content();
                let decoded = quick_xml::escape::unescape(&content)
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
                current.push_str(&decoded);
            }
            Ok(Event::CData(e)) if in_t && rph_depth == 0 => {
                current.push_str(e.as_ref());
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                "t" => in_t = false,
                "rPh" => rph_depth = rph_depth.saturating_sub(1),
                "si" if in_si => {
                    in_si = false;
                    strings.push(core::mem::take(&mut current));
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(XlsxError::Xml(format!("sharedStrings.xml: {e}"))),
            _ => {}
        }
    }

    Ok(strings)
}

#[derive(Clone, Copy, Default)]
struct FontSpec {
    bold: bool,
    italic: bool,
    strike: bool,
    color: Option<ColorValue>,
}

#[derive(Clone, Copy, Default)]
struct BorderSpec {
    top: BorderWeight,
    right: BorderWeight,
    bottom: BorderWeight,
    left: BorderWeight,
}

fn parse_rgb_color(hex: &str) -> Option<ColorValue> {
    let clean = hex.trim().trim_start_matches('#');
    let parsed = u32::from_str_radix(clean, 16).ok()?;
    let rgb = if clean.len() == 8 {
        parsed & 0x00FF_FFFF
    } else if clean.len() == 6 {
        parsed
    } else {
        return None;
    };
    Some(ColorValue::Rgb(rgb))
}

fn parse_border_weight(style: &str) -> BorderWeight {
    match style {
        "" | "none" => BorderWeight::None,
        "medium" | "thick" | "double" | "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" => {
            BorderWeight::Medium
        }
        _ => BorderWeight::Thin,
    }
}

fn builtin_num_fmt(id: u32) -> (Option<NumberFormat>, bool) {
    match id {
        0 => (Some(NumberFormat::General), false),
        1 => (Some(NumberFormat::Integer), false),
        2 => (Some(NumberFormat::Fixed2), false),
        3 => (
            Some(NumberFormat::Fixed {
                decimals: 0,
                thousands: true,
            }),
            false,
        ),
        4 => (
            Some(NumberFormat::Fixed {
                decimals: 2,
                thousands: true,
            }),
            false,
        ),
        5..=8 => (
            Some(NumberFormat::Currency {
                symbol: reticula_core::format::CurrencySymbol::Usd,
                decimals: if id == 5 || id == 6 { 0 } else { 2 },
            }),
            false,
        ),
        9 => (Some(NumberFormat::Percent), false),
        10 => (Some(NumberFormat::PercentDecimals { decimals: 2 }), false),
        11 => (Some(NumberFormat::Scientific { decimals: 2 }), false),
        14..=17 => (Some(NumberFormat::Date), true),
        18 | 19 => (Some(NumberFormat::Time { seconds: false }), true),
        20 | 21 | 45..=47 => (Some(NumberFormat::Time { seconds: true }), true),
        22 => (Some(NumberFormat::DateTime { seconds: false }), true),
        49 => (Some(NumberFormat::PlainText), false),
        _ => (None, false),
    }
}

fn code_looks_like_date_or_time(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    let mut in_quotes = false;
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else {
            break;
        };
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            if b == b'\\' {
                i = i.saturating_add(1);
            } else if matches!(b, b'y' | b'd' | b'h' | b's') {
                return true;
            }
        }
        i = i.saturating_add(1);
    }
    false
}

type PendingXf = (
    u32,
    usize,
    usize,
    usize,
    bool,
    bool,
    bool,
    bool,
    Option<HorizontalAlign>,
);

#[expect(
    clippy::too_many_lines,
    reason = "sequential XML SAX parsing of numFmts, fonts, fills, borders, and cellXfs"
)]
fn parse_styles(xml: &str) -> Result<Vec<ScannedXf>, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut custom_fmts: BTreeMap<u32, String> = BTreeMap::new();
    let mut fonts: Vec<FontSpec> = Vec::new();
    let mut fills: Vec<Option<ColorValue>> = Vec::new();
    let mut borders: Vec<BorderSpec> = Vec::new();
    let mut xfs: Vec<ScannedXf> = Vec::new();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        None,
        Fonts,
        Fills,
        Borders,
        CellXfs,
    }
    let mut section = Section::None;
    let mut cur_font: Option<FontSpec> = None;
    let mut cur_fill: Option<(bool, Option<ColorValue>)> = None;
    let mut cur_border: Option<BorderSpec> = None;
    let mut cur_xf: Option<PendingXf> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "numFmt" => {
                    if let (Some(id), Some(code)) = (
                        attr_str(e, "numFmtId").and_then(|s| s.parse::<u32>().ok()),
                        attr_str(e, "formatCode"),
                    ) {
                        custom_fmts.insert(id, code);
                    }
                }
                "fonts" => section = Section::Fonts,
                "fills" => section = Section::Fills,
                "borders" => section = Section::Borders,
                "cellXfs" => section = Section::CellXfs,
                "font" if section == Section::Fonts => cur_font = Some(FontSpec::default()),
                "b" if cur_font.is_some() => {
                    if let Some(f) = &mut cur_font {
                        f.bold = attr_str(e, "val").is_none_or(|v| v != "0");
                    }
                }
                "i" if cur_font.is_some() => {
                    if let Some(f) = &mut cur_font {
                        f.italic = attr_str(e, "val").is_none_or(|v| v != "0");
                    }
                }
                "strike" if cur_font.is_some() => {
                    if let Some(f) = &mut cur_font {
                        f.strike = attr_str(e, "val").is_none_or(|v| v != "0");
                    }
                }
                "color" if cur_font.is_some() => {
                    if let Some(rgb) = attr_str(e, "rgb").and_then(|s| parse_rgb_color(&s))
                        && let Some(f) = &mut cur_font
                    {
                        f.color = Some(rgb);
                    }
                }
                "fill" if section == Section::Fills => cur_fill = Some((false, None)),
                "patternFill" if cur_fill.is_some() => {
                    if let Some((solid, _)) = &mut cur_fill {
                        *solid = attr_str(e, "patternType")
                            .is_some_and(|p| p.eq_ignore_ascii_case("solid"));
                    }
                }
                "fgColor" if cur_fill.is_some() => {
                    if let Some((true, color)) = &mut cur_fill
                        && let Some(rgb) = attr_str(e, "rgb").and_then(|s| parse_rgb_color(&s))
                    {
                        *color = Some(rgb);
                    }
                }
                "border" if section == Section::Borders => cur_border = Some(BorderSpec::default()),
                "top" | "right" | "bottom" | "left" if cur_border.is_some() => {
                    if let Some(b_spec) = &mut cur_border
                        && let Some(style) = attr_str(e, "style")
                    {
                        let w = parse_border_weight(&style);
                        match e.local_name().as_ref() {
                            "top" => b_spec.top = w,
                            "right" => b_spec.right = w,
                            "bottom" => b_spec.bottom = w,
                            "left" => b_spec.left = w,
                            _ => {}
                        }
                    }
                }
                "xf" if section == Section::CellXfs => {
                    let nf = attr_str(e, "numFmtId")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    let fid = attr_str(e, "fontId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let flid = attr_str(e, "fillId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let bid = attr_str(e, "borderId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let apply_nf = attr_str(e, "applyNumberFormat").is_none_or(|v| v != "0");
                    let apply_font = attr_str(e, "applyFont").is_some_and(|v| v == "1");
                    let apply_fill = attr_str(e, "applyFill").is_some_and(|v| v == "1");
                    let apply_border = attr_str(e, "applyBorder").is_some_and(|v| v == "1");
                    cur_xf = Some((
                        nf,
                        fid,
                        flid,
                        bid,
                        apply_nf,
                        apply_font,
                        apply_fill,
                        apply_border,
                        None,
                    ));
                }
                "alignment" if cur_xf.is_some() => {
                    if let Some((_, _, _, _, _, _, _, _, align)) = &mut cur_xf
                        && let Some(h) = attr_str(e, "horizontal")
                    {
                        *align = match h.as_str() {
                            "left" => Some(HorizontalAlign::Left),
                            "center" => Some(HorizontalAlign::Center),
                            "right" => Some(HorizontalAlign::Right),
                            _ => None,
                        };
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(ref e)) => match e.local_name().as_ref() {
                "numFmt" => {
                    if let (Some(id), Some(code)) = (
                        attr_str(e, "numFmtId").and_then(|s| s.parse::<u32>().ok()),
                        attr_str(e, "formatCode"),
                    ) {
                        custom_fmts.insert(id, code);
                    }
                }
                "font" if section == Section::Fonts => fonts.push(FontSpec::default()),
                "b" if cur_font.is_some() => {
                    if let Some(f) = &mut cur_font {
                        f.bold = attr_str(e, "val").is_none_or(|v| v != "0");
                    }
                }
                "i" if cur_font.is_some() => {
                    if let Some(f) = &mut cur_font {
                        f.italic = attr_str(e, "val").is_none_or(|v| v != "0");
                    }
                }
                "strike" if cur_font.is_some() => {
                    if let Some(f) = &mut cur_font {
                        f.strike = attr_str(e, "val").is_none_or(|v| v != "0");
                    }
                }
                "color" if cur_font.is_some() => {
                    if let Some(rgb) = attr_str(e, "rgb").and_then(|s| parse_rgb_color(&s))
                        && let Some(f) = &mut cur_font
                    {
                        f.color = Some(rgb);
                    }
                }
                "fill" if section == Section::Fills => fills.push(None),
                "patternFill" if cur_fill.is_some() => {
                    if let Some((solid, _)) = &mut cur_fill {
                        *solid = attr_str(e, "patternType")
                            .is_some_and(|p| p.eq_ignore_ascii_case("solid"));
                    }
                }
                "fgColor" if cur_fill.is_some() => {
                    if let Some((true, color)) = &mut cur_fill
                        && let Some(rgb) = attr_str(e, "rgb").and_then(|s| parse_rgb_color(&s))
                    {
                        *color = Some(rgb);
                    }
                }
                "border" if section == Section::Borders => borders.push(BorderSpec::default()),
                "top" | "right" | "bottom" | "left" if cur_border.is_some() => {
                    if let Some(b_spec) = &mut cur_border
                        && let Some(style) = attr_str(e, "style")
                    {
                        let w = parse_border_weight(&style);
                        match e.local_name().as_ref() {
                            "top" => b_spec.top = w,
                            "right" => b_spec.right = w,
                            "bottom" => b_spec.bottom = w,
                            "left" => b_spec.left = w,
                            _ => {}
                        }
                    }
                }
                "xf" if section == Section::CellXfs => {
                    let nf = attr_str(e, "numFmtId")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    let fid = attr_str(e, "fontId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let flid = attr_str(e, "fillId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let bid = attr_str(e, "borderId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let apply_nf = attr_str(e, "applyNumberFormat").is_none_or(|v| v != "0");
                    let apply_font = attr_str(e, "applyFont").is_some_and(|v| v == "1");
                    let apply_fill = attr_str(e, "applyFill").is_some_and(|v| v == "1");
                    let apply_border = attr_str(e, "applyBorder").is_some_and(|v| v == "1");
                    xfs.push(build_scanned_xf(
                        nf,
                        fid,
                        flid,
                        bid,
                        apply_nf,
                        apply_font,
                        apply_fill,
                        apply_border,
                        None,
                        &custom_fmts,
                        &fonts,
                        &fills,
                        &borders,
                    ));
                }
                "alignment" if cur_xf.is_some() => {
                    if let Some((_, _, _, _, _, _, _, _, align)) = &mut cur_xf
                        && let Some(h) = attr_str(e, "horizontal")
                    {
                        *align = match h.as_str() {
                            "left" => Some(HorizontalAlign::Left),
                            "center" => Some(HorizontalAlign::Center),
                            "right" => Some(HorizontalAlign::Right),
                            _ => None,
                        };
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                "fonts" | "fills" | "borders" | "cellXfs" => section = Section::None,
                "font" if section == Section::Fonts => {
                    if let Some(f) = cur_font.take() {
                        fonts.push(f);
                    }
                }
                "fill" if section == Section::Fills => {
                    if let Some((_, color)) = cur_fill.take() {
                        fills.push(color);
                    }
                }
                "border" if section == Section::Borders => {
                    if let Some(b_spec) = cur_border.take() {
                        borders.push(b_spec);
                    }
                }
                "xf" if section == Section::CellXfs => {
                    if let Some((
                        nf,
                        fid,
                        flid,
                        bid,
                        apply_nf,
                        apply_font,
                        apply_fill,
                        apply_border,
                        align,
                    )) = cur_xf.take()
                    {
                        xfs.push(build_scanned_xf(
                            nf,
                            fid,
                            flid,
                            bid,
                            apply_nf,
                            apply_font,
                            apply_fill,
                            apply_border,
                            align,
                            &custom_fmts,
                            &fonts,
                            &fills,
                            &borders,
                        ));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(XlsxError::Xml(format!("styles.xml: {e}"))),
            _ => {}
        }
    }

    Ok(xfs)
}

#[expect(clippy::too_many_arguments)]
fn build_scanned_xf(
    num_fmt_id: u32,
    font_id: usize,
    fill_id: usize,
    border_id: usize,
    apply_nf: bool,
    apply_font: bool,
    apply_fill: bool,
    apply_border: bool,
    align: Option<HorizontalAlign>,
    custom_fmts: &BTreeMap<u32, String>,
    fonts: &[FontSpec],
    fills: &[Option<ColorValue>],
    borders: &[BorderSpec],
) -> ScannedXf {
    let (number_format, is_date_fmt) = if num_fmt_id == 0 && !apply_nf {
        (None, false)
    } else if let Some(code) = custom_fmts.get(&num_fmt_id) {
        let nf = parse_format_code(code).filter(|f| *f != NumberFormat::General);
        let is_date = code_looks_like_date_or_time(code);
        (nf, is_date)
    } else {
        let (nf, is_date) = builtin_num_fmt(num_fmt_id);
        (nf.filter(|f| *f != NumberFormat::General), is_date)
    };

    let mut patch = StylePatch::default();
    if let Some(font) = fonts.get(font_id)
        && (apply_font || font.bold || font.italic || font.strike || font.color.is_some())
    {
        if font.bold {
            patch.bold = Some(true);
        }
        if font.italic {
            patch.italic = Some(true);
        }
        if font.strike {
            patch.strikethrough = Some(true);
        }
        if let Some(c) = font.color {
            patch.text_color = ColorPatch::Set(c);
        }
    }
    if apply_fill && let Some(Some(c)) = fills.get(fill_id) {
        patch.fill_color = ColorPatch::Set(*c);
    }
    if let Some(b) = borders.get(border_id)
        && (apply_border
            || b.top != BorderWeight::None
            || b.right != BorderWeight::None
            || b.bottom != BorderWeight::None
            || b.left != BorderWeight::None)
    {
        if b.top != BorderWeight::None {
            patch.border_top = Some(b.top);
        }
        if b.right != BorderWeight::None {
            patch.border_right = Some(b.right);
        }
        if b.bottom != BorderWeight::None {
            patch.border_bottom = Some(b.bottom);
        }
        if b.left != BorderWeight::None {
            patch.border_left = Some(b.left);
        }
    }
    patch.align = align;

    ScannedXf {
        number_format,
        is_date_fmt,
        style_patch: patch,
    }
}

fn parse_cached_error(s: &str) -> CalcError {
    match s.trim() {
        "#DIV/0!" => CalcError::Div0,
        "#N/A" => CalcError::Na,
        "#NAME?" => CalcError::Name,
        "#NULL!" => CalcError::Null,
        "#NUM!" => CalcError::Num,
        "#REF!" => CalcError::Ref,
        "#SPILL!" => CalcError::Spill,
        "#CALC!" => CalcError::Calc,
        _ => CalcError::Value,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "single-pass worksheet XML metadata, formulas, caches, and unsupported tags"
)]
fn parse_worksheet_xml(
    name: &str,
    zip_path: &str,
    xml: &str,
    shared_strings: &[String],
    _xfs: &[ScannedXf],
) -> Result<ScannedSheet, XlsxError> {
    let mut reader = Reader::from_str(xml);
    let mut out = ScannedSheet {
        name: name.to_owned(),
        zip_path: zip_path.to_owned(),
        ..ScannedSheet::default()
    };

    #[derive(Default)]
    struct CellAccum {
        row: u32,
        col: u32,
        xf_idx: Option<u32>,
        cm_idx: u32,
        cell_type: Option<String>,
        formula_type: Option<String>,
        formula_ref: Option<(u32, u32, u32, u32)>,
        formula_text: Option<String>,
        v_text: Option<String>,
        is_text: Option<String>,
    }

    let mut cur_cell: Option<CellAccum> = None;
    let mut in_f = false;
    let mut in_v = false;
    let mut in_is = false;
    let mut in_is_t = false;
    let mut rph_depth = 0_u32;
    let mut fallback_row = 0_u32;
    let mut fallback_col = 0_u32;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "pane" => handle_pane(e, &mut out),
                "col" => handle_col(e, &mut out),
                "row" => {
                    fallback_row = handle_row(e, &mut out, fallback_row);
                    fallback_col = 0;
                }
                "c" => {
                    let coord = attr_str(e, "r")
                        .and_then(|s| parse_a1_ref(&s))
                        .unwrap_or((fallback_row, fallback_col));
                    fallback_col = coord.1.saturating_add(1);
                    cur_cell = Some(CellAccum {
                        row: coord.0,
                        col: coord.1,
                        xf_idx: attr_str(e, "s").and_then(|s| s.parse::<u32>().ok()),
                        cm_idx: attr_str(e, "cm")
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(0),
                        cell_type: attr_str(e, "t"),
                        ..CellAccum::default()
                    });
                }
                "f" if cur_cell.is_some() => {
                    in_f = true;
                    if let Some(c) = &mut cur_cell {
                        c.formula_type = attr_str(e, "t");
                        c.formula_ref = attr_str(e, "ref").and_then(|r| parse_a1_range(&r));
                        c.formula_text = Some(String::new());
                    }
                }
                "v" if cur_cell.is_some() => {
                    in_v = true;
                    if let Some(c) = &mut cur_cell {
                        c.v_text = Some(String::new());
                    }
                }
                "is" if cur_cell.is_some() => {
                    in_is = true;
                    if let Some(c) = &mut cur_cell {
                        c.is_text = Some(String::new());
                    }
                }
                "rPh" if in_is => rph_depth = rph_depth.saturating_add(1),
                "t" if in_is && rph_depth == 0 => in_is_t = true,
                "mergeCell" => out.merged_cells_count = out.merged_cells_count.saturating_add(1),
                "conditionalFormatting" => {
                    out.conditional_fmt_count = out.conditional_fmt_count.saturating_add(1);
                }
                "drawing" | "legacyDrawing" => {
                    out.drawing_count = out.drawing_count.saturating_add(1);
                }
                _ => {}
            },
            Ok(Event::Empty(ref e)) => match e.local_name().as_ref() {
                "pane" => handle_pane(e, &mut out),
                "col" => handle_col(e, &mut out),
                "row" => {
                    fallback_row = handle_row(e, &mut out, fallback_row);
                    fallback_col = 0;
                }
                "c" => {
                    let coord = attr_str(e, "r")
                        .and_then(|s| parse_a1_ref(&s))
                        .unwrap_or((fallback_row, fallback_col));
                    fallback_col = coord.1.saturating_add(1);
                    let xf_idx = attr_str(e, "s").and_then(|s| s.parse::<u32>().ok());
                    if xf_idx.is_some() {
                        out.cell_metas.push(ScannedCellMeta {
                            row: coord.0,
                            col: coord.1,
                            xf_idx,
                            cm_idx: 0,
                            formula_text: None,
                            formula_type: None,
                            formula_ref: None,
                            cached_value: None,
                        });
                    }
                }
                "f" if cur_cell.is_some() => {
                    if let Some(c) = &mut cur_cell {
                        c.formula_type = attr_str(e, "t");
                        c.formula_ref = attr_str(e, "ref").and_then(|r| parse_a1_range(&r));
                        c.formula_text = Some(String::new());
                    }
                }
                "v" if cur_cell.is_some() => {
                    if let Some(c) = &mut cur_cell {
                        c.v_text = Some(String::new());
                    }
                }
                "mergeCell" => out.merged_cells_count = out.merged_cells_count.saturating_add(1),
                "conditionalFormatting" => {
                    out.conditional_fmt_count = out.conditional_fmt_count.saturating_add(1);
                }
                "drawing" | "legacyDrawing" => {
                    out.drawing_count = out.drawing_count.saturating_add(1);
                }
                _ => {}
            },
            Ok(Event::Text(e)) if in_f || in_v || (in_is_t && rph_depth == 0) => {
                let content = e.xml10_content();
                let decoded = quick_xml::escape::unescape(&content)
                    .map_err(|err| XlsxError::Xml(err.to_string()))?;
                if let Some(c) = &mut cur_cell {
                    if in_f && let Some(ft) = &mut c.formula_text {
                        ft.push_str(&decoded);
                    } else if in_v && let Some(vt) = &mut c.v_text {
                        vt.push_str(&decoded);
                    } else if in_is_t
                        && rph_depth == 0
                        && let Some(it) = &mut c.is_text
                    {
                        it.push_str(&decoded);
                    }
                }
            }
            Ok(Event::CData(e)) if in_f || in_v || (in_is_t && rph_depth == 0) => {
                if let Some(c) = &mut cur_cell {
                    if in_f && let Some(ft) = &mut c.formula_text {
                        ft.push_str(e.as_ref());
                    } else if in_v && let Some(vt) = &mut c.v_text {
                        vt.push_str(e.as_ref());
                    } else if in_is_t
                        && rph_depth == 0
                        && let Some(it) = &mut c.is_text
                    {
                        it.push_str(e.as_ref());
                    }
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                "f" => in_f = false,
                "v" => in_v = false,
                "rPh" if in_is => rph_depth = rph_depth.saturating_sub(1),
                "t" if in_is => in_is_t = false,
                "is" => {
                    in_is = false;
                    in_is_t = false;
                    rph_depth = 0;
                }
                "c" => {
                    if let Some(c) = cur_cell.take() {
                        let cached = match c.cell_type.as_deref() {
                            Some("s") => c
                                .v_text
                                .as_deref()
                                .and_then(|s| s.trim().parse::<usize>().ok())
                                .and_then(|idx| shared_strings.get(idx))
                                .map(|s| ScalarValue::Text(s.clone())),
                            Some("inlineStr") => c.is_text.map(ScalarValue::Text),
                            Some("str") => c.v_text.map(ScalarValue::Text),
                            Some("b") => c.v_text.as_deref().map(|s| {
                                ScalarValue::Bool(s.trim() == "1" || s.eq_ignore_ascii_case("true"))
                            }),
                            Some("e") => c
                                .v_text
                                .as_deref()
                                .map(|s| ScalarValue::Error(parse_cached_error(s))),
                            _ => c
                                .v_text
                                .as_deref()
                                .and_then(|s| s.trim().parse::<f64>().ok())
                                .filter(|n| n.is_finite())
                                .map(ScalarValue::Number),
                        };
                        if c.xf_idx.is_some() || c.formula_text.is_some() || cached.is_some() {
                            out.cell_metas.push(ScannedCellMeta {
                                row: c.row,
                                col: c.col,
                                xf_idx: c.xf_idx,
                                cm_idx: c.cm_idx,
                                formula_text: c.formula_text,
                                formula_type: c.formula_type,
                                formula_ref: c.formula_ref,
                                cached_value: cached,
                            });
                        }
                    }
                    in_f = false;
                    in_v = false;
                    in_is = false;
                    in_is_t = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(XlsxError::Xml(format!("{zip_path}: {e}"))),
            _ => {}
        }
    }

    Ok(out)
}

fn handle_pane(e: &BytesStart<'_>, out: &mut ScannedSheet) {
    let frozen = attr_str(e, "state").is_some_and(|s| s == "frozen" || s == "frozenSplit");
    if !frozen {
        return;
    }
    if let Some(x) = attr_str(e, "xSplit").and_then(|s| s.parse::<f64>().ok()) {
        let cols = (x.round().max(0.0) as u32).min(MAX_FREEZE_COUNT);
        out.frozen_cols = out.frozen_cols.max(cols);
    }
    if let Some(y) = attr_str(e, "ySplit").and_then(|s| s.parse::<f64>().ok()) {
        let rows = (y.round().max(0.0) as u32).min(MAX_FREEZE_COUNT);
        out.frozen_rows = out.frozen_rows.max(rows);
    }
}

fn handle_col(e: &BytesStart<'_>, out: &mut ScannedSheet) {
    let custom = attr_str(e, "customWidth").is_some_and(|v| v == "1" || v == "true");
    let Some(width_val) = attr_str(e, "width").and_then(|s| s.parse::<f64>().ok()) else {
        return;
    };
    if !custom || !width_val.is_finite() || width_val <= 0.0 {
        return;
    }
    let Some(min_c) = attr_str(e, "min").and_then(|s| s.parse::<u32>().ok()) else {
        return;
    };
    let max_c = attr_str(e, "max")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(min_c);
    let px = ((width_val * 7.0 + 5.0).round() as u16).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
    if max_c.saturating_sub(min_c) <= 1_024 && min_c >= 1 {
        for col_one in min_c..=max_c {
            out.col_widths_px.insert(col_one - 1, px);
        }
    }
}

fn handle_row(e: &BytesStart<'_>, out: &mut ScannedSheet, fallback_row: u32) -> u32 {
    let row_zero = attr_str(e, "r")
        .and_then(|s| s.parse::<u32>().ok())
        .and_then(|r| r.checked_sub(1))
        .unwrap_or(fallback_row);
    let custom = attr_str(e, "customHeight").is_some_and(|v| v == "1" || v == "true");
    if custom
        && let Some(ht) = attr_str(e, "ht").and_then(|s| s.parse::<f64>().ok())
        && ht.is_finite()
        && ht > 0.0
    {
        let px = ((ht * 96.0 / 72.0).round() as u16).clamp(MIN_ROW_HEIGHT, MAX_ROW_HEIGHT);
        out.row_heights_px.insert(row_zero, px);
    }
    row_zero.saturating_add(1)
}
