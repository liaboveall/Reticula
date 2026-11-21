//! Driver invoked by `cargo xtask drill` for backup/restore and epoch recovery verification.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use rand::Rng as _;
use reqwest::{Method, StatusCode, header};
use reticula_core::id::{ColId, RowId, SheetId, unpack_id, unpack_replica_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue, Workbook};
use reticula_core::state_digest::StateDigest;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{ClientAction, ClientSyncMachine, ReplicaWatermark};
use reticula_wire::snapshot::{decode_snapshot_any, encode_snapshot_v1};
use reticula_wire::sync_frame::{decode_sync_frame, encode_sync_frame};
use reticula_wire::v2::{WireCommittedTxnV2, WireOpV2, WirePendingTxnV2, WireSyncFrameV1};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type AppResult<T> = Result<T, String>;

const MAIN_BRANCH: Uuid = Uuid::nil();
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const T0_EDITS: usize = 505;
const T1_EXTRA_EDITS: usize = 20;

#[derive(Serialize, Deserialize)]
struct DrillState {
    alice_id: Uuid,
    alice_cookie: String,
    bob_id: Uuid,
    bob_cookie: String,
    wb_a: Uuid,
    wb_b: Uuid,
    t0_seq_a: u64,
    t0_snapshot_seq_a: u64,
    t0_digest_a: StateDigest,
    t0_seq_b: u64,
    t0_digest_b: StateDigest,
}

#[derive(Serialize, Deserialize)]
struct SavedClientState {
    doc_id: Uuid,
    epoch: u8,
    replica: u32,
    confirmed_seq: u64,
    last_acked_client_seq: u64,
    next_id_counter: u32,
    confirmed_snapshot: Vec<u8>,
    pending: Vec<WirePendingTxnV2>,
}

struct Args {
    phase: String,
    base_url: String,
    metrics_url: Option<String>,
    state_path: PathBuf,
    alice_state_path: Option<PathBuf>,
    project: Option<String>,
    env_file: Option<PathBuf>,
}

impl Args {
    fn parse() -> AppResult<Self> {
        let mut phase = None;
        let mut base_url = None;
        let mut metrics_url = None;
        let mut state_path = None;
        let mut alice_state_path = None;
        let mut project = None;
        let mut env_file = None;

        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            let val = iter
                .next()
                .ok_or_else(|| format!("missing value after `{arg}`"))?;
            match arg.as_str() {
                "--phase" => phase = Some(val),
                "--base-url" => base_url = Some(val),
                "--metrics-url" => metrics_url = Some(val),
                "--state" => state_path = Some(PathBuf::from(val)),
                "--alice-state" => alice_state_path = Some(PathBuf::from(val)),
                "--project" => project = Some(val),
                "--env-file" => env_file = Some(PathBuf::from(val)),
                other => return Err(format!("unknown flag `{other}`")),
            }
        }
        Ok(Self {
            phase: phase.ok_or_else(|| String::from("--phase is required"))?,
            base_url: base_url.ok_or_else(|| String::from("--base-url is required"))?,
            metrics_url,
            state_path: state_path.ok_or_else(|| String::from("--state is required"))?,
            alice_state_path,
            project,
            env_file,
        })
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("drill_driver failed: {err}");
        std::process::exit(1);
    }
}

async fn run() -> AppResult<()> {
    let args = Args::parse()?;
    match args.phase.as_str() {
        "prepare-t0" => prepare_t0(&args).await,
        "advance-t1" => advance_t1(&args).await,
        "verify-restore" => verify_restore(&args).await,
        other => Err(format!("unknown phase `{other}`")),
    }
}

fn http_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(IO_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn wait_endpoints(
    client: &reqwest::Client,
    base_url: &str,
    metrics_url: Option<&str>,
) -> AppResult<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(resp) = client.get(format!("{base_url}/readyz")).send().await
            && resp.status() == StatusCode::OK
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {base_url}/readyz"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    if let Some(url) = metrics_url {
        let resp = client
            .get(format!("{url}/metrics"))
            .send()
            .await
            .map_err(|e| format!("GET {url}/metrics: {e}"))?;
        if resp.status() != StatusCode::OK {
            return Err(format!("GET {url}/metrics returned {}", resp.status()));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| format!("metrics body: {e}"))?;
        if !text.contains("reticula_active_rooms") {
            return Err(String::from("metrics missing reticula_active_rooms"));
        }
    }
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

fn seed_user_session(project: &str, env_file: &Path, name: &str) -> AppResult<(Uuid, String)> {
    let mut secret = [0_u8; 32];
    rand::rng().fill_bytes(&mut secret);
    let mut hash_hex = String::with_capacity(64);
    for byte in Sha256::digest(secret) {
        let _ = write!(hash_hex, "{byte:02x}");
    }
    let id = Uuid::new_v4();
    let sql = format!(
        "INSERT INTO users (id, name, avatar_url) VALUES ('{id}', '{name}', '');
         INSERT INTO sessions (token_hash, user_id, expires_at)
           VALUES ('\\x{hash_hex}', '{id}', now() + interval '1 day');"
    );
    psql_exec(project, env_file, &sql)?;
    let cookie = format!("reticula_session={}", URL_SAFE_NO_PAD.encode(secret));
    Ok((id, cookie))
}

async fn api_json(
    client: &reqwest::Client,
    method: Method,
    base_url: &str,
    path: &str,
    cookie: &str,
    body: Option<Value>,
) -> AppResult<(StatusCode, Value)> {
    let mut req = client
        .request(method, format!("{base_url}{path}"))
        .header(header::ORIGIN, base_url)
        .header(header::COOKIE, cookie);
    if let Some(payload) = body {
        req = req.json(&payload);
    }
    let resp = req.send().await.map_err(|e| format!("{path}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("{path} body: {e}"))?;
    let parsed = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).map_err(|e| format!("{path} json ({text}): {e}"))?
    };
    Ok((status, parsed))
}

async fn fetch_bytes(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    cookie: &str,
) -> AppResult<Vec<u8>> {
    let resp = client
        .get(format!("{base_url}{path}"))
        .header(header::ORIGIN, base_url)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("GET {path}: {e}"))?;
    if resp.status() != StatusCode::OK {
        return Err(format!("GET {path} failed: {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("GET {path} bytes: {e}"))
}

async fn fetch_all_tail_txns(
    client: &reqwest::Client,
    base_url: &str,
    doc: Uuid,
    cookie: &str,
    after_seq: u64,
) -> AppResult<Vec<WireCommittedTxnV2>> {
    let mut out = Vec::new();
    let mut cursor = after_seq;
    loop {
        let path = format!("/api/workbooks/{doc}/txns?after_seq={cursor}&limit=500");
        let bytes = fetch_bytes(client, base_url, &path, cookie).await?;
        let batch: Vec<WireCommittedTxnV2> =
            postcard::from_bytes(&bytes).map_err(|e| format!("decode txns batch: {e}"))?;
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

async fn ws_connect(base_url: &str, doc: Uuid, cookie: &str) -> AppResult<Socket> {
    let ws_base = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("expected http:// base url, got `{base_url}`"))?;
    let url = format!("ws://{ws_base}/api/sync/{doc}");
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("ws request: {e}"))?;
    req.headers_mut().insert(
        "Origin",
        base_url
            .parse()
            .map_err(|e| format!("origin header: {e}"))?,
    );
    req.headers_mut().insert(
        "Cookie",
        cookie.parse().map_err(|e| format!("cookie header: {e}"))?,
    );
    let (socket, _) = timeout(IO_TIMEOUT, connect_async(req))
        .await
        .map_err(|_| String::from("ws connect timeout"))?
        .map_err(|e| format!("ws connect: {e}"))?;
    Ok(socket)
}

async fn send_frame(socket: &mut Socket, frame: &WireSyncFrameV1) -> AppResult<()> {
    let bytes = encode_sync_frame(frame).map_err(|e| format!("encode frame: {e:?}"))?;
    timeout(IO_TIMEOUT, socket.send(Message::Binary(bytes.into())))
        .await
        .map_err(|_| String::from("ws send timeout"))?
        .map_err(|e| format!("ws send: {e}"))
}

async fn recv_frame(socket: &mut Socket) -> AppResult<WireSyncFrameV1> {
    timeout(IO_TIMEOUT, async {
        while let Some(msg) = socket.next().await {
            match msg {
                Ok(Message::Binary(bytes)) => {
                    return decode_sync_frame(&bytes).map_err(|e| format!("decode frame: {e:?}"));
                }
                Ok(Message::Close(frame)) => {
                    return Err(format!("ws closed cleanly: {frame:?}"));
                }
                Ok(_) => {}
                Err(err) => return Err(format!("ws recv error: {err}")),
            }
        }
        Err(String::from("ws stream ended unexpectedly"))
    })
    .await
    .map_err(|_| String::from("ws recv timeout"))?
}

struct SyncDriveReport {
    epoch_changed: Option<(u8, u8)>,
    pre_pending_digest: Option<StateDigest>,
    committed_ops: Vec<WireOpV2>,
}

#[derive(Clone, Copy)]
struct ClientCtx<'a> {
    http: &'a reqwest::Client,
    base_url: &'a str,
    cookie: &'a str,
    doc: Uuid,
}

async fn run_client_actions(
    ctx: ClientCtx<'_>,
    socket: &mut Socket,
    machine: &mut ClientSyncMachine,
    actions: Vec<ClientAction>,
    report: &mut SyncDriveReport,
) -> AppResult<()> {
    let mut queue = std::collections::VecDeque::from(actions);
    while let Some(action) = queue.pop_front() {
        match action {
            ClientAction::SendFrame(frame) => {
                send_frame(socket, &frame).await?;
            }
            ClientAction::EpochChanged {
                old_epoch,
                new_epoch,
            } => {
                report.epoch_changed = Some((old_epoch, new_epoch));
            }
            ClientAction::DigestMismatch { seq, .. } => {
                return Err(format!("client reported DigestMismatch at seq {seq}"));
            }
            ClientAction::RequestReconnect => {
                return Err(String::from("client requested unexpected reconnect"));
            }
            ClientAction::RequestSnapshot { snapshot_seq, url } => {
                let raw_snap = fetch_bytes(ctx.http, ctx.base_url, &url, ctx.cookie).await?;
                let decoded = decode_snapshot_any(&raw_snap)
                    .map_err(|e| format!("decode snapshot: {e:?}"))?;
                let watermarks: Vec<ReplicaWatermark> = decoded
                    .watermarks
                    .into_iter()
                    .map(|wm| ReplicaWatermark {
                        replica: wm.replica,
                        user_id: [0; 16],
                        last_client_seq: wm.last_client_seq,
                        max_id_counter: wm.max_id_counter,
                    })
                    .collect();
                let snap_wb = decoded.engine.workbook().clone();
                let tail =
                    fetch_all_tail_txns(ctx.http, ctx.base_url, ctx.doc, ctx.cookie, snapshot_seq)
                        .await?;
                let next_actions =
                    machine.install_snapshot(snapshot_seq, snap_wb, &watermarks, tail);
                report.pre_pending_digest = Some(machine.confirmed().state_digest());
                queue.extend(next_actions);
            }
            ClientAction::PersistPending(_)
            | ClientAction::PersistTruncatedPending(_)
            | ClientAction::SaveUnsyncedLocal { .. }
            | ClientAction::PruneUndoFrom(_)
            | ClientAction::NotifyVisibleChanged => {}
        }
    }
    Ok(())
}

async fn connect_and_catchup(
    ctx: ClientCtx<'_>,
    machine: &mut ClientSyncMachine,
) -> AppResult<(Socket, SyncDriveReport)> {
    let mut socket = ws_connect(ctx.base_url, ctx.doc, ctx.cookie).await?;
    let mut report = SyncDriveReport {
        epoch_changed: None,
        pre_pending_digest: None,
        committed_ops: Vec::new(),
    };
    let hello_actions = machine.on_connected();
    run_client_actions(ctx, &mut socket, machine, hello_actions, &mut report).await?;

    let welcome = recv_frame(&mut socket).await?;
    let welcome_actions = machine.on_welcome(welcome);
    run_client_actions(ctx, &mut socket, machine, welcome_actions, &mut report).await?;
    if report.pre_pending_digest.is_none() {
        report.pre_pending_digest = Some(machine.confirmed().state_digest());
    }
    Ok((socket, report))
}

async fn wait_until_settled(
    ctx: ClientCtx<'_>,
    socket: &mut Socket,
    machine: &mut ClientSyncMachine,
    min_seq: u64,
    report: &mut SyncDriveReport,
) -> AppResult<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while machine.pending_len() > 0 || machine.confirmed_seq() < min_seq {
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting to settle: pending={}, confirmed_seq={}, target={min_seq}",
                machine.pending_len(),
                machine.confirmed_seq()
            ));
        }
        let frame = recv_frame(socket).await?;
        let actions = match frame {
            WireSyncFrameV1::Welcome { .. } => machine.on_welcome(frame),
            WireSyncFrameV1::Ack {
                replica,
                client_seq,
                ..
            } => {
                machine.on_ack(replica, client_seq);
                Vec::new()
            }
            WireSyncFrameV1::Reject { client_seq, reason } => {
                return Err(format!(
                    "unexpected Reject for client_seq={client_seq}: {reason:?}"
                ));
            }
            WireSyncFrameV1::Commit {
                seq,
                replica,
                client_seq,
                user_id,
                timestamp_ms,
                txn,
            } => {
                report.committed_ops.extend(txn.ops.clone());
                machine.on_commit(WireCommittedTxnV2 {
                    seq,
                    replica,
                    client_seq,
                    user_id,
                    timestamp_ms,
                    txn,
                })
            }
            WireSyncFrameV1::Digest { seq, state_digest } => machine.on_digest(seq, state_digest),
            WireSyncFrameV1::Ping(n) => {
                send_frame(socket, &WireSyncFrameV1::Pong(n)).await?;
                Vec::new()
            }
            WireSyncFrameV1::Presence(_) | WireSyncFrameV1::Pong(_) => Vec::new(),
            WireSyncFrameV1::Hello { .. } | WireSyncFrameV1::Push { .. } => Vec::new(),
        };
        run_client_actions(ctx, socket, machine, actions, report).await?;
    }
    Ok(())
}

fn push_batched_cells(
    machine: &mut ClientSyncMachine,
    count: usize,
    row_offset: u64,
    col: u64,
    value_offset: f64,
) -> AppResult<Vec<WireSyncFrameV1>> {
    let mut txns = Vec::with_capacity(count);
    for i in 0..count {
        let op = Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(row_offset + (i as u64 % 50)),
                col: ColId(col),
                input: CellInput::Scalar(ScalarValue::Number(value_offset + i as f64)),
                format: FormatPatch::Keep,
            }],
        };
        let actions = machine
            .push_local(format!("Edit {i}"), vec![op], 1_700_000_000_000 + i as u64)
            .map_err(|e| format!("push_local: {e:?}"))?;
        for action in actions {
            if let ClientAction::SendFrame(WireSyncFrameV1::Push { txns: chunk }) = action {
                txns.extend(chunk);
            }
        }
    }
    let frames = txns
        .chunks(50)
        .map(|chunk| WireSyncFrameV1::Push {
            txns: chunk.to_vec(),
        })
        .collect();
    Ok(frames)
}

async fn prepare_t0(args: &Args) -> AppResult<()> {
    let project = args
        .project
        .as_deref()
        .ok_or_else(|| String::from("--project is required for prepare-t0"))?;
    let env_file = args
        .env_file
        .as_deref()
        .ok_or_else(|| String::from("--env-file is required for prepare-t0"))?;

    let http = http_client()?;
    wait_endpoints(&http, &args.base_url, args.metrics_url.as_deref()).await?;

    let (alice_id, alice_cookie) = seed_user_session(project, env_file, "Alice")?;
    let (bob_id, bob_cookie) = seed_user_session(project, env_file, "Bob")?;

    // Import Workbook A with initial seq=0 snapshot.
    let mut base_wb: Workbook<BoundFormula> = Workbook::new();
    base_wb
        .apply(Op::SetCells {
            sheet: SheetId(0),
            edits: (0..5_u64)
                .map(|r| CellEdit {
                    row: RowId(r),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(1000.0 + r as f64)),
                    format: FormatPatch::Keep,
                })
                .collect(),
        })
        .map_err(|e| format!("seed wb_a: {e:?}"))?;
    let snap_bytes =
        encode_snapshot_v1(&base_wb, &[]).map_err(|e| format!("encode snap: {e:?}"))?;
    let import_resp = http
        .post(format!("{}/api/workbooks/import", args.base_url))
        .header(header::ORIGIN, &args.base_url)
        .header(header::COOKIE, &alice_cookie)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("X-Workbook-Title", "DrillWorkbookA")
        .body(snap_bytes)
        .send()
        .await
        .map_err(|e| format!("import wb_a: {e}"))?;
    if import_resp.status() != StatusCode::CREATED {
        return Err(format!("import wb_a failed: {}", import_resp.status()));
    }
    let import_json: Value = import_resp
        .json()
        .await
        .map_err(|e| format!("import json: {e}"))?;
    let wb_a: Uuid = import_json["id"]
        .as_str()
        .ok_or("missing wb_a id")?
        .parse()
        .map_err(|e| format!("parse wb_a: {e}"))?;

    // Share Workbook A with Bob so Bob is an active editor in T0.
    let (share_status, share_json) = api_json(
        &http,
        Method::POST,
        &args.base_url,
        &format!("/api/workbooks/{wb_a}/share-links"),
        &alice_cookie,
        Some(json!({ "role": "editor", "label": "Bob invite", "singleUse": false })),
    )
    .await?;
    if share_status != StatusCode::CREATED {
        return Err(format!("create share link failed: {share_status}"));
    }
    let token = share_json["url"]
        .as_str()
        .and_then(|u| u.rsplit('/').next())
        .ok_or("missing share token")?;
    let (join_status, _) = api_json(
        &http,
        Method::POST,
        &args.base_url,
        &format!("/api/join/{token}"),
        &bob_cookie,
        None,
    )
    .await?;
    if join_status != StatusCode::OK {
        return Err(format!("bob join failed: {join_status}"));
    }

    // Push 505 edits on Workbook A to trigger an automated seq >= 500 snapshot on disk.
    let ctx_a = ClientCtx {
        http: &http,
        base_url: &args.base_url,
        cookie: &alice_cookie,
        doc: wb_a,
    };
    let mut alice_a = ClientSyncMachine::new(wb_a.into_bytes(), MAIN_BRANCH.into_bytes());
    let (mut ws_a, mut rep_a) = connect_and_catchup(ctx_a, &mut alice_a).await?;
    let push_frames = push_batched_cells(&mut alice_a, T0_EDITS, 10, 1, 1.0)?;
    for frame in push_frames {
        send_frame(&mut ws_a, &frame).await?;
    }
    wait_until_settled(ctx_a, &mut ws_a, &mut alice_a, T0_EDITS as u64, &mut rep_a).await?;
    drop(ws_a);

    // Wait for background snapshot task to publish a seq >= 500 snapshot in DB/history.
    let snap_deadline = Instant::now() + Duration::from_secs(20);
    let t0_snapshot_seq_a = loop {
        let (_, hist) = api_json(
            &http,
            Method::GET,
            &args.base_url,
            &format!("/api/workbooks/{wb_a}/history?limit=10"),
            &alice_cookie,
            None,
        )
        .await?;
        if let Some(seq) = hist["snapshotSeqs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str()?.parse::<u64>().ok())
            .find(|&seq| seq >= 500)
        {
            break seq;
        }
        if Instant::now() >= snap_deadline {
            return Err(String::from("timed out waiting for T0 snapshot seq >= 500"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let (ver_status, _) = api_json(
        &http,
        Method::POST,
        &args.base_url,
        &format!("/api/workbooks/{wb_a}/versions"),
        &alice_cookie,
        Some(json!({ "seq": T0_EDITS.to_string(), "name": "T0 Baseline" })),
    )
    .await?;
    if ver_status != StatusCode::CREATED {
        return Err(format!("create T0 version failed: {ver_status}"));
    }

    // Create Workbook B via POST /api/workbooks (no seq=0 snapshot) + 5 edits + named version.
    let (wb_b_status, wb_b_json) = api_json(
        &http,
        Method::POST,
        &args.base_url,
        "/api/workbooks",
        &alice_cookie,
        Some(json!({ "title": "DrillWorkbookB" })),
    )
    .await?;
    if wb_b_status != StatusCode::CREATED {
        return Err(format!("create wb_b failed: {wb_b_status}"));
    }
    let wb_b: Uuid = wb_b_json["id"]
        .as_str()
        .ok_or("missing wb_b id")?
        .parse()
        .map_err(|e| format!("parse wb_b: {e}"))?;

    let ctx_b = ClientCtx {
        http: &http,
        base_url: &args.base_url,
        cookie: &alice_cookie,
        doc: wb_b,
    };
    let mut alice_b = ClientSyncMachine::new(wb_b.into_bytes(), MAIN_BRANCH.into_bytes());
    let (mut ws_b, mut rep_b) = connect_and_catchup(ctx_b, &mut alice_b).await?;
    for frame in push_batched_cells(&mut alice_b, 5, 0, 0, 500.0)? {
        send_frame(&mut ws_b, &frame).await?;
    }
    wait_until_settled(ctx_b, &mut ws_b, &mut alice_b, 5, &mut rep_b).await?;
    drop(ws_b);

    let (ver_b_status, _) = api_json(
        &http,
        Method::POST,
        &args.base_url,
        &format!("/api/workbooks/{wb_b}/versions"),
        &alice_cookie,
        Some(json!({ "seq": "5", "name": "B Baseline" })),
    )
    .await?;
    if ver_b_status != StatusCode::CREATED {
        return Err(format!("create B version failed: {ver_b_status}"));
    }

    let state = DrillState {
        alice_id,
        alice_cookie,
        bob_id,
        bob_cookie,
        wb_a,
        wb_b,
        t0_seq_a: alice_a.confirmed_seq(),
        t0_snapshot_seq_a,
        t0_digest_a: alice_a.confirmed().state_digest(),
        t0_seq_b: alice_b.confirmed_seq(),
        t0_digest_b: alice_b.confirmed().state_digest(),
    };
    let serialized =
        serde_json::to_string_pretty(&state).map_err(|e| format!("serialize state: {e}"))?;
    fs::write(&args.state_path, serialized).map_err(|e| format!("write state: {e}"))?;
    println!(
        "prepare-t0 complete: wb_a={wb_a} seq={} snap={t0_snapshot_seq_a}, wb_b={wb_b} seq={}",
        state.t0_seq_a, state.t0_seq_b
    );
    Ok(())
}

async fn advance_t1(args: &Args) -> AppResult<()> {
    let alice_state_path = args
        .alice_state_path
        .as_deref()
        .ok_or_else(|| String::from("--alice-state is required for advance-t1"))?;
    let state: DrillState = serde_json::from_str(
        &fs::read_to_string(&args.state_path).map_err(|e| format!("read state: {e}"))?,
    )
    .map_err(|e| format!("parse state: {e}"))?;
    let http = http_client()?;

    let ctx_a = ClientCtx {
        http: &http,
        base_url: &args.base_url,
        cookie: &state.alice_cookie,
        doc: state.wb_a,
    };
    let mut alice = ClientSyncMachine::new(state.wb_a.into_bytes(), MAIN_BRANCH.into_bytes());
    let (mut ws, mut rep) = connect_and_catchup(ctx_a, &mut alice).await?;

    // Advance live src server to T1 (including an epoch=0 row insertion + value edits).
    let run = alice
        .alloc_row_run(1)
        .ok_or_else(|| String::from("alloc_row_run failed at T0"))?;
    let first_row = RowId(run.first_id);
    let insert_op = Op::InsertRows {
        sheet: SheetId(0),
        after: None,
        ids: run,
    };
    let set_op = Op::SetCells {
        sheet: SheetId(0),
        edits: vec![CellEdit {
            row: first_row,
            col: ColId(0),
            input: CellInput::Scalar(ScalarValue::Number(777.0)),
            format: FormatPatch::Keep,
        }],
    };
    for op in [insert_op, set_op] {
        let actions = alice
            .push_local(String::from("T1 row edit"), vec![op], 1_700_000_010_000)
            .map_err(|e| format!("push_local t1: {e:?}"))?;
        run_client_actions(ctx_a, &mut ws, &mut alice, actions, &mut rep).await?;
    }
    for frame in push_batched_cells(&mut alice, T1_EXTRA_EDITS - 2, 100, 2, 2000.0)? {
        send_frame(&mut ws, &frame).await?;
    }
    let t1_target = state.t0_seq_a + T1_EXTRA_EDITS as u64;
    wait_until_settled(ctx_a, &mut ws, &mut alice, t1_target, &mut rep).await?;
    drop(ws);

    // Go offline and author 2 unconfirmed local edits (allocated in old epoch=0).
    alice.on_disconnected();
    let offline_run = alice
        .alloc_row_run(1)
        .ok_or_else(|| String::from("alloc_row_run failed offline"))?;
    let offline_row = RowId(offline_run.first_id);
    alice
        .push_local(
            String::from("Offline InsertRow"),
            vec![Op::InsertRows {
                sheet: SheetId(0),
                after: None,
                ids: offline_run,
            }],
            1_700_000_020_001,
        )
        .map_err(|e| format!("offline insert: {e:?}"))?;
    alice
        .push_local(
            String::from("Offline SetCell"),
            vec![Op::SetCells {
                sheet: SheetId(0),
                edits: vec![CellEdit {
                    row: offline_row,
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(888.0)),
                    format: FormatPatch::Keep,
                }],
            }],
            1_700_000_020_002,
        )
        .map_err(|e| format!("offline set: {e:?}"))?;

    let saved = SavedClientState {
        doc_id: state.wb_a,
        epoch: alice.epoch(),
        replica: alice.replica().ok_or("alice has no replica")?,
        confirmed_seq: alice.confirmed_seq(),
        last_acked_client_seq: alice.last_acked_client_seq(),
        next_id_counter: alice.next_id_counter(),
        confirmed_snapshot: encode_snapshot_v1(alice.confirmed(), &[])
            .map_err(|e| format!("encode t1 snap: {e:?}"))?,
        pending: alice.pending_wire_txns(),
    };
    let encoded = postcard::to_allocvec(&saved).map_err(|e| format!("encode saved: {e}"))?;
    fs::write(alice_state_path, encoded).map_err(|e| format!("write alice-state: {e}"))?;
    println!(
        "advance-t1 complete: confirmed_seq={} epoch={} replica={} pending={}",
        saved.confirmed_seq,
        saved.epoch,
        saved.replica,
        saved.pending.len()
    );
    Ok(())
}

async fn verify_restore(args: &Args) -> AppResult<()> {
    let alice_state_path = args
        .alice_state_path
        .as_deref()
        .ok_or_else(|| String::from("--alice-state is required for verify-restore"))?;
    let state: DrillState = serde_json::from_str(
        &fs::read_to_string(&args.state_path).map_err(|e| format!("read state: {e}"))?,
    )
    .map_err(|e| format!("parse state: {e}"))?;
    let saved_bytes = fs::read(alice_state_path).map_err(|e| format!("read alice-state: {e}"))?;
    let saved: SavedClientState =
        postcard::from_bytes(&saved_bytes).map_err(|e| format!("decode alice-state: {e}"))?;

    let http = http_client()?;
    wait_endpoints(&http, &args.base_url, args.metrics_url.as_deref()).await?;

    // 1. Verify named versions survived restore on both workbooks, and Workbook B digest matches T0.
    let (_, vers_a) = api_json(
        &http,
        Method::GET,
        &args.base_url,
        &format!("/api/workbooks/{}/versions", state.wb_a),
        &state.alice_cookie,
        None,
    )
    .await?;
    if !vers_a
        .as_array()
        .is_some_and(|arr| arr.iter().any(|v| v["name"] == "T0 Baseline"))
    {
        return Err(format!("missing T0 Baseline version on dst: {vers_a}"));
    }

    let (_, vers_b) = api_json(
        &http,
        Method::GET,
        &args.base_url,
        &format!("/api/workbooks/{}/versions", state.wb_b),
        &state.alice_cookie,
        None,
    )
    .await?;
    if !vers_b
        .as_array()
        .is_some_and(|arr| arr.iter().any(|v| v["name"] == "B Baseline"))
    {
        return Err(format!("missing B Baseline version on dst: {vers_b}"));
    }

    let ctx_b = ClientCtx {
        http: &http,
        base_url: &args.base_url,
        cookie: &state.alice_cookie,
        doc: state.wb_b,
    };
    let mut check_b = ClientSyncMachine::new(state.wb_b.into_bytes(), MAIN_BRANCH.into_bytes());
    let (ws_check_b, _) = connect_and_catchup(ctx_b, &mut check_b).await?;
    drop(ws_check_b);
    if check_b.confirmed_seq() != state.t0_seq_b
        || check_b.confirmed().state_digest() != state.t0_digest_b
    {
        return Err(String::from("restored Workbook B did not match T0 digest"));
    }

    // 2. Reconnect Alice's T1-ahead ClientSyncMachine to the restored T0 server (epoch 0 -> 1).
    let ctx_alice = ClientCtx {
        http: &http,
        base_url: &args.base_url,
        cookie: &state.alice_cookie,
        doc: state.wb_a,
    };
    let decoded_t1 = decode_snapshot_any(&saved.confirmed_snapshot)
        .map_err(|e| format!("decode t1 snap: {e:?}"))?;
    let mut alice = ClientSyncMachine::from_persisted(
        saved.doc_id.into_bytes(),
        MAIN_BRANCH.into_bytes(),
        saved.epoch,
        Some(saved.replica),
        saved.confirmed_seq,
        saved.last_acked_client_seq,
        decoded_t1.engine.workbook().clone(),
        saved.pending,
        saved.next_id_counter,
    );
    let (mut ws_alice, mut rep_alice) = connect_and_catchup(ctx_alice, &mut alice).await?;

    if rep_alice.epoch_changed != Some((0, 1)) {
        return Err(format!(
            "expected EpochChanged(0, 1), got {:?}",
            rep_alice.epoch_changed
        ));
    }
    let alice_new_rep = alice.replica().ok_or("alice missing new epoch=1 replica")?;
    let (alice_ep, _) = unpack_replica_id(alice_new_rep);
    if alice_ep != 1 || alice_new_rep == saved.replica {
        return Err(format!(
            "expected fresh epoch=1 replica != {}, got {alice_new_rep} (epoch={alice_ep})",
            saved.replica
        ));
    }
    if rep_alice.pre_pending_digest != Some(state.t0_digest_a) {
        return Err(String::from(
            "restored T0 baseline workbook digest after snapshot/txns fallback did not match T0",
        ));
    }

    // Drain Alice's 2 remapped offline edits onto dst.
    wait_until_settled(
        ctx_alice,
        &mut ws_alice,
        &mut alice,
        state.t0_seq_a + 2,
        &mut rep_alice,
    )
    .await?;

    let mut saw_epoch1_row = false;
    for op in &rep_alice.committed_ops {
        if let WireOpV2::InsertRows { ids, .. } = op {
            let (row_epoch, row_rep, _) = unpack_id(ids.first_id);
            if row_epoch != 1 || row_rep != alice_new_rep {
                return Err(format!(
                    "expected remapped InsertRows ID in epoch=1 replica={alice_new_rep}, got epoch={row_epoch} rep={row_rep}"
                ));
            }
            saw_epoch1_row = true;
        }
    }
    if !saw_epoch1_row {
        return Err(String::from("remapped InsertRows commit missing on dst"));
    }

    // 3. Connect Bob on dst, verify distinct epoch=1 replica, push post-restore edit, and verify 3-way convergence.
    let ctx_bob = ClientCtx {
        http: &http,
        base_url: &args.base_url,
        cookie: &state.bob_cookie,
        doc: state.wb_a,
    };
    let mut bob = ClientSyncMachine::new(state.wb_a.into_bytes(), MAIN_BRANCH.into_bytes());
    let (mut ws_bob, mut rep_bob) = connect_and_catchup(ctx_bob, &mut bob).await?;
    let bob_rep = bob.replica().ok_or("bob missing replica on dst")?;
    if unpack_replica_id(bob_rep).0 != 1 || bob_rep == alice_new_rep {
        return Err(format!(
            "expected distinct epoch=1 replica for Bob, got {bob_rep} (alice={alice_new_rep})"
        ));
    }

    let bob_run = bob.alloc_row_run(1).ok_or("bob alloc_row_run failed")?;
    let (bob_row_ep, bob_row_rep, _) = unpack_id(bob_run.first_id);
    if bob_row_ep != 1 || bob_row_rep != bob_rep {
        return Err(format!(
            "expected Bob new RowId in epoch=1 replica={bob_rep}, got epoch={bob_row_ep} rep={bob_row_rep}"
        ));
    }
    let bob_actions = bob
        .push_local(
            String::from("Bob post-restore edit"),
            vec![
                Op::InsertRows {
                    sheet: SheetId(0),
                    after: None,
                    ids: bob_run,
                },
                Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(bob_run.first_id),
                        col: ColId(3),
                        input: CellInput::Scalar(ScalarValue::Number(1234.5)),
                        format: FormatPatch::Keep,
                    }],
                },
            ],
            1_700_000_030_000,
        )
        .map_err(|e| format!("bob push_local: {e:?}"))?;
    run_client_actions(ctx_bob, &mut ws_bob, &mut bob, bob_actions, &mut rep_bob).await?;

    let final_seq = state.t0_seq_a + 3;
    wait_until_settled(ctx_bob, &mut ws_bob, &mut bob, final_seq, &mut rep_bob).await?;
    wait_until_settled(
        ctx_alice,
        &mut ws_alice,
        &mut alice,
        final_seq,
        &mut rep_alice,
    )
    .await?;
    drop(ws_alice);
    drop(ws_bob);

    // Verify fresh catchup from dst Postgres + snapshots agrees bit-for-bit with Alice and Bob.
    let mut observer = ClientSyncMachine::new(state.wb_a.into_bytes(), MAIN_BRANCH.into_bytes());
    let (ws_obs, _) = connect_and_catchup(ctx_alice, &mut observer).await?;
    drop(ws_obs);

    let d_alice = alice.confirmed().state_digest();
    let d_bob = bob.confirmed().state_digest();
    let d_obs = observer.confirmed().state_digest();
    if d_alice != d_bob || d_bob != d_obs {
        return Err(format!(
            "post-restore StateDigest mismatch: alice={d_alice:?} bob={d_bob:?} observer={d_obs:?}"
        ));
    }

    println!(
        "verify-restore complete: epoch 0->1, alice_rep={alice_new_rep}, bob_rep={bob_rep}, seq={final_seq}, digest={d_obs:?}"
    );
    Ok(())
}
