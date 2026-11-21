//! The collaborative server and what it runs on: the development database,
//! a local server for trying collaboration by hand, and the whole service as
//! it ships, brought up in Docker for a run-through.

use std::fs;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::util::{self, Cmd, Flags, Result, cargo, pnpm};
use crate::web;

const DEV_COMPOSE: &str = "infra/compose.dev.yml";
const STACK_COMPOSE: &str = "infra/compose.yml";
const STACK_PROJECT: &str = "reticula-stack";
const STACK_IMAGE: &str = "reticula:stack";
const STACK_ORIGIN: &str = "http://127.0.0.1:3200";

/// The development database from `infra/compose.dev.yml`.
pub const DEV_DATABASE_URL: &str = "postgres://reticula:reticula_local_dev@127.0.0.1:5434/reticula";

fn docker() -> Cmd {
    Cmd::new("docker")
}

/// `cargo xtask db up|down|reset`.
pub fn db(args: &[String]) -> Result {
    let (action, rest) = args
        .split_first()
        .ok_or_else(|| "usage: cargo xtask db up|down|reset".to_owned())?;
    Flags::parse(rest, &[], &[])?;
    match action.as_str() {
        "up" => {
            dev_db_up()?;
            println!("development database: {DEV_DATABASE_URL}");
            Ok(())
        }
        "down" => docker()
            .args(["compose", "--file", DEV_COMPOSE, "down"])
            .run(),
        "reset" => docker()
            .args(["compose", "--file", DEV_COMPOSE, "down", "--volumes"])
            .run(),
        other => Err(format!(
            "unknown action `{other}`: usage: cargo xtask db up|down|reset"
        )),
    }
}

fn dev_db_up() -> Result {
    util::step("development database (PostgreSQL 18 on 127.0.0.1:5434)");
    docker()
        .args(["compose", "--file", DEV_COMPOSE, "up", "--detach", "--wait"])
        .run()
}

/// The database for tests that need one: `RETICULA_TEST_DATABASE_URL` when it
/// is set (as on CI), otherwise the development database, started if needed.
pub fn test_database_url() -> Result<String> {
    match std::env::var("RETICULA_TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => Ok(url),
        _ => {
            dev_db_up()?;
            Ok(DEV_DATABASE_URL.to_owned())
        }
    }
}

/// `cargo xtask serve [--skip-build] [--origin <url>]`: the production build
/// served by a debug server on http://127.0.0.1:3200 with the development
/// sign-in, for trying collaboration in two browser windows. With
/// `--origin http://localhost:5273` the server accepts the pages of
/// `cargo xtask dev` instead, whose Vite server forwards `/api` to it.
pub fn serve(flags: &Flags) -> Result {
    let origin = flags.get("origin").unwrap_or(STACK_ORIGIN);
    if !flags.has("skip-build") {
        crate::wasm::build(crate::wasm::Profile::Release)?;
        web::build()?;
    }
    let database = test_database_url()?;
    util::step("server (debug, development sign-in)");
    cargo()
        .args(["build", "--locked", "--package", "reticula-server"])
        .run()?;
    let data = util::root().join("target/serve/data");
    fs::create_dir_all(&data).map_err(|e| format!("cannot create {}: {e}", data.display()))?;
    println!(
        "open {origin}/app in two browser profiles and sign in as Alice and Bob; Ctrl+C stops"
    );
    Cmd::new(server_binary())
        .env("BIND_ADDR", "127.0.0.1:3200")
        .env("RETICULA_STATIC_DIR", util::root().join("apps/web/dist"))
        .env("RETICULA_DATA_DIR", &data)
        .env("RETICULA_DATABASE_URL", database)
        .env("RETICULA_PUBLIC_ORIGIN", origin)
        .env("RETICULA_AUTH_MODE", "dev-mock")
        .run()
}

fn server_binary() -> PathBuf {
    util::root().join(format!(
        "target/debug/reticula-server{}",
        std::env::consts::EXE_SUFFIX
    ))
}

/// `cargo xtask stack [--skip-image] [--keep]`: builds the release image,
/// starts `infra/compose.yml` with it and runs `tests/stack` against it.
pub fn stack(flags: &Flags) -> Result {
    if !flags.has("skip-image") {
        util::step("image (infra/Dockerfile)");
        docker()
            .args([
                "build",
                "--file",
                "infra/Dockerfile",
                "--tag",
                STACK_IMAGE,
                ".",
            ])
            .run()?;
    }

    let dir = util::root().join("target/stack");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let env_file = dir.join("stack.env");
    // A throwaway stack on the loopback interface: the password only has to
    // differ between runs. The GitHub application is a placeholder, since
    // the run-through signs its users in through the database.
    let env = format!(
        "RETICULA_IMAGE={STACK_IMAGE}\n\
         PORT=3200\n\
         POSTGRES_PASSWORD={}\n\
         RETICULA_PUBLIC_ORIGIN={STACK_ORIGIN}\n\
         RETICULA_GITHUB_CLIENT_ID=stack-run-through\n\
         RETICULA_GITHUB_CLIENT_SECRET={}\n",
        throwaway_secret(),
        throwaway_secret(),
    );
    fs::write(&env_file, env).map_err(|e| format!("cannot write {}: {e}", env_file.display()))?;
    let compose = |args: &[&str]| {
        docker()
            .args(["compose", "--project-name", STACK_PROJECT])
            .args(["--file", STACK_COMPOSE, "--env-file"])
            .arg(&env_file)
            .args(args)
    };

    util::step("compose up");
    // Whatever an earlier, kept or interrupted run left behind goes first.
    compose(&["down", "--volumes", "--remove-orphans"]).run()?;
    compose(&["up", "--detach", "--wait"]).run()?;

    util::step("playwright (tests/stack)");
    let result = pnpm()
        .args([
            "exec",
            "playwright",
            "test",
            "--config",
            "tests/stack/playwright.config.ts",
        ])
        .env("RETICULA_STACK_URL", STACK_ORIGIN)
        .env("RETICULA_STACK_ENV_FILE", &env_file)
        .env("RETICULA_STACK_PROJECT", STACK_PROJECT)
        .run();

    if flags.has("keep") {
        println!(
            "the stack is still running on {STACK_ORIGIN}; take it down with\n  \
             docker compose --project-name {STACK_PROJECT} --file {STACK_COMPOSE} \
             --env-file {} down --volumes",
            env_file.display()
        );
    } else {
        util::step("compose down");
        compose(&["down", "--volumes"]).run()?;
    }
    result
}

fn throwaway_secret() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    (0..4u64)
        .map(|i| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u128(nanos);
            hasher.write_u64(i);
            format!("{:016x}", hasher.finish())
        })
        .collect()
}
