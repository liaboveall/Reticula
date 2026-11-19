//! Formal M1a + M1b engine benchmark workloads:
//! B1 (20M cells + 1M formulas load), B2–B5, B6 (100k exact VLOOKUP),
//! B7a/B7b (3 formula groups), B8 (memory footprints), B11 (CoW snapshot save),
//! and B12 (50M-cell upper-limit load / save / 100k recalc)
//! (`04-engine.md`, `13-m1a.md` 7.4, `14-m1b.md` 8.3).

use std::collections::BTreeMap;
use std::time::Instant;

use reticula_core::axis::AxisRun;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{FormulaMode, Op, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block, Column, SlotMap};
use reticula_core::strings::StringTable;
use reticula_core::value::Value;
use reticula_core::viewport::{self, Page};
use reticula_formula::ast::{BinaryOp, Literal};
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};
use reticula_formula::engine::FormulaEngine;
use reticula_wire::snapshot::{
    SnapshotEncoder, SnapshotStep, decode_snapshot_any, encode_snapshot_v1,
};
use serde::Serialize;

/// Percentile summary in milliseconds over `samples` timed runs.
#[derive(Clone, Debug, Serialize)]
pub struct DistributionMs {
    /// Warmup iterations discarded prior to measurement.
    pub warmup: usize,
    /// Measured sample count.
    pub samples: usize,
    /// Median (`p50`) duration in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile duration in milliseconds.
    pub p95_ms: f64,
    /// 99th percentile duration in milliseconds.
    pub p99_ms: f64,
    /// Maximum duration in milliseconds.
    pub max_ms: f64,
    /// Arithmetic mean in milliseconds.
    pub mean_ms: f64,
}

impl DistributionMs {
    /// Computes summary statistics from raw millisecond samples.
    #[must_use]
    pub fn from_samples(warmup: usize, mut raw: Vec<f64>) -> Self {
        raw.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let n = raw.len().max(1);
        let pick_pct = |pct: f64| -> f64 {
            if raw.is_empty() {
                return 0.0;
            }
            let idx = ((n as f64 - 1.0) * pct).ceil() as usize;
            raw[idx.min(n - 1)]
        };
        let sum: f64 = raw.iter().sum();
        Self {
            warmup,
            samples: raw.len(),
            p50_ms: round3(pick_pct(0.50)),
            p95_ms: round3(pick_pct(0.95)),
            p99_ms: round3(pick_pct(0.99)),
            max_ms: round3(raw.last().copied().unwrap_or(0.0)),
            mean_ms: round3(sum / n as f64),
        }
    }
}

/// Memory footprint entry for B8 (`14-m1b.md` 8.3).
#[derive(Clone, Debug, Serialize)]
pub struct MemoryFootprint {
    /// Kernel `Workbook::estimated_bytes()`.
    pub kernel_estimated_bytes: usize,
    /// Cell count populated.
    pub cells: u64,
    /// Formula count populated.
    pub formulas: usize,
    /// Distinct interned strings.
    pub unique_strings: usize,
}

/// B8 memory report across all required workload groups.
#[derive(Clone, Debug, Serialize)]
pub struct B8MemoryReport {
    /// 10,000,000 numeric cells (`1M × 10`).
    pub numeric_10m: MemoryFootprint,
    /// 1,000,000 string cells with high repetition (`500` distinct strings).
    pub strings_1m_repeated: MemoryFootprint,
    /// 1,000,000 string cells with unique short strings.
    pub strings_1m_unique: MemoryFootprint,
    /// 1,000,000 formulas `=B{r}*C{r}+$Z$1`.
    pub formulas_1m: MemoryFootprint,
}

/// Native storage & snapshot load/save summary for B1 / B11 / B12.
#[derive(Clone, Debug, Serialize)]
pub struct StorageBenchPair {
    /// Encoded `RTCLSNAP` v1 snapshot byte size.
    pub snapshot_bytes: usize,
    /// Decode + DepGraph rebuild + first `64 × 16` viewport page distribution.
    pub load_ms: DistributionMs,
    /// CoW chunked `SnapshotEncoder` save distribution.
    pub save_ms: DistributionMs,
}

/// Native B12 (`1M × 50` = 50M cells + 100k formulas) summary.
#[derive(Clone, Debug, Serialize)]
pub struct B12NativeReport {
    /// Encoded `RTCLSNAP` v1 snapshot byte size.
    pub snapshot_bytes: usize,
    /// Kernel `estimated_bytes` on the 50M-cell workbook.
    pub kernel_estimated_bytes: usize,
    /// Load (decode + DepGraph + first viewport page).
    pub load_ms: DistributionMs,
    /// CoW chunked save.
    pub save_ms: DistributionMs,
    /// Edit `$Z$1` triggering 100k dependent recalculation.
    pub recalc_100k_ms: DistributionMs,
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Complete native benchmark report for M1a + M1b (B1–B8, B11, B12).
#[derive(Clone, Debug, Serialize)]
pub struct NativeBenchSuite {
    /// B1 + B11 (with 1M formulas): `1M × 20` (20M cells + 1M formulas) load & CoW save.
    pub b1_b11_20m_with_1m_formulas: StorageBenchPair,
    /// B1 + B11 (pure numeric control): `1M × 20` (20M cells, 0 formulas) load & CoW save.
    pub b1_b11_20m_numeric_only: StorageBenchPair,
    /// B2: 1,000,000-row `=SUM(A1:A1000000)` evaluation.
    pub b2_sum_1m: DistributionMs,
    /// B3: 1,000,000 formulas `=B2*C2+$Z$1` full recalculation.
    pub b3_full_recalc_1m: DistributionMs,
    /// B4: 100,000 formulas `=A2*$Z$1`, edit `Z1` incremental recalc (incl. dirty marking).
    pub b4_incremental_100k: DistributionMs,
    /// B5: 1,000,000-row chain `=C1+B2`, edit first row (non-recursive stack).
    pub b5_chain_1m: DistributionMs,
    /// B6: 100,000 exact bitwise `=VLOOKUP(D2, $A$2:$B$100001, 2, FALSE)` on 100,000 rows.
    pub b6_vlookup_100k_exact: DistributionMs,
    /// B7a Group 1 (row-local `=B2*C2+$Z$1`, 1M rows/formulas): insert 1 row at top.
    pub b7a_insert_top_row_local: DistributionMs,
    /// B7b Group 1: first range-referenced cell edit immediately after B7a Group 1.
    pub b7b_edit_after_insert_row_local: DistributionMs,
    /// B7a Group 2 (cross-insertion-point range `=SUM(A1:A1000000)` + 999,999 formulas): insert 1 row inside range.
    pub b7a_insert_cross_range: DistributionMs,
    /// B7b Group 2: edit a range-referenced cell immediately after B7a Group 2.
    pub b7b_edit_after_insert_cross_range: DistributionMs,
    /// B7a Group 3 (100,000 `ROW()` structure-sensitive formulas on 1M-row sheet): insert 1 row at top.
    pub b7a_insert_structural_row_fn: DistributionMs,
    /// B7b Group 3: edit a range-referenced cell immediately after B7a Group 3.
    pub b7b_edit_after_insert_structural: DistributionMs,
    /// B8: Kernel memory footprints for 10M numbers, 1M strings (repeated & unique), and 1M formulas.
    pub b8_memory: B8MemoryReport,
    /// B12: Upper-limit `1M × 50` (50M cells + 100k formulas) load, save, and 100k recalc.
    pub b12_50m_limit: B12NativeReport,
}

/// Runs the complete native benchmark suite and returns percentile distributions.
#[must_use]
pub fn run_native_suite(quick: bool) -> NativeBenchSuite {
    let (w_fast, n_fast) = if quick { (2, 5) } else { (3, 15) };
    let (w_heavy, n_heavy) = if quick { (1, 3) } else { (2, 5) };
    let (w_vlookup, n_vlookup) = if quick { (0, 1) } else { (1, 3) };

    NativeBenchSuite {
        b1_b11_20m_with_1m_formulas: bench_storage_pair(1_000_000, 20, 1_000_000, 1, n_heavy),
        b1_b11_20m_numeric_only: bench_storage_pair(1_000_000, 20, 0, 1, n_heavy),
        b2_sum_1m: bench_b2(w_fast, n_fast),
        b3_full_recalc_1m: bench_b3(w_heavy, n_heavy),
        b4_incremental_100k: bench_b4(w_fast, n_fast),
        b5_chain_1m: bench_b5(w_heavy, n_heavy),
        b6_vlookup_100k_exact: bench_b6(w_vlookup, n_vlookup),
        b7a_insert_top_row_local: bench_b7_row_local(w_fast, n_fast).0,
        b7b_edit_after_insert_row_local: bench_b7_row_local(w_fast, n_fast).1,
        b7a_insert_cross_range: bench_b7_cross_range(w_fast, n_fast).0,
        b7b_edit_after_insert_cross_range: bench_b7_cross_range(w_fast, n_fast).1,
        b7a_insert_structural_row_fn: bench_b7_structural(w_fast, n_fast).0,
        b7b_edit_after_insert_structural: bench_b7_structural(w_fast, n_fast).1,
        b8_memory: bench_b8_memory(),
        b12_50m_limit: bench_b12(1, n_heavy),
    }
}

fn bench_storage_pair(
    rows: u32,
    cols: u32,
    formula_rows: u32,
    warmup: usize,
    samples: usize,
) -> StorageBenchPair {
    let eng = build_dense_grid(rows, cols, formula_rows);
    let snap = encode_snapshot_v1(eng.workbook(), &[]).expect("encode_snapshot_v1");
    let snap_bytes = snap.len();

    let mut save_times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let t0 = Instant::now();
        let mut enc = SnapshotEncoder::new(eng.workbook(), Vec::new());
        let total;
        loop {
            match enc.encode_step(eng.workbook(), 8).expect("encode_step") {
                SnapshotStep::Progress | SnapshotStep::Chunk { .. } => {}
                SnapshotStep::Finished { total_len, .. } => {
                    total = total_len;
                    break;
                }
            }
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(total as usize, snap_bytes);
        if i >= warmup {
            save_times.push(ms);
        }
    }
    drop(eng);

    let mut load_times = Vec::with_capacity(samples);
    let mut scratch = String::with_capacity(64);
    for i in 0..(warmup + samples) {
        let t0 = Instant::now();
        let decoded = decode_snapshot_any(&snap).expect("decode_snapshot_any");
        let wb = decoded.engine.workbook();
        let sheet = wb.sheet(SheetId(0)).unwrap();
        let page = viewport::encode_page_with_formulas(
            sheet,
            Page { row: 0, col: 0 },
            1,
            &mut scratch,
            |r, c| wb.formula(SheetId(0), r, c).is_some(),
        );
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(!page.is_empty());
        assert_eq!(sheet.rows(), rows);
        if i >= warmup {
            load_times.push(ms);
        }
    }

    StorageBenchPair {
        snapshot_bytes: snap_bytes,
        load_ms: DistributionMs::from_samples(warmup, load_times),
        save_ms: DistributionMs::from_samples(warmup, save_times),
    }
}

fn bench_b12(warmup: usize, samples: usize) -> B12NativeReport {
    let mut eng = build_dense_grid(1_000_000, 50, 100_000);
    let kernel_estimated_bytes = eng.workbook().estimated_bytes();
    let snap = encode_snapshot_v1(eng.workbook(), &[]).expect("encode B12 snapshot");
    let snap_bytes = snap.len();

    let mut save_times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let t0 = Instant::now();
        let mut enc = SnapshotEncoder::new(eng.workbook(), Vec::new());
        while !matches!(
            enc.encode_step(eng.workbook(), 8).expect("encode_step"),
            SnapshotStep::Finished { .. }
        ) {}
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if i >= warmup {
            save_times.push(ms);
        }
    }

    let mut recalc_times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let v = 1.05 + (i as f64) * 0.02;
        let t0 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(0), ColId(25), &format!("{v}"))
            .unwrap();
        let n = eng.recalc_all();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, 100_000);
        if i >= warmup {
            recalc_times.push(ms);
        }
    }
    drop(eng);

    let mut load_times = Vec::with_capacity(samples);
    let mut scratch = String::with_capacity(64);
    for i in 0..(warmup + samples) {
        let t0 = Instant::now();
        let decoded = decode_snapshot_any(&snap).expect("decode B12 snapshot");
        let wb = decoded.engine.workbook();
        let sheet = wb.sheet(SheetId(0)).unwrap();
        let page = viewport::encode_page_with_formulas(
            sheet,
            Page { row: 0, col: 0 },
            1,
            &mut scratch,
            |r, c| wb.formula(SheetId(0), r, c).is_some(),
        );
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(!page.is_empty());
        assert_eq!(sheet.rows(), 1_000_000);
        assert_eq!(sheet.cols(), 50);
        if i >= warmup {
            load_times.push(ms);
        }
    }

    B12NativeReport {
        snapshot_bytes: snap_bytes,
        kernel_estimated_bytes,
        load_ms: DistributionMs::from_samples(warmup, load_times),
        save_ms: DistributionMs::from_samples(warmup, save_times),
        recalc_100k_ms: DistributionMs::from_samples(warmup, recalc_times),
    }
}

fn bench_b6(warmup: usize, samples: usize) -> DistributionMs {
    let rows = 100_000_u32;
    let mut sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), rows + 2, 6);
    for r in 1_u32..=rows {
        let row = RowId(u64::from(r));
        sheet.set_value_by_id(row, ColId(0), Value::Number(f64::from(r)));
        sheet.set_value_by_id(row, ColId(1), Value::Number(f64::from(r) * 1.5));
        let key = ((r.wrapping_mul(7919)) % rows) + 1;
        sheet.set_value_by_id(row, ColId(3), Value::Number(f64::from(key)));
        sheet.set_value_by_id(row, ColId(4), Value::Number(f64::from(key) * 1.5));
    }
    let mut wb = Workbook::from_sheet(sheet, None);
    let last_row = RowId(u64::from(rows));
    for r in 1_u32..=rows {
        let row = RowId(u64::from(r));
        let key = ((r.wrapping_mul(7919)) % rows) + 1;
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Call {
                name: String::from("VLOOKUP"),
                args: vec![
                    cell_expr(row, ColId(3), false),
                    BoundExpr::Reference(BoundRef::Area {
                        sheet: None,
                        missing_sheet: None,
                        first_row: RowId(1),
                        first_col: ColId(0),
                        last_row,
                        last_col: ColId(1),
                        abs_first_col: true,
                        abs_first_row: true,
                        abs_last_col: true,
                        abs_last_row: true,
                    }),
                    BoundExpr::Literal(Literal::Number(2.0)),
                    BoundExpr::Literal(Literal::Bool(false)),
                ],
            },
        };
        wb.insert_seeded_formula(
            SheetId(0),
            row,
            ColId(4),
            f,
            Value::Number(f64::from(key) * 1.5),
        );
    }
    let mut eng = FormulaEngine::from_workbook_clean(wb);

    let mut times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let v = 1.5 + (i as f64) * 0.25;
        let t0 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(1), ColId(1), &format!("{v}"))
            .unwrap();
        let n = eng.recalc_all();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, rows);
        if i >= warmup {
            times.push(ms);
        }
    }
    DistributionMs::from_samples(warmup, times)
}

fn bench_b8_memory() -> B8MemoryReport {
    let measure = |eng: FormulaEngine, cells: u64| -> MemoryFootprint {
        let wb = eng.workbook();
        MemoryFootprint {
            kernel_estimated_bytes: wb.estimated_bytes(),
            cells,
            formulas: wb.formulas().len(),
            unique_strings: wb.sheet(SheetId(0)).map_or(0, |s| s.strings().len()),
        }
    };
    B8MemoryReport {
        numeric_10m: measure(build_dense_grid(1_000_000, 10, 0), 10_000_000),
        strings_1m_repeated: measure(build_string_grid(1_000_000, 500), 1_000_000),
        strings_1m_unique: measure(build_string_grid(1_000_000, 1_000_000), 1_000_000),
        formulas_1m: measure(build_1m_row_local_engine(), 3_000_001),
    }
}

/// Builds a 1,000,000-row sheet for B2 (`=SUM(A1:A1000000)` in `B1`).
pub fn bench_b2(warmup: usize, samples: usize) -> DistributionMs {
    let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
    for r in 0_u32..1_000_000 {
        sheet.set_value_by_id(
            RowId(u64::from(r)),
            ColId(0),
            Value::Number(f64::from(r % 97) * 0.5),
        );
    }
    let wb = Workbook::from_sheet(sheet, None);
    let mut eng = FormulaEngine::from_workbook_clean(wb);
    eng.edit_cell_raw(SheetId(0), RowId(0), ColId(1), "=SUM(A1:A1000000)")
        .unwrap();
    eng.recalc_all();

    let mut times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let v = (i as f64) + 1.0;
        let t0 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(500_000), ColId(0), &format!("{v}"))
            .unwrap();
        eng.recalc_all();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if i >= warmup {
            times.push(ms);
        }
    }
    DistributionMs::from_samples(warmup, times)
}

/// B3: 1,000,000 formulas `=B{r}*C{r}+$Z$1` full recalculation.
pub fn bench_b3(warmup: usize, samples: usize) -> DistributionMs {
    let mut eng = build_1m_row_local_engine();
    let mut times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let v = (i as f64) + 2.0;
        let t0 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(0), ColId(25), &format!("{v}"))
            .unwrap();
        let n = eng.recalc_all();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, 1_000_000);
        if i >= warmup {
            times.push(ms);
        }
    }
    DistributionMs::from_samples(warmup, times)
}

/// B4: 100,000 formulas `=A{r}*$Z$1`, edit `Z1` incremental recalc (incl. dirty marking).
pub fn bench_b4(warmup: usize, samples: usize) -> DistributionMs {
    let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
    sheet.set_value_by_id(RowId(0), ColId(25), Value::Number(1.0));
    for r in 1_u32..=100_000 {
        sheet.set_value_by_id(
            RowId(u64::from(r)),
            ColId(0),
            Value::Number(f64::from(r) * 0.25),
        );
    }
    let mut wb = Workbook::from_sheet(sheet, None);
    for r in 1_u32..=100_000 {
        let row = RowId(u64::from(r));
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Binary {
                op: BinaryOp::Mul,
                left: Box::new(cell_expr(row, ColId(0), false)),
                right: Box::new(cell_expr(RowId(0), ColId(25), true)),
            },
        };
        wb.insert_seeded_formula(
            SheetId(0),
            row,
            ColId(1),
            f,
            Value::Number(f64::from(r) * 0.25),
        );
    }
    let mut eng = FormulaEngine::from_workbook_clean(wb);

    let mut times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let factor = 1.05 + (i as f64) * 0.01;
        let t0 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(0), ColId(25), &format!("{factor}"))
            .unwrap();
        let n = eng.recalc_all();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, 100_000);
        if i >= warmup {
            times.push(ms);
        }
    }
    DistributionMs::from_samples(warmup, times)
}

/// B5: 1,000,000-row cumulative chain `C1 = B1`, `C{r} = C{r-1} + B{r}`, edit `B1`.
pub fn bench_b5(warmup: usize, samples: usize) -> DistributionMs {
    let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
    for r in 0_u32..1_000_000 {
        sheet.set_value_by_id(RowId(u64::from(r)), ColId(1), Value::Number(1.0));
        sheet.set_value_by_id(
            RowId(u64::from(r)),
            ColId(2),
            Value::Number(f64::from(r + 1)),
        );
    }
    let mut wb = Workbook::from_sheet(sheet, None);
    wb.insert_seeded_formula(
        SheetId(0),
        RowId(0),
        ColId(2),
        BoundFormula {
            mode: FormulaMode::Dynamic,
            root: cell_expr(RowId(0), ColId(1), false),
        },
        Value::Number(1.0),
    );
    for r in 1_u32..1_000_000 {
        let prev = RowId(u64::from(r - 1));
        let cur = RowId(u64::from(r));
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Binary {
                op: BinaryOp::Add,
                left: Box::new(cell_expr(prev, ColId(2), false)),
                right: Box::new(cell_expr(cur, ColId(1), false)),
            },
        };
        wb.insert_seeded_formula(
            SheetId(0),
            cur,
            ColId(2),
            f,
            Value::Number(f64::from(r + 1)),
        );
    }
    let mut eng = FormulaEngine::from_workbook_clean(wb);

    let mut times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let v = (i as f64) + 2.0;
        let t0 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(0), ColId(1), &format!("{v}"))
            .unwrap();
        let n = eng.recalc_all();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, 1_000_000);
        if i >= warmup {
            times.push(ms);
        }
    }
    DistributionMs::from_samples(warmup, times)
}

/// B7a & B7b Group 1: 1,000,000 row-local formulas `=B{r}*C{r}+$Z$1` + summary
/// `=SUM(B2:B100001)`: insert 1 row at top (`after: None`), then edit `B50`.
fn bench_b7_row_local(warmup: usize, samples: usize) -> (DistributionMs, DistributionMs) {
    let mut eng = build_1m_row_local_engine();
    eng.edit_cell_raw(SheetId(0), RowId(0), ColId(4), "=SUM(B2:B100001)")
        .unwrap();
    eng.recalc_all();

    let mut ins_times = Vec::with_capacity(samples);
    let mut edit_times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let run = eng.allocator_mut().alloc_run(1).unwrap();
        let t0 = Instant::now();
        eng.apply_op(Op::InsertRows {
            sheet: SheetId(0),
            after: None,
            ids: run,
        })
        .unwrap();
        eng.recalc_all();
        let ins_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let val = 10.0 + (i as f64);
        let t1 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(50), ColId(1), &format!("{val}"))
            .unwrap();
        eng.recalc_all();
        let edit_ms = t1.elapsed().as_secs_f64() * 1000.0;

        if i >= warmup {
            ins_times.push(ins_ms);
            edit_times.push(edit_ms);
        }
    }
    (
        DistributionMs::from_samples(warmup, ins_times),
        DistributionMs::from_samples(warmup, edit_times),
    )
}

/// B7a & B7b Group 2: 999,999 formulas + range `=SUM(B1:B1000000)` spanning
/// across the insertion point (`after: Some(RowId(0))`).
fn bench_b7_cross_range(warmup: usize, samples: usize) -> (DistributionMs, DistributionMs) {
    let mut eng = build_1m_row_local_engine();
    eng.edit_cell_raw(SheetId(0), RowId(0), ColId(4), "=SUM(B1:B1000000)")
        .unwrap();
    eng.recalc_all();

    let mut ins_times = Vec::with_capacity(samples);
    let mut edit_times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let run = eng.allocator_mut().alloc_run(1).unwrap();
        let t0 = Instant::now();
        eng.apply_op(Op::InsertRows {
            sheet: SheetId(0),
            after: Some(RowId(0)),
            ids: run,
        })
        .unwrap();
        eng.recalc_all();
        let ins_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let val = 20.0 + (i as f64);
        let t1 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(50), ColId(1), &format!("{val}"))
            .unwrap();
        eng.recalc_all();
        let edit_ms = t1.elapsed().as_secs_f64() * 1000.0;

        if i >= warmup {
            ins_times.push(ins_ms);
            edit_times.push(edit_ms);
        }
    }
    (
        DistributionMs::from_samples(warmup, ins_times),
        DistributionMs::from_samples(warmup, edit_times),
    )
}

/// B7a & B7b Group 3: 1,000,000-row sheet with 100,000 structure-sensitive
/// `=ROW()+B{r}` formulas (which ALL become dirty on top-row insertion!).
fn bench_b7_structural(warmup: usize, samples: usize) -> (DistributionMs, DistributionMs) {
    let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
    for r in 0_u32..1_000_000 {
        sheet.set_value_by_id(RowId(u64::from(r)), ColId(1), Value::Number(1.0));
    }
    let mut wb = Workbook::from_sheet(sheet, None);
    for r in 1_u32..=100_000 {
        let row = RowId(u64::from(r));
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Binary {
                op: BinaryOp::Add,
                left: Box::new(BoundExpr::Call {
                    name: String::from("ROW"),
                    args: Vec::new(),
                }),
                right: Box::new(cell_expr(row, ColId(1), false)),
            },
        };
        wb.insert_seeded_formula(
            SheetId(0),
            row,
            ColId(3),
            f,
            Value::Number(f64::from(r + 2)),
        );
    }
    let mut eng = FormulaEngine::from_workbook_clean(wb);
    eng.edit_cell_raw(SheetId(0), RowId(0), ColId(4), "=SUM(B2:B100001)")
        .unwrap();
    eng.recalc_all();

    let mut ins_times = Vec::with_capacity(samples);
    let mut edit_times = Vec::with_capacity(samples);
    for i in 0..(warmup + samples) {
        let run = eng.allocator_mut().alloc_run(1).unwrap();
        let t0 = Instant::now();
        eng.apply_op(Op::InsertRows {
            sheet: SheetId(0),
            after: None,
            ids: run,
        })
        .unwrap();
        let n = eng.recalc_all();
        let ins_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(n, 100_000);

        let val = 5.0 + (i as f64);
        let t1 = Instant::now();
        eng.edit_cell_raw(SheetId(0), RowId(50), ColId(1), &format!("{val}"))
            .unwrap();
        eng.recalc_all();
        let edit_ms = t1.elapsed().as_secs_f64() * 1000.0;

        if i >= warmup {
            ins_times.push(ins_ms);
            edit_times.push(edit_ms);
        }
    }
    (
        DistributionMs::from_samples(warmup, ins_times),
        DistributionMs::from_samples(warmup, edit_times),
    )
}

fn build_dense_grid(rows: u32, cols: u32, formula_rows: u32) -> FormulaEngine {
    let blocks_per_col = (rows as usize).div_ceil(BLOCK_ROWS as usize);
    let param_col = if cols >= 26 { 25_u32 } else { cols - 2 };
    let formula_col = cols - 1;

    let mut columns = Vec::with_capacity(cols as usize);
    for c in 0..cols {
        let mut col_blocks = Vec::with_capacity(blocks_per_col);
        for b in 0..blocks_per_col {
            let base_r = (b as u32) * BLOCK_ROWS;
            let count = (rows - base_r).min(BLOCK_ROWS) as usize;
            let mut words = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
            for (off, word) in words.iter_mut().enumerate().take(count) {
                let r = base_r + (off as u32);
                let num = if c == param_col && r == 0 {
                    1.0
                } else if c == formula_col && r < formula_rows {
                    let bv = f64::from(((r.wrapping_mul(31) ^ 17) % 1000) + 1) * 0.25;
                    let cv = f64::from(((r.wrapping_mul(31) ^ 34) % 1000) + 1) * 0.25;
                    bv * cv + 1.0
                } else {
                    f64::from(((r.wrapping_mul(31) ^ c.wrapping_mul(17)) % 1000) + 1) * 0.25
                };
                *word = Value::Number(num).pack();
            }
            col_blocks.push(Block::dense(words).unwrap());
        }
        columns.push(Column::from_blocks(col_blocks, rows));
    }

    let sheet = Sheet::from_parts(
        SheetId(0),
        String::from("Sheet1"),
        vec![AxisRun::live(IdRun::new(0, rows).unwrap())],
        vec![AxisRun::live(IdRun::new(0, cols).unwrap())],
        SlotMap::with_base(rows),
        SlotMap::with_base(cols),
        columns,
        vec![NumberFormat::General; cols as usize],
        BTreeMap::new(),
        StringTable::new(),
    )
    .expect("valid dense benchmark sheet");

    let mut wb = Workbook::from_sheet(sheet, None);
    for r in 0..formula_rows {
        let row = RowId(u64::from(r));
        let bv = f64::from(((r.wrapping_mul(31) ^ 17) % 1000) + 1) * 0.25;
        let cv = f64::from(((r.wrapping_mul(31) ^ 34) % 1000) + 1) * 0.25;
        let bc = BoundExpr::Binary {
            op: BinaryOp::Mul,
            left: Box::new(cell_expr(row, ColId(1), false)),
            right: Box::new(cell_expr(row, ColId(2), false)),
        };
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Binary {
                op: BinaryOp::Add,
                left: Box::new(bc),
                right: Box::new(cell_expr(RowId(0), ColId(u64::from(param_col)), true)),
            },
        };
        wb.insert_seeded_formula(
            SheetId(0),
            row,
            ColId(u64::from(formula_col)),
            f,
            Value::Number(bv * cv + 1.0),
        );
    }
    FormulaEngine::from_workbook_clean(wb)
}

fn build_string_grid(rows: u32, distinct: u32) -> FormulaEngine {
    let mut strings = StringTable::new();
    let distinct = distinct.max(1);
    let mut ids = Vec::with_capacity(distinct as usize);
    for i in 0..distinct {
        let s = format!("S{i:07}");
        ids.push(strings.intern(&s).expect("intern benchmark string"));
    }

    let blocks_per_col = (rows as usize).div_ceil(BLOCK_ROWS as usize);
    let mut col_blocks = Vec::with_capacity(blocks_per_col);
    for b in 0..blocks_per_col {
        let base_r = (b as u32) * BLOCK_ROWS;
        let count = (rows - base_r).min(BLOCK_ROWS) as usize;
        let mut words = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
        for (off, word) in words.iter_mut().enumerate().take(count) {
            let r = base_r + (off as u32);
            let sid = ids[(r % distinct) as usize];
            *word = Value::Text(sid).pack();
        }
        col_blocks.push(Block::dense(words).unwrap());
    }
    let columns = vec![Column::from_blocks(col_blocks, rows)];
    let sheet = Sheet::from_parts(
        SheetId(0),
        String::from("Sheet1"),
        vec![AxisRun::live(IdRun::new(0, rows).unwrap())],
        vec![AxisRun::live(IdRun::new(0, 1).unwrap())],
        SlotMap::with_base(rows),
        SlotMap::with_base(1),
        columns,
        vec![NumberFormat::General],
        BTreeMap::new(),
        strings,
    )
    .expect("valid string benchmark sheet");

    FormulaEngine::from_workbook_clean(Workbook::from_sheet(sheet, None))
}

fn build_1m_row_local_engine() -> FormulaEngine {
    let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
    sheet.set_value_by_id(RowId(0), ColId(25), Value::Number(1.0));
    for r in 0_u32..1_000_000 {
        let row = RowId(u64::from(r));
        sheet.set_value_by_id(row, ColId(1), Value::Number(2.0));
        sheet.set_value_by_id(row, ColId(2), Value::Number(3.0));
    }
    let mut wb = Workbook::from_sheet(sheet, None);
    for r in 0_u32..1_000_000 {
        let row = RowId(u64::from(r));
        let bc = BoundExpr::Binary {
            op: BinaryOp::Mul,
            left: Box::new(cell_expr(row, ColId(1), false)),
            right: Box::new(cell_expr(row, ColId(2), false)),
        };
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Binary {
                op: BinaryOp::Add,
                left: Box::new(bc),
                right: Box::new(cell_expr(RowId(0), ColId(25), true)),
            },
        };
        wb.insert_seeded_formula(SheetId(0), row, ColId(3), f, Value::Number(7.0));
    }
    FormulaEngine::from_workbook_clean(wb)
}

fn cell_expr(row: RowId, col: ColId, abs: bool) -> BoundExpr {
    BoundExpr::Reference(BoundRef::Cell {
        sheet: None,
        missing_sheet: None,
        row,
        col,
        abs_col: abs,
        abs_row: abs,
    })
}
