//! Minimal M1a HTTP server (`13-m1a.md` 8.1 & D-030):
//! - `GET /healthz` -> `200 OK`;
//! - serves precompressed `.br` and `.gz` static assets from `RETICULA_STATIC_DIR`
//!   (default `apps/web/dist`);
//! - sets `Cache-Control: public, max-age=31536000, immutable` on `/assets/*` and
//!   `Cache-Control: no-cache` on HTML / SPA routes (`/`, `/app`);
//! - ensures `.wasm` responses carry `Content-Type: application/wasm` for
//!   streaming WebAssembly compilation.

use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};

const IMMUTABLE_CACHE: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");
const NO_CACHE: HeaderValue = HeaderValue::from_static("no-cache");
const WASM_MIME: HeaderValue = HeaderValue::from_static("application/wasm");

/// Builds the Axum router serving `static_dir` and `/healthz`.
pub fn build_app(static_dir: &Path) -> Router {
    let index_fallback = ServeFile::new(static_dir.join("index.html"))
        .precompressed_br()
        .precompressed_gzip();
    let static_service = ServeDir::new(static_dir)
        .precompressed_br()
        .precompressed_gzip()
        .fallback(index_fallback);

    Router::new()
        .route("/healthz", get(healthz))
        .fallback_service(static_service)
        .layer(middleware::from_fn(apply_asset_headers))
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn apply_asset_headers(req: Request, next: Next) -> Response {
    let uri_path = req.uri().path().to_owned();
    let mut response = next.run(req).await;
    if uri_path == "/healthz" {
        response.headers_mut().insert(CACHE_CONTROL, NO_CACHE);
        return response;
    }

    if uri_path.starts_with("/assets/") {
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
        response.headers_mut().insert(CACHE_CONTROL, NO_CACHE);
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

#[tokio::main]
async fn main() {
    let bind_raw = env::var("BIND_ADDR").unwrap_or_else(|_| String::from("127.0.0.1:3200"));
    let bind_addr: SocketAddr = bind_raw
        .parse()
        .unwrap_or_else(|e| panic!("invalid BIND_ADDR `{bind_raw}`: {e}"));
    let static_dir = PathBuf::from(
        env::var("RETICULA_STATIC_DIR").unwrap_or_else(|_| String::from("apps/web/dist")),
    );

    let app = build_app(&static_dir);
    let listener = TcpListener::bind(bind_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
    println!(
        "reticula-server listening on http://{} (static: {})",
        listener
            .local_addr()
            .expect("bound listener has local address"),
        static_dir.display()
    );

    if let Err(err) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("server error: {err}");
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
    async fn serves_healthz_precompressed_assets_wasm_mime_cache_headers_and_spa_fallback() {
        let site = make_temp_site();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_app(&site);
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

        handle.abort();
        let _ = fs::remove_dir_all(&site);
    }
}
