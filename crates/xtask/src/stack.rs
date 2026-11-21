//! The collaborative server and what it runs on: the development database,
//! a local server for trying collaboration by hand, and the whole service as
//! it ships, brought up in Docker for a run-through.

use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compat::sha256_hex;
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
         RETICULA_GITHUB_CLIENT_SECRET={}\n",
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

fn pipe_compose_stdout(project: &str, env_file: &Path, args: &[&str], out_file: &Path) -> Result {
    let file =
        File::create(out_file).map_err(|e| format!("cannot create {}: {e}", out_file.display()))?;
    let mut cmd = Command::new("docker");
    cmd.current_dir(util::root())
        .args([
            "compose",
            "--project-name",
            project,
            "--file",
            STACK_COMPOSE,
            "--env-file",
        ])
        .arg(env_file)
        .args(args)
        .stdout(Stdio::from(file));
    let status = cmd
        .status()
        .map_err(|e| format!("docker compose pipe failed: {e}"))?;
    if !status.success() {
        return Err(format!(
            "docker compose {} failed ({status})",
            args.join(" ")
        ));
    }
    Ok(())
}

fn pipe_compose_stdin(project: &str, env_file: &Path, args: &[&str], in_file: &Path) -> Result {
    let file =
        File::open(in_file).map_err(|e| format!("cannot open {}: {e}", in_file.display()))?;
    let mut cmd = Command::new("docker");
    cmd.current_dir(util::root())
        .args([
            "compose",
            "--project-name",
            project,
            "--file",
            STACK_COMPOSE,
            "--env-file",
        ])
        .arg(env_file)
        .args(args)
        .stdin(Stdio::from(file));
    let status = cmd
        .status()
        .map_err(|e| format!("docker compose stdin pipe failed: {e}"))?;
    if !status.success() {
        return Err(format!(
            "docker compose {} failed ({status})",
            args.join(" ")
        ));
    }
    Ok(())
}

fn row_counts_report(project: &str, env_file: &Path) -> Result<String> {
    compose_psql(
        project,
        env_file,
        "SELECT 'users=' || count(*) FROM users;
         SELECT 'workbooks=' || count(*) FROM workbooks;
         SELECT 'members=' || count(*) FROM members;
         SELECT 'replicas=' || count(*) FROM replicas;
         SELECT 'branches=' || count(*) FROM branches;
         SELECT 'txns=' || count(*) FROM txns;
         SELECT 'snapshots=' || count(*) FROM snapshots;
         SELECT 'versions=' || count(*) FROM versions;",
    )
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
    util::root().join(format!(
        "target/release/examples/{example}{}",
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

/// `cargo xtask drill [--skip-image]`: end-to-end backup, missing-snapshot
/// window simulation, epoch bump (`0 -> 1`), and `T1`-ahead client recovery drill.
pub fn drill(flags: &Flags) -> Result {
    ensure_stack_image(flags.has("skip-image"))?;
    let driver = build_server_example("drill_driver")?;

    let dir = util::root().join("target/drill");
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("clean {}: {e}", dir.display()))?;
    }
    let backup_dir = dir.join("backup");
    fs::create_dir_all(&backup_dir)
        .map_err(|e| format!("cannot create {}: {e}", backup_dir.display()))?;

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

    util::step("drill: take strict backup at T0 (pg_dump -> workbooks volume)");
    let pgdump = backup_dir.join("reticula.pgdump");
    pipe_compose_stdout(
        DRILL_SRC_PROJECT,
        &src_env,
        &[
            "exec",
            "-T",
            "postgres",
            "pg_dump",
            "--username=reticula",
            "--dbname=reticula",
            "--format=custom",
            "--no-owner",
            "--no-privileges",
        ],
        &pgdump,
    )?;
    let row_counts = row_counts_report(DRILL_SRC_PROJECT, &src_env)?;
    let counts_file = backup_dir.join("row-counts.txt");
    fs::write(&counts_file, format!("{row_counts}\n"))
        .map_err(|e| format!("write {}: {e}", counts_file.display()))?;

    let tar_path = backup_dir.join("workbooks.tar.gz");
    let src_vol = format!("{DRILL_SRC_PROJECT}_workbooks");
    pipe_tar_volume_out(&src_vol, &tar_path)?;

    let pgdump_sha =
        sha256_hex(&fs::read(&pgdump).map_err(|e| format!("read {}: {e}", pgdump.display()))?);
    let tar_sha =
        sha256_hex(&fs::read(&tar_path).map_err(|e| format!("read {}: {e}", tar_path.display()))?);

    util::step("drill: advance primary past T0 -> T1 + offline pending edits");
    Cmd::new(&driver)
        .args(["--phase", "advance-t1", "--base-url", src_url, "--state"])
        .arg(&state_path)
        .arg("--alice-state")
        .arg(&alice_state_path)
        .run()?;
    src_guard.down_now()?;

    util::step("drill: restore T0 backup onto isolated destination stack");
    let check_pgdump =
        sha256_hex(&fs::read(&pgdump).map_err(|e| format!("re-read {}: {e}", pgdump.display()))?);
    let check_tar = sha256_hex(
        &fs::read(&tar_path).map_err(|e| format!("re-read {}: {e}", tar_path.display()))?,
    );
    if check_pgdump != pgdump_sha || check_tar != tar_sha {
        return Err(String::from("backup artifact SHA-256 verification failed"));
    }

    compose_cmd(DRILL_DST_PROJECT, &dst_env, &["up", "--no-start"]).run()?;
    compose_cmd(
        DRILL_DST_PROJECT,
        &dst_env,
        &["up", "--detach", "--wait", "postgres"],
    )
    .run()?;

    pipe_compose_stdin(
        DRILL_DST_PROJECT,
        &dst_env,
        &[
            "exec",
            "-T",
            "postgres",
            "pg_restore",
            "--username=reticula",
            "--dbname=reticula",
            "--clean",
            "--if-exists",
            "--no-owner",
            "--no-privileges",
            "--exit-on-error",
        ],
        &pgdump,
    )?;

    let dst_vol = format!("{DRILL_DST_PROJECT}_workbooks");
    pipe_tar_volume_in(&dst_vol, &tar_path)?;

    let dst_counts = row_counts_report(DRILL_DST_PROJECT, &dst_env)?;
    if dst_counts != row_counts {
        return Err(format!(
            "restored table row counts differ:\nexpected:\n{row_counts}\ngot:\n{dst_counts}"
        ));
    }

    verify_all_volume_snapshots(DRILL_DST_PROJECT, &dst_env, &dst_vol)?;

    // Simulate a seq > 0 snapshot file committed during live pg_dump -> tar window.
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
    let omitted_key = format!("snapshots/{wb_a}/{snap_seq}.rtclsnap");
    docker()
        .args(["run", "--rm", "-v"])
        .arg(format!("{dst_vol}:/data"))
        .args([
            POSTGRES_UTIL_IMAGE,
            "rm",
            "-f",
            &format!("/data/{omitted_key}"),
        ])
        .run()?;

    let pruned = prune_missing_snapshots(DRILL_DST_PROJECT, &dst_env, &dst_vol)?;
    if pruned != 1 {
        return Err(format!(
            "expected exactly 1 backup-window snapshot row pruned for {omitted_key}, got {pruned}"
        ));
    }

    let saturated = compose_psql(
        DRILL_DST_PROJECT,
        &dst_env,
        "SELECT count(*) FROM workbooks WHERE log_epoch >= 255;",
    )?;
    if saturated != "0" {
        return Err(format!("log_epoch saturated on {saturated} workbook(s)"));
    }
    compose_psql(
        DRILL_DST_PROJECT,
        &dst_env,
        "UPDATE workbooks SET log_epoch = log_epoch + 1, next_per_epoch_replica = 1, updated_at = now();",
    )?;

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
    Ok(())
}

fn pipe_tar_volume_out(volume: &str, out_file: &Path) -> Result {
    let file =
        File::create(out_file).map_err(|e| format!("cannot create {}: {e}", out_file.display()))?;
    let status = Command::new("docker")
        .current_dir(util::root())
        .args(["run", "--rm", "-v"])
        .arg(format!("{volume}:/data:ro"))
        .args([POSTGRES_UTIL_IMAGE, "tar", "-czf", "-", "-C", "/data", "."])
        .stdout(Stdio::from(file))
        .status()
        .map_err(|e| format!("docker tar out failed: {e}"))?;
    if !status.success() {
        return Err(format!("tar backup of {volume} failed ({status})"));
    }
    Ok(())
}

fn pipe_tar_volume_in(volume: &str, in_file: &Path) -> Result {
    let file =
        File::open(in_file).map_err(|e| format!("cannot open {}: {e}", in_file.display()))?;
    let status = Command::new("docker")
        .current_dir(util::root())
        .args(["run", "--rm", "-i", "-v"])
        .arg(format!("{volume}:/data"))
        .args([
            POSTGRES_UTIL_IMAGE,
            "sh",
            "-c",
            "find /data -mindepth 1 -delete && tar -xzf - -C /data && chown -R 10001:10001 /data",
        ])
        .stdin(Stdio::from(file))
        .status()
        .map_err(|e| format!("docker tar in failed: {e}"))?;
    if !status.success() {
        return Err(format!("tar restore of {volume} failed ({status})"));
    }
    Ok(())
}

fn volume_snapshot_hashes(volume: &str) -> Result<std::collections::BTreeMap<String, String>> {
    let output = docker()
        .args(["run", "--rm", "-v"])
        .arg(format!("{volume}:/data:ro"))
        .args([
            POSTGRES_UTIL_IMAGE,
            "sh",
            "-c",
            "cd /data && find snapshots -type f -name '*.rtclsnap' 2>/dev/null | sort | xargs -r sha256sum",
        ])
        .output()?;
    let mut hashes = std::collections::BTreeMap::new();
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(path)) = (parts.next(), parts.next()) else {
            continue;
        };
        hashes.insert(path.trim_start_matches("./").to_owned(), digest.to_owned());
    }
    Ok(hashes)
}

struct DbSnapshotRow {
    wb: String,
    branch: String,
    seq: u64,
    key: String,
    expected_sha: String,
}

fn db_snapshot_rows(project: &str, env_file: &Path) -> Result<Vec<DbSnapshotRow>> {
    let raw = compose_psql(
        project,
        env_file,
        "SELECT workbook_id || '|' || branch_id || '|' || seq || '|' || storage_key || '|' || encode(sha256, 'hex')
         FROM snapshots ORDER BY workbook_id, seq;",
    )?;
    let mut rows = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() != 5 {
            return Err(format!("unexpected snapshot row format: `{line}`"));
        }
        let seq = parts[2]
            .parse::<u64>()
            .map_err(|e| format!("parse seq `{}`: {e}", parts[2]))?;
        rows.push(DbSnapshotRow {
            wb: parts[0].to_owned(),
            branch: parts[1].to_owned(),
            seq,
            key: parts[3].to_owned(),
            expected_sha: parts[4].to_owned(),
        });
    }
    Ok(rows)
}

fn verify_all_volume_snapshots(project: &str, env_file: &Path, volume: &str) -> Result {
    let disk_hashes = volume_snapshot_hashes(volume)?;
    for row in db_snapshot_rows(project, env_file)? {
        let Some(actual) = disk_hashes.get(&row.key) else {
            return Err(format!(
                "missing snapshot file `{}` during full backup check",
                row.key
            ));
        };
        if actual != &row.expected_sha {
            return Err(format!(
                "snapshot hash mismatch for `{}`: expected {}, got {actual}",
                row.key, row.expected_sha
            ));
        }
    }
    Ok(())
}

fn prune_missing_snapshots(project: &str, env_file: &Path, volume: &str) -> Result<usize> {
    let disk_hashes = volume_snapshot_hashes(volume)?;
    let mut pruned = 0_usize;
    for row in db_snapshot_rows(project, env_file)? {
        if let Some(actual) = disk_hashes.get(&row.key) {
            if actual != &row.expected_sha {
                return Err(format!(
                    "snapshot hash mismatch for `{}`: expected {}, got {actual}",
                    row.key, row.expected_sha
                ));
            }
            continue;
        }
        if row.seq == 0 {
            return Err(format!("fatal: seq=0 base snapshot `{}` missing", row.key));
        }
        let sql = format!(
            "BEGIN;
             WITH removed AS (
               DELETE FROM snapshots
               WHERE workbook_id = '{}'::uuid AND branch_id = '{}'::uuid AND seq = {}
               RETURNING workbook_id, size_bytes
             )
             UPDATE users u
             SET storage_bytes = GREATEST(0, u.storage_bytes - r.size_bytes)
             FROM workbooks w
             JOIN removed r ON r.workbook_id = w.id
             WHERE u.id = w.owner_id;
             COMMIT;",
            row.wb, row.branch, row.seq
        );
        compose_psql(project, env_file, &sql)?;
        pruned = pruned.saturating_add(1);
    }
    Ok(pruned)
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
