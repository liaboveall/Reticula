//! Operational governance: Prometheus metrics, fixed-window rate limits, and
//! anonymous WebSocket concurrency guards.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::{Layer, Service};
use uuid::Uuid;

const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED_KEYS: usize = 4_096;
pub const MAX_ANON_WS_PER_DOC: usize = 50;
pub const MAX_ANON_WS_PER_IP: usize = 10;

#[derive(Clone, Copy, Debug)]
pub struct GovernanceConfig {
    pub max_loaded_bytes: u64,
    pub write_rpm: u32,
    pub anon_rpm: u32,
    pub max_anon_ws_per_doc: usize,
    pub max_anon_ws_per_ip: usize,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        Self {
            max_loaded_bytes: 384 * 1024 * 1024,
            write_rpm: 60,
            anon_rpm: 120,
            max_anon_ws_per_doc: MAX_ANON_WS_PER_DOC,
            max_anon_ws_per_ip: MAX_ANON_WS_PER_IP,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsConnKind {
    Authed,
    Viewer,
    Anon,
}

#[derive(Default)]
pub struct Metrics {
    active_rooms: AtomicU64,
    loaded_bytes: AtomicU64,
    ws_authed: AtomicU64,
    ws_viewer: AtomicU64,
    ws_anon: AtomicU64,
    txns_committed_total: AtomicU64,
    batch_flush_duration_ns_sum: AtomicU64,
    batch_flush_count: AtomicU64,
    snapshots_written_total: AtomicU64,
    snapshots_pruned_total: AtomicU64,
    rate_limited_total: AtomicU64,
    rejected_txns_total: AtomicU64,
}

impl Metrics {
    pub fn set_rooms(&self, active_rooms: u64, loaded_bytes: u64) {
        self.active_rooms.store(active_rooms, Ordering::Relaxed);
        self.loaded_bytes.store(loaded_bytes, Ordering::Relaxed);
    }

    pub fn track_ws(self: &Arc<Self>, kind: WsConnKind) -> WsConnGuard {
        self.counter_for_ws(kind).fetch_add(1, Ordering::Relaxed);
        WsConnGuard {
            metrics: Arc::clone(self),
            kind,
        }
    }

    pub fn set_ws_kind(&self, guard: &mut WsConnGuard, next: WsConnKind) {
        if guard.kind != next {
            self.counter_for_ws(guard.kind)
                .fetch_sub(1, Ordering::Relaxed);
            self.counter_for_ws(next).fetch_add(1, Ordering::Relaxed);
            guard.kind = next;
        }
    }

    fn counter_for_ws(&self, kind: WsConnKind) -> &AtomicU64 {
        match kind {
            WsConnKind::Authed => &self.ws_authed,
            WsConnKind::Viewer => &self.ws_viewer,
            WsConnKind::Anon => &self.ws_anon,
        }
    }

    pub fn record_commit_batch(&self, txns: u64, flush_duration: Duration) {
        self.txns_committed_total.fetch_add(txns, Ordering::Relaxed);
        let ns = u64::try_from(flush_duration.as_nanos()).unwrap_or(u64::MAX);
        self.batch_flush_duration_ns_sum
            .fetch_add(ns, Ordering::Relaxed);
        self.batch_flush_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_snapshots_written(&self, count: u64) {
        self.snapshots_written_total
            .fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_snapshots_pruned(&self, count: u64) {
        self.snapshots_pruned_total
            .fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_rate_limited(&self) {
        self.rate_limited_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_rejected_txns(&self, count: u64) {
        self.rejected_txns_total.fetch_add(count, Ordering::Relaxed);
    }

    pub fn render_prometheus(&self, disk_used_bytes: u64, disk_over_soft_limit: bool) -> String {
        let mut out = String::with_capacity(1024);
        let active_rooms = self.active_rooms.load(Ordering::Relaxed);
        let loaded_bytes = self.loaded_bytes.load(Ordering::Relaxed);
        let ws_authed = self.ws_authed.load(Ordering::Relaxed);
        let ws_viewer = self.ws_viewer.load(Ordering::Relaxed);
        let ws_anon = self.ws_anon.load(Ordering::Relaxed);
        let txns = self.txns_committed_total.load(Ordering::Relaxed);
        let flush_secs = (self.batch_flush_duration_ns_sum.load(Ordering::Relaxed) as f64) / 1e9;
        let flush_count = self.batch_flush_count.load(Ordering::Relaxed);
        let snaps_written = self.snapshots_written_total.load(Ordering::Relaxed);
        let snaps_pruned = self.snapshots_pruned_total.load(Ordering::Relaxed);
        let rate_limited = self.rate_limited_total.load(Ordering::Relaxed);
        let rejected = self.rejected_txns_total.load(Ordering::Relaxed);
        let over_soft = u8::from(disk_over_soft_limit);

        let _ = writeln!(out, "# TYPE reticula_active_rooms gauge");
        let _ = writeln!(out, "reticula_active_rooms {active_rooms}");
        let _ = writeln!(out, "# TYPE reticula_loaded_bytes gauge");
        let _ = writeln!(out, "reticula_loaded_bytes {loaded_bytes}");
        let _ = writeln!(out, "# TYPE reticula_active_ws_connections gauge");
        let _ = writeln!(
            out,
            "reticula_active_ws_connections{{kind=\"authed\"}} {ws_authed}"
        );
        let _ = writeln!(
            out,
            "reticula_active_ws_connections{{kind=\"viewer\"}} {ws_viewer}"
        );
        let _ = writeln!(
            out,
            "reticula_active_ws_connections{{kind=\"anon\"}} {ws_anon}"
        );
        let _ = writeln!(out, "# TYPE reticula_txns_committed_total counter");
        let _ = writeln!(out, "reticula_txns_committed_total {txns}");
        let _ = writeln!(out, "# TYPE reticula_batch_flush_duration_seconds summary");
        let _ = writeln!(
            out,
            "reticula_batch_flush_duration_seconds_sum {flush_secs:.6}"
        );
        let _ = writeln!(
            out,
            "reticula_batch_flush_duration_seconds_count {flush_count}"
        );
        let _ = writeln!(out, "# TYPE reticula_snapshots_written_total counter");
        let _ = writeln!(out, "reticula_snapshots_written_total {snaps_written}");
        let _ = writeln!(out, "# TYPE reticula_snapshots_pruned_total counter");
        let _ = writeln!(out, "reticula_snapshots_pruned_total {snaps_pruned}");
        let _ = writeln!(out, "# TYPE reticula_disk_used_bytes gauge");
        let _ = writeln!(out, "reticula_disk_used_bytes {disk_used_bytes}");
        let _ = writeln!(out, "# TYPE reticula_disk_over_soft_limit gauge");
        let _ = writeln!(out, "reticula_disk_over_soft_limit {over_soft}");
        let _ = writeln!(out, "# TYPE reticula_rate_limited_total counter");
        let _ = writeln!(out, "reticula_rate_limited_total {rate_limited}");
        let _ = writeln!(out, "# TYPE reticula_rejected_txns_total counter");
        let _ = writeln!(out, "reticula_rejected_txns_total {rejected}");
        out
    }
}

pub struct WsConnGuard {
    metrics: Arc<Metrics>,
    kind: WsConnKind,
}

impl Drop for WsConnGuard {
    fn drop(&mut self) {
        self.metrics
            .counter_for_ws(self.kind)
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Hash, PartialEq, Eq)]
enum RateKey {
    UserWrite(Uuid),
    IpAnon(IpAddr),
}

struct WindowEntry {
    started: Instant,
    count: u32,
}

pub struct RateLimiter {
    config: GovernanceConfig,
    metrics: Arc<Metrics>,
    windows: Mutex<HashMap<RateKey, WindowEntry>>,
    anon_ws: Mutex<AnonWsState>,
    soft_disk_warned: AtomicBool,
}

#[derive(Default)]
struct AnonWsState {
    by_doc: HashMap<Uuid, usize>,
    by_ip: HashMap<IpAddr, usize>,
}

impl RateLimiter {
    pub fn new(config: GovernanceConfig, metrics: Arc<Metrics>) -> Self {
        Self {
            config,
            metrics,
            windows: Mutex::new(HashMap::new()),
            anon_ws: Mutex::new(AnonWsState::default()),
            soft_disk_warned: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> GovernanceConfig {
        self.config
    }

    pub fn note_disk_soft_state(&self, over_soft: bool, used: u64, soft_limit: u64) {
        let prev = self.soft_disk_warned.swap(over_soft, Ordering::Relaxed);
        if over_soft && !prev {
            tracing::warn!(
                disk_used_bytes = used,
                disk_soft_bytes = soft_limit,
                "snapshot directory reached soft disk threshold"
            );
        }
    }

    pub fn check_user_write(&self, user: Uuid) -> Result<(), u64> {
        self.check_limit(RateKey::UserWrite(user), self.config.write_rpm)
    }

    pub fn check_anon_ip(&self, ip: IpAddr) -> Result<(), u64> {
        self.check_limit(RateKey::IpAnon(ip), self.config.anon_rpm)
    }

    fn check_limit(&self, key: RateKey, max_per_min: u32) -> Result<(), u64> {
        let now = Instant::now();
        let mut map = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() > MAX_TRACKED_KEYS {
            map.retain(|_, entry| now.duration_since(entry.started) < RATE_WINDOW);
        }
        let entry = map.entry(key).or_insert(WindowEntry {
            started: now,
            count: 0,
        });
        let elapsed = now.duration_since(entry.started);
        if elapsed >= RATE_WINDOW {
            entry.started = now;
            entry.count = 1;
            return Ok(());
        }
        if entry.count >= max_per_min {
            let remaining = RATE_WINDOW.saturating_sub(elapsed).as_secs().max(1);
            self.metrics.inc_rate_limited();
            return Err(remaining);
        }
        entry.count = entry.count.saturating_add(1);
        Ok(())
    }

    pub fn try_acquire_anon_ws(self: &Arc<Self>, doc: Uuid, ip: IpAddr) -> Option<AnonWsPermit> {
        let mut state = self
            .anon_ws
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let doc_count = state.by_doc.get(&doc).copied().unwrap_or(0);
        let ip_count = state.by_ip.get(&ip).copied().unwrap_or(0);
        if doc_count >= self.config.max_anon_ws_per_doc
            || ip_count >= self.config.max_anon_ws_per_ip
        {
            self.metrics.inc_rate_limited();
            return None;
        }
        state.by_doc.insert(doc, doc_count + 1);
        state.by_ip.insert(ip, ip_count + 1);
        Some(AnonWsPermit {
            limiter: Arc::clone(self),
            doc,
            ip,
        })
    }
}

pub struct AnonWsPermit {
    limiter: Arc<RateLimiter>,
    doc: Uuid,
    ip: IpAddr,
}

impl Drop for AnonWsPermit {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .anon_ws
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = state.by_doc.get_mut(&self.doc) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.by_doc.remove(&self.doc);
            }
        }
        if let Some(count) = state.by_ip.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.by_ip.remove(&self.ip);
            }
        }
    }
}

pub fn has_session_cookie(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|val| val.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .map(str::trim)
        .any(|pair| {
            pair.starts_with("reticula_session=") || pair.starts_with("__Host-reticula_session=")
        })
}

pub fn client_ip<B>(req: &Request<B>) -> IpAddr {
    client_ip_parts(
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0),
        req.headers(),
    )
}

pub fn client_ip_parts(peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
    let peer_ip = peer
        .map(|addr| addr.ip())
        .unwrap_or_else(|| IpAddr::from([127, 0, 0, 1]));
    if peer_ip.is_loopback()
        && let Some(forwarded) = headers
            .get("x-real-ip")
            .and_then(|val| val.to_str().ok())
            .and_then(|val| val.trim().parse::<IpAddr>().ok())
    {
        return forwarded;
    }
    peer_ip
}

#[derive(Clone)]
pub struct AnonRateLimitLayer {
    limiter: Arc<RateLimiter>,
}

impl AnonRateLimitLayer {
    pub fn new(limiter: Arc<RateLimiter>) -> Self {
        Self { limiter }
    }
}

impl<S> Layer<S> for AnonRateLimitLayer {
    type Service = AnonRateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AnonRateLimitService {
            inner,
            limiter: Arc::clone(&self.limiter),
        }
    }
}

#[derive(Clone)]
pub struct AnonRateLimitService<S> {
    inner: S,
    limiter: Arc<RateLimiter>,
}

impl<S> Service<Request<Body>> for AnonRateLimitService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let path = req.uri().path();
        let has_cookie = has_session_cookie(req.headers());
        let skip = has_cookie
            || path == "/healthz"
            || path == "/readyz"
            || path == "/metrics"
            || path == "/openapi.json"
            || path == "/api/openapi.json";
        if !skip {
            let ip = client_ip(&req);
            if let Err(retry_after) = self.limiter.check_anon_ip(ip) {
                return Box::pin(async move {
                    let mut resp = (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        r#"{"error":"rate limit exceeded"}"#,
                    )
                        .into_response();
                    if let Ok(val) = HeaderValue::from_str(&retry_after.to_string()) {
                        resp.headers_mut().insert(header::RETRY_AFTER, val);
                    }
                    Ok(resp)
                });
            }
        }
        Box::pin(self.inner.call(req))
    }
}
