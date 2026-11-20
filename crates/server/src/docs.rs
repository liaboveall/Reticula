//! Serializes document changes and emits acknowledgments only after a durable commit.

use std::collections::HashMap;
use std::future::pending;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reticula_sync::{MemberRole, ServerDocMachine, ServerLimits, StagedTxn, TryApplyOutcome};
use reticula_wire::sync_frame::{SyncFrameError, encode_sync_frame};
use reticula_wire::v2::{
    WireCatchupV1, WirePendingTxnV2, WirePresenceV1, WireRejectReasonV1, WireSyncFrameV1,
};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use uuid::Uuid;

use crate::store::{BatchEntry, MAIN_BRANCH, Store, StoreError};

const BATCH_WINDOW: Duration = Duration::from_millis(5);
const BATCH_COUNT: usize = 32;
const BATCH_BYTES: usize = 256 * 1024;
const DIGEST_INTERVAL: u64 = 50;
const QUIET_DIGEST: Duration = Duration::from_secs(5);
const IDLE_CHECK: Duration = Duration::from_secs(60);
const QUEUE_FRAMES: usize = 256;
const QUEUE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RoomError {
    #[error("document actor is unavailable")]
    Unavailable,
    #[error("handshake rejected: {0:?}")]
    Rejected(WireRejectReasonV1),
}

type Registry = Mutex<HashMap<Uuid, mpsc::Sender<Command>>>;

#[derive(Clone)]
pub struct DocRooms {
    store: Store,
    rooms: Arc<Registry>,
}

impl DocRooms {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            rooms: Arc::default(),
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
        let actor = self.room(doc).await;
        let (reply, ready) = oneshot::channel();
        let (sender, receiver) = mpsc::channel(QUEUE_FRAMES);
        let budget = Arc::new(Semaphore::new(QUEUE_BYTES));
        actor
            .send(Command::Join {
                user,
                role,
                display_name,
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
        Ok(RoomConnection {
            id,
            actor,
            receiver,
            budget,
        })
    }

    async fn room(&self, doc: Uuid) -> mpsc::Sender<Command> {
        let mut rooms = self.rooms.lock().await;
        if let Some(room) = rooms.get(&doc).filter(|room| !room.is_closed()) {
            return room.clone();
        }
        let (sender, receiver) = mpsc::channel(64);
        rooms.insert(doc, sender.clone());
        let store = self.store.clone();
        let task = tokio::spawn(async move { DocActor::run(store, doc, receiver).await });
        let channel = sender.clone();
        let registry = Arc::downgrade(&self.rooms);
        tokio::spawn(async move {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("document actor {doc} ended: {error}"),
                Err(error) => eprintln!("document actor {doc} terminated: {error}"),
            }
            if let Some(registry) = registry.upgrade() {
                let mut rooms = registry.lock().await;
                if rooms
                    .get(&doc)
                    .is_some_and(|room| room.same_channel(&channel))
                {
                    rooms.remove(&doc);
                }
            }
        });
        sender
    }
}

pub struct RoomConnection {
    id: u64,
    actor: mpsc::Sender<Command>,
    receiver: mpsc::Receiver<QueuedFrame>,
    budget: Arc<Semaphore>,
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
        let packet = self.receiver.recv().await?;
        (!self.budget.is_closed()).then_some(packet.bytes)
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

struct QueuedFrame {
    bytes: Arc<[u8]>,
    _permit: OwnedSemaphorePermit,
}

struct Output {
    sender: mpsc::Sender<QueuedFrame>,
    budget: Arc<Semaphore>,
}

impl Output {
    fn send(&self, bytes: Arc<[u8]>) -> bool {
        let Ok(permit) = Arc::clone(&self.budget).try_acquire_many_owned(bytes.len() as u32) else {
            return false;
        };
        self.sender
            .try_send(QueuedFrame {
                bytes,
                _permit: permit,
            })
            .is_ok()
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
}

struct Member {
    user: Uuid,
    role: MemberRole,
    replica: u32,
    display_name: String,
    last_presence: Option<Instant>,
    output: Output,
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
}

impl DocActor {
    async fn run(
        store: Store,
        doc: Uuid,
        mut commands: mpsc::Receiver<Command>,
    ) -> Result<(), StoreError> {
        let machine = store.load_machine(doc).await?;
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
                    if room.members.is_empty() && commands.is_empty() {
                        commands.close();
                    }
                    idle_at = Instant::now() + IDLE_CHECK;
                }
                command = commands.recv() => match command {
                    Some(Command::Join { user, role, display_name, hello, output, reply }) => {
                        room.flush().await?;
                        room.join(user, role, display_name, &hello, output, reply).await?;
                    }
                    Some(Command::Push { id, txns, wire_bytes }) => room.push(id, txns, wire_bytes).await?,
                    Some(Command::Presence { id, update }) => room.presence(id, update)?,
                    None => {
                        room.flush().await?;
                        return Ok(());
                    }
                },
            }
        }
    }

    async fn join(
        &mut self,
        user: Uuid,
        role: MemberRole,
        display_name: String,
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
        let bytes = match encode_sync_frame(&welcome) {
            Ok(bytes) if matches!(catchup, WireCatchupV1::Txns { .. }) => bytes.into(),
            Ok(_) | Err(SyncFrameError::FrameTooLarge { .. }) => {
                if let Some(task) = self.snapshot.take() {
                    self.machine.mark_snapshot_written(task.await??);
                }
                let snapshot = self.store.write_snapshot(&self.machine).await?;
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
                    last_presence: None,
                    output,
                },
            );
        }
        Ok(())
    }

    fn presence(&mut self, id: u64, mut update: WirePresenceV1) -> Result<(), StoreError> {
        let Some(member) = self.members.get_mut(&id) else {
            return Ok(());
        };
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
        if let Err(reason) = self.machine.validate_push_batch(&txns, wire_bytes) {
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
        let previous = head - self.batch.len() as u64;
        let batch = self
            .batch
            .iter()
            .map(|item| BatchEntry {
                staged: &item.staged,
                base_seq: item.base_seq,
            })
            .collect::<Vec<_>>();
        self.store
            .commit_batch(
                self.doc,
                self.machine.epoch(),
                &batch,
                self.machine.workbook().estimated_bytes(),
            )
            .await?;
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
            self.snapshot = Some(tokio::spawn(async move {
                Ok(store.persist_snapshot(capture).await?.seq())
            }));
        }
    }
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
