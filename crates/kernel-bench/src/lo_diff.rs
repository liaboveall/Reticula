//! Stratified `>= 50,000`-formula differential evaluation harness comparing
//! Reticula against headless LibreOffice Calc inside the pinned
//! `infra/libreoffice/Dockerfile` container (`17-m3a.md` §8.1).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use reticula_core::decimal15::round_to_15_sig_figs;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{FormulaMode, Op, ScalarValue};
use reticula_core::rng::Rng;
use reticula_core::value::Value;
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_formula::functions::catalog::FUNCTION_CATALOG;
use reticula_formula::value_eval::EvalScalar;
use reticula_xlsx::{ExportOptions, export_xlsx_with_options, read_xlsx_cached_values};
use serde::Serialize;

use crate::diff::{DiffReport, NormValue};

/// Default pinned Docker image built from `infra/libreoffice/Dockerfile`.
pub const DEFAULT_LIBREOFFICE_IMAGE: &str = "reticula-libreoffice:26.8.2";

/// Sentinel formula placed at `Z1` on every generated sheet (`123456 + 789 = 124245`).
pub const SENTINEL_Z1_FORMULA: &str = "=123456+789";
/// Expected value of `Z1` after full recalculation (`124245`).
pub const SENTINEL_Z1_EXPECTED: f64 = 124_245.0;
/// Cross-sheet sentinel formula placed at `Z2` on every generated sheet (`SUM(Data!N1:Q4) = 136`).
pub const SENTINEL_Z2_FORMULA: &str = "=SUM(Data!N1:Q4)";
/// Expected value of `Z2` after full recalculation (`136`).
pub const SENTINEL_Z2_EXPECTED: f64 = 136.0;

const VOLATILE_CLOCK_OR_RNG: [&str; 5] = ["TODAY", "NOW", "RAND", "RANDBETWEEN", "RANDARRAY"];

const DYNAMIC_ARRAY_FUNCTIONS: [&str; 14] = [
    "CHOOSECOLS",
    "CHOOSEROWS",
    "DROP",
    "FILTER",
    "FREQUENCY",
    "HSTACK",
    "SEQUENCE",
    "SORT",
    "SORTBY",
    "TAKE",
    "TEXTSPLIT",
    "TRANSPOSE",
    "UNIQUE",
    "VSTACK",
];

const UNIQUE_ALIASES: [&str; 7] = [
    "CONCATENATE",
    "MODE",
    "PERCENTILE",
    "QUARTILE",
    "RANK",
    "STDEV",
    "VAR",
];

const ARRAY_SLOT_STRIDE: u32 = 8;
const ARRAY_MAX_BOX: u32 = 6;
const ARRAY_SLOT_COLS: [u32; 3] = [0, 8, 16]; // Columns A, I, Q (never touches sentinel Z = col 25)
const ARRAY_SLOTS_PER_SHEET: usize = 300;
const SCALAR_COLS_PER_SHEET: usize = 10; // Columns A..J
const SCALAR_ROWS_PER_SHEET: usize = 300;
const SCALAR_SHEETS_PER_BOOK: usize = 4; // 12,000 scalar formulas per workbook

/// Configuration for [`run_libreoffice_differential`].
#[derive(Clone, Debug)]
pub struct LibreOfficeDiffConfig {
    /// Deterministic RNG seed.
    pub seed: u64,
    /// Stratified formula samples generated per deterministic catalog entry (default `250`).
    pub quota_per_fn: usize,
    /// Docker image tag containing `soffice`.
    pub image: String,
    /// Skip checking/building `infra/libreoffice` when the image is already present.
    pub skip_image_build: bool,
    /// Optional directory where exported input/output `.xlsx` workbooks are persisted
    /// (for example for manual `spikes/excel/diff-excel.ps1` verification).
    pub dump_xlsx_dir: Option<PathBuf>,
    /// Git commit short hash recorded in the report.
    pub commit: String,
    /// UTC date (`YYYY-MM-DD`) recorded in the report.
    pub date: String,
    /// Optional Rust in-memory differential report (`ironcalc_base` + `formualizer-eval`).
    pub rust_reference: Option<DiffReport>,
}

impl Default for LibreOfficeDiffConfig {
    fn default() -> Self {
        Self {
            seed: 0xD1FF_2026_u64,
            quota_per_fn: 250,
            image: DEFAULT_LIBREOFFICE_IMAGE.to_owned(),
            skip_image_build: false,
            dump_xlsx_dir: None,
            commit: String::from("local"),
            date: String::from("2026-11-22"),
            rust_reference: None,
        }
    }
}

/// Per-family agreement breakdown in [`LibreOfficeDiffReport`].
#[derive(Clone, Debug, Serialize)]
pub struct FamilySummary {
    /// Formula family name.
    pub family: String,
    /// Number of formulas evaluated in this family.
    pub total: usize,
    /// Non-error results in Reticula.
    pub non_error: usize,
    /// Exact/15-digit matches with LibreOffice Calc.
    pub matches: usize,
    /// Agreement ratio (`0.0..=1.0`).
    pub agreement_ratio: f64,
    /// Unclassified divergences remaining in this family.
    pub unclassified: usize,
}

/// Per-function agreement breakdown in [`LibreOfficeDiffReport`].
#[derive(Clone, Debug, Serialize)]
pub struct FunctionSummary {
    /// Function or compound category name.
    pub function: String,
    /// Function family.
    pub family: String,
    /// Number of formulas evaluated.
    pub total: usize,
    /// Non-error results in Reticula.
    pub non_error: usize,
    /// Exact/15-digit matches with LibreOffice Calc.
    pub matches: usize,
    /// Agreement ratio (`0.0..=1.0`).
    pub agreement_ratio: f64,
    /// Unclassified divergences remaining for this function.
    pub unclassified: usize,
}

/// Clustered LibreOffice divergence category with human-readable disposition.
#[derive(Clone, Debug, Serialize)]
pub struct LibreOfficeDivergenceCluster {
    /// Reference engine (`"libreoffice-calc"`).
    pub reference_engine: String,
    /// Function/expression category.
    pub category: String,
    /// Function family.
    pub family: String,
    /// Disposition label (or `"unclassified"`).
    pub disposition: String,
    /// Concise root-cause explanation.
    pub reason: String,
    /// Count of formulas in this cluster.
    pub count: usize,
    /// Sample formula text.
    pub sample_formula: String,
    /// Sample Reticula output.
    pub reticula_result: NormValue,
    /// Sample LibreOffice Calc output.
    pub libreoffice_result: NormValue,
}

/// Complete `>= 50,000` stratified LibreOffice differential report (`17-m3a.md` §8.1).
#[derive(Clone, Debug, Serialize)]
pub struct LibreOfficeDiffReport {
    /// Suite identifier (`"m3a-diff"`).
    pub suite: String,
    /// UTC date string.
    pub date: String,
    /// Git commit short hash.
    pub commit: String,
    /// Deterministic RNG seed.
    pub seed: u64,
    /// Quota per deterministic catalog entry.
    pub quota_per_fn: usize,
    /// Exact LibreOffice version string captured from `soffice --version`.
    pub libreoffice_version: String,
    /// Container image used to execute LibreOffice.
    pub container_image: String,
    /// Total formulas compared against LibreOffice.
    pub total_formulas: usize,
    /// Total cells compared including multi-cell spilled arrays.
    pub total_cells_compared: usize,
    /// Number of deterministic catalog entries covered (`195`).
    pub deterministic_functions_covered: usize,
    /// Number of unique deterministic functions covered (`188`).
    pub unique_functions_covered: usize,
    /// Minimum sample count observed across all deterministic catalog entries.
    pub min_samples_per_function: usize,
    /// Non-error scalar/top-left formula count in Reticula.
    pub non_error_count: usize,
    /// Fraction of non-error formulas (`0.0..=1.0`, must be `>= 0.80`).
    pub non_error_ratio: f64,
    /// Number of formulas matching LibreOffice Calc across all cells/15-sig-figs.
    pub matches: usize,
    /// Agreement ratio (`0.0..=1.0`, must be `>= 0.995`).
    pub agreement_ratio: f64,
    /// Unclassified divergence count (must be `0`).
    pub unclassified_count: usize,
    /// Per-family breakdown.
    pub by_family: Vec<FamilySummary>,
    /// Per-function breakdown.
    pub by_function: Vec<FunctionSummary>,
    /// Clustered divergence catalog.
    pub clusters: Vec<LibreOfficeDivergenceCluster>,
    /// Optional bundled Rust reference engine comparison (`ironcalc_base` + `formualizer-eval`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rust_reference: Option<DiffReport>,
}

/// One generated differential test formula.
#[derive(Clone, Debug)]
pub struct GeneratedCase {
    /// Catalog function name (or `"COMPOUND"` for compound expressions).
    pub function: String,
    /// Family identifier.
    pub family: &'static str,
    /// Raw formula string starting with `'='`.
    pub formula: String,
    /// Whether this formula should be placed in an 8x8 dynamic-array slot.
    pub is_dynamic_array: bool,
}

#[derive(Clone, Debug)]
struct PlacedCase {
    sheet_name: String,
    row: u32,
    col: u32,
    case: GeneratedCase,
    reticula_grid: Vec<Vec<NormValue>>,
    reticula_top: NormValue,
}

/// Returns the sorted list of all 195 deterministic built-in catalog entries.
#[must_use]
pub fn deterministic_catalog_functions() -> Vec<&'static str> {
    FUNCTION_CATALOG
        .iter()
        .map(|m| m.name)
        .filter(|name| !VOLATILE_CLOCK_OR_RNG.contains(name))
        .collect()
}

/// Classifies a built-in function name into one reporting family.
#[must_use]
pub fn function_family(name: &str) -> &'static str {
    if name == "COMPOUND" {
        return "compound_expr";
    }
    if DYNAMIC_ARRAY_FUNCTIONS.contains(&name) {
        return "dynamic_array";
    }
    if matches!(
        name,
        "ABS"
            | "ACOS"
            | "ASIN"
            | "ATAN"
            | "ATAN2"
            | "CEILING"
            | "COMBIN"
            | "COS"
            | "COSH"
            | "DEGREES"
            | "EVEN"
            | "EXP"
            | "FACT"
            | "FLOOR"
            | "GCD"
            | "INT"
            | "LCM"
            | "LN"
            | "LOG"
            | "LOG10"
            | "MOD"
            | "MROUND"
            | "ODD"
            | "PERMUT"
            | "PI"
            | "POWER"
            | "PRODUCT"
            | "QUOTIENT"
            | "RADIANS"
            | "ROUND"
            | "ROUNDDOWN"
            | "ROUNDUP"
            | "SIGN"
            | "SIN"
            | "SINH"
            | "SQRT"
            | "SUMSQ"
            | "TAN"
            | "TANH"
            | "TRUNC"
    ) {
        return "math_trig";
    }
    if matches!(name, "SUM" | "AVERAGE" | "COUNT" | "COUNTA" | "MIN" | "MAX") {
        return "aggregation";
    }
    if matches!(
        name,
        "COUNTBLANK"
            | "COUNTIF"
            | "SUMIF"
            | "AVERAGEIF"
            | "COUNTIFS"
            | "SUMIFS"
            | "AVERAGEIFS"
            | "MAXIFS"
            | "MINIFS"
            | "SUMPRODUCT"
            | "SUBTOTAL"
            | "AGGREGATE"
    ) {
        return "criteria_agg";
    }
    if matches!(
        name,
        "ADDRESS"
            | "AREAS"
            | "CHOOSE"
            | "COLUMN"
            | "COLUMNS"
            | "HLOOKUP"
            | "INDEX"
            | "INDIRECT"
            | "LOOKUP"
            | "MATCH"
            | "OFFSET"
            | "ROW"
            | "ROWS"
            | "VLOOKUP"
            | "XLOOKUP"
            | "XMATCH"
    ) {
        return "lookup";
    }
    if matches!(
        name,
        "DATE"
            | "DATEDIF"
            | "DATEVALUE"
            | "DAY"
            | "DAYS"
            | "DAYS360"
            | "EDATE"
            | "EOMONTH"
            | "HOUR"
            | "ISOWEEKNUM"
            | "MINUTE"
            | "MONTH"
            | "NETWORKDAYS"
            | "SECOND"
            | "TIME"
            | "TIMEVALUE"
            | "WEEKDAY"
            | "WEEKNUM"
            | "WORKDAY"
            | "YEAR"
            | "YEARFRAC"
    ) {
        return "datetime";
    }
    if matches!(
        name,
        "CORREL"
            | "LARGE"
            | "MEDIAN"
            | "MODE"
            | "MODE.SNGL"
            | "PERCENTILE"
            | "PERCENTILE.EXC"
            | "PERCENTILE.INC"
            | "QUARTILE"
            | "QUARTILE.EXC"
            | "QUARTILE.INC"
            | "RANK"
            | "RANK.AVG"
            | "RANK.EQ"
            | "SMALL"
            | "STDEV"
            | "STDEV.P"
            | "STDEV.S"
            | "VAR"
            | "VAR.P"
            | "VAR.S"
    ) {
        return "stats";
    }
    if matches!(
        name,
        "FV" | "IRR" | "NPER" | "NPV" | "PMT" | "PV" | "RATE" | "SLN"
    ) {
        return "financial";
    }
    if matches!(
        name,
        "AND"
            | "ERROR.TYPE"
            | "FALSE"
            | "IF"
            | "IFERROR"
            | "IFNA"
            | "IFS"
            | "ISBLANK"
            | "ISERR"
            | "ISERROR"
            | "ISEVEN"
            | "ISFORMULA"
            | "ISLOGICAL"
            | "ISNA"
            | "ISNONTEXT"
            | "ISNUMBER"
            | "ISODD"
            | "ISREF"
            | "NOT"
            | "OR"
            | "SWITCH"
            | "TRUE"
            | "TYPE"
            | "XOR"
    ) {
        return "logic_info";
    }
    "text_conv"
}

/// Generates a deterministic stratified corpus across all 195 deterministic
/// catalog entries (`quota_per_fn` samples each) plus `compound_extra` compound
/// formulas (default `250 * 10 = 2_500`, totaling `51_250` when `quota_per_fn = 250`).
#[must_use]
pub fn generate_stratified_corpus(seed: u64, quota_per_fn: usize) -> Vec<GeneratedCase> {
    let mut rng = Rng::from_seed(seed);
    let funcs = deterministic_catalog_functions();
    let compound_count = quota_per_fn.saturating_mul(10);
    let mut out = Vec::with_capacity(
        funcs
            .len()
            .saturating_mul(quota_per_fn)
            .saturating_add(compound_count),
    );

    for fn_name in funcs {
        let family = function_family(fn_name);
        let is_dyn = DYNAMIC_ARRAY_FUNCTIONS.contains(&fn_name);
        for sample_idx in 0..quota_per_fn {
            let formula = gen_formula_for_function(&mut rng, fn_name, sample_idx);
            out.push(GeneratedCase {
                function: fn_name.to_owned(),
                family,
                formula,
                is_dynamic_array: is_dyn,
            });
        }
    }

    for sample_idx in 0..compound_count {
        let formula = gen_compound_formula(&mut rng, sample_idx);
        out.push(GeneratedCase {
            function: String::from("COMPOUND"),
            family: "compound_expr",
            formula,
            is_dynamic_array: false,
        });
    }

    out
}

/// Populates a workbook with the shared `Data` + `Aux` sandbox fixture and
/// workbook-level named ranges (`TaxRate`, `Scores`, `Grid4x4`).
pub fn create_sandbox_engine() -> FormulaEngine {
    let mut eng = FormulaEngine::new();
    let data_sid = SheetId(0);
    eng.apply_op(Op::RenameSheet {
        sheet: data_sid,
        name: String::from("Data"),
    })
    .expect("rename Data sheet");

    let aux_sid = eng.alloc_sheet_id();
    eng.apply_op(Op::AddSheet {
        sheet: aux_sid,
        name: String::from("Aux"),
        after: Some(data_sid),
    })
    .expect("add Aux sheet");

    let mut put = |sheet: SheetId, r: u64, c: u64, raw: &str| {
        eng.edit_cell_raw(sheet, RowId(r), ColId(c), raw)
            .unwrap_or_else(|e| panic!("fixture cell ({sheet:?}, {r}, {c}) `{raw}` failed: {e:?}"));
    };

    // Col A (0): D-027 boundaries, decimals, zero, negatives, integers
    let col_a = [
        "0.1",
        "0.2",
        "0.3",
        "1.005",
        "-2.675",
        "0",
        "42.5",
        "-15.25",
        "100",
        "1.5",
        "2.5",
        "3.5",
        "999999999999999",
        "0.000001",
        "-0.5",
        "16",
    ];
    for (r, v) in col_a.iter().enumerate() {
        put(data_sid, r as u64, 0, v);
    }

    // Col B (1): varied numbers
    let col_b = [
        "10", "25.5", "-4", "100", "3", "7", "0.25", "12", "5", "8", "15", "20", "2", "6", "9",
        "14",
    ];
    for (r, v) in col_b.iter().enumerate() {
        put(data_sid, r as u64, 1, v);
    }

    // Col C (2): strictly positive numbers
    let col_c = ["1", "2", "4", "8", "16", "25", "36", "50", "100", "12.5"];
    for (r, v) in col_c.iter().enumerate() {
        put(data_sid, r as u64, 2, v);
    }

    // Col D (3): unit-interval [-1, 1] values
    let col_d = ["-0.9", "-0.5", "-0.25", "0", "0.25", "0.5", "0.75", "0.95"];
    for (r, v) in col_d.iter().enumerate() {
        put(data_sid, r as u64, 3, v);
    }

    // Col E (4): rich text strings (E1..E10; E11..E16 left empty for COUNTBLANK/ISBLANK)
    let col_e = [
        "'Hello",
        "'  Reticula   Grid  ",
        "'abc",
        "'XYZ",
        "'Spreadsheet",
        "'alpha-1",
        "'alpha-2",
        "'beta-1",
        "'中文测试",
        "'123.45",
    ];
    for (r, v) in col_e.iter().enumerate() {
        put(data_sid, r as u64, 4, v);
    }

    // Col F (5): booleans & mixed
    let col_f = [
        "TRUE", "FALSE", "TRUE", "FALSE", "TRUE", "FALSE", "10", "'text",
    ];
    for (r, v) in col_f.iter().enumerate() {
        put(data_sid, r as u64, 5, v);
    }

    // Cols G..H (6..7): sorted lookup table
    let col_g = ["10", "20", "30", "40", "50", "60", "70", "80"];
    let col_h = [
        "'ten", "'twenty", "'thirty", "'forty", "'fifty", "'sixty", "'seventy", "'eighty",
    ];
    for r in 0..8 {
        put(data_sid, r as u64, 6, col_g[r]);
        put(data_sid, r as u64, 7, col_h[r]);
    }

    // Cols I..J (8..9): multi-criteria categories + amounts
    let col_i = [
        "'North", "'South", "'North", "'East", "'South", "'West", "'North", "'East",
    ];
    let col_j = ["15", "25", "40", "10", "35", "50", "60", "30"];
    for r in 0..8 {
        put(data_sid, r as u64, 8, col_i[r]);
        put(data_sid, r as u64, 9, col_j[r]);
    }

    // Col K (10): Excel date serials (K1..K8 post-1900-03-01, K9=60 1900-02-29, K10=45)
    let col_k = [
        "45322", "45351", "45658", "46023", "46204", "46345", "44927", "45000", "60", "45",
    ];
    for (r, v) in col_k.iter().enumerate() {
        put(data_sid, r as u64, 10, v);
    }

    // Col L (11): time fractions
    let col_l = [
        "0",
        "0.25",
        "0.5",
        "0.75",
        "0.521527777777778",
        "0.999988425925926",
    ];
    for (r, v) in col_l.iter().enumerate() {
        put(data_sid, r as u64, 11, v);
    }

    // Col M (12): cash flows for IRR / NPV
    let col_m = ["-1000", "300", "350", "400", "450", "-500", "200", "400"];
    for (r, v) in col_m.iter().enumerate() {
        put(data_sid, r as u64, 12, v);
    }

    // Cols N..Q (13..16): 4x4 numeric matrix (1..16, sum = 136)
    for r in 0..4_u64 {
        for c in 0..4_u64 {
            let val = r * 4 + c + 1;
            put(data_sid, r, 13 + c, &val.to_string());
        }
    }

    // Col R (17): sorted bins for FREQUENCY
    let col_r = ["5", "15", "30", "60", "100", "200"];
    for (r, v) in col_r.iter().enumerate() {
        put(data_sid, r as u64, 17, v);
    }

    // Col S (18): scores with ties/duplicates
    let col_s = ["10", "20", "20", "30", "30", "30", "40", "50"];
    for (r, v) in col_s.iter().enumerate() {
        put(data_sid, r as u64, 18, v);
    }

    // Col T (19): holidays
    let col_t = ["46023", "46041", "46100", "46204", "46300", "46381"];
    for (r, v) in col_t.iter().enumerate() {
        put(data_sid, r as u64, 19, v);
    }

    // Col U (20): delimited strings
    let col_u = [
        "'red,green,blue",
        "'alpha-beta-gamma-delta",
        "'one::two::three",
        "'2026/11/22",
        "'A;B;C;D",
        "'single",
    ];
    for (r, v) in col_u.iter().enumerate() {
        put(data_sid, r as u64, 20, v);
    }

    // Col V (21): date/time text
    let col_v = [
        "'2024-02-29",
        "'2026-01-15",
        "'2025-12-31",
        "'14:30:00",
        "'08:15:30",
        "'23:45:00",
    ];
    for (r, v) in col_v.iter().enumerate() {
        put(data_sid, r as u64, 21, v);
    }

    // Col W (22): formula cells + spill anchor at W5 (W5:W7)
    put(data_sid, 0, 22, "=A1+B1");
    put(data_sid, 1, 22, "=SUM(A1:A4)");
    put(data_sid, 2, 22, "=1/0");
    put(data_sid, 3, 22, "=NA()");
    put(data_sid, 4, 22, "=SEQUENCE(3,1,10,5)");

    // Aux sheet: horizontal lookup row + cross-sheet matrix
    let aux_headers = ["'K1", "'K2", "'K3", "'K4"];
    let aux_vals = ["110", "220", "330", "440"];
    for c in 0..4_u64 {
        put(aux_sid, 0, c, aux_headers[c as usize]);
        put(aux_sid, 1, c, aux_vals[c as usize]);
        put(aux_sid, 2, c, &((c + 1) * 5).to_string());
    }

    for (name, expr) in [
        ("TaxRate", "=0.08"),
        ("Scores", "=Data!$S$1:$S$8"),
        ("Grid4x4", "=Data!$N$1:$Q$4"),
    ] {
        let bound =
            BoundFormula::parse_and_bind(expr, FormulaMode::Dynamic, eng.workbook(), data_sid)
                .unwrap_or_else(|e| panic!("bind named range {name}: {e:?}"));
        eng.apply_op(Op::DefineNamedRange {
            scope: None,
            name: String::from(name),
            formula: bound,
            comment: None,
            hidden: false,
        })
        .expect("define sandbox named range");
    }

    eng.recalc_all();
    eng
}

/// Runs the `>= 50,000` stratified differential suite inside Docker LibreOffice Calc
/// and returns the complete [`LibreOfficeDiffReport`].
pub fn run_libreoffice_differential(
    config: &LibreOfficeDiffConfig,
) -> Result<LibreOfficeDiffReport, String> {
    if !config.skip_image_build {
        ensure_libreoffice_image(&config.image)?;
    }
    let libreoffice_version = query_libreoffice_version(&config.image)?;

    let corpus = generate_stratified_corpus(config.seed, config.quota_per_fn);
    let mut scalar_cases: Vec<GeneratedCase> = Vec::new();
    let mut array_cases: Vec<GeneratedCase> = Vec::new();
    for c in corpus {
        if c.is_dynamic_array {
            array_cases.push(c);
        } else {
            scalar_cases.push(c);
        }
    }

    let tmp_dir = make_temp_workdir()?;
    let in_dir = tmp_dir.join("in");
    let out_dir = tmp_dir.join("out");
    fs::create_dir_all(&in_dir)
        .and_then(|()| fs::create_dir_all(&out_dir))
        .map_err(|e| format!("create temp directories in {}: {e}", tmp_dir.display()))?;

    let mut placed_batches: Vec<(String, Vec<PlacedCase>)> = Vec::new();
    let scalar_per_book = SCALAR_COLS_PER_SHEET * SCALAR_ROWS_PER_SHEET * SCALAR_SHEETS_PER_BOOK;
    for (b_idx, chunk) in scalar_cases.chunks(scalar_per_book).enumerate() {
        let batch_name = format!("batch_{:02}.xlsx", b_idx + 1);
        let (placed, xlsx_bytes) = build_and_eval_scalar_batch(&batch_name, chunk)?;
        fs::write(in_dir.join(&batch_name), &xlsx_bytes)
            .map_err(|e| format!("write {batch_name}: {e}"))?;
        placed_batches.push((batch_name, placed));
    }

    if !array_cases.is_empty() {
        let batch_name = String::from("batch_arrays.xlsx");
        let (placed, xlsx_bytes) = build_and_eval_array_batch(&batch_name, &array_cases)?;
        fs::write(in_dir.join(&batch_name), &xlsx_bytes)
            .map_err(|e| format!("write {batch_name}: {e}"))?;
        placed_batches.push((batch_name, placed));
    }

    if let Some(dump_dir) = &config.dump_xlsx_dir {
        let dump_in = dump_dir.join("in");
        fs::create_dir_all(&dump_in)
            .map_err(|e| format!("create dump dir {}: {e}", dump_in.display()))?;
        for (batch_name, _) in &placed_batches {
            fs::copy(in_dir.join(batch_name), dump_in.join(batch_name))
                .map_err(|e| format!("copy {batch_name} to dump dir: {e}"))?;
        }
    }

    invoke_libreoffice_batch(&config.image, &tmp_dir)?;

    if let Some(dump_dir) = &config.dump_xlsx_dir {
        let dump_out = dump_dir.join("out");
        fs::create_dir_all(&dump_out)
            .map_err(|e| format!("create dump out dir {}: {e}", dump_out.display()))?;
        for (batch_name, _) in &placed_batches {
            fs::copy(out_dir.join(batch_name), dump_out.join(batch_name))
                .map_err(|e| format!("copy recalculated {batch_name} to dump dir: {e}"))?;
        }
    }

    let report = compare_all_batches(config, &libreoffice_version, &placed_batches, &out_dir)?;

    let _ = fs::remove_dir_all(&tmp_dir);
    Ok(report)
}

fn build_and_eval_scalar_batch(
    batch_name: &str,
    cases: &[GeneratedCase],
) -> Result<(Vec<PlacedCase>, Vec<u8>), String> {
    let mut eng = create_sandbox_engine();
    let per_sheet = SCALAR_COLS_PER_SHEET * SCALAR_ROWS_PER_SHEET;
    let mut placements: Vec<(SheetId, String, u32, u32, GeneratedCase)> =
        Vec::with_capacity(cases.len());

    for (s_idx, sheet_chunk) in cases.chunks(per_sheet).enumerate() {
        let sheet_name = format!("T{}", s_idx + 1);
        let sid = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: sid,
            name: sheet_name.clone(),
            after: None,
        })
        .map_err(|e| format!("add sheet {sheet_name}: {e:?}"))?;

        eng.edit_cell_raw(sid, RowId(0), ColId(25), SENTINEL_Z1_FORMULA)
            .map_err(|e| format!("write Z1 sentinel on {sheet_name}: {e:?}"))?;
        eng.edit_cell_raw(sid, RowId(1), ColId(25), SENTINEL_Z2_FORMULA)
            .map_err(|e| format!("write Z2 sentinel on {sheet_name}: {e:?}"))?;

        for (i, case) in sheet_chunk.iter().enumerate() {
            let r = (i / SCALAR_COLS_PER_SHEET) as u32;
            let c = (i % SCALAR_COLS_PER_SHEET) as u32;
            eng.edit_cell_raw(sid, RowId(u64::from(r)), ColId(u64::from(c)), &case.formula)
                .map_err(|e| {
                    format!(
                        "Reticula rejected generated formula `{}` ({}): {e:?}",
                        case.formula, case.function
                    )
                })?;
            placements.push((sid, sheet_name.clone(), r, c, case.clone()));
        }
    }

    eng.recalc_all();
    verify_sentinels_reticula(&eng, &placements)?;

    let mut placed = Vec::with_capacity(placements.len());
    for (sid, sheet_name, r, c, case) in placements {
        let top = read_reticula_cell(&eng, sid, r, c);
        placed.push(PlacedCase {
            sheet_name,
            row: r,
            col: c,
            case,
            reticula_grid: vec![vec![top.clone()]],
            reticula_top: top,
        });
    }

    let exported = export_xlsx_with_options(
        &eng,
        ExportOptions {
            omit_cached_values: true,
            full_calc_on_load: true,
        },
    )
    .map_err(|e| format!("export {batch_name}: {e}"))?;
    Ok((placed, exported.bytes))
}

fn build_and_eval_array_batch(
    batch_name: &str,
    cases: &[GeneratedCase],
) -> Result<(Vec<PlacedCase>, Vec<u8>), String> {
    let mut eng = create_sandbox_engine();
    let mut placements: Vec<(SheetId, String, u32, u32, GeneratedCase)> =
        Vec::with_capacity(cases.len());

    for (s_idx, sheet_chunk) in cases.chunks(ARRAY_SLOTS_PER_SHEET).enumerate() {
        let sheet_name = format!("A{}", s_idx + 1);
        let sid = eng.alloc_sheet_id();
        eng.apply_op(Op::AddSheet {
            sheet: sid,
            name: sheet_name.clone(),
            after: None,
        })
        .map_err(|e| format!("add sheet {sheet_name}: {e:?}"))?;

        eng.edit_cell_raw(sid, RowId(0), ColId(25), SENTINEL_Z1_FORMULA)
            .map_err(|e| format!("write Z1 sentinel on {sheet_name}: {e:?}"))?;
        eng.edit_cell_raw(sid, RowId(1), ColId(25), SENTINEL_Z2_FORMULA)
            .map_err(|e| format!("write Z2 sentinel on {sheet_name}: {e:?}"))?;

        for (i, case) in sheet_chunk.iter().enumerate() {
            let slot_row = (i / ARRAY_SLOT_COLS.len()) as u32 * ARRAY_SLOT_STRIDE;
            let slot_col = ARRAY_SLOT_COLS[i % ARRAY_SLOT_COLS.len()];
            eng.edit_cell_raw(
                sid,
                RowId(u64::from(slot_row)),
                ColId(u64::from(slot_col)),
                &case.formula,
            )
            .map_err(|e| {
                format!(
                    "Reticula rejected array formula `{}` ({}): {e:?}",
                    case.formula, case.function
                )
            })?;
            placements.push((sid, sheet_name.clone(), slot_row, slot_col, case.clone()));
        }
    }

    eng.recalc_all();
    verify_sentinels_reticula(&eng, &placements)?;

    let mut placed = Vec::with_capacity(placements.len());
    for (sid, sheet_name, r, c, case) in placements {
        let top = read_reticula_cell(&eng, sid, r, c);
        let (sp_rows, sp_cols) = eng
            .spill_range((sid, RowId(u64::from(r)), ColId(u64::from(c))))
            .unwrap_or((1, 1));
        if sp_rows > ARRAY_MAX_BOX || sp_cols > ARRAY_MAX_BOX {
            return Err(format!(
                "generated array `{}` spilled {sp_rows}x{sp_cols}, exceeding {ARRAY_MAX_BOX}x{ARRAY_MAX_BOX} slot bound",
                case.formula
            ));
        }
        let mut grid = Vec::with_capacity(sp_rows as usize);
        for dr in 0..sp_rows {
            let mut row_vals = Vec::with_capacity(sp_cols as usize);
            for dc in 0..sp_cols {
                row_vals.push(read_reticula_cell(&eng, sid, r + dr, c + dc));
            }
            grid.push(row_vals);
        }
        placed.push(PlacedCase {
            sheet_name,
            row: r,
            col: c,
            case,
            reticula_grid: grid,
            reticula_top: top,
        });
    }

    let exported = export_xlsx_with_options(
        &eng,
        ExportOptions {
            omit_cached_values: true,
            full_calc_on_load: true,
        },
    )
    .map_err(|e| format!("export {batch_name}: {e}"))?;
    Ok((placed, exported.bytes))
}

fn verify_sentinels_reticula(
    eng: &FormulaEngine,
    placements: &[(SheetId, String, u32, u32, GeneratedCase)],
) -> Result<(), String> {
    let mut checked = BTreeSet::new();
    for (sid, name, _, _, _) in placements {
        if checked.insert(*sid) {
            let z1 = read_reticula_cell(eng, *sid, 0, 25);
            let z2 = read_reticula_cell(eng, *sid, 1, 25);
            if z1 != NormValue::Number(SENTINEL_Z1_EXPECTED) {
                return Err(format!(
                    "Reticula sentinel Z1 mismatch on {name}: expected {SENTINEL_Z1_EXPECTED}, got {z1:?}"
                ));
            }
            if z2 != NormValue::Number(SENTINEL_Z2_EXPECTED) {
                return Err(format!(
                    "Reticula sentinel Z2 mismatch on {name}: expected {SENTINEL_Z2_EXPECTED}, got {z2:?}"
                ));
            }
        }
    }
    Ok(())
}

fn read_reticula_cell(eng: &FormulaEngine, sid: SheetId, r: u32, c: u32) -> NormValue {
    let row_id = RowId(u64::from(r));
    let col_id = ColId(u64::from(c));
    if let Some(spilled) = eng.spills().shadow_value(sid, row_id, col_id) {
        return match spilled {
            EvalScalar::Empty => NormValue::Number(0.0),
            EvalScalar::Number(n) => NormValue::Number(round_to_15_sig_figs(*n)),
            EvalScalar::Text(s) => NormValue::Text(s.clone()),
            EvalScalar::Bool(b) => NormValue::Bool(*b),
            EvalScalar::Error(e) => NormValue::Error(e.as_str().to_owned()),
        };
    }
    let Some(sheet) = eng.workbook().sheet(sid) else {
        return NormValue::Error(String::from("#REF!"));
    };
    match sheet.value_by_id(row_id, col_id) {
        Value::Empty => NormValue::Number(0.0),
        Value::Number(n) => NormValue::Number(round_to_15_sig_figs(n)),
        Value::Text(id) => NormValue::Text(sheet.strings().get(id).to_owned()),
        Value::Bool(b) => NormValue::Bool(b),
        Value::Error(e) => NormValue::Error(e.as_str().to_owned()),
        Value::Array(_) => NormValue::Error(String::from("#SPILL!")),
    }
}

fn scalar_to_norm(s: Option<&ScalarValue>) -> NormValue {
    match s {
        None => NormValue::Number(0.0),
        Some(ScalarValue::Number(n)) => NormValue::Number(round_to_15_sig_figs(*n)),
        Some(ScalarValue::Text(t)) => NormValue::Text(t.clone()),
        Some(ScalarValue::Bool(b)) => NormValue::Bool(*b),
        Some(ScalarValue::Error(e)) => NormValue::Error(e.as_str().to_owned()),
    }
}

fn float_rel_tolerance(fn_name: &str) -> Option<f64> {
    if matches!(fn_name, "RATE" | "IRR") {
        // LibreOffice Calc stops iterative financial root-finders around ~1e-9 residual.
        return Some(1e-10);
    }
    if matches!(
        fn_name,
        "SIN"
            | "COS"
            | "TAN"
            | "ASIN"
            | "ACOS"
            | "ATAN"
            | "ATAN2"
            | "SINH"
            | "COSH"
            | "TANH"
            | "EXP"
            | "LN"
            | "LOG"
            | "LOG10"
            | "SQRT"
            | "POWER"
            | "RADIANS"
            | "DEGREES"
            | "STDEV"
            | "STDEV.S"
            | "STDEV.P"
            | "VAR"
            | "VAR.S"
            | "VAR.P"
            | "CORREL"
            | "FV"
            | "PV"
            | "PMT"
            | "NPER"
            | "NPV"
            | "SLN"
            | "TIME"
            | "YEARFRAC"
    ) {
        return Some(1e-12);
    }
    None
}

fn values_agree_for_fn(fn_name: &str, a: &NormValue, b: &NormValue) -> bool {
    match (a, b) {
        (NormValue::Number(x), NormValue::Number(y)) => {
            if round_to_15_sig_figs(*x) == round_to_15_sig_figs(*y) {
                return true;
            }
            if let Some(tol) = float_rel_tolerance(fn_name) {
                let scale = x.abs().max(y.abs()).max(1e-15);
                return (x - y).abs() <= tol * scale;
            }
            false
        }
        (NormValue::Text(x), NormValue::Text(y)) => x == y,
        (NormValue::Bool(x), NormValue::Bool(y)) => x == y,
        (NormValue::Error(x), NormValue::Error(y)) => x == y,
        _ => false,
    }
}

fn compare_all_batches(
    config: &LibreOfficeDiffConfig,
    libreoffice_version: &str,
    placed_batches: &[(String, Vec<PlacedCase>)],
    out_dir: &Path,
) -> Result<LibreOfficeDiffReport, String> {
    let mut total_formulas = 0usize;
    let mut total_cells_compared = 0usize;
    let mut non_error_count = 0usize;
    let mut matches = 0usize;

    #[derive(Default)]
    struct Tallies {
        total: usize,
        non_error: usize,
        matches: usize,
        unclassified: usize,
    }

    let mut fn_tallies: BTreeMap<String, (&'static str, Tallies)> = BTreeMap::new();
    let mut fam_tallies: BTreeMap<&'static str, Tallies> = BTreeMap::new();
    let mut cluster_map: BTreeMap<(String, String, String), LibreOfficeDivergenceCluster> =
        BTreeMap::new();

    for (batch_name, cases) in placed_batches {
        let out_file = out_dir.join(batch_name);
        let bytes = fs::read(&out_file).map_err(|e| {
            format!(
                "missing recalculated LibreOffice file {}: {e}",
                out_file.display()
            )
        })?;
        let sheets_map = read_xlsx_cached_values(&bytes)
            .map_err(|e| format!("read LibreOffice output {batch_name}: {e}"))?;

        let mut verified_sheets = BTreeSet::new();
        for p in cases {
            let lo_cells = sheets_map.get(&p.sheet_name).ok_or_else(|| {
                format!(
                    "sheet `{}` missing from LibreOffice output {batch_name}",
                    p.sheet_name
                )
            })?;
            if verified_sheets.insert(p.sheet_name.clone()) {
                let z1 = scalar_to_norm(lo_cells.get(&(0, 25)));
                let z2 = scalar_to_norm(lo_cells.get(&(1, 25)));
                if z1 != NormValue::Number(SENTINEL_Z1_EXPECTED) {
                    return Err(format!(
                        "LibreOffice did not recalculate sentinel Z1 in {batch_name}!{}: got {z1:?} (expected {SENTINEL_Z1_EXPECTED})",
                        p.sheet_name
                    ));
                }
                if z2 != NormValue::Number(SENTINEL_Z2_EXPECTED) {
                    return Err(format!(
                        "LibreOffice did not recalculate sentinel Z2 in {batch_name}!{}: got {z2:?} (expected {SENTINEL_Z2_EXPECTED})",
                        p.sheet_name
                    ));
                }
            }

            total_formulas += 1;
            let is_non_err = !matches!(p.reticula_top, NormValue::Error(_));
            if is_non_err {
                non_error_count += 1;
            }

            let mut case_matched = true;
            let mut first_mismatch: Option<(NormValue, NormValue)> = None;

            if p.case.is_dynamic_array {
                let r_rows = p.reticula_grid.len() as u32;
                let r_cols = p.reticula_grid.first().map_or(1, Vec::len) as u32;
                let mut max_lo_r = 1_u32;
                let mut max_lo_c = 1_u32;
                for dr in 0..ARRAY_MAX_BOX {
                    for dc in 0..ARRAY_MAX_BOX {
                        if lo_cells.contains_key(&(p.row + dr, p.col + dc)) {
                            max_lo_r = max_lo_r.max(dr + 1);
                            max_lo_c = max_lo_c.max(dc + 1);
                        }
                    }
                }
                let cmp_rows = r_rows.max(max_lo_r);
                let cmp_cols = r_cols.max(max_lo_c);
                for dr in 0..cmp_rows {
                    for dc in 0..cmp_cols {
                        total_cells_compared += 1;
                        let r_val = p
                            .reticula_grid
                            .get(dr as usize)
                            .and_then(|row| row.get(dc as usize))
                            .cloned()
                            .unwrap_or(NormValue::Number(0.0));
                        let lo_val = scalar_to_norm(lo_cells.get(&(p.row + dr, p.col + dc)));
                        if !values_agree_for_fn(&p.case.function, &r_val, &lo_val)
                            && first_mismatch.is_none()
                        {
                            case_matched = false;
                            first_mismatch = Some((r_val, lo_val));
                        }
                    }
                }
            } else {
                total_cells_compared += 1;
                let lo_val = scalar_to_norm(lo_cells.get(&(p.row, p.col)));
                if !values_agree_for_fn(&p.case.function, &p.reticula_top, &lo_val) {
                    case_matched = false;
                    first_mismatch = Some((p.reticula_top.clone(), lo_val));
                }
            }

            let fn_entry = fn_tallies
                .entry(p.case.function.clone())
                .or_insert_with(|| (p.case.family, Tallies::default()));
            fn_entry.1.total += 1;
            if is_non_err {
                fn_entry.1.non_error += 1;
            }

            let fam_entry = fam_tallies.entry(p.case.family).or_default();
            fam_entry.total += 1;
            if is_non_err {
                fam_entry.non_error += 1;
            }

            if case_matched {
                matches += 1;
                fn_entry.1.matches += 1;
                fam_entry.matches += 1;
            } else if let Some((r_val, lo_val)) = first_mismatch {
                let (disp, reason) = classify_libreoffice_divergence(p, &r_val, &lo_val);
                if disp == "unclassified" {
                    fn_entry.1.unclassified += 1;
                    fam_entry.unclassified += 1;
                }
                let c_key = (p.case.function.clone(), disp.to_owned(), reason.to_owned());
                let cluster =
                    cluster_map
                        .entry(c_key)
                        .or_insert_with(|| LibreOfficeDivergenceCluster {
                            reference_engine: String::from("libreoffice-calc"),
                            category: p.case.function.clone(),
                            family: p.case.family.to_owned(),
                            disposition: disp.to_owned(),
                            reason: reason.to_owned(),
                            count: 0,
                            sample_formula: p.case.formula.clone(),
                            reticula_result: r_val,
                            libreoffice_result: lo_val,
                        });
                cluster.count += 1;
            }
        }
    }

    let det_fns = deterministic_catalog_functions();
    let mut min_samples = usize::MAX;
    for name in &det_fns {
        let count = fn_tallies.get(*name).map_or(0, |(_, t)| t.total);
        min_samples = min_samples.min(count);
    }
    if min_samples == usize::MAX {
        min_samples = 0;
    }

    let by_family: Vec<FamilySummary> = fam_tallies
        .into_iter()
        .map(|(family, t)| FamilySummary {
            family: family.to_owned(),
            total: t.total,
            non_error: t.non_error,
            matches: t.matches,
            agreement_ratio: if t.total == 0 {
                1.0
            } else {
                t.matches as f64 / t.total as f64
            },
            unclassified: t.unclassified,
        })
        .collect();

    let by_function: Vec<FunctionSummary> = fn_tallies
        .into_iter()
        .map(|(function, (family, t))| FunctionSummary {
            function,
            family: family.to_owned(),
            total: t.total,
            non_error: t.non_error,
            matches: t.matches,
            agreement_ratio: if t.total == 0 {
                1.0
            } else {
                t.matches as f64 / t.total as f64
            },
            unclassified: t.unclassified,
        })
        .collect();

    let clusters: Vec<LibreOfficeDivergenceCluster> = cluster_map.into_values().collect();
    let unclassified_count: usize = clusters
        .iter()
        .filter(|c| c.disposition == "unclassified")
        .map(|c| c.count)
        .sum();

    let non_error_ratio = if total_formulas == 0 {
        0.0
    } else {
        non_error_count as f64 / total_formulas as f64
    };
    let agreement_ratio = if total_formulas == 0 {
        0.0
    } else {
        matches as f64 / total_formulas as f64
    };

    let unique_aliases_present = UNIQUE_ALIASES
        .iter()
        .filter(|name| fn_tallies_has(name, &by_function))
        .count();

    Ok(LibreOfficeDiffReport {
        suite: String::from("m3a-diff"),
        date: config.date.clone(),
        commit: config.commit.clone(),
        seed: config.seed,
        quota_per_fn: config.quota_per_fn,
        libreoffice_version: libreoffice_version.to_owned(),
        container_image: config.image.clone(),
        total_formulas,
        total_cells_compared,
        deterministic_functions_covered: det_fns.len(),
        unique_functions_covered: det_fns.len().saturating_sub(unique_aliases_present),
        min_samples_per_function: min_samples,
        non_error_count,
        non_error_ratio,
        matches,
        agreement_ratio,
        unclassified_count,
        by_family,
        by_function,
        clusters,
        rust_reference: config.rust_reference.clone(),
    })
}

fn fn_tallies_has(name: &str, by_fn: &[FunctionSummary]) -> bool {
    by_fn.iter().any(|f| f.function == name && f.total > 0)
}

fn classify_libreoffice_divergence(
    placed: &PlacedCase,
    ret: &NormValue,
    lo: &NormValue,
) -> (&'static str, &'static str) {
    let fn_name = placed.case.function.as_str();
    let formula = placed.case.formula.as_str();

    // 1. Excel 1900 leap-year bug (serial 60 = 1900-02-29, pre-March-1900 +1 day shift, and serial <= 0 -> #NUM!).
    if matches!(
        fn_name,
        "DATE"
            | "DAY"
            | "MONTH"
            | "YEAR"
            | "EDATE"
            | "EOMONTH"
            | "DAYS"
            | "DAYS360"
            | "WEEKDAY"
            | "WEEKNUM"
            | "ISOWEEKNUM"
            | "DATEDIF"
            | "NETWORKDAYS"
            | "WORKDAY"
            | "YEARFRAC"
            | "COMPOUND"
    ) && (formula.contains("K9")
        || formula.contains("K10")
        || formula.contains("1900,2,29")
        || formula.contains("1900-02-29"))
    {
        return (
            "excel_1900_calendar_rule",
            "Excel 1900 leap-year compatibility (1900-02-29 = serial 60, Jan-Feb 1900 +1 offset) vs proleptic Gregorian in LibreOffice",
        );
    }

    // 2. D-027 Excel 15-significant-digit rounding & cancellation rules.
    if matches!(
        fn_name,
        "ROUND"
            | "ROUNDDOWN"
            | "ROUNDUP"
            | "TRUNC"
            | "INT"
            | "FLOOR"
            | "CEILING"
            | "MROUND"
            | "COMPOUND"
    ) {
        if let (NormValue::Number(a), NormValue::Number(b)) = (ret, lo)
            && (*a - *b).abs() <= 1e-1 * a.abs().max(1.0)
        {
            return (
                "excel_15_digit_rule",
                "Excel 15-significant-digit rounding/comparison rule (D-027) vs raw IEEE-754 intermediate in LibreOffice",
            );
        }
        if matches!((ret, lo), (NormValue::Bool(_), NormValue::Bool(_))) {
            return (
                "excel_15_digit_rule",
                "Excel 15-significant-digit equality snapping on near-equal decimal expressions (D-027)",
            );
        }
    }

    // 3. `POWER(0, negative)`: Excel/Reticula `#NUM!` vs `#DIV/0!` variant.
    if fn_name == "POWER"
        && matches!(
            (ret, lo),
            (NormValue::Error(a), NormValue::Error(b)) if a == "#NUM!" && b == "#DIV/0!"
        )
    {
        return (
            "known_error_code_variant",
            "POWER(0, negative) returns #NUM! in Reticula/spec vs #DIV/0! in Calc",
        );
    }

    // 4. `YEAR` / `MONTH` / `DAY` on out-of-range serial <= 0 -> #NUM! in Reticula vs 1899 proleptic date in Calc.
    if matches!(fn_name, "DATE" | "YEAR" | "MONTH" | "DAY")
        && matches!((ret, lo), (NormValue::Error(e), NormValue::Number(_)) if e == "#NUM!")
    {
        return (
            "excel_1900_calendar_rule",
            "Excel date domain floor (serial >= 1) returns #NUM! vs negative/1899 dates in Calc",
        );
    }

    // 5. `MODE` / `MODE.SNGL` when no duplicates exist: `#N/A` in Excel/Reticula vs `#VALUE!` in Calc.
    if matches!(fn_name, "MODE" | "MODE.SNGL")
        && matches!(
            (ret, lo),
            (NormValue::Error(a), NormValue::Error(b)) if a == "#N/A" && b == "#VALUE!"
        )
    {
        return (
            "libreoffice_mode_no_dup_error",
            "MODE with no duplicate values returns #N/A in Excel/Reticula vs #VALUE! in LibreOffice Calc",
        );
    }

    // 6. 1-arg `NUMBERVALUE`: optional decimal separator in Excel OOXML vs mandatory 2nd arg in Calc.
    if fn_name == "NUMBERVALUE"
        && matches!((ret, lo), (NormValue::Number(_), NormValue::Error(b)) if b == "#VALUE!")
    {
        return (
            "libreoffice_odf_numbervalue_optional_arg",
            "1-arg NUMBERVALUE defaults decimal separator in Excel OOXML but requires 2nd arg in LibreOffice Calc",
        );
    }

    // 7. `TYPE(range)`: returns `64` (Array) in Excel 365/Reticula vs `#VALUE!` in Calc non-array calls.
    if fn_name == "TYPE"
        && matches!((ret, lo), (NormValue::Number(a), NormValue::Error(b)) if *a == 64.0 && b == "#VALUE!")
    {
        return (
            "libreoffice_type_array_arg",
            "TYPE(range) returns 64 (Array) in Excel 365/Reticula vs #VALUE! in LibreOffice non-array call",
        );
    }

    // 8. `UNIQUE` boolean array serialization: Calc writes `<c t="n"><v>1|0</v>` in CSE array XML.
    if fn_name == "UNIQUE"
        && matches!(
            (ret, lo),
            (NormValue::Bool(b), NormValue::Number(n)) if (*b && *n == 1.0) || (!*b && *n == 0.0)
        )
    {
        return (
            "libreoffice_array_bool_number_serialization",
            "LibreOffice serializes boolean array spill elements as numeric 1/0 without t=\"b\"",
        );
    }

    // 9. Calc stores constant boolean cells as `TRUE()`/`FALSE()` formula cells (`ISFORMULA=TRUE`, `TYPE=8`).
    if fn_name == "ISFORMULA"
        && matches!((ret, lo), (NormValue::Bool(false), NormValue::Bool(true)))
        && formula.contains("Data!F")
    {
        return (
            "libreoffice_boolean_cell_as_formula",
            "LibreOffice serializes boolean constants as TRUE()/FALSE() formulas, making ISFORMULA(TRUE) return TRUE",
        );
    }
    if fn_name == "TYPE"
        && matches!((ret, lo), (NormValue::Number(a), NormValue::Number(b)) if *a == 4.0 && *b == 8.0)
        && formula.contains("Data!F")
    {
        return (
            "libreoffice_boolean_cell_as_formula",
            "LibreOffice evaluates TYPE() on stored boolean TRUE()/FALSE() cells as 8 (Formula) vs 4 (Logical) in Excel",
        );
    }
    if fn_name == "TYPE"
        && matches!((ret, lo), (NormValue::Number(a), NormValue::Number(b)) if *a == 1.0 && *b == 8.0)
        && formula.contains("Data!W1")
    {
        return (
            "libreoffice_type_formula_return_8",
            "LibreOffice TYPE(formula_cell) returns 8 (Formula) per ODF whereas Excel returns the evaluated value type (1)",
        );
    }

    // 10. Calc treats booleans as numeric 1/0 for ISNUMBER.
    if fn_name == "ISNUMBER"
        && matches!((ret, lo), (NormValue::Bool(false), NormValue::Bool(true)))
        && formula.contains("Data!F")
    {
        return (
            "libreoffice_boolean_is_number",
            "LibreOffice evaluates ISNUMBER(TRUE/FALSE) as TRUE vs FALSE in Excel/Reticula",
        );
    }

    // 11. Calc exports Err:502 domain errors on LN(-1)/SQRT(-4) as `#VALUE!` instead of `#NUM!`.
    if matches!(fn_name, "LN" | "SQRT")
        && matches!(
            (ret, lo),
            (NormValue::Error(a), NormValue::Error(b)) if a == "#NUM!" && b == "#VALUE!"
        )
    {
        return (
            "libreoffice_err502_domain_value_error",
            "LibreOffice exports Err:502 negative real domain errors on LN/SQRT as #VALUE! vs #NUM! in Excel/Reticula",
        );
    }

    // 12. `DATEDIF(..., "YD")` spanning leap year 2024 (`Data!K1`/`Data!K2`) to non-leap 2026: 1-day anchor ambiguity.
    if fn_name == "DATEDIF"
        && formula.contains("YD")
        && (formula.contains("Data!K1") || formula.contains("Data!K2"))
        && let (NormValue::Number(a), NormValue::Number(b)) = (ret, lo)
        && (*a - *b).abs() == 1.0
    {
        return (
            "datedif_yd_leap_year_anchor",
            "DATEDIF \"YD\" spanning leap year 2024 (start-year vs end-year anchor) differs by 1 day across spreadsheet engines",
        );
    }

    (
        "unclassified",
        "Unclassified divergence requiring triage against Excel semantics",
    )
}

fn ensure_libreoffice_image(image: &str) -> Result<(), String> {
    let inspected = Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .map_err(|e| format!("failed to run docker image inspect {image}: {e}"))?;
    if inspected.status.success() {
        return Ok(());
    }
    eprintln!("building LibreOffice container `{image}` from infra/libreoffice/Dockerfile...");
    let status = Command::new("docker")
        .args(["build", "-t", image, "infra/libreoffice"])
        .status()
        .map_err(|e| format!("failed to run docker build infra/libreoffice: {e}"))?;
    if !status.success() {
        return Err(format!(
            "docker build infra/libreoffice exited with {status}"
        ));
    }
    Ok(())
}

fn query_libreoffice_version(image: &str) -> Result<String, String> {
    let output = Command::new("docker")
        .args(["run", "--rm", image, "soffice", "--version"])
        .output()
        .map_err(|e| format!("failed to query soffice --version: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "soffice --version failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn invoke_libreoffice_batch(image: &str, work_dir: &Path) -> Result<(), String> {
    let abs = if work_dir.is_absolute() {
        work_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("current_dir: {e}"))?
            .join(work_dir)
    };
    let host_mount = abs.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
    let volume_arg = format!("{host_mount}:/work");

    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &volume_arg,
            image,
            "sh",
            "-c",
            "soffice --headless --norestore --nodefault --nolockcheck -env:UserInstallation=file:///tmp/lo-profile --convert-to xlsx --outdir /work/out /work/in/*.xlsx",
        ])
        .output()
        .map_err(|e| format!("failed to execute LibreOffice container: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "LibreOffice container failed ({}):\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn make_temp_workdir() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("current_dir: {e}"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = cwd
        .join("target")
        .join(format!("tmp-lo-diff-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    Ok(dir)
}

// ------------------------------------------------------------ formula generator

fn pick<'a>(rng: &mut Rng, items: &'a [&'a str]) -> &'a str {
    let idx = rng.next_below(items.len() as u64) as usize;
    items[idx]
}

fn rand_range(rng: &mut Rng, lo: i32, hi_incl: i32) -> i32 {
    let span = u64::try_from(hi_incl - lo + 1).unwrap_or(1);
    lo + i32::try_from(rng.next_below(span)).unwrap_or(0)
}

const DATA_A_REFS: [&str; 12] = [
    "Data!A1", "Data!A2", "Data!A3", "Data!A4", "Data!A5", "Data!A7", "Data!A8", "Data!A9",
    "Data!A10", "Data!A11", "Data!A15", "Data!A16",
];
const DATA_B_REFS: [&str; 12] = [
    "Data!B1", "Data!B2", "Data!B3", "Data!B4", "Data!B5", "Data!B6", "Data!B7", "Data!B8",
    "Data!B9", "Data!B10", "Data!B11", "Data!B12",
];
const DATA_C_POS: [&str; 10] = [
    "Data!C1", "Data!C2", "Data!C3", "Data!C4", "Data!C5", "Data!C6", "Data!C7", "Data!C8",
    "Data!C9", "Data!C10",
];
const DATA_D_UNIT: [&str; 8] = [
    "Data!D1", "Data!D2", "Data!D3", "Data!D4", "Data!D5", "Data!D6", "Data!D7", "Data!D8",
];
const DATA_TEXT: [&str; 9] = [
    "Data!E1", "Data!E2", "Data!E3", "Data!E4", "Data!E5", "Data!E6", "Data!E7", "Data!E8",
    "Data!E9",
];
const DATA_DATES: [&str; 8] = [
    "Data!K1", "Data!K2", "Data!K3", "Data!K4", "Data!K5", "Data!K6", "Data!K7", "Data!K8",
];
const DATA_TIMES: [&str; 6] = [
    "Data!L1", "Data!L2", "Data!L3", "Data!L4", "Data!L5", "Data!L6",
];

#[expect(clippy::too_many_lines)]
fn gen_formula_for_function(rng: &mut Rng, name: &str, idx: usize) -> String {
    match name {
        // --- Math & Trig ---
        "ABS" => format!("=ABS({})", pick(rng, &DATA_A_REFS)),
        "SIGN" => format!("=SIGN({})", pick(rng, &DATA_A_REFS)),
        "ACOS" => {
            if idx % 25 == 24 {
                String::from("=ACOS(1.5)")
            } else {
                format!("=ACOS({})", pick(rng, &DATA_D_UNIT))
            }
        }
        "ASIN" => {
            if idx % 25 == 24 {
                String::from("=ASIN(-1.5)")
            } else {
                format!("=ASIN({})", pick(rng, &DATA_D_UNIT))
            }
        }
        "ATAN" => format!("=ATAN({})", pick(rng, &DATA_A_REFS)),
        "ATAN2" => format!(
            "=ATAN2({},{})",
            pick(rng, &DATA_B_REFS),
            pick(rng, &DATA_C_POS)
        ),
        "SIN" => format!("=SIN({})", pick(rng, &DATA_D_UNIT)),
        "COS" => format!("=COS({})", pick(rng, &DATA_D_UNIT)),
        "TAN" => format!("=TAN({})", pick(rng, &DATA_D_UNIT)),
        "SINH" => format!("=SINH({})", pick(rng, &DATA_D_UNIT)),
        "COSH" => format!("=COSH({})", pick(rng, &DATA_D_UNIT)),
        "TANH" => format!("=TANH({})", pick(rng, &DATA_D_UNIT)),
        "RADIANS" => format!("=RADIANS({})", rand_range(rng, -180, 360)),
        "DEGREES" => format!("=DEGREES({})", pick(rng, &DATA_D_UNIT)),
        "EXP" => format!("=EXP({:.2})", f64::from(rand_range(rng, -5, 5)) * 0.5),
        "LN" => {
            if idx % 30 == 29 {
                String::from("=LN(-1)")
            } else {
                format!("=LN({})", pick(rng, &DATA_C_POS))
            }
        }
        "LOG10" => format!("=LOG10({})", pick(rng, &DATA_C_POS)),
        "LOG" => {
            let x = pick(rng, &DATA_C_POS);
            if idx.is_multiple_of(2) {
                format!("=LOG({x})")
            } else {
                let base = pick(rng, &["2", "5", "10", "Data!C2"]);
                format!("=LOG({x},{base})")
            }
        }
        "SQRT" => {
            if idx % 30 == 29 {
                String::from("=SQRT(-4)")
            } else {
                format!("=SQRT({})", pick(rng, &DATA_C_POS))
            }
        }
        "PI" => String::from("=PI()"),
        "POWER" => {
            let base = pick(rng, &["2", "3", "4", "0.5", "Data!C3", "Data!C5"]);
            let exp = rand_range(rng, -2, 4);
            format!("=POWER({base},{exp})")
        }
        "MOD" => {
            let a = pick(rng, &DATA_A_REFS);
            let b = pick(rng, &["3", "-3", "7", "-4", "2.5", "Data!C2"]);
            format!("=MOD({a},{b})")
        }
        "QUOTIENT" => {
            let a = rand_range(rng, -50, 50);
            let b = pick(rng, &["3", "-3", "7", "4", "Data!C3"]);
            format!("=QUOTIENT({a},{b})")
        }
        "INT" => format!("=INT({})", pick(rng, &DATA_A_REFS)),
        "TRUNC" => {
            let x = pick(rng, &DATA_A_REFS);
            if idx.is_multiple_of(2) {
                format!("=TRUNC({x})")
            } else {
                format!("=TRUNC({x},{})", rng.next_below(4))
            }
        }
        "ROUND" | "ROUNDDOWN" | "ROUNDUP" => {
            let x = pick(
                rng,
                &[
                    "Data!A4", "Data!A5", "Data!A7", "Data!A8", "Data!B2", "1.235", "-4.565",
                ],
            );
            let d = rand_range(rng, -1, 3);
            format!("={name}({x},{d})")
        }
        "FLOOR" | "CEILING" => {
            let pairs = [
                ("0.3", "0.1"),
                ("1.2", "0.1"),
                ("-2.5", "-1"),
                ("Data!B2", "0.5"),
                ("Data!A7", "2"),
                ("15", "4"),
            ];
            let (x, sig) = pairs[rng.next_below(pairs.len() as u64) as usize];
            format!("={name}({x},{sig})")
        }
        "MROUND" => {
            let pairs = [
                ("10", "3"),
                ("-10", "-3"),
                ("1.3", "0.2"),
                ("Data!B2", "0.5"),
                ("0", "5"),
            ];
            let (x, m) = pairs[rng.next_below(pairs.len() as u64) as usize];
            format!("=MROUND({x},{m})")
        }
        "EVEN" | "ODD" => {
            let x = pick(rng, &["1.5", "2.1", "-3.2", "0", "Data!B2", "Data!A8"]);
            format!("={name}({x})")
        }
        "FACT" => {
            let n = rng.next_below(11);
            format!("=FACT({n})")
        }
        "COMBIN" | "PERMUT" => {
            let n = rand_range(rng, 2, 10);
            let k = rand_range(rng, 0, n);
            format!("={name}({n},{k})")
        }
        "GCD" | "LCM" => {
            let a = rand_range(rng, 2, 36);
            let b = rand_range(rng, 2, 36);
            if idx.is_multiple_of(2) {
                format!("={name}({a},{b})")
            } else {
                let c = rand_range(rng, 2, 24);
                format!("={name}({a},{b},{c})")
            }
        }
        "PRODUCT" | "SUMSQ" => {
            let r = pick(rng, &["Data!C1:C5", "Data!B1:B6", "Data!N1:Q2"]);
            format!("={name}({r})")
        }

        // --- Aggregations ---
        "SUM" | "AVERAGE" | "COUNT" | "COUNTA" | "MIN" | "MAX" => {
            let r = pick(
                rng,
                &[
                    "Data!A1:A16",
                    "Data!B1:B16",
                    "Data!N1:Q4",
                    "Data!S1:S8",
                    "Data!C1:C10",
                ],
            );
            if idx.is_multiple_of(3) {
                format!("={name}({r},{})", pick(rng, &DATA_B_REFS))
            } else {
                format!("={name}({r})")
            }
        }

        // --- Criteria & Conditional Aggregations ---
        "COUNTBLANK" => {
            let r = pick(rng, &["Data!E1:E16", "Data!E8:E16", "Data!A1:B8"]);
            format!("=COUNTBLANK({r})")
        }
        "COUNTIF" => {
            if idx.is_multiple_of(2) {
                let crit = pick(rng, &["\">=30\"", "\"<50\"", "\"<>20\"", "30"]);
                format!("=COUNTIF(Data!G1:G8,{crit})")
            } else {
                let crit = pick(rng, &["\"North\"", "\"alpha*\"", "\"*1\"", "\"?est\""]);
                let col = if crit == "\"North\"" || crit == "\"?est\"" {
                    "Data!I1:I8"
                } else {
                    "Data!E1:E9"
                };
                format!("=COUNTIF({col},{crit})")
            }
        }
        "SUMIF" | "AVERAGEIF" => {
            if idx.is_multiple_of(2) {
                let crit = pick(rng, &["\"North\"", "\"South\"", "\"East\"", "\"N*\""]);
                format!("={name}(Data!I1:I8,{crit},Data!J1:J8)")
            } else {
                let crit = pick(rng, &["\">=30\"", "\"<60\"", "\">20\""]);
                format!("={name}(Data!G1:G8,{crit},Data!J1:J8)")
            }
        }
        "COUNTIFS" => {
            let cat = pick(rng, &["\"North\"", "\"South\"", "\"*t\""]);
            let num = pick(rng, &["\">=20\"", "\"<=45\"", "\">10\""]);
            format!("=COUNTIFS(Data!I1:I8,{cat},Data!J1:J8,{num})")
        }
        "SUMIFS" | "AVERAGEIFS" | "MAXIFS" | "MINIFS" => {
            let cat = pick(rng, &["\"North\"", "\"South\"", "\"East\""]);
            let num = pick(rng, &["\">=15\"", "\"<50\""]);
            format!("={name}(Data!J1:J8,Data!I1:I8,{cat},Data!J1:J8,{num})")
        }
        "SUMPRODUCT" => {
            let r1 = pick(rng, &["Data!B1:B8", "Data!J1:J8"]);
            let r2 = pick(rng, &["Data!C1:C8", "Data!G1:G8"]);
            format!("=SUMPRODUCT({r1},{r2})")
        }
        "SUBTOTAL" => {
            let fnum = pick(rng, &["1", "2", "3", "4", "5", "6", "9", "109", "101"]);
            let r = pick(rng, &["Data!B1:B16", "Data!J1:J8", "Data!S1:S8"]);
            format!("=SUBTOTAL({fnum},{r})")
        }
        "AGGREGATE" => {
            let fnum = pick(rng, &["1", "2", "4", "5", "9", "12"]);
            let opt = pick(rng, &["0", "2", "4", "6"]);
            let r = pick(rng, &["Data!B1:B16", "Data!S1:S8", "Data!N1:Q4"]);
            format!("=AGGREGATE({fnum},{opt},{r})")
        }

        // --- Lookup & Reference ---
        "VLOOKUP" => {
            if idx.is_multiple_of(3) {
                let k = pick(rng, &["15", "25", "45", "75", "85"]);
                format!("=VLOOKUP({k},Data!G1:H8,2,TRUE)")
            } else {
                let k = pick(rng, &["10", "20", "40", "60", "80", "99"]);
                format!("=VLOOKUP({k},Data!G1:H8,2,FALSE)")
            }
        }
        "HLOOKUP" => {
            let k = pick(rng, &["\"K1\"", "\"K2\"", "\"K3\"", "\"K4\"", "\"K9\""]);
            format!("=HLOOKUP({k},Aux!A1:D2,2,FALSE)")
        }
        "XLOOKUP" => {
            let mode = pick(rng, &["0", "-1", "1"]);
            let k = pick(rng, &["10", "25", "40", "75", "99"]);
            format!("=XLOOKUP({k},Data!G1:G8,Data!H1:H8,\"Missing\",{mode})")
        }
        "XMATCH" => {
            let mode = pick(rng, &["0", "-1", "1"]);
            let k = pick(rng, &["10", "25", "40", "75", "99"]);
            format!("=XMATCH({k},Data!G1:G8,{mode})")
        }
        "LOOKUP" => {
            let k = pick(rng, &["15", "30", "55", "80", "5"]);
            format!("=LOOKUP({k},Data!G1:G8,Data!H1:H8)")
        }
        "MATCH" => {
            if idx.is_multiple_of(3) {
                let pat = pick(rng, &["\"alpha*\"", "\"beta*\"", "\"Hello\""]);
                format!("=MATCH({pat},Data!E1:E9,0)")
            } else if idx % 3 == 1 {
                let k = pick(rng, &["15", "35", "65", "80"]);
                format!("=MATCH({k},Data!G1:G8,1)")
            } else {
                let k = pick(rng, &["10", "30", "70", "99"]);
                format!("=MATCH({k},Data!G1:G8,0)")
            }
        }
        "INDEX" => {
            let r = rand_range(rng, 1, 4);
            let c = rand_range(rng, 1, 4);
            format!("=INDEX(Data!N1:Q4,{r},{c})")
        }
        "CHOOSE" => {
            let k = rand_range(rng, 1, 4);
            format!(
                "=CHOOSE({k},{},{},{},\"ok\")",
                pick(rng, &DATA_B_REFS),
                pick(rng, &DATA_C_POS),
                rand_range(rng, 10, 99)
            )
        }
        "ROW" => {
            let cell = pick(rng, &["Data!A1", "Data!G5", "Aux!B2", "Data!N4"]);
            format!("=ROW({cell})")
        }
        "COLUMN" => {
            let cell = pick(rng, &["Data!A1", "Data!G5", "Aux!C2", "Data!Q4"]);
            format!("=COLUMN({cell})")
        }
        "ROWS" => {
            let r = pick(
                rng,
                &["Data!A1:B8", "Data!G1:H8", "Data!N1:Q4", "Aux!A1:D2"],
            );
            format!("=ROWS({r})")
        }
        "COLUMNS" => {
            let r = pick(
                rng,
                &["Data!A1:E8", "Data!G1:H8", "Data!N1:Q4", "Aux!A1:D2"],
            );
            format!("=COLUMNS({r})")
        }
        "AREAS" => String::from("=AREAS(Data!N1:Q4)"),
        "ADDRESS" => {
            let r = rand_range(rng, 1, 20);
            let c = rand_range(rng, 1, 10);
            let abs_m = rand_range(rng, 1, 4);
            if idx.is_multiple_of(3) {
                format!("=ADDRESS({r},{c},{abs_m},TRUE,\"Data\")")
            } else {
                format!("=ADDRESS({r},{c},{abs_m})")
            }
        }
        "INDIRECT" => {
            let target = pick(
                rng,
                &[
                    "\"Data!A1\"",
                    "\"Data!B3\"",
                    "\"Data!G4\"",
                    "\"Data!H2\"",
                    "\"Aux!B2\"",
                ],
            );
            format!("=INDIRECT({target})")
        }
        "OFFSET" => {
            let r = rand_range(rng, 0, 3);
            let c = rand_range(rng, 0, 3);
            format!("=OFFSET(Data!N1,{r},{c},1,1)")
        }

        // --- Dynamic Arrays (placed in 8x8 slots, max 6x6 output) ---
        "SEQUENCE" => {
            let rows = rand_range(rng, 1, 5);
            let cols = rand_range(rng, 1, 4);
            let start = rand_range(rng, -5, 10);
            let step = rand_range(rng, 1, 4);
            format!("=SEQUENCE({rows},{cols},{start},{step})")
        }
        "TRANSPOSE" => {
            let r = pick(rng, &["Data!N1:Q4", "Aux!A1:D2", "Data!G1:H5"]);
            format!("=TRANSPOSE({r})")
        }
        "FILTER" => {
            let op = pick(rng, &[">20", ">=30", "<45", ">100"]);
            format!("=FILTER(Data!I1:J6,Data!J1:J6{op},\"Empty\")")
        }
        "SORT" => {
            let col = rand_range(rng, 1, 2);
            let ord = if idx.is_multiple_of(2) { 1 } else { -1 };
            format!("=SORT(Data!I1:J6,{col},{ord})")
        }
        "SORTBY" => {
            let ord = if idx.is_multiple_of(2) { 1 } else { -1 };
            format!("=SORTBY(Data!I1:I6,Data!J1:J6,{ord})")
        }
        "UNIQUE" => {
            let src = if idx % 25 == 24 {
                "Data!F1:F6"
            } else {
                pick(rng, &["Data!S1:S6", "Data!I1:I6", "Data!G1:G5"])
            };
            format!("=UNIQUE({src})")
        }
        "CHOOSECOLS" => {
            let c1 = rand_range(rng, 1, 4);
            let c2 = rand_range(rng, 1, 4);
            format!("=CHOOSECOLS(Data!N1:Q4,{c1},{c2})")
        }
        "CHOOSEROWS" => {
            let r1 = rand_range(rng, 1, 4);
            let r2 = rand_range(rng, 1, 4);
            format!("=CHOOSEROWS(Data!N1:Q4,{r1},{r2})")
        }
        "TAKE" => {
            let r = pick(rng, &["1", "2", "3", "-2"]);
            let c = pick(rng, &["1", "2", "3", "-2"]);
            format!("=TAKE(Data!N1:Q4,{r},{c})")
        }
        "DROP" => {
            let r = pick(rng, &["1", "2", "-1"]);
            let c = pick(rng, &["1", "2", "-1"]);
            format!("=DROP(Data!N1:Q4,{r},{c})")
        }
        "HSTACK" => String::from("=HSTACK(Data!A1:B4,Data!C1:D4)"),
        "VSTACK" => String::from("=VSTACK(Data!A1:D2,Data!G1:J2)"),
        "TEXTSPLIT" => {
            let choices = [
                ("Data!U1", "\",\""),
                ("Data!U2", "\"-\""),
                ("Data!U3", "\"::\""),
                ("Data!U4", "\"/\""),
                ("Data!U5", "\";\""),
            ];
            let (cell, delim) = choices[rng.next_below(choices.len() as u64) as usize];
            format!("=TEXTSPLIT({cell},{delim})")
        }
        "FREQUENCY" => {
            let data_r = pick(rng, &["Data!B1:B16", "Data!J1:J8", "Data!S1:S8"]);
            let bins_r = pick(rng, &["Data!R1:R4", "Data!R1:R5"]);
            format!("=FREQUENCY({data_r},{bins_r})")
        }

        // --- Date & Time ---
        "DATE" => {
            if idx % 20 == 19 {
                String::from("=DATE(1900,2,29)")
            } else {
                let y = rand_range(rng, 2018, 2028);
                let m = rand_range(rng, 1, 14);
                let d = rand_range(rng, 0, 29);
                format!("=DATE({y},{m},{d})")
            }
        }
        "YEAR" | "MONTH" | "DAY" => {
            let dc = if idx % 30 == 29 {
                "Data!K9"
            } else {
                pick(rng, &DATA_DATES)
            };
            format!("={name}({dc})")
        }
        "EDATE" | "EOMONTH" => {
            let dc = pick(rng, &DATA_DATES);
            let delta = rand_range(rng, -12, 12);
            format!("={name}({dc},{delta})")
        }
        "DAYS" => {
            let d1 = pick(rng, &DATA_DATES);
            let d2 = pick(rng, &DATA_DATES);
            format!("=DAYS({d1},{d2})")
        }
        "TIME" => {
            let h = rand_range(rng, 0, 23);
            let m = rand_range(rng, 0, 59);
            let s = rand_range(rng, 0, 59);
            format!("=TIME({h},{m},{s})")
        }
        "HOUR" | "MINUTE" | "SECOND" => {
            let tc = pick(rng, &DATA_TIMES);
            format!("={name}({tc})")
        }
        "DATEVALUE" => {
            let dc = pick(rng, &["Data!V1", "Data!V2", "Data!V3"]);
            format!("=DATEVALUE({dc})")
        }
        "TIMEVALUE" => {
            let tc = pick(rng, &["Data!V4", "Data!V5", "Data!V6"]);
            format!("=TIMEVALUE({tc})")
        }
        "WEEKDAY" => {
            let dc = pick(rng, &DATA_DATES);
            let rt = pick(rng, &["1", "2", "3", "11", "17"]);
            format!("=WEEKDAY({dc},{rt})")
        }
        "WEEKNUM" => {
            let dc = pick(rng, &DATA_DATES);
            let rt = pick(rng, &["1", "2", "21"]);
            format!("=WEEKNUM({dc},{rt})")
        }
        "ISOWEEKNUM" => format!("=ISOWEEKNUM({})", pick(rng, &DATA_DATES)),
        "DATEDIF" => {
            let pairs = [
                ("Data!K7", "Data!K6"),
                ("Data!K1", "Data!K4"),
                ("Data!K2", "Data!K5"),
                ("Data!K3", "Data!K6"),
            ];
            let (s, e) = pairs[rng.next_below(pairs.len() as u64) as usize];
            let unit = pick(rng, &["\"D\"", "\"M\"", "\"Y\"", "\"YM\"", "\"YD\""]);
            format!("=DATEDIF({s},{e},{unit})")
        }
        "DAYS360" => {
            let pairs = [
                ("Data!K7", "Data!K6"),
                ("Data!K1", "Data!K4"),
                ("Data!K2", "Data!K5"),
            ];
            let (s, e) = pairs[rng.next_below(pairs.len() as u64) as usize];
            let method = if idx.is_multiple_of(2) {
                "FALSE"
            } else {
                "TRUE"
            };
            format!("=DAYS360({s},{e},{method})")
        }
        "NETWORKDAYS" => {
            let pairs = [
                ("Data!K7", "Data!K6"),
                ("Data!K4", "Data!K6"),
                ("Data!K2", "Data!K5"),
            ];
            let (s, e) = pairs[rng.next_below(pairs.len() as u64) as usize];
            if idx.is_multiple_of(2) {
                format!("=NETWORKDAYS({s},{e})")
            } else {
                format!("=NETWORKDAYS({s},{e},Data!T1:T6)")
            }
        }
        "WORKDAY" => {
            let s = pick(rng, &DATA_DATES);
            let days = rand_range(rng, -20, 40);
            if idx.is_multiple_of(2) {
                format!("=WORKDAY({s},{days})")
            } else {
                format!("=WORKDAY({s},{days},Data!T1:T6)")
            }
        }
        "YEARFRAC" => {
            let pairs = [
                ("Data!K7", "Data!K6"),
                ("Data!K1", "Data!K4"),
                ("Data!K2", "Data!K5"),
            ];
            let (s, e) = pairs[rng.next_below(pairs.len() as u64) as usize];
            let basis = rand_range(rng, 0, 4);
            format!("=YEARFRAC({s},{e},{basis})")
        }

        // --- Statistics ---
        "MODE" | "MODE.SNGL" => {
            let r = if idx % 25 == 24 {
                "Data!B1:B16"
            } else {
                pick(rng, &["Data!S1:S8", "Data!S2:S7"])
            };
            format!("={name}({r})")
        }
        "MEDIAN" | "STDEV" | "STDEV.S" | "STDEV.P" | "VAR" | "VAR.S" | "VAR.P" => {
            let r = pick(rng, &["Data!S1:S8", "Data!B1:B16", "Data!N1:Q4"]);
            format!("={name}({r})")
        }
        "LARGE" | "SMALL" => {
            let k = rand_range(rng, 1, 8);
            let r = pick(rng, &["Data!S1:S8", "Data!B1:B16"]);
            format!("={name}({r},{k})")
        }
        "RANK" | "RANK.EQ" | "RANK.AVG" => {
            let x = pick(rng, &["10", "20", "30", "40", "50"]);
            let ord = if idx.is_multiple_of(2) { 0 } else { 1 };
            format!("={name}({x},Data!S1:S8,{ord})")
        }
        "PERCENTILE" | "PERCENTILE.INC" => {
            let p = f64::from(rand_range(rng, 0, 10)) / 10.0;
            format!("={name}(Data!S1:S8,{p:.1})")
        }
        "PERCENTILE.EXC" => {
            let p = f64::from(rand_range(rng, 2, 8)) / 10.0;
            format!("=PERCENTILE.EXC(Data!S1:S8,{p:.1})")
        }
        "QUARTILE" | "QUARTILE.INC" => {
            let q = rand_range(rng, 0, 4);
            format!("={name}(Data!S1:S8,{q})")
        }
        "QUARTILE.EXC" => {
            let q = rand_range(rng, 1, 3);
            format!("=QUARTILE.EXC(Data!S1:S8,{q})")
        }
        "CORREL" => {
            let r1 = pick(rng, &["Data!C1:C8", "Data!S1:S8", "Data!B1:B8"]);
            let r2 = pick(rng, &["Data!S1:S8", "Data!J1:J8", "Data!G1:G8"]);
            format!("=CORREL({r1},{r2})")
        }

        // --- Financial ---
        "PMT" | "PV" | "FV" => {
            let rate = 0.005 * f64::from(rand_range(rng, 1, 20));
            let nper = rand_range(rng, 6, 60);
            let a = rand_range(rng, 100, 2000);
            let t = if idx.is_multiple_of(3) { 1 } else { 0 };
            format!("={name}({rate:.3},{nper},-{a},0,{t})")
        }
        "NPER" => {
            let rate = 0.005 * f64::from(rand_range(rng, 1, 10));
            let pmt = rand_range(rng, 150, 400);
            let pv = rand_range(rng, 1000, 3000);
            format!("=NPER({rate:.3},-{pmt},{pv})")
        }
        "RATE" => {
            let nper = rand_range(rng, 12, 36);
            let pmt = -rand_range(rng, 110, 180);
            let pv = nper * 90;
            format!("=RATE({nper},{pmt},{pv})")
        }
        "NPV" => {
            let rate = 0.02 * f64::from(rand_range(rng, 1, 8));
            format!("=NPV({rate:.2},Data!M2:M5,Data!M7:M8)")
        }
        "IRR" => {
            let guess = 0.05 * f64::from(rand_range(rng, 1, 4));
            if idx.is_multiple_of(2) {
                format!("=IRR(Data!M1:M5,{guess:.2})")
            } else {
                format!("=IRR(Data!M1:M8,{guess:.2})")
            }
        }
        "SLN" => {
            let cost = rand_range(rng, 5000, 20000);
            let salv = rand_range(rng, 500, 2000);
            let life = rand_range(rng, 3, 15);
            format!("=SLN({cost},{salv},{life})")
        }

        // --- Logic & Information ---
        "IF" => format!(
            "=IF({}>{},{},{})",
            pick(rng, &DATA_B_REFS),
            rand_range(rng, 0, 30),
            pick(rng, &DATA_C_POS),
            rand_range(rng, 1, 50)
        ),
        "IFERROR" => {
            let den = pick(rng, &["Data!A6", "Data!B1", "Data!C2"]);
            format!("=IFERROR(10/{den},999)")
        }
        "IFNA" => {
            let probe = pick(rng, &["MATCH(99,Data!G1:G8,0)", "MATCH(30,Data!G1:G8,0)"]);
            format!("=IFNA({probe},\"NotFound\")")
        }
        "IFS" => {
            let x = pick(rng, &DATA_B_REFS);
            format!("=IFS({x}>50,\"High\",{x}>10,\"Mid\",TRUE,\"Low\")")
        }
        "SWITCH" => {
            let k = rand_range(rng, 1, 4);
            format!("=SWITCH({k},1,\"One\",2,\"Two\",3,\"Three\",\"Other\")")
        }
        "AND" | "OR" | "XOR" => {
            let a = rand_range(rng, 0, 2);
            let b = rand_range(rng, 0, 2);
            let s1 = if a == 0 { "Data!F1" } else { "Data!B1>5" };
            let s2 = if b == 0 { "Data!F2" } else { "Data!B3<0" };
            format!("={name}({s1},{s2})")
        }
        "NOT" => format!("=NOT({})", pick(rng, &["Data!F1", "Data!F2", "Data!B1>5"])),
        "TRUE" | "FALSE" | "NA" => format!("={name}()"),
        "ISBLANK" | "ISTEXT" | "ISLOGICAL" | "ISERR" | "ISNA" | "ISERROR" | "ISNONTEXT" => {
            let cell = pick(
                rng,
                &[
                    "Data!A1", "Data!E1", "Data!E12", "Data!F1", "Data!W1", "Data!W3", "Data!W4",
                ],
            );
            format!("={name}({cell})")
        }
        "ISNUMBER" | "ISFORMULA" => {
            let cell = if idx % 25 == 24 {
                "Data!F1"
            } else {
                pick(
                    rng,
                    &[
                        "Data!A1", "Data!E1", "Data!E12", "Data!W1", "Data!W3", "Data!W4",
                    ],
                )
            };
            format!("={name}({cell})")
        }
        "ISREF" => {
            let arg = pick(rng, &["Data!A1", "Data!N1:Q4", "Aux!A1", "Data!E1", "123"]);
            format!("=ISREF({arg})")
        }
        "ISEVEN" | "ISODD" => {
            let x = pick(rng, &["1", "2", "3.9", "-4.1", "Data!B5", "Data!B12"]);
            format!("={name}({x})")
        }
        "ERROR.TYPE" => {
            let arg = pick(rng, &["Data!W3", "Data!W4", "Data!W1", "1/0", "NA()"]);
            format!("=ERROR.TYPE({arg})")
        }
        "TYPE" => {
            let arg = if idx % 25 == 24 {
                "Data!N1:Q2"
            } else if idx % 25 == 23 {
                "Data!F1"
            } else {
                pick(
                    rng,
                    &["Data!A1", "Data!E1", "Data!W3", "Data!W1", "Data!K1"],
                )
            };
            format!("=TYPE({arg})")
        }

        // --- Text & Conversion ---
        "LEN" | "LOWER" | "UPPER" | "TRIM" | "PROPER" | "CLEAN" | "T" | "N" => {
            let t = pick(rng, &DATA_TEXT);
            format!("={name}({t})")
        }
        "LEFT" | "RIGHT" => {
            let t = pick(rng, &DATA_TEXT);
            let k = rand_range(rng, 0, 8);
            format!("={name}({t},{k})")
        }
        "MID" => {
            let t = pick(rng, &DATA_TEXT);
            let st = rand_range(rng, 1, 5);
            let k = rand_range(rng, 0, 6);
            format!("=MID({t},{st},{k})")
        }
        "CONCAT" | "CONCATENATE" => {
            let t1 = pick(rng, &DATA_TEXT);
            let t2 = pick(rng, &DATA_TEXT);
            format!("={name}({t1},\"-\",{t2})")
        }
        "TEXTJOIN" => {
            let ignore = if idx.is_multiple_of(2) {
                "TRUE"
            } else {
                "FALSE"
            };
            format!("=TEXTJOIN(\"|\",{ignore},Data!E1:E5)")
        }
        "TEXTBEFORE" | "TEXTAFTER" => {
            let n = pick(rng, &["1", "2", "-1"]);
            if idx.is_multiple_of(2) {
                format!("={name}(Data!U1,\",\",{n})")
            } else {
                format!("={name}(Data!U2,\"-\",{n})")
            }
        }
        "EXACT" => {
            let t1 = pick(rng, &DATA_TEXT);
            let t2 = pick(rng, &["\"Hello\"", "\"hello\"", "Data!E1", "Data!E3"]);
            format!("=EXACT({t1},{t2})")
        }
        "REPT" => {
            let t = pick(rng, &["\"ab\"", "\"*\"", "Data!E3"]);
            let n = rand_range(rng, 0, 5);
            format!("=REPT({t},{n})")
        }
        "CHAR" => {
            let code = rand_range(rng, 65, 90);
            format!("=CHAR({code})")
        }
        "CODE" => format!("=CODE({})", pick(rng, &["Data!E1", "Data!E3", "Data!E4"])),
        "UNICHAR" => {
            let code = pick(rng, &["65", "90", "20013", "9731"]);
            format!("=UNICHAR({code})")
        }
        "UNICODE" => format!("=UNICODE({})", pick(rng, &["Data!E1", "Data!E9"])),
        "FIND" | "SEARCH" => {
            let hay = pick(rng, &["Data!E1", "Data!E2", "Data!E5", "Data!E6"]);
            let pat = pick(rng, &["\"e\"", "\"l\"", "\"alpha\"", "\"1\""]);
            format!("={name}({pat},{hay},1)")
        }
        "SUBSTITUTE" => {
            let src = pick(rng, &["Data!E2", "Data!E5", "Data!E6"]);
            if idx.is_multiple_of(2) {
                format!("=SUBSTITUTE({src},\"e\",\"X\")")
            } else {
                format!("=SUBSTITUTE({src},\"a\",\"Z\",1)")
            }
        }
        "REPLACE" => {
            let src = pick(rng, &["Data!E1", "Data!E5", "Data!E6"]);
            let st = rand_range(rng, 1, 4);
            let len = rand_range(rng, 1, 3);
            format!("=REPLACE({src},{st},{len},\"ZZ\")")
        }
        "FIXED" => {
            let n = pick(rng, &["1234.567", "-42.1", "Data!B2", "Data!A7"]);
            let d = rand_range(rng, 0, 3);
            let no_commas = if idx.is_multiple_of(2) {
                "FALSE"
            } else {
                "TRUE"
            };
            format!("=FIXED({n},{d},{no_commas})")
        }
        "DOLLAR" => {
            let n = pick(rng, &["1234.56", "42.5", "0", "Data!B2"]);
            let d = rand_range(rng, 0, 2);
            format!("=DOLLAR({n},{d})")
        }
        "NUMBERVALUE" => {
            if idx % 25 == 24 {
                String::from("=NUMBERVALUE(\"-42.25\")")
            } else if idx.is_multiple_of(3) {
                String::from("=NUMBERVALUE(\"1,234.50\",\".\",\",\")")
            } else {
                let s = pick(rng, &["\"12.5%\"", "\"-42.25\"", "Data!E10", "\"0.125\""]);
                format!("=NUMBERVALUE({s},\".\")")
            }
        }
        "VALUE" => {
            let s = pick(
                rng,
                &["\"123.45\"", "\"-42\"", "\"0.5\"", "\"12.5%\"", "Data!E10"],
            );
            format!("=VALUE({s})")
        }
        "TEXT" => {
            if idx.is_multiple_of(2) {
                let n = pick(rng, &["Data!B1", "Data!B2", "Data!B4", "Data!A7"]);
                let fmt = pick(rng, &["\"0\"", "\"0.00\"", "\"#,##0.00\"", "\"0.0%\""]);
                format!("=TEXT({n},{fmt})")
            } else {
                let dc = pick(rng, &DATA_DATES);
                format!("=TEXT({dc},\"yyyy-mm-dd\")")
            }
        }
        other => panic!("missing generator branch for deterministic catalog function `{other}`"),
    }
}

fn gen_compound_formula(rng: &mut Rng, idx: usize) -> String {
    match idx % 10 {
        0 => format!(
            "=ROUND({}*TaxRate+{}/{},2)",
            pick(rng, &DATA_B_REFS),
            pick(rng, &DATA_C_POS),
            pick(rng, &["2", "4", "5"])
        ),
        1 => format!(
            "=IFERROR(VLOOKUP({},Data!G1:H8,2,FALSE)&\"-\"&UPPER({}),\"NA\")",
            pick(rng, &["10", "20", "40", "60", "99"]),
            pick(rng, &DATA_TEXT)
        ),
        2 => format!(
            "=SUM(Scores)+INDEX(Grid4x4,{},{})",
            rand_range(rng, 1, 4),
            rand_range(rng, 1, 4)
        ),
        3 => format!(
            "=HLOOKUP(\"K{}\",Aux!A1:D2,2,FALSE)+SUM(Aux!A3:D3)",
            rand_range(rng, 1, 4)
        ),
        4 => {
            if idx.is_multiple_of(2) {
                String::from("=(Data!A1+Data!A2)=Data!A3")
            } else {
                String::from("=Data!A3-(Data!A1+Data!A2)")
            }
        }
        5 => format!("=-{}^{}", rand_range(rng, 2, 5), rand_range(rng, 2, 3)),
        6 => format!(
            "=SUMIFS(Data!J1:J8,Data!I1:I8,{})/MAX(1,COUNTIF(Data!I1:I8,{}))",
            pick(rng, &["\"North\"", "\"South\"", "\"East\""]),
            pick(rng, &["\"North\"", "\"South\""])
        ),
        7 => format!(
            "=IF(AND(ISNUMBER({}),{}<100),TEXT(EOMONTH({},1),\"yyyy-mm-dd\"),\"OTHER\")",
            pick(rng, &DATA_B_REFS),
            pick(rng, &DATA_B_REFS),
            pick(rng, &DATA_DATES)
        ),
        8 => format!(
            "=XMATCH({},Data!G1:G8,1)+SUMPRODUCT(Data!J1:J4,Aux!A3:D3)",
            pick(rng, &["15", "25", "45", "65"])
        ),
        _ => format!(
            "=LEN(TRIM({}))*10+MOD({},7)",
            pick(rng, &DATA_TEXT),
            pick(rng, &DATA_C_POS)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Cursor, Read as _};

    use reticula_formula::functions::catalog::FUNCTION_CATALOG;
    use zip::ZipArchive;

    use super::{
        ARRAY_MAX_BOX, VOLATILE_CLOCK_OR_RNG, build_and_eval_array_batch,
        build_and_eval_scalar_batch, deterministic_catalog_functions, generate_stratified_corpus,
    };
    use crate::diff::NormValue;

    #[test]
    fn stratified_corpus_covers_all_195_deterministic_catalog_entries_and_strips_cached_values() {
        let det = deterministic_catalog_functions();
        assert_eq!(
            det.len(),
            FUNCTION_CATALOG.len() - VOLATILE_CLOCK_OR_RNG.len()
        );
        assert_eq!(det.len(), 195);

        let corpus = generate_stratified_corpus(0xD1FF_2026, 2);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut scalar = Vec::new();
        let mut array = Vec::new();
        for c in corpus {
            *counts.entry(c.function.clone()).or_default() += 1;
            if c.is_dynamic_array {
                array.push(c);
            } else {
                scalar.push(c);
            }
        }
        for fn_name in det {
            assert_eq!(
                counts.get(fn_name).copied().unwrap_or(0),
                2,
                "expected 2 samples for deterministic function {fn_name}"
            );
        }

        let (s_placed, s_xlsx) =
            build_and_eval_scalar_batch("test_scalar.xlsx", &scalar).expect("scalar batch builds");
        let (a_placed, a_xlsx) =
            build_and_eval_array_batch("test_array.xlsx", &array).expect("array batch builds");

        let total = s_placed.len() + a_placed.len();
        let non_err = s_placed
            .iter()
            .chain(a_placed.iter())
            .filter(|p| !matches!(p.reticula_top, NormValue::Error(_)))
            .count();
        assert!(
            non_err * 10 >= total * 8,
            "expected >= 80% non-error on mini stratified corpus, got {non_err}/{total}"
        );
        for p in &a_placed {
            assert!(p.reticula_grid.len() <= ARRAY_MAX_BOX as usize);
            assert!(
                p.reticula_grid
                    .iter()
                    .all(|r| r.len() <= ARRAY_MAX_BOX as usize)
            );
        }

        // Verify input XLSX strips `<v>` inside `<c><f>...` and forces fullCalcOnLoad="1" + calcId="191029".
        for xlsx in [&s_xlsx, &a_xlsx] {
            let mut za = ZipArchive::new(Cursor::new(xlsx.as_slice())).expect("valid zip");
            let mut wb_xml = String::new();
            za.by_name("xl/workbook.xml")
                .expect("xl/workbook.xml")
                .read_to_string(&mut wb_xml)
                .expect("read workbook.xml");
            assert!(wb_xml.contains("fullCalcOnLoad=\"1\""));
            assert!(wb_xml.contains("calcId=\"191029\""));

            for i in 0..za.len() {
                let mut entry = za.by_index(i).expect("entry");
                let name = entry.name().expect("name").into_owned();
                if name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml") {
                    let mut xml = String::new();
                    entry.read_to_string(&mut xml).expect("read sheet");
                    assert!(
                        !xml.contains("</f><v>"),
                        "formula cached value <v> leaked into `{name}`"
                    );
                }
            }
        }
    }
}
