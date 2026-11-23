//! Document actor lifecycle, multi-branch isolation, shared 384 MiB memory
//! budget LRU eviction, and permission enforcement.

use std::collections::HashMap;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reticula_agent::diff::{
    BranchDiff, ClosureIssue, MergePlanError, MergeSelection, compute_branch_diff, plan_merge_ops,
    preview_merge,
};
use reticula_core::ops::{Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_sync::{
    CommittedPermute, MemberRole, ReplicaWatermark, ServerDocMachine, ServerLimits, StagedTxn,
    TryApplyOutcome, VIEWER_REPLICA,
};
use reticula_wire::convert_v1::WireConvertError;
use reticula_wire::convert_v3::ops_to_wire_txn_v3;
use reticula_wire::sync_frame::{SyncFrameError, encode_sync_frame, encode_sync_frame_v2};
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WirePresenceV1, WireRejectReasonV1, WireSyncFrameV1,
};
use reticula_wire::v3::{
    SYNC_PROTOCOL_VERSION_V2, WireCatchupV2, WireCommittedTxnV3, WirePendingTxnV3, WireSyncFrameV2,
};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use uuid::Uuid;

use crate::ops::{Metrics, WsConnGuard, WsConnKind};
use crate::store::{BatchEntry, DEFAULT_SNAPSHOT_KEEP_RECENT, MAIN_BRANCH, Store, StoreError};

const BATCH_WINDOW: Duration = Duration::from_millis(5);
const BATCH_COUNT: usize = 32;
const BATCH_BYTES: usize = 256 * 1024;
const DIGEST_INTERVAL: u64 = 50;
const QUIET_DIGEST: Duration = Duration::from_secs(5);
const IDLE_CHECK: Duration = Duration::from_secs(60);
const QUEUE_FRAMES: usize = 256;
const QUEUE_BYTES: usize = 16 * 1024 * 1024;
const EVICT_WAIT_BUDGET: Duration = Duration::from_millis(500);
const EVICT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SHUTDOWN_WAIT_LIMIT: Duration = Duration::from_millis(400);
const MIN_ROOM_BYTES: u64 = 64 * 1024;
const SLOW_FLUSH_WARN: Duration = Duration::from_millis(50);

pub const CLOSE_DUPLICATE_REPLICA: u16 = 4001;
pub const CLOSE_ACCESS_REVOKED: u16 = 4003;
pub const CLOSE_SERVER_BUSY: u16 = 4008;

type RoomKey = (Uuid, Uuid);

#[derive(Debug, Error)]
pub enum RoomError {
    #[error("document actor is unavailable")]
    Unavailable,
    #[error("server document memory budget is saturated")]
    Busy,
    #[error("access revoked before session join")]
    AccessRevoked,
    #[error("handshake rejected: {0:?}")]
    Rejected(WireRejectReasonV1),
}

/// Authoritative in-memory snapshot captured from a live (or freshly loaded) room.
#[derive(Clone, Debug)]
pub struct RoomSnapshot {
    pub epoch: u8,
    pub head_seq: u64,
    pub workbook: Workbook<BoundFormula>,
    pub watermarks: Vec<ReplicaWatermark>,
    pub permutes: Vec<CommittedPermute>,
}

/// Result of executing a selective three-way merge inside the authoritative `main` actor.
#[derive(Debug)]
pub enum MergeCommitOutcome {
    Merged {
        merge_seq: u64,
        committed_txns: usize,
        committed_ops: usize,
    },
    Conflict {
        code: &'static str,
        message: String,
        diff: BranchDiff,
        closure_issues: Vec<ClosureIssue>,
        unresolved_conflicts: Vec<String>,
    },
    DiskFull,
    Rejected(WireRejectReasonV1),
    RemapFailed(String),
    Storage(StoreError),
}

#[derive(Clone, Debug)]
pub enum RoomOutbound {
    Frame(Arc<[u8]>),
    Close { code: u16, reason: &'static str },
}

struct RoomSlot {
    sender: mpsc::Sender<Command>,
    est_bytes: Arc<AtomicU64>,
    active: Arc<AtomicUsize>,
    last_used_ms: Arc<AtomicU64>,
    shutting_down: bool,
}

type Registry = Mutex<HashMap<RoomKey, RoomSlot>>;

/// Shared memory lease deducted from `DocRooms::max_loaded_bytes` (e.g. `2×`
/// draft workbook RAM for an active AI agent `FormulaEngine`).
pub struct BudgetLease {
    bytes: u64,
    extra_budget_bytes: Arc<AtomicU64>,
    rooms: Arc<Registry>,
    metrics: Arc<Metrics>,
}

impl BudgetLease {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn update_bytes(&mut self, next_bytes: u64) {
        if next_bytes == self.bytes {
            return;
        }
        if next_bytes > self.bytes {
            self.extra_budget_bytes
                .fetch_add(next_bytes - self.bytes, Ordering::AcqRel);
        } else {
            self.extra_budget_bytes
                .fetch_sub(self.bytes - next_bytes, Ordering::AcqRel);
        }
        self.bytes = next_bytes;
        if let Ok(rooms) = self.rooms.try_lock() {
            sync_room_metrics(&rooms, &self.extra_budget_bytes, &self.metrics);
        }
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        self.extra_budget_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
        if let Ok(rooms) = self.rooms.try_lock() {
            sync_room_metrics(&rooms, &self.extra_budget_bytes, &self.metrics);
        }
    }
}

#[derive(Clone)]
pub struct DocRooms {
    store: Store,
    rooms: Arc<Registry>,
    extra_budget_bytes: Arc<AtomicU64>,
    max_loaded_bytes: u64,
    metrics: Arc<Metrics>,
    epoch_start: Instant,
}

impl DocRooms {
    pub fn new(store: Store) -> Self {
        Self::with_governance(store, 384 * 1024 * 1024, Arc::new(Metrics::default()))
    }

    pub fn with_governance(store: Store, max_loaded_bytes: u64, metrics: Arc<Metrics>) -> Self {
        Self {
            store,
            rooms: Arc::default(),
            extra_budget_bytes: Arc::new(AtomicU64::new(0)),
            max_loaded_bytes,
            metrics,
            epoch_start: Instant::now(),
        }
    }

    pub fn max_loaded_bytes(&self) -> u64 {
        self.max_loaded_bytes
    }

    /// `user` and `role` are supplied by the authenticated transport.
    pub async fn join(
        &self,
        doc: Uuid,
        user: Uuid,
        role: MemberRole,
        display_name: String,
        hello: WireSyncFrameV2,
    ) -> Result<RoomConnection, RoomError> {
        self.join_session(doc, user, role, display_name, None, hello)
            .await
    }

    pub async fn join_session(
        &self,
        doc: Uuid,
        user: Uuid,
        role: MemberRole,
        display_name: String,
        share_link_id: Option<Uuid>,
        hello: WireSyncFrameV2,
    ) -> Result<RoomConnection, RoomError> {
        self.join_session_branch(
            doc,
            MAIN_BRANCH,
            user,
            role,
            display_name,
            share_link_id,
            hello,
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "transport handshake passes workbook, branch, principal, optional share link, and Hello frame"
    )]
    pub async fn join_session_branch(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        user: Uuid,
        role: MemberRole,
        display_name: String,
        share_link_id: Option<Uuid>,
        hello: WireSyncFrameV2,
    ) -> Result<RoomConnection, RoomError> {
        let protocol_version = match &hello {
            WireSyncFrameV2::Hello {
                protocol_version, ..
            } => *protocol_version,
            _ => 0,
        };
        let lease = self.acquire_room_lease(doc, branch_id).await?;
        let (reply, ready) = oneshot::channel();
        let (sender, receiver) = mpsc::channel(QUEUE_FRAMES);
        let budget = Arc::new(Semaphore::new(QUEUE_BYTES));
        lease
            .sender
            .send(Command::Join {
                user,
                role,
                display_name,
                share_link_id,
                hello,
                output: Output {
                    sender,
                    budget: Arc::clone(&budget),
                    graceful_drop: false,
                },
                reply,
            })
            .await
            .map_err(|_| RoomError::Unavailable)?;
        let id = ready.await.map_err(|_| RoomError::Unavailable)??;
        let actor = lease.sender.clone();
        let active = Arc::clone(&lease.active);
        let last_used_ms = Arc::clone(&lease.last_used_ms);
        std::mem::forget(lease);
        Ok(RoomConnection {
            id,
            protocol_version,
            actor,
            receiver,
            budget,
            active,
            last_used_ms,
            epoch_start: self.epoch_start,
        })
    }

    async fn senders_for_doc(&self, doc: Uuid) -> Vec<mpsc::Sender<Command>> {
        let rooms = self.rooms.lock().await;
        rooms
            .iter()
            .filter(|((d, _), _)| *d == doc)
            .map(|(_, slot)| slot.sender.clone())
            .collect()
    }

    pub async fn would_be_busy(&self, doc: Uuid) -> bool {
        self.would_be_busy_branch(doc, MAIN_BRANCH).await
    }

    pub async fn would_be_busy_branch(&self, doc: Uuid, branch_id: Uuid) -> bool {
        let key = (doc, branch_id);
        let rooms = self.rooms.lock().await;
        if rooms
            .get(&key)
            .is_some_and(|slot| !slot.shutting_down && !slot.sender.is_closed())
        {
            return false;
        }
        let Ok(est) = self.store.branch_est_bytes(doc, branch_id).await else {
            return false;
        };
        let needed = est.max(MIN_ROOM_BYTES);
        let active_bytes: u64 = rooms
            .values()
            .filter(|slot| {
                !slot.shutting_down
                    && !slot.sender.is_closed()
                    && slot.active.load(Ordering::Acquire) > 0
            })
            .map(|slot| slot.est_bytes.load(Ordering::Relaxed))
            .sum();
        let extra = self.extra_budget_bytes.load(Ordering::Acquire);
        active_bytes.saturating_add(extra).saturating_add(needed) > self.max_loaded_bytes
    }

    /// Reserves `bytes` out of the shared `max_loaded_bytes` budget (evicting idle
    /// rooms LRU — idle draft rooms before idle main rooms — or returning `RoomError::Busy`).
    pub async fn acquire_budget_lease(&self, bytes: u64) -> Result<BudgetLease, RoomError> {
        let needed = bytes.max(MIN_ROOM_BYTES);
        let deadline = Instant::now() + EVICT_WAIT_BUDGET;
        loop {
            let eviction_targets: Vec<(RoomKey, mpsc::Sender<Command>)>;
            {
                let mut rooms = self.rooms.lock().await;
                rooms.retain(|_, slot| !slot.sender.is_closed());
                let total_loaded: u64 = rooms
                    .values()
                    .filter(|slot| !slot.shutting_down)
                    .map(|slot| slot.est_bytes.load(Ordering::Relaxed))
                    .sum();
                let extra = self.extra_budget_bytes.load(Ordering::Acquire);
                if total_loaded.saturating_add(extra).saturating_add(needed)
                    <= self.max_loaded_bytes
                {
                    self.extra_budget_bytes.fetch_add(needed, Ordering::AcqRel);
                    sync_room_metrics(&rooms, &self.extra_budget_bytes, &self.metrics);
                    return Ok(BudgetLease {
                        bytes: needed,
                        extra_budget_bytes: Arc::clone(&self.extra_budget_bytes),
                        rooms: Arc::clone(&self.rooms),
                        metrics: Arc::clone(&self.metrics),
                    });
                }

                eviction_targets = choose_eviction_targets(
                    &mut rooms,
                    None,
                    total_loaded.saturating_add(extra),
                    needed,
                    self.max_loaded_bytes,
                );
            }

            if !eviction_targets.is_empty() {
                self.evict_targets(eviction_targets).await;
                continue;
            }

            if Instant::now() >= deadline {
                tracing::warn!(
                    needed_bytes = needed,
                    max_loaded_bytes = self.max_loaded_bytes,
                    "rejecting agent budget lease due to shared memory budget saturation"
                );
                return Err(RoomError::Busy);
            }
            tokio::time::sleep(EVICT_POLL_INTERVAL).await;
        }
    }

    /// Captures the authoritative workbook state of `(doc, branch_id)` — flushing the
    /// live actor if loaded, otherwise reading cleanly from PostgreSQL without occupying
    /// a room slot.
    pub async fn snapshot_state(
        &self,
        doc: Uuid,
        branch_id: Uuid,
    ) -> Result<RoomSnapshot, StoreError> {
        let sender = {
            let rooms = self.rooms.lock().await;
            rooms
                .get(&(doc, branch_id))
                .filter(|slot| !slot.shutting_down && !slot.sender.is_closed())
                .map(|slot| slot.sender.clone())
        };
        if let Some(sender) = sender {
            let (reply, rx) = oneshot::channel();
            if sender.send(Command::SnapshotState { reply }).await.is_ok()
                && let Ok(res) = rx.await
            {
                return res;
            }
        }
        let machine = self.store.load_machine_branch(doc, branch_id).await?;
        Ok(RoomSnapshot {
            epoch: machine.epoch(),
            head_seq: machine.head_seq(),
            workbook: machine.workbook().clone(),
            watermarks: machine.watermarks(),
            permutes: machine.committed_permutes().to_vec(),
        })
    }

    /// Commits a server-authored transaction (such as an AI draft tool write) on
    /// `(doc, branch_id)` through the authoritative `DocActor`, persisting durably to
    /// PostgreSQL and broadcasting `Commit` frames to all connected subscribers.
    #[expect(
        clippy::too_many_arguments,
        reason = "server commit requires workbook, branch, author, replica, client_seq, label, and ops"
    )]
    pub async fn server_commit(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        user: Uuid,
        replica: u32,
        client_seq: u64,
        label: String,
        ops: Vec<Op<BoundFormula>>,
    ) -> Result<u64, RoomError> {
        let lease = self.acquire_room_lease(doc, branch_id).await?;
        let (reply, rx) = oneshot::channel();
        lease
            .sender
            .send(Command::ServerCommit {
                user,
                replica,
                client_seq,
                label,
                ops,
                reply,
            })
            .await
            .map_err(|_| RoomError::Unavailable)?;
        rx.await.map_err(|_| RoomError::Unavailable)?
    }

    /// Broadcasts a server-synthesized presence frame (e.g. AI Analyst cursor with
    /// `color_slot = 0x80 | 5`) to any clients currently watching `(doc, branch_id)`.
    pub async fn broadcast_presence(&self, doc: Uuid, branch_id: Uuid, update: WirePresenceV1) {
        let sender = {
            let rooms = self.rooms.lock().await;
            rooms
                .get(&(doc, branch_id))
                .filter(|slot| !slot.shutting_down && !slot.sender.is_closed())
                .map(|slot| slot.sender.clone())
        };
        if let Some(sender) = sender {
            let _ = sender.send(Command::BroadcastPresence { update }).await;
        }
    }

    /// Executes an atomic three-way merge inside the authoritative `(doc, MAIN_BRANCH)`
    /// `DocActor`, guaranteeing zero TOCTOU race with concurrent editors on `main`.
    #[expect(
        clippy::too_many_arguments,
        reason = "three-way merge needs workbook, user, role, base, draft, fork_seq, draft_txns, selection, and label"
    )]
    pub async fn apply_merge(
        &self,
        doc: Uuid,
        user: Uuid,
        role: MemberRole,
        base: Workbook<BoundFormula>,
        draft: Workbook<BoundFormula>,
        fork_seq: u64,
        draft_txns: Vec<WireCommittedTxnV3>,
        selection: MergeSelection,
        label: String,
    ) -> Result<MergeCommitOutcome, RoomError> {
        let lease = self.acquire_room_lease(doc, MAIN_BRANCH).await?;
        let (reply, rx) = oneshot::channel();
        lease
            .sender
            .send(Command::ApplyMerge(Box::new(ApplyMergeCmd {
                user,
                role,
                base,
                draft,
                fork_seq,
                draft_txns,
                selection,
                label,
                reply,
            })))
            .await
            .map_err(|_| RoomError::Unavailable)?;
        rx.await.map_err(|_| RoomError::Unavailable)
    }

    pub async fn update_member_role(&self, doc: Uuid, user: Uuid, role: Option<MemberRole>) {
        for sender in self.senders_for_doc(doc).await {
            let _ = sender.send(Command::UpdateRole { user, role }).await;
        }
    }

    pub async fn revoke_share_link(&self, doc: Uuid, link_id: Uuid) {
        for sender in self.senders_for_doc(doc).await {
            let _ = sender.send(Command::RevokeShareToken { link_id }).await;
        }
    }

    pub async fn kick_all_anonymous(&self, doc: Uuid) {
        for sender in self.senders_for_doc(doc).await {
            let _ = sender.send(Command::KickAllAnonymous).await;
        }
    }

    /// Shuts down all loaded rooms (main + all branches) belonging to `doc`.
    pub async fn shutdown_room(&self, doc: Uuid) {
        let targets: Vec<(RoomKey, mpsc::Sender<Command>)> = {
            let mut rooms = self.rooms.lock().await;
            rooms
                .iter_mut()
                .filter(|((d, _), _)| *d == doc)
                .map(|(k, slot)| {
                    slot.shutting_down = true;
                    (*k, slot.sender.clone())
                })
                .collect()
        };
        self.evict_targets(targets).await;
    }

    /// Shuts down a single branch room `(doc, branch_id)`.
    pub async fn shutdown_branch_room(&self, doc: Uuid, branch_id: Uuid) {
        let key = (doc, branch_id);
        let sender = {
            let mut rooms = self.rooms.lock().await;
            let Some(slot) = rooms.get_mut(&key) else {
                return;
            };
            slot.shutting_down = true;
            slot.sender.clone()
        };
        self.evict_targets(vec![(key, sender)]).await;
    }

    async fn evict_targets(&self, targets: Vec<(RoomKey, mpsc::Sender<Command>)>) {
        for (cand_key, sender) in targets {
            let (tx, rx) = oneshot::channel();
            if sender.send(Command::Shutdown { ack: tx }).await.is_ok() {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT_LIMIT, rx).await;
            }
            let mut rooms = self.rooms.lock().await;
            if rooms
                .get(&cand_key)
                .is_some_and(|slot| slot.sender.same_channel(&sender))
            {
                rooms.remove(&cand_key);
            }
            sync_room_metrics(&rooms, &self.extra_budget_bytes, &self.metrics);
        }
    }

    async fn acquire_room_lease(&self, doc: Uuid, branch_id: Uuid) -> Result<RoomLease, RoomError> {
        let key = (doc, branch_id);
        let needed = self
            .store
            .branch_est_bytes(doc, branch_id)
            .await
            .map_err(|_| RoomError::Unavailable)?
            .max(MIN_ROOM_BYTES);

        let deadline = Instant::now() + EVICT_WAIT_BUDGET;
        loop {
            let eviction_targets: Vec<(RoomKey, mpsc::Sender<Command>)>;
            {
                let mut rooms = self.rooms.lock().await;
                rooms.retain(|_, slot| !slot.sender.is_closed());
                if let Some(slot) = rooms.get_mut(&key)
                    && !slot.shutting_down
                {
                    slot.active.fetch_add(1, Ordering::AcqRel);
                    slot.last_used_ms
                        .store(now_since_ms(self.epoch_start), Ordering::Relaxed);
                    return Ok(RoomLease {
                        sender: slot.sender.clone(),
                        active: Arc::clone(&slot.active),
                        last_used_ms: Arc::clone(&slot.last_used_ms),
                        epoch_start: self.epoch_start,
                    });
                }

                let total_loaded: u64 = rooms
                    .values()
                    .filter(|slot| !slot.shutting_down)
                    .map(|slot| slot.est_bytes.load(Ordering::Relaxed))
                    .sum();
                let extra = self.extra_budget_bytes.load(Ordering::Acquire);
                if total_loaded.saturating_add(extra).saturating_add(needed)
                    <= self.max_loaded_bytes
                {
                    let est_bytes = Arc::new(AtomicU64::new(needed));
                    let active = Arc::new(AtomicUsize::new(1));
                    let last_used_ms = Arc::new(AtomicU64::new(now_since_ms(self.epoch_start)));
                    let sender = self.spawn_room(
                        doc,
                        branch_id,
                        &est_bytes,
                        Arc::clone(&active),
                        Arc::clone(&last_used_ms),
                        &mut rooms,
                    );
                    sync_room_metrics(&rooms, &self.extra_budget_bytes, &self.metrics);
                    return Ok(RoomLease {
                        sender,
                        active,
                        last_used_ms,
                        epoch_start: self.epoch_start,
                    });
                }

                eviction_targets = choose_eviction_targets(
                    &mut rooms,
                    Some(key),
                    total_loaded.saturating_add(extra),
                    needed,
                    self.max_loaded_bytes,
                );
            }

            if !eviction_targets.is_empty() {
                for (cand_key, _) in &eviction_targets {
                    tracing::info!(
                        workbook_id = %cand_key.0,
                        branch_id = %cand_key.1,
                        "evicting idle LRU document actor"
                    );
                }
                self.evict_targets(eviction_targets).await;
                continue;
            }

            if Instant::now() >= deadline {
                tracing::warn!(
                    workbook_id = %doc,
                    branch_id = %branch_id,
                    needed_bytes = needed,
                    max_loaded_bytes = self.max_loaded_bytes,
                    "rejecting document load due to memory budget saturation"
                );
                return Err(RoomError::Busy);
            }
            tokio::time::sleep(EVICT_POLL_INTERVAL).await;
        }
    }

    fn spawn_room(
        &self,
        doc: Uuid,
        branch_id: Uuid,
        slot_est_bytes: &Arc<AtomicU64>,
        slot_active: Arc<AtomicUsize>,
        last_used_ms: Arc<AtomicU64>,
        rooms: &mut HashMap<RoomKey, RoomSlot>,
    ) -> mpsc::Sender<Command> {
        let key = (doc, branch_id);
        let (sender, receiver) = mpsc::channel(64);
        rooms.insert(
            key,
            RoomSlot {
                sender: sender.clone(),
                est_bytes: Arc::clone(slot_est_bytes),
                active: Arc::clone(&slot_active),
                last_used_ms,
                shutting_down: false,
            },
        );
        let store = self.store.clone();
        let metrics = Arc::clone(&self.metrics);
        let all_rooms = Arc::clone(&self.rooms);
        let extra_budget = Arc::clone(&self.extra_budget_bytes);
        let est_ref = Arc::clone(slot_est_bytes);
        let task = tokio::spawn(async move {
            DocActor::run(
                store,
                doc,
                branch_id,
                receiver,
                metrics,
                all_rooms,
                extra_budget,
                est_ref,
                slot_active,
            )
            .await
        });
        let channel = sender.clone();
        let registry = Arc::downgrade(&self.rooms);
        let metrics = Arc::clone(&self.metrics);
        let extra_budget = Arc::clone(&self.extra_budget_bytes);
        tokio::spawn(async move {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(
                        workbook_id = %doc,
                        branch_id = %branch_id,
                        %error,
                        "document actor stopped with error"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        workbook_id = %doc,
                        branch_id = %branch_id,
                        %error,
                        "document actor panicked"
                    );
                }
            }
            if let Some(registry) = registry.upgrade() {
                let mut rooms = registry.lock().await;
                if rooms
                    .get(&key)
                    .is_some_and(|slot| slot.sender.same_channel(&channel))
                {
                    rooms.remove(&key);
                }
                sync_room_metrics(&rooms, &extra_budget, &metrics);
            }
        });
        sender
    }
}

fn choose_eviction_targets(
    rooms: &mut HashMap<RoomKey, RoomSlot>,
    skip_key: Option<RoomKey>,
    current_total: u64,
    needed: u64,
    max_loaded_bytes: u64,
) -> Vec<(RoomKey, mpsc::Sender<Command>)> {
    // Evict idle non-main draft rooms first, then idle main rooms, ordered by LRU.
    let mut idle: Vec<(bool, u64, RoomKey, u64)> = rooms
        .iter()
        .filter(|(key, slot)| {
            Some(**key) != skip_key
                && !slot.shutting_down
                && slot.active.load(Ordering::Acquire) == 0
        })
        .map(|(key, slot)| {
            (
                key.1 == MAIN_BRANCH,
                slot.last_used_ms.load(Ordering::Relaxed),
                *key,
                slot.est_bytes.load(Ordering::Relaxed),
            )
        })
        .collect();
    idle.sort_by_key(|(is_main, ts, key, _)| (*is_main, *ts, *key));

    let mut projected = current_total;
    let mut chosen = Vec::new();
    for (_, _, cand_key, cand_bytes) in idle {
        if projected.saturating_add(needed) <= max_loaded_bytes {
            break;
        }
        if let Some(slot) = rooms.get_mut(&cand_key) {
            slot.shutting_down = true;
            projected = projected.saturating_sub(cand_bytes);
            chosen.push((cand_key, slot.sender.clone()));
        }
    }
    chosen
}

struct RoomLease {
    sender: mpsc::Sender<Command>,
    active: Arc<AtomicUsize>,
    last_used_ms: Arc<AtomicU64>,
    epoch_start: Instant,
}

impl Drop for RoomLease {
    fn drop(&mut self) {
        self.last_used_ms
            .store(now_since_ms(self.epoch_start), Ordering::Relaxed);
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct RoomConnection {
    id: u64,
    protocol_version: u8,
    actor: mpsc::Sender<Command>,
    receiver: mpsc::Receiver<QueuedItem>,
    budget: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    last_used_ms: Arc<AtomicU64>,
    epoch_start: Instant,
}

impl Drop for RoomConnection {
    fn drop(&mut self) {
        let _ = self.actor.try_send(Command::Leave { id: self.id });
        self.last_used_ms
            .store(now_since_ms(self.epoch_start), Ordering::Relaxed);
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl RoomConnection {
    #[must_use]
    pub const fn protocol_version(&self) -> u8 {
        self.protocol_version
    }

    pub async fn push(
        &self,
        txns: Vec<WirePendingTxnV3>,
        wire_bytes: usize,
    ) -> Result<(), RoomError> {
        self.actor
            .send(Command::Push {
                id: self.id,
                txns,
                wire_bytes,
            })
            .await
            .map_err(|_| RoomError::Unavailable)
    }

    pub async fn recv(&mut self) -> Option<Arc<[u8]>> {
        match self.recv_event().await? {
            RoomOutbound::Frame(bytes) => Some(bytes),
            RoomOutbound::Close { .. } => None,
        }
    }

    pub async fn recv_event(&mut self) -> Option<RoomOutbound> {
        let packet = self.receiver.recv().await?;
        match packet.item {
            RoomOutbound::Frame(_) if self.budget.is_closed() => None,
            other => Some(other),
        }
    }

    pub async fn presence(&self, update: WirePresenceV1) -> Result<(), RoomError> {
        self.actor
            .send(Command::Presence {
                id: self.id,
                update,
            })
            .await
            .map_err(|_| RoomError::Unavailable)
    }
}

struct QueuedItem {
    item: RoomOutbound,
    _permit: OwnedSemaphorePermit,
}

struct Output {
    sender: mpsc::Sender<QueuedItem>,
    budget: Arc<Semaphore>,
    graceful_drop: bool,
}

impl Output {
    fn send(&self, bytes: Arc<[u8]>) -> bool {
        let Ok(permit) = Arc::clone(&self.budget).try_acquire_many_owned(bytes.len() as u32) else {
            return false;
        };
        self.sender
            .try_send(QueuedItem {
                item: RoomOutbound::Frame(bytes),
                _permit: permit,
            })
            .is_ok()
    }

    fn send_close(&self, code: u16, reason: &'static str) {
        if let Ok(permit) = Arc::clone(&self.budget).try_acquire_many_owned(0) {
            let _ = self.sender.try_send(QueuedItem {
                item: RoomOutbound::Close { code, reason },
                _permit: permit,
            });
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if !self.graceful_drop {
            self.budget.close();
        }
    }
}

enum Command {
    Join {
        user: Uuid,
        role: MemberRole,
        display_name: String,
        share_link_id: Option<Uuid>,
        hello: WireSyncFrameV2,
        output: Output,
        reply: oneshot::Sender<Result<u64, RoomError>>,
    },
    Push {
        id: u64,
        txns: Vec<WirePendingTxnV3>,
        wire_bytes: usize,
    },
    Presence {
        id: u64,
        update: WirePresenceV1,
    },
    BroadcastPresence {
        update: WirePresenceV1,
    },
    ServerCommit {
        user: Uuid,
        replica: u32,
        client_seq: u64,
        label: String,
        ops: Vec<Op<BoundFormula>>,
        reply: oneshot::Sender<Result<u64, RoomError>>,
    },
    SnapshotState {
        reply: oneshot::Sender<Result<RoomSnapshot, StoreError>>,
    },
    ApplyMerge(Box<ApplyMergeCmd>),
    Leave {
        id: u64,
    },
    UpdateRole {
        user: Uuid,
        role: Option<MemberRole>,
    },
    RevokeShareToken {
        link_id: Uuid,
    },
    KickAllAnonymous,
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

struct ApplyMergeCmd {
    user: Uuid,
    role: MemberRole,
    base: Workbook<BoundFormula>,
    draft: Workbook<BoundFormula>,
    fork_seq: u64,
    draft_txns: Vec<WireCommittedTxnV3>,
    selection: MergeSelection,
    label: String,
    reply: oneshot::Sender<MergeCommitOutcome>,
}

struct Member {
    user: Uuid,
    role: MemberRole,
    replica: u32,
    protocol_version: u8,
    display_name: String,
    share_link_id: Option<Uuid>,
    last_presence: Option<Instant>,
    output: Output,
    ws_guard: WsConnGuard,
}

struct QueuedTxn {
    staged: StagedTxn,
    base_seq: u64,
    author: Option<u64>,
}

type SnapshotTask = JoinHandle<Result<u64, StoreError>>;

struct DocActor {
    store: Store,
    doc: Uuid,
    branch_id: Uuid,
    is_ai_draft: bool,
    machine: ServerDocMachine,
    members: HashMap<u64, Member>,
    next_member: u64,
    batch: Vec<QueuedTxn>,
    bytes: usize,
    flush_at: Option<Instant>,
    quiet_at: Option<Instant>,
    snapshot: Option<SnapshotTask>,
    metrics: Arc<Metrics>,
    registry: Arc<Registry>,
    extra_budget_bytes: Arc<AtomicU64>,
    slot_est_bytes: Arc<AtomicU64>,
    slot_active: Arc<AtomicUsize>,
}

impl DocActor {
    #[expect(
        clippy::too_many_arguments,
        reason = "actor task owns store, branch identity, command receiver, shared metrics, and registry handles"
    )]
    async fn run(
        store: Store,
        doc: Uuid,
        branch_id: Uuid,
        mut commands: mpsc::Receiver<Command>,
        metrics: Arc<Metrics>,
        registry: Arc<Registry>,
        extra_budget_bytes: Arc<AtomicU64>,
        slot_est_bytes: Arc<AtomicU64>,
        slot_active: Arc<AtomicUsize>,
    ) -> Result<(), StoreError> {
        let is_ai_draft = if branch_id == MAIN_BRANCH {
            false
        } else {
            store
                .get_branch(doc, branch_id)
                .await?
                .ok_or(StoreError::NotFound)?
                .kind
                == "ai_draft"
        };
        let machine = store.load_machine_branch(doc, branch_id).await?;
        let initial_est = (machine.workbook().estimated_bytes() as u64).max(MIN_ROOM_BYTES);
        slot_est_bytes.store(initial_est, Ordering::Relaxed);
        {
            let rooms = registry.lock().await;
            sync_room_metrics(&rooms, &extra_budget_bytes, &metrics);
        }
        if branch_id == MAIN_BRANCH && store.snapshot_branch(doc, MAIN_BRANCH, 0).await?.is_none() {
            let base = ServerDocMachine::new(
                doc.into_bytes(),
                MAIN_BRANCH.into_bytes(),
                String::new(),
                ServerLimits::default(),
            );
            store.write_snapshot(&base).await?;
        }
        let mut room = Self {
            store,
            doc,
            branch_id,
            is_ai_draft,
            machine,
            members: HashMap::new(),
            next_member: 0,
            batch: Vec::with_capacity(BATCH_COUNT),
            bytes: 0,
            flush_at: None,
            quiet_at: None,
            snapshot: None,
            metrics,
            registry,
            extra_budget_bytes,
            slot_est_bytes,
            slot_active,
        };
        let mut idle_at = Instant::now() + IDLE_CHECK;
        loop {
            room.schedule_snapshot();
            tokio::select! {
                biased;
                () = deadline(room.flush_at) => room.flush().await?,
                result = wait_snapshot(&mut room.snapshot), if room.batch.is_empty() => {
                    room.snapshot = None;
                    room.machine.mark_snapshot_written(result??);
                }
                () = deadline(room.quiet_at), if room.batch.is_empty() => {
                    room.quiet_at = None;
                    room.broadcast(&room.machine.digest_frame())?;
                }
                () = sleep_until(idle_at) => {
                    room.members.retain(|_, member| !member.output.sender.is_closed());
                    if room.members.is_empty()
                        && commands.is_empty()
                        && room.slot_active.load(Ordering::Acquire) == 0
                    {
                        commands.close();
                    }
                    idle_at = Instant::now() + IDLE_CHECK;
                }
                command = commands.recv() => match command {
                    Some(Command::Join { user, role, display_name, share_link_id, hello, output, reply }) => {
                        room.flush().await?;
                        room.join(user, role, display_name, share_link_id, &hello, output, reply).await?;
                    }
                    Some(Command::Push { id, txns, wire_bytes }) => room.push(id, txns, wire_bytes).await?,
                    Some(Command::Presence { id, update }) => room.presence(id, update)?,
                    Some(Command::BroadcastPresence { update }) => {
                        room.broadcast(&WireSyncFrameV2::Presence(update))?;
                    }
                    Some(Command::ServerCommit { user, replica, client_seq, label, ops, reply }) => {
                        let result = room.server_commit(user, replica, client_seq, label, ops).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::SnapshotState { reply }) => {
                        let res = match room.flush().await {
                            Ok(()) => Ok(RoomSnapshot {
                                epoch: room.machine.epoch(),
                                head_seq: room.machine.head_seq(),
                                workbook: room.machine.workbook().clone(),
                                watermarks: room.machine.watermarks(),
                                permutes: room.machine.committed_permutes().to_vec(),
                            }),
                            Err(err) => Err(err),
                        };
                        let _ = reply.send(res);
                    }
                    Some(Command::ApplyMerge(cmd)) => {
                        let ApplyMergeCmd {
                            user,
                            role,
                            base,
                            draft,
                            fork_seq,
                            draft_txns,
                            selection,
                            label,
                            reply,
                        } = *cmd;
                        let outcome = room
                            .apply_merge(user, role, base, draft, fork_seq, draft_txns, selection, label)
                            .await;
                        let _ = reply.send(outcome);
                    }
                    Some(Command::Leave { id }) => {
                        room.members.remove(&id);
                    }
                    Some(Command::UpdateRole { user, role }) => {
                        room.flush().await?;
                        room.update_role(user, role);
                    }
                    Some(Command::RevokeShareToken { link_id }) => room.revoke_share_token(link_id),
                    Some(Command::KickAllAnonymous) => room.kick_all_anonymous(),
                    Some(Command::Shutdown { ack }) => {
                        room.flush().await?;
                        if let Some(task) = room.snapshot.take() {
                            let _ = task.await;
                        }
                        for (_, member) in room.members.drain() {
                            member.output.send_close(1001, "Server room closing");
                        }
                        let _ = ack.send(());
                        return Ok(());
                    }
                    None => {
                        room.flush().await?;
                        return Ok(());
                    }
                },
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "handshake joins user role, display identity, optional share link, and channel output"
    )]
    async fn join(
        &mut self,
        user: Uuid,
        _role: MemberRole,
        display_name: String,
        share_link_id: Option<Uuid>,
        hello: &WireSyncFrameV2,
        output: Output,
        reply: oneshot::Sender<Result<u64, RoomError>>,
    ) -> Result<(), StoreError> {
        let protocol_version = match hello {
            WireSyncFrameV2::Hello {
                protocol_version, ..
            } => *protocol_version,
            _ => SYNC_PROTOCOL_VERSION_V2,
        };
        let mut role = if let Some(link_id) = share_link_id {
            if self.branch_id != MAIN_BRANCH
                || !self
                    .store
                    .is_anonymous_share_active(self.doc, link_id)
                    .await?
            {
                let _ = reply.send(Err(RoomError::AccessRevoked));
                return Ok(());
            }
            MemberRole::Viewer
        } else {
            let Some(access) = self.store.workbook_access(self.doc, user).await? else {
                let _ = reply.send(Err(RoomError::AccessRevoked));
                return Ok(());
            };
            access.role
        };

        // AI draft branches are strictly read-only over WebSocket (`VIEWER_REPLICA = 0`).
        if self.is_ai_draft {
            role = MemberRole::Viewer;
        }

        // Atomically allocate fresh replicas in PostgreSQL across all branches to avoid
        // cross-room `StaleActor` races (`18-m4.md` §7.1).
        let needs_fresh_replica = role.can_write()
            && matches!(
                hello,
                WireSyncFrameV2::Hello {
                    protocol_version,
                    doc_id,
                    branch_id,
                    replica,
                    known_epoch,
                    ..
                } if (*protocol_version == SYNC_PROTOCOL_VERSION
                    || *protocol_version == SYNC_PROTOCOL_VERSION_V2)
                    && *doc_id == self.machine.doc_id()
                    && *branch_id == self.machine.branch_id()
                    && (*known_epoch != self.machine.epoch() || replica.is_none())
            );
        let external_replica = if needs_fresh_replica {
            match self
                .store
                .allocate_replica(self.doc, self.machine.epoch(), user)
                .await
            {
                Ok(rep) => Some(rep),
                Err(StoreError::QuotaExceeded(_)) => {
                    let _ = reply.send(Err(RoomError::Rejected(WireRejectReasonV1::QuotaExceeded)));
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        } else {
            None
        };

        let mut welcome = match self.machine.on_hello_with_external_replica(
            user.into_bytes(),
            Some(role),
            hello,
            external_replica,
        ) {
            Ok(welcome) => welcome,
            Err(reason) => {
                let _ = reply.send(Err(RoomError::Rejected(reason)));
                return Ok(());
            }
        };
        let WireSyncFrameV2::Welcome {
            replica,
            ref catchup,
            ..
        } = welcome
        else {
            return Err(StoreError::Invalid("invalid handshake result"));
        };

        if replica != VIEWER_REPLICA {
            let duplicate_ids: Vec<u64> = self
                .members
                .iter()
                .filter(|(_, m)| {
                    m.user == user && m.share_link_id.is_none() && m.replica == replica
                })
                .map(|(id, _)| *id)
                .collect();
            for old_id in duplicate_ids {
                if let Some(old_member) = self.members.remove(&old_id) {
                    tracing::info!(
                        workbook_id = %self.doc,
                        branch_id = %self.branch_id,
                        replica,
                        "closing superseded replica connection"
                    );
                    old_member
                        .output
                        .send_close(CLOSE_DUPLICATE_REPLICA, "Replaced by newer session");
                }
            }
        }

        let bytes = match encode_for_version(protocol_version, &welcome) {
            Ok(bytes) if matches!(catchup, WireCatchupV2::Txns { .. }) => bytes,
            Ok(_) | Err(EncodeError::TooLarge) => {
                if let Some(task) = self.snapshot.take() {
                    self.machine.mark_snapshot_written(task.await??);
                }
                let snapshot = self.store.write_snapshot(&self.machine).await?;
                self.metrics.inc_snapshots_written(1);
                if let Ok(pruned) = self
                    .store
                    .prune_snapshots_branch(self.doc, self.branch_id, DEFAULT_SNAPSHOT_KEEP_RECENT)
                    .await
                    && pruned > 0
                {
                    self.metrics.inc_snapshots_pruned(pruned);
                }
                self.machine.mark_snapshot_written(snapshot.seq());
                let url = if self.branch_id == MAIN_BRANCH {
                    format!("/api/workbooks/{}/snapshots/{}", self.doc, snapshot.seq())
                } else {
                    format!(
                        "/api/workbooks/{}/branches/{}/snapshots/{}",
                        self.doc,
                        self.branch_id,
                        snapshot.seq()
                    )
                };
                if let WireSyncFrameV2::Welcome {
                    ref mut catchup, ..
                } = welcome
                {
                    *catchup = WireCatchupV2::SnapshotRequired {
                        snapshot_seq: snapshot.seq(),
                        url,
                    };
                }
                match encode_for_version(protocol_version, &welcome) {
                    Ok(bytes) => bytes,
                    Err(EncodeError::UnsupportedInV1) => {
                        let _ = reply.send(Err(RoomError::Rejected(
                            WireRejectReasonV1::UpgradeRequired,
                        )));
                        return Ok(());
                    }
                    Err(EncodeError::TooLarge | EncodeError::Failed) => {
                        return Err(StoreError::Invalid("handshake encoding"));
                    }
                }
            }
            Err(EncodeError::UnsupportedInV1) => {
                let _ = reply.send(Err(RoomError::Rejected(
                    WireRejectReasonV1::UpgradeRequired,
                )));
                return Ok(());
            }
            Err(EncodeError::Failed) => return Err(StoreError::Invalid("handshake encoding")),
        };

        let ws_kind = if share_link_id.is_some() {
            WsConnKind::Anon
        } else if role.can_write() {
            WsConnKind::Authed
        } else {
            WsConnKind::Viewer
        };
        let ws_guard = self.metrics.track_ws(ws_kind);

        self.next_member += 1;
        let id = self.next_member;
        if output.send(bytes) && reply.send(Ok(id)).is_ok() {
            self.members.insert(
                id,
                Member {
                    user,
                    role,
                    replica,
                    protocol_version,
                    display_name,
                    share_link_id,
                    last_presence: None,
                    output,
                    ws_guard,
                },
            );
        }
        Ok(())
    }

    async fn server_commit(
        &mut self,
        user: Uuid,
        replica: u32,
        client_seq: u64,
        label: String,
        ops: Vec<Op<BoundFormula>>,
    ) -> Result<u64, RoomError> {
        if self.store.disk_over_hard_limit() {
            return Err(RoomError::Rejected(WireRejectReasonV1::QuotaExceeded));
        }
        self.flush().await.map_err(|_| RoomError::Unavailable)?;
        self.machine
            .register_external_replica(replica, user.into_bytes());
        let wire_txn = ops_to_wire_txn_v3(label, &ops)
            .map_err(|_| RoomError::Rejected(WireRejectReasonV1::InvalidOp))?;
        let base_seq = self.machine.head_seq();
        let ts = now_ms();
        let pending = WirePendingTxnV3 {
            replica,
            client_seq,
            base_server_seq: base_seq,
            timestamp_ms: ts,
            txn: wire_txn,
        };
        match self.machine.validate_and_try_apply(
            user.into_bytes(),
            Some(MemberRole::Editor),
            replica,
            pending,
            ts,
        ) {
            Ok(TryApplyOutcome::Staged(staged)) => {
                let seq = staged.committed.seq;
                self.bytes += staged.payload_bytes;
                self.batch.push(QueuedTxn {
                    staged,
                    base_seq,
                    author: None,
                });
                if let Err(err) = self.flush().await {
                    self.rollback_pending_batch();
                    tracing::error!(
                        workbook_id = %self.doc,
                        branch_id = %self.branch_id,
                        %err,
                        "server commit flush failed"
                    );
                    return Err(RoomError::Unavailable);
                }
                Ok(seq)
            }
            Ok(TryApplyOutcome::DuplicateAck { seq, .. }) => Ok(seq),
            Err(reason) => Err(RoomError::Rejected(reason)),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "three-way merge compares base, draft, and live main inside the authoritative actor"
    )]
    async fn apply_merge(
        &mut self,
        user: Uuid,
        role: MemberRole,
        base: Workbook<BoundFormula>,
        draft: Workbook<BoundFormula>,
        fork_seq: u64,
        draft_txns: Vec<WireCommittedTxnV3>,
        selection: MergeSelection,
        label: String,
    ) -> MergeCommitOutcome {
        if !role.can_write() {
            return MergeCommitOutcome::Rejected(WireRejectReasonV1::Forbidden);
        }
        if self.store.disk_over_hard_limit() {
            return MergeCommitOutcome::DiskFull;
        }
        if let Err(err) = self.flush().await {
            return MergeCommitOutcome::Storage(err);
        }

        let main_head_seq = self.machine.head_seq();
        let plan = compute_branch_diff(
            &base,
            &draft,
            self.machine.workbook(),
            fork_seq,
            main_head_seq,
            &draft_txns,
        );
        let known_hunk_ids: std::collections::HashSet<&str> =
            plan.diff.hunks.iter().map(|h| h.id.as_str()).collect();
        let stale_hunk_selection = selection
            .accepted_hunks
            .iter()
            .any(|id| !known_hunk_ids.contains(id.as_str()));
        let main_advanced = selection
            .expected_main_seq
            .as_deref()
            .is_some_and(|exp| exp != plan.diff.main_head_seq);
        let unaddressed_conflicts: Vec<String> = plan
            .diff
            .hunks
            .iter()
            .flat_map(|h| h.cells.iter())
            .filter(|c| c.conflict && !selection.conflict_choices.contains_key(&c.cell_key))
            .map(|c| c.cell_key.clone())
            .collect();
        if stale_hunk_selection || (main_advanced && !unaddressed_conflicts.is_empty()) {
            let preview = preview_merge(&plan, &selection);
            let unresolved = if !unaddressed_conflicts.is_empty() {
                unaddressed_conflicts
            } else {
                preview.unresolved_conflicts
            };
            return MergeCommitOutcome::Conflict {
                code: "UNRESOLVED_CONFLICTS",
                message: "Main changed while you were reviewing; review the updated diff and resolve any conflicts.".to_owned(),
                diff: plan.diff,
                closure_issues: preview.closure_issues,
                unresolved_conflicts: unresolved,
            };
        }
        let preview = preview_merge(&plan, &selection);
        if !preview.unresolved_conflicts.is_empty() {
            return MergeCommitOutcome::Conflict {
                code: "UNRESOLVED_CONFLICTS",
                message: format!(
                    "{} conflict cell(s) require an explicit Keep Main or Use AI Draft choice",
                    preview.unresolved_conflicts.len()
                ),
                diff: plan.diff,
                closure_issues: preview.closure_issues,
                unresolved_conflicts: preview.unresolved_conflicts,
            };
        }
        if !preview.closure_issues.is_empty() && !selection.allow_ref_errors {
            return MergeCommitOutcome::Conflict {
                code: "CLOSURE_BLOCKED",
                message: format!(
                    "{} formula reference(s) depend on rejected structural inserts",
                    preview.closure_issues.len()
                ),
                diff: plan.diff,
                closure_issues: preview.closure_issues,
                unresolved_conflicts: preview.unresolved_conflicts,
            };
        }

        let merge_replica = match self
            .store
            .allocate_replica(self.doc, self.machine.epoch(), user)
            .await
        {
            Ok(rep) => rep,
            Err(err) => return MergeCommitOutcome::Storage(err),
        };
        let shards = match plan_merge_ops(
            &plan,
            &selection,
            self.machine.workbook(),
            self.machine.epoch(),
            merge_replica,
            1,
        ) {
            Ok(shards) => shards,
            Err(MergePlanError::ClosureBlocked(issues)) => {
                return MergeCommitOutcome::Conflict {
                    code: "CLOSURE_BLOCKED",
                    message: format!(
                        "{} formula reference(s) depend on rejected structural inserts",
                        issues.len()
                    ),
                    diff: plan.diff,
                    closure_issues: issues,
                    unresolved_conflicts: Vec::new(),
                };
            }
            Err(MergePlanError::UnresolvedConflicts(conflicts)) => {
                return MergeCommitOutcome::Conflict {
                    code: "UNRESOLVED_CONFLICTS",
                    message: format!(
                        "{} conflict cell(s) require an explicit resolution",
                        conflicts.len()
                    ),
                    diff: plan.diff,
                    closure_issues: Vec::new(),
                    unresolved_conflicts: conflicts,
                };
            }
            Err(MergePlanError::RemapFailed(msg)) => return MergeCommitOutcome::RemapFailed(msg),
        };
        if shards.is_empty() {
            return MergeCommitOutcome::Merged {
                merge_seq: self.machine.head_seq(),
                committed_txns: 0,
                committed_ops: 0,
            };
        }

        self.machine
            .register_external_replica(merge_replica, user.into_bytes());
        let mut client_seq = 1_u64;
        let mut total_ops = 0_usize;
        let shard_count = shards.len();
        for (idx, ops) in shards.into_iter().enumerate() {
            total_ops += ops.len();
            let shard_label = if shard_count == 1 {
                label.clone()
            } else {
                format!("{label} ({}/{shard_count})", idx + 1)
            };
            let Ok(wire_txn) = ops_to_wire_txn_v3(shard_label, &ops) else {
                self.rollback_pending_batch();
                return MergeCommitOutcome::Rejected(WireRejectReasonV1::InvalidOp);
            };
            let base_seq = self.machine.head_seq();
            let ts = now_ms();
            let pending = WirePendingTxnV3 {
                replica: merge_replica,
                client_seq,
                base_server_seq: base_seq,
                timestamp_ms: ts,
                txn: wire_txn,
            };
            match self.machine.validate_and_try_apply(
                user.into_bytes(),
                Some(role),
                merge_replica,
                pending,
                ts,
            ) {
                Ok(TryApplyOutcome::Staged(staged)) => {
                    self.bytes += staged.payload_bytes;
                    self.batch.push(QueuedTxn {
                        staged,
                        base_seq,
                        author: None,
                    });
                    client_seq += 1;
                }
                Ok(TryApplyOutcome::DuplicateAck { .. }) => {
                    client_seq += 1;
                }
                Err(reason) => {
                    self.rollback_pending_batch();
                    return MergeCommitOutcome::Rejected(reason);
                }
            }
        }

        if let Err(err) = self.flush().await {
            self.rollback_pending_batch();
            return MergeCommitOutcome::Storage(err);
        }

        MergeCommitOutcome::Merged {
            merge_seq: self.machine.head_seq(),
            committed_txns: shard_count,
            committed_ops: total_ops,
        }
    }

    fn rollback_pending_batch(&mut self) {
        let pending = std::mem::take(&mut self.batch);
        for item in pending.into_iter().rev() {
            self.machine.rollback_staged(item.staged);
        }
        self.bytes = 0;
        self.flush_at = None;
    }

    fn update_role(&mut self, user: Uuid, next_role: Option<MemberRole>) {
        let mut to_close = Vec::new();
        for (id, member) in &mut self.members {
            if member.user == user && member.share_link_id.is_none() {
                match next_role {
                    Some(role) => {
                        let effective = if self.is_ai_draft {
                            MemberRole::Viewer
                        } else {
                            role
                        };
                        member.role = effective;
                        let kind = if effective.can_write() {
                            WsConnKind::Authed
                        } else {
                            WsConnKind::Viewer
                        };
                        self.metrics.set_ws_kind(&mut member.ws_guard, kind);
                    }
                    None => to_close.push(*id),
                }
            }
        }
        for id in to_close {
            if let Some(member) = self.members.remove(&id) {
                member
                    .output
                    .send_close(CLOSE_ACCESS_REVOKED, "Access revoked");
            }
        }
    }

    fn revoke_share_token(&mut self, link_id: Uuid) {
        let to_close: Vec<u64> = self
            .members
            .iter()
            .filter(|(_, m)| m.share_link_id == Some(link_id))
            .map(|(id, _)| *id)
            .collect();
        for id in to_close {
            if let Some(member) = self.members.remove(&id) {
                member
                    .output
                    .send_close(CLOSE_ACCESS_REVOKED, "Access revoked");
            }
        }
    }

    fn kick_all_anonymous(&mut self) {
        let to_close: Vec<u64> = self
            .members
            .iter()
            .filter(|(_, m)| m.share_link_id.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in to_close {
            if let Some(member) = self.members.remove(&id) {
                member
                    .output
                    .send_close(CLOSE_ACCESS_REVOKED, "Access revoked");
            }
        }
    }

    fn presence(&mut self, id: u64, mut update: WirePresenceV1) -> Result<(), StoreError> {
        let Some(member) = self.members.get_mut(&id) else {
            return Ok(());
        };
        if !member.role.can_write() || member.replica == VIEWER_REPLICA {
            return Ok(());
        }
        if update.replica != member.replica {
            return self.send(
                id,
                &WireSyncFrameV2::Reject {
                    client_seq: 0,
                    reason: WireRejectReasonV1::Forbidden,
                },
            );
        }
        let now = Instant::now();
        if member
            .last_presence
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(50))
        {
            return Ok(());
        }
        member.last_presence = Some(now);
        update.user_id = member.user.into_bytes();
        update.display_name.clone_from(&member.display_name);
        update.color_slot = (member.replica % 8) as u8;
        self.broadcast_filtered(Some(id), &WireSyncFrameV2::Presence(update))
    }

    async fn push(
        &mut self,
        id: u64,
        txns: Vec<WirePendingTxnV3>,
        wire_bytes: usize,
    ) -> Result<(), StoreError> {
        let Some(member) = self.members.get(&id) else {
            return Ok(());
        };
        let (user, role, replica) = (member.user, member.role, member.replica);
        if !role.can_write() || replica == VIEWER_REPLICA {
            self.metrics
                .inc_rejected_txns(u64::try_from(txns.len().max(1)).unwrap_or(1));
            self.flush().await?;
            return self.send(
                id,
                &WireSyncFrameV2::Reject {
                    client_seq: txns.first().map_or(0, |txn| txn.client_seq),
                    reason: WireRejectReasonV1::Forbidden,
                },
            );
        }
        if self.store.disk_over_hard_limit() {
            self.metrics
                .inc_rejected_txns(u64::try_from(txns.len().max(1)).unwrap_or(1));
            self.flush().await?;
            return self.send(
                id,
                &WireSyncFrameV2::Reject {
                    client_seq: txns.first().map_or(0, |txn| txn.client_seq),
                    reason: WireRejectReasonV1::QuotaExceeded,
                },
            );
        }
        if let Err(reason) = self.machine.validate_push_batch(&txns, wire_bytes) {
            self.metrics
                .inc_rejected_txns(u64::try_from(txns.len().max(1)).unwrap_or(1));
            self.flush().await?;
            return self.send(
                id,
                &WireSyncFrameV2::Reject {
                    client_seq: txns.first().map_or(0, |txn| txn.client_seq),
                    reason,
                },
            );
        }
        let durable_head = self.machine.head_seq() - self.batch.len() as u64;
        for txn in txns {
            let base_seq = txn.base_server_seq;
            let client_seq = txn.client_seq;
            let result = if base_seq <= durable_head {
                self.machine.validate_and_try_apply(
                    user.into_bytes(),
                    Some(role),
                    replica,
                    txn,
                    now_ms(),
                )
            } else {
                Err(WireRejectReasonV1::InvalidOp)
            };
            match result {
                Ok(TryApplyOutcome::Staged(staged)) => {
                    self.flush_at
                        .get_or_insert_with(|| Instant::now() + BATCH_WINDOW);
                    self.bytes += staged.payload_bytes;
                    self.batch.push(QueuedTxn {
                        staged,
                        base_seq,
                        author: Some(id),
                    });
                    if self.batch.len() == BATCH_COUNT
                        || self.bytes >= BATCH_BYTES
                        || self.flush_at.is_some_and(|at| at <= Instant::now())
                    {
                        self.flush().await?;
                    }
                }
                Ok(TryApplyOutcome::DuplicateAck {
                    seq, timestamp_ms, ..
                }) => {
                    self.flush().await?;
                    let (seq, timestamp_ms) = if timestamp_ms == 0 {
                        self.store
                            .receipt_branch(self.doc, self.branch_id, replica, client_seq)
                            .await?
                    } else {
                        (seq, timestamp_ms)
                    };
                    self.send(
                        id,
                        &WireSyncFrameV2::Ack {
                            replica,
                            client_seq,
                            seq,
                            timestamp_ms,
                        },
                    )?;
                }
                Err(reason) => {
                    self.metrics.inc_rejected_txns(1);
                    self.flush().await?;
                    self.send(id, &WireSyncFrameV2::Reject { client_seq, reason })?;
                    break;
                }
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), StoreError> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let head = self.machine.head_seq();
        let batch_len = self.batch.len() as u64;
        let previous = head - batch_len;
        let batch = self
            .batch
            .iter()
            .map(|item| BatchEntry {
                staged: &item.staged,
                base_seq: item.base_seq,
            })
            .collect::<Vec<_>>();
        let est_bytes = self.machine.workbook().estimated_bytes();
        let started = Instant::now();
        self.store
            .commit_batch_branch(
                self.doc,
                self.branch_id,
                self.machine.epoch(),
                &batch,
                est_bytes,
            )
            .await?;
        let elapsed = started.elapsed();
        self.metrics.record_commit_batch(batch_len, elapsed);
        if elapsed > SLOW_FLUSH_WARN {
            tracing::warn!(
                workbook_id = %self.doc,
                branch_id = %self.branch_id,
                batch_txns = batch_len,
                elapsed_ms = elapsed.as_millis() as u64,
                "slow PostgreSQL commit batch flush"
            );
        }
        self.slot_est_bytes
            .store((est_bytes as u64).max(MIN_ROOM_BYTES), Ordering::Relaxed);
        {
            let rooms = self.registry.lock().await;
            sync_room_metrics(&rooms, &self.extra_budget_bytes, &self.metrics);
        }
        self.bytes = 0;
        self.flush_at = None;
        self.quiet_at = Some(Instant::now() + QUIET_DIGEST);
        for entry in std::mem::take(&mut self.batch) {
            let commit = self.machine.commit_staged(entry.staged);
            let (seq, replica, client_seq, timestamp_ms) = (
                commit.seq,
                commit.replica,
                commit.client_seq,
                commit.timestamp_ms,
            );
            if let Some(author) = entry.author {
                self.send(
                    author,
                    &WireSyncFrameV2::Ack {
                        replica,
                        client_seq,
                        seq,
                        timestamp_ms,
                    },
                )?;
            }
            self.broadcast(&WireSyncFrameV2::Commit {
                seq,
                replica,
                client_seq,
                timestamp_ms,
                user_id: commit.user_id,
                txn: commit.txn,
            })?;
        }
        if head / DIGEST_INTERVAL != previous / DIGEST_INTERVAL {
            self.broadcast(&self.machine.digest_frame())?;
        }
        Ok(())
    }

    fn send(&mut self, id: u64, frame: &WireSyncFrameV2) -> Result<(), StoreError> {
        let Some(proto) = self.members.get(&id).map(|m| m.protocol_version) else {
            return Ok(());
        };
        match encode_for_version(proto, frame) {
            Ok(bytes) => {
                if let Some(member) = self.members.get(&id)
                    && !member.output.send(bytes)
                {
                    self.members.remove(&id);
                }
                Ok(())
            }
            Err(EncodeError::UnsupportedInV1) => {
                if let Some(mut member) = self.members.remove(&id) {
                    notify_upgrade_required(&mut member);
                }
                Ok(())
            }
            Err(EncodeError::TooLarge | EncodeError::Failed) => {
                Err(StoreError::Invalid("outbound frame encoding"))
            }
        }
    }

    fn broadcast(&mut self, frame: &WireSyncFrameV2) -> Result<(), StoreError> {
        self.broadcast_filtered(None, frame)
    }

    fn broadcast_filtered(
        &mut self,
        skip_id: Option<u64>,
        frame: &WireSyncFrameV2,
    ) -> Result<(), StoreError> {
        let mut encoded_v1: Option<Result<Arc<[u8]>, EncodeError>> = None;
        let mut encoded_v2: Option<Result<Arc<[u8]>, EncodeError>> = None;
        let mut encode_failed = false;

        self.members.retain(|key, member| {
            if skip_id == Some(*key) {
                return true;
            }
            let slot = if member.protocol_version == SYNC_PROTOCOL_VERSION {
                &mut encoded_v1
            } else {
                &mut encoded_v2
            };
            let result =
                slot.get_or_insert_with(|| encode_for_version(member.protocol_version, frame));
            match result {
                Ok(bytes) => member.output.send(Arc::clone(bytes)),
                Err(EncodeError::UnsupportedInV1) => {
                    notify_upgrade_required(member);
                    false
                }
                Err(EncodeError::TooLarge | EncodeError::Failed) => {
                    encode_failed = true;
                    true
                }
            }
        });

        if encode_failed {
            return Err(StoreError::Invalid("outbound frame encoding"));
        }
        Ok(())
    }

    fn schedule_snapshot(&mut self) {
        if self.batch.is_empty() && self.machine.needs_snapshot() && self.snapshot.is_none() {
            let capture = (&self.machine).into();
            let store = self.store.clone();
            let doc = self.doc;
            let branch_id = self.branch_id;
            let metrics = Arc::clone(&self.metrics);
            self.snapshot = Some(tokio::spawn(async move {
                let seq = store.persist_snapshot(capture).await?.seq();
                metrics.inc_snapshots_written(1);
                if let Ok(pruned) = store
                    .prune_snapshots_branch(doc, branch_id, DEFAULT_SNAPSHOT_KEEP_RECENT)
                    .await
                    && pruned > 0
                {
                    metrics.inc_snapshots_pruned(pruned);
                }
                Ok(seq)
            }));
        }
    }
}

fn sync_room_metrics(
    rooms: &HashMap<RoomKey, RoomSlot>,
    extra_budget_bytes: &AtomicU64,
    metrics: &Metrics,
) {
    let active_rooms = rooms.len() as u64;
    let draft_rooms = rooms.keys().filter(|(_, br)| *br != MAIN_BRANCH).count() as u64;
    let room_bytes: u64 = rooms
        .values()
        .map(|slot| slot.est_bytes.load(Ordering::Relaxed))
        .sum();
    let loaded_bytes = room_bytes.saturating_add(extra_budget_bytes.load(Ordering::Relaxed));
    metrics.set_rooms_with_drafts(active_rooms, draft_rooms, loaded_bytes);
}

fn now_since_ms(epoch_start: Instant) -> u64 {
    u64::try_from(epoch_start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

enum EncodeError {
    TooLarge,
    UnsupportedInV1,
    Failed,
}

fn encode_for_version(version: u8, frame: &WireSyncFrameV2) -> Result<Arc<[u8]>, EncodeError> {
    let encoded = if version == SYNC_PROTOCOL_VERSION_V2 {
        encode_sync_frame_v2(frame)
    } else if version == SYNC_PROTOCOL_VERSION {
        let v1 = WireSyncFrameV1::try_from(frame).map_err(|err| match err {
            WireConvertError::UnsupportedInV2 => EncodeError::UnsupportedInV1,
            _ => EncodeError::Failed,
        })?;
        encode_sync_frame(&v1)
    } else {
        return Err(EncodeError::UnsupportedInV1);
    };
    encoded.map(Into::into).map_err(|err| match err {
        SyncFrameError::FrameTooLarge { .. } => EncodeError::TooLarge,
        _ => EncodeError::Failed,
    })
}

fn notify_upgrade_required(member: &mut Member) {
    member.output.graceful_drop = true;
    if let Ok(reject) = encode_for_version(
        SYNC_PROTOCOL_VERSION,
        &WireSyncFrameV2::Reject {
            client_seq: 0,
            reason: WireRejectReasonV1::UpgradeRequired,
        },
    ) {
        let _ = member.output.send(reject);
    }
    member.output.send_close(1008, "Protocol upgrade required");
}

async fn deadline(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => pending().await,
    }
}

async fn wait_snapshot(
    task: &mut Option<SnapshotTask>,
) -> Result<Result<u64, StoreError>, tokio::task::JoinError> {
    match task {
        Some(task) => task.await,
        None => pending().await,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_millis()).ok())
        .unwrap_or(0)
}
