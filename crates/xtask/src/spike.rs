//! Running the experiments under `spikes/`.

use crate::util::{self, Result, cargo, node, tool};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

pub fn run(name: &str) -> Result {
    match name {
        "determinism" => determinism(),
        other => Err(format!("unknown spike `{other}`; available: determinism")),
    }
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
