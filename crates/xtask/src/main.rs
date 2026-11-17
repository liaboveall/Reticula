//! Project tasks, run as `cargo xtask <command>`.
//!
//! Every build, check and test in this repository goes through here, so the
//! same commands work on Windows, Linux and macOS without shell scripts, and CI
//! runs exactly what a developer runs.

mod codegen;
mod rust;
mod setup;
mod spike;
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
  test [--rust] [--wasm] [--web]
                          Tests: native, the kernel tests again under WebAssembly, and
                          the front-end unit tests. With no option, all three.
  deny                    Dependency licences, advisories and sources.
  wasm [--dev]            Build the WebAssembly module and its bindings.
  codegen [--check]       Write the TypeScript files generated from the Rust sources;
                          --check only reports whether they are up to date.
  digest [--update]       Print the determinism digest; --update records it as expected.
  dev                     Build a debug module and start the development server.
  build [--skip-wasm]     Production build with precompressed assets and a size report.
  e2e [--browser <name>] [--skip-build]
                          End-to-end tests in chromium, firefox and webkit.
  ci                      Everything CI runs on a push. Run it before pushing.
  spike <name> [options]  Run one of the experiments under spikes/ and print its report.
                          `s1` and `s2` measure the grid in a browser and pass their
                          options on: see spikes/grid/measure.mjs. `s10` serves the
                          page for checking input methods: see spikes/grid/ime.mjs.
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
        "test" => test(&Flags::parse(rest, &["rust", "wasm", "web"], &[])?),
        "deny" => {
            Flags::parse(rest, &[], &[])?;
            rust::deny()
        }
        "wasm" => {
            let flags = Flags::parse(rest, &["dev"], &[])?;
            wasm::build(if flags.has("dev") {
                Profile::Dev
            } else {
                Profile::Release
            })
        }
        "codegen" => codegen::run(Flags::parse(rest, &["check"], &[])?.has("check")),
        "digest" => digest(&Flags::parse(rest, &["update"], &[])?),
        "dev" => {
            Flags::parse(rest, &[], &[])?;
            wasm::build(Profile::Dev)?;
            web::dev()
        }
        "build" => build(&Flags::parse(rest, &["skip-wasm"], &[])?),
        "e2e" => e2e(&Flags::parse(rest, &["skip-build"], &["browser"])?),
        "ci" => {
            Flags::parse(rest, &[], &[])?;
            ci()
        }
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
    let everything = !flags.has("rust") && !flags.has("wasm") && !flags.has("web");
    if everything || flags.has("rust") {
        rust::test()?;
    }
    if everything || flags.has("wasm") {
        rust::test_wasm()?;
    }
    if everything || flags.has("web") {
        web::test()?;
    }
    Ok(())
}

fn digest(flags: &Flags) -> Result {
    use reticula_core::determinism;

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
    setup::check_workflow_pins()?;
    rust::test()?;
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
