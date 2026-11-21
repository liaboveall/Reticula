//! Native run of benchmark B9: the slow path of collaborative sync when a
//! remote commit arrives while local edits are pending (`15-m2a.md` 8.2).
//! The workload lives in `reticula_wasm::bench_sync`, which the Chrome WASM
//! page steps the same way.

use std::time::Instant;

use reticula_wasm::bench_sync::{B9Case, SyncBench};
use serde::Serialize;

use crate::bench_workloads::DistributionMs;

/// One B9 fixture: what was pending and what a remote commit cost.
#[derive(Clone, Debug, Serialize)]
pub struct B9CaseReport {
    /// Pending local transactions when each remote commit arrives.
    pub pending_txns: usize,
    /// Cells those transactions write.
    pub pending_cells: usize,
    /// `ClientSyncMachine::on_commit` alone: rewind, apply, replay and the
    /// incremental state digest.
    pub machine_ms: DistributionMs,
    /// The worker engine's handling of the incoming frame, which includes the
    /// sync machine and the rebuild of the formula engine.
    pub engine_frame_ms: DistributionMs,
    /// From the incoming frame to the last recalculation slice.
    pub engine_total_ms: DistributionMs,
    /// Mean number of recalculation slices after each commit.
    pub recalc_slices_mean: f64,
}

/// Where the time of a reconciled commit goes, measured on the document
/// with nothing pending.
#[derive(Clone, Debug, Serialize)]
pub struct B9Breakdown {
    /// One clone of the visible workbook, which the slow path makes of the
    /// confirmed state on every remote commit.
    pub workbook_clone_ms: DistributionMs,
    /// Rebuilding the formula engine from the reconciled workbook.
    pub engine_rebuild_ms: DistributionMs,
    /// Recalculating every formula afterwards, in time slices.
    pub full_recalc_ms: DistributionMs,
}

/// Benchmark B9 on the 100,000-row sample document.
#[derive(Clone, Debug, Serialize)]
pub struct B9Report {
    /// The document every fixture starts from.
    pub document: &'static str,
    /// B9a: 1,000 single-cell edits pending.
    pub b9a_1000_single_cells: B9CaseReport,
    /// B9b: one 100,000-cell edit and 9 single-cell edits pending.
    pub b9b_100k_cell_block: B9CaseReport,
    /// Control: nothing pending, so the commit is applied in place.
    pub control_nothing_pending: B9CaseReport,
    /// Where the time goes.
    pub breakdown: B9Breakdown,
}

/// Warmup and measured rounds of each fixture.
#[must_use]
pub const fn rounds(quick: bool) -> (usize, usize) {
    if quick { (1, 5) } else { (3, 50) }
}

/// Runs B9 natively.
#[must_use]
pub fn run_b9(quick: bool) -> B9Report {
    let (warmup, samples) = rounds(quick);
    B9Report {
        document: "sample, 100,000 data rows x 22 columns, 100,000 formulas",
        b9a_1000_single_cells: run_case(B9Case::ThousandSingleCells, warmup, samples),
        b9b_100k_cell_block: run_case(B9Case::BlockOf100kCells, warmup, samples),
        control_nothing_pending: run_case(B9Case::NothingPending, warmup, samples),
        breakdown: run_breakdown(warmup, samples),
    }
}

fn run_breakdown(warmup: usize, samples: usize) -> B9Breakdown {
    let mut bench = SyncBench::new(B9Case::NothingPending);
    let mut clone = Vec::with_capacity(samples);
    for i in 0..warmup + samples {
        let started = Instant::now();
        bench.step_clone();
        if i >= warmup {
            clone.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    let mut rebuild = Vec::with_capacity(samples);
    let mut recalc = Vec::with_capacity(samples);
    for i in 0..warmup + samples {
        let rebuild_ms = bench.step_rebuild();
        let started = Instant::now();
        bench.finish_engine();
        let recalc_ms = started.elapsed().as_secs_f64() * 1_000.0;
        if i >= warmup {
            rebuild.push(rebuild_ms);
            recalc.push(recalc_ms);
        }
    }
    B9Breakdown {
        workbook_clone_ms: DistributionMs::from_samples(warmup, clone),
        engine_rebuild_ms: DistributionMs::from_samples(warmup, rebuild),
        full_recalc_ms: DistributionMs::from_samples(warmup, recalc),
    }
}

fn run_case(case: B9Case, warmup: usize, samples: usize) -> B9CaseReport {
    let mut bench = SyncBench::new(case);
    let mut machine = Vec::with_capacity(samples);
    for i in 0..warmup + samples {
        let started = Instant::now();
        bench.step_machine();
        if i >= warmup {
            machine.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    let mut frame = Vec::with_capacity(samples);
    let mut total = Vec::with_capacity(samples);
    let mut slices = 0_u64;
    for i in 0..warmup + samples {
        let started = Instant::now();
        let frame_ms = bench.step_engine();
        let round_slices = bench.finish_engine();
        let total_ms = started.elapsed().as_secs_f64() * 1_000.0;
        if i >= warmup {
            frame.push(frame_ms);
            total.push(total_ms);
            slices += u64::from(round_slices);
        }
    }
    B9CaseReport {
        pending_txns: bench.pending(),
        pending_cells: bench.pending_cells(),
        machine_ms: DistributionMs::from_samples(warmup, machine),
        engine_frame_ms: DistributionMs::from_samples(warmup, frame),
        engine_total_ms: DistributionMs::from_samples(warmup, total),
        recalc_slices_mean: slices as f64 / samples.max(1) as f64,
    }
}
