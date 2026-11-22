//! 10-category `.xlsx` round-trip, unsupported-feature detection, security
//! limits, and streaming `patch_xlsx` test suite (`17-m3a.md` §7).

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write as _};

use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdAllocator, RowId, SheetId};
use reticula_core::ops::{FormulaMode, Op, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_core::value::{CalcError, Value};
use reticula_formula::binder::{BoundExpr, BoundFormula, bind_expr};
use reticula_formula::engine::FormulaEngine;
use reticula_formula::parser::parse_formula;
use reticula_xlsx::{
    ChangedCell, EXCEL_MAX_COLS, EXCEL_MAX_ROWS, ExportOptions, UnsupportedKind,
    export_snapshot_to_xlsx, export_xlsx, export_xlsx_with_options, import_xlsx,
    import_xlsx_to_snapshot, patch_snapshot_to_xlsx, patch_xlsx,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

fn cell_text(sheet: &Sheet, row: u32, col: u32) -> Option<String> {
    match sheet.value(row, col) {
        Value::Text(idx) => Some(sheet.strings().get(idx).to_owned()),
        _ => None,
    }
}

fn read_zip_entry(bytes: &[u8], target: &str) -> Option<Vec<u8>> {
    let mut z = ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut entry = z.by_name(target).ok()?;
    let mut out = Vec::new();
    entry.read_to_end(&mut out).ok()?;
    Some(out)
}

fn read_zip_text(bytes: &[u8], target: &str) -> Option<String> {
    read_zip_entry(bytes, target).and_then(|v| String::from_utf8(v).ok())
}

fn zip_has_entry(bytes: &[u8], target: &str) -> bool {
    ZipArchive::new(Cursor::new(bytes)).is_ok_and(|mut z| z.by_name(target).is_ok())
}

fn eval_edit(engine: &mut FormulaEngine, sheet_idx: u32, row: u32, col: u32, raw: &str) {
    let sid = SheetId(sheet_idx);
    let r = RowId(u64::from(row));
    let c = ColId(u64::from(col));
    let op = engine.build_cell_edit(sid, r, c, raw).expect("edit builds");
    engine.apply_op(op).expect("edit applies");
    engine.recalc_all();
}

#[test]
fn category1_arithmetic_1904_dates_and_formats_round_trip() {
    let mut writer = rust_xlsxwriter::Workbook::new();
    let date_fmt = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd");
    let pct_fmt = rust_xlsxwriter::Format::new().set_num_format("0.00%");
    let ws = writer.add_worksheet();
    ws.write_number(0, 0, 12.5).unwrap();
    ws.write_number(0, 1, 3.0).unwrap();
    ws.write_formula(0, 2, "=(A1+B1)*2").unwrap();
    // Serial 0 under 1904 epoch == 1904-01-01 == 1462 in 1900 epoch.
    ws.write_number_with_format(1, 0, 0.0, &date_fmt).unwrap();
    ws.write_number_with_format(1, 1, 0.1234, &pct_fmt).unwrap();
    let raw_bytes = writer.save_to_buffer().unwrap();

    // Flip `date1904="1"` in `xl/workbook.xml`.
    let mut rz = ZipArchive::new(Cursor::new(&raw_bytes)).unwrap();
    let mut wb_1904 = Vec::new();
    {
        let mut wz = ZipWriter::new(Cursor::new(&mut wb_1904));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for i in 0..rz.len() {
            let mut f = rz.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            if name == "xl/workbook.xml" {
                let mut xml = String::new();
                f.read_to_string(&mut xml).unwrap();
                let patched = xml.replacen("<workbookPr ", "<workbookPr date1904=\"1\" ", 1);
                assert!(patched.contains("date1904=\"1\""));
                wz.start_file(&name, opts).unwrap();
                wz.write_all(patched.as_bytes()).unwrap();
            } else {
                wz.raw_copy_file(f).unwrap();
            }
        }
        wz.finish().unwrap();
    }

    let imported = import_xlsx(&wb_1904).expect("import 1904");
    let s0 = imported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0.value(0, 2), Value::Number(31.0));
    assert_eq!(s0.value(1, 0), Value::Number(1462.0));
    assert_eq!(
        s0.effective_format_by_id(RowId(1), ColId(0)),
        NumberFormat::Date
    );
    assert_eq!(
        s0.effective_format_by_id(RowId(1), ColId(1)),
        NumberFormat::PercentDecimals { decimals: 2 }
    );

    let exported = export_xlsx(&imported.engine).expect("export");
    let reimported = import_xlsx(&exported.bytes).expect("reimport");
    let s0_back = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0_back.value(0, 2), Value::Number(31.0));
    assert_eq!(s0_back.value(1, 0), Value::Number(1462.0));
}

#[test]
fn category2_cross_sheet_references_with_quotes_and_spaces_round_trip() {
    let mut engine = FormulaEngine::new();
    engine
        .apply_op(Op::AddSheet {
            sheet: SheetId(1),
            name: String::from("Sales'Data 26"),
            after: Some(SheetId(0)),
        })
        .unwrap();
    eval_edit(&mut engine, 1, 0, 0, "40");
    eval_edit(&mut engine, 1, 1, 0, "2");
    eval_edit(&mut engine, 0, 0, 0, "=SUM('Sales''Data 26'!A1:A2)");

    let exported = export_xlsx(&engine).expect("export");
    let reimported = import_xlsx(&exported.bytes).expect("reimport");
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0.value(0, 0), Value::Number(42.0));
}

#[test]
fn category3_dynamic_array_spills_and_implicit_intersection_do_not_conflict() {
    let mut engine = FormulaEngine::new();
    eval_edit(&mut engine, 0, 0, 0, "=SEQUENCE(3,2,10,5)");
    eval_edit(&mut engine, 0, 5, 0, "1");
    eval_edit(&mut engine, 0, 6, 0, "2");
    eval_edit(&mut engine, 0, 7, 0, "3");
    eval_edit(&mut engine, 0, 5, 1, "=@A6:A8+10");

    assert_eq!(
        engine.spill_range((SheetId(0), RowId(0), ColId(0))),
        Some((3, 2))
    );
    let exported = export_xlsx(&engine).expect("export");
    let sheet_xml = read_zip_text(&exported.bytes, "xl/worksheets/sheet1.xml").unwrap();
    assert!(sheet_xml.contains("ref=\"A1:B3\""));
    assert!(sheet_xml.contains("_xlfn.SEQUENCE(3,2,10,5)"));

    // Simulate Excel cached spill shadows written inside B1:B3 & A2:A3 alongside XLDAPR anchor.
    let mut rz = ZipArchive::new(Cursor::new(&exported.bytes)).unwrap();
    let mut with_shadows = Vec::new();
    {
        let mut wz = ZipWriter::new(Cursor::new(&mut with_shadows));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for i in 0..rz.len() {
            let mut f = rz.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            if name == "xl/worksheets/sheet1.xml" {
                let mut xml = String::new();
                f.read_to_string(&mut xml).unwrap();
                let patched = xml.replacen("</row>", "<c r=\"B1\"><v>15</v></c></row>", 1);
                wz.start_file(&name, opts).unwrap();
                wz.write_all(patched.as_bytes()).unwrap();
            } else {
                wz.raw_copy_file(f).unwrap();
            }
        }
        wz.finish().unwrap();
    }

    let reimported = import_xlsx(&with_shadows).expect("reimport with cached spill shadow");
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0.value(0, 0), Value::Number(10.0));
    assert_eq!(
        reimported
            .engine
            .spill_range((SheetId(0), RowId(0), ColId(0))),
        Some((3, 2))
    );
    assert_eq!(s0.value(5, 1), Value::Number(11.0));
}

#[test]
fn category4_workbook_and_sheet_local_named_ranges_round_trip() {
    let mut engine = FormulaEngine::new();
    engine
        .apply_op(Op::AddSheet {
            sheet: SheetId(1),
            name: String::from("Rates"),
            after: Some(SheetId(0)),
        })
        .unwrap();
    eval_edit(&mut engine, 0, 0, 0, "100");
    eval_edit(&mut engine, 1, 0, 0, "0.25");

    let g_ast = parse_formula("=Sheet1!$A$1").unwrap();
    let g_bf = BoundFormula {
        mode: FormulaMode::Dynamic,
        root: bind_expr(&g_ast, engine.workbook(), SheetId(0)),
    };
    engine
        .apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from("BaseAmount"),
            formula: g_bf,
            comment: None,
            hidden: false,
        })
        .unwrap();

    let l_ast = parse_formula("=Rates!$A$1").unwrap();
    let l_bf = BoundFormula {
        mode: FormulaMode::Dynamic,
        root: bind_expr(&l_ast, engine.workbook(), SheetId(1)),
    };
    engine
        .apply_op(Op::DefineNamedRange {
            scope: Some(SheetId(1)),
            name: String::from("LocalRate"),
            formula: l_bf,
            comment: None,
            hidden: false,
        })
        .unwrap();

    eval_edit(&mut engine, 0, 1, 0, "=BaseAmount*2");
    eval_edit(&mut engine, 1, 1, 0, "=BaseAmount*LocalRate");

    let exported = export_xlsx(&engine).expect("export");
    let reimported = import_xlsx(&exported.bytes).expect("reimport");
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    let s1 = reimported.engine.workbook().sheet(SheetId(1)).unwrap();
    assert_eq!(s0.value(1, 0), Value::Number(200.0));
    assert_eq!(s1.value(1, 0), Value::Number(25.0));
    assert!(
        reimported
            .engine
            .workbook()
            .named_ranges()
            .iter()
            .any(|(k, d)| k.scope.is_none() && d.name == "BaseAmount")
    );
    assert!(
        reimported
            .engine
            .workbook()
            .named_ranges()
            .iter()
            .any(|(k, d)| k.scope == Some(SheetId(1)) && d.name == "LocalRate")
    );
}

#[test]
fn category5_xlfn_xlws_prefixes_never_double_on_round_trip() {
    let mut engine = FormulaEngine::new();
    eval_edit(&mut engine, 0, 0, 0, "10");
    eval_edit(&mut engine, 0, 1, 0, "20");
    eval_edit(&mut engine, 0, 2, 0, "5");
    eval_edit(&mut engine, 0, 0, 1, "ten");
    eval_edit(&mut engine, 0, 1, 1, "twenty");
    eval_edit(&mut engine, 0, 2, 1, "five");
    eval_edit(&mut engine, 0, 0, 2, "=XLOOKUP(20,A1:A3,B1:B3)");
    eval_edit(&mut engine, 0, 1, 2, "=SORT(FILTER(A1:A3,A1:A3>=10))");

    let exported = export_xlsx(&engine).expect("export");
    let sheet_xml = read_zip_text(&exported.bytes, "xl/worksheets/sheet1.xml").unwrap();
    assert!(sheet_xml.contains("_xlfn.XLOOKUP("));
    assert!(sheet_xml.contains("_xlfn._xlws.SORT("));
    assert!(sheet_xml.contains("_xlfn._xlws.FILTER("));
    assert!(!sheet_xml.contains("_xlfn._xlfn."));

    let reimported = import_xlsx(&exported.bytes).expect("reimport");
    let exported2 = export_xlsx(&reimported.engine).expect("re-export");
    let sheet_xml2 = read_zip_text(&exported2.bytes, "xl/worksheets/sheet1.xml").unwrap();
    assert!(!sheet_xml2.contains("_xlfn._xlfn."));
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(cell_text(s0, 0, 2).as_deref(), Some("twenty"));
    assert_eq!(s0.value(1, 2), Value::Number(10.0));
}

#[test]
fn category6_preserved_opaque_and_cached_formulas_survive_recalc_and_round_trip() {
    let mut writer = rust_xlsxwriter::Workbook::new();
    let ws = writer.add_worksheet();
    ws.write_number(0, 0, 100.0).unwrap();
    ws.write_formula(
        0,
        1,
        rust_xlsxwriter::Formula::new("SUM(Table1[Amount])").set_result("250.5"),
    )
    .unwrap();
    ws.write_formula(
        1,
        1,
        rust_xlsxwriter::Formula::new("SUM(Sheet1:Sheet3!A1)").set_result("OK"),
    )
    .unwrap();
    ws.write_formula(
        2,
        1,
        rust_xlsxwriter::Formula::new("CUSTOM_COMPANY_FN(A1)").set_result("42"),
    )
    .unwrap();
    // Write an uncached structured-ref cell `<c r="B4"><f>Table1[@Total]*2</f></c>`
    // (no `<v>` node) by setting formula and removing B4's default `<v>0</v>`.
    ws.write_formula(3, 1, "Table1[@Total]*2").unwrap();
    let raw_xlsx = writer.save_to_buffer().unwrap();
    let mut rz = ZipArchive::new(Cursor::new(&raw_xlsx)).unwrap();
    let mut xlsx = Vec::new();
    {
        let mut wz = ZipWriter::new(Cursor::new(&mut xlsx));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for i in 0..rz.len() {
            let mut f = rz.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            if name == "xl/worksheets/sheet1.xml" {
                let mut xml = String::new();
                f.read_to_string(&mut xml).unwrap();
                let patched = xml.replacen(
                    "<f>Table1[@Total]*2</f><v>0</v>",
                    "<f>Table1[@Total]*2</f>",
                    1,
                );
                wz.start_file(&name, opts).unwrap();
                wz.write_all(patched.as_bytes()).unwrap();
            } else {
                wz.raw_copy_file(f).unwrap();
            }
        }
        wz.finish().unwrap();
    }

    let imported = import_xlsx(&xlsx).expect("import preserved");
    assert_eq!(imported.report.preserved_formula_count, 4);
    let s0 = imported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0.value(0, 1), Value::Number(250.5));
    assert_eq!(cell_text(s0, 1, 1).as_deref(), Some("OK"));
    assert_eq!(s0.value(2, 1), Value::Number(42.0));
    assert_eq!(s0.value(3, 1), Value::Error(CalcError::Name));

    let formula_c = imported
        .engine
        .workbook()
        .formulas()
        .get(&(SheetId(0), RowId(0), ColId(1)))
        .unwrap();
    assert!(matches!(formula_c.root, BoundExpr::PreservedCached { .. }));
    assert_eq!(
        formula_c.unparse(imported.engine.workbook(), SheetId(0)),
        "=SUM(Table1[Amount])"
    );

    let reexported = export_xlsx(&imported.engine).expect("export preserved");
    let reimported = import_xlsx(&reexported.bytes).expect("reimport preserved");
    let s0_back = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0_back.value(0, 1), Value::Number(250.5));
    assert_eq!(s0_back.value(2, 1), Value::Number(42.0));
}

#[test]
fn category7_large_integers_and_15_digit_precision_boundary_round_trip() {
    let mut engine = FormulaEngine::new();
    eval_edit(&mut engine, 0, 0, 0, "9007199254740991");
    eval_edit(&mut engine, 0, 1, 0, "123456789012345");
    eval_edit(&mut engine, 0, 2, 0, "=0.1+0.2");

    let (snap, rep) = import_xlsx_to_snapshot(&export_xlsx(&engine).unwrap().bytes).unwrap();
    assert_eq!(rep.cell_count, 2);
    assert_eq!(rep.formula_count, 1);

    let exported = export_snapshot_to_xlsx(&snap).unwrap();
    let reimported = import_xlsx(&exported.bytes).unwrap();
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0.value(0, 0), Value::Number(9_007_199_254_740_991.0));
    assert_eq!(s0.value(1, 0), Value::Number(123_456_789_012_345.0));
    let mut formatted = String::new();
    let _ = s0.write_text(2, 0, &mut formatted);
    assert_eq!(formatted, "0.3");
}

#[test]
fn category8_cjk_emoji_xml_special_chars_and_whitespace_round_trip() {
    let mut engine = FormulaEngine::new();
    eval_edit(&mut engine, 0, 0, 0, "净利润 🚀 & <Revenue> \"Q1\"");
    eval_edit(&mut engine, 0, 1, 0, "  padded space  ");

    let exported = export_xlsx(&engine).expect("export utf8");
    let reimported = import_xlsx(&exported.bytes).expect("reimport utf8");
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(
        cell_text(s0, 0, 0).as_deref(),
        Some("净利润 🚀 & <Revenue> \"Q1\"")
    );
    assert_eq!(cell_text(s0, 1, 0).as_deref(), Some("  padded space  "));
}

fn build_rich_original_xlsx_with_unsupported_parts() -> Vec<u8> {
    let mut writer = rust_xlsxwriter::Workbook::new();
    let bold_blue = rust_xlsxwriter::Format::new()
        .set_bold()
        .set_background_color(rust_xlsxwriter::Color::RGB(0xE0F2FE));
    let ws = writer.add_worksheet();
    ws.write_string_with_format(0, 0, "Metric", &bold_blue)
        .unwrap();
    ws.write_number_with_format(1, 0, 100.0, &bold_blue)
        .unwrap();
    ws.write_number(2, 0, 50.0).unwrap();
    ws.write_formula(3, 0, "=SUM(A2:A3)").unwrap();
    let base = writer.save_to_buffer().unwrap();

    let mut rz = ZipArchive::new(Cursor::new(&base)).unwrap();
    let mut rich = Vec::new();
    {
        let mut wz = ZipWriter::new(Cursor::new(&mut rich));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for i in 0..rz.len() {
            let mut f = rz.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            if name == "xl/worksheets/sheet1.xml" {
                let mut xml = String::new();
                f.read_to_string(&mut xml).unwrap();
                let injected = xml.replacen(
                    "</worksheet>",
                    "<mergeCells count=\"1\"><mergeCell ref=\"B1:C1\"/></mergeCells>\
                     <conditionalFormatting sqref=\"A2:A3\"><cfRule type=\"cellIs\" priority=\"1\" operator=\"greaterThan\"><formula>0</formula></cfRule></conditionalFormatting>\
                     <drawing r:id=\"rId1\"/>\
                     </worksheet>",
                    1,
                );
                wz.start_file(&name, opts).unwrap();
                wz.write_all(injected.as_bytes()).unwrap();
            } else if name == "[Content_Types].xml" {
                let mut xml = String::new();
                f.read_to_string(&mut xml).unwrap();
                let injected = xml.replacen(
                    "</Types>",
                    "<Override PartName=\"/xl/calcChain.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml\"/>\
                     <Override PartName=\"/xl/charts/chart1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawingml.chart+xml\"/>\
                     </Types>",
                    1,
                );
                wz.start_file(&name, opts).unwrap();
                wz.write_all(injected.as_bytes()).unwrap();
            } else if name == "xl/_rels/workbook.xml.rels" {
                let mut xml = String::new();
                f.read_to_string(&mut xml).unwrap();
                let injected = xml.replacen(
                    "</Relationships>",
                    "<Relationship Id=\"rId99\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/calcChain\" Target=\"calcChain.xml\"/>\
                     </Relationships>",
                    1,
                );
                wz.start_file(&name, opts).unwrap();
                wz.write_all(injected.as_bytes()).unwrap();
            } else {
                wz.raw_copy_file(f).unwrap();
            }
        }
        for (path, payload) in [
            ("xl/calcChain.xml", b"<calcChain xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><c r=\"A4\" i=\"1\"/></calcChain>".as_slice()),
            ("xl/charts/chart1.xml", b"<c:chartSpace><synthetic-chart/></c:chartSpace>".as_slice()),
            ("xl/drawings/drawing1.xml", b"<xdr:wsDr><synthetic-drawing/></xdr:wsDr>".as_slice()),
            ("xl/media/image1.png", b"\x89PNG\r\n\x1a\nFAKEPNG".as_slice()),
            ("xl/vbaProject.bin", b"FAKE_VBA_PROJECT_BYTES".as_slice()),
            ("xl/pivotTables/pivotTable1.xml", b"<pivotTableDefinition name=\"PT1\"/>".as_slice()),
            ("xl/comments1.xml", b"<comments><commentList/></comments>".as_slice()),
            ("xl/externalLinks/externalLink1.xml", b"<externalLink/>".as_slice()),
        ] {
            wz.start_file(path, opts).unwrap();
            wz.write_all(payload).unwrap();
        }
        wz.finish().unwrap();
    }
    rich
}

#[test]
fn category9_unsupported_detection_and_streaming_patch_xlsx_preserve_all_parts() {
    let orig_xlsx = build_rich_original_xlsx_with_unsupported_parts();
    assert!(zip_has_entry(&orig_xlsx, "xl/calcChain.xml"));

    let mut imported = import_xlsx(&orig_xlsx).expect("import rich xlsx");
    let kinds: Vec<UnsupportedKind> = imported.report.unsupported.iter().map(|u| u.kind).collect();
    for expected in [
        UnsupportedKind::MergedCells,
        UnsupportedKind::ConditionalFormatting,
        UnsupportedKind::DrawingOrImage,
        UnsupportedKind::Chart,
        UnsupportedKind::VbaMacro,
        UnsupportedKind::PivotTable,
        UnsupportedKind::Comment,
        UnsupportedKind::ExternalConnection,
    ] {
        assert!(
            kinds.contains(&expected),
            "missing unsupported feature report: {expected:?}"
        );
    }

    eval_edit(&mut imported.engine, 0, 1, 0, "250");
    eval_edit(&mut imported.engine, 0, 9, 3, "Patched 中文");

    let changed = vec![
        ChangedCell {
            sheet_name: String::from("Sheet1"),
            row: 1,
            col: 0,
        },
        ChangedCell {
            sheet_name: String::from("Sheet1"),
            row: 9,
            col: 3,
        },
    ];
    let patched = patch_xlsx(&orig_xlsx, &imported.engine, &changed).expect("patch_xlsx");

    assert!(!zip_has_entry(&patched.bytes, "xl/calcChain.xml"));
    let ct = read_zip_text(&patched.bytes, "[Content_Types].xml").unwrap();
    assert!(!ct.contains("calcChain.xml"));
    let rels = read_zip_text(&patched.bytes, "xl/_rels/workbook.xml.rels").unwrap();
    assert!(!rels.contains("calcChain.xml"));
    let wb_xml = read_zip_text(&patched.bytes, "xl/workbook.xml").unwrap();
    assert!(wb_xml.contains("fullCalcOnLoad=\"1\""));
    assert_eq!(
        read_zip_entry(&patched.bytes, "xl/charts/chart1.xml").unwrap(),
        b"<c:chartSpace><synthetic-chart/></c:chartSpace>"
    );
    assert_eq!(
        read_zip_entry(&patched.bytes, "xl/vbaProject.bin").unwrap(),
        b"FAKE_VBA_PROJECT_BYTES"
    );
    assert_eq!(
        read_zip_entry(&patched.bytes, "xl/media/image1.png").unwrap(),
        b"\x89PNG\r\n\x1a\nFAKEPNG"
    );

    let sheet_xml = read_zip_text(&patched.bytes, "xl/worksheets/sheet1.xml").unwrap();
    assert!(sheet_xml.contains("<mergeCells"));
    assert!(sheet_xml.contains("<conditionalFormatting"));
    assert!(sheet_xml.contains("inlineStr"));
    assert!(sheet_xml.contains("Patched 中文"));
    assert!(sheet_xml.contains("<c r=\"A2\" s=\""));

    let reimported = import_xlsx(&patched.bytes).expect("reimport patched");
    let s0 = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(s0.value(1, 0), Value::Number(250.0));
    assert_eq!(s0.value(3, 0), Value::Number(300.0));
    assert_eq!(cell_text(s0, 9, 3).as_deref(), Some("Patched 中文"));

    let (snap, _) = import_xlsx_to_snapshot(&orig_xlsx).unwrap();
    let patched_snap = patch_snapshot_to_xlsx(&orig_xlsx, &snap, &changed).unwrap();
    assert!(zip_has_entry(&patched_snap.bytes, "xl/charts/chart1.xml"));
}

#[test]
fn category10a_security_zip_bomb_and_64mib_limits_rejected() {
    use reticula_xlsx::{MAX_XLSX_INPUT_BYTES, XlsxError};

    let huge_compressed = vec![0_u8; MAX_XLSX_INPUT_BYTES + 1];
    assert!(matches!(
        import_xlsx(&huge_compressed),
        Err(XlsxError::InputTooLarge { .. })
    ));

    let mut bomb = Vec::new();
    {
        let mut wz = ZipWriter::new(Cursor::new(&mut bomb));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        wz.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
        let spaces = vec![b' '; 1024 * 1024];
        for _ in 0..257 {
            wz.write_all(&spaces).unwrap();
        }
        wz.finish().unwrap();
    }
    assert!(bomb.len() < 1_000_000);
    assert!(matches!(
        import_xlsx(&bomb),
        Err(XlsxError::UncompressedTooLarge { .. })
    ));
}

#[test]
fn category10b_styles_freeze_dimensions_omit_cache_and_bounds_warning() {
    let mut sheet = Sheet::with_axes(
        SheetId(0),
        String::from("Styled"),
        EXCEL_MAX_ROWS + 5,
        EXCEL_MAX_COLS + 2,
    );
    sheet.set_freeze(2, 1);
    sheet.set_row_height_by_id(RowId(0), Some(48));
    sheet.set_col_width_by_id(ColId(0), Some(180));
    sheet.set_cell_style_patch(
        RowId(0),
        ColId(0),
        StylePatch {
            bold: Some(true),
            italic: Some(true),
            align: Some(HorizontalAlign::Center),
            text_color: ColorPatch::Set(ColorValue::Rgb(0x1D4ED8)),
            fill_color: ColorPatch::Set(ColorValue::Rgb(0xDBEAFE)),
            border_bottom: Some(BorderWeight::Medium),
            ..StylePatch::default()
        },
    );
    let _ = sheet.set_value_by_id(RowId(0), ColId(0), Value::Number(10.0));
    let _ = sheet.set_value_by_id(
        RowId(u64::from(EXCEL_MAX_ROWS)),
        ColId(0),
        Value::Number(99.0),
    );

    let mut formulas = BTreeMap::new();
    let f_ast = parse_formula("=A1*3").unwrap();
    let wb_tmp = Workbook::<BoundFormula>::from_parts(
        vec![sheet.clone()],
        BTreeMap::new(),
        IdAllocator::local(),
        1,
        None,
    )
    .unwrap();
    formulas.insert(
        (SheetId(0), RowId(1), ColId(0)),
        BoundFormula {
            mode: FormulaMode::Dynamic,
            root: bind_expr(&f_ast, &wb_tmp, SheetId(0)),
        },
    );
    let wb = Workbook::from_parts(vec![sheet], formulas, IdAllocator::local(), 1, None).unwrap();
    let engine = FormulaEngine::from_workbook_recalc_all(wb);

    let exported = export_xlsx_with_options(
        &engine,
        ExportOptions {
            omit_cached_values: true,
            full_calc_on_load: true,
        },
    )
    .expect("export with options");
    assert_eq!(exported.warnings.len(), 1);
    assert_eq!(exported.warnings[0].code, "EXCEL_BOUNDS_TRUNCATED");
    assert_eq!(exported.warnings[0].count, 1);

    let s1_xml = read_zip_text(&exported.bytes, "xl/worksheets/sheet1.xml").unwrap();
    assert!(!s1_xml.contains("<f>A1*3</f><v>"));
    assert!(
        read_zip_text(&exported.bytes, "xl/workbook.xml")
            .unwrap()
            .contains("fullCalcOnLoad=\"1\"")
    );

    let reimported = import_xlsx(&exported.bytes).expect("reimport styled");
    let rs = reimported.engine.workbook().sheet(SheetId(0)).unwrap();
    assert_eq!(rs.frozen_rows(), 2);
    assert_eq!(rs.frozen_cols(), 1);
    assert!(rs.row_height_by_id(RowId(0)).is_some_and(|px| px >= 40));
    assert!(rs.col_width_by_id(ColId(0)).is_some_and(|px| px >= 160));
    let st = rs.effective_style_by_id(RowId(0), ColId(0));
    assert!(st.bold);
    assert!(st.italic);
    assert_eq!(st.align, HorizontalAlign::Center);
    assert_eq!(st.text_color, Some(ColorValue::Rgb(0x1D4ED8)));
    assert_eq!(st.fill_color, Some(ColorValue::Rgb(0xDBEAFE)));
    assert_eq!(rs.value(1, 0), Value::Number(30.0));
}

#[test]
fn spike_benchmark_fresh_export_vs_streaming_patch_measurement() {
    use std::time::Instant;

    let mut engine = FormulaEngine::new();
    for r in 0..200_u32 {
        eval_edit(&mut engine, 0, r, 0, &format!("{}", r + 1));
        eval_edit(&mut engine, 0, r, 1, &format!("=A{}*2", r + 1));
    }
    let small_orig = export_xlsx(&engine).unwrap().bytes;

    let rich_orig = {
        let mut rz = ZipArchive::new(Cursor::new(&small_orig)).unwrap();
        let mut out = Vec::new();
        let mut wz = ZipWriter::new(Cursor::new(&mut out));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for i in 0..rz.len() {
            let f = rz.by_index(i).unwrap();
            wz.raw_copy_file(f).unwrap();
        }
        wz.start_file("xl/charts/chart1.xml", opts).unwrap();
        wz.write_all(&vec![b'x'; 8_192]).unwrap();
        wz.start_file("xl/vbaProject.bin", opts).unwrap();
        wz.write_all(&vec![0x5Au8; 16_384]).unwrap();
        wz.finish().unwrap();
        out
    };

    let mut imp_small = import_xlsx(&small_orig).unwrap();
    eval_edit(&mut imp_small.engine, 0, 5, 0, "999");
    let ch_small = vec![ChangedCell {
        sheet_name: String::from("Sheet1"),
        row: 5,
        col: 0,
    }];

    let t0 = Instant::now();
    let res_export_small = export_xlsx(&imp_small.engine).unwrap();
    let export_small_us = t0.elapsed().as_micros();

    let t1 = Instant::now();
    let res_patch_small = patch_xlsx(&small_orig, &imp_small.engine, &ch_small).unwrap();
    let patch_small_us = t1.elapsed().as_micros();

    let mut imp_rich = import_xlsx(&rich_orig).unwrap();
    eval_edit(&mut imp_rich.engine, 0, 5, 0, "999");
    let t2 = Instant::now();
    let res_export_rich = export_xlsx(&imp_rich.engine).unwrap();
    let export_rich_us = t2.elapsed().as_micros();

    let t3 = Instant::now();
    let res_patch_rich = patch_xlsx(&rich_orig, &imp_rich.engine, &ch_small).unwrap();
    let patch_rich_us = t3.elapsed().as_micros();

    eprintln!(
        "XLSX_SPIKE_METRICS: small_orig={} small_export={} ({}us) small_patch={} ({}us) | rich_orig={} rich_export={} ({}us, keeps_vba={}) rich_patch={} ({}us, keeps_vba={})",
        small_orig.len(),
        res_export_small.bytes.len(),
        export_small_us,
        res_patch_small.bytes.len(),
        patch_small_us,
        rich_orig.len(),
        res_export_rich.bytes.len(),
        export_rich_us,
        zip_has_entry(&res_export_rich.bytes, "xl/vbaProject.bin"),
        res_patch_rich.bytes.len(),
        patch_rich_us,
        zip_has_entry(&res_patch_rich.bytes, "xl/vbaProject.bin"),
    );
    assert!(zip_has_entry(&res_patch_rich.bytes, "xl/vbaProject.bin"));
    assert!(!zip_has_entry(&res_export_rich.bytes, "xl/vbaProject.bin"));
}
