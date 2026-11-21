//! Building the WebAssembly module and its JavaScript bindings.

use std::path::Path;

use crate::util::{self, Cmd, Result, cargo, node, pnpm, tool};

/// Where the bindings go: the `@reticula/protocol` package re-exports them.
const CORE_OUT_DIR: &str = "packages/protocol/pkg";
const CORE_MODULE: &str = "packages/protocol/pkg/reticula_wasm_bg.wasm";
const SIM_OUT_DIR: &str = "packages/protocol/sim-pkg";
const SIM_MODULE: &str = "packages/protocol/sim-pkg/reticula_sim_wasm_bg.wasm";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Dev,
    Release,
}

pub fn build(profile: Profile) -> Result {
    util::step(match profile {
        Profile::Dev => "wasm (debug)",
        Profile::Release => "wasm (release)",
    });

    build_package(
        "reticula-wasm",
        "reticula_wasm.wasm",
        CORE_OUT_DIR,
        CORE_MODULE,
        profile,
    )?;
    build_package(
        "reticula-sim-wasm",
        "reticula_sim_wasm.wasm",
        SIM_OUT_DIR,
        SIM_MODULE,
        profile,
    )?;

    if profile == Profile::Release {
        report_sizes(&[Path::new(CORE_MODULE), Path::new(SIM_MODULE)])?;
    }
    Ok(())
}

fn build_package(
    package: &str,
    wasm_name: &str,
    out_dir: &str,
    module_path: &str,
    profile: Profile,
) -> Result {
    let mut build = cargo().args([
        "build",
        "--locked",
        "--package",
        package,
        "--target",
        "wasm32-unknown-unknown",
    ]);
    let mut bindgen =
        Cmd::new(tool("wasm-bindgen")).args(["--target", "web", "--out-dir", out_dir]);
    let directory = match profile {
        Profile::Dev => {
            bindgen = bindgen.args(["--debug", "--keep-debug"]);
            "debug"
        }
        Profile::Release => {
            build = build.arg("--release");
            "release"
        }
    };
    build.run()?;
    bindgen
        .arg(format!(
            "target/wasm32-unknown-unknown/{directory}/{wasm_name}"
        ))
        .run()?;

    if profile == Profile::Release {
        pnpm()
            .args([
                "exec",
                "wasm-opt",
                "-O3",
                module_path,
                "--output",
                module_path,
            ])
            .run()?;
    }
    Ok(())
}

/// Prints the raw, gzip and brotli size of each file.
pub fn report_sizes(files: &[&Path]) -> Result {
    // Node ships both compressors; this avoids a dependency for a size report.
    const SCRIPT: &str = "\
const fs = require('node:fs'), zlib = require('node:zlib');
for (const file of process.argv.slice(1)) {
  const data = fs.readFileSync(file);
  const gzip = zlib.gzipSync(data, { level: 9 }).length;
  const brotli = zlib.brotliCompressSync(data, { params: {
    [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
    [zlib.constants.BROTLI_PARAM_SIZE_HINT]: data.length,
  } }).length;
  console.log([data.length, gzip, brotli, file].join('\\t'));
}";
    let output = node().args(["--eval", SCRIPT]).args(files).output()?;
    let mut summary = String::from("| file | raw | gzip | brotli |\n|---|--:|--:|--:|\n");
    println!("{:>12} {:>12} {:>12}  file", "raw", "gzip", "brotli");
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if let [raw, gzip, brotli, file] = fields.as_slice() {
            let file = file.replace('\\', "/");
            println!("{raw:>12} {gzip:>12} {brotli:>12}  {file}");
            summary.push_str(&format!("| `{file}` | {raw} | {gzip} | {brotli} |\n"));
        }
    }
    // On CI the same table goes into the job summary.
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write as _;
        let written = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .and_then(|mut file| writeln!(file, "{summary}"));
        written.map_err(|error| format!("cannot write the job summary: {error}"))?;
    }
    Ok(())
}
