//! Running the experiments under `spikes/`.

use crate::util::{self, Cmd, Result, cargo, node, pnpm, tool};
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
    if name == "s10" {
        return input_methods(options);
    }
    if let Some(option) = options.first() {
        return Err(format!("spike `{name}` takes no options, got `{option}`"));
    }
    match name {
        "axis" => axis(),
        "axis-cost" => axis_cost(),
        "determinism" => determinism(),
        "s9-builds" => engine_builds(),
        other => Err(format!(
            "unknown spike `{other}`; available: axis, axis-cost, determinism, s1, s2, s4, s9, \
             s9-builds, s10"
        )),
    }
}

/// Measures the grid in a browser: viewport pages (`s1`) or drawing text
/// (`s2`). The page that is measured is built like the app, from the same
/// engine and grid, and is driven by a script that takes the options.
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

/// Runs the experiments on local storage and tabs in a browser: files in the
/// origin private file system, and locks shared between tabs. Their page needs
/// no build; the script that drives it takes the options.
fn storage(options: &[String]) -> Result {
    util::step("s4");
    node().arg("spikes/opfs/run.mjs").args(options).run()
}

/// Measures a first visit: the production build, served compressed over a
/// network that is held back, opened in a browser that has never seen it. The
/// script that does it takes the options.
fn first_visit(options: &[String]) -> Result {
    wasm::build(Profile::Release)?;
    web::build()?;
    util::step("s9");
    node().arg("spikes/startup/measure.mjs").args(options).run()
}

/// Builds the engine at several optimisation levels, apart from the build the
/// app uses, and reports for each how large the module is and how fast it
/// runs in a browser: generating the sample, and the workloads of the
/// identity-addressing prototype, which stand for calculation.
fn engine_builds() -> Result {
    const LEVELS: [(&str, &str); 4] = [("3", "-O3"), ("2", "-O2"), ("s", "-Os"), ("z", "-Oz")];
    const DIRECTORY: &str = "target/spikes/startup/builds";

    let mut builds = Vec::new();
    for (level, shrink) in LEVELS {
        util::step(&format!("engine at opt-level {level}"));
        let target = format!("{DIRECTORY}/{level}");
        cargo()
            .args(["build", "--locked", "--release"])
            .args(["--package", "reticula-wasm", "--package", "spike-axis"])
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
    node()
        .arg("spikes/startup/engine.mjs")
        .args(["--out", "target/spikes/startup/engine-builds.json"])
        .args(&builds)
        .run()?;

    for (level, _) in LEVELS {
        util::step(&format!("calculation at opt-level {level} (in Chrome)"));
        node()
            .arg("spikes/axis/time-chromium.mjs")
            .arg(format!(
                "{DIRECTORY}/{level}/{WASM_TARGET}/release/spike_axis.wasm"
            ))
            .arg("chrome")
            .run()?;
    }
    Ok(())
}

/// Serves the page on which input methods are checked by hand, and opens it
/// in a browser if the options name one. It runs until it is stopped.
fn input_methods(options: &[String]) -> Result {
    wasm::build(Profile::Release)?;
    web::build_spikes()?;
    util::step("s10");
    node().arg("spikes/grid/ime.mjs").args(options).run()
}

/// Runs the identity-addressing prototype: the row moves recorded from Excel,
/// then the concurrent edits, printed as a transcript of what users end up with.
fn axis() -> Result {
    util::step("row moves, against Excel");
    cargo()
        .args([
            "test",
            "--locked",
            "--package",
            "spike-axis",
            "--test",
            "excel_row_moves",
        ])
        .run()?;

    util::step("concurrent edits");
    cargo()
        .args([
            "test",
            "--locked",
            "--package",
            "spike-axis",
            "--test",
            "concurrency",
            "--",
        ])
        .args(["--nocapture", "--test-threads=1"])
        .run()
}

/// Times what identity addressing costs next to addressing by position:
/// natively, then as WebAssembly inside Chromium.
fn axis_cost() -> Result {
    const PACKAGE: &str = "spike-axis";

    util::step("cost (native, release)");
    cargo()
        .args([
            "test",
            "--locked",
            "--release",
            "--package",
            PACKAGE,
            "--test",
            "cost",
            "--",
        ])
        .args(["--ignored", "--nocapture", "print_costs"])
        .run()?;

    util::step("cost (wasm32, release, in Chromium)");
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
        .arg("spikes/axis/time-chromium.mjs")
        .arg(format!("target/{WASM_TARGET}/release/spike_axis.wasm"))
        .run()
}

/// Prints everything `spikes/determinism` measures, for this machine's native
/// target and for WebAssembly. Compare the reports of different machines.
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
