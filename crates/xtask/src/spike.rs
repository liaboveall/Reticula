//! Running the experiments under `spikes/`.

use crate::util::{self, Cmd, Flags, Result, cargo, node, pnpm, tool};
use crate::wasm::{self, Profile};
use crate::web;

const WASM_TARGET: &str = "wasm32-unknown-unknown";

pub fn run(name: &str, options: &[String]) -> Result {
    if matches!(name, "s1" | "s2") {
        return grid(name, options);
    }
    if name == "s4" {
        return storage(options);
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
            "unknown spike `{other}`; available: determinism, s1, s2, s4, s9, s9-builds, s10"
        )),
    }
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

/// Runs the experiments on local storage and tabs in a browser.
fn storage(options: &[String]) -> Result {
    util::step("s4");
    node().arg("spikes/opfs/run.mjs").args(options).run()
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
