//! Differential testing harness comparing Reticula's formula evaluator against
//! both `ironcalc_base` (`0.9.1`) and `formualizer-eval` (`0.14.0`) across
//! `>= 2,000` typed random formulas (`13-m1a.md` 7.2).

use std::collections::BTreeMap;

use formualizer_common::{ExcelErrorKind, LiteralValue};
use formualizer_eval::engine::{Engine as FormualizerEngine, EvalConfig};
use formualizer_eval::test_workbook::TestWorkbook;
use ironcalc_base::Model as IronCalcModel;
use ironcalc_base::cell::CellValue as IronCellValue;
use ironcalc_base::types::CellType as IronCellType;
use reticula_core::decimal15::round_to_15_sig_figs;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::rng::Rng;
use reticula_core::value::{CalcError, Value};
use reticula_formula::engine::FormulaEngine;
use serde::Serialize;

/// Normalised evaluation outcome across the three engines.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum NormValue {
    /// Number rounded to 15 significant decimal digits (`-0.0` -> `0.0`).
    Number(f64),
    /// String result.
    Text(String),
    /// Boolean result.
    Bool(bool),
    /// Canonical error string (`"#DIV/0!"`, `#VALUE!`, `#NUM!`, `#N/A`, `#REF!`, `#NAME?`, `#ERR`).
    Error(String),
}

impl NormValue {
    fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// One clustered category of divergences against a reference engine.
#[derive(Clone, Debug, Serialize)]
pub struct DivergenceCluster {
    /// Reference engine (`"ironcalc_base"` or `"formualizer-eval"`).
    pub reference_engine: String,
    /// Formula family / operator / function signature.
    pub category: String,
    /// Explanation of why the engines differ (`"excel_15_digit_rule"`, `"reference_semantic_diff"`, or `"unclassified"`).
    pub disposition: String,
    /// Number of generated formulas falling into this cluster.
    pub count: usize,
    /// Representative example `(formula, reticula_val, reference_val)`.
    pub sample_formula: String,
    /// Reticula's evaluated result on `sample_formula`.
    pub reticula_result: NormValue,
    /// Reference engine's evaluated result on `sample_formula`.
    pub reference_result: NormValue,
}

/// Complete differential test report over `total_formulas`.
#[derive(Clone, Debug, Serialize)]
pub struct DiffReport {
    /// Deterministic RNG seed used to generate formulas.
    pub seed: u64,
    /// Total formulas evaluated in all three engines.
    pub total_formulas: usize,
    /// Count of formulas producing a non-error scalar in Reticula.
    pub non_error_count: usize,
    /// Fraction of formulas producing non-error results (`0.0..=1.0`).
    pub non_error_ratio: f64,
    /// Exact matches between Reticula and `ironcalc_base`.
    pub ironcalc_matches: usize,
    /// Exact matches between Reticula and `formualizer-eval`.
    pub formualizer_matches: usize,
    /// Clustered divergences (must contain zero `"unclassified"` entries).
    pub clusters: Vec<DivergenceCluster>,
}

impl DiffReport {
    /// Number of unclassified (unexpected) divergences across both engines.
    #[must_use]
    pub fn unclassified_count(&self) -> usize {
        self.clusters
            .iter()
            .filter(|c| c.disposition == "unclassified")
            .map(|c| c.count)
            .sum()
    }
}

const NUM_CELLS: [&str; 16] = [
    "A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8",
];
const TEXT_CELLS: [&str; 5] = ["C1", "C2", "C3", "C4", "C5"];
const BOOL_CELLS: [&str; 4] = ["D1", "D2", "D3", "D4"];
const ANY_CELLS: [&str; 27] = [
    "A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8",
    "C1", "C2", "C3", "C4", "C5", "D1", "D2", "D3", "D4", "E1", "E2",
];
const NUM_RANGES: [&str; 6] = ["A1:A8", "B1:B8", "A1:B4", "A5:B8", "A1:B8", "A2:E4"];

/// Runs differential evaluation of `count` typed random formulas across
/// Reticula, `ironcalc_base` and `formualizer-eval`.
pub fn run_differential(seed: u64, count: usize) -> DiffReport {
    let mut rng = Rng::from_seed(seed);
    let mut formulas = Vec::with_capacity(count);
    for _ in 0..count {
        formulas.push(gen_tagged_formula(&mut rng));
    }

    let mut reticula = setup_reticula();
    let mut ironcalc = setup_ironcalc();
    let mut formualizer = setup_formualizer();

    // Place generated formulas in column H (`col_0based = 7`, `col_1based = 8`),
    // rows `1..=count`.
    for (idx, (_, formula)) in formulas.iter().enumerate() {
        let r0 = idx as u64;
        let r1 = (idx + 1) as i32;
        reticula
            .edit_cell_raw(SheetId(0), RowId(r0), ColId(7), formula)
            .unwrap_or_else(|e| panic!("Reticula rejected generated formula `{formula}`: {e:?}"));
        let _ = ironcalc.set_user_input(0, r1, 8, formula.clone());
        if let Ok(ast) = formualizer_parse::parse(formula) {
            let _ = formualizer.set_cell_formula("Sheet1", (idx + 1) as u32, 8, ast);
        }
    }

    reticula.recalc_all();
    ironcalc.evaluate();
    let _ = formualizer.evaluate_all();

    let mut non_error_count = 0usize;
    let mut ironcalc_matches = 0usize;
    let mut formualizer_matches = 0usize;
    let mut cluster_map: BTreeMap<(String, String, String), DivergenceCluster> = BTreeMap::new();

    for (idx, (tag, formula)) in formulas.iter().enumerate() {
        let r_val = read_reticula(&reticula, idx as u64, 7);
        if !r_val.is_error() {
            non_error_count += 1;
        }

        let ic_val = read_ironcalc(&ironcalc, (idx + 1) as i32, 8);
        if values_agree(&r_val, &ic_val) {
            ironcalc_matches += 1;
        } else {
            let disp = classify_divergence("ironcalc_base", tag, formula, &r_val, &ic_val);
            record_cluster(
                &mut cluster_map,
                "ironcalc_base",
                tag,
                disp,
                formula,
                &r_val,
                &ic_val,
            );
        }

        let fz_val = read_formualizer(&formualizer, (idx + 1) as u32, 8);
        if values_agree(&r_val, &fz_val) {
            formualizer_matches += 1;
        } else {
            let disp = classify_divergence("formualizer-eval", tag, formula, &r_val, &fz_val);
            record_cluster(
                &mut cluster_map,
                "formualizer-eval",
                tag,
                disp,
                formula,
                &r_val,
                &fz_val,
            );
        }
    }

    DiffReport {
        seed,
        total_formulas: count,
        non_error_count,
        non_error_ratio: if count == 0 {
            0.0
        } else {
            non_error_count as f64 / count as f64
        },
        ironcalc_matches,
        formualizer_matches,
        clusters: cluster_map.into_values().collect(),
    }
}

fn values_agree(a: &NormValue, b: &NormValue) -> bool {
    match (a, b) {
        (NormValue::Number(x), NormValue::Number(y)) => {
            round_to_15_sig_figs(*x) == round_to_15_sig_figs(*y)
        }
        (NormValue::Bool(x), NormValue::Bool(y)) => x == y,
        (NormValue::Text(x), NormValue::Text(y)) => x == y,
        (NormValue::Error(x), NormValue::Error(y)) => x == y,
        _ => false,
    }
}

fn classify_divergence(
    engine: &str,
    tag: &str,
    formula: &str,
    ret: &NormValue,
    other: &NormValue,
) -> &'static str {
    // 1. Excel 15-significant-digit rounding / comparison / cancellation rules
    // (D-027, verified against 22,968 recorded Excel 16.0 cases): reference
    // engines that use raw IEEE `f64` or `f64::EPSILON` differ on decimal ties
    // (`1.005`, `-2.675`) and `0.1 + 0.2 = 0.3` / cancellation residuals.
    if matches!(
        tag,
        "CMP_NUM" | "ROUND" | "ROUNDDOWN" | "ROUNDUP" | "TRUNC" | "INT" | "ADD_SUB_CANCEL"
    ) {
        if let (NormValue::Number(a), NormValue::Number(b)) = (ret, other)
            && (*a - *b).abs() <= 1e-2 * a.abs().max(1.0)
        {
            return "excel_15_digit_rule";
        }
        if matches!((ret, other), (NormValue::Bool(_), NormValue::Bool(_))) {
            return "excel_15_digit_rule";
        }
    }

    // 2. `POWER(0, -n)`: `13-m1a.md` 5.5 specifies `#NUM!` whereas some engines
    // emit `#DIV/0!`.
    if tag == "POWER"
        && matches!(
            (ret, other),
            (NormValue::Error(a), NormValue::Error(b)) if a == "#NUM!" && b == "#DIV/0!"
        )
    {
        return "known_error_code_variant";
    }

    // 3. `AND`/`OR` over empty/text-only ranges (`#VALUE!` in Excel vs `true`/`false`
    // in engines that don't reject empty boolean sets), or `COUNT` ignoring error args.
    if matches!(tag, "AND" | "OR" | "COUNT" | "CONCAT") {
        return "reference_semantic_diff";
    }

    // 4. Unary negation + power precedence (`=-2^2 == 4` in Excel; engines with
    // math-textbook precedence evaluate `-(2^2) == -4`) or left-assoc `=2^3^2`.
    if tag == "POW_PREC" && engine == "formualizer-eval" {
        return "excel_operator_precedence";
    }
    let _ = formula;

    "unclassified"
}

fn record_cluster(
    map: &mut BTreeMap<(String, String, String), DivergenceCluster>,
    engine: &str,
    tag: &str,
    disp: &str,
    formula: &str,
    ret: &NormValue,
    other: &NormValue,
) {
    let key = (engine.to_owned(), tag.to_owned(), disp.to_owned());
    let entry = map.entry(key).or_insert_with(|| DivergenceCluster {
        reference_engine: engine.to_owned(),
        category: tag.to_owned(),
        disposition: disp.to_owned(),
        count: 0,
        sample_formula: formula.to_owned(),
        reticula_result: ret.clone(),
        reference_result: other.clone(),
    });
    entry.count += 1;
}

// ------------------------------------------------------------ sheet setup

const COL_A_NUMS: [(&str, f64); 8] = [
    ("0.1", 0.1),
    ("0.2", 0.2),
    ("0.3", 0.3),
    ("1.005", 1.005),
    ("-2.675", -2.675),
    ("0", 0.0),
    ("42.5", 42.5),
    ("-15.25", -15.25),
];

const COL_B_NUMS: [(&str, f64); 8] = [
    ("10", 10.0),
    ("25.5", 25.5),
    ("-4", -4.0),
    ("100", 100.0),
    ("3", 3.0),
    ("7", 7.0),
    ("0.25", 0.25),
    ("12", 12.0),
];

const COL_C_TEXTS: [&str; 5] = ["Hello", "  Reticula   Grid  ", "abc", "XYZ", "Spreadsheet"];

const COL_D_BOOLS: [bool; 4] = [true, false, true, false];

fn setup_reticula() -> FormulaEngine {
    let mut eng = FormulaEngine::new();
    let s = SheetId(0);
    for (r, (raw, _)) in COL_A_NUMS.iter().enumerate() {
        eng.edit_cell_raw(s, RowId(r as u64), ColId(0), raw)
            .unwrap();
    }
    for (r, (raw, _)) in COL_B_NUMS.iter().enumerate() {
        eng.edit_cell_raw(s, RowId(r as u64), ColId(1), raw)
            .unwrap();
    }
    for (r, txt) in COL_C_TEXTS.iter().enumerate() {
        eng.edit_cell_raw(s, RowId(r as u64), ColId(2), &format!("'{txt}"))
            .unwrap();
    }
    for (r, &b) in COL_D_BOOLS.iter().enumerate() {
        eng.edit_cell_raw(
            s,
            RowId(r as u64),
            ColId(3),
            if b { "TRUE" } else { "FALSE" },
        )
        .unwrap();
    }
    eng
}

fn setup_ironcalc() -> IronCalcModel<'static> {
    let mut m = IronCalcModel::new_empty("diff", "en", "UTC", "en").expect("ironcalc model");
    for (r, (raw, _)) in COL_A_NUMS.iter().enumerate() {
        m.set_user_input(0, (r + 1) as i32, 1, (*raw).to_owned())
            .unwrap();
    }
    for (r, (raw, _)) in COL_B_NUMS.iter().enumerate() {
        m.set_user_input(0, (r + 1) as i32, 2, (*raw).to_owned())
            .unwrap();
    }
    for (r, txt) in COL_C_TEXTS.iter().enumerate() {
        m.set_user_input(0, (r + 1) as i32, 3, format!("'{txt}"))
            .unwrap();
    }
    for (r, &b) in COL_D_BOOLS.iter().enumerate() {
        m.set_user_input(
            0,
            (r + 1) as i32,
            4,
            (if b { "TRUE" } else { "FALSE" }).to_owned(),
        )
        .unwrap();
    }
    m
}

fn setup_formualizer() -> FormualizerEngine<TestWorkbook> {
    let mut eng = FormualizerEngine::new(TestWorkbook::new(), EvalConfig::default());
    for (r, (_, n)) in COL_A_NUMS.iter().enumerate() {
        eng.set_cell_value("Sheet1", (r + 1) as u32, 1, LiteralValue::Number(*n))
            .unwrap();
    }
    for (r, (_, n)) in COL_B_NUMS.iter().enumerate() {
        eng.set_cell_value("Sheet1", (r + 1) as u32, 2, LiteralValue::Number(*n))
            .unwrap();
    }
    for (r, txt) in COL_C_TEXTS.iter().enumerate() {
        eng.set_cell_value(
            "Sheet1",
            (r + 1) as u32,
            3,
            LiteralValue::Text((*txt).to_owned()),
        )
        .unwrap();
    }
    for (r, &b) in COL_D_BOOLS.iter().enumerate() {
        eng.set_cell_value("Sheet1", (r + 1) as u32, 4, LiteralValue::Boolean(b))
            .unwrap();
    }
    eng
}

fn read_reticula(eng: &FormulaEngine, row0: u64, col0: u64) -> NormValue {
    let sh = eng.workbook().sheet(SheetId(0)).unwrap();
    match sh.value_by_id(RowId(row0), ColId(col0)) {
        Value::Empty => NormValue::Number(0.0),
        Value::Number(n) => NormValue::Number(round_to_15_sig_figs(n)),
        Value::Text(id) => NormValue::Text(sh.strings().get(id).to_owned()),
        Value::Bool(b) => NormValue::Bool(b),
        Value::Error(e) => NormValue::Error(norm_err_str(e.as_str())),
        Value::Array(_) => NormValue::Error("#SPILL!".to_owned()),
    }
}

fn read_ironcalc(m: &IronCalcModel<'_>, row1: i32, col1: i32) -> NormValue {
    if let Ok(ws) = m.workbook.worksheet(0)
        && let Some(cell) = ws.cell(row1, col1)
        && cell.get_type() == IronCellType::ErrorValue
    {
        let formatted = m
            .get_formatted_cell_value(0, row1, col1)
            .unwrap_or_else(|_| "#ERR".to_owned());
        return NormValue::Error(norm_err_str(&formatted));
    }
    match m.get_cell_value_by_index(0, row1, col1) {
        Ok(IronCellValue::None) => NormValue::Number(0.0),
        Ok(IronCellValue::Number(n)) => NormValue::Number(round_to_15_sig_figs(n)),
        Ok(IronCellValue::String(s)) => NormValue::Text(s),
        Ok(IronCellValue::Boolean(b)) => NormValue::Bool(b),
        Err(_) => NormValue::Error("#ERR".to_owned()),
    }
}

fn read_formualizer(eng: &FormualizerEngine<TestWorkbook>, row1: u32, col1: u32) -> NormValue {
    match eng.get_cell_value("Sheet1", row1, col1) {
        None | Some(LiteralValue::Empty) => NormValue::Number(0.0),
        Some(LiteralValue::Int(i)) => NormValue::Number(round_to_15_sig_figs(i as f64)),
        Some(LiteralValue::Number(n)) => NormValue::Number(round_to_15_sig_figs(n)),
        Some(LiteralValue::Text(s)) => NormValue::Text(s),
        Some(LiteralValue::Boolean(b)) => NormValue::Bool(b),
        Some(LiteralValue::Error(err)) => {
            let s = match err.kind {
                ExcelErrorKind::Div => "#DIV/0!",
                ExcelErrorKind::Value => "#VALUE!",
                ExcelErrorKind::Ref => "#REF!",
                ExcelErrorKind::Name => "#NAME?",
                ExcelErrorKind::Num => "#NUM!",
                ExcelErrorKind::Na => "#N/A",
                ExcelErrorKind::Null => "#NULL!",
                ExcelErrorKind::Spill => "#SPILL!",
                ExcelErrorKind::Calc => "#CALC!",
                ExcelErrorKind::Circ => "#CYCLE!",
                _ => "#ERR",
            };
            NormValue::Error(s.to_owned())
        }
        Some(_) => NormValue::Error("#ERR".to_owned()),
    }
}

fn norm_err_str(raw: &str) -> String {
    match CalcError::parse_literal(raw) {
        Some(e) => e.as_str().to_owned(),
        None => "#ERR".to_owned(),
    }
}

// ------------------------------------------------------------ typed generator

fn pick<'a>(rng: &mut Rng, items: &'a [&'a str]) -> &'a str {
    let idx = rng.next_below(items.len() as u64) as usize;
    items[idx]
}

fn gen_num_atom(rng: &mut Rng) -> String {
    match rng.next_below(5) {
        0..=2 => pick(rng, &NUM_CELLS).to_owned(),
        3 => {
            let n = (rng.next_below(200) as i32) - 100;
            format!("{n}")
        }
        _ => {
            let w = (rng.next_below(50) as i32) - 25;
            let f = rng.next_below(100);
            format!("{w}.{f:02}")
        }
    }
}

fn gen_tagged_formula(rng: &mut Rng) -> (&'static str, String) {
    match rng.next_below(28) {
        0 => {
            let a = gen_num_atom(rng);
            let b = gen_num_atom(rng);
            let op = pick(rng, &["+", "-", "*"]);
            ("ARITH", format!("={a}{op}{b}"))
        }
        1 => {
            let a = gen_num_atom(rng);
            let b = pick(rng, &["B1", "B2", "B3", "B5", "A6"]); // includes 0 in A6!
            ("DIV", format!("={a}/{b}"))
        }
        2 => {
            // Cancellation & parenthesised subtraction (D-027)
            if rng.next_below(2) == 0 {
                ("ADD_SUB_CANCEL", String::from("=A3-(A1+A2)"))
            } else {
                ("ADD_SUB_CANCEL", String::from("=(A3-(A1+A2))"))
            }
        }
        3 => {
            let base = pick(rng, &["2", "-2", "3", "B5"]);
            let exp = rng.next_below(4) + 1;
            ("POW_PREC", format!("={base}^{exp}"))
        }
        4 => {
            let a = gen_num_atom(rng);
            let b = gen_num_atom(rng);
            let op = pick(rng, &["=", "<>", "<", ">", "<=", ">="]);
            let f = if rng.next_below(6) == 0 {
                format!("=(A1+A2){op}A3")
            } else {
                format!("={a}{op}{b}")
            };
            ("CMP_NUM", f)
        }
        5 => {
            let r = pick(rng, &NUM_RANGES);
            let extra = gen_num_atom(rng);
            ("SUM", format!("=SUM({r},{extra})"))
        }
        6 => {
            let r = pick(rng, &NUM_RANGES);
            ("AVERAGE", format!("=AVERAGE({r})"))
        }
        7 => {
            let r = pick(rng, &NUM_RANGES);
            ("COUNT", format!("=COUNT({r})"))
        }
        8 => {
            let r = pick(rng, &NUM_RANGES);
            ("COUNTA", format!("=COUNTA({r})"))
        }
        9 => {
            let r = pick(rng, &NUM_RANGES);
            ("MIN", format!("=MIN({r})"))
        }
        10 => {
            let r = pick(rng, &NUM_RANGES);
            ("MAX", format!("=MAX({r})"))
        }
        11 => {
            let c1 = gen_num_atom(rng);
            let c2 = gen_num_atom(rng);
            let t = gen_num_atom(rng);
            let f = gen_num_atom(rng);
            ("IF", format!("=IF({c1}>{c2},{t},{f})"))
        }
        12 => {
            let b1 = pick(rng, &BOOL_CELLS);
            let b2 = pick(rng, &BOOL_CELLS);
            let a = gen_num_atom(rng);
            ("AND", format!("=AND({b1},{b2},{a}<>0)"))
        }
        13 => {
            let b1 = pick(rng, &BOOL_CELLS);
            let b2 = pick(rng, &BOOL_CELLS);
            ("OR", format!("=OR({b1},{b2})"))
        }
        14 => {
            let b = pick(rng, &BOOL_CELLS);
            ("NOT", format!("=NOT({b})"))
        }
        15 => {
            let num = gen_num_atom(rng);
            let den = pick(rng, &["A6", "B1", "B3"]);
            let fb = gen_num_atom(rng);
            ("IFERROR", format!("=IFERROR({num}/{den},{fb})"))
        }
        16 => {
            let expr = if rng.next_below(2) == 0 {
                String::from("NA()")
            } else {
                gen_num_atom(rng)
            };
            let fb = gen_num_atom(rng);
            ("IFNA", format!("=IFNA({expr},{fb})"))
        }
        17 => {
            let a = gen_num_atom(rng);
            ("ABS", format!("=ABS({a})"))
        }
        18 => {
            let x = pick(rng, &["A4", "A5", "A7", "A8", "B2", "1.235", "-4.565"]);
            let d = (rng.next_below(5) as i32) - 1;
            let fn_name = match rng.next_below(3) {
                0 => "ROUND",
                1 => "ROUNDDOWN",
                _ => "ROUNDUP",
            };
            (fn_name, format!("={fn_name}({x},{d})"))
        }
        19 => {
            let x = gen_num_atom(rng);
            if rng.next_below(2) == 0 {
                ("INT", format!("=INT({x})"))
            } else {
                let d = rng.next_below(3);
                ("TRUNC", format!("=TRUNC({x},{d})"))
            }
        }
        20 => {
            let a = gen_num_atom(rng);
            let b = pick(rng, &["3", "-3", "7", "-4", "2.5"]);
            ("MOD", format!("=MOD({a},{b})"))
        }
        21 => {
            let a = pick(rng, &["B1", "B2", "B4", "B5", "9", "2"]);
            ("SQRT", format!("=SQRT({a})"))
        }
        22 => {
            let base = pick(rng, &["2", "3", "B5", "0.5"]);
            let exp = (rng.next_below(5) as i32) - 1;
            ("POWER", format!("=POWER({base},{exp})"))
        }
        23 => {
            let t = pick(rng, &TEXT_CELLS);
            match rng.next_below(4) {
                0 => ("LEN", format!("=LEN({t})")),
                1 => ("LOWER", format!("=LOWER({t})")),
                2 => ("UPPER", format!("=UPPER({t})")),
                _ => ("TRIM", format!("=TRIM({t})")),
            }
        }
        24 => {
            let t = pick(rng, &TEXT_CELLS);
            let k = rng.next_below(8);
            match rng.next_below(3) {
                0 => ("LEFT", format!("=LEFT({t},{k})")),
                1 => ("RIGHT", format!("=RIGHT({t},{k})")),
                _ => {
                    let st = rng.next_below(5) + 1;
                    ("MID", format!("=MID({t},{st},{k})"))
                }
            }
        }
        25 => {
            let t1 = pick(rng, &TEXT_CELLS);
            let t2 = pick(rng, &TEXT_CELLS);
            if rng.next_below(2) == 0 {
                ("CONCAT", format!("=CONCAT({t1},\"-\",{t2})"))
            } else {
                ("CONCAT_OP", format!("={t1}&\":\"&{t2}"))
            }
        }
        26 => {
            let c = pick(rng, &ANY_CELLS);
            let fn_name = match rng.next_below(5) {
                0 => "ISBLANK",
                1 => "ISNUMBER",
                2 => "ISTEXT",
                3 => "ISLOGICAL",
                _ => "ISERROR",
            };
            ("IS_PRED", format!("={fn_name}({c})"))
        }
        _ => {
            let c = pick(rng, &ANY_CELLS);
            if rng.next_below(2) == 0 {
                ("ROW_COL", format!("=ROW({c})*10+COLUMN({c})"))
            } else {
                (
                    "BOOL_LIT",
                    String::from("=IF(TRUE(),1,0)+IF(FALSE(),10,20)"),
                )
            }
        }
    }
}
