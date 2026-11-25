//! Building the WebAssembly module and its JavaScript bindings.

mod fallbacks;
mod names;

use std::fs;
use std::path::Path;

use crate::util::{self, Cmd, Result, cargo, node, pnpm, tool};

/// Where the bindings go: the `@reticula/protocol` package re-exports them.
const CORE_OUT_DIR: &str = "packages/protocol/pkg";
const CORE_MODULE: &str = "packages/protocol/pkg/reticula_wasm_bg.wasm";
const SIM_OUT_DIR: &str = "packages/protocol/sim-pkg";
const SIM_MODULE: &str = "packages/protocol/sim-pkg/reticula_sim_wasm_bg.wasm";
const XLSX_OUT_DIR: &str = "packages/protocol/xlsx-pkg";
const XLSX_MODULE: &str = "packages/protocol/xlsx-pkg/reticula_xlsx_wasm_bg.wasm";
const TABLE_OUT_DIR: &str = "packages/protocol/table-pkg";
const TABLE_MODULE: &str = "packages/protocol/table-pkg/reticula_table_wasm_bg.wasm";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Dev,
    Release,
}

pub fn build(profile: Profile) -> Result {
    build_with_options(profile, false)
}

pub fn build_with_options(profile: Profile, parallel: bool) -> Result {
    util::step(match (profile, parallel) {
        (Profile::Dev, false) => "wasm (debug)",
        (Profile::Release, false) => "wasm (release)",
        (Profile::Dev, true) => "wasm (debug + parallel atomics)",
        (Profile::Release, true) => "wasm (release + parallel atomics)",
    });

    build_package(
        "reticula-wasm",
        "reticula_wasm.wasm",
        CORE_OUT_DIR,
        CORE_MODULE,
        profile,
        parallel,
    )?;
    build_package(
        "reticula-sim-wasm",
        "reticula_sim_wasm.wasm",
        SIM_OUT_DIR,
        SIM_MODULE,
        profile,
        false,
    )?;
    build_package(
        "reticula-xlsx-wasm",
        "reticula_xlsx_wasm.wasm",
        XLSX_OUT_DIR,
        XLSX_MODULE,
        profile,
        false,
    )?;
    build_package(
        "reticula-table-wasm",
        "reticula_table_wasm.wasm",
        TABLE_OUT_DIR,
        TABLE_MODULE,
        profile,
        false,
    )?;

    if profile == Profile::Release {
        report_sizes(&[
            Path::new(CORE_MODULE),
            Path::new(SIM_MODULE),
            Path::new(XLSX_MODULE),
            Path::new(TABLE_MODULE),
        ])?;
    }
    Ok(())
}

fn build_package(
    package: &str,
    wasm_name: &str,
    out_dir: &str,
    module_path: &str,
    profile: Profile,
    parallel: bool,
) -> Result {
    let mut build = if parallel {
        cargo().args([
            "+nightly",
            "build",
            "-Z",
            "build-std=panic_abort,std",
            "--locked",
            "--package",
            package,
            "--features",
            "parallel",
            "--target",
            "wasm32-unknown-unknown",
        ])
    } else {
        cargo().args([
            "build",
            "--locked",
            "--package",
            package,
            "--target",
            "wasm32-unknown-unknown",
        ])
    };
    if parallel {
        build = build.env(
            "RUSTFLAGS",
            "-C target-feature=+atomics,+bulk-memory,+mutable-globals",
        );
    }
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
        .arg(
            util::target_dir()
                .join("wasm32-unknown-unknown")
                .join(directory)
                .join(wasm_name),
        )
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
        // A parallel module imports its memory, which the bindings name in their own way.
        if !parallel {
            slim(module_path)?;
        }
    }
    Ok(())
}

/// Makes a module and the files wasm-bindgen wrote for it smaller.
fn slim(module_path: &str) -> Result {
    let module = util::root().join(module_path);
    let stem = module_path.trim_end_matches("_bg.wasm");
    let bindings = util::root().join(format!("{stem}.js"));
    let types = util::root().join(format!("{stem}.d.ts"));
    let module_types = util::root().join(format!("{stem}_bg.wasm.d.ts"));

    let error = |what: &str, path: &Path, e: std::io::Error| {
        format!("cannot {what} {}: {e}", path.display())
    };
    let read = |path: &Path| fs::read_to_string(path).map_err(|e| error("read", path, e));
    let (slim_bindings, slim_types) = fallbacks::remove(&read(&bindings)?, &read(&types)?)
        .map_err(|e| format!("{}: {e}", bindings.display()))?;
    let short = names::Package {
        module: fs::read(&module).map_err(|e| error("read", &module, e))?,
        bindings: slim_bindings,
        types: slim_types,
        module_types: read(&module_types)?,
    }
    .shorten()
    .map_err(|e| format!("{module_path}: {e}"))?;
    for (path, content) in [
        (&module, short.module.as_slice()),
        (&bindings, short.bindings.as_bytes()),
        (&types, short.types.as_bytes()),
        (&module_types, short.module_types.as_bytes()),
    ] {
        fs::write(path, content).map_err(|e| error("write", path, e))?;
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
