//! The collaborative server and what it runs on: the development database,
//! a local server for trying collaboration by hand, and the whole service as
//! it ships, brought up in Docker for a run-through.

use std::fs;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::util::{self, Cmd, Flags, Result, cargo, pnpm};
use crate::web;

const DEV_COMPOSE: &str = "infra/compose.dev.yml";
const STACK_COMPOSE: &str = "infra/compose.yml";
const STACK_PROJECT: &str = "reticula-stack";
const STACK_IMAGE: &str = "reticula:stack";
const STACK_ORIGIN: &str = "http://127.0.0.1:3200";
const POSTGRES_UTIL_IMAGE: &str = "postgres:18.7-bookworm";
const DRILL_SRC_PROJECT: &str = "reticula-drill-src";
const DRILL_DST_PROJECT: &str = "reticula-drill-dst";
const LOADTEST_PROJECT: &str = "reticula-loadtest";

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
    util::target_dir().join(format!(
        "debug/reticula-server{}",
        std::env::consts::EXE_SUFFIX
    ))
}

struct ComposeGuard {
    project: String,
    env_file: PathBuf,
    active: bool,
}

impl ComposeGuard {
    fn new(project: &str, env_file: PathBuf) -> Self {
        Self {
            project: project.to_owned(),
            env_file,
            active: true,
        }
    }

    fn down_now(&mut self) -> Result {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        compose_cmd(
            &self.project,
            &self.env_file,
            &["down", "--volumes", "--remove-orphans"],
        )
        .run()
    }
}

impl Drop for ComposeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = Command::new("docker")
                .current_dir(util::root())
                .args([
                    "compose",
                    "--project-name",
                    &self.project,
                    "--file",
                    STACK_COMPOSE,
                    "--env-file",
                ])
                .arg(&self.env_file)
                .args(["down", "--volumes", "--remove-orphans"])
                .status();
        }
    }
}

pub(crate) fn ensure_stack_image(skip_image: bool) -> Result {
    if skip_image {
        return Ok(());
    }
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
        .run()
}

pub(crate) fn write_compose_env(
    dir: &Path,
    filename: &str,
    port: u16,
    metrics_port: u16,
    origin: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let env_file = dir.join(filename);
    let env = format!(
        "RETICULA_IMAGE={STACK_IMAGE}\n\
         PORT={port}\n\
         METRICS_PORT={metrics_port}\n\
         POSTGRES_PASSWORD={}\n\
         RETICULA_PUBLIC_ORIGIN={origin}\n\
         RETICULA_GITHUB_CLIENT_ID=stack-run-through\n\
         RETICULA_GITHUB_CLIENT_SECRET={}\n\
         RETICULA_LLM_MODE=mock\n",
        throwaway_secret(),
        throwaway_secret(),
    );
    fs::write(&env_file, env).map_err(|e| format!("cannot write {}: {e}", env_file.display()))?;
    Ok(env_file)
}

pub(crate) fn compose_cmd(project: &str, env_file: &Path, args: &[&str]) -> Cmd {
    docker()
        .args(["compose", "--project-name", project])
        .args(["--file", STACK_COMPOSE, "--env-file"])
        .arg(env_file)
        .args(args)
}

fn compose_psql(project: &str, env_file: &Path, sql: &str) -> Result<String> {
    compose_cmd(
        project,
        env_file,
        &[
            "exec",
            "-T",
            "postgres",
            "psql",
            "--username=reticula",
            "--dbname=reticula",
            "--set=ON_ERROR_STOP=1",
            "--tuples-only",
            "--no-align",
            "--command",
            sql,
        ],
    )
    .output()
    .map(|s| s.trim().to_owned())
}

/// `cargo xtask stack [--skip-image] [--keep]`: builds the release image,
/// starts `infra/compose.yml` with it and runs `tests/stack` against it.
pub fn stack(flags: &Flags) -> Result {
    ensure_stack_image(flags.has("skip-image"))?;

    let dir = util::root().join("target/stack");
    let env_file = write_compose_env(&dir, "stack.env", 3200, 3201, STACK_ORIGIN)?;
    let compose = |args: &[&str]| compose_cmd(STACK_PROJECT, &env_file, args);

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

fn driver_binary(example: &str) -> PathBuf {
    util::target_dir().join(format!(
        "release/examples/{example}{}",
        std::env::consts::EXE_SUFFIX
    ))
}

pub(crate) fn build_server_example(example: &str) -> Result<PathBuf> {
    util::step(&format!("build {example} (release)"));
    cargo()
        .env("SQLX_OFFLINE", "true")
        .args([
            "build",
            "--locked",
            "--release",
            "--package",
            "reticula-server",
            "--example",
            example,
        ])
        .run()?;
    Ok(driver_binary(example))
}

/// `cargo xtask drill [--skip-image]`: a backup, the monitor and a restore as the
/// scripts in `infra/ops` do them, with a snapshot and a table that are not in the
/// backup, an epoch bump (`0 -> 1`), and the recovery of a client ahead at `T1`.
pub fn drill(flags: &Flags) -> Result {
    let shell = OpsShell::find()?;
    ensure_stack_image(flags.has("skip-image"))?;
    let driver = build_server_example("drill_driver")?;

    let dir = util::root().join("target/drill");
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("clean {}: {e}", dir.display()))?;
    }

    let src_url = "http://127.0.0.1:3210";
    let src_metrics = "http://127.0.0.1:3211";
    let dst_url = "http://127.0.0.1:3212";
    let dst_metrics = "http://127.0.0.1:3213";
    let src_env = write_compose_env(&dir, "src.env", 3210, 3211, src_url)?;
    let dst_env = write_compose_env(&dir, "dst.env", 3212, 3213, dst_url)?;
    let state_path = dir.join("state.json");
    let alice_state_path = dir.join("alice-t1.bin");

    compose_cmd(
        DRILL_SRC_PROJECT,
        &src_env,
        &["down", "--volumes", "--remove-orphans"],
    )
    .run()?;
    compose_cmd(
        DRILL_DST_PROJECT,
        &dst_env,
        &["down", "--volumes", "--remove-orphans"],
    )
    .run()?;

    let mut src_guard = ComposeGuard::new(DRILL_SRC_PROJECT, src_env.clone());
    let mut dst_guard = ComposeGuard::new(DRILL_DST_PROJECT, dst_env.clone());

    util::step("drill: start primary stack (T0)");
    compose_cmd(DRILL_SRC_PROJECT, &src_env, &["up", "--detach", "--wait"]).run()?;

    Cmd::new(&driver)
        .args([
            "--phase",
            "prepare-t0",
            "--base-url",
            src_url,
            "--metrics-url",
            src_metrics,
            "--state",
        ])
        .arg(&state_path)
        .args(["--project", DRILL_SRC_PROJECT, "--env-file"])
        .arg(&src_env)
        .run()?;

    util::step("drill: back up at T0 (infra/ops/backup.sh)");
    let state_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&state_path).map_err(|e| format!("read state: {e}"))?,
    )
    .map_err(|e| format!("parse state: {e}"))?;
    let wb_a = state_json["wb_a"]
        .as_str()
        .ok_or_else(|| String::from("missing wb_a in state.json"))?;
    let snap_seq = state_json["t0_snapshot_seq_a"]
        .as_u64()
        .ok_or_else(|| String::from("missing t0_snapshot_seq_a in state.json"))?;
    let lost_sha = state_json["lost_table"]["sha256"]
        .as_str()
        .ok_or_else(|| String::from("missing lost_table in state.json"))?;
    shell.run(
        &dir.join("backup.sh"),
        &format!(
            r#"rm -rf "$WORK"
mkdir -p "$WORK"
export COMPOSE_PROJECT={DRILL_SRC_PROJECT} ENV_FILE='{env_file}' BACKUP_ROOT="$WORK/backups"
volume={DRILL_SRC_PROJECT}_workbooks
key=snapshots/{wb_a}/{snap_seq}.rtclsnap
# As if it had been written between the dump and the archive.
docker run --rm -v "$volume:/data:ro" {POSTGRES_UTIL_IMAGE} cat "/data/$key" > "$WORK/snapshot" < /dev/null
docker run --rm -v "$volume:/data" {POSTGRES_UTIL_IMAGE} rm "/data/$key" < /dev/null
bash infra/ops/backup.sh
docker run --rm -i -v "$volume:/data" {POSTGRES_UTIL_IMAGE} \
  sh -c "cat > '/data/$key' && chown 10001:10001 '/data/$key'" < "$WORK/snapshot"
# How full the disk and the memory of this machine are is not what it is for.
RETICULA_URL={src_url} METRICS_URL={src_metrics}/metrics DISK_WARN_PCT=101 MEM_WARN_PCT=101 \
  bash infra/ops/monitor.sh
"#,
            env_file = shell.path(&src_env),
        ),
    )?;

    util::step("drill: advance primary past T0 -> T1 + offline pending edits");
    Cmd::new(&driver)
        .args(["--phase", "advance-t1", "--base-url", src_url, "--state"])
        .arg(&state_path)
        .arg("--alice-state")
        .arg(&alice_state_path)
        .run()?;
    src_guard.down_now()?;

    util::step("drill: restore onto an isolated destination stack (infra/ops/restore.sh)");
    shell.run(
        &dir.join("restore.sh"),
        &format!(
            r#"export COMPOSE_PROJECT={DRILL_DST_PROJECT} ENV_FILE='{env_file}'
# One copy of a table did not make it.
sha={lost_sha}
rm "$WORK/backups-tables/${{sha:0:2}}/$sha.rtbl"
bash infra/ops/restore.sh "$WORK"/backups/*
"#,
            env_file = shell.path(&dst_env),
        ),
    )?;
    let left = compose_psql(
        DRILL_DST_PROJECT,
        &dst_env,
        &format!(
            "SELECT count(*) FROM snapshots WHERE workbook_id = '{wb_a}' AND seq = {snap_seq};"
        ),
    )?;
    if left != "0" {
        return Err(format!(
            "expected the row of the snapshot that is not in the archive to be forgotten, got {left}"
        ));
    }

    compose_cmd(DRILL_DST_PROJECT, &dst_env, &["up", "--detach", "--wait"]).run()?;

    util::step("drill: verify T1-ahead client reconnect, epoch bump, and convergence");
    Cmd::new(&driver)
        .args([
            "--phase",
            "verify-restore",
            "--base-url",
            dst_url,
            "--metrics-url",
            dst_metrics,
            "--state",
        ])
        .arg(&state_path)
        .arg("--alice-state")
        .arg(&alice_state_path)
        .run()?;

    util::step("drill: teardown destination stack");
    dst_guard.down_now()?;
    shell.run(&dir.join("clean.sh"), "rm -rf \"$WORK\"\n")
}

/// The shell the scripts in `infra/ops` run in: bash, with Docker.
struct OpsShell {
    command: &'static [&'static str],
}

impl OpsShell {
    fn find() -> Result<Self> {
        // The bash of Git changes the paths it hands to Docker.
        let command: &[&str] = if cfg!(windows) {
            &["wsl", "-e", "bash"]
        } else {
            &["bash"]
        };
        let (program, args) = (command[0], &command[1..]);
        Cmd::new(program)
            .args(args)
            .args(["-c", "docker version"])
            .output()
            .map(|_| Self { command })
            .map_err(|error| {
                format!(
                    "the scripts in infra/ops need bash and Docker (on Windows: a WSL distribution \
                     with the integration of Docker Desktop): {error}"
                )
            })
    }

    /// A path as the shell has it.
    fn path(&self, path: &Path) -> String {
        let path = path.to_string_lossy().replace('\\', "/");
        match path.split_once(":/") {
            Some((drive, rest)) if cfg!(windows) => {
                format!("/mnt/{}/{rest}", drive.to_lowercase())
            }
            _ => path,
        }
    }

    /// Runs a script in the repository. `$WORK` is a directory for what it keeps,
    /// on a file system that has owners and modes.
    fn run(&self, file: &Path, script: &str) -> Result {
        let text = format!(
            "set -euo pipefail\ncd '{}'\nWORK=\"${{TMPDIR:-/tmp}}/reticula-drill\"\n{script}",
            self.path(&util::root())
        );
        fs::write(file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        Cmd::new(self.command[0])
            .args(&self.command[1..])
            .args([self.path(file)])
            .run()
    }
}

/// `cargo xtask loadtest [--skip-image] [--out <file>]`: starts the resource-limited
/// compose stack (`768m`/1.0 CPU server, `768m`/0.5 CPU Postgres), imports a
/// ~1.95M-cell (~60 MiB) workbook, and drives 50 concurrent WebSocket clients at 100 txns/s.
pub fn loadtest(flags: &Flags) -> Result {
    ensure_stack_image(flags.has("skip-image"))?;
    let driver = build_server_example("loadtest_driver")?;

    let dir = util::root().join("target/loadtest");
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("clean {}: {e}", dir.display()))?;
    }
    let url = "http://127.0.0.1:3220";
    let metrics_url = "http://127.0.0.1:3221";
    let env_file = write_compose_env(&dir, "loadtest.env", 3220, 3221, url)?;

    let commit = crate::qa::git_output(&["rev-parse", "--short", "HEAD"]);
    let date = crate::qa::current_utc_date();
    let out_path = match flags.get("out") {
        Some(custom) => util::root().join(custom),
        None => util::root()
            .join("bench/results")
            .join(format!("{date}-{commit}-load.json")),
    };

    compose_cmd(
        LOADTEST_PROJECT,
        &env_file,
        &["down", "--volumes", "--remove-orphans"],
    )
    .run()?;
    let mut guard = ComposeGuard::new(LOADTEST_PROJECT, env_file.clone());

    util::step("loadtest: start resource-limited compose stack (768m server, 768m postgres)");
    compose_cmd(LOADTEST_PROJECT, &env_file, &["up", "--detach", "--wait"]).run()?;

    util::step("loadtest: 50 concurrent clients at 100 txns/s (5s warmup + 60s measured)");
    let res = Cmd::new(&driver)
        .args([
            "--base-url",
            url,
            "--metrics-url",
            metrics_url,
            "--project",
            LOADTEST_PROJECT,
            "--env-file",
        ])
        .arg(&env_file)
        .arg("--out")
        .arg(&out_path)
        .args(["--commit", &commit, "--date", &date])
        .run();

    util::step("loadtest: teardown compose stack");
    let down_res = guard.down_now();
    res?;
    down_res
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
