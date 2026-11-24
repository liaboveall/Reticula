//! Project tasks, run as `cargo xtask <command>`.
//!
//! Every build, check and test in this repository goes through here, so the
//! same commands work on Windows, Linux and macOS without shell scripts, and CI
//! runs exactly what a developer runs.

mod codegen;
mod compat;
mod eval;
mod qa;
mod rust;
mod setup;
mod spike;
mod stack;
mod util;
mod wasm;
mod web;

use std::process::ExitCode;

use util::{Flags, Result};
use wasm::Profile;

const HELP: &str = "\
Usage: cargo xtask <command> [options]

Commands:
  setup [--browsers]      Install the pinned command-line tools into .tools/ and the
                          JavaScript dependencies. --browsers also downloads the
                          Playwright browsers.
  check [--rust] [--web] [--skip-wasm]
                          Formatting, lints and type checks. With no option, both halves.
  test [--rust] [--wasm] [--web] [--server]
                          Tests: native, the kernel tests again under WebAssembly, and
                          the front-end unit tests. With no option, those three.
                          --server runs the server's PostgreSQL tests, against
                          RETICULA_TEST_DATABASE_URL or the development database.
  deny                    Dependency licences, advisories and sources.
  wasm [--dev] [--parallel]
                          Build the WebAssembly module and its bindings (`--parallel` enables
                          nightly atomics + rayon worker threads on `reticula-wasm`).
  codegen [--check]       Write the TypeScript files generated from the Rust sources;
                          --check only reports whether they are up to date.
  digest [--update]       Print the determinism digest; --update records it as expected.
  compat [--freeze v1|v2|v3]
                          Verify frozen wire v1, v2, and v3 SHA-256 hashes (or generate/freeze fixtures).
  diff [--engine rust|libreoffice|all] [--count <n>] [--quota-per-fn <n>] [--seed <n>] [--image <tag>] [--skip-image] [--dump-xlsx <dir>] [--archive] [--out <file>]
                          Differential evaluation against ironcalc_base, formualizer-eval, and/or headless LibreOffice Calc.
  fuzz [--cpu-seconds <s>] [--threads <n>] [--affinity <mask>] [--target <name>]
                          Multi-threaded mutation & structured fuzzing campaign over the six
                          targets, or with --target (parser, snapshot, wal, ops, eval, sync)
                          over one of them.
  sim [--seeds <n>] [--start-seed <n>] [--seed <s>] [--threads <n>] [--fail-fast] [--out <file>]
                          Deterministic multi-client sync simulation.
  bench [--affinity <mask>] [--browser <name>] [--quick] [--out <file>] [--suite engine|b9]
                          Run native and Chrome WASM engine benchmarks (B1-B8, B11, B12),
                          or with --suite b9 the collaborative slow path (B9).
  dev                     Build a debug module and start the development server.
  db up|down|reset        The development database (PostgreSQL 18 in Docker on
                          127.0.0.1:5434) for the server, its tests and the end-to-end
                          tests. `reset` also deletes its data.
  serve [--skip-build] [--origin <url>]
                          Serve the production build from a debug server on
                          http://127.0.0.1:3200 with the development sign-in, for
                          trying collaboration in two browser windows. With
                          --origin http://localhost:5273 it serves `cargo xtask dev`.
  stack [--skip-image] [--keep]
                          Build the release image, start infra/compose.yml with it
                          (server, PostgreSQL, volumes, resource limits) and run
                          tests/stack against it: sync, a killed server, a restarted
                          database, recreated containers.
  drill [--skip-image]    End-to-end backup, missing-snapshot window cleanup,
                          log_epoch bump (0 -> 1), and T1-ahead client recovery drill.
  loadtest [--skip-image] [--out <file>]
                          Run 50 concurrent clients at 100 txns/s against a ~2M-cell
                          (~60 MiB) workbook under 768m/1.0-CPU compose limits.
  build [--skip-wasm] [--check-budget]
                          Production build with preloads, precompressed assets and size budget check.
  e2e [--browser <name>] [--skip-build]
                          End-to-end tests in chromium, firefox and webkit. The
                          collaborative ones use RETICULA_TEST_DATABASE_URL, or else
                          the development database, which they start.
  ci                      Everything CI runs on a push. Run it before pushing.
  spike <name> [options]  Run one of the experiments under spikes/ and print its report.
                          `s1` and `s2` measure the grid in a browser and pass their
                          options on: see spikes/grid/measure.mjs. `s6 [--mock] [--out <file>]`
                          probes multi-step LLM tool-calling reliability and cost. `s9` measures
                          a first visit of the production build: see spikes/startup/measure.mjs;
                          `s9-builds [--affinity <mask>]` compares the engine at several
                          optimisation levels. `s10` serves the page for checking input
                          methods: see spikes/grid/ime.mjs.
  eval <replays|download|run|grade|report> [options]
                          Generate the 10 built-in AI demo replays (`replays`) or run/grade/report
                          SpreadsheetBench Verified evaluations.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result {
    let Some((command, rest)) = args.split_first() else {
        print!("{HELP}");
        return Ok(());
    };
    match command.as_str() {
        "setup" => setup::run(&Flags::parse(rest, &["browsers"], &[])?),
        "check" => check(&Flags::parse(rest, &["rust", "web", "skip-wasm"], &[])?),
        "test" => test(&Flags::parse(
            rest,
            &["rust", "wasm", "web", "server"],
            &[],
        )?),
        "deny" => {
            Flags::parse(rest, &[], &[])?;
            rust::deny()
        }
        "wasm" => {
            let flags = Flags::parse(rest, &["dev", "parallel"], &[])?;
            wasm::build_with_options(
                if flags.has("dev") {
                    Profile::Dev
                } else {
                    Profile::Release
                },
                flags.has("parallel"),
            )
        }
        "codegen" => codegen::run(Flags::parse(rest, &["check"], &[])?.has("check")),
        "digest" => digest(&Flags::parse(rest, &["update"], &[])?),
        "compat" => compat::run(&Flags::parse(rest, &[], &["freeze"])?),
        "diff" => qa::diff(&Flags::parse(
            rest,
            &["skip-image", "archive"],
            &[
                "engine",
                "count",
                "quota-per-fn",
                "seed",
                "image",
                "dump-xlsx",
                "out",
                "date",
            ],
        )?),
        "fuzz" => qa::fuzz(&Flags::parse(
            rest,
            &[],
            &["cpu-seconds", "threads", "affinity", "out", "target"],
        )?),
        "sim" => qa::sim(&Flags::parse(
            rest,
            &["fail-fast"],
            &["seeds", "start-seed", "seed", "threads", "out"],
        )?),
        "bench" => qa::bench(&Flags::parse(
            rest,
            &["quick"],
            &["affinity", "browser", "date", "out", "suite"],
        )?),
        "dev" => {
            Flags::parse(rest, &[], &[])?;
            wasm::build(Profile::Dev)?;
            web::dev()
        }
        "db" => stack::db(rest),
        "serve" => stack::serve(&Flags::parse(rest, &["skip-build"], &["origin"])?),
        "stack" => stack::stack(&Flags::parse(rest, &["skip-image", "keep"], &[])?),
        "drill" => stack::drill(&Flags::parse(rest, &["skip-image"], &[])?),
        "loadtest" => stack::loadtest(&Flags::parse(rest, &["skip-image"], &["out"])?),
        "build" => build(&Flags::parse(rest, &["skip-wasm", "check-budget"], &[])?),
        "e2e" => e2e(&Flags::parse(rest, &["skip-build"], &["browser"])?),
        "ci" => {
            Flags::parse(rest, &[], &[])?;
            ci()
        }
        "eval" => eval::run(rest),
        "spike" => match rest.split_first() {
            Some((name, options)) => spike::run(name, options),
            None => Err("usage: cargo xtask spike <name> [options]".to_owned()),
        },
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{HELP}")),
    }
}

fn check(flags: &Flags) -> Result {
    let everything = !flags.has("rust") && !flags.has("web");
    if everything || flags.has("rust") {
        rust::check()?;
        compat::check()?;
        setup::check_workflow_pins()?;
    }
    if everything || flags.has("web") {
        codegen::run(true)?;
        // The TypeScript types of the engine are generated with the module.
        if !flags.has("skip-wasm") {
            wasm::build(Profile::Dev)?;
        }
        web::check()?;
    }
    Ok(())
}

fn test(flags: &Flags) -> Result {
    let everything =
        !flags.has("rust") && !flags.has("wasm") && !flags.has("web") && !flags.has("server");
    if everything || flags.has("rust") {
        rust::test()?;
    }
    if everything || flags.has("wasm") {
        rust::test_wasm()?;
    }
    if everything || flags.has("web") {
        web::test()?;
    }
    if flags.has("server") {
        rust::test_server()?;
    }
    Ok(())
}

fn digest(flags: &Flags) -> Result {
    use reticula_formula::determinism;

    let hex = determinism::to_hex(determinism::digest());
    let hex = String::from_utf8_lossy(&hex);
    let path = util::root().join("tests/determinism/expected.txt");
    if flags.has("update") {
        std::fs::write(&path, format!("{hex}\n"))
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("{hex} (recorded)");
        return Ok(());
    }
    let expected = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    println!("{hex}");
    if expected.trim() == hex {
        Ok(())
    } else {
        Err(format!(
            "expected {}; run with --update if the workload changed",
            expected.trim()
        ))
    }
}

fn build(flags: &Flags) -> Result {
    if !flags.has("skip-wasm") {
        wasm::build(Profile::Release)?;
    }
    web::build()?;
    web::build_spikes()
}

fn e2e(flags: &Flags) -> Result {
    if !flags.has("skip-build") {
        wasm::build(Profile::Release)?;
        web::build()?;
        web::build_spikes()?;
    }
    web::e2e(flags.get("browser"))
}

fn ci() -> Result {
    rust::check()?;
    compat::check()?;
    setup::check_workflow_pins()?;
    rust::test()?;
    rust::test_server()?;
    // 5,000 seeds, as on CI; the nightly workflow runs 100,000.
    qa::sim(&Flags::parse(&[], &[], &[])?)?;
    rust::test_wasm()?;
    rust::deny()?;
    wasm::build(Profile::Release)?;
    codegen::run(true)?;
    web::check()?;
    web::test()?;
    web::build()?;
    web::build_spikes()?;
    web::e2e(None)
}
