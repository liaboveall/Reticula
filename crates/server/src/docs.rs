//! Document actor lifecycle, memory budget LRU eviction, and permission enforcement.

use std::collections::HashMap;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reticula_sync::{
    MemberRole, ServerDocMachine, ServerLimits, StagedTxn, TryApplyOutcome, VIEWER_REPLICA,
};
use reticula_wire::sync_frame::{SyncFrameError, encode_sync_frame};
use reticula_wire::v2::{
    WireCatchupV1, WirePendingTxnV2, WirePresenceV1, WireRejectReasonV1, WireSyncFrameV1,
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

#[derive(Debug, Error)]
pub enum RoomError {
    #[error("document actor is unavailable")]
    Unavailable,
    #[error("server document memory budget is saturated")]
    Busy,
    #[error("handshake rejected: {0:?}")]
    Rejected(WireRejectReasonV1),
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

type Registry = Mutex<HashMap<Uuid, RoomSlot>>;

#[derive(Clone)]
pub struct DocRooms {
    store: Store,
    rooms: Arc<Registry>,
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
            max_loaded_bytes,
            metrics,
            epoch_start: Instant::now(),
        }
    }

    /// `user` and `role` are supplied by the authenticated transport.
    pub async fn join(
        &self,
        doc: Uuid,
        user: Uuid,
        role: MemberRole,
        display_name: String,
        hello: WireSyncFrameV1,
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
        hello: WireSyncFrameV1,
    ) -> Result<RoomConnection, RoomError> {
        let lease = self.acquire_room_lease(doc).await?;
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
            actor,
            receiver,
            budget,
            active,
            last_used_ms,
            epoch_start: self.epoch_start,
        })
    }

    async fn room_sender_or_dead(&self, doc: &Uuid) -> Option<mpsc::Sender<Command>> {
        let rooms = self.rooms.lock().await;
        rooms.get(doc).map(|slot| slot.sender.clone())
    }

    pub async fn would_be_busy(&self, doc: Uuid) -> bool {
        let rooms = self.rooms.lock().await;
        if rooms
            .get(&doc)
            .is_some_and(|slot| !slot.shutting_down && !slot.sender.is_closed())
        {
            return false;
        }
        let Ok(est) = self.store.workbook_est_bytes(doc).await else {
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
        active_bytes.saturating_add(needed) > self.max_loaded_bytes
    }

    pub async fn update_member_role(&self, doc: Uuid, user: Uuid, role: Option<MemberRole>) {
        if let Some(sender) = self.room_sender_or_dead(&doc).await {
            let _ = sender.send(Command::UpdateRole { user, role }).await;
        }
    }

    pub async fn revoke_share_link(&self, doc: Uuid, link_id: Uuid) {
        if let Some(sender) = self.room_sender_or_dead(&doc).await {
            let _ = sender.send(Command::RevokeShareToken { link_id }).await;
        }
    }

    pub async fn kick_all_anonymous(&self, doc: Uuid) {
        if let Some(sender) = self.room_sender_or_dead(&doc).await {
            let _ = sender.send(Command::KickAllAnonymous).await;
        }
    }

    pub async fn shutdown_room(&self, doc: Uuid) {
        let sender = {
            let mut rooms = self.rooms.lock().await;
            let Some(slot) = rooms.get_mut(&doc) else {
                return;
            };
            slot.shutting_down = true;
            slot.sender.clone()
        };
        let (tx, rx) = oneshot::channel();
        if sender.send(Command::Shutdown { ack: tx }).await.is_ok() {
            let _ = tokio::time::timeout(SHUTDOWN_WAIT_LIMIT, rx).await;
        }
        let mut rooms = self.rooms.lock().await;
        if rooms
            .get(&doc)
            .is_some_and(|slot| slot.sender.same_channel(&sender))
        {
            rooms.remove(&doc);
        }
        sync_room_metrics(&rooms, &self.metrics);
    }

    async fn acquire_room_lease(&self, doc: Uuid) -> Result<RoomLease, RoomError> {
        let needed = self
            .store
            .workbook_est_bytes(doc)
            .await
            .map_err(|_| RoomError::Unavailable)?
            .max(MIN_ROOM_BYTES);

        let deadline = Instant::now() + EVICT_WAIT_BUDGET;
        loop {
            let eviction_targets: Vec<(Uuid, mpsc::Sender<Command>)>;
            {
                let mut rooms = self.rooms.lock().await;
                rooms.retain(|_, slot| !slot.sender.is_closed());
                if let Some(slot) = rooms.get_mut(&doc)
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
                if total_loaded.saturating_add(needed) <= self.max_loaded_bytes {
                    let est_bytes = Arc::new(AtomicU64::new(needed));
                    let active = Arc::new(AtomicUsize::new(1));
                    let last_used_ms = Arc::new(AtomicU64::new(now_since_ms(self.epoch_start)));
                    let sender = self.spawn_room(
                        doc,
                        &est_bytes,
                        Arc::clone(&active),
                        Arc::clone(&last_used_ms),
                        &mut rooms,
                    );
                    sync_room_metrics(&rooms, &self.metrics);
                    return Ok(RoomLease {
                        sender,
                        active,
                        last_used_ms,
                        epoch_start: self.epoch_start,
                    });
                }

                let mut idle: Vec<(u64, Uuid, u64)> = rooms
                    .iter()
                    .filter(|(id, slot)| {
                        **id != doc
                            && !slot.shutting_down
                            && slot.active.load(Ordering::Acquire) == 0
                    })
                    .map(|(id, slot)| {
                        (
                            slot.last_used_ms.load(Ordering::Relaxed),
                            *id,
                            slot.est_bytes.load(Ordering::Relaxed),
                        )
                    })
                    .collect();
                idle.sort_by_key(|(ts, id, _)| (*ts, *id));

                let mut projected = total_loaded;
                let mut chosen = Vec::new();
                for (_, cand_id, cand_bytes) in idle {
                    if projected.saturating_add(needed) <= self.max_loaded_bytes {
                        break;
                    }
                    if let Some(slot) = rooms.get_mut(&cand_id) {
                        slot.shutting_down = true;
                        projected = projected.saturating_sub(cand_bytes);
                        chosen.push((cand_id, slot.sender.clone()));
                    }
                }
                eviction_targets = chosen;
            }

            if !eviction_targets.is_empty() {
                for (cand_id, sender) in eviction_targets {
                    tracing::info!(workbook_id = %cand_id, "evicting idle LRU document actor");
                    let (tx, rx) = oneshot::channel();
                    if sender.send(Command::Shutdown { ack: tx }).await.is_ok() {
                        let _ = tokio::time::timeout(SHUTDOWN_WAIT_LIMIT, rx).await;
                    }
                    let mut rooms = self.rooms.lock().await;
                    if rooms
                        .get(&cand_id)
                        .is_some_and(|slot| slot.sender.same_channel(&sender))
                    {
                        rooms.remove(&cand_id);
                    }
                    sync_room_metrics(&rooms, &self.metrics);
                }
                continue;
            }

            if Instant::now() >= deadline {
                tracing::warn!(
                    workbook_id = %doc,
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
        slot_est_bytes: &Arc<AtomicU64>,
        slot_active: Arc<AtomicUsize>,
        last_used_ms: Arc<AtomicU64>,
        rooms: &mut HashMap<Uuid, RoomSlot>,
    ) -> mpsc::Sender<Command> {
        let (sender, receiver) = mpsc::channel(64);
        rooms.insert(
            doc,
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
        let est_ref = Arc::clone(slot_est_bytes);
        let task = tokio::spawn(async move {
            DocActor::run(
                store,
                doc,
                receiver,
                metrics,
                all_rooms,
                est_ref,
                slot_active,
            )
            .await
        });
        let channel = sender.clone();
        let registry = Arc::downgrade(&self.rooms);
        let metrics = Arc::clone(&self.metrics);
        tokio::spawn(async move {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(workbook_id = %doc, %error, "document actor stopped with error");
                }
                Err(error) => {
                    tracing::error!(workbook_id = %doc, %error, "document actor panicked");
                }
            }
            if let Some(registry) = registry.upgrade() {
                let mut rooms = registry.lock().await;
                if rooms
                    .get(&doc)
                    .is_some_and(|slot| slot.sender.same_channel(&channel))
                {
                    rooms.remove(&doc);
                }
                sync_room_metrics(&rooms, &metrics);
            }
        });
        sender
    }
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
    pub async fn push(
        &self,
        txns: Vec<WirePendingTxnV2>,
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
        self.budget.close();
    }
}

enum Command {
    Join {
        user: Uuid,
        role: MemberRole,
        display_name: String,
        share_link_id: Option<Uuid>,
        hello: WireSyncFrameV1,
        output: Output,
        reply: oneshot::Sender<Result<u64, RoomError>>,
    },
    Push {
        id: u64,
        txns: Vec<WirePendingTxnV2>,
        wire_bytes: usize,
    },
    Presence {
        id: u64,
        update: WirePresenceV1,
    },
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

struct Member {
    user: Uuid,
    role: MemberRole,
    replica: u32,
    display_name: String,
    share_link_id: Option<Uuid>,
    last_presence: Option<Instant>,
    output: Output,
    ws_guard: WsConnGuard,
}

struct QueuedTxn {
    staged: StagedTxn,
    base_seq: u64,
    author: u64,
}

type SnapshotTask = JoinHandle<Result<u64, StoreError>>;

struct DocActor {
    store: Store,
    doc: Uuid,
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
    slot_est_bytes: Arc<AtomicU64>,
    slot_active: Arc<AtomicUsize>,
}

impl DocActor {
    async fn run(
        store: Store,
        doc: Uuid,
        mut commands: mpsc::Receiver<Command>,
        metrics: Arc<Metrics>,
        registry: Arc<Registry>,
        slot_est_bytes: Arc<AtomicU64>,
        slot_active: Arc<AtomicUsize>,
    ) -> Result<(), StoreError> {
        let machine = store.load_machine(doc).await?;
        let initial_est = (machine.workbook().estimated_bytes() as u64).max(MIN_ROOM_BYTES);
        slot_est_bytes.store(initial_est, Ordering::Relaxed);
        {
            let rooms = registry.lock().await;
            sync_room_metrics(&rooms, &metrics);
        }
        if store.snapshot(doc, 0).await?.is_none() {
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
        role: MemberRole,
        display_name: String,
        share_link_id: Option<Uuid>,
        hello: &WireSyncFrameV1,
        output: Output,
        reply: oneshot::Sender<Result<u64, RoomError>>,
    ) -> Result<(), StoreError> {
        let next_replica = self.machine.next_per_epoch_replica();
        let mut welcome = match self.machine.on_hello(user.into_bytes(), Some(role), hello) {
            Ok(welcome) => welcome,
            Err(reason) => {
                let _ = reply.send(Err(RoomError::Rejected(reason)));
                return Ok(());
            }
        };
        let WireSyncFrameV1::Welcome {
            replica,
            ref catchup,
            ..
        } = welcome
        else {
            return Err(StoreError::Invalid("invalid handshake result"));
        };
        if self.machine.next_per_epoch_replica() != next_replica {
            self.store.register_replica(self.doc, user, replica).await?;
        }

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
                    tracing::info!(workbook_id = %self.doc, replica, "closing superseded replica connection");
                    old_member
                        .output
                        .send_close(CLOSE_DUPLICATE_REPLICA, "Replaced by newer session");
                }
            }
        }

        let bytes = match encode_sync_frame(&welcome) {
            Ok(bytes) if matches!(catchup, WireCatchupV1::Txns { .. }) => bytes.into(),
            Ok(_) | Err(SyncFrameError::FrameTooLarge { .. }) => {
                if let Some(task) = self.snapshot.take() {
                    self.machine.mark_snapshot_written(task.await??);
                }
                let snapshot = self.store.write_snapshot(&self.machine).await?;
                self.metrics.inc_snapshots_written(1);
                if let Ok(pruned) = self
                    .store
                    .prune_snapshots(self.doc, DEFAULT_SNAPSHOT_KEEP_RECENT)
                    .await
                    && pruned > 0
                {
                    self.metrics.inc_snapshots_pruned(pruned);
                }
                self.machine.mark_snapshot_written(snapshot.seq());
                if let WireSyncFrameV1::Welcome {
                    ref mut catchup, ..
                } = welcome
                {
                    *catchup = WireCatchupV1::SnapshotRequired {
                        snapshot_seq: snapshot.seq(),
                        url: format!("/api/workbooks/{}/snapshots/{}", self.doc, snapshot.seq()),
                    };
                }
                encode(&welcome)?
            }
            Err(_) => return Err(StoreError::Invalid("handshake encoding")),
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

    fn update_role(&mut self, user: Uuid, next_role: Option<MemberRole>) {
        let mut to_close = Vec::new();
        for (id, member) in &mut self.members {
            if member.user == user && member.share_link_id.is_none() {
                match next_role {
                    Some(role) => {
                        member.role = role;
                        let kind = if role.can_write() {
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
                &WireSyncFrameV1::Reject {
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
        let bytes = encode(&WireSyncFrameV1::Presence(update))?;
        self.members
            .retain(|key, member| *key == id || member.output.send(Arc::clone(&bytes)));
        Ok(())
    }

    async fn push(
        &mut self,
        id: u64,
        txns: Vec<WirePendingTxnV2>,
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
                &WireSyncFrameV1::Reject {
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
                &WireSyncFrameV1::Reject {
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
                &WireSyncFrameV1::Reject {
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
                        author: id,
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
                        self.store.receipt(self.doc, replica, client_seq).await?
                    } else {
                        (seq, timestamp_ms)
                    };
                    self.send(
                        id,
                        &WireSyncFrameV1::Ack {
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
                    self.send(id, &WireSyncFrameV1::Reject { client_seq, reason })?;
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
            .commit_batch(self.doc, self.machine.epoch(), &batch, est_bytes)
            .await?;
        let elapsed = started.elapsed();
        self.metrics.record_commit_batch(batch_len, elapsed);
        if elapsed > SLOW_FLUSH_WARN {
            tracing::warn!(
                workbook_id = %self.doc,
                batch_txns = batch_len,
                elapsed_ms = elapsed.as_millis() as u64,
                "slow PostgreSQL commit batch flush"
            );
        }
        self.slot_est_bytes
            .store((est_bytes as u64).max(MIN_ROOM_BYTES), Ordering::Relaxed);
        {
            let rooms = self.registry.lock().await;
            sync_room_metrics(&rooms, &self.metrics);
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
            self.send(
                entry.author,
                &WireSyncFrameV1::Ack {
                    replica,
                    client_seq,
                    seq,
                    timestamp_ms,
                },
            )?;
            self.broadcast(&WireSyncFrameV1::Commit {
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

    fn send(&mut self, id: u64, frame: &WireSyncFrameV1) -> Result<(), StoreError> {
        if let Some(member) = self.members.get(&id)
            && !member.output.send(encode(frame)?)
        {
            self.members.remove(&id);
        }
        Ok(())
    }

    fn broadcast(&mut self, frame: &WireSyncFrameV1) -> Result<(), StoreError> {
        let bytes = encode(frame)?;
        self.members
            .retain(|_, member| member.output.send(Arc::clone(&bytes)));
        Ok(())
    }

    fn schedule_snapshot(&mut self) {
        if self.batch.is_empty() && self.machine.needs_snapshot() && self.snapshot.is_none() {
            let capture = (&self.machine).into();
            let store = self.store.clone();
            let doc = self.doc;
            let metrics = Arc::clone(&self.metrics);
            self.snapshot = Some(tokio::spawn(async move {
                let seq = store.persist_snapshot(capture).await?.seq();
                metrics.inc_snapshots_written(1);
                if let Ok(pruned) = store
                    .prune_snapshots(doc, DEFAULT_SNAPSHOT_KEEP_RECENT)
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

fn sync_room_metrics(rooms: &HashMap<Uuid, RoomSlot>, metrics: &Metrics) {
    let active_rooms = rooms.len() as u64;
    let loaded_bytes: u64 = rooms
        .values()
        .map(|slot| slot.est_bytes.load(Ordering::Relaxed))
        .sum();
    metrics.set_rooms(active_rooms, loaded_bytes);
}

fn now_since_ms(epoch_start: Instant) -> u64 {
    u64::try_from(epoch_start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn encode(frame: &WireSyncFrameV1) -> Result<Arc<[u8]>, StoreError> {
    encode_sync_frame(frame)
        .map(Into::into)
        .map_err(|_| StoreError::Invalid("outbound frame encoding"))
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
