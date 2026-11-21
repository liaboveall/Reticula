//! Differential testing (`diff`), fuzzing (`fuzz`) and archived engine
//! benchmarks (`bench`) (`13-m1a.md` 7.2, 7.3 & 7.4).

use std::fs;
use std::process::Command;

use crate::util::{self, Cmd, Flags, Result, cargo, node, pnpm, tool};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

pub fn diff(flags: &Flags) -> Result {
    let count = flags.get("count").unwrap_or("10000");
    let out = flags
        .get("out")
        .map_or_else(|| "target/qa/diff-report.json".to_owned(), str::to_owned);

    util::step(&format!(
        "differential evaluation ({count} formulas vs ironcalc_base & formualizer-eval)"
    ));
    cargo()
        .args([
            "run",
            "--locked",
            "--release",
            "--package",
            "reticula-kernel-bench",
            "--bin",
            "runner",
            "--",
            "diff",
            "--count",
            count,
            "--out",
            &out,
        ])
        .run()
}

pub fn fuzz(flags: &Flags) -> Result {
    let cpu_seconds = flags.get("cpu-seconds").unwrap_or("3600");
    let threads = flags.get("threads").unwrap_or("16");
    let out = flags
        .get("out")
        .map_or_else(|| "target/qa/fuzz-report.json".to_owned(), str::to_owned);

    util::step(&format!(
        "fuzzing ({cpu_seconds} CPU-seconds across {threads} threads)"
    ));
    let mut cmd = cargo().args([
        "run",
        "--locked",
        "--release",
        "--package",
        "reticula-kernel-bench",
        "--bin",
        "runner",
        "--",
        "fuzz",
        "--cpu-seconds",
        cpu_seconds,
        "--threads",
        threads,
        "--out",
        &out,
    ]);
    if let Some(mask) = flags.get("affinity") {
        cmd = cmd.args(["--affinity", mask]);
    }
    if let Some(target) = flags.get("target") {
        cmd = cmd.args(["--target", target]);
    }
    cmd.run()
}

pub fn sim(flags: &Flags) -> Result {
    let mut pass_args: Vec<String> = Vec::new();
    if let Some(seed) = flags.get("seed") {
        util::step(&format!(
            "deterministic sync simulation (single seed {seed})"
        ));
        pass_args.push("--seed".to_owned());
        pass_args.push(seed.to_owned());
    } else {
        let seeds = flags.get("seeds").unwrap_or("5000");
        util::step(&format!("deterministic sync simulation ({seeds} seeds)"));
        pass_args.push("--seeds".to_owned());
        pass_args.push(seeds.to_owned());
    }
    if let Some(start) = flags.get("start-seed") {
        pass_args.push("--start-seed".to_owned());
        pass_args.push(start.to_owned());
    }
    if let Some(threads) = flags.get("threads") {
        pass_args.push("--threads".to_owned());
        pass_args.push(threads.to_owned());
    }
    if flags.has("fail-fast") {
        pass_args.push("--fail-fast".to_owned());
    }
    if let Some(out) = flags.get("out") {
        pass_args.push("--out".to_owned());
        pass_args.push(out.to_owned());
    }

    let mut cmd = cargo().args([
        "run",
        "--locked",
        "--release",
        "--package",
        "reticula-sim",
        "--bin",
        "sim",
        "--",
    ]);
    for arg in &pass_args {
        cmd = cmd.arg(arg);
    }
    cmd.run()
}

pub fn bench(flags: &Flags) -> Result {
    let affinity = flags.get("affinity").unwrap_or("FFFF");
    let browser = flags.get("browser").unwrap_or("chrome");
    // `engine` is B1-B8, B11 and B12; `b9` is the collaborative slow path.
    let suite = flags.get("suite").unwrap_or("engine");
    let (native_command, suite_label, archive_kind) = match suite {
        "engine" => ("native-bench", "m1b-engine", "engine"),
        "b9" => ("sync-bench", "m2a-sync", "sync"),
        other => return Err(format!("unknown benchmark suite `{other}` (engine or b9)")),
    };

    util::step("native benchmarks (release)");
    let native_out = util::root().join("target/bench-wasm/native.json");
    if let Some(parent) = native_out.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut native_cmd = cargo().args([
        "run",
        "--locked",
        "--release",
        "--package",
        "reticula-kernel-bench",
        "--bin",
        "runner",
        "--",
        native_command,
        "--affinity",
        affinity,
        "--out",
    ]);
    native_cmd = native_cmd.arg(&native_out);
    if flags.has("quick") {
        native_cmd = native_cmd.arg("--quick");
    }
    native_cmd.run()?;

    util::step("building WASM benchmark module (release + wasm-opt)");
    cargo()
        .args([
            "build",
            "--locked",
            "--release",
            "--package",
            "reticula-wasm",
            "--features",
            "bench",
            "--target",
            WASM_TARGET,
        ])
        .run()?;
    let pkg_dir = util::root().join("target/bench-wasm/pkg");
    Cmd::new(tool("wasm-bindgen"))
        .args(["--target", "web", "--out-dir"])
        .arg(&pkg_dir)
        .arg(format!("target/{WASM_TARGET}/release/reticula_wasm.wasm"))
        .run()?;
    let wasm_file = pkg_dir.join("reticula_wasm_bg.wasm");
    pnpm()
        .args(["exec", "wasm-opt", "-O3"])
        .arg(&wasm_file)
        .arg("--output")
        .arg(&wasm_file)
        .run()?;

    util::step(&format!(
        "WASM benchmarks in {browser} (affinity {affinity})"
    ));
    let wasm_out = util::root().join("target/bench-wasm/wasm.json");
    let mut wasm_cmd = node();
    wasm_cmd = wasm_cmd
        .arg("bench/run-wasm-bench.mjs")
        .arg("--pkg")
        .arg(&pkg_dir)
        .args([
            "--browser",
            browser,
            "--affinity",
            affinity,
            "--suite",
            suite,
        ])
        .arg("--out")
        .arg(&wasm_out);
    if flags.has("quick") {
        wasm_cmd = wasm_cmd.arg("--quick");
    }
    wasm_cmd.run()?;

    // Combine native + WASM results into `bench/results/<date>-<commit>.json`.
    let native_val: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&native_out).map_err(|e| format!("read native.json: {e}"))?,
    )
    .map_err(|e| format!("parse native.json: {e}"))?;
    let wasm_val: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&wasm_out).map_err(|e| format!("read wasm.json: {e}"))?,
    )
    .map_err(|e| format!("parse wasm.json: {e}"))?;

    let commit = git_output(&["rev-parse", "--short", "HEAD"]);
    let dirty = !git_output(&["status", "--porcelain"]).is_empty();
    let today = flags
        .get("date")
        .map_or_else(current_utc_date, str::to_owned);

    let archive_path = match flags.get("out") {
        Some(custom) => util::root().join(custom),
        None => util::root()
            .join("bench/results")
            .join(format!("{today}-{commit}-{archive_kind}.json")),
    };
    if let Some(parent) = archive_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }

    let combined = serde_json::json!({
        "suite": suite_label,
        "date": today,
        "commit": commit,
        "uncommittedChanges": dirty,
        "affinity": affinity,
        "native": native_val,
        "wasmChrome": wasm_val,
    });
    fs::write(
        &archive_path,
        format!("{}\n", serde_json::to_string_pretty(&combined).unwrap()),
    )
    .map_err(|e| format!("write {}: {e}", archive_path.display()))?;
    println!("archived benchmark report to {}", archive_path.display());
    Ok(())
}

fn git_output(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(util::root())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn current_utc_date() -> String {
    // Query UTC date via Node (`new Date().toISOString().slice(0, 10)`) without extra crates.
    Command::new("node")
        .args([
            "-e",
            "process.stdout.write(new Date().toISOString().slice(0,10))",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "2026-11-18".to_owned())
}
