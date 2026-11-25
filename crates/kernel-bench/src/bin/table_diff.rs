//! The native half of `cargo xtask diff --tables`: makes up tables and answers
//! questions about them. The other half is `bench/table-diff/run.mjs`.

use std::path::PathBuf;
use std::process::ExitCode;

use reticula_kernel_bench::table_diff::{answer, generate};

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

fn run(args: &[String]) -> Result<(), String> {
    let option = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
            .ok_or_else(|| format!("{name} is missing"))
    };
    let number = |name: &str| {
        option(name)?
            .parse::<u64>()
            .map_err(|error| format!("{name}: {error}"))
    };
    let dir = PathBuf::from(option("--dir")?);
    match args.first().map(String::as_str) {
        Some("generate") => generate(&dir, number("--seed")?),
        Some("answer") => {
            let count = usize::try_from(number("--count")?).map_err(|error| error.to_string())?;
            answer(&dir, &option("--from")?, count, number("--seed")?)
        }
        _ => Err("usage: table_diff generate|answer --dir <dir> --seed <n> ...".to_owned()),
    }
}
