#![cfg(debug_assertions)]

use std::collections::BTreeSet;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use reqwest::{Method, Response, StatusCode, header};
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue, SeedOrigin, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_server::docs::{CLOSE_ACCESS_REVOKED, CLOSE_DUPLICATE_REPLICA, DocRooms, RoomError};
use reticula_server::http::{self, ApiState, AuthConfig};
use reticula_server::ops::{GovernanceConfig, Metrics};
use reticula_server::store::{
    DEFAULT_SNAPSHOT_KEEP_RECENT, DELETED_USER_ID, MAIN_BRANCH, SessionIdentity, StorageLimits,
    Store, StoreError,
};
use reticula_sync::MemberRole;
use reticula_wire::convert_v2::ops_to_wire_txn_v2;
use reticula_wire::snapshot::{encode_seed_stub_v1, encode_snapshot_v1};
use reticula_wire::sync_frame::{decode_sync_frame, encode_sync_frame};
use reticula_wire::v2::{WirePendingTxnV2, WirePresenceV1, WireRejectReasonV1, WireSyncFrameV1};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "http://127.0.0.1";
const WAIT: Duration = Duration::from_secs(12);
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
        let name = format!("reticula_m2b_{}", Uuid::new_v4().simple());
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
    _root: TempDir,
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
    async fn start(db: &IsolatedDb, limits: StorageLimits, gov: GovernanceConfig) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::connect_with_limits(&db.url, root.path().to_path_buf(), limits)
            .await
            .unwrap();
        let auth = AuthConfig::dev_mock(ORIGIN).unwrap();
        let api = ApiState::with_governance(store.clone(), auth, gov);
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
            _root: root,
            store,
            api,
            base: format!("http://{addr}"),
            client,
            handle,
        }
    }

    fn data_dir(&self) -> &Path {
        self.store.data_dir()
    }

    async fn req_json(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
        extra_headers: &[(&str, &str)],
        origin: Option<&str>,
    ) -> Response {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        self.req_json(method, path, cookie, body, &[], Some(ORIGIN))
            .await
    }

    async fn request_share(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        share: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        if let Some(token) = share {
            self.req_json(
                method,
                path,
                cookie,
                body,
                &[("X-Reticula-Share", token)],
                Some(ORIGIN),
            )
            .await
        } else {
            self.request(method, path, cookie, body).await
        }
    }

    async fn import_bytes(
        &self,
        cookie: Option<&str>,
        encoded_title: &str,
        bytes: Vec<u8>,
    ) -> Response {
        let mut req = self
            .client
            .post(format!("{}/api/workbooks/import", self.base))
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("X-Workbook-Title", encoded_title)
            .body(bytes);
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        req.send().await.unwrap()
    }

    async fn login(&self, account: &str) -> (Uuid, String) {
        let resp = self
            .request(
                Method::POST,
                "/api/auth/dev-login",
                None,
                Some(json!({ "account": account })),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        let body: Value = resp.json().await.unwrap();
        let id = body["id"].as_str().unwrap().parse().unwrap();
        (id, cookie)
    }

    async fn create_custom_user(&self, name: &str) -> (Uuid, String) {
        let secret = *Uuid::new_v4().as_bytes();
        let mut raw = [0_u8; 32];
        raw[..16].copy_from_slice(&secret);
        let cookie = format!("reticula_session={}", URL_SAFE_NO_PAD.encode(raw));
        let user = self
            .store
            .sign_in(
                &SessionIdentity {
                    provider: "test",
                    subject: &Uuid::new_v4().to_string(),
                    name,
                    avatar: "",
                },
                Sha256::digest(raw).into(),
                None,
                3600,
            )
            .await
            .unwrap();
        (user.id, cookie)
    }

    async fn create_workbook(&self, cookie: &str, title: &str) -> Uuid {
        let resp = self
            .request(
                Method::POST,
                "/api/workbooks",
                Some(cookie),
                Some(json!({ "title": title })),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        resp.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn create_link(
        &self,
        doc: Uuid,
        cookie: &str,
        role: &str,
        label: &str,
        single_use: bool,
    ) -> (Uuid, String) {
        let resp = self
            .request(
                Method::POST,
                &format!("/api/workbooks/{doc}/share-links"),
                Some(cookie),
                Some(json!({
                    "role": role,
                    "label": label,
                    "singleUse": single_use,
                })),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: Value = resp.json().await.unwrap();
        let link_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let url = body["url"].as_str().unwrap();
        let token = if role == "viewer" {
            url.split("share=").nth(1).unwrap().to_owned()
        } else {
            url.rsplit('/').next().unwrap().to_owned()
        };
        (link_id, token)
    }

    async fn ws_connect(
        &self,
        doc: Uuid,
        cookie: Option<&str>,
        share: Option<&str>,
        origin: &str,
    ) -> Result<Socket, tungstenite::Error> {
        let base_ws = self.base.strip_prefix("http://").unwrap();
        let url = match share {
            Some(token) => format!("ws://{base_ws}/api/sync/{doc}?share={token}"),
            None => format!("ws://{base_ws}/api/sync/{doc}"),
        };
        let mut req = url.into_client_request()?;
        req.headers_mut().insert("Origin", origin.parse().unwrap());
        if let Some(cookie) = cookie {
            req.headers_mut().insert("Cookie", cookie.parse().unwrap());
        }
        timeout(WAIT, connect_async(req))
            .await
            .unwrap()
            .map(|(socket, _)| socket)
    }

    async fn metrics_text(&self) -> String {
        let resp = self
            .client
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        resp.text().await.unwrap()
    }
}

fn hello_frame(doc: Uuid, replica: Option<u32>, seq: u64) -> WireSyncFrameV1 {
    WireSyncFrameV1::Hello {
        protocol_version: 1,
        doc_id: doc.into_bytes(),
        branch_id: MAIN_BRANCH.into_bytes(),
        replica,
        last_applied_seq: seq,
        known_epoch: 0,
    }
}

async fn send(socket: &mut Socket, frame: &WireSyncFrameV1) {
    timeout(
        WAIT,
        socket.send(Message::Binary(encode_sync_frame(frame).unwrap().into())),
    )
    .await
    .unwrap()
    .unwrap();
}

async fn recv(socket: &mut Socket) -> WireSyncFrameV1 {
    timeout(WAIT, async {
        while let Some(msg) = socket.next().await {
            if let Message::Binary(bytes) = msg.unwrap() {
                return decode_sync_frame(&bytes).unwrap();
            }
        }
        panic!("WebSocket closed before binary frame");
    })
    .await
    .unwrap()
}

async fn recv_close_code(socket: &mut Socket) -> u16 {
    timeout(WAIT, async {
        while let Some(msg) = socket.next().await {
            if let Message::Close(Some(frame)) = msg.unwrap() {
                return u16::from(frame.code);
            }
        }
        panic!("WebSocket closed without CloseFrame");
    })
    .await
    .unwrap()
}

fn edit_txn(replica: u32, client_seq: u64, base_server_seq: u64, val: f64) -> WirePendingTxnV2 {
    WirePendingTxnV2 {
        replica,
        client_seq,
        base_server_seq,
        timestamp_ms: 0,
        txn: ops_to_wire_txn_v2(
            format!("Edit {client_seq}"),
            &[Op::SetCells {
                sheet: SheetId(0),
                edits: vec![CellEdit {
                    row: RowId(client_seq),
                    col: ColId(u64::from(replica).max(1)),
                    input: CellInput::Scalar(ScalarValue::Number(val)),
                    format: FormatPatch::Keep,
                }],
            }],
        ),
    }
}

fn sample_snapshot(rows: u32) -> Vec<u8> {
    let mut wb: Workbook<BoundFormula> = Workbook::new();
    let edits: Vec<CellEdit<BoundFormula>> = (1..=u64::from(rows))
        .map(|row| CellEdit {
            row: RowId(row),
            col: ColId(1),
            input: CellInput::Scalar(ScalarValue::Number(row as f64)),
            format: FormatPatch::Keep,
        })
        .collect();
    wb.apply(Op::SetCells {
        sheet: SheetId(0),
        edits,
    })
    .unwrap();
    encode_snapshot_v1(&wb, &[]).unwrap()
}

fn percent_encode_utf8(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for &byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn permission_matrix_six_identities() {
    let db = IsolatedDb::create().await;
    let server =
        TestServer::start(&db, StorageLimits::default(), GovernanceConfig::default()).await;

    let (owner_id, owner) = server.login("alice").await;
    let (_editor_id, editor) = server.login("bob").await;
    let (viewer_id, viewer) = server.login("carol").await;
    let (_outsider_id, outsider) = server.create_custom_user("Outsider").await;

    let doc = server.create_workbook(&owner, "Matrix Book").await;
    let (_, editor_tok) = server
        .create_link(doc, &owner, "editor", "Editor invite", false)
        .await;
    let (_, viewer_join_tok) = server
        .create_link(doc, &owner, "viewer", "Viewer invite", false)
        .await;
    let (_, anon_tok) = server
        .create_link(doc, &owner, "viewer", "Public readonly", false)
        .await;
    let (_, expired_tok) = server
        .create_link(doc, &owner, "viewer", "Expired", false)
        .await;
    let (_, single_use_tok) = server
        .create_link(doc, &owner, "editor", "Once", true)
        .await;

    let pool = PgPool::connect(&db.url).await.unwrap();
    let expired_hash = Sha256::digest(URL_SAFE_NO_PAD.decode(&expired_tok).unwrap());
    sqlx::query(
        "UPDATE share_links SET expires_at = now() - interval '1 minute' WHERE token_hash = $1",
    )
    .bind(expired_hash.as_slice())
    .execute(&pool)
    .await
    .unwrap();

    // Join editor & viewer, and consume the single-use editor link with editor.
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{editor_tok}"),
                Some(&editor),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{viewer_join_tok}"),
                Some(&viewer),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{single_use_tok}"),
                Some(&editor),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );

    // Owner cannot set link_view on a doc without an active viewer link; doc already has anon_tok.
    let other_doc = server.create_workbook(&owner, "Private Only").await;
    assert_eq!(
        server
            .request(
                Method::PATCH,
                &format!("/api/workbooks/{other_doc}"),
                Some(&owner),
                Some(json!({ "visibility": "link_view" })),
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (_, other_view_tok) = server
        .create_link(other_doc, &owner, "viewer", "Other doc", false)
        .await;

    // Enable link_view on main doc.
    assert_eq!(
        server
            .request(
                Method::PATCH,
                &format!("/api/workbooks/{doc}"),
                Some(&owner),
                Some(json!({ "visibility": "link_view" })),
            )
            .await
            .status(),
        StatusCode::OK
    );

    // Seed 1 commit + 1 snapshot + 1 version by owner so snapshots/0, history, txns, versions exist.
    let (mut owner_ws, owner_welcome) = server
        .ws_connect(doc, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    let owner_rep = welcome_replica(&owner_welcome);
    send(
        &mut owner_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(owner_rep, 1, 0, 11.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut owner_ws).await,
        WireSyncFrameV1::Ack { seq: 1, .. }
    ));
    assert!(matches!(
        recv(&mut owner_ws).await,
        WireSyncFrameV1::Commit { seq: 1, .. }
    ));

    let v_resp = server
        .request(
            Method::POST,
            &format!("/api/workbooks/{doc}/versions"),
            Some(&owner),
            Some(json!({ "seq": "1", "name": "Baseline" })),
        )
        .await;
    assert_eq!(v_resp.status(), StatusCode::CREATED);
    let v_owner_id: String = v_resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (disposable_link_id, _) = server
        .create_link(doc, &owner, "viewer", "Disposable", false)
        .await;
    let (_removable_id, removable_cookie) = server.create_custom_user("Removable").await;
    let (_, rem_tok) = server
        .create_link(doc, &owner, "viewer", "Removable join", false)
        .await;
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{rem_tok}"),
                Some(&removable_cookie),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );

    // Non-member with invalid/expired/consumed/wrong-doc share tokens gets 403 on read endpoints and WS.
    for bad in [
        expired_tok.as_str(),
        single_use_tok.as_str(),
        other_view_tok.as_str(),
    ] {
        assert_eq!(
            server
                .request_share(
                    Method::GET,
                    &format!("/api/workbooks/{doc}"),
                    Some(&outsider),
                    Some(bad),
                    None
                )
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        match server
            .ws_connect(doc, Some(&outsider), Some(bad), ORIGIN)
            .await
        {
            Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), StatusCode::FORBIDDEN),
            other => panic!("expected 403 WS rejection for bad share token, got {other:?}"),
        }
    }
    // Non-member WITH a valid link_view share token can read via X-Reticula-Share and WS ?share=.
    assert_eq!(
        server
            .request_share(
                Method::GET,
                &format!("/api/workbooks/{doc}"),
                Some(&outsider),
                Some(&anon_tok),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );

    // 6-identity matrix across HTTP routes:
    // (label, cookie, share_header)
    let identities: [(&str, Option<&str>, Option<&str>); 6] = [
        ("unauth", None, None),
        ("anon", None, Some(anon_tok.as_str())),
        ("non_member", Some(outsider.as_str()), None),
        ("viewer", Some(viewer.as_str()), None),
        ("editor", Some(editor.as_str()), None),
        ("owner", Some(owner.as_str()), None),
    ];

    for (label, cookie, share) in identities {
        let is_authed = cookie.is_some();
        let can_read_doc = matches!(label, "anon" | "viewer" | "editor" | "owner");
        let can_edit = matches!(label, "editor" | "owner");
        let is_owner = label == "owner";
        let unauth_or_forbidden = |ok_cond: bool| -> StatusCode {
            if ok_cond {
                StatusCode::OK
            } else if is_authed {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::UNAUTHORIZED
            }
        };

        assert_eq!(
            server
                .request(Method::GET, "/api/workbooks", cookie, None)
                .await
                .status(),
            if is_authed {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{label} GET /workbooks"
        );
        assert_eq!(
            server
                .request(Method::GET, "/api/account", cookie, None)
                .await
                .status(),
            if is_authed {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{label} GET /account"
        );
        assert_eq!(
            server
                .request(
                    Method::POST,
                    "/api/workbooks",
                    cookie,
                    Some(json!({ "title": format!("{label}-book") }))
                )
                .await
                .status(),
            if is_authed {
                StatusCode::CREATED
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{label} POST /workbooks"
        );
        assert_eq!(
            server
                .import_bytes(cookie, "Imported", sample_snapshot(3))
                .await
                .status(),
            if is_authed {
                StatusCode::CREATED
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{label} POST /workbooks/import"
        );

        assert_eq!(
            server
                .request_share(
                    Method::GET,
                    &format!("/api/workbooks/{doc}"),
                    cookie,
                    share,
                    None
                )
                .await
                .status(),
            unauth_or_forbidden(can_read_doc),
            "{label} GET /workbooks/{{id}}"
        );
        assert_eq!(
            server
                .request_share(
                    Method::GET,
                    &format!("/api/workbooks/{doc}/snapshots/0"),
                    cookie,
                    share,
                    None
                )
                .await
                .status(),
            unauth_or_forbidden(can_read_doc),
            "{label} GET /snapshots/0"
        );
        assert_eq!(
            server
                .request_share(
                    Method::GET,
                    &format!("/api/workbooks/{doc}/history"),
                    cookie,
                    share,
                    None
                )
                .await
                .status(),
            unauth_or_forbidden(can_read_doc),
            "{label} GET /history"
        );
        assert_eq!(
            server
                .request_share(
                    Method::GET,
                    &format!("/api/workbooks/{doc}/txns"),
                    cookie,
                    share,
                    None
                )
                .await
                .status(),
            unauth_or_forbidden(can_read_doc),
            "{label} GET /txns"
        );
        assert_eq!(
            server
                .request_share(
                    Method::GET,
                    &format!("/api/workbooks/{doc}/versions"),
                    cookie,
                    share,
                    None
                )
                .await
                .status(),
            unauth_or_forbidden(can_read_doc),
            "{label} GET /versions"
        );

        assert_eq!(
            server
                .request(
                    Method::GET,
                    &format!("/api/workbooks/{doc}/members"),
                    cookie,
                    None
                )
                .await
                .status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if label == "non_member" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            },
            "{label} GET /members"
        );

        let share_list_resp = server
            .request(
                Method::GET,
                &format!("/api/workbooks/{doc}/share-links"),
                cookie,
                None,
            )
            .await;
        assert_eq!(
            share_list_resp.status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if is_owner {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} GET /share-links"
        );
        if is_owner {
            let body = share_list_resp.text().await.unwrap();
            assert!(!body.contains(&anon_tok));
            assert!(!body.contains(&editor_tok));
        }

        // Write endpoints
        assert_eq!(
            server
                .request(
                    Method::PATCH,
                    &format!("/api/workbooks/{doc}"),
                    cookie,
                    Some(json!({ "title": "Matrix Book Updated" })),
                )
                .await
                .status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if is_owner {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} PATCH /workbooks/{{id}}"
        );
        assert_eq!(
            server
                .request(
                    Method::PATCH,
                    &format!("/api/workbooks/{doc}/members/{viewer_id}"),
                    cookie,
                    Some(json!({ "role": "viewer" })),
                )
                .await
                .status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if is_owner {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} PATCH /members"
        );
        assert_eq!(
            server
                .request(
                    Method::POST,
                    &format!("/api/workbooks/{doc}/share-links"),
                    cookie,
                    Some(json!({ "role": "viewer" })),
                )
                .await
                .status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if is_owner {
                StatusCode::CREATED
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} POST /share-links"
        );
        let create_v = server
            .request(
                Method::POST,
                &format!("/api/workbooks/{doc}/versions"),
                cookie,
                Some(json!({ "seq": "1", "name": format!("Tag-{label}") })),
            )
            .await;
        assert_eq!(
            create_v.status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if can_edit {
                StatusCode::CREATED
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} POST /versions"
        );
        if create_v.status() == StatusCode::CREATED {
            let vid = create_v.json::<Value>().await.unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(
                server
                    .request(
                        Method::DELETE,
                        &format!("/api/workbooks/{doc}/versions/{vid}"),
                        cookie,
                        None,
                    )
                    .await
                    .status(),
                StatusCode::NO_CONTENT,
                "{label} own DELETE /versions"
            );
        } else {
            assert_eq!(
                server
                    .request(
                        Method::DELETE,
                        &format!("/api/workbooks/{doc}/versions/{v_owner_id}"),
                        cookie,
                        None,
                    )
                    .await
                    .status(),
                if !is_authed {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::FORBIDDEN
                },
                "{label} DELETE owner version"
            );
        }
        assert_eq!(
            server
                .request(
                    Method::DELETE,
                    &format!("/api/workbooks/{doc}/share-links/{disposable_link_id}"),
                    cookie,
                    None,
                )
                .await
                .status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if is_owner {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} DELETE /share-links"
        );
        assert_eq!(
            server
                .request(
                    Method::DELETE,
                    &format!("/api/workbooks/{doc}/members/{_removable_id}"),
                    cookie,
                    None,
                )
                .await
                .status(),
            if !is_authed {
                StatusCode::UNAUTHORIZED
            } else if is_owner {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            },
            "{label} DELETE /members"
        );
    }

    // Editor can self-leave cleanly (tested using another temporary member).
    let (self_leave_id, self_leave_cookie) = server.create_custom_user("SelfLeave").await;
    let (_, sl_tok) = server
        .create_link(doc, &owner, "editor", "Self leave", false)
        .await;
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{sl_tok}"),
                Some(&self_leave_cookie),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .request(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/members/{self_leave_id}"),
                Some(&self_leave_cookie),
                None,
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );

    // Owner cannot demote or remove self as sole owner (422).
    assert_eq!(
        server
            .request(
                Method::PATCH,
                &format!("/api/workbooks/{doc}/members/{owner_id}"),
                Some(&owner),
                Some(json!({ "role": "editor" })),
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        server
            .request(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/members/{owner_id}"),
                Some(&owner),
                None,
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // CSRF checks on PATCH / DELETE when Origin is missing or mismatched.
    assert_eq!(
        server
            .req_json(
                Method::PATCH,
                &format!("/api/workbooks/{doc}"),
                Some(&owner),
                Some(json!({ "title": "Evil" })),
                &[],
                Some("https://evil.example"),
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .req_json(
                Method::DELETE,
                &format!("/api/workbooks/{doc}"),
                Some(&owner),
                None,
                &[],
                None,
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    // WS action matrix across all 6 identities:
    match server.ws_connect(doc, None, None, ORIGIN).await {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), StatusCode::UNAUTHORIZED),
        other => panic!("expected 401 for unauth WS, got {other:?}"),
    }
    match server.ws_connect(doc, Some(&outsider), None, ORIGIN).await {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), StatusCode::FORBIDDEN),
        other => panic!("expected 403 for non-member WS, got {other:?}"),
    }

    // Anonymous viewer WS: replica = 0, Push rejected with Forbidden, Presence ignored.
    let (mut anon_ws, anon_welcome) = server
        .ws_connect(doc, None, Some(&anon_tok), ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    assert_eq!(welcome_replica(&anon_welcome), 0);
    send(
        &mut anon_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(0, 1, 1, 9.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut anon_ws).await,
        WireSyncFrameV1::Reject {
            reason: WireRejectReasonV1::Forbidden,
            ..
        }
    ));
    send(
        &mut anon_ws,
        &WireSyncFrameV1::Presence(WirePresenceV1 {
            replica: 0,
            user_id: [0; 16],
            display_name: String::new(),
            color_slot: 0,
            sheet_id: 0,
            cursor_row: 0,
            cursor_col: 0,
            selection: None,
        }),
    )
    .await;
    assert!(
        timeout(Duration::from_millis(120), owner_ws.next())
            .await
            .is_err()
    );
    drop(anon_ws);

    // Authenticated viewer WS: replica = 0, Push rejected with Forbidden.
    let (mut viewer_ws, viewer_welcome) = server
        .ws_connect(doc, Some(&viewer), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    assert_eq!(welcome_replica(&viewer_welcome), 0);
    send(
        &mut viewer_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(0, 1, 1, 9.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut viewer_ws).await,
        WireSyncFrameV1::Reject {
            reason: WireRejectReasonV1::Forbidden,
            ..
        }
    ));
    drop(viewer_ws);

    // Editor WS: replica != 0, Push succeeds, Presence broadcasts to owner.
    let (mut editor_ws, editor_welcome) = server
        .ws_connect(doc, Some(&editor), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    let ed_rep = welcome_replica(&editor_welcome);
    assert_ne!(ed_rep, 0);
    send(
        &mut editor_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(ed_rep, 1, 1, 22.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut editor_ws).await,
        WireSyncFrameV1::Ack { seq: 2, .. }
    ));
    assert!(matches!(
        recv(&mut editor_ws).await,
        WireSyncFrameV1::Commit { seq: 2, .. }
    ));
    assert!(matches!(
        recv(&mut owner_ws).await,
        WireSyncFrameV1::Commit { seq: 2, .. }
    ));

    send(
        &mut editor_ws,
        &WireSyncFrameV1::Presence(WirePresenceV1 {
            replica: ed_rep,
            user_id: [0; 16],
            display_name: String::new(),
            color_slot: 0,
            sheet_id: 0,
            cursor_row: 3,
            cursor_col: 4,
            selection: None,
        }),
    )
    .await;
    assert!(
        matches!(recv(&mut owner_ws).await, WireSyncFrameV1::Presence(p) if p.cursor_row == 3 && p.cursor_col == 4)
    );
    drop(editor_ws);
    drop(owner_ws);

    // Non-owner cannot delete workbook; owner can.
    for (label, cookie, _) in identities {
        if label != "owner" {
            assert_eq!(
                server
                    .request(
                        Method::DELETE,
                        &format!("/api/workbooks/{doc}"),
                        cookie,
                        None
                    )
                    .await
                    .status(),
                if cookie.is_some() {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
        }
    }
    assert_eq!(
        server
            .request(
                Method::DELETE,
                &format!("/api/workbooks/{doc}"),
                Some(&owner),
                None
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );

    drop(pool);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn live_downgrade_revocation_and_duplicate_replica() {
    let db = IsolatedDb::create().await;
    let server =
        TestServer::start(&db, StorageLimits::default(), GovernanceConfig::default()).await;

    let (_owner_id, owner) = server.login("alice").await;
    let (editor_id, editor) = server.login("bob").await;
    let doc = server.create_workbook(&owner, "Live Control").await;

    let (_ed_link_id, ed_tok) = server
        .create_link(doc, &owner, "editor", "Bob edit", false)
        .await;
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{ed_tok}"),
                Some(&editor),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );

    let (link1_id, anon_tok1) = server
        .create_link(doc, &owner, "viewer", "Anon 1", false)
        .await;
    let (_link2_id, anon_tok2) = server
        .create_link(doc, &owner, "viewer", "Anon 2", false)
        .await;
    assert_eq!(
        server
            .request(
                Method::PATCH,
                &format!("/api/workbooks/{doc}"),
                Some(&owner),
                Some(json!({ "visibility": "link_view" })),
            )
            .await
            .status(),
        StatusCode::OK
    );

    // 1. Editor opens WS and commits txn 1.
    let (mut ed_ws, ed_welcome) = server
        .ws_connect(doc, Some(&editor), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    let ed_rep = welcome_replica(&ed_welcome);
    send(
        &mut ed_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(ed_rep, 1, 0, 5.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ed_ws).await,
        WireSyncFrameV1::Ack { seq: 1, .. }
    ));
    assert!(matches!(
        recv(&mut ed_ws).await,
        WireSyncFrameV1::Commit { seq: 1, .. }
    ));

    // Downgrade Editor -> Viewer while WS stays open.
    assert_eq!(
        server
            .request(
                Method::PATCH,
                &format!("/api/workbooks/{doc}/members/{editor_id}"),
                Some(&owner),
                Some(json!({ "role": "viewer" })),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    send(
        &mut ed_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(ed_rep, 2, 1, 6.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ed_ws).await,
        WireSyncFrameV1::Reject {
            reason: WireRejectReasonV1::Forbidden,
            ..
        }
    ));

    // Remove member -> same open WS receives Close(4003).
    assert_eq!(
        server
            .request(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/members/{editor_id}"),
                Some(&owner),
                None,
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(recv_close_code(&mut ed_ws).await, CLOSE_ACCESS_REVOKED);

    // 2. Anonymous viewer #1 kicked by share-link revocation with 4003.
    let (mut anon_ws1, _) = server
        .ws_connect(doc, None, Some(&anon_tok1), ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    assert_eq!(
        server
            .request(
                Method::DELETE,
                &format!("/api/workbooks/{doc}/share-links/{link1_id}"),
                Some(&owner),
                None,
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(recv_close_code(&mut anon_ws1).await, CLOSE_ACCESS_REVOKED);
    assert_eq!(
        server
            .request_share(
                Method::GET,
                &format!("/api/workbooks/{doc}"),
                None,
                Some(&anon_tok1),
                None
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    match server.ws_connect(doc, None, Some(&anon_tok1), ORIGIN).await {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), StatusCode::FORBIDDEN),
        other => panic!("expected 403 after share-link revocation, got {other:?}"),
    }

    // 3. Anonymous viewer #2 kicked by visibility=private with 4003.
    let (mut anon_ws2, _) = server
        .ws_connect(doc, None, Some(&anon_tok2), ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    assert_eq!(
        server
            .request(
                Method::PATCH,
                &format!("/api/workbooks/{doc}"),
                Some(&owner),
                Some(json!({ "visibility": "private" })),
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(recv_close_code(&mut anon_ws2).await, CLOSE_ACCESS_REVOKED);

    // 4. Duplicate replica connection closes the superseded session with 4001.
    let (mut owner_ws1, owner_welcome) = server
        .ws_connect(doc, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    let rep = welcome_replica(&owner_welcome);
    assert_ne!(rep, 0);
    let (owner_ws2, owner_welcome2) = server
        .ws_connect(doc, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, Some(rep), 0)
        .await;
    assert_eq!(welcome_replica(&owner_welcome2), rep);
    assert_eq!(
        recv_close_code(&mut owner_ws1).await,
        CLOSE_DUPLICATE_REPLICA
    );
    drop(owner_ws2);

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn import_prevalidation_and_body_limits() {
    let db = IsolatedDb::create().await;
    let server =
        TestServer::start(&db, StorageLimits::default(), GovernanceConfig::default()).await;
    let (_owner_id, owner) = server.login("alice").await;

    // 1. Valid snapshot import with non-ASCII UTF-8 title.
    let raw_title = "Budget 2026 — Café";
    let import_resp = server
        .import_bytes(
            Some(&owner),
            &percent_encode_utf8(raw_title),
            sample_snapshot(5),
        )
        .await;
    assert_eq!(import_resp.status(), StatusCode::CREATED);
    let doc: Uuid = import_resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let summary: Value = server
        .request(
            Method::GET,
            &format!("/api/workbooks/{doc}"),
            Some(&owner),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(summary["title"].as_str().unwrap(), raw_title);
    assert_eq!(
        server
            .request(
                Method::GET,
                &format!("/api/workbooks/{doc}/snapshots/0"),
                Some(&owner),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );
    let (ws, welcome) = server
        .ws_connect(doc, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    assert_ne!(welcome_replica(&welcome), 0);
    drop(ws);

    // 2. 1,000,000-row SeedOrigin stub rejected pre-allocation with 422.
    let million_stub = encode_seed_stub_v1(
        SeedOrigin {
            seed: 42,
            rows: 1_000_000,
            generator_version: 1,
        },
        &[],
    )
    .unwrap();
    assert_eq!(
        server
            .import_bytes(Some(&owner), "MillionRows", million_stub)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // 3. Standard JSON route enforces 16 KiB limit (413 Payload Too Large).
    let oversized_title = "A".repeat(20 * 1024);
    assert_eq!(
        server
            .request(
                Method::POST,
                "/api/workbooks",
                Some(&owner),
                Some(json!({ "title": oversized_title })),
            )
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn memory_budget_lru_eviction_and_busy_backpressure() {
    let db = IsolatedDb::create().await;
    let gov = GovernanceConfig {
        max_loaded_bytes: 96 * 1024,
        ..GovernanceConfig::default()
    };
    let server = TestServer::start(&db, StorageLimits::default(), gov).await;
    let (owner_id, owner) = server.login("alice").await;

    let doc_a = server.create_workbook(&owner, "Room A").await;
    let doc_b = server.create_workbook(&owner, "Room B").await;
    let doc_c = server.create_workbook(&owner, "Room C").await;

    // Load room A, then close WS cleanly so room A becomes idle.
    let (mut ws_a, _) = server
        .ws_connect(doc_a, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc_a, None, 0)
        .await;
    let _ = ws_a.close(None).await;
    drop(ws_a);

    // Poll /metrics until active WS count drops to 0.
    let mut active_zero = false;
    for _ in 0..30 {
        let metrics = server.metrics_text().await;
        if metrics.contains("reticula_active_ws_connections{kind=\"authed\"} 0") {
            active_zero = true;
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(
        active_zero,
        "room A did not release active WS count cleanly"
    );

    // Room B evicts idle room A immediately and succeeds.
    let (ws_b, welcome_b) = server
        .ws_connect(doc_b, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc_b, None, 0)
        .await;
    assert_ne!(welcome_replica(&welcome_b), 0);
    let metrics = server.metrics_text().await;
    assert!(metrics.contains("reticula_active_rooms 1"));

    // While B is active (64 KiB used out of 96 KiB), room C returns HTTP 503 on WS upgrade.
    match server.ws_connect(doc_c, Some(&owner), None, ORIGIN).await {
        Err(tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        other => panic!("expected 503 when memory budget saturated, got {other:?}"),
    }
    drop(ws_b);

    // Direct DocRooms::join also returns RoomError::Busy when budget is saturated.
    let rooms = DocRooms::with_governance(
        server.store.clone(),
        96 * 1024,
        Arc::new(Metrics::default()),
    );
    let conn_b = rooms
        .join(
            doc_b,
            owner_id,
            MemberRole::Owner,
            "alice".into(),
            hello_frame(doc_b, None, 0),
        )
        .await
        .unwrap();
    let busy = rooms
        .join(
            doc_c,
            owner_id,
            MemberRole::Owner,
            "alice".into(),
            hello_frame(doc_c, None, 0),
        )
        .await;
    assert!(matches!(busy, Err(RoomError::Busy)));
    drop(conn_b);

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn quotas_hard_disk_limit_and_snapshot_retention_pruning() {
    // Part A: User storage quota (403) + delete recovery.
    let snap = sample_snapshot(3);
    let snap_len = snap.len() as u64;
    {
        let db = IsolatedDb::create().await;
        let limits = StorageLimits {
            user_quota_bytes: snap_len,
            ..StorageLimits::default()
        };
        let server = TestServer::start(&db, limits, GovernanceConfig::default()).await;
        let (_owner_id, owner) = server.login("alice").await;

        let r1 = server
            .import_bytes(Some(&owner), "First", snap.clone())
            .await;
        assert_eq!(r1.status(), StatusCode::CREATED);
        let doc1: Uuid = r1.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        let r2 = server
            .import_bytes(Some(&owner), "Second", snap.clone())
            .await;
        assert_eq!(r2.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            server
                .request(
                    Method::POST,
                    "/api/workbooks",
                    Some(&owner),
                    Some(json!({ "title": "OverQuota" }))
                )
                .await
                .status(),
            StatusCode::FORBIDDEN
        );

        assert_eq!(
            server
                .request(
                    Method::DELETE,
                    &format!("/api/workbooks/{doc1}"),
                    Some(&owner),
                    None
                )
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            server
                .request(
                    Method::POST,
                    "/api/workbooks",
                    Some(&owner),
                    Some(json!({ "title": "Recovered" }))
                )
                .await
                .status(),
            StatusCode::CREATED
        );

        drop(server);
        db.cleanup().await;
    }

    // Part B: Global hard disk limit -> 507 Insufficient Storage + WS Push Reject::QuotaExceeded.
    {
        let db = IsolatedDb::create().await;
        let limits = StorageLimits {
            disk_hard_bytes: snap_len + 64,
            user_quota_bytes: 100 * 1024 * 1024,
            ..StorageLimits::default()
        };
        let server = TestServer::start(&db, limits, GovernanceConfig::default()).await;
        let (_owner_id, owner) = server.login("alice").await;

        let r1 = server
            .import_bytes(Some(&owner), "SeedDisk", snap.clone())
            .await;
        assert_eq!(r1.status(), StatusCode::CREATED);
        let doc1: Uuid = r1.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        // Next import exceeds hard disk limit -> 507.
        assert_eq!(
            server
                .import_bytes(Some(&owner), "DiskFullImport", snap.clone())
                .await
                .status(),
            StatusCode::INSUFFICIENT_STORAGE
        );

        // Push txns on doc1 until log_bytes pushes total past disk_hard_bytes.
        let (mut ws, welcome) = server
            .ws_connect(doc1, Some(&owner), None, ORIGIN)
            .await
            .unwrap()
            .pipe_hello(doc1, None, 0)
            .await;
        let rep = welcome_replica(&welcome);
        let mut seq = 0_u64;
        let mut rejected_over_disk = false;
        for cseq in 1..=20 {
            send(
                &mut ws,
                &WireSyncFrameV1::Push {
                    txns: vec![edit_txn(rep, cseq, seq, cseq as f64)],
                },
            )
            .await;
            match recv(&mut ws).await {
                WireSyncFrameV1::Ack { seq: ack_seq, .. } => {
                    seq = ack_seq;
                    assert!(matches!(
                        recv(&mut ws).await,
                        WireSyncFrameV1::Commit { .. }
                    ));
                    if server.store.disk_over_hard_limit() {
                        assert_eq!(
                            server
                                .request(
                                    Method::POST,
                                    "/api/workbooks",
                                    Some(&owner),
                                    Some(json!({ "title": "Blocked" }))
                                )
                                .await
                                .status(),
                            StatusCode::INSUFFICIENT_STORAGE
                        );
                        send(
                            &mut ws,
                            &WireSyncFrameV1::Push {
                                txns: vec![edit_txn(rep, cseq + 1, seq, 99.0)],
                            },
                        )
                        .await;
                        assert!(matches!(
                            recv(&mut ws).await,
                            WireSyncFrameV1::Reject {
                                reason: WireRejectReasonV1::QuotaExceeded,
                                ..
                            }
                        ));
                        rejected_over_disk = true;
                        break;
                    }
                }
                other => panic!("unexpected frame while filling disk limit: {other:?}"),
            }
        }
        assert!(rejected_over_disk);
        assert!(server.store.disk_over_hard_limit());
        drop(ws);

        drop(server);
        db.cleanup().await;
    }

    // Part C: Snapshot retention pruning keeps seq=0, bucket-0 milestone(1), pinned version base(2), and latest 5(5..=9).
    {
        let db = IsolatedDb::create().await;
        let server =
            TestServer::start(&db, StorageLimits::default(), GovernanceConfig::default()).await;
        let (_owner_id, owner) = server.login("alice").await;
        let doc = server.create_workbook(&owner, "Retention").await;

        let machine = server.store.load_machine(doc).await.unwrap();
        let snap0 = server.store.write_snapshot(&machine).await.unwrap();
        assert_eq!(snap0.seq(), 0);
        drop(machine);
        let (mut ws, welcome) = server
            .ws_connect(doc, Some(&owner), None, ORIGIN)
            .await
            .unwrap()
            .pipe_hello(doc, None, 0)
            .await;
        let rep = welcome_replica(&welcome);
        for cseq in 1..=9_u64 {
            send(
                &mut ws,
                &WireSyncFrameV1::Push {
                    txns: vec![edit_txn(rep, cseq, cseq - 1, cseq as f64)],
                },
            )
            .await;
            assert!(matches!(recv(&mut ws).await, WireSyncFrameV1::Ack { seq, .. } if seq == cseq));
            assert!(
                matches!(recv(&mut ws).await, WireSyncFrameV1::Commit { seq, .. } if seq == cseq)
            );
            let m = server.store.load_machine(doc).await.unwrap();
            let s = server.store.write_snapshot(&m).await.unwrap();
            assert_eq!(s.seq(), cseq);
        }
        drop(ws);

        // Pin seq=2 via named version.
        assert_eq!(
            server
                .request(
                    Method::POST,
                    &format!("/api/workbooks/{doc}/versions"),
                    Some(&owner),
                    Some(json!({ "seq": "2", "name": "KeepTwo" })),
                )
                .await
                .status(),
            StatusCode::CREATED
        );

        let before_account: Value = server
            .request(Method::GET, "/api/account", Some(&owner), None)
            .await
            .json()
            .await
            .unwrap();
        let before_bytes = before_account["storageBytes"].as_u64().unwrap();
        let pruned_expected = server.store.snapshot(doc, 3).await.unwrap().unwrap().size()
            + server.store.snapshot(doc, 4).await.unwrap().unwrap().size();

        let pruned = server
            .store
            .prune_snapshots(doc, DEFAULT_SNAPSHOT_KEEP_RECENT)
            .await
            .unwrap();
        assert_eq!(pruned, 2);

        let after_account: Value = server
            .request(Method::GET, "/api/account", Some(&owner), None)
            .await
            .json()
            .await
            .unwrap();
        let after_bytes = after_account["storageBytes"].as_u64().unwrap();
        assert_eq!(before_bytes - after_bytes, pruned_expected);

        let expected_kept: BTreeSet<u64> = [0, 1, 2, 5, 6, 7, 8, 9].into_iter().collect();
        for seq in 0..=9_u64 {
            let exists = server.store.snapshot(doc, seq).await.unwrap().is_some();
            assert_eq!(exists, expected_kept.contains(&seq), "seq={seq}");
        }
        let disk_count = fs::read_dir(server.data_dir().join("snapshots").join(doc.to_string()))
            .unwrap()
            .count();
        assert_eq!(disk_count, expected_kept.len());

        drop(server);
        db.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn delete_account_preserves_collaborator_history() {
    let db = IsolatedDb::create().await;
    let server =
        TestServer::start(&db, StorageLimits::default(), GovernanceConfig::default()).await;

    let (_alice_id, alice) = server.login("alice").await;
    let (_bob_id, bob) = server.login("bob").await;

    let doc_a = server.create_workbook(&alice, "Shared Book").await;
    let (_link_id, invite_tok) = server
        .create_link(doc_a, &alice, "editor", "Invite Bob", false)
        .await;
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/join/{invite_tok}"),
                Some(&bob),
                None
            )
            .await
            .status(),
        StatusCode::OK
    );

    // Bob commits a txn and creates a named version on Alice's workbook.
    let (mut bob_ws, bob_welcome) = server
        .ws_connect(doc_a, Some(&bob), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc_a, None, 0)
        .await;
    let bob_rep = welcome_replica(&bob_welcome);
    send(
        &mut bob_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(bob_rep, 1, 0, 77.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut bob_ws).await,
        WireSyncFrameV1::Ack { seq: 1, .. }
    ));
    assert!(matches!(
        recv(&mut bob_ws).await,
        WireSyncFrameV1::Commit { seq: 1, .. }
    ));

    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/workbooks/{doc_a}/versions"),
                Some(&bob),
                Some(json!({ "seq": "1", "name": "Bob Milestone" })),
            )
            .await
            .status(),
        StatusCode::CREATED
    );

    // Bob also owns his own imported workbook (with snapshot directory on disk).
    let b_resp = server
        .import_bytes(Some(&bob), "BobPersonal", sample_snapshot(3))
        .await;
    assert_eq!(b_resp.status(), StatusCode::CREATED);
    let doc_b: Uuid = b_resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let doc_b_dir = server.data_dir().join("snapshots").join(doc_b.to_string());
    assert!(doc_b_dir.exists());

    // Wrong confirm rejected with 400.
    assert_eq!(
        server
            .request(
                Method::DELETE,
                "/api/account",
                Some(&bob),
                Some(json!({ "confirm": "NOPE" })),
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    // Confirm DELETE -> 204, clears cookie, kicks open WS with 4003, removes owned dir/rows.
    let del_resp = server
        .request(
            Method::DELETE,
            "/api/account",
            Some(&bob),
            Some(json!({ "confirm": "DELETE" })),
        )
        .await;
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);
    let clear_cookie = del_resp.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(clear_cookie.contains("Max-Age=0"));
    assert_eq!(recv_close_code(&mut bob_ws).await, CLOSE_ACCESS_REVOKED);

    assert_eq!(
        server
            .request(Method::GET, "/api/auth/me", Some(&bob), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(!doc_b_dir.exists());

    // Alice's workbook history, txns, versions, and cold load_machine still succeed with DELETED_USER_ID attribution.
    let hist: Value = server
        .request(
            Method::GET,
            &format!("/api/workbooks/{doc_a}/history"),
            Some(&alice),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let entry = &hist["entries"][0];
    assert_eq!(
        entry["userId"].as_str().unwrap(),
        DELETED_USER_ID.to_string()
    );
    assert_eq!(entry["authorName"].as_str().unwrap(), "Deleted user");

    let versions: Value = server
        .request(
            Method::GET,
            &format!("/api/workbooks/{doc_a}/versions"),
            Some(&alice),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        versions[0]["createdBy"].as_str().unwrap(),
        DELETED_USER_ID.to_string()
    );
    assert_eq!(
        versions[0]["createdByName"].as_str().unwrap(),
        "Deleted user"
    );

    let txns_resp = server
        .request(
            Method::GET,
            &format!("/api/workbooks/{doc_a}/txns?after_seq=0"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(txns_resp.status(), StatusCode::OK);
    assert!(!txns_resp.bytes().await.unwrap().is_empty());

    let machine = server.store.load_machine(doc_a).await.unwrap();
    assert_eq!(machine.head_seq(), 1);

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn metrics_loopback_gate_and_write_rate_limit() {
    let db = IsolatedDb::create().await;
    let gov = GovernanceConfig {
        write_rpm: 3,
        ..GovernanceConfig::default()
    };
    let server = TestServer::start(&db, StorageLimits::default(), gov).await;
    let (_owner_id, owner) = server.login("alice").await;

    let doc = server.create_workbook(&owner, "One").await;
    assert_eq!(
        server
            .request(
                Method::POST,
                "/api/workbooks",
                Some(&owner),
                Some(json!({ "title": "Two" }))
            )
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        server
            .request(
                Method::POST,
                "/api/workbooks",
                Some(&owner),
                Some(json!({ "title": "Three" }))
            )
            .await
            .status(),
        StatusCode::CREATED
    );
    let limited = server
        .request(
            Method::POST,
            "/api/workbooks",
            Some(&owner),
            Some(json!({ "title": "Four" })),
        )
        .await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers().contains_key(header::RETRY_AFTER));

    // Commit one txn so txns_committed_total increments.
    let (mut ws, welcome) = server
        .ws_connect(doc, Some(&owner), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    let rep = welcome_replica(&welcome);
    send(
        &mut ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(rep, 1, 0, 1.0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        WireSyncFrameV1::Ack { seq: 1, .. }
    ));
    assert!(matches!(
        recv(&mut ws).await,
        WireSyncFrameV1::Commit { seq: 1, .. }
    ));

    let metrics_resp = server
        .client
        .get(format!("{}/metrics", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics_resp.status(), StatusCode::OK);
    let content_type = metrics_resp.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(content_type.contains("text/plain; version=0.0.4"));
    let body = metrics_resp.text().await.unwrap();
    assert!(body.contains("reticula_active_rooms 1"));
    assert!(body.contains("reticula_active_ws_connections{kind=\"authed\"} 1"));
    assert!(body.contains("reticula_txns_committed_total 1"));
    assert!(body.contains("reticula_disk_used_bytes "));
    assert!(body.contains("reticula_rate_limited_total 1"));
    drop(ws);

    // Non-loopback ConnectInfo receives 403 Forbidden on /metrics.
    let metrics_router = http::metrics_route(Some(server.api.clone()));
    let mut external_req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    external_req
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            4321,
        )));
    let ext_resp = metrics_router.oneshot(external_req).await.unwrap();
    assert_eq!(ext_resp.status(), StatusCode::FORBIDDEN);

    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn load_machine_missing_snapshot_fallback_and_corruption_detection() {
    let db = IsolatedDb::create().await;
    let server =
        TestServer::start(&db, StorageLimits::default(), GovernanceConfig::default()).await;
    let (_, alice) = server.login("alice").await;

    let import = server
        .import_bytes(Some(&alice), "Fallback", sample_snapshot(3))
        .await;
    assert_eq!(import.status(), StatusCode::CREATED);
    let doc: Uuid = import.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let (mut ws, welcome) = server
        .ws_connect(doc, Some(&alice), None, ORIGIN)
        .await
        .unwrap()
        .pipe_hello(doc, None, 0)
        .await;
    let rep = welcome_replica(&welcome);

    for cseq in 1..=2_u64 {
        send(
            &mut ws,
            &WireSyncFrameV1::Push {
                txns: vec![edit_txn(rep, cseq, cseq - 1, 100.0 + cseq as f64)],
            },
        )
        .await;
        assert!(matches!(recv(&mut ws).await, WireSyncFrameV1::Ack { .. }));
        assert!(matches!(
            recv(&mut ws).await,
            WireSyncFrameV1::Commit { .. }
        ));
    }

    let snap2_machine = server.store.load_machine(doc).await.unwrap();
    assert_eq!(snap2_machine.head_seq(), 2);
    server.store.write_snapshot(&snap2_machine).await.unwrap();

    send(
        &mut ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit_txn(rep, 3, 2, 103.0)],
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, WireSyncFrameV1::Ack { .. }));
    assert!(matches!(
        recv(&mut ws).await,
        WireSyncFrameV1::Commit { .. }
    ));
    drop(ws);

    let expected_digest = server
        .store
        .load_machine(doc)
        .await
        .unwrap()
        .workbook()
        .state_digest();

    // Remove the non-zero snapshot (seq=2); load_machine must fall back to seq=0 and replay txns.
    let snap2_path = server
        .data_dir()
        .join(format!("snapshots/{doc}/2.rtclsnap"));
    assert!(snap2_path.exists());
    fs::remove_file(&snap2_path).unwrap();

    let recovered = server.store.load_machine(doc).await.unwrap();
    assert_eq!(recovered.head_seq(), 3);
    assert_eq!(recovered.workbook().state_digest(), expected_digest);

    // Corrupting the remaining seq=0 snapshot must still fail loudly (checksum/length mismatch).
    let snap0_path = server
        .data_dir()
        .join(format!("snapshots/{doc}/0.rtclsnap"));
    fs::write(&snap0_path, b"corrupted").unwrap();
    assert!(matches!(
        server.store.load_machine(doc).await,
        Err(StoreError::Invalid("snapshot checksum or length"))
    ));

    // Missing seq=0 base snapshot must return NotFound rather than silently fabricating an empty book.
    fs::remove_file(&snap0_path).unwrap();
    assert!(matches!(
        server.store.load_machine(doc).await,
        Err(StoreError::Io(ref err)) if err.kind() == std::io::ErrorKind::NotFound
    ));

    drop(server);
    db.cleanup().await;
}

trait SocketExt {
    async fn pipe_hello(
        self,
        doc: Uuid,
        replica: Option<u32>,
        seq: u64,
    ) -> (Socket, WireSyncFrameV1);
}

impl SocketExt for Socket {
    async fn pipe_hello(
        mut self,
        doc: Uuid,
        replica: Option<u32>,
        seq: u64,
    ) -> (Socket, WireSyncFrameV1) {
        send(&mut self, &hello_frame(doc, replica, seq)).await;
        let welcome = recv(&mut self).await;
        (self, welcome)
    }
}

fn welcome_replica(frame: &WireSyncFrameV1) -> u32 {
    match frame {
        WireSyncFrameV1::Welcome { replica, .. } => *replica,
        other => panic!("expected Welcome frame, got {other:?}"),
    }
}
