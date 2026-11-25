//! Checks and tests of the Rust workspace.

use crate::util::{self, Cmd, Result, cargo, node, tool};

const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// A target that has no standard library at all. Kernel crates are `no_std`;
/// checking them here proves it, because anything that reaches for `std`,
/// in the crate or in one of its dependencies, fails to compile.
const NO_STD_TARGET: &str = "wasm32v1-none";

/// Crates that make up the engine kernel, by directory name under `crates/`.
const KERNEL_CRATES: [&str; 5] = ["core", "formula", "wire", "sync", "table"];

/// All workspace packages compiled & linted against `wasm32-unknown-unknown`.
const WASM_CLIPPY_PACKAGES: [&str; 10] = [
    "reticula-core",
    "reticula-formula",
    "reticula-wire",
    "reticula-sync",
    "reticula-table",
    "reticula-wasm",
    "reticula-sim-wasm",
    "reticula-xlsx",
    "reticula-xlsx-wasm",
    "reticula-table-wasm",
];

/// Packages whose unit/integration tests run under `wasm-bindgen-test-runner`.
/// (`reticula-xlsx` / `reticula-xlsx-wasm` are verified in Node below because
/// `wasm-bindgen-test-runner` unescapes `\x` inside Windows `\crates\xlsx\` paths.)
const WASM_TEST_PACKAGES: [&str; 8] = [
    "reticula-core",
    "reticula-formula",
    "reticula-wire",
    "reticula-sync",
    "reticula-table",
    "reticula-wasm",
    "reticula-sim-wasm",
    "reticula-table-wasm",
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
    for package in WASM_CLIPPY_PACKAGES {
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

/// Runs the server's tests that need PostgreSQL (`#[ignore]` in a plain run):
/// storage, the permission matrix over HTTP and WebSocket, and the forced
/// process exit that must not lose an acknowledged transaction.
pub fn test_server() -> Result {
    let database = crate::stack::test_database_url()?;
    util::step("server tests against PostgreSQL");
    Cmd::new(tool("cargo-nextest"))
        .args([
            "nextest",
            "run",
            "--package",
            "reticula-server",
            "--locked",
            "--run-ignored",
            "only",
        ])
        .env("RETICULA_TEST_DATABASE_URL", database)
        .run()
}

/// Runs the kernel tests again as WebAssembly in Node, plus the on-demand
/// `reticula-xlsx-wasm` round-trip/patch smoke verification.
pub fn test_wasm() -> Result {
    util::step("kernel tests (wasm32)");
    let mut test = cargo().args(["test", "--locked", "--target", WASM_TARGET]);
    for package in WASM_TEST_PACKAGES {
        test = test.args(["--package", package]);
    }
    test.env(
        "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER",
        tool("wasm-bindgen-test-runner"),
    )
    .run()?;

    test_xlsx_wasm_in_node()
}

fn test_xlsx_wasm_in_node() -> Result {
    util::step("xlsx-wasm smoke test (wasm32 + Node)");
    cargo()
        .args([
            "build",
            "--locked",
            "--package",
            "reticula-xlsx-wasm",
            "--target",
            WASM_TARGET,
        ])
        .run()?;
    Cmd::new(tool("wasm-bindgen"))
        .args([
            "--target",
            "web",
            "--out-dir",
            "target/wasm-xlsx-smoke",
            "--debug",
            "target/wasm32-unknown-unknown/debug/reticula_xlsx_wasm.wasm",
        ])
        .run()?;

    const SCRIPT: &str = "\
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const pkgUrl = pathToFileURL(path.resolve('target/wasm-xlsx-smoke/reticula_xlsx_wasm.js')).href;
const mod = await import(pkgUrl);
mod.initSync({ module: fs.readFileSync('target/wasm-xlsx-smoke/reticula_xlsx_wasm_bg.wasm') });

const snap = fs.readFileSync('tests/compat/v3/base.rtclsnap');
const exp1 = mod.exportXlsxSnapshot(snap);
const xlsx = exp1.takeBytes();
assert.equal(xlsx[0], 0x50);
assert.equal(xlsx[1], 0x4b);
assert.equal(xlsx[2], 0x03);
assert.equal(xlsx[3], 0x04);
exp1.free();

const imp1 = mod.importXlsx(xlsx);
const report1 = imp1.report();
assert.ok(report1.sheetCount >= 1);
const importedSnap = imp1.takeSnapshot();
assert.ok(importedSnap.byteLength > 32);
imp1.free();

const patched = mod.patchXlsxSnapshot(xlsx, importedSnap, [{ sheetName: 'Sheet1', row: 0, col: 0 }]);
const patchedXlsx = patched.takeBytes();
patched.free();

const imp2 = mod.importXlsx(patchedXlsx);
const report2 = imp2.report();
assert.equal(report2.sheetCount, report1.sheetCount);
imp2.free();
console.log('xlsx-wasm Node smoke passed:', { sheets: report1.sheetCount, formulas: report1.formulaCount, cells: report1.cellCount });
";
    node().args(["--input-type=module", "--eval", SCRIPT]).run()
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
