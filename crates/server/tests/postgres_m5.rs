#![cfg(debug_assertions)]

//! Tables of workbooks: who may do what, uploads that are cut short, what an
//! owner may store, and what is left when something is deleted.

#[path = "support/table_fixture.rs"]
mod table_fixture;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Method, Response, StatusCode, Url, header};
use reticula_server::agent::AgentRuntimeConfig;
use reticula_server::http::{self, ApiState, AuthConfig};
use reticula_server::ops::GovernanceConfig;
use reticula_server::store::{
    DELETED_USER_ID, SessionIdentity, StorageLimits, Store, TABLE_UPLOAD_IDLE,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use table_fixture::{table_file, table_file_of};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const ORIGIN: &str = "http://127.0.0.1";
const WAIT: Duration = Duration::from_secs(15);
const MIB: usize = 1024 * 1024;

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
        let name = format!("reticula_m5_{}", Uuid::new_v4().simple());
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
    app: Router,
    root: TempDir,
    store: Store,
    pool: PgPool,
    base: String,
    addr: SocketAddr,
    client: reqwest::Client,
    handle: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Someone signed in.
struct Person {
    cookie: String,
    id: Uuid,
}

/// A request, as one of the identities makes it.
#[derive(Clone, Copy, Default)]
struct As<'a> {
    cookie: Option<&'a str>,
    share: Option<&'a str>,
}

fn by(person: &Person) -> As<'_> {
    As {
        cookie: Some(&person.cookie),
        share: None,
    }
}

impl TestServer {
    async fn start(db: &IsolatedDb) -> Self {
        Self::with(db, StorageLimits::default(), GovernanceConfig::default()).await
    }

    async fn with(db: &IsolatedDb, limits: StorageLimits, gov: GovernanceConfig) -> Self {
        let root = tempfile::tempdir().unwrap();
        // Writes are limited by the minute, and these tests are fast.
        let gov = GovernanceConfig {
            write_rpm: 10_000,
            ..gov
        };
        let store = Store::connect_with_limits(&db.url, root.path().to_path_buf(), limits)
            .await
            .unwrap();
        let api = ApiState::with_agent(
            store.clone(),
            AuthConfig::dev_mock(ORIGIN).unwrap(),
            gov,
            AgentRuntimeConfig::mock_for_tests(),
        );
        let app = Router::new().nest("/api", http::router(api));
        let served = app.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                served.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        Self {
            app,
            root,
            store,
            pool: PgPoolOptions::new()
                .max_connections(2)
                .connect(&db.url)
                .await
                .unwrap(),
            base: format!("http://{addr}"),
            addr,
            client: client(),
            handle,
        }
    }

    async fn person(&self, name: &str) -> Person {
        let mut token = [0_u8; 32];
        token[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        let user = self
            .store
            .sign_in(
                &SessionIdentity {
                    provider: "integration",
                    subject: &Uuid::new_v4().to_string(),
                    name,
                    avatar: "",
                },
                Sha256::digest(token).into(),
                None,
                600,
            )
            .await
            .unwrap();
        Person {
            cookie: format!("reticula_session={}", URL_SAFE_NO_PAD.encode(token)),
            id: user.id,
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        who: As<'_>,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Response {
        let mut request = self
            .client
            .request(method, format!("{}/api{path}", self.base))
            .header(header::ORIGIN, ORIGIN);
        if let Some(cookie) = who.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(share) = who.share {
            request = request.header("x-reticula-share", share);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send().await.unwrap()
    }

    async fn json(&self, method: Method, path: &str, who: As<'_>, body: Value) -> (u16, Value) {
        answer(
            self.send(
                method,
                path,
                who,
                &[("content-type", "application/json")],
                Some(body.to_string().into_bytes()),
            )
            .await,
        )
        .await
    }

    async fn get(&self, path: &str, who: As<'_>) -> (u16, Value) {
        answer(self.send(Method::GET, path, who, &[], None).await).await
    }

    async fn workbook(&self, owner: &Person) -> Uuid {
        let (status, body) = self
            .json(
                Method::POST,
                "/workbooks",
                by(owner),
                json!({ "title": "Tables" }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        body["id"].as_str().unwrap().parse().unwrap()
    }

    async fn join(&self, doc: Uuid, owner: &Person, person: &Person, role: &str) {
        let (status, link) = self
            .json(
                Method::POST,
                &format!("/workbooks/{doc}/share-links"),
                by(owner),
                json!({ "role": role }),
            )
            .await;
        assert_eq!(status, 201, "{link}");
        let url = link["url"].as_str().unwrap();
        let token = url
            .strip_prefix("/join/")
            .or_else(|| url.split("share=").nth(1))
            .unwrap();
        let (status, body) = self
            .json(
                Method::POST,
                &format!("/join/{token}"),
                by(person),
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }

    /// A link for anyone, and its ID.
    async fn open_link(&self, doc: Uuid, owner: &Person) -> (String, Uuid) {
        let (status, link) = self
            .json(
                Method::POST,
                &format!("/workbooks/{doc}/share-links"),
                by(owner),
                json!({ "role": "viewer" }),
            )
            .await;
        assert_eq!(status, 201, "{link}");
        let (status, body) = self
            .json(
                Method::PATCH,
                &format!("/workbooks/{doc}"),
                by(owner),
                json!({ "visibility": "link_view" }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let token = link["url"]
            .as_str()
            .unwrap()
            .split("share=")
            .nth(1)
            .unwrap();
        (
            token.to_owned(),
            link["id"].as_str().unwrap().parse().unwrap(),
        )
    }

    async fn declare(&self, doc: Uuid, id: Uuid, who: As<'_>, bytes: &[u8]) -> (u16, Value) {
        self.json(
            Method::POST,
            &format!("/workbooks/{doc}/tables"),
            who,
            declaration(id, bytes),
        )
        .await
    }

    async fn patch(
        &self,
        doc: Uuid,
        id: Uuid,
        who: As<'_>,
        offset: usize,
        bytes: &[u8],
    ) -> (u16, Value) {
        answer(
            self.send(
                Method::PATCH,
                &format!("/workbooks/{doc}/tables/{id}/data"),
                who,
                &[("upload-offset", &offset.to_string())],
                Some(bytes.to_vec()),
            )
            .await,
        )
        .await
    }

    async fn complete(&self, doc: Uuid, id: Uuid, who: As<'_>) -> (u16, Value) {
        self.json(
            Method::POST,
            &format!("/workbooks/{doc}/tables/{id}/complete"),
            who,
            json!({}),
        )
        .await
    }

    /// Uploads a table from the beginning to the end.
    async fn upload(&self, doc: Uuid, who: As<'_>, bytes: &[u8]) -> Uuid {
        let id = Uuid::new_v4();
        let (status, body) = self.declare(doc, id, who, bytes).await;
        assert_eq!(status, 201, "{body}");
        let mut offset = 0;
        for chunk in bytes.chunks(8 * MIB) {
            let (status, body) = self.patch(doc, id, who, offset, chunk).await;
            offset += chunk.len();
            assert_eq!((status, &body["uploadOffset"]), (200, &json!(offset)));
        }
        let (status, body) = self.complete(doc, id, who).await;
        assert_eq!((status, &body["status"]), (200, &json!("ready")), "{body}");
        id
    }

    async fn read(&self, doc: Uuid, id: Uuid, who: As<'_>, headers: &[(&str, &str)]) -> Response {
        self.send(
            Method::GET,
            &format!("/workbooks/{doc}/tables/{id}/data"),
            who,
            headers,
            None,
        )
        .await
    }

    async fn table_bytes(&self, person: &Person) -> i64 {
        sqlx::query_scalar("SELECT table_bytes FROM users WHERE id = $1")
            .bind(person.id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn status_in_database(&self, doc: Uuid, id: Uuid) -> Option<String> {
        sqlx::query_scalar("SELECT status FROM tables WHERE workbook_id = $1 AND id = $2")
            .bind(doc)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .unwrap()
    }

    /// As if a table had been declared two hours ago.
    async fn age(&self, id: Uuid) {
        sqlx::query("UPDATE tables SET created_at = now() - interval '2 hours' WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn drop_idle(&self) -> usize {
        self.store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap()
    }

    /// Makes a request and goes away after its handler has been polled so often.
    async fn cut_short(&self, method: Method, path: &str, who: &Person, body: &str, polls: usize) {
        let request = axum::http::Request::builder()
            .method(method)
            .uri(format!("/api{path}"))
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, &who.cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap();
        let mut answer = std::pin::pin!(tower::ServiceExt::oneshot(self.app.clone(), request));
        let mut left = polls;
        std::future::poll_fn(|context| {
            if left == 0 {
                return std::task::Poll::Ready(());
            }
            left -= 1;
            answer.as_mut().poll(context).map(|_| ())
        })
        .await;
    }

    fn part(&self, doc: Uuid, id: Uuid) -> PathBuf {
        self.file(doc, id, "part")
    }

    fn file(&self, doc: Uuid, id: Uuid, kind: &str) -> PathBuf {
        table_path(self.root.path(), doc, id, kind)
    }

    /// Starts an upload whose body stays open after `sent`.
    async fn hold(
        &self,
        doc: Uuid,
        id: Uuid,
        who: As<'_>,
        length: usize,
        sent: &[u8],
    ) -> TcpStream {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream
            .write_all(
                format!(
                    "PATCH /api/workbooks/{doc}/tables/{id}/data HTTP/1.1\r\nHost: x\r\n\
                     Origin: {ORIGIN}\r\nCookie: {}\r\nUpload-Offset: 0\r\n\
                     Content-Length: {length}\r\nConnection: close\r\n\r\n",
                    who.cookie.unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(sent).await.unwrap();
        // It is under way once something has been written.
        let part = self.part(doc, id);
        eventually("the upload to start", || {
            std::fs::metadata(&part).is_ok_and(|found| found.len() == sent.len() as u64)
        })
        .await;
        stream
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(WAIT)
        .build()
        .unwrap()
}

fn table_path(root: &Path, doc: Uuid, id: Uuid, kind: &str) -> PathBuf {
    root.join(format!("tables/{doc}/{id}.{kind}"))
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn declaration(id: Uuid, bytes: &[u8]) -> Value {
    json!({ "id": id, "name": "Orders", "bytes": bytes.len(), "sha256": hex_digest(bytes) })
}

async fn answer(response: Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(10)).await;
    }
}

fn header_of(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn permission_matrix_six_identities_across_all_table_routes() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let editor = server.person("Editor").await;
    let viewer = server.person("Viewer").await;
    let outsider = server.person("Outsider").await;
    let doc = server.workbook(&owner).await;
    server.join(doc, &owner, &editor, "editor").await;
    server.join(doc, &owner, &viewer, "viewer").await;
    let (link, _) = server.open_link(doc, &owner).await;
    let bytes = table_file(3_000, 1);
    let ready = server.upload(doc, by(&owner), &bytes).await;

    let anonymous = As {
        cookie: None,
        share: Some(&link),
    };
    let nobody = As::default();
    // What each may do: (reads, writes).
    for (who, name, reads, writes) in [
        (nobody, "nobody", 401, 401),
        (anonymous, "anonymous with a link", 200, 403),
        (by(&outsider), "outsider", 403, 403),
        (by(&viewer), "viewer", 200, 403),
        (by(&editor), "editor", 200, 0),
        (by(&owner), "owner", 200, 0),
    ] {
        let tables = format!("/workbooks/{doc}/tables");
        let data = format!("{tables}/{ready}/data");
        for (method, path) in [
            (Method::GET, &tables),
            (Method::GET, &data),
            (Method::HEAD, &data),
        ] {
            let response = server.send(method.clone(), path, who, &[], None).await;
            assert_eq!(response.status().as_u16(), reads, "{name}: {method} {path}");
            assert_eq!(header_of(&response, "cache-control"), "private, no-store");
            if reads != 200 {
                assert!(!response.headers().contains_key("etag"), "{name}");
            }
        }

        // Each works on a table of their own, or is refused on the one there is.
        let allowed = writes == 0;
        let id = if allowed { Uuid::new_v4() } else { ready };
        let path = format!("{tables}/{id}");
        let rename = json!({ "name": format!("Of the {name}"), "position": 0 });
        let outcomes = [
            (server.declare(doc, id, who, &bytes).await, 201),
            (server.patch(doc, id, who, 0, &bytes).await, 200),
            (server.complete(doc, id, who).await, 200),
            (server.json(Method::PATCH, &path, who, rename).await, 200),
            (
                answer(server.send(Method::DELETE, &path, who, &[], None).await).await,
                204,
            ),
        ];
        for (at, ((status, body), success)) in outcomes.into_iter().enumerate() {
            let expected = if allowed { success } else { writes };
            assert_eq!(status, expected, "{name}: write {at}: {body}");
        }
    }

    // Nothing that was refused did anything.
    let (_, list) = server
        .get(&format!("/workbooks/{doc}/tables"), by(&owner))
        .await;
    assert_eq!(list["tables"].as_array().unwrap().len(), 1);
    assert_eq!(list["tables"][0]["name"], "Orders");
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);

    // A page of another site cannot write in the name of its visitor.
    for (method, path) in [
        (Method::POST, format!("/workbooks/{doc}/tables")),
        (
            Method::PATCH,
            format!("/workbooks/{doc}/tables/{ready}/data"),
        ),
        (Method::DELETE, format!("/workbooks/{doc}/tables/{ready}")),
    ] {
        let status = server
            .client
            .request(method, format!("{}/api{path}", server.base))
            .header(header::ORIGIN, "https://elsewhere.example")
            .header(header::COOKIE, &owner.cookie)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // A table is found in its own workbook only.
    let other = server.workbook(&owner).await;
    assert_eq!(
        server.read(other, ready, by(&owner), &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    let (status, _) = server
        .json(
            Method::PATCH,
            &format!("/workbooks/{other}/tables/{ready}"),
            by(&owner),
            json!({ "name": "Taken" }),
        )
        .await;
    assert_eq!(status, 404);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn access_that_has_ended() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let editor = server.person("Editor").await;
    let viewer = server.person("Viewer").await;
    let doc = server.workbook(&owner).await;
    server.join(doc, &owner, &editor, "editor").await;
    server.join(doc, &owner, &viewer, "viewer").await;
    let (link, link_id) = server.open_link(doc, &owner).await;
    let anonymous = As {
        cookie: None,
        share: Some(&link),
    };
    let bytes = table_file(3_000, 2);
    let table = server.upload(doc, by(&editor), &bytes).await;
    let reads = async |who| {
        [
            server.get(&format!("/workbooks/{doc}/tables"), who).await.0,
            server.read(doc, table, who, &[]).await.status().as_u16(),
            server
                .read(doc, table, who, &[("range", "bytes=0-9")])
                .await
                .status()
                .as_u16(),
        ]
    };
    for who in [anonymous, by(&viewer), by(&editor)] {
        assert_eq!(reads(who).await, [200, 200, 206]);
    }

    // Downgraded: reads, and writes no more, not even to an upload of theirs.
    let pending = Uuid::new_v4();
    let upload = table_file(5_000, 3);
    assert_eq!(
        server.declare(doc, pending, by(&editor), &upload).await.0,
        201
    );
    assert_eq!(
        server
            .patch(doc, pending, by(&editor), 0, &upload[..1_000])
            .await
            .0,
        200
    );
    let (status, _) = server
        .json(
            Method::PATCH,
            &format!("/workbooks/{doc}/members/{}", editor.id),
            by(&owner),
            json!({ "role": "viewer" }),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(reads(by(&editor)).await, [200, 200, 206]);
    assert_eq!(
        server
            .patch(doc, pending, by(&editor), 1_000, &upload[1_000..])
            .await
            .0,
        403
    );
    assert_eq!(server.complete(doc, pending, by(&editor)).await.0, 403);
    assert_eq!(
        server
            .declare(doc, Uuid::new_v4(), by(&editor), &bytes)
            .await
            .0,
        403
    );
    // The upload is not listed for a viewer either.
    let (_, list) = server
        .get(&format!("/workbooks/{doc}/tables"), by(&editor))
        .await;
    assert_eq!(list["tables"].as_array().unwrap().len(), 1);
    // Someone else may carry on with it.
    assert_eq!(
        server
            .patch(doc, pending, by(&owner), 1_000, &upload[1_000..])
            .await
            .0,
        200
    );
    assert_eq!(server.complete(doc, pending, by(&owner)).await.0, 200);

    // Removed.
    let removed = server
        .send(
            Method::DELETE,
            &format!("/workbooks/{doc}/members/{}", viewer.id),
            by(&owner),
            &[],
            None,
        )
        .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(reads(by(&viewer)).await, [403, 403, 403]);

    // Signed out.
    let out = server
        .send(Method::POST, "/auth/logout", by(&editor), &[], None)
        .await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);
    assert_eq!(reads(by(&editor)).await, [401, 401, 401]);

    // Links off, on again, and this one revoked.
    for (visibility, expected) in [("private", 403), ("link_view", 200)] {
        let (status, _) = server
            .json(
                Method::PATCH,
                &format!("/workbooks/{doc}"),
                by(&owner),
                json!({ "visibility": visibility }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(reads(anonymous).await[0], expected);
        assert_eq!(reads(anonymous).await[1], expected);
    }
    let revoked = server
        .send(
            Method::DELETE,
            &format!("/workbooks/{doc}/share-links/{link_id}"),
            by(&owner),
            &[],
            None,
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    assert_eq!(reads(anonymous).await, [403, 403, 403]);

    // Deleted.
    assert_eq!(reads(by(&owner)).await, [200, 200, 206]);
    let deleted = server
        .send(
            Method::DELETE,
            &format!("/workbooks/{doc}"),
            by(&owner),
            &[],
            None,
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(reads(by(&owner)).await, [403, 403, 403]);
    drop(server);
    db.cleanup().await;
}

/// A request that waits its turn is for what its sender may do by then.
#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn access_that_ends_while_a_request_waits() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let editor = server.person("Editor").await;
    let doc = server.workbook(&owner).await;
    server.join(doc, &owner, &editor, "editor").await;
    let bytes = table_file(10_000, 1);
    let id = Uuid::new_v4();
    assert_eq!(server.declare(doc, id, by(&owner), &bytes).await.0, 201);
    let mut held = server.hold(doc, id, by(&owner), 1_000, &bytes[..10]).await;
    let waiting = async {
        tokio::join!(
            server.patch(doc, id, by(&editor), 1_000, &bytes[1_000..]),
            server.complete(doc, id, by(&editor)),
        )
    };
    let ending = async {
        sleep(Duration::from_millis(200)).await;
        let removed = server
            .send(
                Method::DELETE,
                &format!("/workbooks/{doc}/members/{}", editor.id),
                by(&owner),
                &[],
                None,
            )
            .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        held.write_all(&bytes[10..1_000]).await.unwrap();
    };
    let (((patch, _), (complete, _)), ()) = tokio::join!(waiting, ending);
    assert_eq!((patch, complete), (403, 403));
    assert_eq!(
        std::fs::metadata(server.part(doc, id)).unwrap().len(),
        1_000
    );
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn an_upload_in_pieces() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);
    let bytes = table_file(9 * MIB, 4);
    let id = Uuid::new_v4();

    let (status, record) = server.declare(doc, id, me, &bytes).await;
    assert_eq!(status, 201);
    assert_eq!(record["status"], "uploading");
    assert_eq!(record["uploadOffset"], 0);
    assert_eq!(record["sha256"], hex_digest(&bytes));
    // Not there to be read yet.
    assert_eq!(
        server.read(doc, id, me, &[]).await.status(),
        StatusCode::NOT_FOUND
    );

    // More than a request may have, by its length and without one.
    let (status, _) = server.patch(doc, id, me, 0, &bytes[..8 * MIB + 1]).await;
    assert_eq!(status, 413);
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(
            format!(
                "PATCH /api/workbooks/{doc}/tables/{id}/data HTTP/1.1\r\nHost: x\r\nOrigin: {ORIGIN}\r\n\
                 Cookie: {}\r\nUpload-Offset: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                owner.cookie
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    for chunk in bytes.chunks(MIB) {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(chunk).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
    }
    stream.write_all(b"0\r\n\r\n").await.unwrap();
    let mut reply = String::new();
    timeout(WAIT, stream.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
    // It left nothing behind.
    assert_eq!(std::fs::metadata(server.part(doc, id)).unwrap().len(), 0);

    // Where the bytes go has to be said, and be where the last ones ended.
    for offset in ["", "-1", "+0", "x", "0x10"] {
        let response = server
            .send(
                Method::PATCH,
                &format!("/workbooks/{doc}/tables/{id}/data"),
                me,
                &[("upload-offset", offset)],
                Some(vec![1]),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{offset:?}");
    }
    let (status, body) = server.patch(doc, id, me, 0, &bytes[..MIB]).await;
    assert_eq!((status, &body["uploadOffset"]), (200, &json!(MIB)));
    for offset in [0, MIB - 1, MIB + 1, usize::MAX] {
        let (status, body) = server.patch(doc, id, me, offset, &bytes[..8 * MIB]).await;
        assert_eq!(
            (status, &body["uploadOffset"]),
            (409, &json!(MIB)),
            "{body}"
        );
    }

    // A connection that breaks keeps what arrived.
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(
            format!(
                "PATCH /api/workbooks/{doc}/tables/{id}/data HTTP/1.1\r\nHost: x\r\nOrigin: {ORIGIN}\r\n\
                 Cookie: {}\r\nUpload-Offset: {MIB}\r\nContent-Length: {}\r\n\r\n",
                owner.cookie,
                4 * MIB
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(&bytes[MIB..2 * MIB + 7]).await.unwrap();
    stream.shutdown().await.unwrap();
    let part = server.part(doc, id);
    eventually("what arrived to be written", || {
        std::fs::metadata(&part).unwrap().len() == 2 * MIB as u64 + 7
    })
    .await;
    drop(stream);
    let (status, record) = server.declare(doc, id, me, &bytes).await;
    assert_eq!(
        (status, &record["uploadOffset"]),
        (200, &json!(2 * MIB + 7))
    );

    // Not all of it is there.
    let (status, body) = server.complete(doc, id, me).await;
    assert_eq!((status, &body["uploadOffset"]), (409, &json!(2 * MIB + 7)));

    let rest = 2 * MIB + 7;
    let (status, body) = server.patch(doc, id, me, rest, &bytes[rest..]).await;
    assert_eq!((status, &body["uploadOffset"]), (200, &json!(bytes.len())));
    // Nor more than was declared, and nothing at all is fine.
    assert_eq!(server.patch(doc, id, me, bytes.len(), &[0]).await.0, 413);
    assert_eq!(server.patch(doc, id, me, bytes.len(), &[]).await.0, 200);
    let (status, record) = server.complete(doc, id, me).await;
    assert_eq!((status, &record["status"]), (200, &json!("ready")));
    assert!(record.get("uploadOffset").is_none());
    assert!(!server.part(doc, id).exists());
    let read = server.read(doc, id, me, &[]).await;
    assert_eq!(read.bytes().await.unwrap().as_ref(), bytes);

    // It is over: again is the same, and adding to it is not possible.
    assert_eq!(server.complete(doc, id, me).await, (200, record));
    assert_eq!(server.patch(doc, id, me, bytes.len(), &[]).await.0, 409);
    let (status, again) = server.declare(doc, id, me, &bytes).await;
    assert_eq!((status, &again["status"]), (200, &json!("ready")));
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn declaring_a_table() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);
    let path = format!("/workbooks/{doc}/tables");
    let bytes = table_file(1_000, 5);
    let good = declaration(Uuid::new_v4(), &bytes);
    let with = |key: &str, value: Value| {
        let mut body = good.clone();
        body[key] = value;
        body
    };
    for (body, expected) in [
        (with("sha256", json!("abc")), 400),
        (with("sha256", json!("g".repeat(64))), 400),
        (with("name", json!("  ")), 422),
        (with("name", json!("a\u{0}b")), 422),
        (with("name", json!("n".repeat(121))), 422),
        (with("bytes", json!(0)), 422),
        (with("bytes", json!(79)), 422),
        (with("bytes", json!(-1)), 422),
        (with("id", json!("table")), 422),
        (with("bytes", json!(513 * MIB)), 413),
        (with("bytes", json!(u64::MAX)), 413),
        (json!({}), 422),
    ] {
        let (status, answer) = server.json(Method::POST, &path, me, body.clone()).await;
        assert_eq!(status, expected, "{body}: {answer}");
    }
    assert_eq!(server.table_bytes(&owner).await, 0);

    // The same again changes nothing.
    assert_eq!(
        server.json(Method::POST, &path, me, good.clone()).await.0,
        201
    );
    let (status, record) = server.json(Method::POST, &path, me, good.clone()).await;
    assert_eq!((status, &record["uploadOffset"]), (200, &json!(0)));
    let digits = good["sha256"].as_str().unwrap().to_uppercase();
    assert_eq!(
        server
            .json(Method::POST, &path, me, with("sha256", json!(digits)))
            .await
            .0,
        200
    );
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);
    // Other bytes under its ID do not replace it.
    for other in [
        with("sha256", json!("0".repeat(64))),
        with("bytes", json!(bytes.len() + 1)),
    ] {
        assert_eq!(server.json(Method::POST, &path, me, other).await.0, 409);
    }

    // At the same time, too.
    let id = good["id"].as_str().unwrap();
    let deleted = server
        .send(Method::DELETE, &format!("{path}/{id}"), me, &[], None)
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let twice = futures_util::future::join_all(
        (0..6).map(|_| server.json(Method::POST, &path, me, good.clone())),
    )
    .await;
    let mut statuses: Vec<u16> = twice.iter().map(|(status, _)| *status).collect();
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 200, 200, 200, 200, 201]);
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn requests_for_one_table_at_the_same_time() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);
    let bytes = table_file(3 * MIB, 6);
    let id = Uuid::new_v4();
    server.declare(doc, id, me, &bytes).await;

    // One of those that all start at the same place gets to write.
    let all = futures_util::future::join_all(
        (0..4).map(|_| server.patch(doc, id, me, 0, &bytes[..2 * MIB])),
    )
    .await;
    let written = all.iter().filter(|(status, _)| *status == 200).count();
    assert_eq!(written, 1, "{all:?}");
    for (status, body) in &all {
        assert!([200, 409].contains(status));
        assert_eq!(body["uploadOffset"], 2 * MIB);
    }
    let (status, _) = server.patch(doc, id, me, 2 * MIB, &bytes[2 * MIB..]).await;
    assert_eq!(status, 200);
    let all = futures_util::future::join_all((0..3).map(|_| server.complete(doc, id, me))).await;
    for (status, record) in &all {
        assert_eq!((*status, &record["status"]), (200, &json!("ready")));
    }

    // A deletion does not wait for a body that is slow to arrive.
    let slow = Uuid::new_v4();
    server.declare(doc, slow, me, &bytes).await;
    let mut held = server
        .hold(doc, slow, me, bytes.len(), &bytes[..1_000])
        .await;
    let started = Instant::now();
    let deleted = server
        .send(
            Method::DELETE,
            &format!("/workbooks/{doc}/tables/{slow}"),
            me,
            &[],
            None,
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!server.part(doc, slow).exists());
    // Whoever was sending it learns that it is gone.
    held.write_all(&bytes[1_000..]).await.unwrap();
    let mut reply = String::new();
    timeout(WAIT, held.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    assert!(!server.part(doc, slow).exists());
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn bytes_that_are_not_those_declared() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);

    // The right number of other bytes.
    let bytes = table_file(50_000, 7);
    let wrong = table_file(50_000, 8);
    let id = Uuid::new_v4();
    server.declare(doc, id, me, &bytes).await;
    assert_eq!(server.patch(doc, id, me, 0, &wrong).await.0, 200);
    let (status, body) = server.complete(doc, id, me).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(
        body["error"],
        "the table file does not have the declared checksum"
    );
    // They are gone, and the upload can start over.
    assert_eq!(std::fs::metadata(server.part(doc, id)).unwrap().len(), 0);
    assert_eq!(
        server.status_in_database(doc, id).await.unwrap(),
        "uploading"
    );
    assert_eq!(server.patch(doc, id, me, 0, &bytes).await.0, 200);
    assert_eq!(server.complete(doc, id, me).await.0, 200);

    // Bytes with the right checksum that are no table.
    let mut other_magic = table_file(5_000, 9);
    other_magic[0] = b'P';
    let mut cut = table_file(5_000, 9);
    cut.drain(100..200);
    for (bytes, reason) in [
        (other_magic, "this is not a table file"),
        (cut, "the table file is not as long as it says it is"),
    ] {
        let id = Uuid::new_v4();
        assert_eq!(server.declare(doc, id, me, &bytes).await.0, 201);
        assert_eq!(server.patch(doc, id, me, 0, &bytes).await.0, 200);
        let (status, body) = server.complete(doc, id, me).await;
        assert_eq!((status, &body["error"]), (422, &json!(reason)));
        assert_eq!(std::fs::metadata(server.part(doc, id)).unwrap().len(), 0);
        let deleted = server
            .send(
                Method::DELETE,
                &format!("/workbooks/{doc}/tables/{id}"),
                me,
                &[],
                None,
            )
            .await;
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    }

    // A file longer than declared, however that came about.
    let bytes = table_file(5_000, 10);
    let id = Uuid::new_v4();
    server.declare(doc, id, me, &bytes).await;
    server.patch(doc, id, me, 0, &bytes).await;
    let mut longer = bytes.clone();
    longer.push(0);
    std::fs::write(server.part(doc, id), &longer).unwrap();
    assert_eq!(server.complete(doc, id, me).await.0, 422);
    assert_eq!(std::fs::metadata(server.part(doc, id)).unwrap().len(), 0);

    // Checked, and the server stopped before it said so: an upload again.
    server.patch(doc, id, me, 0, &bytes).await;
    std::fs::rename(server.part(doc, id), server.file(doc, id, "rtbl")).unwrap();
    let (_, list) = server.get(&format!("/workbooks/{doc}/tables"), me).await;
    let listed = list["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|table| table["id"] == id.to_string());
    assert_eq!(listed.unwrap()["uploadOffset"], bytes.len());
    let (status, record) = server.declare(doc, id, me, &bytes).await;
    assert_eq!(
        (status, &record["uploadOffset"]),
        (200, &json!(bytes.len()))
    );
    let (status, record) = server.complete(doc, id, me).await;
    assert_eq!((status, &record["status"]), (200, &json!("ready")));
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn ranges_of_bytes() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);
    let bytes = table_file(300_000, 11);
    let length = bytes.len();
    let id = server.upload(doc, me, &bytes).await;
    let etag = format!("\"{}\"", hex_digest(&bytes));

    let whole = server.read(doc, id, me, &[]).await;
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(header_of(&whole, "etag"), etag);
    assert_eq!(header_of(&whole, "accept-ranges"), "bytes");
    assert_eq!(header_of(&whole, "content-length"), length.to_string());
    assert_eq!(
        header_of(&whole, "content-type"),
        "application/octet-stream"
    );
    assert_eq!(header_of(&whole, "cache-control"), "private, no-store");
    assert_eq!(header_of(&whole, "x-content-type-options"), "nosniff");
    assert_eq!(whole.bytes().await.unwrap().as_ref(), bytes);

    for (range, start, end) in [
        ("bytes=0-0".to_owned(), 0, 1),
        ("bytes=100-199".to_owned(), 100, 200),
        // Over the chunks it is read in.
        ("bytes=65000-200000".to_owned(), 65_000, 200_001),
        (format!("bytes={}-", length - 1), length - 1, length),
        (format!("bytes=10-{}", length * 2), 10, length),
        ("bytes=-64".to_owned(), length - 64, length),
        (format!("bytes=-{}", length * 2), 0, length),
    ] {
        for validator in [None, Some(etag.as_str())] {
            let mut headers = vec![("range", range.as_str())];
            headers.extend(validator.map(|etag| ("if-range", etag)));
            let part = server.read(doc, id, me, &headers).await;
            assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT, "{range}");
            assert_eq!(
                header_of(&part, "content-range"),
                format!("bytes {start}-{}/{length}", end - 1)
            );
            assert_eq!(
                header_of(&part, "content-length"),
                (end - start).to_string()
            );
            assert_eq!(header_of(&part, "etag"), etag);
            assert_eq!(part.bytes().await.unwrap().as_ref(), &bytes[start..end]);
        }
    }

    for range in [
        format!("bytes={length}-"),
        format!("bytes={}-{}", length + 5, length + 9),
        "bytes=-0".to_owned(),
        "bytes=9-8".to_owned(),
        "bytes=0-1,5-6".to_owned(),
        "bytes=x".to_owned(),
    ] {
        let none = server.read(doc, id, me, &[("range", &range)]).await;
        assert_eq!(none.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(
            header_of(&none, "content-range"),
            format!("bytes */{length}")
        );
        assert!(none.bytes().await.unwrap().is_empty());
    }

    // Of another file, or of none: everything.
    for (range, validator) in [
        ("bytes=0-9", "\"0000\""),
        ("bytes=0-9", "Wed, 21 Oct 2015 07:28:00 GMT"),
        ("rows=0-9", etag.as_str()),
    ] {
        let whole = server
            .read(doc, id, me, &[("range", range), ("if-range", validator)])
            .await;
        assert_eq!(whole.status(), StatusCode::OK, "{range} {validator}");
        assert_eq!(whole.bytes().await.unwrap().len(), length);
    }

    // The same without the bytes. A range is of what GET would send.
    let path = format!("/workbooks/{doc}/tables/{id}/data");
    for headers in [&[][..], &[("range", "bytes=0-9")]] {
        let head = server.send(Method::HEAD, &path, me, headers, None).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(header_of(&head, "content-length"), length.to_string());
        assert_eq!(header_of(&head, "etag"), etag);
        assert!(head.bytes().await.unwrap().is_empty());
    }

    // The file is not the one that was checked.
    std::fs::write(server.file(doc, id, "rtbl"), &bytes[1..]).unwrap();
    assert_eq!(
        server.read(doc, id, me, &[]).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    std::fs::remove_file(server.file(doc, id, "rtbl")).unwrap();
    assert_eq!(
        server.read(doc, id, me, &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn the_list_of_tables() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let viewer = server.person("Viewer").await;
    let doc = server.workbook(&owner).await;
    server.join(doc, &owner, &viewer, "viewer").await;
    let me = by(&owner);
    let path = format!("/workbooks/{doc}/tables");
    let list = async |who: As<'_>, since: &str| {
        let (status, body) = server.get(&format!("{path}?since={since}"), who).await;
        assert_eq!(status, 200, "{body}");
        body
    };
    let names = |list: &Value| -> Vec<String> {
        list["tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|table| table["name"].as_str().unwrap().to_owned())
            .collect()
    };

    let empty = list(me, "").await;
    assert_eq!((&empty["changed"], names(&empty).len()), (&json!(true), 0));
    let mut version = empty["version"].as_str().unwrap().to_owned();
    let same = list(me, &version).await;
    assert_eq!(same["changed"], false);
    assert!(same.get("tables").is_none());

    // Every change makes another version.
    let mut ids = Vec::new();
    for seed in 0..3 {
        ids.push(server.upload(doc, me, &table_file(1_000, seed)).await);
    }
    let table = |id: Uuid| format!("{path}/{id}");
    let mut expect = async |order: &[&str]| {
        let now = list(me, &version).await;
        assert_eq!(now["changed"], true);
        assert_eq!(names(&now), order);
        version = now["version"].as_str().unwrap().to_owned();
        assert_eq!(list(me, &version).await["changed"], false);
    };
    expect(&["Orders", "Orders", "Orders"]).await;
    for (at, name) in ["A", "B", "C"].into_iter().enumerate() {
        let (status, record) = server
            .json(
                Method::PATCH,
                &table(ids[at]),
                me,
                json!({ "name": format!(" {name} ") }),
            )
            .await;
        assert_eq!((status, &record["name"]), (200, &json!(name)));
    }
    expect(&["A", "B", "C"]).await;
    for (id, position, order) in [
        (ids[2], 0, ["C", "A", "B"]),
        (ids[2], 1, ["A", "C", "B"]),
        (ids[0], 99, ["C", "B", "A"]),
    ] {
        let (status, _) = server
            .json(
                Method::PATCH,
                &table(id),
                me,
                json!({ "position": position }),
            )
            .await;
        assert_eq!(status, 200);
        expect(&order).await;
    }
    for bad in [
        json!({ "name": "" }),
        json!({ "name": "a\nb" }),
        json!({ "position": -1 }),
    ] {
        assert_eq!(
            server.json(Method::PATCH, &table(ids[0]), me, bad).await.0,
            422
        );
    }
    let (status, _) = server
        .json(
            Method::PATCH,
            &table(Uuid::new_v4()),
            me,
            json!({ "name": "D" }),
        )
        .await;
    assert_eq!(status, 404);

    // An upload is for those who may upload, and a deletion for everyone.
    let pending = Uuid::new_v4();
    let bytes = table_file(2_000, 9);
    server.declare(doc, pending, me, &bytes).await;
    server.patch(doc, pending, me, 0, &bytes[..500]).await;
    expect(&["C", "B", "A", "Orders"]).await;
    let mine = list(me, "").await;
    assert_eq!(mine["tables"][3]["status"], "uploading");
    assert_eq!(mine["tables"][3]["uploadOffset"], 500);
    assert_eq!(names(&list(by(&viewer), "").await), ["C", "B", "A"]);
    for id in [pending, ids[1]] {
        let response = server.send(Method::DELETE, &table(id), me, &[], None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let response = server.send(Method::DELETE, &table(id), me, &[], None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    expect(&["C", "A"]).await;

    // One whose file was lost is still listed, and cannot be read.
    sqlx::query("UPDATE tables SET status = 'missing' WHERE id = $1")
        .bind(ids[0])
        .execute(&server.pool)
        .await
        .unwrap();
    let lost = list(by(&viewer), "").await;
    assert_eq!(lost["tables"][1]["status"], "missing");
    assert_eq!(
        server.read(doc, ids[0], by(&viewer), &[]).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(server.complete(doc, ids[0], me).await.0, 409);

    // A restored database counts from where it was, and is another one.
    sqlx::query("UPDATE workbooks SET log_epoch = log_epoch + 1 WHERE id = $1")
        .bind(doc)
        .execute(&server.pool)
        .await
        .unwrap();
    expect(&["C", "A"]).await;
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn what_an_owner_and_everyone_may_store() {
    let db = IsolatedDb::create().await;
    let limits = StorageLimits {
        table_max_bytes: 40_000,
        table_user_quota_bytes: 100_000,
        table_total_bytes: 150_000,
        ..StorageLimits::default()
    };
    let server = TestServer::with(&db, limits, GovernanceConfig::default()).await;
    let owner = server.person("Owner").await;
    let editor = server.person("Editor").await;
    let doc = server.workbook(&owner).await;
    let second = server.workbook(&owner).await;
    server.join(doc, &owner, &editor, "editor").await;
    let file = table_file_of;

    // One table.
    let (status, body) = server
        .declare(doc, Uuid::new_v4(), by(&owner), &file(40_001, 1))
        .await;
    assert_eq!(
        (status, &body["error"]),
        (
            413,
            &json!("the table file is larger than this server accepts")
        )
    );

    // The owner's, whoever uploads them and to whichever workbook.
    let first = server.upload(doc, by(&editor), &file(40_000, 1)).await;
    server.upload(second, by(&owner), &file(40_000, 2)).await;
    assert_eq!(server.table_bytes(&owner).await, 80_000);
    assert_eq!(server.table_bytes(&editor).await, 0);
    let (status, body) = server
        .declare(doc, Uuid::new_v4(), by(&editor), &file(20_001, 3))
        .await;
    assert_eq!(
        (status, &body["error"]),
        (403, &json!("owner table storage quota exceeded"))
    );
    // An upload counts from the start.
    let pending = Uuid::new_v4();
    assert_eq!(
        server
            .declare(doc, pending, by(&editor), &file(20_000, 4))
            .await
            .0,
        201
    );
    assert_eq!(server.table_bytes(&owner).await, 100_000);
    let (_, account) = server.get("/account", by(&owner)).await;
    assert_eq!(account["tableBytes"], 100_000);
    assert_eq!(account["tableQuotaBytes"], 100_000);
    assert_eq!(server.store.disk_used_bytes(), 100_000);

    // One upload at a time, in all workbooks of an owner.
    for other in [doc, second] {
        let response = server
            .send(
                Method::POST,
                &format!("/workbooks/{other}/tables"),
                by(&owner),
                &[("content-type", "application/json")],
                Some(
                    declaration(Uuid::new_v4(), &file(5_000, 5))
                        .to_string()
                        .into_bytes(),
                ),
            )
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(header_of(&response, "retry-after"), "30");
    }
    // Giving one up, and deleting one, give the bytes back.
    for (id, left) in [(pending, 80_000), (first, 40_000)] {
        let response = server
            .send(
                Method::DELETE,
                &format!("/workbooks/{doc}/tables/{id}"),
                by(&editor),
                &[],
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(server.table_bytes(&owner).await, left);
    }
    assert!(!server.file(doc, first, "rtbl").exists());

    // Everyone's.
    let neighbour = server.person("Neighbour").await;
    let theirs = server.workbook(&neighbour).await;
    for seed in 0..2 {
        server
            .upload(theirs, by(&neighbour), &file(40_000, 10 + seed))
            .await;
    }
    let (status, body) = server
        .declare(doc, Uuid::new_v4(), by(&owner), &file(30_001, 6))
        .await;
    assert_eq!(
        (status, &body["error"]),
        (507, &json!("server storage capacity reached"))
    );
    assert_eq!(
        server
            .declare(doc, Uuid::new_v4(), by(&owner), &file(30_000, 6))
            .await
            .0,
        201
    );
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn tables_count_towards_the_hard_limit() {
    let db = IsolatedDb::create().await;
    let limits = StorageLimits {
        disk_soft_bytes: 60_000,
        disk_hard_bytes: 100_000,
        ..StorageLimits::default()
    };
    let server = TestServer::with(&db, limits, GovernanceConfig::default()).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);
    let used = server.store.disk_used_bytes() as usize;
    let table = server
        .upload(doc, me, &table_file_of(90_000 - used, 1))
        .await;
    assert!(server.store.disk_over_soft_limit());
    let (status, _) = server
        .declare(doc, Uuid::new_v4(), me, &table_file(20_000, 2))
        .await;
    assert_eq!(status, 507);
    server.upload(doc, me, &table_file_of(10_000, 3)).await;
    assert!(server.store.disk_over_hard_limit());
    // Nothing else fits either.
    let (status, _) = server
        .json(
            Method::POST,
            "/workbooks",
            me,
            json!({ "title": "One more" }),
        )
        .await;
    assert_eq!(status, 507);
    let import = server
        .send(
            Method::POST,
            "/workbooks/import",
            me,
            &[("x-workbook-title", "One%20more")],
            Some(vec![1; 100]),
        )
        .await;
    assert_eq!(import.status(), StatusCode::INSUFFICIENT_STORAGE);
    // What a restart counts is the same.
    let again = Store::connect_with_limits(&db.url, server.root.path().to_path_buf(), limits)
        .await
        .unwrap();
    assert_eq!(again.disk_used_bytes(), server.store.disk_used_bytes());
    let response = server
        .send(
            Method::DELETE,
            &format!("/workbooks/{doc}/tables/{table}"),
            me,
            &[],
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(server.store.disk_used_bytes() as usize, used + 10_000);
    server.workbook(&owner).await;
    drop(server);
    db.cleanup().await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn a_volume_that_is_nearly_full() {
    let db = IsolatedDb::create().await;
    let limits = StorageLimits {
        min_free_bytes: u64::MAX / 2,
        ..StorageLimits::default()
    };
    let server = TestServer::with(&db, limits, GovernanceConfig::default()).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let (status, _) = server
        .declare(doc, Uuid::new_v4(), by(&owner), &table_file(1_000, 1))
        .await;
    assert_eq!(status, 507);
    assert_eq!(server.table_bytes(&owner).await, 0);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn four_uploads_at_once() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let bytes = table_file(100_000, 1);
    let mut held = Vec::new();
    let mut people = Vec::new();
    for at in 0..5 {
        let owner = server.person(&format!("Owner {at}")).await;
        let doc = server.workbook(&owner).await;
        let id = Uuid::new_v4();
        assert_eq!(server.declare(doc, id, by(&owner), &bytes).await.0, 201);
        if at < 4 {
            held.push(
                server
                    .hold(doc, id, by(&owner), bytes.len(), &bytes[..10])
                    .await,
            );
        }
        people.push((owner, doc, id));
    }
    let (owner, doc, id) = &people[4];
    let response = server
        .send(
            Method::PATCH,
            &format!("/workbooks/{doc}/tables/{id}/data"),
            by(owner),
            &[("upload-offset", "0")],
            Some(bytes.clone()),
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header_of(&response, "retry-after"), "5");
    // Nothing of it was kept, and one that is done makes room.
    assert_eq!(
        std::fs::metadata(server.part(*doc, *id)).map_or(0, |found| found.len()),
        0
    );
    let mut first = held.remove(0);
    first.write_all(&bytes[10..]).await.unwrap();
    let mut reply = String::new();
    timeout(WAIT, first.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert_eq!(server.patch(*doc, *id, by(owner), 0, &bytes).await.0, 200);
    drop(held);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn bytes_per_minute() {
    let db = IsolatedDb::create().await;
    let gov = GovernanceConfig {
        table_download_bpm: 300_000,
        table_upload_bpm: 300_000,
        table_anon_rpm: 4,
        anon_rpm: 2,
        ..GovernanceConfig::default()
    };
    let server = TestServer::with(&db, StorageLimits::default(), gov).await;
    let owner = server.person("Owner").await;
    let viewer = server.person("Viewer").await;
    let doc = server.workbook(&owner).await;
    server.join(doc, &owner, &viewer, "viewer").await;
    let (link, _) = server.open_link(doc, &owner).await;
    let bytes = table_file(400_000, 1);
    let table = server.upload(doc, by(&owner), &bytes).await;

    // The upload used its own allowance up.
    let next = Uuid::new_v4();
    assert_eq!(server.declare(doc, next, by(&owner), &bytes).await.0, 201);
    let response = server
        .send(
            Method::PATCH,
            &format!("/workbooks/{doc}/tables/{next}/data"),
            by(&owner),
            &[("upload-offset", "0")],
            Some(bytes.clone()),
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let seconds: u64 = header_of(&response, "retry-after").parse().unwrap();
    assert!((10..=60).contains(&seconds), "{seconds}");
    assert_eq!(answer(response).await.1["error"], "rate limit exceeded");

    // A download is let through and the next one waits; others do not.
    for person in [&owner, &viewer] {
        assert_eq!(
            server.read(doc, table, by(person), &[]).await.status(),
            StatusCode::OK
        );
        let response = server
            .read(doc, table, by(person), &[("range", "bytes=0-0")])
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!header_of(&response, "retry-after").is_empty());
        // The list and everything else still answer.
        assert_eq!(
            server
                .get(&format!("/workbooks/{doc}/tables"), by(person))
                .await
                .0,
            200
        );
        assert_eq!(
            server.get(&format!("/workbooks/{doc}"), by(person)).await.0,
            200
        );
    }

    // Anonymous requests: those for tables and the others apart.
    let anonymous = As {
        cookie: None,
        share: Some(&link),
    };
    let list = format!("/workbooks/{doc}/tables");
    let book = format!("/workbooks/{doc}");
    let mut statuses = Vec::new();
    for path in [
        &book, &book, &book, &list, &list, &list, &list, &list, &book,
    ] {
        statuses.push(server.get(path, anonymous).await.0);
    }
    assert_eq!(statuses, [200, 200, 429, 200, 200, 200, 200, 429, 429]);
    // A cookie that is no session does not get around it.
    let pretender = As {
        cookie: Some("reticula_session=made-up"),
        share: Some(&link),
    };
    assert_eq!(server.get(&list, pretender).await.0, 429);
    assert_eq!(server.get(&book, pretender).await.0, 429);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn uploads_that_were_abandoned() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let me = by(&owner);
    let bytes = table_file(10_000, 1);
    let ready = server.upload(doc, me, &bytes).await;
    let id = Uuid::new_v4();
    server.declare(doc, id, me, &bytes).await;
    server.patch(doc, id, me, 0, &bytes[..100]).await;
    let two_hours_ago = SystemTime::now() - Duration::from_secs(7_200);
    let age = async |id: Uuid| {
        sqlx::query("UPDATE tables SET created_at = now() - interval '2 hours' WHERE id = $1")
            .bind(id)
            .execute(&server.pool)
            .await
            .unwrap();
    };
    let age_file = |path: PathBuf| {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
    };

    // New, or old with something that arrived lately.
    assert_eq!(
        server
            .store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap(),
        0
    );
    age_file(server.part(doc, id));
    assert_eq!(
        server
            .store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap(),
        0
    );
    age(id).await;
    age(ready).await;
    age_file(server.file(doc, ready, "rtbl"));
    server.patch(doc, id, me, 100, &bytes[100..200]).await;
    assert_eq!(
        server
            .store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap(),
        0
    );

    // Old, and being added to at this moment.
    let held = Uuid::new_v4();
    let other_owner = server.person("Other owner").await;
    let other = server.workbook(&other_owner).await;
    server.declare(other, held, by(&other_owner), &bytes).await;
    let stream = server
        .hold(other, held, by(&other_owner), bytes.len(), &bytes[..10])
        .await;
    age(held).await;
    age_file(server.part(other, held));
    assert_eq!(
        server
            .store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap(),
        0
    );
    drop(stream);
    // Until the server notices that nothing more is coming.
    let deadline = Instant::now() + WAIT;
    while server
        .store
        .drop_idle_tables(TABLE_UPLOAD_IDLE)
        .await
        .unwrap()
        == 0
    {
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.status_in_database(other, held).await, None);
    assert_eq!(server.table_bytes(&other_owner).await, 0);

    // Idle: the row, the file and the reservation go, and nothing else.
    age_file(server.part(doc, id));
    let before = server.get(&format!("/workbooks/{doc}/tables"), me).await.1;
    assert_eq!(
        server
            .store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap(),
        1
    );
    assert_eq!(server.status_in_database(doc, id).await, None);
    assert!(!server.part(doc, id).exists());
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);
    let after = server.get(&format!("/workbooks/{doc}/tables"), me).await.1;
    assert_ne!(before["version"], after["version"]);
    assert_eq!(
        server.read(doc, ready, me, &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(server.patch(doc, id, me, 200, &bytes[200..]).await.0, 404);

    // One that never got a byte.
    let never = Uuid::new_v4();
    assert_eq!(server.declare(doc, never, me, &bytes).await.0, 201);
    age(never).await;
    assert!(
        server
            .store
            .drop_idle_tables(TABLE_UPLOAD_IDLE)
            .await
            .unwrap()
            >= 1
    );
    assert_eq!(server.status_in_database(doc, never).await, None);
    assert_eq!(server.table_bytes(&owner).await, bytes.len() as i64);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn files_without_a_row_go_at_the_start() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let root = server.root.path().to_path_buf();
    let reconnect = async || Store::connect(&db.url, root.clone()).await.unwrap();
    let write = |path: PathBuf| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();
        path
    };
    let gone = Uuid::new_v4();

    // Not when it is all there is.
    let orphan = write(table_path(&root, gone, Uuid::new_v4(), "rtbl"));
    reconnect().await;
    assert!(orphan.exists());

    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let bytes = table_file(1_000, 1);
    let ready = server.upload(doc, by(&owner), &bytes).await;
    for seed in 2..5 {
        server
            .upload(doc, by(&owner), &table_file(1_000, seed))
            .await;
    }
    let pending = Uuid::new_v4();
    server.declare(doc, pending, by(&owner), &bytes).await;
    server
        .patch(doc, pending, by(&owner), 0, &bytes[..10])
        .await;
    let orphans = [
        orphan,
        write(table_path(&root, doc, Uuid::new_v4(), "part")),
        write(table_path(&root, doc, Uuid::new_v4(), "rtbl")),
        // A table of that ID is in another workbook.
        write(table_path(&root, gone, ready, "rtbl")),
    ];
    // What the server did not name is not its to remove.
    let kept = [
        write(root.join("tables/notes.txt")),
        write(root.join(format!("tables/{doc}/notes.txt"))),
        write(root.join(format!("tables/{doc}/{}.bak", Uuid::new_v4()))),
        server.file(doc, ready, "rtbl"),
        server.part(doc, pending),
    ];
    let unfinished = write(root.join(format!("tables/{doc}/x.tmp")));
    reconnect().await;
    for path in orphans {
        assert!(!path.exists(), "{}", path.display());
    }
    for path in kept {
        assert!(path.exists(), "{}", path.display());
    }
    assert!(!unfinished.exists());
    assert!(!root.join(format!("tables/{gone}")).exists());
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn deleting_a_workbook_and_an_account() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let guest = server.person("Guest").await;
    let first = server.workbook(&owner).await;
    let second = server.workbook(&owner).await;
    server.join(first, &owner, &guest, "editor").await;
    let guests_own = server.workbook(&guest).await;
    let bytes = table_file(50_000, 1);
    let size = bytes.len() as i64;
    server.upload(first, by(&owner), &bytes).await;
    server.upload(second, by(&owner), &bytes).await;
    let by_guest = server
        .upload(
            second_for(&server, &owner, &guest, second).await,
            by(&guest),
            &bytes,
        )
        .await;
    server.upload(guests_own, by(&guest), &bytes).await;
    assert_eq!(server.table_bytes(&owner).await, 3 * size);
    assert_eq!(server.table_bytes(&guest).await, size);
    let used = server.store.disk_used_bytes();

    // A workbook, while something is being uploaded to it.
    let pending = Uuid::new_v4();
    server.declare(first, pending, by(&owner), &bytes).await;
    let mut held = server
        .hold(first, pending, by(&owner), bytes.len(), &bytes[..1_000])
        .await;
    let started = Instant::now();
    let deleted = server
        .send(
            Method::DELETE,
            &format!("/workbooks/{first}"),
            by(&owner),
            &[],
            None,
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(server.table_bytes(&owner).await, 2 * size);
    assert_eq!(server.store.disk_used_bytes(), used - size as u64);
    held.write_all(&bytes[1_000..]).await.unwrap();
    let mut reply = String::new();
    timeout(WAIT, held.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    let directory = |doc: Uuid| server.root.path().join(format!("tables/{doc}"));
    assert!(!directory(first).exists());
    assert!(directory(second).exists());

    // An account: its workbooks go, and what it uploaded to others stays.
    let deleted = server
        .json(
            Method::DELETE,
            "/account",
            by(&guest),
            json!({ "confirm": "DELETE" }),
        )
        .await;
    assert_eq!(deleted.0, 204);
    assert!(!directory(guests_own).exists());
    let author: Uuid = sqlx::query_scalar("SELECT created_by FROM tables WHERE id = $1")
        .bind(by_guest)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(author, DELETED_USER_ID);
    assert_eq!(
        server
            .read(second, by_guest, by(&owner), &[])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(server.table_bytes(&owner).await, 2 * size);
    assert_eq!(server.store.disk_used_bytes(), used - 2 * size as u64);

    let deleted = server
        .json(
            Method::DELETE,
            "/account",
            by(&owner),
            json!({ "confirm": "DELETE" }),
        )
        .await;
    assert_eq!(deleted.0, 204);
    assert!(!directory(second).exists());
    assert_eq!(server.store.disk_used_bytes(), used - 4 * size as u64);
    drop(server);
    db.cleanup().await;
}

async fn second_for(server: &TestServer, owner: &Person, guest: &Person, doc: Uuid) -> Uuid {
    server.join(doc, owner, guest, "editor").await;
    doc
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn an_upload_goes_on_after_the_server_was_killed() {
    let db = IsolatedDb::create().await;
    let data_dir = tempfile::tempdir().unwrap();
    let static_dir = tempfile::tempdir().unwrap();
    std::fs::write(static_dir.path().join("index.html"), "<!doctype html>").unwrap();
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let base = format!("http://{addr}");
    let spawn_server = || {
        Command::new(env!("CARGO_BIN_EXE_reticula-server"))
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
            .unwrap()
    };
    let client = client();
    let wait_ready = async || {
        for _ in 0..80 {
            if let Ok(response) = client.get(format!("{base}/readyz")).send().await
                && response.status() == StatusCode::OK
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
        panic!("server did not become ready at {base}");
    };
    let mut server = spawn_server();
    wait_ready().await;
    let login = client
        .post(format!("{base}/api/auth/dev-login"))
        .header(header::ORIGIN, &base)
        .json(&json!({ "account": "alice" }))
        .send()
        .await
        .unwrap();
    let cookie = header_of(&login, "set-cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let send = async |method: Method, path: &str, offset: Option<usize>, body: Vec<u8>| {
        let mut request = client
            .request(method, format!("{base}/api{path}"))
            .header(header::ORIGIN, &base)
            .header(header::COOKIE, &cookie);
        request = match offset {
            Some(offset) => request.header("upload-offset", offset),
            None => request.header(header::CONTENT_TYPE, "application/json"),
        };
        answer(request.body(body).send().await.unwrap()).await
    };
    let json_bytes = |value: Value| value.to_string().into_bytes();
    let (_, book) = send(
        Method::POST,
        "/workbooks",
        None,
        json_bytes(json!({ "title": "Killed" })),
    )
    .await;
    let doc: Uuid = book["id"].as_str().unwrap().parse().unwrap();
    let bytes = table_file(5 * MIB, 1);
    let id = Uuid::new_v4();
    let tables = format!("/workbooks/{doc}/tables");
    let declared = send(
        Method::POST,
        &tables,
        None,
        json_bytes(declaration(id, &bytes)),
    )
    .await;
    assert_eq!(declared.0, 201);
    let data = format!("{tables}/{id}/data");
    let first = send(Method::PATCH, &data, Some(0), bytes[..MIB].to_vec()).await;
    assert_eq!(first.0, 200);

    // Killed in the middle of the second piece.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!(
                "PATCH /api{data} HTTP/1.1\r\nHost: x\r\nOrigin: {base}\r\nCookie: {cookie}\r\n\
                 Upload-Offset: {MIB}\r\nContent-Length: {}\r\n\r\n",
                4 * MIB
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(&bytes[MIB..3 * MIB]).await.unwrap();
    let part = table_path(data_dir.path(), doc, id, "part");
    eventually("some of the second piece", || {
        std::fs::metadata(&part).unwrap().len() > MIB as u64
    })
    .await;
    server.kill().await.unwrap();

    let mut server = spawn_server();
    wait_ready().await;
    // Where it is, as the list, a declaration and a wrong offset all say.
    let length = std::fs::metadata(&part).unwrap().len() as usize;
    assert!((MIB + 1..=3 * MIB).contains(&length), "{length}");
    let (status, list) = send(Method::GET, &tables, None, Vec::new()).await;
    assert_eq!(
        (status, &list["tables"][0]["uploadOffset"]),
        (200, &json!(length))
    );
    let (status, record) = send(
        Method::POST,
        &tables,
        None,
        json_bytes(declaration(id, &bytes)),
    )
    .await;
    assert_eq!((status, &record["uploadOffset"]), (200, &json!(length)));
    let (status, body) = send(Method::PATCH, &data, Some(3 * MIB + 1), vec![0]).await;
    assert_eq!((status, &body["uploadOffset"]), (409, &json!(length)));
    let (status, _) = send(Method::PATCH, &data, Some(length), bytes[length..].to_vec()).await;
    assert_eq!(status, 200);
    let (status, record) = send(
        Method::POST,
        &format!("{tables}/{id}/complete"),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(
        (status, &record["status"]),
        (200, &json!("ready")),
        "{record}"
    );
    let read = client
        .get(format!("{base}/api{data}"))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(read.bytes().await.unwrap().as_ref(), bytes);
    server.kill().await.unwrap();
    db.cleanup().await;
}

/// The periodic task, stopped at a workbook someone else is writing.
async fn while_idle_tables_are_dropped(
    server: &TestServer,
    meanwhile: impl AsyncFnOnce(&Person, Uuid),
) {
    let (first, second) = (server.person("First").await, server.person("Second").await);
    let busy = server.workbook(&first).await;
    let bytes = table_file(10_000, 1);
    let abandoned = Uuid::new_v4();
    server.declare(busy, abandoned, by(&first), &bytes).await;
    server.age(abandoned).await;
    let doc = server.workbook(&second).await;
    let mut held = server.pool.begin().await.unwrap();
    // Declared before the tables of the second, so the task comes to it first.
    let id = Uuid::new_v4();
    server.declare(doc, id, by(&second), &bytes).await;
    server.patch(doc, id, by(&second), 0, &bytes).await;
    server.age(id).await;
    sqlx::query("SELECT id FROM workbooks WHERE id = $1 FOR UPDATE")
        .bind(busy)
        .execute(&mut *held)
        .await
        .unwrap();
    let store = server.store.clone();
    let dropping = tokio::spawn(async move { store.drop_idle_tables(TABLE_UPLOAD_IDLE).await });
    sleep(Duration::from_millis(300)).await;
    meanwhile(&second, doc).await;
    held.commit().await.unwrap();
    dropping.await.unwrap().unwrap();
    assert_eq!(server.status_in_database(busy, abandoned).await, None);
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn a_table_completed_while_idle_ones_are_dropped() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let table = std::cell::Cell::new(None);
    while_idle_tables_are_dropped(&server, async |owner, doc| {
        let id: Uuid = sqlx::query_scalar("SELECT id FROM tables WHERE workbook_id = $1")
            .bind(doc)
            .fetch_one(&server.pool)
            .await
            .unwrap();
        assert_eq!(server.complete(doc, id, by(owner)).await.0, 200);
        table.set(Some((doc, id, owner.cookie.clone())));
    })
    .await;
    let (doc, id, cookie) = table.take().unwrap();
    let who = As {
        cookie: Some(&cookie),
        share: None,
    };
    assert_eq!(
        server.read(doc, id, who, &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(server.status_in_database(doc, id).await.unwrap(), "ready");
    // Nor when it comes by again.
    server.drop_idle().await;
    assert_eq!(
        server.read(doc, id, who, &[]).await.status(),
        StatusCode::OK
    );
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn a_table_declared_again_while_idle_ones_are_dropped() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let table = std::cell::Cell::new(None);
    let bytes = table_file(10_000, 1);
    while_idle_tables_are_dropped(&server, async |owner, doc| {
        let id: Uuid = sqlx::query_scalar("SELECT id FROM tables WHERE workbook_id = $1")
            .bind(doc)
            .fetch_one(&server.pool)
            .await
            .unwrap();
        let path = format!("/workbooks/{doc}/tables/{id}");
        let deleted = server
            .send(Method::DELETE, &path, by(owner), &[], None)
            .await;
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert_eq!(server.declare(doc, id, by(owner), &bytes).await.0, 201);
        table.set(Some((doc, id)));
    })
    .await;
    let (doc, id) = table.take().unwrap();
    assert_eq!(
        server.status_in_database(doc, id).await.unwrap(),
        "uploading"
    );
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn uploads_under_a_clock_that_was_set_back() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let id = Uuid::new_v4();
    let bytes = table_file(10_000, 1);
    server.declare(doc, id, by(&owner), &bytes).await;
    server.patch(doc, id, by(&owner), 0, &bytes[..5_000]).await;
    server.age(id).await;
    let arrived = |later: Duration| {
        std::fs::File::options()
            .write(true)
            .open(server.part(doc, id))
            .unwrap()
            .set_modified(SystemTime::now() + later)
            .unwrap();
    };
    arrived(Duration::from_secs(30));
    assert_eq!(server.drop_idle().await, 0);
    // So far ahead that it says nothing.
    arrived(Duration::from_secs(365 * 24 * 60 * 60));
    assert_eq!(server.drop_idle().await, 1);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn an_upload_that_cannot_be_looked_at_does_not_keep_the_others() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let (first, second) = (server.person("First").await, server.person("Second").await);
    let bytes = table_file(10_000, 1);
    let mut tables = Vec::new();
    for owner in [&first, &second] {
        let doc = server.workbook(owner).await;
        let id = Uuid::new_v4();
        server.declare(doc, id, by(owner), &bytes).await;
        server.age(id).await;
        tables.push((doc, id));
    }
    // A file where the directory of its tables goes.
    let tables_dir = server.root.path().join("tables");
    std::fs::create_dir_all(&tables_dir).unwrap();
    let _ = std::fs::remove_dir(tables_dir.join(tables[0].0.to_string()));
    std::fs::write(tables_dir.join(tables[0].0.to_string()), b"x").unwrap();
    server.drop_idle().await;
    assert_eq!(
        server.status_in_database(tables[1].0, tables[1].1).await,
        None
    );
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn a_viewer_who_becomes_an_editor_sees_the_uploads() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let guest = server.person("Guest").await;
    let doc = server.workbook(&owner).await;
    server.join(doc, &owner, &guest, "viewer").await;
    server
        .declare(doc, Uuid::new_v4(), by(&owner), &table_file(10_000, 1))
        .await;
    let path = format!("/workbooks/{doc}/tables");
    let (_, before) = server.get(&path, by(&guest)).await;
    assert_eq!(before["tables"].as_array().unwrap().len(), 0);
    server.join(doc, &owner, &guest, "editor").await;
    let since = before["version"].as_str().unwrap();
    let (_, after) = server
        .get(&format!("{path}?since={since}"), by(&guest))
        .await;
    assert_eq!(after["changed"], true);
    assert_eq!(after["tables"].as_array().unwrap().len(), 1);
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn an_upload_that_starts_while_the_last_table_goes() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let me = by(&owner);
    let bytes = table_file(6_000, 1);
    let doc = server.workbook(&owner).await;
    for _ in 0..60 {
        let old = server.upload(doc, me, &bytes).await;
        let id = Uuid::new_v4();
        assert_eq!(server.declare(doc, id, me, &bytes).await.0, 201);
        let old = format!("/workbooks/{doc}/tables/{old}");
        let (deleted, (status, body)) = tokio::join!(
            server.send(Method::DELETE, &old, me, &[], None),
            server.patch(doc, id, me, 0, &bytes[..100]),
        );
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert_eq!(status, 200, "{body}");
        let path = format!("/workbooks/{doc}/tables/{id}");
        server.send(Method::DELETE, &path, me, &[], None).await;
    }
    drop(server);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn deletions_go_on_without_the_client() {
    let db = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let bytes = table_file(20_000, 1);
    for polls in 1..40 {
        for account in [false, true] {
            let owner = server.person("Owner").await;
            let doc = server.workbook(&owner).await;
            let id = server.upload(doc, by(&owner), &bytes).await;
            let (path, body) = if account {
                ("/account".to_owned(), r#"{"confirm":"DELETE"}"#)
            } else {
                (format!("/workbooks/{doc}"), "{}")
            };
            server
                .cut_short(Method::DELETE, &path, &owner, body, polls)
                .await;
            // Either nothing happened, or all of it does.
            let file = server.file(doc, id, "rtbl");
            let deadline = Instant::now() + WAIT;
            loop {
                let row: Option<Uuid> =
                    sqlx::query_scalar("SELECT id FROM workbooks WHERE id = $1")
                        .bind(doc)
                        .fetch_optional(&server.pool)
                        .await
                        .unwrap();
                if row.is_some() == file.exists() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cut short after {polls} polls, account {account}: the file stayed"
                );
                sleep(Duration::from_millis(20)).await;
            }
        }
    }
    drop(server);
    db.cleanup().await;
}

/// A volume under another database, or under one from long ago.
#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn the_files_of_another_database() {
    let db = IsolatedDb::create().await;
    let other = IsolatedDb::create().await;
    let server = TestServer::start(&db).await;
    let owner = server.person("Owner").await;
    let doc = server.workbook(&owner).await;
    let mut tables = Vec::new();
    for seed in 0..3 {
        tables.push(
            server
                .upload(doc, by(&owner), &table_file(10_000, seed))
                .await,
        );
    }
    let elsewhere = TestServer::start(&other).await;
    let stranger = elsewhere.person("Stranger").await;
    elsewhere.workbook(&stranger).await;
    Store::connect(&other.url, server.root.path().to_path_buf())
        .await
        .unwrap();
    for id in &tables {
        assert!(server.file(doc, *id, "rtbl").exists());
    }

    // Some of them are in no row, and come back with their rows.
    sqlx::query("CREATE TABLE put_aside AS SELECT * FROM tables WHERE id = $1")
        .bind(tables[0])
        .execute(&server.pool)
        .await
        .unwrap();
    let id = tables[0];
    sqlx::query("DELETE FROM tables WHERE id = $1")
        .bind(id)
        .execute(&server.pool)
        .await
        .unwrap();
    let reconnect = async || {
        Store::connect(&db.url, server.root.path().to_path_buf())
            .await
            .unwrap()
    };
    reconnect().await;
    assert!(!server.file(doc, id, "rtbl").exists());
    assert!(server.file(doc, tables[1], "rtbl").exists());
    sqlx::query("INSERT INTO tables SELECT * FROM put_aside")
        .execute(&server.pool)
        .await
        .unwrap();
    reconnect().await;
    let read = server.read(doc, id, by(&owner), &[]).await;
    assert_eq!(read.bytes().await.unwrap().as_ref(), table_file(10_000, 0));

    // Those nobody came back for go for good.
    sqlx::query("DELETE FROM tables WHERE id = $1")
        .bind(id)
        .execute(&server.pool)
        .await
        .unwrap();
    let store = reconnect().await;
    let removed = server.root.path().join("tables-removed");
    let kept = || std::fs::read_dir(&removed).map_or(0, Iterator::count);
    assert_eq!(kept(), 1);
    store
        .purge_removed_tables(Duration::from_secs(3_600))
        .await
        .unwrap();
    assert_eq!(kept(), 1);
    store.purge_removed_tables(Duration::ZERO).await.unwrap();
    assert_eq!(kept(), 0);
    drop(elsewhere);
    drop(server);
    other.cleanup().await;
    db.cleanup().await;
}
