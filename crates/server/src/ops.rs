//! Operational governance: Prometheus metrics, rate limits by requests and by
//! bytes, and anonymous WebSocket concurrency guards.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::{Layer, Service};
use uuid::Uuid;

use crate::proxies::TrustedProxies;

const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Keys are swept when there are so many, and no more often than this.
const SWEEP_KEYS: usize = 4_096;
const SWEEP_EVERY: Duration = Duration::from_secs(5);
/// As many keys as are kept. Who comes after that waits for the next sweep.
const MAX_TRACKED_KEYS: usize = 65_536;
pub const MAX_ANON_WS_PER_DOC: usize = 50;
pub const MAX_ANON_WS_PER_IP: usize = 10;
/// What a request for the bytes of a table costs at least, so that small ones
/// are not free.
const TABLE_REQUEST_FLOOR_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug)]
pub struct GovernanceConfig {
    pub max_loaded_bytes: u64,
    pub write_rpm: u32,
    pub anon_rpm: u32,
    pub max_anon_ws_per_doc: usize,
    pub max_anon_ws_per_ip: usize,
    /// Bytes of tables a user may upload per minute.
    pub table_upload_bpm: u64,
    /// Bytes of tables a user, or an anonymous address, may download per minute.
    pub table_download_bpm: u64,
    /// Requests for tables an anonymous address may make per minute.
    pub table_anon_rpm: u32,
    pub trusted_proxies: TrustedProxies,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        Self {
            max_loaded_bytes: 384 * 1024 * 1024,
            write_rpm: 60,
            anon_rpm: 120,
            max_anon_ws_per_doc: MAX_ANON_WS_PER_DOC,
            max_anon_ws_per_ip: MAX_ANON_WS_PER_IP,
            // All an owner may store, so only churn meets it.
            table_upload_bpm: 1024 * 1024 * 1024,
            table_download_bpm: 2 * 1024 * 1024 * 1024,
            table_anon_rpm: 6_000,
            trusted_proxies: TrustedProxies::default(),
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
    draft_rooms: AtomicU64,
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
    agent_runs_active: AtomicU64,
    agent_runs_waiting_review: AtomicU64,
    agent_runs_merged: AtomicU64,
    agent_runs_rejected: AtomicU64,
    agent_runs_failed: AtomicU64,
    agent_runs_cancelled: AtomicU64,
    agent_tokens_prompt: AtomicU64,
    agent_tokens_cached: AtomicU64,
    agent_tokens_completion: AtomicU64,
}

impl Metrics {
    pub fn set_rooms(&self, active_rooms: u64, loaded_bytes: u64) {
        self.set_rooms_with_drafts(active_rooms, 0, loaded_bytes);
    }

    pub fn set_rooms_with_drafts(&self, active_rooms: u64, draft_rooms: u64, loaded_bytes: u64) {
        self.active_rooms.store(active_rooms, Ordering::Relaxed);
        self.draft_rooms.store(draft_rooms, Ordering::Relaxed);
        self.loaded_bytes.store(loaded_bytes, Ordering::Relaxed);
    }

    pub fn inc_agent_runs_active(&self) {
        self.agent_runs_active.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_agent_runs_active(&self) {
        let _ = self
            .agent_runs_active
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    pub fn record_agent_run_status(&self, status: &str) {
        let counter = match status {
            "waiting_review" => &self.agent_runs_waiting_review,
            "merged" => &self.agent_runs_merged,
            "rejected" => &self.agent_runs_rejected,
            "cancelled" => &self.agent_runs_cancelled,
            _ => &self.agent_runs_failed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_agent_tokens(&self, prompt: u64, cached: u64, completion: u64) {
        if prompt > 0 {
            self.agent_tokens_prompt
                .fetch_add(prompt, Ordering::Relaxed);
        }
        if cached > 0 {
            self.agent_tokens_cached
                .fetch_add(cached, Ordering::Relaxed);
        }
        if completion > 0 {
            self.agent_tokens_completion
                .fetch_add(completion, Ordering::Relaxed);
        }
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
        let draft_rooms = self.draft_rooms.load(Ordering::Relaxed);
        let agent_active = self.agent_runs_active.load(Ordering::Relaxed);
        let runs_waiting = self.agent_runs_waiting_review.load(Ordering::Relaxed);
        let runs_merged = self.agent_runs_merged.load(Ordering::Relaxed);
        let runs_rejected = self.agent_runs_rejected.load(Ordering::Relaxed);
        let runs_failed = self.agent_runs_failed.load(Ordering::Relaxed);
        let runs_cancelled = self.agent_runs_cancelled.load(Ordering::Relaxed);
        let tok_prompt = self.agent_tokens_prompt.load(Ordering::Relaxed);
        let tok_cached = self.agent_tokens_cached.load(Ordering::Relaxed);
        let tok_completion = self.agent_tokens_completion.load(Ordering::Relaxed);

        let _ = writeln!(out, "# TYPE reticula_active_rooms gauge");
        let _ = writeln!(out, "reticula_active_rooms {active_rooms}");
        let _ = writeln!(out, "# TYPE reticula_draft_rooms_active gauge");
        let _ = writeln!(out, "reticula_draft_rooms_active {draft_rooms}");
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
        let _ = writeln!(out, "# TYPE reticula_agent_runs_active gauge");
        let _ = writeln!(out, "reticula_agent_runs_active {agent_active}");
        let _ = writeln!(out, "# TYPE reticula_agent_runs_total counter");
        let _ = writeln!(
            out,
            "reticula_agent_runs_total{{status=\"waiting_review\"}} {runs_waiting}"
        );
        let _ = writeln!(
            out,
            "reticula_agent_runs_total{{status=\"merged\"}} {runs_merged}"
        );
        let _ = writeln!(
            out,
            "reticula_agent_runs_total{{status=\"rejected\"}} {runs_rejected}"
        );
        let _ = writeln!(
            out,
            "reticula_agent_runs_total{{status=\"failed\"}} {runs_failed}"
        );
        let _ = writeln!(
            out,
            "reticula_agent_runs_total{{status=\"cancelled\"}} {runs_cancelled}"
        );
        let _ = writeln!(out, "# TYPE reticula_agent_tokens_total counter");
        let _ = writeln!(
            out,
            "reticula_agent_tokens_total{{kind=\"prompt\"}} {tok_prompt}"
        );
        let _ = writeln!(
            out,
            "reticula_agent_tokens_total{{kind=\"cached\"}} {tok_cached}"
        );
        let _ = writeln!(
            out,
            "reticula_agent_tokens_total{{kind=\"completion\"}} {tok_completion}"
        );
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
    UserTableWrite(Uuid),
    IpAnon(IpAddr),
    IpAnonTable(IpAddr),
}

impl RateKey {
    fn is_visitor(&self) -> bool {
        matches!(self, Self::IpAnon(_) | Self::IpAnonTable(_))
    }
}

struct WindowEntry {
    started: Instant,
    count: u32,
}

/// What limits count an address as. Whoever has an IPv6 address has the 64 bits
/// after the prefix to choose from.
fn counted_as(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from_bits(ip.to_bits() & !u128::from(u64::MAX))),
        v4 => v4,
    }
}

/// What is known of each key, for a bounded number of keys.
struct Tracked<K, V> {
    entries: HashMap<K, V>,
    swept: Option<Instant>,
}

impl<K, V> Default for Tracked<K, V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            swept: None,
        }
    }
}

impl<K: Hash + Eq, V> Tracked<K, V> {
    /// The entry of a key, or nothing when there is no room for it. `keep` tells
    /// entries that still say something.
    fn entry(
        &mut self,
        key: K,
        now: Instant,
        keep: impl FnMut(&K, &mut V) -> bool,
        new: impl FnOnce() -> V,
    ) -> Option<&mut V> {
        let is_due = self
            .swept
            .is_none_or(|swept| now.saturating_duration_since(swept) >= SWEEP_EVERY);
        if self.entries.len() >= SWEEP_KEYS && is_due {
            self.entries.retain(keep);
            self.swept = Some(now);
        }
        if self.entries.len() >= MAX_TRACKED_KEYS && !self.entries.contains_key(&key) {
            return None;
        }
        Some(self.entries.entry(key).or_insert_with(new))
    }
}

/// The keys of users apart from those of visitors, whose number anyone can drive
/// up.
struct Budgets<K, V> {
    users: Mutex<Tracked<K, V>>,
    visitors: Mutex<Tracked<K, V>>,
}

impl<K, V> Default for Budgets<K, V> {
    fn default() -> Self {
        Self {
            users: Mutex::default(),
            visitors: Mutex::default(),
        }
    }
}

impl<K, V> Budgets<K, V> {
    fn of(&self, is_visitor: bool) -> MutexGuard<'_, Tracked<K, V>> {
        let tracked = if is_visitor {
            &self.visitors
        } else {
            &self.users
        };
        tracked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Who downloads the bytes of a table.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum TableReader {
    User(Uuid),
    Anonymous(IpAddr),
}

#[derive(Hash, PartialEq, Eq)]
enum ByteKey {
    Upload(Uuid),
    Download(TableReader),
}

impl ByteKey {
    fn is_visitor(&self) -> bool {
        matches!(self, Self::Download(TableReader::Anonymous(_)))
    }
}

/// A minute's allowance of bytes that refills steadily. A request is admitted
/// while anything is left and may overdraw it, so one larger than what is left
/// gets through and the next one waits.
struct ByteBucket {
    balance: f64,
    refilled: Instant,
}

impl ByteBucket {
    fn refill(&mut self, per_minute: u64, now: Instant) {
        let earned = now.saturating_duration_since(self.refilled).as_secs_f64()
            * (per_minute as f64)
            / RATE_WINDOW.as_secs_f64();
        self.balance = (self.balance + earned).min(per_minute as f64);
        self.refilled = now;
    }

    /// Seconds until a request is admitted again, or nothing when it is now.
    fn wait(&self, per_minute: u64) -> Option<u64> {
        if self.balance > 0.0 {
            return None;
        }
        let per_second = (per_minute as f64) / RATE_WINDOW.as_secs_f64();
        let seconds = ((1.0 - self.balance) / per_second).ceil();
        // An allowance of nothing never refills.
        Some(if seconds.is_finite() {
            (seconds as u64).max(1)
        } else {
            RATE_WINDOW.as_secs()
        })
    }
}

pub struct RateLimiter {
    config: GovernanceConfig,
    metrics: Arc<Metrics>,
    windows: Budgets<RateKey, WindowEntry>,
    bytes: Budgets<ByteKey, ByteBucket>,
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
            windows: Budgets::default(),
            bytes: Budgets::default(),
            anon_ws: Mutex::new(AnonWsState::default()),
            soft_disk_warned: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> &GovernanceConfig {
        &self.config
    }

    /// The address limits are counted against.
    pub fn client_ip(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
        self.config.trusted_proxies.client_ip(peer, headers)
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
        self.check_limit(
            RateKey::UserWrite(user),
            self.config.write_rpm,
            Instant::now(),
        )
    }

    /// Changes to tables other than their bytes.
    pub fn check_user_table_write(&self, user: Uuid) -> Result<(), u64> {
        self.check_limit(
            RateKey::UserTableWrite(user),
            self.config.write_rpm,
            Instant::now(),
        )
    }

    pub fn check_anon_ip(&self, ip: IpAddr) -> Result<(), u64> {
        self.check_limit(
            RateKey::IpAnon(counted_as(ip)),
            self.config.anon_rpm,
            Instant::now(),
        )
    }

    /// Anonymous requests for tables, which are many and small next to the
    /// others and have a budget of their own.
    pub fn check_anon_table(&self, ip: IpAddr) -> Result<(), u64> {
        self.check_anon_table_at(ip, Instant::now())
    }

    fn check_anon_table_at(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        self.check_limit(
            RateKey::IpAnonTable(counted_as(ip)),
            self.config.table_anon_rpm,
            now,
        )
    }

    /// Admits a download of `bytes`, or says in how many seconds to ask again.
    pub fn check_table_download(&self, reader: TableReader, bytes: u64) -> Result<(), u64> {
        self.check_table_download_at(reader, bytes, Instant::now())
    }

    fn check_table_download_at(
        &self,
        reader: TableReader,
        bytes: u64,
        now: Instant,
    ) -> Result<(), u64> {
        let reader = match reader {
            TableReader::Anonymous(ip) => TableReader::Anonymous(counted_as(ip)),
            user @ TableReader::User(_) => user,
        };
        self.take_bytes(
            ByteKey::Download(reader),
            bytes.max(TABLE_REQUEST_FLOOR_BYTES),
            now,
        )
    }

    /// Admits an upload whose size only [`Self::charge_table_upload`] knows.
    pub fn check_table_upload(&self, user: Uuid) -> Result<(), u64> {
        self.take_bytes(
            ByteKey::Upload(user),
            TABLE_REQUEST_FLOOR_BYTES,
            Instant::now(),
        )
    }

    pub fn charge_table_upload(&self, user: Uuid, bytes: u64) {
        let key = ByteKey::Upload(user);
        let mut buckets = self.bytes.of(key.is_visitor());
        if let Some(bucket) = buckets.entries.get_mut(&key) {
            bucket.balance -= bytes.saturating_sub(TABLE_REQUEST_FLOOR_BYTES) as f64;
        }
    }

    fn bytes_per_minute(&self, key: &ByteKey) -> u64 {
        match key {
            ByteKey::Upload(_) => self.config.table_upload_bpm,
            ByteKey::Download(_) => self.config.table_download_bpm,
        }
    }

    fn take_bytes(&self, key: ByteKey, bytes: u64, now: Instant) -> Result<(), u64> {
        let per_minute = self.bytes_per_minute(&key);
        let mut buckets = self.bytes.of(key.is_visitor());
        // A full one is as good as a new one.
        let keep = |key: &ByteKey, bucket: &mut ByteBucket| {
            let per_minute = self.bytes_per_minute(key);
            bucket.refill(per_minute, now);
            bucket.balance < per_minute as f64
        };
        let new = || ByteBucket {
            balance: per_minute as f64,
            refilled: now,
        };
        let Some(bucket) = buckets.entry(key, now, keep, new) else {
            return self.no_room();
        };
        bucket.refill(per_minute, now);
        if let Some(seconds) = bucket.wait(per_minute) {
            self.metrics.inc_rate_limited();
            return Err(seconds);
        }
        bucket.balance -= bytes as f64;
        Ok(())
    }

    fn no_room(&self) -> Result<(), u64> {
        self.metrics.inc_rate_limited();
        Err(SWEEP_EVERY.as_secs())
    }

    fn check_limit(&self, key: RateKey, max_per_min: u32, now: Instant) -> Result<(), u64> {
        let mut windows = self.windows.of(key.is_visitor());
        let keep =
            |_: &RateKey, entry: &mut WindowEntry| now.duration_since(entry.started) < RATE_WINDOW;
        let new = || WindowEntry {
            started: now,
            count: 0,
        };
        let Some(entry) = windows.entry(key, now, keep, new) else {
            return self.no_room();
        };
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
        let ip = counted_as(ip);
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

/// Whether a path below `/api` is one of the tables of a workbook.
fn is_table_path(path: &str) -> bool {
    let mut parts = path.strip_prefix("/api").unwrap_or(path).split('/');
    parts.next() == Some("")
        && parts.next() == Some("workbooks")
        && parts.next().is_some_and(|id| !id.is_empty())
        && parts.next() == Some("tables")
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
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0);
            let ip = self.limiter.client_ip(peer, req.headers());
            let admitted = if is_table_path(path) {
                self.limiter.check_anon_table(ip)
            } else {
                self.limiter.check_anon_ip(ip)
            };
            if let Err(retry_after) = admitted {
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use tower::ServiceExt;

    use super::*;

    fn limiter(config: GovernanceConfig) -> Arc<RateLimiter> {
        Arc::new(RateLimiter::new(config, Arc::default()))
    }

    async fn status_of(
        limiter: &Arc<RateLimiter>,
        peer: &str,
        real_ip: Option<&str>,
        path: &str,
    ) -> StatusCode {
        let service = AnonRateLimitLayer::new(Arc::clone(limiter)).layer(tower::service_fn(
            |_: Request<Body>| async { Ok::<_, Infallible>(StatusCode::OK.into_response()) },
        ));
        let mut request = Request::builder().uri(path);
        if let Some(ip) = real_ip {
            request = request.header("x-real-ip", ip);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        service.oneshot(request).await.unwrap().status()
    }

    const GATEWAY: &str = "172.18.0.1:40000";
    const TABLES: &str = "/workbooks/018f4ad0-0000-7000-8000-000000000001/tables";

    /// Behind the published port of a container the peer is the gateway of
    /// its network, whoever the visitor is.
    #[tokio::test]
    async fn visitors_behind_a_container_gateway_have_separate_budgets() {
        let limiter = limiter(GovernanceConfig {
            anon_rpm: 2,
            trusted_proxies: "172.16.0.0/12".parse().unwrap(),
            ..GovernanceConfig::default()
        });
        for _ in 0..2 {
            assert_eq!(
                status_of(&limiter, GATEWAY, Some("203.0.113.1"), "/auth/me").await,
                StatusCode::OK
            );
        }
        assert_eq!(
            status_of(&limiter, GATEWAY, Some("203.0.113.1"), "/auth/me").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            status_of(&limiter, GATEWAY, Some("203.0.113.2"), "/auth/me").await,
            StatusCode::OK
        );
    }

    /// A visitor who reaches the server directly cannot pick a budget.
    #[tokio::test]
    async fn the_header_of_an_untrusted_peer_changes_no_budget() {
        let limiter = limiter(GovernanceConfig {
            anon_rpm: 2,
            ..GovernanceConfig::default()
        });
        for (visitor, expected) in [
            ("203.0.113.1", StatusCode::OK),
            ("203.0.113.2", StatusCode::OK),
            ("203.0.113.3", StatusCode::TOO_MANY_REQUESTS),
        ] {
            assert_eq!(
                status_of(&limiter, GATEWAY, Some(visitor), "/auth/me").await,
                expected
            );
        }
    }

    #[tokio::test]
    async fn tables_and_everything_else_leave_each_others_requests_alone() {
        let limiter = limiter(GovernanceConfig {
            anon_rpm: 1,
            table_anon_rpm: 2,
            ..GovernanceConfig::default()
        });
        let ok = StatusCode::OK;
        let limited = StatusCode::TOO_MANY_REQUESTS;
        let data = format!("/api{TABLES}/1/data");
        for (path, expected) in [
            ("/auth/me", ok),
            ("/auth/me", limited),
            (TABLES, ok),
            (data.as_str(), ok),
            (TABLES, limited),
            // Not a table, whatever its name.
            ("/workbooks/1/tablesque", limited),
            ("/workbooks//tables", limited),
            ("/tables", limited),
        ] {
            assert_eq!(
                status_of(&limiter, "198.51.100.1:1", None, path).await,
                expected,
                "{path}"
            );
        }
    }

    /// An office behind one address: twenty tabs that each ask for the list
    /// every twenty seconds and read a table, for five minutes.
    #[test]
    fn twenty_anonymous_tabs_reading_a_table_are_never_turned_away() {
        let limiter = limiter(GovernanceConfig::default());
        let ip: IpAddr = "203.0.113.1".parse().unwrap();
        let reader = TableReader::Anonymous(ip);
        let start = Instant::now();
        for second in 0..300_u64 {
            for tab in 0..20_u64 {
                let now = start + Duration::from_millis(second * 1_000 + tab * 50);
                if second % 20 == tab {
                    limiter.check_anon_table_at(ip, now).unwrap();
                }
                // A screenful is a run of bytes of each of a dozen columns.
                if second % 4 == tab % 4 {
                    for _ in 0..12 {
                        limiter.check_anon_table_at(ip, now).unwrap();
                        limiter
                            .check_table_download_at(reader, 96 * 1024, now)
                            .unwrap();
                    }
                }
            }
        }
        // None of it came out of the budget of the other requests.
        for _ in 0..GovernanceConfig::default().anon_rpm {
            limiter.check_anon_ip(ip).unwrap();
        }
        limiter.check_anon_ip(ip).unwrap_err();
        limiter.check_anon_table(ip).unwrap();
    }

    #[test]
    fn a_download_waits_for_the_bytes_taken_before_it() {
        let per_minute = 60 * 1024 * 1024;
        let limiter = limiter(GovernanceConfig {
            table_download_bpm: per_minute,
            ..GovernanceConfig::default()
        });
        let user = TableReader::User(Uuid::from_u128(1));
        let start = Instant::now();
        // Larger than the allowance, and let through all the same.
        limiter
            .check_table_download_at(user, 2 * per_minute, start)
            .unwrap();
        assert_eq!(limiter.check_table_download_at(user, 1, start), Err(61));
        let later = start + Duration::from_secs(59);
        assert_eq!(limiter.check_table_download_at(user, 1, later), Err(2));
        let later = start + Duration::from_secs(61);
        limiter.check_table_download_at(user, 1, later).unwrap();
        // Others are not affected.
        for other in [
            TableReader::User(Uuid::from_u128(2)),
            TableReader::Anonymous("203.0.113.1".parse().unwrap()),
        ] {
            limiter.check_table_download_at(other, 1, start).unwrap();
        }
    }

    #[test]
    fn small_reads_cost_at_least_the_floor() {
        let limiter = limiter(GovernanceConfig {
            table_download_bpm: 4 * TABLE_REQUEST_FLOOR_BYTES,
            ..GovernanceConfig::default()
        });
        let user = TableReader::User(Uuid::from_u128(1));
        let now = Instant::now();
        for _ in 0..4 {
            limiter.check_table_download_at(user, 1, now).unwrap();
        }
        limiter.check_table_download_at(user, 1, now).unwrap_err();
    }

    #[test]
    fn an_upload_is_charged_for_what_arrived() {
        let limiter = limiter(GovernanceConfig {
            table_upload_bpm: 1024 * 1024,
            ..GovernanceConfig::default()
        });
        let (uploader, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        limiter.check_table_upload(uploader).unwrap();
        limiter.charge_table_upload(uploader, 8 * 1024 * 1024);
        limiter.check_table_upload(uploader).unwrap_err();
        limiter.check_table_upload(other).unwrap();
        // Neither direction, nor any other request, pays for it.
        limiter
            .check_table_download(TableReader::User(uploader), 1)
            .unwrap();
        limiter.check_user_write(uploader).unwrap();
        limiter.check_user_table_write(uploader).unwrap();
    }

    #[test]
    fn changes_to_tables_do_not_use_up_the_other_writes() {
        let limiter = limiter(GovernanceConfig {
            write_rpm: 1,
            ..GovernanceConfig::default()
        });
        let user = Uuid::from_u128(1);
        limiter.check_user_write(user).unwrap();
        limiter.check_user_write(user).unwrap_err();
        limiter.check_user_table_write(user).unwrap();
        limiter.check_user_table_write(user).unwrap_err();
    }

    fn v6(bits: u128) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from_bits((0x2001_0db8 << 96) + bits))
    }

    #[test]
    fn the_addresses_of_an_ipv6_network_share_a_budget() {
        let limiter = limiter(GovernanceConfig {
            anon_rpm: 3,
            table_anon_rpm: 3,
            table_download_bpm: TABLE_REQUEST_FLOOR_BYTES,
            max_anon_ws_per_ip: 2,
            ..GovernanceConfig::default()
        });
        for n in 0..3 {
            limiter.check_anon_ip(v6(n)).unwrap();
            limiter.check_anon_table(v6(n << 40)).unwrap();
        }
        limiter.check_anon_ip(v6(3)).unwrap_err();
        limiter
            .check_anon_table(v6(u128::from(u64::MAX)))
            .unwrap_err();
        let reader = |n| TableReader::Anonymous(v6(n));
        let now = Instant::now();
        limiter.check_table_download_at(reader(0), 1, now).unwrap();
        limiter
            .check_table_download_at(reader(1), 1, now)
            .unwrap_err();
        let _sockets: Vec<_> = (0..2)
            .map(|n| limiter.try_acquire_anon_ws(Uuid::nil(), v6(n)).unwrap())
            .collect();
        assert!(limiter.try_acquire_anon_ws(Uuid::nil(), v6(2)).is_none());
        // Another network, and the neighbours of an IPv4 address, have their own.
        limiter.check_anon_ip(v6(1 << 64)).unwrap();
        for n in 1..5 {
            limiter
                .check_anon_ip(IpAddr::from([203, 0, 113, n]))
                .unwrap();
        }
    }

    #[test]
    fn many_networks_are_neither_kept_nor_looked_through_every_time() {
        let mut tracked = Tracked::default();
        let start = Instant::now();
        let mut looked_at = 0_usize;
        let mut admitted = 0_usize;
        for n in 0..200_000_u64 {
            // A minute's worth, a thousand at a time.
            let now = start + Duration::from_millis(n / 1_000 * 300);
            let keep = |_: &u64, _: &mut u64| {
                looked_at += 1;
                true
            };
            admitted += usize::from(tracked.entry(n, now, keep, || n).is_some());
        }
        assert_eq!(admitted, MAX_TRACKED_KEYS);
        assert_eq!(tracked.entries.len(), MAX_TRACKED_KEYS);
        // At most thirteen times, all of them.
        assert!(looked_at <= 13 * MAX_TRACKED_KEYS, "{looked_at}");
        // Whoever is known goes on.
        let late = start + Duration::from_secs(60);
        assert!(tracked.entry(5, late, |_, _| true, || 0).is_some());
        // There is room once some have nothing to say any more.
        assert!(tracked.entry(300_000, late, |_, _| true, || 0).is_none());
        let later = late + SWEEP_EVERY;
        assert!(
            tracked
                .entry(300_000, later, |n, _| *n < 10, || 0)
                .is_some()
        );
        assert_eq!(tracked.entries.len(), 11);
    }

    #[test]
    fn visitors_take_no_room_from_users() {
        let limiter = limiter(GovernanceConfig::default());
        for n in 0..MAX_TRACKED_KEYS as u128 {
            limiter.check_anon_ip(v6(n << 64)).unwrap();
        }
        assert_eq!(limiter.check_anon_ip(v6(1 << 90)), Err(5));
        limiter.check_anon_ip(v6(0)).unwrap();
        limiter.check_user_write(Uuid::from_u128(1)).unwrap();
        limiter.check_table_upload(Uuid::from_u128(1)).unwrap();
    }

    #[test]
    fn an_allowance_of_nothing_admits_nothing() {
        let limiter = limiter(GovernanceConfig {
            table_download_bpm: 0,
            ..GovernanceConfig::default()
        });
        let user = TableReader::User(Uuid::from_u128(1));
        assert_eq!(limiter.check_table_download(user, 1), Err(60));
    }
}
