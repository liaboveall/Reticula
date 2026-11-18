//! Native CLI runner for `cargo xtask diff`, `cargo xtask fuzz` and
//! `cargo xtask bench` (`13-m1a.md` 7.2, 7.3 & 7.4).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use reticula_kernel_bench::bench_workloads::run_native_suite;
use reticula_kernel_bench::diff::run_differential;
use reticula_kernel_bench::fuzz_runner::run_fuzz_campaign;

fn pin_current_process(mask: &str) {
    assert!(
        !mask.is_empty() && mask.chars().all(|c| c.is_ascii_hexdigit()),
        "invalid hex affinity mask `{mask}`"
    );
    let pid = std::process::id();
    let status = if cfg!(windows) {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).ProcessorAffinity = 0x{mask}"),
            ])
            .status()
    } else {
        Command::new("taskset")
            .args(["--all-tasks", "--pid", mask, &pid.to_string()])
            .status()
    };
    let ok = status.map(|s| s.success()).unwrap_or(false);
    assert!(ok, "failed to pin process {pid} to affinity mask 0x{mask}");
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("usage: runner <diff|fuzz|native-bench> [options]");
        std::process::exit(2);
    };

    match sub.as_str() {
        "diff" => run_diff_cmd(rest),
        "fuzz" => run_fuzz_cmd(rest),
        "native-bench" => run_bench_cmd(rest),
        other => {
            eprintln!("unknown subcommand `{other}`");
            std::process::exit(2);
        }
    }
}

fn run_diff_cmd(args: &[String]) {
    let mut count = 2_500_usize;
    let mut seed = 0xD1FF_2026_u64;
    let mut out: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--count" => {
                i += 1;
                count = args[i].parse().expect("valid --count");
            }
            "--seed" => {
                i += 1;
                seed = args[i].parse().expect("valid --seed");
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown diff flag `{flag}`"),
        }
        i += 1;
    }

    let report = run_differential(seed, count);
    println!(
        "Differential report (seed={:#x}, total={} formulas, non-error={:.1}%):",
        report.seed,
        report.total_formulas,
        report.non_error_ratio * 100.0
    );
    println!(
        "  ironcalc_base    exact/15-digit matches: {} / {}",
        report.ironcalc_matches, report.total_formulas
    );
    println!(
        "  formualizer-eval exact/15-digit matches: {} / {}",
        report.formualizer_matches, report.total_formulas
    );
    for c in &report.clusters {
        println!(
            "  cluster [{}] {} ({}) x{}: `{}` -> reticula={:?}, ref={:?}",
            c.reference_engine,
            c.category,
            c.disposition,
            c.count,
            c.sample_formula,
            c.reticula_result,
            c.reference_result
        );
    }

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
        )
        .expect("write diff report");
    }

    if report.unclassified_count() > 0 {
        eprintln!(
            "error: {} unclassified differential divergences!",
            report.unclassified_count()
        );
        std::process::exit(1);
    }
}

fn run_fuzz_cmd(args: &[String]) {
    let mut cpu_seconds = 3_600_u64;
    let mut threads = 16_usize;
    let mut affinity: Option<String> = None;
    let mut out: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cpu-seconds" => {
                i += 1;
                cpu_seconds = args[i].parse().expect("valid --cpu-seconds");
            }
            "--threads" => {
                i += 1;
                threads = args[i].parse().expect("valid --threads");
            }
            "--affinity" => {
                i += 1;
                affinity = Some(args[i].clone());
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown fuzz flag `{flag}`"),
        }
        i += 1;
    }

    if let Some(mask) = &affinity {
        pin_current_process(mask);
    }

    let report = run_fuzz_campaign(cpu_seconds, threads);
    println!(
        "Fuzzing complete: {:.1} wall s x {} threads = {:.1} CPU-s ({:.2} CPU-hours)",
        report.wall_seconds,
        report.threads,
        report.cpu_seconds,
        report.cpu_seconds / 3600.0
    );
    println!(
        "  executions: parser={}, snapshot={}, ops={}, eval={} (total={})",
        report.parser_execs,
        report.snapshot_execs,
        report.ops_execs,
        report.eval_execs,
        report.parser_execs + report.snapshot_execs + report.ops_execs + report.eval_execs
    );

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&report).unwrap()),
        )
        .expect("write fuzz report");
    }
}

fn run_bench_cmd(args: &[String]) {
    let mut quick = false;
    let mut affinity: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quick" => quick = true,
            "--affinity" => {
                i += 1;
                affinity = Some(args[i].clone());
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            flag => panic!("unknown native-bench flag `{flag}`"),
        }
        i += 1;
    }

    if let Some(mask) = &affinity {
        pin_current_process(mask);
    }

    let suite = run_native_suite(quick);
    let json = serde_json::to_string_pretty(&suite).unwrap();
    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, format!("{json}\n")).expect("write native bench");
    }
    println!("{json}");
}
