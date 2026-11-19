//! Checks and tests of the Rust workspace.

use crate::util::{self, Cmd, Result, cargo, tool};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// A target that has no standard library at all. Kernel crates are `no_std`;
/// checking them here proves it, because anything that reaches for `std`,
/// in the crate or in one of its dependencies, fails to compile.
const NO_STD_TARGET: &str = "wasm32v1-none";

/// Crates that make up the engine kernel, by directory name under `crates/`.
const KERNEL_CRATES: [&str; 3] = ["core", "formula", "wire"];

/// Packages that are compiled to WebAssembly.
const WASM_PACKAGES: [&str; 4] = [
    "reticula-core",
    "reticula-formula",
    "reticula-wire",
    "reticula-wasm",
];

pub fn check() -> Result {
    util::step("rustfmt");
    cargo().args(["fmt", "--all", "--check"]).run()?;

    util::step("clippy");
    cargo()
        .args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ])
        .run()?;

    util::step("clippy (wasm32)");
    let mut clippy = cargo().args([
        "clippy",
        "--all-targets",
        "--locked",
        "--target",
        WASM_TARGET,
    ]);
    for package in WASM_PACKAGES {
        clippy = clippy.args(["--package", package]);
    }
    clippy.args(["--", "-D", "warnings"]).run()?;

    util::step("kernel without a standard library");
    let mut check = cargo().args(["check", "--locked", "--target", NO_STD_TARGET]);
    for name in KERNEL_CRATES {
        check = check.args(["--package", &format!("reticula-{name}")]);
    }
    check.run()?;

    util::step("kernel lint block");
    check_kernel_lint_block()
}

pub fn test() -> Result {
    util::step("tests");
    Cmd::new(tool("cargo-nextest"))
        .args([
            "nextest",
            "run",
            "--workspace",
            "--locked",
            "--no-tests",
            "pass",
        ])
        .run()?;

    util::step("doc tests");
    cargo()
        .args(["test", "--doc", "--workspace", "--locked"])
        .run()
}

/// Runs the kernel tests again as WebAssembly, in Node.
pub fn test_wasm() -> Result {
    util::step("kernel tests (wasm32)");
    let mut test = cargo().args(["test", "--locked", "--target", WASM_TARGET]);
    for name in KERNEL_CRATES {
        test = test.args(["--package", &format!("reticula-{name}")]);
    }
    test.env(
        "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER",
        tool("wasm-bindgen-test-runner"),
    )
    .run()
}

pub fn deny() -> Result {
    util::step("cargo-deny");
    Cmd::new(tool("cargo-deny")).args(["deny", "check"]).run()
}

/// Every kernel crate must be `no_std` and carry the same lint block.
fn check_kernel_lint_block() -> Result {
    const BEGIN: &str = "// --- kernel lint block";
    const END: &str = "// --- end kernel lint block";

    let mut reference: Option<(&str, String)> = None;
    for name in KERNEL_CRATES {
        let path = util::root().join("crates").join(name).join("src/lib.rs");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if !text.lines().any(|line| line.trim() == "#![no_std]") {
            return Err(format!("kernel crate `{name}` is not #![no_std]"));
        }
        let block = text
            .split_once(BEGIN)
            .and_then(|(_, rest)| rest.split_once(END))
            .map(|(block, _)| block.to_owned())
            .ok_or_else(|| format!("kernel crate `{name}` has no kernel lint block"))?;
        match &reference {
            None => reference = Some((name, block)),
            Some((first, expected)) if *expected != block => {
                return Err(format!(
                    "the kernel lint blocks of `{first}` and `{name}` differ; they must be identical"
                ));
            }
            Some(_) => {}
        }
    }
    println!("{} kernel crate(s) checked", KERNEL_CRATES.len());
    Ok(())
}
