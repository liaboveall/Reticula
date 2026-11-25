//! Differential testing (`diff`), fuzzing (`fuzz`) and archived benchmarks
//! (`bench`) (`13-m1a.md` 7.2, 7.3 & 7.4).

use std::fs;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use crate::util::{self, Cmd, Flags, Result, cargo, node, pnpm, tool};
use crate::wasm::{self, Profile};
use crate::web;

const WASM_TARGET: &str = "wasm32-unknown-unknown";

pub fn diff(flags: &Flags) -> Result {
    let engine = flags.get("engine").unwrap_or("rust");
    let count = flags.get("count").unwrap_or("10000");
    let quota_per_fn = flags.get("quota-per-fn").unwrap_or("250");
    let seed = flags.get("seed");
    let commit = git_output(&["rev-parse", "--short", "HEAD"]);
    let date = flags
        .get("date")
        .map_or_else(current_utc_date, str::to_owned);
    let default_out = match engine {
        "rust" => String::from("target/qa/diff-report.json"),
        "libreoffice" | "all" => String::from("target/qa/libreoffice-diff-report.json"),
        other => {
            return Err(format!(
                "unknown --engine `{other}` (expected rust, libreoffice, or all)"
            ));
        }
    };
    let out = if flags.has("archive") && flags.get("out").is_none() {
        format!("bench/results/{date}-{commit}-diff.json")
    } else {
        flags.get("out").map_or(default_out, str::to_owned)
    };

    if matches!(engine, "libreoffice" | "all") && !flags.has("skip-image") {
        let image = flags.get("image").unwrap_or("reticula-libreoffice:26.8.2");
        let inspect = Command::new("docker")
            .args(["image", "inspect", image])
            .output()
            .map_err(|e| format!("failed to run docker image inspect {image}: {e}"))?;
        if !inspect.status.success() {
            util::step(&format!("building LibreOffice container `{image}`"));
            Cmd::new("docker")
                .args(["build", "-t", image, "infra/libreoffice"])
                .run()?;
        }
    }

    util::step(&format!(
        "differential evaluation (engine={engine}, count={count}, quota-per-fn={quota_per_fn})"
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
        "diff",
        "--engine",
        engine,
        "--count",
        count,
        "--quota-per-fn",
        quota_per_fn,
        "--commit",
        &commit,
        "--date",
        &date,
        "--out",
        &out,
    ]);
    if let Some(s) = seed {
        cmd = cmd.args(["--seed", s]);
    }
    if let Some(image) = flags.get("image") {
        cmd = cmd.args(["--image", image]);
    }
    if flags.has("skip-image") {
        cmd = cmd.arg("--skip-image");
    }
    if let Some(dump) = flags.get("dump-xlsx") {
        cmd = cmd.args(["--dump-xlsx", dump]);
    }
    cmd.run()?;

    if flags.has("archive") && flags.get("out").is_some() {
        let archive_path = util::root()
            .join("bench/results")
            .join(format!("{date}-{commit}-diff.json"));
        if let Some(parent) = archive_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        fs::copy(util::root().join(&out), &archive_path)
            .map_err(|e| format!("archive diff report to {}: {e}", archive_path.display()))?;
        println!("archived differential report to {}", archive_path.display());
    }
    Ok(())
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

/// The switches and the options of `bench`.
pub const BENCH_SWITCHES: [&str; 7] = [
    "quick",
    "headless",
    "headed",
    "software",
    "no-archive",
    "skip-build",
    "skip-firefox",
];
pub const BENCH_OPTIONS: [&str; 14] = [
    "affinity",
    "browser",
    "date",
    "out",
    "suite",
    "slowdown",
    "dpr",
    "rounds",
    "seconds",
    "product-seconds",
    "only",
    "results",
    "baseline",
    "visits",
];

const RENDER_SWITCHES: [&str; 4] = ["quick", "headless", "software", "no-archive"];
const RENDER_OPTIONS: [&str; 8] = [
    "browser",
    "slowdown",
    "dpr",
    "rounds",
    "seconds",
    "product-seconds",
    "only",
    "results",
];
const GRID_SWITCHES: [&str; 3] = ["quick", "no-archive", "skip-firefox"];
const GRID_OPTIONS: [&str; 1] = ["results"];
const AB_SWITCHES: [&str; 3] = ["quick", "no-archive", "headed"];
const AB_OPTIONS: [&str; 5] = ["baseline", "rounds", "visits", "only", "results"];

/// A flag that the suite would not heed is a mistake: what is measured would not
/// be what was asked for.
fn check_flags(flags: &Flags, suite: &str) -> Result {
    let known: Vec<&str> = match suite {
        "engine" | "b9" => vec!["quick", "browser", "date", "out"],
        "render" => [&["skip-build"], &RENDER_SWITCHES[..], &RENDER_OPTIONS].concat(),
        "grid" => [&["skip-build"], &GRID_SWITCHES[..], &GRID_OPTIONS].concat(),
        "ab" => [&["skip-build"], &AB_SWITCHES[..], &AB_OPTIONS].concat(),
        "startup" => vec!["skip-build", "quick", "no-archive", "results"],
        other => {
            return Err(format!(
                "unknown benchmark suite `{other}` (engine, b9, render, grid, startup or ab)"
            ));
        }
    };
    let known = [&["suite", "affinity"], &known[..]].concat();
    match flags.other_than(&known) {
        Some(name) => Err(format!("the suite `{suite}` takes no `--{name}`")),
        None => Ok(()),
    }
}

pub fn bench(flags: &Flags) -> Result {
    let suite = flags.get("suite").unwrap_or("engine");
    check_flags(flags, suite)?;
    match suite {
        // B1-B8, B11 and B12.
        "engine" => kernel_bench(flags, "engine", "native-bench", "m6-engine", "engine"),
        // The collaborative slow path.
        "b9" => kernel_bench(flags, "b9", "sync-bench", "m6-sync", "sync"),
        // B13: the renderers.
        "render" => script_bench(
            flags,
            "bench/run-render-bench.mjs",
            &RENDER_SWITCHES,
            &RENDER_OPTIONS,
        ),
        // S1 and S2: viewport pages, and drawing a thousand cells.
        "grid" => script_bench(
            flags,
            "bench/run-grid-bench.mjs",
            &GRID_SWITCHES,
            &GRID_OPTIONS,
        ),
        // Against an earlier commit, which the script builds as well.
        "ab" => script_bench(flags, "bench/ab.mjs", &AB_SWITCHES, &AB_OPTIONS),
        // B10: a first visit.
        _ => startup_bench(flags),
    }
}

/// Builds the pages and the app, and runs a script that measures them and
/// archives its own report.
fn script_bench(flags: &Flags, script: &str, switches: &[&str], options: &[&str]) -> Result {
    build_for_bench(flags)?;
    util::step(script);
    let mut command = node()
        .arg(script)
        .args(["--affinity", flags.get("affinity").unwrap_or("FFFF")]);
    for name in switches {
        if flags.has(name) {
            command = command.arg(format!("--{name}"));
        }
    }
    for name in options {
        if let Some(value) = flags.get(name) {
            command = command.arg(format!("--{name}")).arg(value);
        }
    }
    command.run()
}

fn build_for_bench(flags: &Flags) -> Result {
    if flags.has("skip-build") {
        return Ok(());
    }
    wasm::build(Profile::Release)?;
    web::build()?;
    web::build_spikes()
}

/// Measures a first visit of the production build, and archives the report.
fn startup_bench(flags: &Flags) -> Result {
    const REPORT: &str = "target/bench-render/startup.json";
    build_for_bench(flags)?;
    util::step("B10: a first visit");
    let mut command = node().args([
        "spikes/startup/measure.mjs",
        "--affinity",
        flags.get("affinity").unwrap_or("FFFF"),
        "--out",
        REPORT,
    ]);
    if flags.has("quick") {
        command = command.arg("--quick");
    }
    command.run()?;
    if flags.has("no-archive") {
        return Ok(());
    }
    let text = fs::read_to_string(util::root().join(REPORT))
        .map_err(|e| format!("cannot read {REPORT}: {e}"))?;
    let results = util::root().join(flags.get("results").unwrap_or("bench/results"));
    let archive = results.join(startup_archive_name(&text)?);
    check_built_after_commit("apps/web/dist/index.html")?;
    fs::create_dir_all(&results).map_err(|e| format!("mkdir {}: {e}", results.display()))?;
    fs::write(&archive, text).map_err(|e| format!("write {}: {e}", archive.display()))?;
    println!("archived in {}", archive.display());
    Ok(())
}

/// Whether a file that a build wrote is the commit's, going by when it was written.
fn check_built_after_commit(path: &str) -> Result {
    let committed = git_output(&["log", "-1", "--format=%ct"])
        .parse()
        .map_err(|e| format!("cannot tell when the commit was made: {e}"))?;
    let built = fs::metadata(util::root().join(path))
        .and_then(|file| file.modified())
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    if built < UNIX_EPOCH + Duration::from_secs(committed) {
        return Err(format!(
            "the report is not archived: {path} was not built after the commit"
        ));
    }
    Ok(())
}

/// The name of a report of first visits among the results: when it was made and
/// of what. A report with something wrong has none.
fn startup_archive_name(text: &str) -> Result<String> {
    let report: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("cannot parse the report: {e}"))?;
    let mut reasons = Vec::new();
    if report["uncommittedChanges"] != serde_json::json!(false) {
        reasons.push("the working tree has uncommitted changes");
    }
    if report["problems"] != serde_json::json!([]) {
        reasons.push("the page reported errors");
    }
    if report["options"]["quick"] != serde_json::json!(false) {
        reasons.push("it is of a quick run");
    }
    if !reasons.is_empty() {
        return Err(format!(
            "the report is not archived: {}",
            reasons.join("; ")
        ));
    }
    let (Some(date), Some(commit)) = (
        report["date"].as_str().and_then(|date| date.get(..10)),
        report["commit"]
            .as_str()
            .filter(|commit| !commit.is_empty()),
    ) else {
        return Err("the report does not say when it was made, or of what".to_owned());
    };
    Ok(format!("{date}-{commit}-startup.json"))
}

/// Runs a suite natively and in the browser, and archives both.
fn kernel_bench(
    flags: &Flags,
    suite: &str,
    native_command: &str,
    suite_label: &str,
    archive_kind: &str,
) -> Result {
    let affinity = flags.get("affinity").unwrap_or("FFFF");
    let browser = flags.get("browser").unwrap_or("chrome");

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
    // Results do not count: nothing is built from them, and the first one that is
    // archived of a commit must not stand in the way of the others.
    let dirty = !git_output(&[
        "status",
        "--porcelain",
        "--",
        ".",
        ":(exclude)bench/results",
    ])
    .is_empty();
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

pub(crate) fn git_output(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(util::root())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

pub(crate) fn current_utc_date() -> String {
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{BENCH_OPTIONS, BENCH_SWITCHES, check_flags, startup_archive_name};
    use crate::util::Flags;

    fn check(suite: &str, args: &[&str]) -> super::Result {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        check_flags(
            &Flags::parse(&args, &BENCH_SWITCHES, &BENCH_OPTIONS)?,
            suite,
        )
    }

    #[test]
    fn a_suite_takes_its_own_flags_only() {
        assert_eq!(check("engine", &["--quick", "--out", "a.json"]), Ok(()));
        assert_eq!(
            check("render", &["--slowdown", "4", "--skip-build"]),
            Ok(())
        );
        assert_eq!(check("ab", &["--visits", "5", "--affinity", "FF"]), Ok(()));
        for (suite, flag) in [
            ("engine", "--headless"),
            ("grid", "--headless"),
            ("ab", "--software"),
            ("startup", "--headless"),
        ] {
            assert!(check(suite, &[flag]).is_err(), "{suite} {flag}");
        }
        // It would be archived in place of the one in Chrome.
        assert!(check("startup", &["--browser", "firefox"]).is_err());
        assert!(check("other", &[]).is_err());
    }

    fn report() -> Value {
        json!({
            "suite": "m6-startup",
            "date": "2026-11-26T10:13:32.305Z",
            "commit": "abc1234",
            "uncommittedChanges": false,
            "options": { "quick": false },
            "problems": [],
        })
    }

    #[test]
    fn a_startup_report_is_archived_under_its_date_and_commit() {
        assert_eq!(
            startup_archive_name(&report().to_string()).as_deref(),
            Ok("2026-11-26-abc1234-startup.json")
        );
    }

    #[test]
    fn a_startup_report_with_something_wrong_is_not_archived() {
        for (key, value) in [
            ("uncommittedChanges", json!(true)),
            ("uncommittedChanges", Value::Null),
            ("problems", json!(["an error"])),
            ("options", json!({ "quick": true })),
            ("commit", json!("")),
            ("date", Value::Null),
        ] {
            let mut report = report();
            report[key] = value;
            assert!(
                startup_archive_name(&report.to_string()).is_err(),
                "{report}"
            );
        }
        assert!(startup_archive_name("{").is_err());
    }
}
