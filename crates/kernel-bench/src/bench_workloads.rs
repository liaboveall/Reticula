//! Formal M1a engine benchmark workloads B2, B3, B4, B5, B7a (3 formula groups)
//! and B7b (3 formula groups) (`04-engine.md` & `13-m1a.md` 7.4).

use std::time::Instant;

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{FormulaMode, Op, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
use reticula_formula::ast::{BinaryOp, Literal};
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};
use reticula_formula::engine::FormulaEngine;
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

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Complete native benchmark report for B2–B5 and B7a/B7b.
#[derive(Clone, Debug, Serialize)]
pub struct NativeBenchSuite {
    /// B2: 1,000,000-row `=SUM(A1:A1000000)` evaluation.
    pub b2_sum_1m: DistributionMs,
    /// B3: 1,000,000 formulas `=B2*C2+$Z$1` full recalculation.
    pub b3_full_recalc_1m: DistributionMs,
    /// B4: 100,000 formulas `=A2*$Z$1`, edit `Z1` incremental recalc (incl. dirty marking).
    pub b4_incremental_100k: DistributionMs,
    /// B5: 1,000,000-row chain `=C1+B2`, edit first row (non-recursive stack).
    pub b5_chain_1m: DistributionMs,
    /// B7a Group 1 (row-local `=B2*C2+$Z$1`, 1M rows/formulas): insert 1 row at top.
    pub b7a_insert_top_row_local: DistributionMs,
    /// B7b Group 1: first range-referenced cell edit immediately after B7a Group 1.
    pub b7b_edit_after_insert_row_local: DistributionMs,
    /// B7a Group 2 (cross-insertion-point range `=SUM(A1:A1000000)` + 999,999 formulas): insert 1 row inside range.
    pub b7a_insert_cross_range: DistributionMs,
    /// B7b Group 2: edit a range-referenced cell immediately after B7a Group 2.
    pub b7b_edit_after_insert_cross_range: DistributionMs,
    /// B7a Group 3 (100,000 `ROW()` structure-sensitive formulas on 1M-row sheet): insert 1 row at top (axis + dirty + recalc).
    pub b7a_insert_structural_row_fn: DistributionMs,
    /// B7b Group 3: edit a range-referenced cell immediately after B7a Group 3.
    pub b7b_edit_after_insert_structural: DistributionMs,
}

/// Runs the complete native benchmark suite and returns percentile distributions.
#[must_use]
pub fn run_native_suite(quick: bool) -> NativeBenchSuite {
    let (w_fast, n_fast) = if quick { (2, 5) } else { (3, 15) };
    let (w_heavy, n_heavy) = if quick { (1, 3) } else { (2, 7) };

    NativeBenchSuite {
        b2_sum_1m: bench_b2(w_fast, n_fast),
        b3_full_recalc_1m: bench_b3(w_heavy, n_heavy),
        b4_incremental_100k: bench_b4(w_fast, n_fast),
        b5_chain_1m: bench_b5(w_heavy, n_heavy),
        b7a_insert_top_row_local: bench_b7_row_local(w_fast, n_fast).0,
        b7b_edit_after_insert_row_local: bench_b7_row_local(w_fast, n_fast).1,
        b7a_insert_cross_range: bench_b7_cross_range(w_fast, n_fast).0,
        b7b_edit_after_insert_cross_range: bench_b7_cross_range(w_fast, n_fast).1,
        b7a_insert_structural_row_fn: bench_b7_structural(w_fast, n_fast).0,
        b7b_edit_after_insert_structural: bench_b7_structural(w_fast, n_fast).1,
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
    // Add one range formula `=SUM(B2:B100001)` in `E1` for B7b's post-insert range edit!
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
            after: Some(RowId(0)), // strictly inside B1:B1000000!
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
    let _ = Literal::Number(0.0);
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
