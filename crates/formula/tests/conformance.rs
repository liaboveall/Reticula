//! Structured formula conformance test runner covering all 193 unique built-in
//! functions (200 catalog entries) on both native and `wasm32-unknown-unknown`.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use std::collections::BTreeMap;

use reticula_core::decimal15::round_to_15_sig_figs;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_core::value::Value;
use reticula_formula::engine::FormulaEngine;
use reticula_formula::functions::catalog::FUNCTION_CATALOG;

const SUITES: &[(&str, &str)] = &[
    (
        "layer0_core.tsv",
        include_str!("../../../tests/conformance/layer0_core.tsv"),
    ),
    (
        "lookup_reference.tsv",
        include_str!("../../../tests/conformance/lookup_reference.tsv"),
    ),
    (
        "criteria.tsv",
        include_str!("../../../tests/conformance/criteria.tsv"),
    ),
    (
        "datetime.tsv",
        include_str!("../../../tests/conformance/datetime.tsv"),
    ),
    (
        "rounding_text_volatile.tsv",
        include_str!("../../../tests/conformance/rounding_text_volatile.tsv"),
    ),
    (
        "m3a_math_trig.tsv",
        include_str!("../../../tests/conformance/m3a_math_trig.tsv"),
    ),
    (
        "m3a_text.tsv",
        include_str!("../../../tests/conformance/m3a_text.tsv"),
    ),
    (
        "m3a_datetime.tsv",
        include_str!("../../../tests/conformance/m3a_datetime.tsv"),
    ),
    (
        "m3a_stats_financial.tsv",
        include_str!("../../../tests/conformance/m3a_stats_financial.tsv"),
    ),
    (
        "m3a_lookup_logic.tsv",
        include_str!("../../../tests/conformance/m3a_lookup_logic.tsv"),
    ),
    (
        "m3a_criteria_dynamic.tsv",
        include_str!("../../../tests/conformance/m3a_criteria_dynamic.tsv"),
    ),
];

fn build_fixture_engine() -> FormulaEngine {
    let mut eng = FormulaEngine::new();
    let s0 = SheetId(0);
    let s1 = eng.alloc_sheet_id();
    eng.apply_op(Op::AddSheet {
        sheet: s1,
        name: String::from("Data"),
        after: Some(s0),
    })
    .unwrap();

    // Column A (`0`): 0.1, 0.2, 0.3, 1.005, -2.675, 0
    for (r, raw) in ["0.1", "0.2", "0.3", "1.005", "-2.675", "0"]
        .iter()
        .enumerate()
    {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(0), raw)
            .unwrap();
    }
    // Column B (`1`): 10, 25.5, -4, 100, 3, 7
    for (r, raw) in ["10", "25.5", "-4", "100", "3", "7"].iter().enumerate() {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(1), raw)
            .unwrap();
    }
    // Column C (`2`): strings + C6 formula returning `""`
    for (r, txt) in ["Hello", "  Reticula   Grid  ", "abc", "XYZ", "Spreadsheet"]
        .iter()
        .enumerate()
    {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(2), &format!("'{txt}"))
            .unwrap();
    }
    eng.edit_cell_raw(s0, RowId(5), ColId(2), "=\"\"").unwrap();

    // Column D (`3`): TRUE, FALSE, TRUE, FALSE
    for (r, raw) in ["TRUE", "FALSE", "TRUE", "FALSE"].iter().enumerate() {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(3), raw)
            .unwrap();
    }
    // Column E (`4`): E1 = 50, E2..E4 intentionally blank
    eng.edit_cell_raw(s0, RowId(0), ColId(4), "50").unwrap();

    // Column F (`5`): wildcard literals + S3b 1-ULP bitwise test pair `(1.0, 1.0 + eps)`
    eng.edit_cell_raw(s0, RowId(0), ColId(5), "'item*0")
        .unwrap();
    eng.edit_cell_raw(s0, RowId(1), ColId(5), "'item*1")
        .unwrap();
    eng.edit_cell_raw(s0, RowId(2), ColId(5), "'item?2")
        .unwrap();
    let one_exact = 1.0_f64;
    let one_plus_ulp = f64::from_bits(1.0_f64.to_bits() + 1);
    eng.apply_op(Op::SetCells {
        sheet: s0,
        edits: vec![
            CellEdit {
                row: RowId(3),
                col: ColId(5),
                input: CellInput::Scalar(ScalarValue::Number(one_exact)),
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(4),
                col: ColId(5),
                input: CellInput::Scalar(ScalarValue::Number(one_plus_ulp)),
                format: FormatPatch::Keep,
            },
        ],
    })
    .unwrap();

    // Column G (`6`) & H (`7`): sorted lookup table
    for (r, (num_s, word)) in [
        ("10", "ten"),
        ("20", "twenty"),
        ("30", "thirty"),
        ("40", "forty"),
        ("50", "fifty"),
    ]
    .iter()
    .enumerate()
    {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(6), num_s)
            .unwrap();
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(7), &format!("'{word}"))
            .unwrap();
    }

    // Columns I..K (`8..=10`), rows 1..2: horizontal lookup table
    for (c_off, (k, v)) in [("K1", "110"), ("K2", "220"), ("K3", "330")]
        .iter()
        .enumerate()
    {
        eng.edit_cell_raw(s0, RowId(0), ColId(8 + c_off as u64), &format!("'{k}"))
            .unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(8 + c_off as u64), v)
            .unwrap();
    }

    // Column L (`11`): 0.1, 0.2, -0.3 (for S3b `SUMIF` non-zeroing test)
    eng.edit_cell_raw(s0, RowId(0), ColId(11), "0.1").unwrap();
    eng.edit_cell_raw(s0, RowId(1), ColId(11), "0.2").unwrap();
    eng.edit_cell_raw(s0, RowId(2), ColId(11), "-0.3").unwrap();

    // Second sheet `Data` (`s1`): A1:B3
    for (r, (sku, price)) in [("SKU-A", "120"), ("SKU-B", "250"), ("SKU-C", "380")]
        .iter()
        .enumerate()
    {
        eng.edit_cell_raw(s1, RowId(r as u64), ColId(0), &format!("'{sku}"))
            .unwrap();
        eng.edit_cell_raw(s1, RowId(r as u64), ColId(1), price)
            .unwrap();
    }

    // Fixed host clock & PRNG seed matching `rounding_text_volatile.tsv` header.
    eng.set_eval_env(46_345.25, 0x1234_5678_9ABC_DEF0);
    eng.recalc_all();
    eng
}

#[test]
fn all_200_functions_pass_excel_conformance_suites() {
    let mut eng = build_fixture_engine();
    let s0 = SheetId(0);
    let test_col = ColId(15); // Column P
    let mut per_fn_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut row_cursor: u64 = 0;

    let mut pending: Vec<(&str, usize, String, String, String, String, u64)> = Vec::new();
    for &(suite_name, content) in SUITES {
        for (line_idx, raw_line) in content.lines().enumerate() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = raw_line.split('\t').collect();
            assert_eq!(
                parts.len(),
                4,
                "{suite_name}:{}: expected 4 tab-separated columns, got `{raw_line}`",
                line_idx + 1
            );
            let fn_name = parts[0].trim().to_owned();
            let formula = parts[1].trim().to_owned();
            let exp_kind = parts[2].trim().to_owned();
            let exp_val = parts[3].to_owned();

            *per_fn_counts.entry(fn_name.clone()).or_insert(0) += 1;
            eng.edit_cell_raw(s0, RowId(row_cursor), test_col, &formula)
                .unwrap_or_else(|e| {
                    panic!(
                        "{suite_name}:{}: failed to bind `{formula}`: {e:?}",
                        line_idx + 1
                    )
                });
            pending.push((
                suite_name,
                line_idx + 1,
                fn_name,
                formula,
                exp_kind,
                exp_val,
                row_cursor,
            ));
            row_cursor += 1;
        }
    }

    eng.set_eval_env(46_345.25, 0x1234_5678_9ABC_DEF0);
    eng.recalc_all();

    let sheet = eng.workbook().sheet(s0).unwrap();
    for (suite_name, line_no, fn_name, formula, exp_kind, exp_val, r) in pending {
        let actual = sheet.value_by_id(RowId(r), test_col);
        match exp_kind.as_str() {
            "NUM" => {
                let expected_n: f64 = exp_val.trim().parse().unwrap();
                let Value::Number(actual_n) = actual else {
                    panic!(
                        "{suite_name}:{line_no} [{fn_name}] `{formula}`: expected NUM {expected_n}, got {actual:?}"
                    );
                };
                assert_eq!(
                    round_to_15_sig_figs(actual_n),
                    round_to_15_sig_figs(expected_n),
                    "{suite_name}:{line_no} [{fn_name}] `{formula}`"
                );
            }
            "TEXT" => {
                let Value::Text(sid) = actual else {
                    panic!(
                        "{suite_name}:{line_no} [{fn_name}] `{formula}`: expected TEXT `{exp_val}`, got {actual:?}"
                    );
                };
                assert_eq!(
                    sheet.strings().get(sid),
                    exp_val,
                    "{suite_name}:{line_no} [{fn_name}] `{formula}`"
                );
            }
            "BOOL" => {
                let exp_b = exp_val.trim().eq_ignore_ascii_case("TRUE");
                assert_eq!(
                    actual,
                    Value::Bool(exp_b),
                    "{suite_name}:{line_no} [{fn_name}] `{formula}`"
                );
            }
            "ERR" => {
                let Value::Error(err) = actual else {
                    panic!(
                        "{suite_name}:{line_no} [{fn_name}] `{formula}`: expected ERR `{exp_val}`, got {actual:?}"
                    );
                };
                assert_eq!(
                    err.as_str(),
                    exp_val.trim(),
                    "{suite_name}:{line_no} [{fn_name}] `{formula}`"
                );
            }
            other => panic!("unknown expected kind `{other}` in {suite_name}:{line_no}"),
        }
    }

    // Enforce that every one of the 200 catalogued function entries has >= 2 conformance cases!
    assert_eq!(FUNCTION_CATALOG.len(), 200);
    for meta in &FUNCTION_CATALOG {
        let count = per_fn_counts.get(meta.name).copied().unwrap_or(0);
        assert!(
            count >= 2,
            "function `{}` has only {count} conformance cases (minimum 2 required)",
            meta.name
        );
    }
}

#[test]
fn dynamic_spills_nested_subtotal_and_cross_references_match_excel() {
    let mut eng = build_fixture_engine();
    let s0 = SheetId(0);

    // 1. Dynamic array spill from SEQUENCE & TEXTSPLIT & spill `#` consumer.
    eng.edit_cell_raw(s0, RowId(10), ColId(0), "=SEQUENCE(3, 2, 10, 5)")
        .unwrap(); // A11:B13 -> [10,15; 20,25; 30,35]
    eng.edit_cell_raw(
        s0,
        RowId(10),
        ColId(4),
        "=TEXTSPLIT(\"x,y;z\", \",\", \";\")",
    )
    .unwrap(); // E11:F12 -> [x,y; z,#N/A]
    eng.edit_cell_raw(s0, RowId(14), ColId(0), "=SUM(A11#)")
        .unwrap(); // A15 -> 135

    // 2. SUBTOTAL excludes nested SUBTOTAL while summing plain ranges.
    eng.edit_cell_raw(s0, RowId(20), ColId(0), "10").unwrap();
    eng.edit_cell_raw(s0, RowId(21), ColId(0), "20").unwrap();
    eng.edit_cell_raw(s0, RowId(22), ColId(0), "=SUBTOTAL(9, A21:A22)")
        .unwrap(); // 30
    eng.edit_cell_raw(s0, RowId(23), ColId(0), "=SUBTOTAL(9, A21:A23)")
        .unwrap(); // 30 (skips A23 nested SUBTOTAL!)

    // 3. ISFORMULA array spill over mixed cells & INIDRECT dynamic recalc.
    eng.edit_cell_raw(s0, RowId(25), ColId(0), "=ISFORMULA(C5:C6)")
        .unwrap(); // A26:A27 -> [FALSE; TRUE]
    eng.edit_cell_raw(s0, RowId(25), ColId(2), "C1").unwrap();
    eng.edit_cell_raw(s0, RowId(26), ColId(2), "=INDIRECT(C26)")
        .unwrap();

    eng.recalc_all();

    assert_eq!(eng.spill_range((s0, RowId(10), ColId(0))), Some((3, 2)));
    let s = eng.workbook().sheet(s0).unwrap();
    assert_eq!(s.value_by_id(RowId(14), ColId(0)), Value::Number(135.0));
    assert_eq!(s.value_by_id(RowId(22), ColId(0)), Value::Number(30.0));
    assert_eq!(s.value_by_id(RowId(23), ColId(0)), Value::Number(30.0));

    let e11 = eng.inspect_cell(s0, RowId(10), ColId(4)).unwrap();
    let f12 = eng.inspect_cell(s0, RowId(11), ColId(5)).unwrap();
    assert_eq!(e11.display, "x");
    assert_eq!(f12.display, "#N/A");
    assert_eq!(f12.error.as_deref(), Some("#N/A"));

    let a27 = eng.inspect_cell(s0, RowId(26), ColId(0)).unwrap();
    assert_eq!(a27.display, "TRUE");
    let c27 = eng.inspect_cell(s0, RowId(26), ColId(2)).unwrap();
    assert_eq!(c27.display, "Hello");

    // Mutate C26 -> volatile INDIRECT updates automatically.
    eng.edit_cell_raw(s0, RowId(25), ColId(2), "G2").unwrap();
    eng.recalc_all();
    let c27_new = eng.inspect_cell(s0, RowId(26), ColId(2)).unwrap();
    assert_eq!(c27_new.display, "20");
}
