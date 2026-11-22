#![cfg(debug_assertions)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, Response, StatusCode, header};
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_core::value::Value as CellValue;
use reticula_server::store::{MAIN_BRANCH, SessionIdentity, Store};
use reticula_wire::convert_v2::ops_to_wire_txn_v2;
use reticula_wire::snapshot::decode_snapshot_any;
use reticula_wire::sync_frame::{decode_sync_frame, encode_sync_frame};
use reticula_wire::v2::{WirePendingTxnV2, WirePresenceV1, WireRejectReasonV1, WireSyncFrameV1};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

const ORIGIN: &str = "http://127.0.0.1";
const WAIT: Duration = Duration::from_secs(12);
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    child: Child,
    base: String,
    client: reqwest::Client,
}

impl Server {
    async fn start(database_url: &str, data_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_reticula-server"))
            .env("BIND_ADDR", "127.0.0.1:0")
            .env("RETICULA_AUTH_MODE", "dev-mock")
            .env("RETICULA_PUBLIC_ORIGIN", ORIGIN)
            .env("RETICULA_DATABASE_URL", database_url)
            .env("RETICULA_DATA_DIR", data_dir)
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut logs = BufReader::new(child.stdout.take().unwrap()).lines();
        let startup = timeout(WAIT, logs.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("server exited before binding");
        let base = startup
            .split_whitespace()
            .find(|part| part.starts_with("http://127.0.0.1:"))
            .expect("missing bound port")
            .to_owned();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(WAIT)
            .build()
            .unwrap();
        assert_eq!(
            client
                .get(format!("{base}/readyz"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        Self {
            child,
            base,
            client,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header(header::ORIGIN, ORIGIN);
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn login(&self, account: &str) -> String {
        let response = self
            .request(
                Method::POST,
                "/api/auth/dev-login",
                None,
                Some(json!({ "account": account })),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
        cookie.split(';').next().unwrap().to_owned()
    }

    async fn create(&self, owner: &str) -> Uuid {
        let response = self
            .request(
                Method::POST,
                "/api/workbooks",
                Some(owner),
                Some(json!({ "title": "Collaboration" })),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn link(&self, doc: Uuid, owner: &str, single_use: bool) -> String {
        let path = format!("/api/workbooks/{doc}/share-links");
        let response = self
            .request(
                Method::POST,
                &path,
                Some(owner),
                Some(json!({ "singleUse": single_use })),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        format!(
            "/api{}",
            response.json::<Value>().await.unwrap()["url"]
                .as_str()
                .unwrap()
        )
    }

    async fn connect(
        &self,
        doc: Uuid,
        cookie: Option<&str>,
        origin: &str,
    ) -> Result<Socket, tungstenite::Error> {
        let mut request = format!(
            "ws://{}/api/sync/{doc}",
            self.base.strip_prefix("http://").unwrap()
        )
        .into_client_request()?;
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
        if let Some(cookie) = cookie {
            request
                .headers_mut()
                .insert("Cookie", cookie.parse().unwrap());
        }
        timeout(WAIT, connect_async(request))
            .await
            .unwrap()
            .map(|(socket, _)| socket)
    }

    async fn hello(
        &self,
        doc: Uuid,
        cookie: &str,
        replica: Option<u32>,
        seq: u64,
    ) -> (Socket, WireSyncFrameV1) {
        let mut socket = self.connect(doc, Some(cookie), ORIGIN).await.unwrap();
        send(&mut socket, &hello_frame(doc, replica, seq)).await;
        let response = recv(&mut socket).await;
        (socket, response)
    }
}

fn database_url() -> String {
    std::env::var("RETICULA_TEST_DATABASE_URL").unwrap()
}

fn hello_frame(doc: Uuid, replica: Option<u32>, last_applied_seq: u64) -> WireSyncFrameV1 {
    WireSyncFrameV1::Hello {
        protocol_version: 1,
        doc_id: doc.into_bytes(),
        branch_id: MAIN_BRANCH.into_bytes(),
        replica,
        last_applied_seq,
        known_epoch: 0,
    }
}

fn replica(frame: WireSyncFrameV1) -> u32 {
    match frame {
        WireSyncFrameV1::Welcome { replica, .. } => replica,
        other => panic!("expected Welcome, got {other:?}"),
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
        while let Some(message) = socket.next().await {
            if let Message::Binary(bytes) = message.unwrap() {
                return decode_sync_frame(&bytes).unwrap();
            }
        }
        panic!("sync connection closed before next frame");
    })
    .await
    .unwrap()
}

fn edit(replica: u32, client_seq: u64, base_server_seq: u64) -> WirePendingTxnV2 {
    WirePendingTxnV2 {
        replica,
        client_seq,
        base_server_seq,
        timestamp_ms: 0,
        txn: ops_to_wire_txn_v2(
            "Edit".into(),
            &[Op::SetCells {
                sheet: SheetId(0),
                edits: vec![CellEdit {
                    row: RowId(client_seq),
                    col: ColId(u64::from(replica)),
                    input: CellInput::Scalar(ScalarValue::Number(client_seq as f64)),
                    format: FormatPatch::Keep,
                }],
            }],
        )
        .unwrap(),
    }
}

fn rejected_http(result: Result<Socket, tungstenite::Error>, expected: StatusCode) {
    match result {
        Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), expected),
        other => panic!("unexpected WebSocket result: {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn permission_matrix_and_binary_sync_over_http() {
    let database = database_url();
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(&database, root.path()).await;
    let owner = server.login("alice").await;
    let editor = server.login("bob").await;
    let doc = server.create(&owner).await;
    let workbook = format!("/api/workbooks/{doc}");
    let snapshot = format!("{workbook}/snapshots/0");
    let share = format!("{workbook}/share-links");
    let (_, welcome) = server.hello(doc, &owner, Some(1), 0).await;
    assert_eq!(replica(welcome), 1);
    let store = Store::connect(&database, root.path().to_owned())
        .await
        .unwrap();
    let secret = *Uuid::new_v4().as_bytes();
    let mut token = [0_u8; 32];
    token[..16].copy_from_slice(&secret);
    let outsider = format!("reticula_session={}", URL_SAFE_NO_PAD.encode(token));
    store
        .sign_in(
            &SessionIdentity {
                provider: "integration",
                subject: &Uuid::new_v4().to_string(),
                name: "Outsider",
                avatar: "",
            },
            Sha256::digest(token).into(),
            None,
            120,
        )
        .await
        .unwrap();

    let link = server.link(doc, &owner, true).await;
    assert_eq!(
        server
            .request(Method::POST, &link, None, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::POST, &link, Some(&editor), None)
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        server
            .request(Method::POST, &link, Some(&outsider), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .request(Method::POST, "/api/join/invalid", Some(&editor), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let revoked = server.link(doc, &owner, false).await;
    let expired = server.link(doc, &owner, false).await;
    let db = PgPool::connect(&database).await.unwrap();
    for (link, expired) in [(&revoked, false), (&expired, true)] {
        let token = link.rsplit('/').next().unwrap();
        let hash = Sha256::digest(URL_SAFE_NO_PAD.decode(token).unwrap());
        sqlx::query("UPDATE share_links SET revoked_at = CASE WHEN NOT $2 THEN now() END, expires_at = CASE WHEN $2 THEN now() END WHERE token_hash = $1")
            .bind(hash.as_slice()).bind(expired).execute(&db).await.unwrap();
        assert_eq!(
            server
                .request(Method::POST, link, Some(&outsider), None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    for (cookie, read, invite) in [
        (None, StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED),
        (
            Some(outsider.as_str()),
            StatusCode::FORBIDDEN,
            StatusCode::FORBIDDEN,
        ),
        (Some(editor.as_str()), StatusCode::OK, StatusCode::FORBIDDEN),
        (Some(owner.as_str()), StatusCode::OK, StatusCode::CREATED),
    ] {
        let metadata = server.request(Method::GET, &workbook, cookie, None).await;
        assert_eq!(metadata.status(), read);
        if read == StatusCode::OK {
            assert_eq!(
                metadata.headers()[header::CACHE_CONTROL],
                "private, no-store"
            );
            let content = metadata.json::<Value>().await.unwrap();
            assert_eq!(content["members"]["editor"], 1);
            assert_eq!(
                content["role"],
                if invite == StatusCode::CREATED {
                    "owner"
                } else {
                    "editor"
                }
            );
        }
        let file = server.request(Method::GET, &snapshot, cookie, None).await;
        assert_eq!(file.status(), read);
        if read == StatusCode::OK {
            assert!(decode_snapshot_any(&file.bytes().await.unwrap()).is_ok());
        }
        assert_eq!(
            server
                .request(Method::POST, &share, cookie, Some(json!({})))
                .await
                .status(),
            invite
        );
        if read != StatusCode::OK {
            rejected_http(server.connect(doc, cookie, ORIGIN).await, read);
        }
    }
    rejected_http(
        server
            .connect(doc, Some(&owner), "https://different.example")
            .await,
        StatusCode::FORBIDDEN,
    );
    assert_eq!(
        server
            .client
            .post(format!("{}{share}", server.base))
            .header(header::COOKIE, &owner)
            .header(header::ORIGIN, "https://different.example")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    let (mut owner_ws, owner_welcome) = server.hello(doc, &owner, Some(1), 0).await;
    let owner_replica = replica(owner_welcome);
    let (_, forbidden) = server.hello(doc, &editor, Some(owner_replica), 0).await;
    assert_eq!(
        forbidden,
        WireSyncFrameV1::Reject {
            client_seq: 0,
            reason: WireRejectReasonV1::Forbidden
        }
    );
    let (mut active_editor, editor_welcome) = server.hello(doc, &editor, None, 0).await;
    let editor_replica = replica(editor_welcome);
    send(
        &mut active_editor,
        &WireSyncFrameV1::Push {
            txns: vec![edit(owner_replica, 1, 0)],
        },
    )
    .await;
    assert_eq!(
        recv(&mut active_editor).await,
        WireSyncFrameV1::Reject {
            client_seq: 1,
            reason: WireRejectReasonV1::Forbidden
        }
    );

    send(
        &mut active_editor,
        &WireSyncFrameV1::Presence(WirePresenceV1 {
            replica: editor_replica,
            user_id: [0; 16],
            display_name: "Impersonation".into(),
            color_slot: 99,
            sheet_id: 0,
            cursor_row: 1,
            cursor_col: 1,
            selection: None,
        }),
    )
    .await;
    assert!(
        matches!(recv(&mut owner_ws).await, WireSyncFrameV1::Presence(value) if value.display_name == "Bob" && value.user_id != [0; 16] && value.color_slot < 8)
    );
    send(&mut owner_ws, &WireSyncFrameV1::Ping(77)).await;
    assert_eq!(recv(&mut owner_ws).await, WireSyncFrameV1::Pong(77));
    send(
        &mut active_editor,
        &WireSyncFrameV1::Push {
            txns: vec![edit(editor_replica, 1, 0)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut active_editor).await,
        WireSyncFrameV1::Ack { seq: 1, .. }
    ));
    assert!(matches!(
        recv(&mut owner_ws).await,
        WireSyncFrameV1::Commit { seq: 1, .. }
    ));
    send(
        &mut owner_ws,
        &WireSyncFrameV1::Push {
            txns: vec![edit(owner_replica, 1, 1)],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut owner_ws).await,
        WireSyncFrameV1::Ack { seq: 2, .. }
    ));
    assert_eq!(
        server
            .request(Method::GET, "/api/auth/me", Some(&editor), None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .request(Method::POST, "/api/auth/logout", Some(&editor), None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    timeout(WAIT, async {
        while let Some(Ok(message)) = active_editor.next().await {
            if message.is_close() {
                break;
            }
        }
    })
    .await
    .expect("logout left the WebSocket active");
    rejected_http(
        server.connect(doc, Some(&editor), ORIGIN).await,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        server
            .request(Method::GET, "/api/auth/me", Some(&editor), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .request(
                Method::POST,
                "/api/client-errors",
                None,
                Some(json!({ "version": "test", "summary": "test report" }))
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server
            .request(Method::GET, "/api/openapi.json", None, None)
            .await
            .status(),
        StatusCode::OK
    );
    server.child.kill().await.unwrap();
}

async fn ack_seqs(socket: &mut Socket, count: usize) -> Vec<u64> {
    let mut seen = Vec::new();
    while seen.len() < count {
        if let WireSyncFrameV1::Ack { seq, .. } = recv(socket).await {
            seen.push(seq);
        }
    }
    seen
}

async fn push_edits(
    socket: &mut Socket,
    replica: u32,
    sequences: std::ops::RangeInclusive<u64>,
    base: u64,
) {
    let frame = WireSyncFrameV1::Push {
        txns: sequences.map(|seq| edit(replica, seq, base)).collect(),
    };
    send(socket, &frame).await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn forced_process_exit_preserves_every_observed_ack() {
    let database = database_url();
    let tag = format!("reticula-kill-{}", Uuid::new_v4());
    let mut url = reqwest::Url::parse(&database).unwrap();
    url.query_pairs_mut().append_pair("application_name", &tag);
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(url.as_str(), root.path()).await;
    let owner = server.login("alice").await;
    let editor = server.login("bob").await;
    let doc = server.create(&owner).await;
    let invite = server.link(doc, &owner, false).await;
    assert_eq!(
        server
            .request(Method::POST, &invite, Some(&editor), None)
            .await
            .status(),
        StatusCode::OK
    );
    let (mut a, welcome_a) = server.hello(doc, &owner, Some(1), 0).await;
    let (mut b, welcome_b) = server.hello(doc, &editor, None, 0).await;
    let (ra, rb) = (replica(welcome_a), replica(welcome_b));
    tokio::join!(
        push_edits(&mut a, ra, 1..=16, 0),
        push_edits(&mut b, rb, 1..=16, 0)
    );
    let (mut acks, other) = tokio::join!(ack_seqs(&mut a, 16), ack_seqs(&mut b, 16));
    acks.extend(other);
    acks.sort_unstable();
    assert_eq!(acks, (1..=32).collect::<Vec<_>>());

    let db = PgPool::connect(&database).await.unwrap();
    let mut lock = db.begin().await.unwrap();
    sqlx::query("SELECT id FROM workbooks WHERE id = $1 FOR UPDATE")
        .bind(doc)
        .execute(&mut *lock)
        .await
        .unwrap();
    tokio::join!(
        push_edits(&mut a, ra, 17..=48, 32),
        push_edits(&mut b, rb, 17..=48, 32)
    );
    timeout(WAIT, async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock')")
                .bind(&tag).fetch_one(&db).await.unwrap();
            if waiting { break }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("server never reached the blocked write");
    server.child.start_kill().unwrap();
    let termination = server.child.wait().await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(termination.signal(), Some(9));
    }
    #[cfg(not(unix))]
    assert!(!termination.success());
    lock.rollback().await.unwrap();

    let mut recovered = Server::start(&database, root.path()).await;
    let (_, welcome) = recovered.hello(doc, &owner, Some(ra), 0).await;
    assert!(matches!(
        welcome,
        WireSyncFrameV1::Welcome {
            head_seq: 32,
            last_client_seq: 16,
            ..
        }
    ));
    let persisted: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM txns WHERE workbook_id = $1 ORDER BY seq")
            .bind(doc)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        persisted,
        acks.into_iter().map(|seq| seq as i64).collect::<Vec<_>>()
    );
    let store = Store::connect(&database, root.path().to_owned())
        .await
        .unwrap();
    let machine = store.load_machine(doc).await.unwrap();
    let sheet = machine.workbook().sheet(SheetId(0)).unwrap();
    for replica in [ra, rb] {
        for seq in 1..=16 {
            assert_eq!(
                sheet.value_by_id(RowId(seq), ColId(u64::from(replica))),
                CellValue::Number(seq as f64)
            );
        }
    }
    recovered.child.kill().await.unwrap();
}
