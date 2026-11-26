#![cfg(debug_assertions)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use reqwest::{Method, Response, StatusCode, header};
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_server::agent::AgentRuntimeConfig;
use reticula_server::http::{self, ApiState, AuthConfig};
use reticula_server::ops::GovernanceConfig;
use reticula_server::store::{DELETED_USER_ID, MAIN_BRANCH, StorageLimits, Store};
use reticula_sync::VIEWER_REPLICA;
use reticula_wire::convert_v3::ops_to_wire_txn_v3;
use reticula_wire::sync_frame::{decode_sync_frame_v2, encode_sync_frame_v2};
use reticula_wire::v2::WireRejectReasonV1;
use reticula_wire::v3::{SYNC_PROTOCOL_VERSION_V2, WirePendingTxnV3, WireSyncFrameV2};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

const ORIGIN: &str = "http://127.0.0.1";
const WAIT: Duration = Duration::from_secs(15);
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct IsolatedDb {
    admin: PgPool,
    name: String,
    url: String,
}

impl IsolatedDb {
    async fn create() -> Self {
        let admin_url = std::env::var("RETICULA_TEST_DATABASE_URL")
            .expect("RETICULA_TEST_DATABASE_URL must be set");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&admin_url)
            .await
            .unwrap();
        let name = format!("reticula_m4_{}", Uuid::new_v4().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let mut parsed = Url::parse(&admin_url).unwrap();
        parsed.set_path(&format!("/{name}"));
        Self {
            admin,
            name,
            url: parsed.to_string(),
        }
    }

    async fn cleanup(self) {
        let _ = sqlx::query(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
        self.admin.close().await;
    }
}

struct TestServer {
    _root: Option<TempDir>,
    store: Store,
    api: ApiState,
    base: String,
    client: reqwest::Client,
    handle: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl TestServer {
    async fn start(
        db: &IsolatedDb,
        limits: StorageLimits,
        gov: GovernanceConfig,
        agent_cfg: AgentRuntimeConfig,
    ) -> Self {
        Self::start_with_root(db, tempfile::tempdir().unwrap(), limits, gov, agent_cfg).await
    }

    async fn start_with_root(
        db: &IsolatedDb,
        root: TempDir,
        limits: StorageLimits,
        gov: GovernanceConfig,
        agent_cfg: AgentRuntimeConfig,
    ) -> Self {
        let store = Store::connect_with_limits(&db.url, root.path().to_path_buf(), limits)
            .await
            .unwrap();
        let auth = AuthConfig::dev_mock(ORIGIN).unwrap();
        let api = ApiState::with_agent(store.clone(), auth, gov, agent_cfg);
        api.agent().recover_interrupted_runs().await.unwrap();
        let app = Router::new()
            .nest("/api", http::router(api.clone()))
            .merge(http::metrics_route(Some(api.clone())));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(WAIT)
            .build()
            .unwrap();
        Self {
            _root: Some(root),
            store,
            api,
            base: format!("http://{addr}"),
            client,
            handle,
        }
    }

    fn stop_keeping_root(mut self) -> TempDir {
        self.handle.abort();
        self._root.take().expect("root dir")
    }

    async fn dev_login(&self, account: &str) -> (String, Uuid) {
        let resp = self
            .client
            .post(format!("{}/api/auth/dev-login", self.base))
            .header(header::ORIGIN, ORIGIN)
            .json(&json!({ "account": account }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|raw| raw.split(';').next().map(str::to_owned))
            .expect("session cookie");
        let user: Value = resp.json().await.unwrap();
        let uid = Uuid::parse_str(user["id"].as_str().unwrap()).unwrap();
        (cookie, uid)
    }

    async fn req(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        share: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut builder = self
            .client
            .request(method, format!("{}{}", self.base, path));
        builder = builder.header(header::ORIGIN, ORIGIN);
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        if let Some(s) = share {
            builder = builder.header("x-reticula-share", s);
        }
        if let Some(b) = body {
            builder = builder.json(&b);
        }
        builder.send().await.unwrap()
    }

    async fn create_workbook(&self, cookie: &str, title: &str) -> Uuid {
        let resp = self
            .req(
                Method::POST,
                "/api/workbooks",
                Some(cookie),
                None,
                Some(json!({ "title": title })),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: Value = resp.json().await.unwrap();
        Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
    }

    async fn connect_ws(
        &self,
        doc: Uuid,
        branch: Option<Uuid>,
        cookie: Option<&str>,
        share: Option<&str>,
    ) -> Result<Socket, StatusCode> {
        let mut qs = Vec::new();
        if let Some(b) = branch {
            qs.push(format!("branch={b}"));
        }
        if let Some(s) = share {
            qs.push(format!("share={s}"));
        }
        let suffix = if qs.is_empty() {
            String::new()
        } else {
            format!("?{}", qs.join("&"))
        };
        let ws_url = format!(
            "{}/api/sync/{}{}",
            self.base.replace("http://", "ws://"),
            doc,
            suffix
        );
        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut()
            .insert(header::ORIGIN, ORIGIN.parse().unwrap());
        if let Some(c) = cookie {
            req.headers_mut().insert(header::COOKIE, c.parse().unwrap());
        }
        match connect_async(req).await {
            Ok((socket, _)) => Ok(socket),
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status()),
            Err(err) => panic!("unexpected ws error: {err}"),
        }
    }
}

async fn ws_send_frame(ws: &mut Socket, frame: &WireSyncFrameV2) {
    let bytes = encode_sync_frame_v2(frame).unwrap();
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

async fn ws_recv_frame(ws: &mut Socket) -> WireSyncFrameV2 {
    loop {
        let msg = timeout(WAIT, ws.next())
            .await
            .expect("ws frame timeout")
            .expect("ws stream open")
            .expect("ws message ok");
        if let Message::Binary(bytes) = msg {
            return decode_sync_frame_v2(&bytes).unwrap();
        }
    }
}

async fn wait_run_status(
    server: &TestServer,
    cookie: &str,
    doc: Uuid,
    run_id: Uuid,
    target: &str,
) -> Value {
    for _ in 0..80 {
        let resp = server
            .req(
                Method::GET,
                &format!("/api/workbooks/{doc}/agent/runs/{run_id}"),
                Some(cookie),
                None,
                None,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let detail: Value = resp.json().await.unwrap();
        if detail["run"]["status"].as_str() == Some(target) {
            return detail;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for agent run {run_id} to reach status `{target}`");
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn permission_matrix_six_identities_across_all_12_m4_routes_and_branch_ws() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(
        &db,
        StorageLimits::default(),
        GovernanceConfig::default(),
        AgentRuntimeConfig::mock_for_tests(),
    )
    .await;

    let (alice_cookie, _alice_id) = server.dev_login("alice").await;
    let (bob_cookie, bob_id) = server.dev_login("bob").await;
    let (carol_cookie, _carol_id) = server.dev_login("carol").await;
    let doc = server
        .create_workbook(&alice_cookie, "M4 Permissions")
        .await;

    // Create a viewer anonymous share link + link_view visibility.
    let share_resp = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/share-links"),
            Some(&alice_cookie),
            None,
            Some(json!({ "role": "viewer", "label": "anon-view" })),
        )
        .await;
    assert_eq!(share_resp.status(), StatusCode::CREATED);
    let share_url = share_resp.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let share_token = share_url.split("share=").nth(1).unwrap().to_owned();
    let vis_resp = server
        .req(
            Method::PATCH,
            &format!("/api/workbooks/{doc}"),
            Some(&alice_cookie),
            None,
            Some(json!({ "visibility": "link_view" })),
        )
        .await;
    assert_eq!(vis_resp.status(), StatusCode::OK);

    // Add Bob as editor, then we'll also test Bob downgraded to viewer!
    let ed_link = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/share-links"),
            Some(&alice_cookie),
            None,
            Some(json!({ "role": "editor", "label": "ed" })),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let ed_token = ed_link["url"]
        .as_str()
        .unwrap()
        .trim_start_matches("/join/");
    assert_eq!(
        server
            .req(
                Method::POST,
                &format!("/api/join/{ed_token}"),
                Some(&bob_cookie),
                None,
                None,
            )
            .await
            .status(),
        StatusCode::OK
    );

    // Alice starts a run so we have a real draft branch + run_id + snapshot seq=0.
    let run_resp = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "Summarize revenue" })),
        )
        .await;
    assert_eq!(run_resp.status(), StatusCode::CREATED);
    let run: Value = run_resp.json().await.unwrap();
    let run_id = Uuid::parse_str(run["id"].as_str().unwrap()).unwrap();
    let branch_id = Uuid::parse_str(run["branchId"].as_str().unwrap()).unwrap();
    let _ = wait_run_status(&server, &alice_cookie, doc, run_id, "waiting_review").await;

    // 1. Unauthenticated -> 401 on all 12 routes + branch WS.
    for (method, path, body) in [
        (Method::GET, format!("/api/workbooks/{doc}/branches"), None),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/branches"),
            Some(json!({ "title": "x" })),
        ),
        (
            Method::DELETE,
            format!("/api/workbooks/{doc}/branches/{branch_id}"),
            None,
        ),
        (
            Method::GET,
            format!("/api/workbooks/{doc}/branches/{branch_id}/snapshots/0"),
            None,
        ),
        (
            Method::GET,
            format!("/api/workbooks/{doc}/branches/{branch_id}/diff"),
            None,
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/branches/{branch_id}/merge/preview"),
            Some(json!({ "acceptedHunks": [] })),
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
            Some(json!({ "acceptedHunks": [] })),
        ),
        (
            Method::GET,
            format!("/api/workbooks/{doc}/agent/runs"),
            None,
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/agent/runs"),
            Some(json!({ "prompt": "hi" })),
        ),
        (
            Method::GET,
            format!("/api/workbooks/{doc}/agent/runs/{run_id}"),
            None,
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/agent/runs/{run_id}/cancel"),
            None,
        ),
        (
            Method::GET,
            format!("/api/workbooks/{doc}/agent/runs/{run_id}/events"),
            None,
        ),
    ] {
        let st = server
            .req(method.clone(), &path, None, None, body.clone())
            .await
            .status();
        assert_eq!(st, StatusCode::UNAUTHORIZED, "unauth {method} {path}");
        // Anonymous share link -> 403 on all 12 routes!
        let anon_st = server
            .req(
                method.clone(),
                &path,
                None,
                Some(&share_token),
                body.clone(),
            )
            .await
            .status();
        assert_eq!(anon_st, StatusCode::FORBIDDEN, "anon {method} {path}");
        // Signed-in non-member Carol -> 403 on all 12 routes!
        let carol_st = server
            .req(method.clone(), &path, Some(&carol_cookie), None, body)
            .await
            .status();
        assert_eq!(
            carol_st,
            StatusCode::FORBIDDEN,
            "non-member {method} {path}"
        );
    }

    // Anonymous WS rejected on non-main branch (403); unauth (401); non-member Carol (403).
    assert_eq!(
        server
            .connect_ws(doc, Some(branch_id), None, None)
            .await
            .unwrap_err(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .connect_ws(doc, Some(branch_id), None, Some(&share_token))
            .await
            .unwrap_err(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .connect_ws(doc, Some(branch_id), Some(&carol_cookie), None)
            .await
            .unwrap_err(),
        StatusCode::FORBIDDEN
    );

    // 2. Editor Bob CANNOT cancel Alice's run or abandon Alice's branch (403)!
    assert_eq!(
        server
            .req(
                Method::POST,
                &format!("/api/workbooks/{doc}/agent/runs/{run_id}/cancel"),
                Some(&bob_cookie),
                None,
                None,
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .req(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/branches/{branch_id}"),
                Some(&bob_cookie),
                None,
                None,
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    // 3. Editor Bob CAN create and abandon his own branch!
    let bob_br_resp = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/branches"),
            Some(&bob_cookie),
            None,
            Some(json!({ "kind": "user", "title": "Bob scratch" })),
        )
        .await;
    assert_eq!(bob_br_resp.status(), StatusCode::CREATED);
    let bob_bid = Uuid::parse_str(
        bob_br_resp.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        server
            .req(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/branches/{bob_bid}"),
                Some(&bob_cookie),
                None,
                None,
            )
            .await
            .status(),
        StatusCode::OK
    );

    // 4. Downgrade Bob to Viewer -> GET/SSE/branch-snapshot/WS return 200/101, mutations return 403!
    assert_eq!(
        server
            .req(
                Method::PATCH,
                &format!("/api/workbooks/{doc}/members/{bob_id}"),
                Some(&alice_cookie),
                None,
                Some(json!({ "role": "viewer" })),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    for path in [
        format!("/api/workbooks/{doc}/branches"),
        format!("/api/workbooks/{doc}/branches/{branch_id}/snapshots/0"),
        format!("/api/workbooks/{doc}/branches/{branch_id}/diff"),
        format!("/api/workbooks/{doc}/agent/runs"),
        format!("/api/workbooks/{doc}/agent/runs/{run_id}"),
    ] {
        assert_eq!(
            server
                .req(Method::GET, &path, Some(&bob_cookie), None, None)
                .await
                .status(),
            StatusCode::OK,
            "viewer GET {path}"
        );
    }
    let sse_resp = server
        .req(
            Method::GET,
            &format!("/api/workbooks/{doc}/agent/runs/{run_id}/events"),
            Some(&bob_cookie),
            None,
            None,
        )
        .await;
    assert_eq!(sse_resp.status(), StatusCode::OK);
    assert_eq!(
        sse_resp
            .headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );
    for (method, path, body) in [
        (
            Method::POST,
            format!("/api/workbooks/{doc}/branches"),
            Some(json!({ "title": "v" })),
        ),
        (
            Method::DELETE,
            format!("/api/workbooks/{doc}/branches/{branch_id}"),
            None,
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/branches/{branch_id}/merge/preview"),
            Some(json!({ "acceptedHunks": [] })),
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
            Some(json!({ "acceptedHunks": [] })),
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/agent/runs"),
            Some(json!({ "prompt": "hi" })),
        ),
        (
            Method::POST,
            format!("/api/workbooks/{doc}/agent/runs/{run_id}/cancel"),
            None,
        ),
    ] {
        assert_eq!(
            server
                .req(method.clone(), &path, Some(&bob_cookie), None, body)
                .await
                .status(),
            StatusCode::FORBIDDEN,
            "viewer write {method} {path}"
        );
    }

    // 5. Even Owner connecting WS to an `ai_draft` branch is clamped to VIEWER_REPLICA = 0 and cannot Push!
    let mut draft_ws = server
        .connect_ws(doc, Some(branch_id), Some(&alice_cookie), None)
        .await
        .expect("owner branch ws");
    ws_send_frame(
        &mut draft_ws,
        &WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: doc.into_bytes(),
            branch_id: branch_id.into_bytes(),
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        },
    )
    .await;
    let welcome = ws_recv_frame(&mut draft_ws).await;
    match welcome {
        WireSyncFrameV2::Welcome { replica, .. } => assert_eq!(replica, VIEWER_REPLICA),
        other => panic!("expected Welcome, got {other:?}"),
    }
    let txn = ops_to_wire_txn_v3(
        "forbidden write".into(),
        &[Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Number(1.0)),
                format: FormatPatch::Keep,
            }],
        }],
    )
    .unwrap();
    ws_send_frame(
        &mut draft_ws,
        &WireSyncFrameV2::Push {
            txns: vec![WirePendingTxnV3 {
                replica: VIEWER_REPLICA,
                client_seq: 1,
                base_server_seq: 0,
                timestamp_ms: 1,
                txn,
            }],
        },
    )
    .await;
    let reject = ws_recv_frame(&mut draft_ws).await;
    assert!(matches!(
        reject,
        WireSyncFrameV2::Reject {
            reason: WireRejectReasonV1::Forbidden,
            ..
        }
    ));

    // Owner can abandon the draft branch.
    assert_eq!(
        server
            .req(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/branches/{branch_id}"),
                Some(&alice_cookie),
                None,
                None,
            )
            .await
            .status(),
        StatusCode::OK
    );

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn end_to_end_ai_run_conflict_closure_and_live_merge_to_main() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(
        &db,
        StorageLimits::default(),
        GovernanceConfig::default(),
        AgentRuntimeConfig::mock_for_tests(),
    )
    .await;

    let (alice_cookie, _) = server.dev_login("alice").await;
    let doc = server
        .create_workbook(&alice_cookie, "Conflict & Closure")
        .await;

    // Connect Alice on `main` WS to seed B1=10 and later observe live merged commits!
    let mut main_ws = server
        .connect_ws(doc, None, Some(&alice_cookie), None)
        .await
        .expect("main ws");
    ws_send_frame(
        &mut main_ws,
        &WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: doc.into_bytes(),
            branch_id: MAIN_BRANCH.into_bytes(),
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        },
    )
    .await;
    let main_replica = match ws_recv_frame(&mut main_ws).await {
        WireSyncFrameV2::Welcome { replica, .. } => replica,
        other => panic!("expected Welcome, got {other:?}"),
    };

    // Seed A1="Item", B1=10 on main.
    let seed_txn = ops_to_wire_txn_v3(
        "seed".into(),
        &[Op::SetCells {
            sheet: SheetId(0),
            edits: vec![
                CellEdit {
                    row: RowId(0),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Text("Item".into())),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(0),
                    col: ColId(1),
                    input: CellInput::Scalar(ScalarValue::Number(10.0)),
                    format: FormatPatch::Keep,
                },
            ],
        }],
    )
    .unwrap();
    ws_send_frame(
        &mut main_ws,
        &WireSyncFrameV2::Push {
            txns: vec![WirePendingTxnV3 {
                replica: main_replica,
                client_seq: 1,
                base_server_seq: 0,
                timestamp_ms: 100,
                txn: seed_txn,
            }],
        },
    )
    .await;
    // Drain Ack + Commit.
    let _ = ws_recv_frame(&mut main_ws).await;
    let _ = ws_recv_frame(&mut main_ws).await;

    let main_est_before = server.store.workbook_est_bytes(doc).await.unwrap();

    // Start `[closure]` mock AI run (inserts row 2 with A2=25 and sets B1 = =A2*2).
    let run_resp = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "[closure] Insert row 2 and set B1 = =A2*2" })),
        )
        .await;
    assert_eq!(run_resp.status(), StatusCode::CREATED);
    let run: Value = run_resp.json().await.unwrap();
    let run_id = Uuid::parse_str(run["id"].as_str().unwrap()).unwrap();
    let branch_id = Uuid::parse_str(run["branchId"].as_str().unwrap()).unwrap();
    let _ = wait_run_status(&server, &alice_cookie, doc, run_id, "waiting_review").await;

    // Verify workbooks.est_bytes was NOT overwritten by draft branch commits!
    assert_eq!(
        server.store.workbook_est_bytes(doc).await.unwrap(),
        main_est_before
    );
    assert!(server.store.branch_est_bytes(doc, branch_id).await.unwrap() > 0);

    // Concurrent edit on `main`: change B1 from 10 to 999 so B1 becomes a true 3-way conflict!
    let conflict_txn = ops_to_wire_txn_v3(
        "concurrent main edit".into(),
        &[Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(999.0)),
                format: FormatPatch::Keep,
            }],
        }],
    )
    .unwrap();
    ws_send_frame(
        &mut main_ws,
        &WireSyncFrameV2::Push {
            txns: vec![WirePendingTxnV3 {
                replica: main_replica,
                client_seq: 2,
                base_server_seq: 1,
                timestamp_ms: 200,
                txn: conflict_txn,
            }],
        },
    )
    .await;
    let _ = ws_recv_frame(&mut main_ws).await;
    let _ = ws_recv_frame(&mut main_ws).await;

    // Fetch 3-way diff.
    let diff: Value = server
        .req(
            Method::GET,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/diff"),
            Some(&alice_cookie),
            None,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(diff["summary"]["structuralHunks"].as_u64().unwrap() >= 1);
    assert!(diff["summary"]["conflictCells"].as_u64().unwrap() >= 1);

    let hunks = diff["hunks"].as_array().unwrap();
    let struct_id = hunks.iter().find(|h| h["kind"] == "structural").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let conflict_hunk = hunks.iter().find(|h| h["hasConflict"] == true).unwrap();
    let conflict_hunk_id = conflict_hunk["id"].as_str().unwrap().to_owned();
    let conflict_cell_key = conflict_hunk["cells"][0]["cellKey"]
        .as_str()
        .unwrap()
        .to_owned();

    // 1. Merge without conflict resolution -> 409 UNRESOLVED_CONFLICTS with structured JSON!
    let c1 = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
            Some(&alice_cookie),
            None,
            Some(json!({
                "acceptedHunks": [struct_id, conflict_hunk_id],
                "conflictChoices": {},
                "allowRefErrors": false
            })),
        )
        .await;
    assert_eq!(c1.status(), StatusCode::CONFLICT);
    let c1_body: Value = c1.json().await.unwrap();
    assert_eq!(c1_body["code"], "UNRESOLVED_CONFLICTS");
    assert!(
        !c1_body["unresolvedConflicts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // 2. Accept conflicting formula B1 (=A2*2) while rejecting inserted row 2 -> 409 CLOSURE_BLOCKED!
    let mut take_draft = BTreeMap::new();
    take_draft.insert(conflict_cell_key.clone(), "take_draft");
    let c2 = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
            Some(&alice_cookie),
            None,
            Some(json!({
                "acceptedHunks": [conflict_hunk_id],
                "conflictChoices": take_draft,
                "allowRefErrors": false
            })),
        )
        .await;
    assert_eq!(c2.status(), StatusCode::CONFLICT);
    let c2_body: Value = c2.json().await.unwrap();
    assert_eq!(c2_body["code"], "CLOSURE_BLOCKED");
    assert!(!c2_body["closureIssues"].as_array().unwrap().is_empty());

    // 3. Preview with both structural insert + TakeDraft -> canMerge == true!
    let preview: Value = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/merge/preview"),
            Some(&alice_cookie),
            None,
            Some(json!({
                "acceptedHunks": [struct_id, conflict_hunk_id],
                "conflictChoices": take_draft,
                "allowRefErrors": false
            })),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(preview["canMerge"], true);

    // 4. Execute merge -> 200 OK, main WS immediately receives live Commit!
    let merged = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
            Some(&alice_cookie),
            None,
            Some(json!({
                "acceptedHunks": [struct_id, conflict_hunk_id],
                "conflictChoices": take_draft,
                "allowRefErrors": false
            })),
        )
        .await;
    assert_eq!(merged.status(), StatusCode::OK);
    let merged_body: Value = merged.json().await.unwrap();
    assert!(merged_body["committedOps"].as_u64().unwrap() >= 2);

    // Verify connected main WS receives Commit from the merge replica!
    let live_commit = ws_recv_frame(&mut main_ws).await;
    assert!(matches!(live_commit, WireSyncFrameV2::Commit { .. }));

    // Verify branch status is now `merged` and second merge returns 409!
    let branch_row = server
        .store
        .get_branch(doc, branch_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(branch_row.status, "merged");
    assert!(branch_row.merge_seq.is_some());
    assert_eq!(
        server
            .req(
                Method::POST,
                &format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
                Some(&alice_cookie),
                None,
                Some(json!({ "acceptedHunks": [] })),
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );

    // Verify Prometheus `/metrics` reports M4 counters!
    let metrics_text = server.api.render_metrics();
    assert!(metrics_text.contains("reticula_agent_runs_active"));
    assert!(metrics_text.contains("reticula_agent_runs_total{status=\"merged\"} 1"));
    assert!(metrics_text.contains("reticula_draft_rooms_active"));

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn quotas_shared_budget_and_delete_account_during_active_run() {
    let db = IsolatedDb::create().await;
    // Tight budget (200 KiB) & daily_run_limit = 1 to verify both enforcement paths deterministically.
    let mut agent_cfg = AgentRuntimeConfig::mock_for_tests();
    agent_cfg.daily_run_limit = 1;
    let gov = GovernanceConfig {
        max_loaded_bytes: 200 * 1024,
        ..GovernanceConfig::default()
    };
    let server = TestServer::start(&db, StorageLimits::default(), gov, agent_cfg).await;

    let (alice_cookie, _) = server.dev_login("alice").await;
    let (bob_cookie, bob_id) = server.dev_login("bob").await;
    let doc = server
        .create_workbook(&alice_cookie, "Quota & Delete")
        .await;

    // Invite Bob as editor.
    let link = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/share-links"),
            Some(&alice_cookie),
            None,
            Some(json!({ "role": "editor", "label": "b" })),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let tok = link["url"].as_str().unwrap().trim_start_matches("/join/");
    assert_eq!(
        server
            .req(
                Method::POST,
                &format!("/api/join/{tok}"),
                Some(&bob_cookie),
                None,
                None
            )
            .await
            .status(),
        StatusCode::OK
    );

    // 1. Hold two active main rooms (2 * 64 KiB = 128 KiB), leaving only 72 KiB < 128 KiB needed for a draft engine lease -> 503 Busy!
    let doc2 = server.create_workbook(&alice_cookie, "Hold RAM").await;
    let mut ws1 = server
        .connect_ws(doc, None, Some(&alice_cookie), None)
        .await
        .unwrap();
    ws_send_frame(
        &mut ws1,
        &WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: doc.into_bytes(),
            branch_id: MAIN_BRANCH.into_bytes(),
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        },
    )
    .await;
    let _ = ws_recv_frame(&mut ws1).await;

    let mut ws2 = server
        .connect_ws(doc2, None, Some(&alice_cookie), None)
        .await
        .unwrap();
    ws_send_frame(
        &mut ws2,
        &WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: doc2.into_bytes(),
            branch_id: MAIN_BRANCH.into_bytes(),
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        },
    )
    .await;
    let _ = ws_recv_frame(&mut ws2).await;

    let busy_resp = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "Should fail with 503 under 200 KiB budget" })),
        )
        .await;
    assert_eq!(busy_resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Drop ws2 so doc2 becomes idle and can be evicted LRU to make room for the 2x draft lease!
    drop(ws2);
    sleep(Duration::from_millis(80)).await;

    // 2. Alice's first run now succeeds (evicting idle doc2) and consumes her 1/1 daily quota.
    let run1 = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "First run within quota" })),
        )
        .await;
    assert_eq!(run1.status(), StatusCode::CREATED);
    let r1_id =
        Uuid::parse_str(run1.json::<Value>().await.unwrap()["id"].as_str().unwrap()).unwrap();
    let _ = wait_run_status(&server, &alice_cookie, doc, r1_id, "waiting_review").await;

    // Alice's second run exceeds daily_run_limit (1) -> 403!
    let run2 = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "Second run should hit daily quota" })),
        )
        .await;
    assert_eq!(run2.status(), StatusCode::FORBIDDEN);

    // Account summary exposes AI quota stats accurately.
    let acct: Value = server
        .req(Method::GET, "/api/account", Some(&alice_cookie), None, None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(acct["aiEnabled"], true);
    assert_eq!(acct["aiRunsToday"], 1);
    assert_eq!(acct["aiRunLimitDaily"], 1);
    assert!(acct["aiTokensUsedToday"].as_u64().unwrap() > 0);

    // 3. Bob starts a `[slow]` run on Alice's shared workbook, then immediately deletes his own account!
    let bob_run = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&bob_cookie),
            None,
            Some(json!({ "prompt": "[slow] Running when Bob deletes account" })),
        )
        .await;
    assert_eq!(bob_run.status(), StatusCode::CREATED);
    let bob_run_id = Uuid::parse_str(
        bob_run.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();

    let del_resp = server
        .req(
            Method::DELETE,
            "/api/account",
            Some(&bob_cookie),
            None,
            Some(json!({ "confirm": "DELETE" })),
        )
        .await;
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);

    // Alice's workbook survives, Bob's run is migrated to DELETED_USER_ID, and Bob's quota rows are gone!
    let migrated = server
        .store
        .get_agent_run(doc, bob_run_id)
        .await
        .unwrap()
        .expect("run preserved on shared workbook");
    assert_eq!(migrated.user_id, DELETED_USER_ID);
    assert_eq!(migrated.status, "cancelled");
    assert_eq!(
        server
            .store
            .get_daily_quota(bob_id)
            .await
            .unwrap()
            .agent_runs,
        0
    );

    drop(ws1);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn sigkill_real_child_process_recovery_resume_and_clean_fail() {
    let db = IsolatedDb::create().await;
    let data_dir = tempfile::tempdir().unwrap();
    let static_dir = tempfile::tempdir().unwrap();
    std::fs::write(static_dir.path().join("index.html"), "<!doctype html>").unwrap();

    // Pick a free ephemeral localhost port.
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let base = format!("http://{addr}");
    let bin = env!("CARGO_BIN_EXE_reticula-server");

    let spawn_server = || {
        Command::new(bin)
            .env("BIND_ADDR", addr.to_string())
            .env("RETICULA_STATIC_DIR", static_dir.path())
            .env("RETICULA_DATA_DIR", data_dir.path())
            .env("RETICULA_DATABASE_URL", &db.url)
            .env("RETICULA_PUBLIC_ORIGIN", &base)
            .env("RETICULA_AUTH_MODE", "dev-mock")
            .env("RETICULA_LLM_MODE", "mock")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn reticula-server binary")
    };

    let client = reqwest::Client::builder().timeout(WAIT).build().unwrap();

    let wait_ready = || async {
        for _ in 0..80 {
            if let Ok(resp) = client.get(format!("{base}/readyz")).send().await
                && resp.status() == StatusCode::OK
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
        panic!("server did not become ready at {base}");
    };

    // Process 1: start server, trigger `[slow]` AI run, wait for first step persisted, then SIGKILL!
    let mut child1 = spawn_server();
    wait_ready().await;

    let login = client
        .post(format!("{base}/api/auth/dev-login"))
        .header(header::ORIGIN, &base)
        .json(&json!({ "account": "alice" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|raw| raw.split(';').next().map(str::to_owned))
        .unwrap();

    let wb: Value = client
        .post(format!("{base}/api/workbooks"))
        .header(header::ORIGIN, &base)
        .header(header::COOKIE, &cookie)
        .json(&json!({ "title": "Crash Recovery Test" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let doc = Uuid::parse_str(wb["id"].as_str().unwrap()).unwrap();

    let run: Value = client
        .post(format!("{base}/api/workbooks/{doc}/agent/runs"))
        .header(header::ORIGIN, &base)
        .header(header::COOKIE, &cookie)
        .json(&json!({ "prompt": "[slow] Summarize metrics across restart" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = Uuid::parse_str(run["id"].as_str().unwrap()).unwrap();

    // Poll until a resumable `tool_result` checkpoint (user_prompt + assistant_thought +
    // tool_call + tool_result) is persisted while the run is still `running`.
    let mut ready_to_kill = false;
    let mut last_poll = Value::Null;
    for _ in 0..160 {
        let detail: Value = client
            .get(format!("{base}/api/workbooks/{doc}/agent/runs/{run_id}"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let is_resumable = detail["run"]["status"] == "running"
            && detail["steps"].as_array().is_some_and(|s| {
                s.len() >= 4
                    && s.last().and_then(|step| step["kind"].as_str()) == Some("tool_result")
            });
        last_poll = detail;
        if is_resumable {
            ready_to_kill = true;
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        ready_to_kill,
        "run never reached resumable tool_result checkpoint before SIGKILL: {last_poll}"
    );

    // Force-kill child1 immediately (SIGKILL / TerminateProcess)!
    child1.kill().await.unwrap();
    let _ = child1.wait().await;

    // Process 2: restart on the same DB + data_dir -> run MUST resume cleanly to `waiting_review`!
    let mut child2 = spawn_server();
    wait_ready().await;

    let mut resumed_detail = Value::Null;
    for _ in 0..100 {
        let detail: Value = client
            .get(format!("{base}/api/workbooks/{doc}/agent/runs/{run_id}"))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resumed_detail = detail;
        if matches!(
            resumed_detail["run"]["status"].as_str(),
            Some("waiting_review" | "failed" | "cancelled")
        ) {
            break;
        }
        sleep(Duration::from_millis(150)).await;
    }
    assert_eq!(
        resumed_detail["run"]["status"], "waiting_review",
        "interrupted run should have resumed cleanly after SIGKILL: {resumed_detail}"
    );
    assert_eq!(resumed_detail["run"]["resumeCount"], 1);

    // Stop child2, force resume_count = 2 on a `running` run, restart child3 -> MUST cleanly fail!
    child2.kill().await.unwrap();
    let _ = child2.wait().await;

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&db.url)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_runs SET status = 'running', resume_count = 2 WHERE id = $1")
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let mut child3 = spawn_server();
    wait_ready().await;

    let failed_detail: Value = client
        .get(format!("{base}/api/workbooks/{doc}/agent/runs/{run_id}"))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(failed_detail["run"]["status"], "failed");
    assert!(
        failed_detail["run"]["errorMessage"]
            .as_str()
            .unwrap_or("")
            .contains("resume limit")
    );

    child3.kill().await.unwrap();
    let _ = child3.wait().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn sse_last_event_id_concurrent_start_active_merge_guard_and_owner_delete() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(
        &db,
        StorageLimits::default(),
        GovernanceConfig::default(),
        AgentRuntimeConfig::mock_for_tests(),
    )
    .await;

    let (alice_cookie, _) = server.dev_login("alice").await;
    let (bob_cookie, _) = server.dev_login("bob").await;
    let doc = server
        .create_workbook(&alice_cookie, "SSE & Race Guards")
        .await;

    // Invite Bob as editor.
    let link: Value = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/share-links"),
            Some(&alice_cookie),
            None,
            Some(json!({ "role": "editor", "label": "b" })),
        )
        .await
        .json()
        .await
        .unwrap();
    let tok = link["url"].as_str().unwrap().trim_start_matches("/join/");
    assert_eq!(
        server
            .req(
                Method::POST,
                &format!("/api/join/{tok}"),
                Some(&bob_cookie),
                None,
                None,
            )
            .await
            .status(),
        StatusCode::OK
    );

    // 1. Concurrent start_run race for the same user: exactly one 201, one 409!
    let runs_path = format!("/api/workbooks/{doc}/agent/runs");
    let (r_a, r_b) = tokio::join!(
        server.req(
            Method::POST,
            &runs_path,
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "[slow] Concurrent race A" })),
        ),
        server.req(
            Method::POST,
            &runs_path,
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "[slow] Concurrent race B" })),
        ),
    );
    let statuses = [r_a.status(), r_b.status()];
    assert!(
        statuses.contains(&StatusCode::CREATED) && statuses.contains(&StatusCode::CONFLICT),
        "expected one 201 and one 409, got {statuses:?}"
    );
    let winner = if r_a.status() == StatusCode::CREATED {
        r_a.json::<Value>().await.unwrap()
    } else {
        r_b.json::<Value>().await.unwrap()
    };
    let run_id = Uuid::parse_str(winner["id"].as_str().unwrap()).unwrap();
    let branch_id = Uuid::parse_str(winner["branchId"].as_str().unwrap()).unwrap();

    // 2. Merging while the AI run is still active MUST return 409 Conflict!
    let early_merge = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/merge"),
            Some(&alice_cookie),
            None,
            Some(json!({ "acceptedHunks": [], "conflictChoices": {}, "allowRefErrors": false })),
        )
        .await;
    assert_eq!(early_merge.status(), StatusCode::CONFLICT);

    // 3. Stream live SSE and verify ephemeral text_delta/presence events do NOT emit `id:`
    //    (so Last-Event-ID never gets poisoned to 1_000_000+), then reconnect with `Last-Event-ID: 1`
    //    and confirm all steps with idx >= 1 are replayed cleanly.
    let sse_text = server
        .client
        .get(format!(
            "{}/api/workbooks/{doc}/agent/runs/{run_id}/events",
            server.base
        ))
        .header(header::COOKIE, &alice_cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for block in sse_text.split("\n\n") {
        if block.contains("event: text_delta") || block.contains("event: presence") {
            assert!(
                !block.lines().any(|l| l.starts_with("id:")),
                "ephemeral SSE frame must not emit id: header: {block}"
            );
        }
    }
    let replay_text = server
        .client
        .get(format!(
            "{}/api/workbooks/{doc}/agent/runs/{run_id}/events",
            server.base
        ))
        .header(header::COOKIE, &alice_cookie)
        .header("Last-Event-ID", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        replay_text.contains("event: step"),
        "reconnect with Last-Event-ID: 1 must replay subsequent steps, got: {replay_text}"
    );
    assert!(
        !replay_text.contains("\nid: 1\n") && !replay_text.starts_with("id: 1\n"),
        "step 1 must not be re-emitted when Last-Event-ID: 1"
    );

    // 4. Bob starts a slow run on Alice's OWNED workbook, then Alice deletes her account!
    let bob_run = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&bob_cookie),
            None,
            Some(json!({ "prompt": "[slow] Bob run on Alice owned doc" })),
        )
        .await;
    assert_eq!(bob_run.status(), StatusCode::CREATED);
    let del_alice = server
        .req(
            Method::DELETE,
            "/api/account",
            Some(&alice_cookie),
            None,
            Some(json!({ "confirm": "DELETE" })),
        )
        .await;
    assert_eq!(del_alice.status(), StatusCode::NO_CONTENT);

    drop(server);
    db.cleanup().await;
}

async fn main_txn_count(pool: &PgPool, doc: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM txns WHERE workbook_id = $1 AND branch_id = $2")
        .bind(doc)
        .bind(MAIN_BRANCH)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn a_merge_that_fails_leaves_main_as_it_was_and_lands_once_when_retried() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(
        &db,
        StorageLimits::default(),
        GovernanceConfig::default(),
        AgentRuntimeConfig::mock_for_tests(),
    )
    .await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url)
        .await
        .unwrap();

    let (cookie, _) = server.dev_login("alice").await;
    let doc = server.create_workbook(&cookie, "Merge Once").await;
    let run: Value = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&cookie),
            None,
            Some(json!({ "prompt": "[conflict] Update A2 and add an uplift in B2" })),
        )
        .await
        .json()
        .await
        .unwrap();
    let run_id = Uuid::parse_str(run["id"].as_str().unwrap()).unwrap();
    let branch_id = Uuid::parse_str(run["branchId"].as_str().unwrap()).unwrap();
    let _ = wait_run_status(&server, &cookie, doc, run_id, "waiting_review").await;

    let diff: Value = server
        .req(
            Method::GET,
            &format!("/api/workbooks/{doc}/branches/{branch_id}/diff"),
            Some(&cookie),
            None,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let hunks: Vec<String> = diff["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(!hunks.is_empty());
    let merge_path = format!("/api/workbooks/{doc}/branches/{branch_id}/merge");
    let merge_body =
        json!({ "acceptedHunks": hunks, "conflictChoices": {}, "allowRefErrors": false });

    // The draft's status cannot be written: the changes must not land on `main` either.
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_merged() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.status = 'merged' THEN RAISE EXCEPTION 'simulated failure'; END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER refuse_merged BEFORE UPDATE ON branches
             FOR EACH ROW EXECUTE FUNCTION refuse_merged();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let failed = server
        .req(
            Method::POST,
            &merge_path,
            Some(&cookie),
            None,
            Some(merge_body.clone()),
        )
        .await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(main_txn_count(&pool, doc).await, 0);
    let branch = server
        .store
        .get_branch(doc, branch_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(branch.status, "active");
    let run_after: Value = server
        .req(
            Method::GET,
            &format!("/api/workbooks/{doc}/agent/runs/{run_id}"),
            Some(&cookie),
            None,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(run_after["run"]["status"], "waiting_review");

    // Restart the server against the same DB and data directory, then clear the fault and retry:
    // main must still be clean before retry, and the retry must land once and only once.
    let root = server.stop_keeping_root();
    sqlx::raw_sql("DROP TRIGGER refuse_merged ON branches; DROP FUNCTION refuse_merged();")
        .execute(&pool)
        .await
        .unwrap();
    let server = TestServer::start_with_root(
        &db,
        root,
        StorageLimits::default(),
        GovernanceConfig::default(),
        AgentRuntimeConfig::mock_for_tests(),
    )
    .await;
    assert_eq!(main_txn_count(&pool, doc).await, 0);
    let merged = server
        .req(
            Method::POST,
            &merge_path,
            Some(&cookie),
            None,
            Some(merge_body.clone()),
        )
        .await;
    assert_eq!(merged.status(), StatusCode::OK);
    let body: Value = merged.json().await.unwrap();
    let committed = i64::from(u32::try_from(body["committedTxns"].as_u64().unwrap()).unwrap());
    assert!(committed >= 1);
    assert_eq!(main_txn_count(&pool, doc).await, committed);
    let branch = server
        .store
        .get_branch(doc, branch_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(branch.status, "merged");
    let main_after = server.store.load_machine(doc).await.unwrap();
    assert_eq!(main_after.head_seq(), u64::try_from(committed).unwrap());
    let retried = server
        .req(
            Method::POST,
            &merge_path,
            Some(&cookie),
            None,
            Some(merge_body),
        )
        .await;
    assert_eq!(retried.status(), StatusCode::CONFLICT);
    assert_eq!(main_txn_count(&pool, doc).await, committed);

    pool.close().await;
    drop(server);
    db.cleanup().await;
}

/// Reads a stream of events until it ends; `false` when it is still going after `within`.
async fn ends_within(resp: &mut Response, within: Duration) -> bool {
    timeout(within, async {
        while let Ok(Some(_)) = resp.chunk().await {}
    })
    .await
    .is_ok()
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn the_events_of_a_run_stop_for_whoever_may_no_longer_read_them() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(
        &db,
        StorageLimits::default(),
        GovernanceConfig::default(),
        AgentRuntimeConfig::mock_for_tests(),
    )
    .await;
    let (alice_cookie, _) = server.dev_login("alice").await;
    let (bob_cookie, bob_id) = server.dev_login("bob").await;
    let doc = server.create_workbook(&alice_cookie, "Who reads").await;
    let link: Value = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/share-links"),
            Some(&alice_cookie),
            None,
            Some(json!({ "role": "editor", "label": "b" })),
        )
        .await
        .json()
        .await
        .unwrap();
    let tok = link["url"].as_str().unwrap().trim_start_matches("/join/");
    let joined = server
        .req(
            Method::POST,
            &format!("/api/join/{tok}"),
            Some(&bob_cookie),
            None,
            None,
        )
        .await;
    assert_eq!(joined.status(), StatusCode::OK);

    let run: Value = server
        .req(
            Method::POST,
            &format!("/api/workbooks/{doc}/agent/runs"),
            Some(&alice_cookie),
            None,
            Some(json!({ "prompt": "[slow] A run that takes its time" })),
        )
        .await
        .json()
        .await
        .unwrap();
    let run_id = run["id"].as_str().unwrap().to_owned();
    let events = format!("/api/workbooks/{doc}/agent/runs/{run_id}/events");

    // Bob follows it, and is then removed: his stream ends long before the run.
    let mut bobs = server
        .req(Method::GET, &events, Some(&bob_cookie), None, None)
        .await;
    assert_eq!(bobs.status(), StatusCode::OK);
    assert!(!ends_within(&mut bobs, Duration::from_millis(300)).await);
    let removed = server
        .req(
            Method::DELETE,
            &format!("/api/workbooks/{doc}/members/{bob_id}"),
            Some(&alice_cookie),
            None,
            None,
        )
        .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert!(
        ends_within(&mut bobs, Duration::from_secs(3)).await,
        "a removed member's stream goes on"
    );
    let again = server
        .req(Method::GET, &events, Some(&bob_cookie), None, None)
        .await;
    assert_eq!(again.status(), StatusCode::FORBIDDEN);

    // Alice follows it, and signs out: hers ends as well.
    let mut alices = server
        .req(Method::GET, &events, Some(&alice_cookie), None, None)
        .await;
    assert_eq!(alices.status(), StatusCode::OK);
    assert!(!ends_within(&mut alices, Duration::from_millis(300)).await);
    let out = server
        .req(
            Method::POST,
            "/api/auth/logout",
            Some(&alice_cookie),
            None,
            None,
        )
        .await;
    assert!(out.status().is_success());
    assert!(
        ends_within(&mut alices, Duration::from_secs(3)).await,
        "a stream of a session that was signed out goes on"
    );

    drop(server);
    db.cleanup().await;
}
