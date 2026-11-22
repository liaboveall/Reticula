//! CLI entrypoint for deterministic sync simulation (`cargo xtask sim`).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use reticula_sim::{minimize_failure, run_seed, run_seeds_range};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut seeds: u64 = 5_000;
    let mut start_seed: u64 = 0;
    let mut single_seed: Option<u64> = None;
    let mut threads: usize = std::thread::available_parallelism().map_or(4, usize::from);
    let mut fail_fast = false;
    let mut out: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seeds" => {
                i += 1;
                seeds = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .expect("valid --seeds");
            }
            "--start-seed" => {
                i += 1;
                start_seed = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .expect("valid --start-seed");
            }
            "--seed" => {
                i += 1;
                single_seed = Some(
                    args.get(i)
                        .and_then(|s| s.parse().ok())
                        .expect("valid --seed"),
                );
            }
            "--threads" => {
                i += 1;
                threads = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .expect("valid --threads");
            }
            "--fail-fast" => {
                fail_fast = true;
            }
            "--out" => {
                i += 1;
                let path = args.get(i).expect("missing path for --out");
                out = Some(PathBuf::from(path));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: sim [--seeds <n>] [--start-seed <n>] [--seed <s_u64>] [--threads <t>] [--fail-fast] [--out <file>]"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown sim flag `{other}`");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    if let Some(seed) = single_seed {
        return match run_seed(seed) {
            Ok(summary) => {
                println!(
                    "sim seed={seed} PASSED: epoch={}, head_seq={}, digest=({:#018x},{:#018x},{:#018x}), logical={:#018x}",
                    summary.final_epoch,
                    summary.final_head_seq,
                    summary.final_digest_lo,
                    summary.final_digest_hi,
                    summary.final_digest_order,
                    summary.final_logical_digest
                );
                if let Some(path) = out {
                    write_json(&path, &summary);
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                let failure = minimize_failure(seed, &err);
                eprintln!(
                    "sim seed={seed} FAILED: {}\nMinimized {} -> {} events:\n{:#?}",
                    failure.error,
                    failure.original_len,
                    failure.minimized_events.len(),
                    failure.minimized_events
                );
                if let Some(path) = out {
                    write_json(&path, &failure);
                }
                ExitCode::FAILURE
            }
        };
    }

    let report = run_seeds_range(start_seed, seeds, threads, fail_fast);
    println!(
        "Simulation complete: {}/{} seeds passed ({} failed) in {:.2}s ({} threads)",
        report.seeds_run.saturating_sub(report.failures_count),
        seeds,
        report.failures_count,
        report.wall_seconds,
        report.threads
    );
    println!(
        "  faults: edits={}, if={}, formulas={}, moves={}, permutes={}, move_cells={}, names={}, e16_xform={}, sheets={}, reorder={}, slow_disc={}, snap_catchup={}, crashes={}, opfs_loss={}, srv_crash={}, epoch_restore={}, downgrades={}, rejects={}, dup_acks={}, rogue_rej={}, two_tab={}",
        report.faults.local_edits,
        report.faults.set_cells_if_ops,
        report.faults.formula_ops,
        report.faults.move_row_col_ops,
        report.faults.permute_content_ops,
        report.faults.move_cells_ops,
        report.faults.named_range_ops,
        report.faults.e16_transformed_edits,
        report.faults.structural_sheet_ops,
        report.faults.reordered_deliveries,
        report.faults.slow_receiver_disconnects,
        report.faults.snapshot_required_catchups,
        report.faults.client_crashes,
        report.faults.opfs_tail_losses,
        report.faults.server_crashes,
        report.faults.epoch_backup_restores,
        report.faults.role_downgrades,
        report.faults.terminal_rejects,
        report.faults.duplicate_acks,
        report.faults.rogue_attempts_rejected,
        report.faults.two_tab_same_user_commits
    );

    for f in &report.failures {
        eprintln!(
            "  FAILURE seed={}: {} (minimized {} -> {} events): {:?}",
            f.seed,
            f.error,
            f.original_len,
            f.minimized_events.len(),
            f.minimized_events
        );
    }

    if let Some(path) = out {
        write_json(&path, &report);
    }

    if report.failures_count > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn write_json<T: serde::Serialize>(path: &PathBuf, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = serde_json::to_string_pretty(value).expect("serialize sim report");
    fs::write(path, format!("{body}\n")).expect("write sim output file");
}
