//! Running the experiments under `spikes/`.

use crate::util::{self, Cmd, Flags, Result, cargo, node, pnpm, tool};
use crate::wasm::{self, Profile};
use crate::web;

const WASM_TARGET: &str = "wasm32-unknown-unknown";

pub fn run(name: &str, options: &[String]) -> Result {
    if matches!(name, "s1" | "s2") {
        return grid(name, options);
    }
    if name == "s11" {
        return zoomed_out(options);
    }
    if name == "s6" {
        return s6_agent_loop(&Flags::parse(options, &["mock"], &["out"])?);
    }
    if name == "s9" {
        return first_visit(options);
    }
    if name == "s9-builds" {
        return engine_builds(Flags::parse(options, &[], &["affinity"])?.get("affinity"));
    }
    if name == "s10" {
        return input_methods(options);
    }
    if let Some(option) = options.first() {
        return Err(format!("spike `{name}` takes no options, got `{option}`"));
    }
    match name {
        "determinism" => determinism(),
        other => Err(format!(
            "unknown spike `{other}`; available: determinism, s1, s2, s6, s9, s9-builds, s10, s11"
        )),
    }
}

/// Runs the S6 multi-step AI tool-loop probe (live endpoint from `.env.local` / env,
/// or `--mock` deterministic dry-run) and prints latency, token, cache, and cost metrics.
fn s6_agent_loop(flags: &Flags) -> Result {
    use reticula_agent::LlmConfig;
    use reticula_agent::eval::{estimate_eval_cost_usd, run_s6_probe, s6_mock_provider};
    use reticula_agent::llm::{LlmProvider, OpenAiCompatProvider};

    let provider: Box<dyn LlmProvider> = if flags.has("mock") {
        Box::new(s6_mock_provider())
    } else {
        let dotenv = util::root().join(".env.local");
        let cfg = LlmConfig::from_env_and_dotenv(Some(&dotenv))
            .map_err(|e| format!("S6 LLM config error: {e}"))?
            .ok_or_else(|| {
                "S6 live probe requires RETICULA_LLM_ENABLED=true and RETICULA_LLM_API_KEY in .env.local or process env (or run `cargo xtask spike s6 --mock` for a local harness dry-run)".to_owned()
            })?;
        Box::new(
            OpenAiCompatProvider::new(cfg)
                .map_err(|e| format!("failed to build OpenAI-compatible client: {e}"))?,
        )
    };

    util::step(&format!(
        "s6 multi-step tool loop probe ({} / {})",
        provider.provider_name(),
        provider.model_name()
    ));
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let report = rt.block_on(run_s6_probe(provider.as_ref()));
    println!(
        "completion: {:.1}% | avg_steps: {:.2} | avg_in/step: {:.0} | avg_out/step: {:.0} | cache_hit: {:.1}%",
        report.completion_rate * 100.0,
        report.avg_steps,
        report.avg_prompt_tokens_per_step,
        report.avg_completion_tokens_per_step,
        report.cache_hit_rate * 100.0,
    );
    println!(
        "TTFT p50/p95: {}ms / {}ms | step latency p50/p95/max: {}ms / {}ms / {}ms | 429s: {} | schema_violations: {}",
        report.ttft_p50_ms,
        report.ttft_p95_ms,
        report.step_latency_p50_ms,
        report.step_latency_p95_ms,
        report.step_latency_max_ms,
        report.total_rate_limited_hits,
        report.total_schema_violations,
    );
    // Illustrative cost projection using reference $1/$0.25/$4 per 1M tokens for 400 x 3 + 400 runs.
    let est_1600 = estimate_eval_cost_usd(
        1_600,
        report.avg_steps,
        report.avg_prompt_tokens_per_step,
        report.cache_hit_rate,
        report.avg_completion_tokens_per_step,
        1.0,
        0.25,
        4.0,
    );
    println!("estimated 1,600-case eval cost (at ref $1/$0.25/$4 per 1M tok): ${est_1600:.2}");
    for sc in &report.scenarios {
        println!(
            "  - {:<32} ok={} steps={} commits={} errors={} in={} cached={} out={}",
            sc.id,
            sc.completed,
            sc.steps,
            sc.commits,
            sc.remaining_errors,
            sc.prompt_tokens,
            sc.cached_prompt_tokens,
            sc.completion_tokens,
        );
    }
    if let Some(out) = flags.get("out") {
        let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(out, json).map_err(|e| format!("cannot write {out}: {e}"))?;
        println!("wrote S6 report JSON to {out}");
    }
    Ok(())
}

/// Measures the grid in a browser: viewport pages (`s1`) or drawing text
/// (`s2`).
fn grid(name: &str, options: &[String]) -> Result {
    wasm::build(Profile::Release)?;
    web::build_spikes()?;
    util::step(name);
    node()
        .arg("spikes/grid/measure.mjs")
        .arg(name)
        .args(options)
        .run()
}

/// Measures drawing about ten thousand visible cells, on a 2D canvas and with
/// the WebGPU prototype.
fn zoomed_out(options: &[String]) -> Result {
    wasm::build(Profile::Release)?;
    web::build_spikes()?;
    util::step("s11");
    node().arg("spikes/grid/zoomed.mjs").args(options).run()
}

/// Measures a first visit: the production build, served compressed over a
/// network that is held back, opened in a browser that has never seen it.
fn first_visit(options: &[String]) -> Result {
    wasm::build(Profile::Release)?;
    web::build()?;
    util::step("s9");
    node().arg("spikes/startup/measure.mjs").args(options).run()
}

/// Builds the engine at four optimisation levels (`-O3`, `-O2`, `-Os`, `-Oz`)
/// and compares compressed module size, 1M-row sample generation time, and B3/B4
/// formula recalculation speed in Chrome (`13-m1a.md` 6.4).
fn engine_builds(affinity: Option<&str>) -> Result {
    const LEVELS: [(&str, &str); 4] = [("3", "-O3"), ("2", "-O2"), ("s", "-Os"), ("z", "-Oz")];
    const DIRECTORY: &str = "target/spikes/startup/builds";

    let mut builds = Vec::new();
    for (level, shrink) in LEVELS {
        util::step(&format!("engine at opt-level {level}"));
        let target = format!("{DIRECTORY}/{level}");
        cargo()
            .args([
                "build",
                "--locked",
                "--release",
                "--package",
                "reticula-wasm",
                "--features",
                "bench",
            ])
            .args(["--target", WASM_TARGET, "--target-dir", &target])
            .env("CARGO_PROFILE_RELEASE_OPT_LEVEL", level)
            .run()?;
        let build = format!("{target}/pkg");
        Cmd::new(tool("wasm-bindgen"))
            .args(["--target", "web", "--out-dir", &build])
            .arg(format!("{target}/{WASM_TARGET}/release/reticula_wasm.wasm"))
            .run()?;
        let module = format!("{build}/reticula_wasm_bg.wasm");
        pnpm()
            .args(["exec", "wasm-opt", shrink, &module, "--output", &module])
            .run()?;
        builds.push(build);
    }

    util::step("size, and generating the sample (in Chrome)");
    let mut engine = node()
        .arg("spikes/startup/engine.mjs")
        .args(["--out", "target/spikes/startup/engine-builds.json"]);
    if let Some(mask) = affinity {
        engine = engine.args(["--affinity", mask]);
    }
    engine.args(&builds).run()?;

    for (level, _) in LEVELS {
        util::step(&format!(
            "B4/B3 recalculation at opt-level {level} (in Chrome)"
        ));
        let pkg = format!("{DIRECTORY}/{level}/pkg");
        let out = format!("{DIRECTORY}/{level}/recalc.json");
        let mut bench = node().arg("bench/run-wasm-bench.mjs").args([
            "--pkg",
            &pkg,
            "--only-b4",
            "--out",
            &out,
        ]);
        if let Some(mask) = affinity {
            bench = bench.args(["--affinity", mask]);
        }
        bench.run()?;
    }
    Ok(())
}

/// Serves the page on which input methods are checked by hand.
fn input_methods(options: &[String]) -> Result {
    wasm::build(Profile::Release)?;
    web::build_spikes()?;
    util::step("s10");
    node().arg("spikes/grid/ime.mjs").args(options).run()
}

/// Prints everything `spikes/determinism` measures, for this machine's native
/// target and for WebAssembly.
fn determinism() -> Result {
    const PACKAGE: &str = "spike-determinism";
    let report = ["--test", "report", "--", "--nocapture", "print_report"];

    util::step("report (native)");
    cargo()
        .args(["test", "--locked", "--package", PACKAGE])
        .args(report)
        .run()?;

    util::step("report (wasm32, in Node)");
    cargo()
        .args([
            "test",
            "--locked",
            "--package",
            PACKAGE,
            "--target",
            WASM_TARGET,
        ])
        .args(report)
        .env(
            "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER",
            tool("wasm-bindgen-test-runner"),
        )
        .run()?;

    util::step("rounding cost (native, release)");
    cargo()
        .args([
            "test",
            "--locked",
            "--release",
            "--package",
            PACKAGE,
            "--test",
            "report",
            "--",
        ])
        .args(["--ignored", "--nocapture", "print_rounding_cost"])
        .run()?;

    util::step("rounding cost (wasm32, release, in Node)");
    cargo()
        .args([
            "build",
            "--locked",
            "--release",
            "--package",
            PACKAGE,
            "--target",
            WASM_TARGET,
        ])
        .run()?;
    node()
        .arg("spikes/determinism/time-wasm.mjs")
        .arg(format!(
            "target/{WASM_TARGET}/release/spike_determinism.wasm"
        ))
        .run()
}
