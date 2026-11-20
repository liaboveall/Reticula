//! Deterministic discrete-event sync simulator (`crates/sim`, `15-m2a.md` §6).
//!
//! Uses only [`reticula_core::rng::Rng`] (never `rand` or `HashMap`/`HashSet`)
//! so any failing `u64` seed reproduces identically across hosts and minimizes
//! via delta-debugging (`minimize_failure`).

mod workload;
mod world;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

pub use crate::workload::{OpChoice, RogueKind, SimEvent, generate_script};
use crate::world::SimWorld;
pub use crate::world::{FaultCounts, SeedSummary};

/// Minimal failing counterexample produced by [`minimize_failure`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SimFailure {
    /// Original `u64` seed that triggered the invariant violation.
    pub seed: u64,
    /// Invariant violation message from the unminimized run.
    pub error: String,
    /// Number of events in the original generated script.
    pub original_len: usize,
    /// Debug-formatted minimal failing event subset after delta-debugging.
    pub minimized_events: Vec<String>,
    /// Violation observed when re-running `minimized_events`.
    pub minimized_error: String,
}

/// Aggregated multi-seed campaign summary.
#[derive(Clone, Debug, Serialize)]
pub struct SimReport {
    /// Starting seed index (`start_seed`).
    pub start_seed: u64,
    /// Total seeds evaluated.
    pub seeds_run: u64,
    /// Total failing seeds encountered.
    pub failures_count: u64,
    /// Worker threads used.
    pub threads: usize,
    /// Elapsed wall-clock seconds.
    pub wall_seconds: f64,
    /// Aggregate fault and operation counts across all passed seeds.
    pub faults: FaultCounts,
    /// Minimized failure reports (bounded in multi-seed runs).
    pub failures: Vec<SimFailure>,
}

/// Runs one deterministic seed (`generate_script(seed)` + quiescence drain + Invariants 1–10).
pub fn run_seed(seed: u64) -> Result<SeedSummary, String> {
    let script = generate_script(seed);
    let mut summary = run_event_script(seed, &script)?;
    summary.seed = seed;
    Ok(summary)
}

/// Runs an explicit event slice deterministically starting from an empty world seeded by `seed`.
pub fn run_event_script(seed: u64, events: &[SimEvent]) -> Result<SeedSummary, String> {
    let mut world = SimWorld::new(seed);
    for (idx, &event) in events.iter().enumerate() {
        world
            .step(event)
            .map_err(|err| format!("step {idx} ({event:?}): {err}"))?;
    }
    let mut summary = world.drain_and_verify()?;
    summary.seed = seed;
    Ok(summary)
}

/// Delta-debugging reducer finding a minimal failing event subsequence for `seed`.
#[must_use]
pub fn minimize_failure(seed: u64, original_error: &str) -> SimFailure {
    let original = generate_script(seed);
    let original_len = original.len();
    let (best, best_error) = ddmin_with(&original, original_error, |candidate| {
        run_event_script(seed, candidate).err()
    });

    SimFailure {
        seed,
        error: original_error.to_owned(),
        original_len,
        minimized_events: best.iter().map(|e| format!("{e:?}")).collect(),
        minimized_error: best_error,
    }
}

fn ddmin_with<T: Clone>(
    items: &[T],
    initial_error: &str,
    mut fails: impl FnMut(&[T]) -> Option<String>,
) -> (Vec<T>, String) {
    let mut best = items.to_vec();
    let mut best_error = initial_error.to_owned();

    let mut granularity = 2_usize;
    while best.len() > 1 && granularity <= best.len() {
        let chunk_size = best.len().div_ceil(granularity).max(1);
        let mut reduced = false;
        let mut start = 0_usize;

        while start < best.len() {
            let end = (start + chunk_size).min(best.len());
            let mut candidate = Vec::with_capacity(best.len() - (end - start));
            candidate.extend_from_slice(&best[..start]);
            candidate.extend_from_slice(&best[end..]);
            if let Some(err) = fails(&candidate) {
                best = candidate;
                best_error = err;
                reduced = true;
                break;
            }
            start = end;
        }

        if reduced {
            granularity = (granularity.saturating_sub(1)).max(2);
        } else if granularity == best.len() {
            break;
        } else {
            granularity = (granularity.saturating_mul(2)).min(best.len());
        }
    }

    (best, best_error)
}

/// Executes `count` consecutive seeds starting at `start_seed` across `threads` workers.
#[must_use]
pub fn run_seeds_range(start_seed: u64, count: u64, threads: usize, fail_fast: bool) -> SimReport {
    let worker_count = threads.max(1);
    let next_offset = AtomicU64::new(0);
    let passed = AtomicU64::new(0);
    let abort_flag = AtomicBool::new(false);
    let shared_faults = Mutex::new(FaultCounts::default());
    let shared_failures: Mutex<Vec<SimFailure>> = Mutex::new(Vec::new());

    let started = Instant::now();
    std::thread::scope(|scope| {
        for _tid in 0..worker_count {
            let next_ref = &next_offset;
            let pass_ref = &passed;
            let abort_ref = &abort_flag;
            let faults_ref = &shared_faults;
            let failures_ref = &shared_failures;

            scope.spawn(move || {
                let mut local_faults = FaultCounts::default();
                let mut local_passed = 0_u64;

                loop {
                    if fail_fast && abort_ref.load(Ordering::Relaxed) {
                        break;
                    }
                    let batch_start = next_ref.fetch_add(64, Ordering::Relaxed);
                    if batch_start >= count {
                        break;
                    }
                    let batch_end = batch_start.saturating_add(64).min(count);
                    for off in batch_start..batch_end {
                        if fail_fast && abort_ref.load(Ordering::Relaxed) {
                            break;
                        }
                        let seed = start_seed.wrapping_add(off);
                        match run_seed(seed) {
                            Ok(summary) => {
                                local_faults.merge(&summary.faults);
                                local_passed += 1;
                            }
                            Err(err) => {
                                let minimized = minimize_failure(seed, &err);
                                if let Ok(mut guard) = failures_ref.lock()
                                    && guard.len() < 16
                                {
                                    guard.push(minimized);
                                }
                                if fail_fast {
                                    abort_ref.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }
                    }
                }

                pass_ref.fetch_add(local_passed, Ordering::Relaxed);
                if let Ok(mut guard) = faults_ref.lock() {
                    guard.merge(&local_faults);
                }
            });
        }
    });

    let wall_seconds = started.elapsed().as_secs_f64();
    let failures = shared_failures.into_inner().unwrap_or_default();
    let faults = shared_faults.into_inner().unwrap_or_default();
    let failures_count = failures.len() as u64;
    let passed_count = passed.load(Ordering::Relaxed);

    SimReport {
        start_seed,
        seeds_run: passed_count.saturating_add(failures_count),
        failures_count,
        threads: worker_count,
        wall_seconds,
        faults,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::{ddmin_with, generate_script, run_event_script, run_seed, run_seeds_range};

    #[test]
    fn identical_seed_reproduces_bit_identical_digest_and_fault_counts() {
        let a = run_seed(42).expect("seed 42 passes");
        let b = run_seed(42).expect("seed 42 rerun passes");
        assert_eq!(a, b);
    }

    #[test]
    fn smoke_200_seeds_cover_all_10_fault_categories_with_zero_failures() {
        let report = run_seeds_range(0, 200, 4, true);
        assert_eq!(
            report.failures_count, 0,
            "unexpected failures: {:?}",
            report.failures
        );
        assert_eq!(report.seeds_run, 200);
        let f = &report.faults;
        assert!(f.local_edits > 0, "expected local edits");
        assert!(f.set_cells_if_ops > 0, "expected SetCellsIf ops");
        assert!(f.formula_ops > 0, "expected bound formula ops");
        assert!(f.move_row_col_ops > 0, "expected MoveRows/MoveCols ops");
        assert!(f.structural_sheet_ops > 0, "expected sheet structural ops");
        assert!(f.reordered_deliveries > 0, "expected async deliveries");
        assert!(
            f.slow_receiver_disconnects > 0,
            "expected slow-receiver disconnects"
        );
        assert!(
            f.snapshot_required_catchups > 0,
            "expected SnapshotRequired catchups"
        );
        assert!(f.client_crashes > 0, "expected client crash restarts");
        assert!(f.opfs_tail_losses > 0, "expected OPFS tail-loss events");
        assert!(f.server_crashes > 0, "expected server crash restarts");
        assert!(
            f.epoch_backup_restores > 0,
            "expected epoch backup restores"
        );
        assert!(f.terminal_rejects > 0, "expected terminal Reject handling");
        assert!(f.duplicate_acks > 0, "expected idempotent DuplicateAcks");
        assert!(f.rogue_attempts_rejected > 0, "expected rogue rejections");
        assert!(
            f.two_tab_same_user_commits > 0,
            "expected two-tab USER_A commits"
        );
    }

    #[test]
    fn reducer_preserves_determinism_on_passing_seed() {
        let script = generate_script(7);
        let res = run_event_script(7, &script).unwrap();
        assert_eq!(res.seed, 7);

        let items: Vec<usize> = (0..20).collect();
        let (reduced, msg) = ddmin_with(&items, "both 5 and 17 present", |sub| {
            (sub.contains(&5) && sub.contains(&17)).then(|| String::from("minimal failure"))
        });
        assert_eq!(reduced, vec![5, 17]);
        assert_eq!(msg, "minimal failure");
    }
}
