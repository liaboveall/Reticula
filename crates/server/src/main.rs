//! Reticula HTTP server:
//! - `GET /healthz` -> `200 OK`;
//! - `GET /readyz` checks configured collaboration storage;
//! - serves precompressed `.br` and `.gz` static assets from `RETICULA_STATIC_DIR`
//!   (default `apps/web/dist`);
//! - sets `Cache-Control: public, max-age=31536000, immutable` on `/assets/*` and
//!   `Cache-Control: no-cache` on HTML / SPA routes (`/`, `/app`);
//! - answers 404 for a missing `/assets/*`, which is never an SPA route;
//! - ensures `.wasm` responses carry `Content-Type: application/wasm` for
//!   streaming WebAssembly compilation.

use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, REFERRER_POLICY};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use reticula_server::http::{self, ApiState, AuthConfig};
use reticula_server::ops::GovernanceConfig;
use reticula_server::proxies::{TrustedProxies, TrustedProxiesError};
use reticula_server::store::{REMOVED_TABLES_KEPT, StorageLimits, Store, TABLE_UPLOAD_IDLE};
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use tracing_subscriber::EnvFilter;

const IMMUTABLE_CACHE: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");
const NO_CACHE: HeaderValue = HeaderValue::from_static("no-cache");
const WASM_MIME: HeaderValue = HeaderValue::from_static("application/wasm");
const COOP_HEADER: HeaderName = HeaderName::from_static("cross-origin-opener-policy");
const COEP_HEADER: HeaderName = HeaderName::from_static("cross-origin-embedder-policy");
const CORP_HEADER: HeaderName = HeaderName::from_static("cross-origin-resource-policy");
const SAME_ORIGIN: HeaderValue = HeaderValue::from_static("same-origin");
const REQUIRE_CORP: HeaderValue = HeaderValue::from_static("require-corp");

/// Builds the Axum router serving precompressed assets and health endpoints
/// with Cross-Origin Isolation headers disabled (default).
pub fn build_app(static_dir: &Path, api: Option<ApiState>) -> Router {
    build_app_with_coi(static_dir, api, false)
}

/// Builds the Axum router with optional Cross-Origin Isolation headers
/// (`COOP`/`COEP`/`CORP`), controlled via `RETICULA_COI` (`false` by default).
pub fn build_app_with_coi(static_dir: &Path, api: Option<ApiState>, coi_enabled: bool) -> Router {
    let index_fallback = ServeFile::new(static_dir.join("index.html"))
        .precompressed_br()
        .precompressed_gzip();
    let static_service = ServeDir::new(static_dir)
        .precompressed_br()
        .precompressed_gzip()
        .fallback(index_fallback);
    // The names under `/assets/` are hashed. One that is not there belongs to
    // another build, and the document in its place would be taken for it.
    let asset_service = ServeDir::new(static_dir.join("assets"))
        .append_index_html_on_directories(false)
        .precompressed_br()
        .precompressed_gzip();

    let store = api.as_ref().map(|api| api.store().clone());
    let metrics_router = http::metrics_route(api.clone());
    let routes = match api {
        Some(api) => http::router(api),
        None => Router::new().fallback(|| async { StatusCode::SERVICE_UNAVAILABLE }),
    };
    let router = Router::new()
        .route("/healthz", get(healthz))
        .merge(metrics_router)
        .nest("/api", routes)
        .route(
            "/readyz",
            get(move || async move {
                match store {
                    Some(store) if store.ready().await.is_ok() => (StatusCode::OK, "ok\n"),
                    _ => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "collaboration storage unavailable\n",
                    ),
                }
            }),
        )
        .nest_service("/assets", asset_service)
        .fallback_service(static_service)
        .layer(middleware::from_fn(apply_asset_headers));
    if coi_enabled {
        router.layer(middleware::from_fn(apply_coi_headers))
    } else {
        router
    }
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn apply_coi_headers(req: Request, next: Next) -> Response {
    let uri_path = req.uri().path().to_owned();
    let mut response = next.run(req).await;
    if uri_path == "/metrics"
        || uri_path == "/healthz"
        || uri_path == "/readyz"
        || uri_path == "/api"
        || uri_path.starts_with("/api/")
    {
        return response;
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let ext = Path::new(&uri_path).extension();
    let is_html = mime == "text/html" || (!uri_path.starts_with("/assets/") && ext.is_none());
    let is_js = mime.contains("javascript")
        || ext.is_some_and(|e| e.eq_ignore_ascii_case("js") || e.eq_ignore_ascii_case("mjs"));

    let headers = response.headers_mut();
    headers.insert(CORP_HEADER, SAME_ORIGIN);
    if is_html || is_js {
        headers.insert(COOP_HEADER, SAME_ORIGIN);
        headers.insert(COEP_HEADER, REQUIRE_CORP);
    }
    response
}

async fn apply_asset_headers(req: Request, next: Next) -> Response {
    let uri_path = req.uri().path().to_owned();
    let mut response = next.run(req).await;
    if uri_path == "/metrics" {
        return response;
    }
    if uri_path == "/healthz" {
        response.headers_mut().insert(CACHE_CONTROL, NO_CACHE);
        return response;
    }

    let status = response.status();
    if uri_path.starts_with("/assets/")
        && (status.is_success() || status == StatusCode::NOT_MODIFIED)
    {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, IMMUTABLE_CACHE);
        if Path::new(&uri_path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wasm"))
        {
            response.headers_mut().insert(CONTENT_TYPE, WASM_MIME);
        }
    } else {
        response
            .headers_mut()
            .entry(CACHE_CONTROL)
            .or_insert(NO_CACHE);
        if uri_path.starts_with("/join/") || uri_path == "/app" {
            response
                .headers_mut()
                .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        }
    }
    response
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Drops table uploads that were abandoned, and files that were in no row for
/// long, now and from time to time.
async fn tidy_tables(store: Store) {
    let mut ticks = tokio::time::interval(Duration::from_secs(300));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        if let Err(error) = store.drop_idle_tables(TABLE_UPLOAD_IDLE).await {
            tracing::error!(error = %error, "could not drop idle table uploads");
        }
        if let Err(error) = store.purge_removed_tables(REMOVED_TABLES_KEPT).await {
            tracing::error!(error = %error, "could not remove files of tables that were put aside");
        }
    }
}

async fn wait_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> T {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("invalid value in {name}: `{raw}`")),
        _ => default,
    }
}

fn storage_limits_from_env() -> StorageLimits {
    let defaults = StorageLimits::default();
    StorageLimits {
        max_doc_cells: parse_env("RETICULA_MAX_DOC_CELLS", defaults.max_doc_cells),
        max_doc_bytes: parse_env("RETICULA_MAX_DOC_BYTES", defaults.max_doc_bytes),
        max_import_bytes: parse_env("RETICULA_MAX_IMPORT_BYTES", defaults.max_import_bytes),
        user_quota_bytes: parse_env("RETICULA_USER_QUOTA_BYTES", defaults.user_quota_bytes),
        disk_soft_bytes: parse_env("RETICULA_DISK_SOFT_BYTES", defaults.disk_soft_bytes),
        disk_hard_bytes: parse_env("RETICULA_DISK_HARD_BYTES", defaults.disk_hard_bytes),
        table_max_bytes: parse_env("RETICULA_TABLE_MAX_BYTES", defaults.table_max_bytes),
        table_user_quota_bytes: parse_env(
            "RETICULA_TABLE_USER_QUOTA_BYTES",
            defaults.table_user_quota_bytes,
        ),
        table_total_bytes: parse_env("RETICULA_TABLE_TOTAL_BYTES", defaults.table_total_bytes),
        min_free_bytes: parse_env("RETICULA_MIN_FREE_BYTES", defaults.min_free_bytes),
    }
}

fn trusted_proxies_from_env() -> Result<TrustedProxies, TrustedProxiesError> {
    match env::var("RETICULA_TRUSTED_PROXIES") {
        Ok(raw) if !raw.trim().is_empty() => raw.parse(),
        _ => Ok(TrustedProxies::default()),
    }
}

fn governance_from_env(trusted_proxies: TrustedProxies) -> GovernanceConfig {
    let defaults = GovernanceConfig::default();
    GovernanceConfig {
        max_loaded_bytes: parse_env("RETICULA_MAX_LOADED_BYTES", defaults.max_loaded_bytes),
        write_rpm: parse_env("RETICULA_WRITE_RPM", defaults.write_rpm),
        anon_rpm: parse_env("RETICULA_ANON_RPM", defaults.anon_rpm),
        max_anon_ws_per_doc: parse_env(
            "RETICULA_MAX_ANON_WS_PER_DOC",
            defaults.max_anon_ws_per_doc,
        ),
        max_anon_ws_per_ip: parse_env("RETICULA_MAX_ANON_WS_PER_IP", defaults.max_anon_ws_per_ip),
        table_upload_bpm: parse_env(
            "RETICULA_TABLE_UPLOAD_BYTES_PER_MINUTE",
            defaults.table_upload_bpm,
        ),
        table_download_bpm: parse_env(
            "RETICULA_TABLE_DOWNLOAD_BYTES_PER_MINUTE",
            defaults.table_download_bpm,
        ),
        table_anon_rpm: parse_env("RETICULA_TABLE_ANON_RPM", defaults.table_anon_rpm),
        trusted_proxies,
    }
}

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    let bind_raw = env::var("BIND_ADDR").unwrap_or_else(|_| String::from("127.0.0.1:3200"));
    let bind_addr: SocketAddr = bind_raw
        .parse()
        .unwrap_or_else(|e| panic!("invalid BIND_ADDR `{bind_raw}`: {e}"));
    let static_dir = PathBuf::from(
        env::var("RETICULA_STATIC_DIR").unwrap_or_else(|_| String::from("apps/web/dist")),
    );

    let trusted_proxies = trusted_proxies_from_env().unwrap_or_else(|error| {
        tracing::error!(error = %error, "invalid RETICULA_TRUSTED_PROXIES");
        std::process::exit(1);
    });

    let api = match env::var("RETICULA_DATABASE_URL") {
        Ok(database_url) => {
            let data_dir = env::var_os("RETICULA_DATA_DIR")
                .expect("RETICULA_DATA_DIR is required when collaboration storage is enabled");
            let auth = AuthConfig::from_env(bind_addr.ip()).unwrap_or_else(|error| {
                tracing::error!(error = %error, "could not configure collaboration authentication");
                std::process::exit(1);
            });
            let limits = storage_limits_from_env();
            let gov = governance_from_env(trusted_proxies);
            match Store::connect_with_limits(&database_url, PathBuf::from(data_dir), limits).await {
                Ok(store) => {
                    let api_state = ApiState::with_governance(store, auth, gov);
                    if let Err(error) = api_state.agent().recover_interrupted_runs().await {
                        tracing::error!(error = %error, "failed to recover interrupted AI agent runs");
                    }
                    if let Ok(cleaned) = api_state.store().cleanup_expired_branch_snapshots(7).await
                        && cleaned > 0
                    {
                        tracing::info!(
                            cleaned,
                            "cleaned expired branch snapshots older than 7 days"
                        );
                    }
                    tokio::spawn(tidy_tables(api_state.store().clone()));
                    Some(api_state)
                }
                Err(error) => {
                    tracing::error!(error = %error, "could not initialize collaboration storage");
                    std::process::exit(1);
                }
            }
        }
        Err(env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid RETICULA_DATABASE_URL: {error}"),
    };
    let metrics_bind = env::var("RETICULA_METRICS_ADDR")
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            raw.parse::<SocketAddr>()
                .unwrap_or_else(|e| panic!("invalid RETICULA_METRICS_ADDR `{raw}`: {e}"))
        });
    let metrics_server = if let Some(metrics_addr) = metrics_bind {
        let listener = TcpListener::bind(metrics_addr)
            .await
            .unwrap_or_else(|e| panic!("failed to bind metrics {metrics_addr}: {e}"));
        let local_metrics = listener
            .local_addr()
            .expect("bound metrics listener has local address");
        tracing::info!(addr = %local_metrics, "reticula metrics listener ready");
        Some((listener, http::internal_metrics_route(api.clone())))
    } else {
        None
    };

    let coi_enabled = env::var("RETICULA_COI").is_ok_and(|raw| {
        let trimmed = raw.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    });
    let app = build_app_with_coi(&static_dir, api, coi_enabled);
    let listener = TcpListener::bind(bind_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
    let local_addr = listener
        .local_addr()
        .expect("bound listener has local address");
    println!(
        "reticula-server listening on http://{} (static: {})",
        local_addr,
        static_dir.display()
    );
    tracing::info!(
        addr = %local_addr,
        static_dir = %static_dir.display(),
        "reticula-server listening"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let main_serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_shutdown(shutdown_rx.clone()));

    let serve_result = match metrics_server {
        Some((m_listener, m_app)) => {
            let m_serve =
                axum::serve(m_listener, m_app).with_graceful_shutdown(wait_shutdown(shutdown_rx));
            tokio::try_join!(main_serve, m_serve).map(|((), ())| ())
        }
        None => main_serve.await,
    };
    if let Err(err) = serve_result {
        tracing::error!(%err, "server error");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    fn make_temp_site() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("reticula-server-test-{stamp}"));
        fs::create_dir_all(dir.join("assets")).unwrap();

        fs::write(dir.join("index.html"), b"<!doctype html><title>raw</title>").unwrap();
        fs::write(dir.join("index.html.br"), b"BR_INDEX_PAYLOAD").unwrap();
        fs::write(dir.join("index.html.gz"), b"GZ_INDEX_PAYLOAD").unwrap();

        fs::write(dir.join("sw.js"), b"/* sw */").unwrap();
        fs::write(dir.join("sw.js.br"), b"BR_SW_PAYLOAD").unwrap();

        fs::write(dir.join("assets/app-123.js"), b"console.log(1);").unwrap();
        fs::write(dir.join("assets/app-123.js.gz"), b"GZ_JS_PAYLOAD").unwrap();

        fs::write(dir.join("assets/kern-456.wasm"), b"\0asm\x01\0\0\0").unwrap();
        fs::write(dir.join("assets/kern-456.wasm.br"), b"BR_WASM_PAYLOAD").unwrap();
        dir
    }

    async fn send_raw_http(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    #[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
    async fn readiness_checks_configured_postgres_and_snapshot_directory() {
        let url = env::var("RETICULA_TEST_DATABASE_URL").unwrap();
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("workbooks");
        let store = Store::connect(&url, dir.clone()).await.unwrap();
        let auth =
            AuthConfig::github("http://127.0.0.1", "client".into(), "secret".into()).unwrap();
        let app = build_app(data.path(), Some(ApiState::new(store, auth)));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let request = "GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        assert!(
            send_raw_http(addr, request)
                .await
                .starts_with("HTTP/1.1 200 OK\r\n")
        );

        fs::remove_dir(&dir).unwrap();
        fs::write(dir, "not a directory").unwrap();
        assert!(
            send_raw_http(addr, request)
                .await
                .starts_with("HTTP/1.1 503 Service Unavailable\r\n")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn serves_healthz_precompressed_assets_wasm_mime_cache_headers_and_spa_fallback() {
        let site = make_temp_site();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_app(&site, None);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // 1. /healthz -> 200 OK, no-cache
        let health = send_raw_http(
            addr,
            "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(health.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(
            health
                .to_ascii_lowercase()
                .contains("cache-control: no-cache\r\n")
        );
        assert!(health.ends_with("\r\n\r\nok\n"));

        let readiness = send_raw_http(
            addr,
            "GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(readiness.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));

        // 2. /assets/kern-456.wasm with Accept-Encoding: br, gzip -> brotli + wasm MIME + immutable
        let wasm_resp = send_raw_http(
            addr,
            "GET /assets/kern-456.wasm HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip, br\r\nConnection: close\r\n\r\n",
        )
        .await;
        let wasm_lower = wasm_resp.to_ascii_lowercase();
        assert!(wasm_resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(wasm_lower.contains("content-encoding: br\r\n"));
        assert!(wasm_lower.contains("content-type: application/wasm\r\n"));
        assert!(wasm_lower.contains("cache-control: public, max-age=31536000, immutable\r\n"));
        assert!(wasm_resp.ends_with("\r\n\r\nBR_WASM_PAYLOAD"));

        // 3. /assets/app-123.js with Accept-Encoding: gzip -> gzip + immutable
        let js_resp = send_raw_http(
            addr,
            "GET /assets/app-123.js HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
        )
        .await;
        let js_lower = js_resp.to_ascii_lowercase();
        assert!(js_resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(js_lower.contains("content-encoding: gzip\r\n"));
        assert!(js_lower.contains("cache-control: public, max-age=31536000, immutable\r\n"));
        assert!(js_resp.ends_with("\r\n\r\nGZ_JS_PAYLOAD"));

        // 4. /app SPA route fallback -> serves index.html.br with Cache-Control: no-cache
        let spa_resp = send_raw_http(
            addr,
            "GET /app HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: br\r\nConnection: close\r\n\r\n",
        )
        .await;
        let spa_lower = spa_resp.to_ascii_lowercase();
        assert!(spa_resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(spa_lower.contains("content-encoding: br\r\n"));
        assert!(spa_lower.contains("cache-control: no-cache\r\n"));
        assert!(spa_resp.ends_with("\r\n\r\nBR_INDEX_PAYLOAD"));

        // 5. /sw.js -> precompressed brotli with Cache-Control: no-cache (`14-m1b.md` 7.4)
        let sw_resp = send_raw_http(
            addr,
            "GET /sw.js HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: br\r\nConnection: close\r\n\r\n",
        )
        .await;
        let sw_lower = sw_resp.to_ascii_lowercase();
        assert!(sw_resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(sw_lower.contains("content-encoding: br\r\n"));
        assert!(sw_lower.contains("cache-control: no-cache\r\n"));
        assert!(sw_resp.ends_with("\r\n\r\nBR_SW_PAYLOAD"));

        handle.abort();
        let _ = fs::remove_dir_all(&site);
    }

    #[tokio::test]
    async fn missing_assets_are_not_found_and_not_cached_as_immutable() {
        use axum::body::Body;
        use tower::ServiceExt;

        let site = make_temp_site();
        let app = build_app(&site, None);
        for path in [
            "/assets/app-999.js",
            "/assets/kern-999.wasm",
            "/assets/style-999.css",
            "/assets/nested/app.js",
            "/assets/",
        ] {
            for encoding in ["identity", "br", "gzip"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(path)
                            .header("accept-encoding", encoding)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
                let headers = response.headers();
                assert_eq!(headers.get(CACHE_CONTROL), Some(&NO_CACHE), "{path}");
                assert!(!headers.contains_key(CONTENT_TYPE), "{path}");
            }
        }

        // What is there is still served and kept, also when revalidated.
        let served = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/app-123.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(served.headers().get(CACHE_CONTROL), Some(&IMMUTABLE_CACHE));
        let modified = served.headers().get("last-modified").unwrap().clone();
        let unchanged = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/app-123.js")
                    .header("if-modified-since", modified)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            unchanged.headers().get(CACHE_CONTROL),
            Some(&IMMUTABLE_CACHE)
        );

        // Other paths are still routes of the app.
        let route = app
            .oneshot(
                Request::builder()
                    .uri("/join/some-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(route.status(), StatusCode::OK);

        let _ = fs::remove_dir_all(&site);
    }

    #[tokio::test]
    async fn metrics_route_requires_loopback_connect_info() {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use tower::ServiceExt;

        let site = make_temp_site();
        let app = build_app(&site, None);

        let missing_peer = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_peer.status(), StatusCode::FORBIDDEN);

        let mut external_req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        external_req
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 1234))));
        let external_peer = app.clone().oneshot(external_req).await.unwrap();
        assert_eq!(external_peer.status(), StatusCode::FORBIDDEN);

        let mut loopback_req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        loopback_req
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        let loopback_peer = app.oneshot(loopback_req).await.unwrap();
        assert_eq!(loopback_peer.status(), StatusCode::SERVICE_UNAVAILABLE);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app_tcp = build_app(&site, None);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app_tcp.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        let tcp_resp = send_raw_http(
            addr,
            "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(tcp_resp.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));

        handle.abort();
        let _ = fs::remove_dir_all(&site);
    }

    #[tokio::test]
    async fn coi_headers_are_absent_by_default_and_emitted_when_enabled() {
        use axum::body::Body;
        use tower::ServiceExt;

        let site = make_temp_site();
        fs::write(site.join("assets/style.css"), b"body{margin:0}").unwrap();

        let app_off = build_app_with_coi(&site, None, false);
        for path in [
            "/",
            "/app",
            "/sw.js",
            "/assets/app-123.js",
            "/assets/kern-456.wasm",
        ] {
            let resp = app_off
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let headers = resp.headers();
            assert!(
                !headers.contains_key("cross-origin-opener-policy"),
                "{path}"
            );
            assert!(
                !headers.contains_key("cross-origin-embedder-policy"),
                "{path}"
            );
            assert!(
                !headers.contains_key("cross-origin-resource-policy"),
                "{path}"
            );
        }

        let app_on = build_app_with_coi(&site, None, true);
        for isolated_path in ["/", "/app", "/sw.js", "/assets/app-123.js"] {
            let resp = app_on
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(isolated_path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let headers = resp.headers();
            assert_eq!(
                headers
                    .get("cross-origin-opener-policy")
                    .map(|v| v.to_str().unwrap()),
                Some("same-origin"),
                "{isolated_path}"
            );
            assert_eq!(
                headers
                    .get("cross-origin-embedder-policy")
                    .map(|v| v.to_str().unwrap()),
                Some("require-corp"),
                "{isolated_path}"
            );
            assert_eq!(
                headers
                    .get("cross-origin-resource-policy")
                    .map(|v| v.to_str().unwrap()),
                Some("same-origin"),
                "{isolated_path}"
            );
        }

        for static_path in ["/assets/kern-456.wasm", "/assets/style.css"] {
            let resp = app_on
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(static_path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let headers = resp.headers();
            assert!(
                !headers.contains_key("cross-origin-opener-policy"),
                "{static_path}"
            );
            assert!(
                !headers.contains_key("cross-origin-embedder-policy"),
                "{static_path}"
            );
            assert_eq!(
                headers
                    .get("cross-origin-resource-policy")
                    .map(|v| v.to_str().unwrap()),
                Some("same-origin"),
                "{static_path}"
            );
        }

        let _ = fs::remove_dir_all(&site);
    }
}
