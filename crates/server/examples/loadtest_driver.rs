//! 50-client concurrency load-test driver invoked by `cargo xtask loadtest`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use rand::Rng as _;
use reqwest::{StatusCode, header};
use reticula_core::id::{ColId, IdAllocator, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::state_digest::StateDigest;
use reticula_core::value::Value as CellValue;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{ClientAction, ClientSyncMachine, CommittedPermute, ReplicaWatermark};
use reticula_wire::snapshot::{
    decode_snapshot_any, decode_snapshot_for_collab, encode_snapshot_v1,
};
use reticula_wire::sync_frame::{decode_sync_frame_v2, encode_sync_frame_v2};
use reticula_wire::v3::{WireCommittedTxnV3, WireSyncFrameV2};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::{Barrier, watch};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type AppResult<T> = Result<T, String>;

const MAIN_BRANCH: Uuid = Uuid::nil();
const CLIENT_COUNT: usize = 50;
const SEND_INTERVAL_MS: u64 = 500;
const CLIENT_STAGGER_MS: u64 = 10;
const WARMUP_STEPS: usize = 10;
const MEASURED_STEPS: usize = 120;
const TOTAL_STEPS: usize = WARMUP_STEPS + MEASURED_STEPS;
const REFERENCE_RTT_MS: f64 = 50.0;
const GATE_RTT50_P95_MS: f64 = 150.0;
const SERVER_MEM_LIMIT_MIB: f64 = 768.0;
const GATE_SERVER_MEM_MIB: f64 = 768.0 * 0.70;
const IO_TIMEOUT: Duration = Duration::from_secs(60);

struct Args {
    base_url: String,
    metrics_url: String,
    project: String,
    env_file: PathBuf,
    out_path: PathBuf,
    commit: String,
    date: String,
}

impl Args {
    fn parse() -> AppResult<Self> {
        let mut base_url = None;
        let mut metrics_url = None;
        let mut project = None;
        let mut env_file = None;
        let mut out_path = None;
        let mut commit = None;
        let mut date = None;

        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            let val = iter
                .next()
                .ok_or_else(|| format!("missing value after `{arg}`"))?;
            match arg.as_str() {
                "--base-url" => base_url = Some(val),
                "--metrics-url" => metrics_url = Some(val),
                "--project" => project = Some(val),
                "--env-file" => env_file = Some(PathBuf::from(val)),
                "--out" => out_path = Some(PathBuf::from(val)),
                "--commit" => commit = Some(val),
                "--date" => date = Some(val),
                other => return Err(format!("unknown flag `{other}`")),
            }
        }
        Ok(Self {
            base_url: base_url.ok_or("--base-url is required")?,
            metrics_url: metrics_url.ok_or("--metrics-url is required")?,
            project: project.ok_or("--project is required")?,
            env_file: env_file.ok_or("--env-file is required")?,
            out_path: out_path.ok_or("--out is required")?,
            commit: commit.unwrap_or_else(|| String::from("unknown")),
            date: date.unwrap_or_else(|| String::from("2026-11-21")),
        })
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("loadtest_driver failed: {err}");
        std::process::exit(1);
    }
}

struct BuiltDoc {
    snap_bytes: Vec<u8>,
    base_wb: Workbook<BoundFormula>,
    populated_cells: u64,
    estimated_bytes: usize,
}

fn make_string_sheet(
    id: SheetId,
    name: &str,
    rows: u32,
    pad_repeats: usize,
    extra_num_rows: u32,
) -> AppResult<Sheet> {
    let pad = "abcdefghij".repeat(pad_repeats);
    let mut sheet = Sheet::standard(id, String::from(name));
    for r in 0..rows {
        let sid = sheet
            .add_string(&format!("lt-{}-{r:05}-{pad}", id.0))
            .ok_or_else(|| format!("add_string failed on {} row {r}", id.0))?;
        if !sheet.set_value_by_id(RowId(u64::from(r)), ColId(0), CellValue::Text(sid)) {
            return Err(format!("set_value_by_id failed on {} row {r}", id.0));
        }
    }
    for r in 0..extra_num_rows {
        if !sheet.set_value_by_id(
            RowId(u64::from(r)),
            ColId(1),
            CellValue::Number(f64::from(r)),
        ) {
            return Err(format!("set_value_by_id num failed on {} row {r}", id.0));
        }
    }
    Ok(sheet)
}

fn build_large_workbook() -> AppResult<BuiltDoc> {
    const ROWS: u32 = 100_000;
    const NUM_COLS: u32 = 17;
    const STR_ROWS_S1: u32 = 50_000;
    let pad1 = "abcdefghij".repeat(65);
    let mut sheet1 = Sheet::standard(SheetId(0), String::from("Sheet1"));

    for col in 0..NUM_COLS {
        for r in 0..ROWS {
            if !sheet1.set_value_by_id(
                RowId(u64::from(r)),
                ColId(u64::from(col)),
                CellValue::Number(f64::from(r + col)),
            ) {
                return Err(format!("set_value_by_id failed at ({r}, {col})"));
            }
        }
    }

    for r in 0..STR_ROWS_S1 {
        let sid = sheet1
            .add_string(&format!("lt-0-{r:05}-{pad1}"))
            .ok_or_else(|| format!("add_string failed at {r}"))?;
        if !sheet1.set_value_by_id(
            RowId(u64::from(r)),
            ColId(u64::from(NUM_COLS)),
            CellValue::Text(sid),
        ) {
            return Err(format!("set_value_by_id text failed at ({r}, {NUM_COLS})"));
        }
    }

    // Pack each sheet's StringTable near its power-of-two ceiling (32 / 8 / 4 MiB)
    // so both decode capacity and cloned estimated_bytes land cleanly in 60..=64 MiB.
    let sheet2 = make_string_sheet(SheetId(1), "Sheet2", 15_000, 54, 150_000)?;
    let sheet3 = make_string_sheet(SheetId(2), "Sheet3", 7_500, 54, 54_500)?;

    let wb: Workbook<BoundFormula> = Workbook::from_parts(
        vec![sheet1, sheet2, sheet3],
        BTreeMap::new(),
        IdAllocator::local(),
        3,
        None,
    )
    .map_err(|e| format!("from_parts: {e:?}"))?;
    let snap_bytes = encode_snapshot_v1(&wb, &[]).map_err(|e| format!("encode snap: {e:?}"))?;
    let any = decode_snapshot_any(&snap_bytes).map_err(|e| format!("decode_any: {e:?}"))?;
    println!(
        "decoded_any: {} cells, {:.2} MiB est_bytes (pre-decode {:.2} MiB, snap {:.2} MiB)",
        any.engine.workbook().populated_cell_count(),
        any.engine.workbook().estimated_bytes() as f64 / (1024.0 * 1024.0),
        wb.estimated_bytes() as f64 / (1024.0 * 1024.0),
        snap_bytes.len() as f64 / (1024.0 * 1024.0),
    );
    let decoded = decode_snapshot_for_collab(&snap_bytes, 2_000_000, 64 * 1024 * 1024)
        .map_err(|e| format!("verify collab limits: {e:?}"))?;
    let base_wb = decoded.engine.workbook().clone();
    let populated_cells = base_wb.populated_cell_count();
    let estimated_bytes = base_wb.estimated_bytes();

    if !(1_900_000..=2_000_000).contains(&populated_cells) {
        return Err(format!(
            "unexpected populated cell count: {populated_cells}"
        ));
    }
    if !(60 * 1024 * 1024..=64 * 1024 * 1024).contains(&estimated_bytes) {
        return Err(format!(
            "unexpected estimated_bytes: {estimated_bytes} ({:.2} MiB; expected 60..=64 MiB)",
            estimated_bytes as f64 / (1024.0 * 1024.0)
        ));
    }
    Ok(BuiltDoc {
        snap_bytes,
        base_wb,
        populated_cells,
        estimated_bytes,
    })
}

fn seed_users_and_sessions(
    project: &str,
    env_file: &Path,
    count: usize,
) -> AppResult<Vec<(Uuid, String)>> {
    let mut sql = String::from("BEGIN;\n");
    let mut users = Vec::with_capacity(count);
    for idx in 0..count {
        let mut secret = [0_u8; 32];
        rand::rng().fill_bytes(&mut secret);
        let mut hash_hex = String::with_capacity(64);
        for byte in Sha256::digest(secret) {
            let _ = write!(hash_hex, "{byte:02x}");
        }
        let id = Uuid::new_v4();
        let _ = writeln!(
            sql,
            "INSERT INTO users (id, name, avatar_url) VALUES ('{id}', 'LoadClient-{idx:02}', '');
             INSERT INTO sessions (token_hash, user_id, expires_at)
               VALUES ('\\x{hash_hex}', '{id}', now() + interval '1 day');"
        );
        users.push((
            id,
            format!("reticula_session={}", URL_SAFE_NO_PAD.encode(secret)),
        ));
    }
    sql.push_str("COMMIT;");
    psql_exec(project, env_file, &sql)?;
    Ok(users)
}

fn add_members_sql(project: &str, env_file: &Path, doc: Uuid, user_ids: &[Uuid]) -> AppResult<()> {
    let mut sql = String::from("BEGIN;\n");
    for id in user_ids {
        let _ = writeln!(
            sql,
            "INSERT INTO members (workbook_id, user_id, role) VALUES ('{doc}', '{id}', 'editor') ON CONFLICT DO NOTHING;"
        );
    }
    sql.push_str("COMMIT;");
    psql_exec(project, env_file, &sql)?;
    Ok(())
}

fn psql_exec(project: &str, env_file: &Path, sql: &str) -> AppResult<String> {
    let out = Command::new("docker")
        .args([
            "compose",
            "--project-name",
            project,
            "--file",
            "infra/compose.yml",
            "--env-file",
        ])
        .arg(env_file)
        .args([
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
        ])
        .output()
        .map_err(|e| format!("docker compose psql: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "psql failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn wait_endpoints(
    http: &reqwest::Client,
    base_url: &str,
    metrics_url: &str,
) -> AppResult<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(resp) = http.get(format!("{base_url}/readyz")).send().await
            && resp.status() == StatusCode::OK
            && let Ok(mresp) = http.get(format!("{metrics_url}/metrics")).send().await
            && mresp.status() == StatusCode::OK
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {base_url}/readyz and {metrics_url}/metrics"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct SharedState {
    origin: Instant,
    load_origin: OnceLock<tokio::time::Instant>,
    send_times_ns: Vec<Vec<AtomicU64>>,
    rep_map: Mutex<HashMap<u32, usize>>,
    base_client_seq: Mutex<Vec<u64>>,
    settled_count: AtomicUsize,
    disconnects: AtomicUsize,
    digest_mismatches: AtomicUsize,
    server_digest: Mutex<Option<StateDigest>>,
    client_digests: Mutex<Vec<Option<StateDigest>>>,
}

#[derive(Default)]
struct ResourceSummary {
    server_peak_mib: f64,
    postgres_peak_mib: f64,
    max_loaded_bytes: u64,
    snapshots_written: u64,
    snapshots_pruned: u64,
    sample_count: usize,
}

fn parse_mem_mib(mem_usage: &str) -> Option<f64> {
    let used = mem_usage.split('/').next()?.trim();
    if let Some(val) = used.strip_suffix("GiB") {
        val.trim().parse::<f64>().ok().map(|v| v * 1024.0)
    } else if let Some(val) = used.strip_suffix("MiB") {
        val.trim().parse::<f64>().ok()
    } else if let Some(val) = used.strip_suffix("KiB") {
        val.trim().parse::<f64>().ok().map(|v| v / 1024.0)
    } else if let Some(val) = used.strip_suffix('B') {
        val.trim()
            .parse::<f64>()
            .ok()
            .map(|v| v / (1024.0 * 1024.0))
    } else {
        None
    }
}

fn scrape_prometheus_gauge(text: &str, name: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(name)
            && (rest.starts_with(' ') || rest.starts_with('{'))
            && let Some(val_str) = line.split_whitespace().last()
            && let Ok(val) = val_str.parse::<u64>()
        {
            return Some(val);
        }
    }
    None
}

async fn sample_resources(
    project: String,
    env_file: PathBuf,
    metrics_url: String,
    http: reqwest::Client,
    stop: watch::Receiver<bool>,
) -> ResourceSummary {
    let mut summary = ResourceSummary::default();
    let ids_raw = Command::new("docker")
        .args([
            "compose",
            "--project-name",
            &project,
            "--file",
            "infra/compose.yml",
            "--env-file",
        ])
        .arg(&env_file)
        .args(["ps", "--quiet"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let container_ids: Vec<String> = ids_raw.split_whitespace().map(str::to_owned).collect();

    loop {
        if !container_ids.is_empty() {
            let mut cmd = Command::new("docker");
            cmd.args([
                "stats",
                "--no-stream",
                "--format",
                "{{.Name}}\t{{.MemUsage}}",
            ]);
            for id in &container_ids {
                cmd.arg(id);
            }
            if let Ok(out) = cmd.output()
                && out.status.success()
            {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    let mut parts = line.split('\t');
                    let (Some(name), Some(usage)) = (parts.next(), parts.next()) else {
                        continue;
                    };
                    if let Some(mib) = parse_mem_mib(usage) {
                        if name.contains("server") {
                            summary.server_peak_mib = summary.server_peak_mib.max(mib);
                        } else if name.contains("postgres") {
                            summary.postgres_peak_mib = summary.postgres_peak_mib.max(mib);
                        }
                    }
                }
                summary.sample_count = summary.sample_count.saturating_add(1);
            }
        }
        if let Ok(resp) = http.get(format!("{metrics_url}/metrics")).send().await
            && let Ok(text) = resp.text().await
        {
            if let Some(v) = scrape_prometheus_gauge(&text, "reticula_loaded_bytes") {
                summary.max_loaded_bytes = summary.max_loaded_bytes.max(v);
            }
            if let Some(v) = scrape_prometheus_gauge(&text, "reticula_snapshots_written_total") {
                summary.snapshots_written = summary.snapshots_written.max(v);
            }
            if let Some(v) = scrape_prometheus_gauge(&text, "reticula_snapshots_pruned_total") {
                summary.snapshots_pruned = summary.snapshots_pruned.max(v);
            }
        }
        if *stop.borrow() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    summary
}

async fn run() -> AppResult<()> {
    let args = Args::parse()?;
    let http = reqwest::Client::builder()
        .timeout(IO_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    wait_endpoints(&http, &args.base_url, &args.metrics_url).await?;

    println!("building ~1.95M-cell (~60 MiB) benchmark workbook...");
    let built = tokio::task::spawn_blocking(build_large_workbook)
        .await
        .map_err(|e| format!("build workbook task: {e}"))??;
    let snap_bytes = Arc::new(built.snap_bytes);
    let base_wb = Arc::new(built.base_wb);
    println!(
        "workbook ready: {} cells, {:.2} MiB est_bytes, {:.2} MiB compressed snapshot",
        built.populated_cells,
        built.estimated_bytes as f64 / (1024.0 * 1024.0),
        snap_bytes.len() as f64 / (1024.0 * 1024.0),
    );

    let users = seed_users_and_sessions(&args.project, &args.env_file, CLIENT_COUNT)?;
    let owner_cookie = &users[0].1;

    let import_resp = http
        .post(format!("{}/api/workbooks/import", args.base_url))
        .header(header::ORIGIN, &args.base_url)
        .header(header::COOKIE, owner_cookie)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("X-Workbook-Title", "LoadTest-2M")
        .body(snap_bytes.as_ref().clone())
        .send()
        .await
        .map_err(|e| format!("import workbook: {e}"))?;
    if import_resp.status() != StatusCode::CREATED {
        return Err(format!("import failed: {}", import_resp.status()));
    }
    let import_json: Value = import_resp
        .json()
        .await
        .map_err(|e| format!("import json: {e}"))?;
    let doc: Uuid = import_json["id"]
        .as_str()
        .ok_or("missing workbook id")?
        .parse()
        .map_err(|e| format!("parse doc uuid: {e}"))?;

    let other_ids: Vec<Uuid> = users[1..].iter().map(|(id, _)| *id).collect();
    add_members_sql(&args.project, &args.env_file, doc, &other_ids)?;

    let (mon_tx, mon_rx) = watch::channel(false);
    let monitor_task = tokio::spawn(sample_resources(
        args.project.clone(),
        args.env_file.clone(),
        args.metrics_url.clone(),
        http.clone(),
        mon_rx,
    ));

    let shared = Arc::new(SharedState {
        origin: Instant::now(),
        load_origin: OnceLock::new(),
        send_times_ns: (0..CLIENT_COUNT)
            .map(|_| (0..TOTAL_STEPS).map(|_| AtomicU64::new(0)).collect())
            .collect(),
        rep_map: Mutex::new(HashMap::with_capacity(CLIENT_COUNT)),
        base_client_seq: Mutex::new(vec![1; CLIENT_COUNT]),
        settled_count: AtomicUsize::new(0),
        disconnects: AtomicUsize::new(0),
        digest_mismatches: AtomicUsize::new(0),
        server_digest: Mutex::new(None),
        client_digests: Mutex::new(vec![None; CLIENT_COUNT]),
    });

    let connected_barrier = Arc::new(Barrier::new(CLIENT_COUNT));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut client_tasks = Vec::with_capacity(CLIENT_COUNT);

    println!("connecting {CLIENT_COUNT} concurrent WebSocket clients...");
    for (idx, (_, cookie)) in users.into_iter().enumerate() {
        let http = http.clone();
        let base_url = args.base_url.clone();
        let snap_bytes = Arc::clone(&snap_bytes);
        let base_wb = Arc::clone(&base_wb);
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&connected_barrier);
        let rx = shutdown_rx.clone();
        // Stagger initial WS + snapshot handshakes lightly in waves of 10.
        let delay = Duration::from_millis((idx / 10) as u64 * 120);
        client_tasks.push(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            run_load_client(
                idx, base_url, cookie, doc, http, snap_bytes, base_wb, shared, barrier, rx,
            )
            .await
        }));
    }

    // Wait for all 50 clients to finish sending & catching up to seq = TOTAL_STEPS * CLIENT_COUNT.
    let expected_total_txns = (CLIENT_COUNT * TOTAL_STEPS) as u64;
    let load_deadline = Duration::from_secs(((TOTAL_STEPS as u64 * SEND_INTERVAL_MS) / 1000) + 45);
    let settle_start = Instant::now();
    while shared.settled_count.load(Ordering::Acquire) < CLIENT_COUNT {
        if settle_start.elapsed() > load_deadline {
            let _ = shutdown_tx.send(true);
            return Err(format!(
                "timed out waiting for all {CLIENT_COUNT} clients to settle (settled={}, disconnects={})",
                shared.settled_count.load(Ordering::Acquire),
                shared.disconnects.load(Ordering::Acquire),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for the server's quiet Digest frame at head_seq == expected_total_txns.
    let digest_deadline = Instant::now() + Duration::from_secs(10);
    while shared.server_digest.lock().unwrap().is_none() && Instant::now() < digest_deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = shutdown_tx.send(true);
    let _ = mon_tx.send(true);

    let mut all_latencies_ns =
        Vec::with_capacity(CLIENT_COUNT * MEASURED_STEPS * (CLIENT_COUNT - 1));
    let mut server_sync_seen_seq = 0_u64;
    for task in client_tasks {
        let outcome = task.await.map_err(|e| format!("client task join: {e}"))??;
        server_sync_seen_seq = server_sync_seen_seq.max(outcome.final_seq);
        all_latencies_ns.extend(outcome.latencies_ns);
    }

    let resources = monitor_task
        .await
        .map_err(|e| format!("monitor task join: {e}"))?;

    let disconnects = shared.disconnects.load(Ordering::Acquire);
    let digest_mismatches = shared.digest_mismatches.load(Ordering::Acquire);
    let server_digest = shared.server_digest.lock().unwrap().take();
    let client_digests = shared.client_digests.lock().unwrap();
    let first_client_digest = client_digests
        .first()
        .copied()
        .flatten()
        .ok_or("missing client digest")?;
    let all_clients_match = client_digests
        .iter()
        .all(|d| d.is_some_and(|val| val == first_client_digest));
    let server_matches = server_digest.is_some_and(|d| d == first_client_digest);

    if all_latencies_ns.is_empty() {
        return Err(String::from(
            "no measured remote-visibility latency samples recorded",
        ));
    }
    all_latencies_ns.sort_unstable();
    let loopback_stats = summarize_ms(&all_latencies_ns, 0.0);
    let rtt50_stats = summarize_ms(&all_latencies_ns, REFERENCE_RTT_MS);

    let p95_rtt50 = rtt50_stats["p95_ms"].as_f64().unwrap_or(f64::INFINITY);
    let expected_samples = CLIENT_COUNT * MEASURED_STEPS * (CLIENT_COUNT - 1);
    let lost_txns = if server_sync_seen_seq == expected_total_txns
        && all_latencies_ns.len() == expected_samples
    {
        0
    } else {
        expected_samples.saturating_sub(all_latencies_ns.len())
    };

    let server_peak_pct = (resources.server_peak_mib / SERVER_MEM_LIMIT_MIB) * 100.0;
    let measured_duration_s = (MEASURED_STEPS as f64 * SEND_INTERVAL_MS as f64) / 1000.0;
    let achieved_tps = (CLIENT_COUNT * MEASURED_STEPS) as f64 / measured_duration_s;

    let report = json!({
        "suite": "m2b-load",
        "date": args.date,
        "commit": args.commit,
        "clients": CLIENT_COUNT,
        "warmupSeconds": (WARMUP_STEPS as u64 * SEND_INTERVAL_MS) / 1000,
        "durationSeconds": (MEASURED_STEPS as u64 * SEND_INTERVAL_MS) / 1000,
        "targetTxnsPerSec": (CLIENT_COUNT as f64) * (1000.0 / SEND_INTERVAL_MS as f64),
        "achievedTxnsPerSec": achieved_tps,
        "totalTxns": expected_total_txns,
        "warmupTxns": CLIENT_COUNT * WARMUP_STEPS,
        "measuredTxns": CLIENT_COUNT * MEASURED_STEPS,
        "remoteDeliverySamples": all_latencies_ns.len(),
        "document": {
            "populatedCells": built.populated_cells,
            "estimatedBytes": built.estimated_bytes,
            "snapshotCompressedBytes": snap_bytes.len(),
        },
        "disconnects": disconnects,
        "lostTxns": lost_txns,
        "digestMismatches": digest_mismatches,
        "allDigestsMatch": all_clients_match && server_matches,
        "referenceRttMs": REFERENCE_RTT_MS,
        "remoteVisibilityLoopbackMs": loopback_stats,
        "remoteVisibilityRtt50Ms": rtt50_stats,
        "resources": {
            "serverMemLimitMiB": SERVER_MEM_LIMIT_MIB,
            "serverPeakMiB": (resources.server_peak_mib * 100.0).round() / 100.0,
            "serverPeakPct": (server_peak_pct * 100.0).round() / 100.0,
            "postgresMemLimitMiB": SERVER_MEM_LIMIT_MIB,
            "postgresPeakMiB": (resources.postgres_peak_mib * 100.0).round() / 100.0,
            "maxLoadedBytes": resources.max_loaded_bytes,
            "snapshotsWritten": resources.snapshots_written,
            "snapshotsPruned": resources.snapshots_pruned,
            "sampleCount": resources.sample_count,
        },
    });

    if let Some(parent) = args.out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(
        &args.out_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&report).map_err(|e| format!("json: {e}"))?
        ),
    )
    .map_err(|e| format!("write {}: {e}", args.out_path.display()))?;

    println!("archived loadtest report to {}", args.out_path.display());
    println!(
        "loopback p50={:.2}ms p95={:.2}ms | +50ms RTT p95={:.2}ms | server peak={:.2} MiB ({:.1}% of 768 MiB)",
        loopback_stats["p50_ms"].as_f64().unwrap_or(0.0),
        loopback_stats["p95_ms"].as_f64().unwrap_or(0.0),
        p95_rtt50,
        resources.server_peak_mib,
        server_peak_pct,
    );

    if disconnects > 0 {
        return Err(format!(
            "loadtest failed: {disconnects} unexpected disconnect(s)"
        ));
    }
    if lost_txns > 0 {
        return Err(format!(
            "loadtest failed: {lost_txns} missing remote deliveries (seen seq {server_sync_seen_seq}, samples {})",
            all_latencies_ns.len()
        ));
    }
    if digest_mismatches > 0 || !all_clients_match || !server_matches {
        return Err(format!(
            "loadtest failed: digest divergence (mismatches={digest_mismatches}, all_clients_match={all_clients_match}, server_matches={server_matches})"
        ));
    }
    if p95_rtt50 > GATE_RTT50_P95_MS {
        return Err(format!(
            "loadtest failed: p95 with +50ms RTT is {p95_rtt50:.2} ms > {GATE_RTT50_P95_MS} ms"
        ));
    }
    if resources.server_peak_mib > GATE_SERVER_MEM_MIB {
        return Err(format!(
            "loadtest failed: server peak memory {:.2} MiB > {GATE_SERVER_MEM_MIB:.2} MiB",
            resources.server_peak_mib
        ));
    }
    Ok(())
}

struct ClientOutcome {
    final_seq: u64,
    latencies_ns: Vec<u64>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "self-contained async task arguments for one independent client"
)]
async fn run_load_client(
    idx: usize,
    base_url: String,
    cookie: String,
    doc: Uuid,
    http: reqwest::Client,
    expected_snap_bytes: Arc<Vec<u8>>,
    base_wb: Arc<Workbook<BoundFormula>>,
    shared: Arc<SharedState>,
    barrier: Arc<Barrier>,
    mut shutdown: watch::Receiver<bool>,
) -> AppResult<ClientOutcome> {
    let ws_base = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("expected http:// base_url, got `{base_url}`"))?;
    let url = format!("ws://{ws_base}/api/sync/{doc}");
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("ws req: {e}"))?;
    req.headers_mut().insert(
        "Origin",
        base_url.parse().map_err(|e| format!("origin: {e}"))?,
    );
    req.headers_mut().insert(
        "Cookie",
        cookie.parse().map_err(|e| format!("cookie: {e}"))?,
    );

    let (mut socket, _) = timeout(IO_TIMEOUT, connect_async(req))
        .await
        .map_err(|_| format!("client {idx} ws connect timeout"))?
        .map_err(|e| format!("client {idx} ws connect: {e}"))?;

    let mut machine = ClientSyncMachine::new(doc.into_bytes(), MAIN_BRANCH.into_bytes());
    for action in machine.on_connected() {
        if let ClientAction::SendFrame(frame) = action {
            send_ws(&mut socket, &frame).await?;
        }
    }

    let welcome = recv_ws(&mut socket).await?;
    let welcome_actions = machine.on_welcome(welcome);
    let mut queue = VecDeque::from(welcome_actions);
    while let Some(action) = queue.pop_front() {
        match action {
            ClientAction::SendFrame(frame) => send_ws(&mut socket, &frame).await?,
            ClientAction::RequestSnapshot { snapshot_seq, url } => {
                let resp = http
                    .get(format!("{base_url}{url}"))
                    .header(header::ORIGIN, &base_url)
                    .header(header::COOKIE, &cookie)
                    .send()
                    .await
                    .map_err(|e| format!("client {idx} GET {url}: {e}"))?;
                if resp.status() != StatusCode::OK {
                    return Err(format!("client {idx} GET {url} failed: {}", resp.status()));
                }
                let raw = resp
                    .bytes()
                    .await
                    .map_err(|e| format!("client {idx} snap bytes: {e}"))?;
                let (snap_wb, watermarks, permutes) =
                    if snapshot_seq == 0 && raw.as_ref() == expected_snap_bytes.as_slice() {
                        ((*base_wb).clone(), Vec::new(), Vec::new())
                    } else {
                        let decoded = decode_snapshot_any(&raw)
                            .map_err(|e| format!("client {idx} decode snap: {e:?}"))?;
                        let wms = decoded
                            .watermarks
                            .into_iter()
                            .map(|wm| ReplicaWatermark {
                                replica: wm.replica,
                                user_id: [0; 16],
                                last_client_seq: wm.last_client_seq,
                                max_id_counter: wm.max_id_counter,
                            })
                            .collect();
                        let permutes = decoded
                            .permutes
                            .iter()
                            .map(CommittedPermute::from)
                            .collect();
                        (decoded.engine.workbook().clone(), wms, permutes)
                    };
                let tail = fetch_tail(&http, &base_url, doc, &cookie, snapshot_seq).await?;
                queue.extend(machine.install_snapshot(
                    snapshot_seq,
                    snap_wb,
                    &watermarks,
                    permutes,
                    tail,
                ));
            }
            _ => {}
        }
    }

    let rep = machine
        .replica()
        .ok_or_else(|| format!("client {idx} has no replica"))?;
    shared.rep_map.lock().unwrap().insert(rep, idx);
    shared.base_client_seq.lock().unwrap()[idx] = machine.next_client_seq();

    // Wait until all 50 clients have caught up and registered their replica IDs.
    if barrier.wait().await.is_leader() {
        let _ = shared
            .load_origin
            .set(tokio::time::Instant::now() + Duration::from_millis(200));
    }
    barrier.wait().await;
    let load_origin = *shared.load_origin.get().ok_or("missing load_origin")?;
    let snapshot_rep_map = shared.rep_map.lock().unwrap().clone();
    let base_client_seqs = shared.base_client_seq.lock().unwrap().clone();

    let expected_total_txns = (CLIENT_COUNT * TOTAL_STEPS) as u64;
    let marked_settled = AtomicBool::new(false);
    let mut step = 0_usize;
    let mut latencies_ns = Vec::with_capacity(MEASURED_STEPS * (CLIENT_COUNT - 1));
    let target_offset_ms = (idx as u64) * CLIENT_STAGGER_MS;
    let mut next_send_at = load_origin + Duration::from_millis(target_offset_ms);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(next_send_at), if step < TOTAL_STEPS => {
                let op = Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(idx as u64),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(
                            (idx as f64) * 1_000_000.0 + (step as f64),
                        )),
                        format: FormatPatch::Keep,
                    }],
                };
                let actions = machine
                    .push_local(
                        format!("Load {idx}:{step}"),
                        vec![op],
                        1_700_000_000_000 + (step as u64),
                    )
                    .map_err(|e| format!("client {idx} push_local: {e:?}"))?;
                let elapsed = shared.origin.elapsed().as_nanos();
                let elapsed_ns = u64::try_from(elapsed).unwrap_or(u64::MAX - 1).saturating_add(1);
                shared.send_times_ns[idx][step].store(elapsed_ns, Ordering::Release);
                for action in actions {
                    if let ClientAction::SendFrame(frame) = action {
                        send_ws(&mut socket, &frame).await?;
                    }
                }
                step += 1;
                next_send_at = load_origin
                    + Duration::from_millis(target_offset_ms + (step as u64) * SEND_INTERVAL_MS);
            }
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        let frame = decode_sync_frame_v2(&bytes)
                            .map_err(|e| format!("client {idx} decode: {e:?}"))?;
                        match frame {
                            WireSyncFrameV2::Ack { replica, client_seq, .. } => {
                                machine.on_ack(replica, client_seq);
                            }
                            WireSyncFrameV2::Reject { client_seq, reason } => {
                                return Err(format!("client {idx} rejected client_seq={client_seq}: {reason:?}"));
                            }
                            WireSyncFrameV2::Commit { seq, replica, client_seq, user_id, timestamp_ms, txn } => {
                                let now_ns = u64::try_from(shared.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
                                if let Some(&author_idx) = snapshot_rep_map.get(&replica)
                                    && author_idx != idx
                                    && let Some(step_idx) = client_seq.checked_sub(base_client_seqs[author_idx])
                                    && let Some(step_usize) = usize::try_from(step_idx).ok()
                                    && (WARMUP_STEPS..TOTAL_STEPS).contains(&step_usize)
                                {
                                    let sent_plus_1 = shared.send_times_ns[author_idx][step_usize].load(Ordering::Acquire);
                                    if sent_plus_1 > 0 {
                                        let lat = now_ns.saturating_sub(sent_plus_1 - 1);
                                        latencies_ns.push(lat);
                                    }
                                }
                                let acts = machine.on_commit(WireCommittedTxnV3 {
                                    seq, replica, client_seq, user_id, timestamp_ms, txn,
                                });
                                for act in acts {
                                    if let ClientAction::DigestMismatch { .. } = act {
                                        shared.digest_mismatches.fetch_add(1, Ordering::AcqRel);
                                    }
                                }
                                if !marked_settled.load(Ordering::Relaxed)
                                    && step == TOTAL_STEPS
                                    && machine.pending_len() == 0
                                    && machine.confirmed_seq() >= expected_total_txns
                                {
                                    marked_settled.store(true, Ordering::Relaxed);
                                    shared.client_digests.lock().unwrap()[idx] =
                                        Some(machine.confirmed().state_digest());
                                    shared.settled_count.fetch_add(1, Ordering::Release);
                                }
                            }
                            WireSyncFrameV2::Digest { seq, state_digest } => {
                                for act in machine.on_digest(seq, state_digest) {
                                    if let ClientAction::DigestMismatch { .. } = act {
                                        shared.digest_mismatches.fetch_add(1, Ordering::AcqRel);
                                    }
                                }
                                if seq == expected_total_txns
                                    && let Ok(parsed) = StateDigest::try_from(state_digest)
                                {
                                    *shared.server_digest.lock().unwrap() = Some(parsed);
                                }
                            }
                            WireSyncFrameV2::Ping(n) => {
                                send_ws(&mut socket, &WireSyncFrameV2::Pong(n)).await?;
                            }
                            WireSyncFrameV2::Presence(_) | WireSyncFrameV2::Pong(_) => {}
                            WireSyncFrameV2::Welcome { .. } | WireSyncFrameV2::Hello { .. } | WireSyncFrameV2::Push { .. } => {}
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        shared.disconnects.fetch_add(1, Ordering::AcqRel);
                        return Err(format!("client {idx} closed unexpectedly"));
                    }
                    Some(Err(err)) => {
                        shared.disconnects.fetch_add(1, Ordering::AcqRel);
                        return Err(format!("client {idx} ws error: {err}"));
                    }
                }
            }
        }
    }

    Ok(ClientOutcome {
        final_seq: machine.confirmed_seq(),
        latencies_ns,
    })
}

async fn send_ws(socket: &mut Socket, frame: &WireSyncFrameV2) -> AppResult<()> {
    let bytes = encode_sync_frame_v2(frame).map_err(|e| format!("encode frame: {e:?}"))?;
    timeout(IO_TIMEOUT, socket.send(Message::Binary(bytes.into())))
        .await
        .map_err(|_| String::from("ws send timeout"))?
        .map_err(|e| format!("ws send: {e}"))
}

async fn recv_ws(socket: &mut Socket) -> AppResult<WireSyncFrameV2> {
    timeout(IO_TIMEOUT, async {
        while let Some(msg) = socket.next().await {
            match msg {
                Ok(Message::Binary(bytes)) => {
                    return decode_sync_frame_v2(&bytes)
                        .map_err(|e| format!("decode frame: {e:?}"));
                }
                Ok(Message::Close(f)) => return Err(format!("ws closed: {f:?}")),
                Ok(_) => {}
                Err(e) => return Err(format!("ws recv: {e}")),
            }
        }
        Err(String::from("ws closed cleanly before frame"))
    })
    .await
    .map_err(|_| String::from("ws recv timeout"))?
}

async fn fetch_tail(
    http: &reqwest::Client,
    base_url: &str,
    doc: Uuid,
    cookie: &str,
    after_seq: u64,
) -> AppResult<Vec<WireCommittedTxnV3>> {
    let mut out = Vec::new();
    let mut cursor = after_seq;
    loop {
        let path = format!("/api/workbooks/{doc}/txns?after_seq={cursor}&limit=500");
        let resp = http
            .get(format!("{base_url}{path}"))
            .header(header::ORIGIN, base_url)
            .header(header::COOKIE, cookie)
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        if resp.status() != StatusCode::OK {
            return Err(format!("GET {path}: {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("txns bytes: {e}"))?;
        let batch: Vec<WireCommittedTxnV3> =
            postcard::from_bytes(&bytes).map_err(|e| format!("decode txns: {e}"))?;
        if batch.is_empty() {
            break;
        }
        cursor = batch.last().map_or(cursor, |t| t.seq);
        let len = batch.len();
        out.extend(batch);
        if len < 500 {
            break;
        }
    }
    Ok(out)
}

fn summarize_ms(sorted_ns: &[u64], add_ms: f64) -> Value {
    let n = sorted_ns.len();
    let pick = |num: usize, den: usize| -> f64 {
        let idx = ((n.saturating_sub(1)) * num) / den;
        (sorted_ns[idx] as f64) / 1_000_000.0 + add_ms
    };
    let sum_ns: u128 = sorted_ns.iter().map(|&v| u128::from(v)).sum();
    let mean_ms = (sum_ns as f64) / (n as f64 * 1_000_000.0) + add_ms;
    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let max_ms = (sorted_ns[n - 1] as f64) / 1_000_000.0 + add_ms;

    json!({
        "samples": n,
        "mean_ms": round2(mean_ms),
        "p50_ms": round2(pick(50, 100)),
        "p95_ms": round2(pick(95, 100)),
        "p99_ms": round2(pick(99, 100)),
        "max_ms": round2(max_ms),
    })
}
