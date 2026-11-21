//! Shared protocol constants, configuration, roles, watermarks, and
//! client/server state-machine input/output descriptors.

use alloc::string::String;
use alloc::vec::Vec;

use reticula_core::ops::{InverseOp, Op};
use reticula_formula::binder::BoundFormula;
use reticula_wire::v2::{WireCommittedTxnV2, WirePendingTxnV2, WireSyncFrameV1};

/// Default server quota: maximum populated cells per collaborative document (`2,000,000`).
pub const DEFAULT_MAX_POPULATED_CELLS: u64 = 2_000_000;

/// Default server quota: maximum estimated heap size per collaborative document (`64 MiB`).
pub const DEFAULT_MAX_DOC_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum transactions allowed in a single `Push` frame (`64`).
pub const DEFAULT_MAX_PUSH_BATCH: usize = 64;

/// Default maximum serialized payload bytes allowed for a single `Push` batch (`2 MiB`).
pub const DEFAULT_MAX_PUSH_BYTES: usize = 2 * 1024 * 1024;

/// Default maximum number of recent transactions retained in memory for inline catch-up (`500`).
pub const DEFAULT_CATCHUP_WINDOW: usize = 500;

/// Default snapshot interval in committed transactions (`500`).
pub const DEFAULT_SNAPSHOT_TXN_THRESHOLD: u32 = 500;

/// Default snapshot interval in accumulated transaction payload bytes (`8 MiB`).
pub const DEFAULT_SNAPSHOT_BYTES_THRESHOLD: usize = 8 * 1024 * 1024;

/// Sentinel replica ID returned to read-only viewers (`MemberRole::Viewer`).
pub const VIEWER_REPLICA: u32 = 0;

/// Access control role of a collaborative workbook member (`08-platform.md` & `16-m2b.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberRole {
    /// Workbook creator / owner (full edit + share-link administration).
    Owner,
    /// Invited collaborator with read/write privileges.
    Editor,
    /// Read-only collaborator or anonymous link viewer (`replica = VIEWER_REPLICA`).
    Viewer,
}

impl MemberRole {
    /// Returns `true` for roles permitted to submit workbook mutations.
    #[must_use]
    pub const fn can_write(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }
}

/// Per-replica monotonic progress and identity watermark tracked on the server
/// and inside `RTCLSNAP` snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaWatermark {
    /// Partitioned 24-bit replica identifier (`(epoch << 16) | per_epoch_seq`).
    pub replica: u32,
    /// Owning user UUID bytes.
    pub user_id: [u8; 16],
    /// Highest `client_seq` durably committed by this replica.
    pub last_client_seq: u64,
    /// Highest identity counter committed in `(epoch, replica)` ID space.
    pub max_id_counter: u32,
}

/// Configurable enforcement limits for [`crate::ServerDocMachine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerLimits {
    /// Maximum populated cells permitted in the document workbook.
    pub max_populated_cells: u64,
    /// Maximum estimated in-memory workbook bytes permitted.
    pub max_doc_bytes: usize,
    /// Maximum transactions accepted per `Push` frame.
    pub max_push_batch: usize,
    /// Maximum total encoded payload bytes accepted per `Push` frame.
    pub max_push_bytes: usize,
    /// Maximum committed transactions retained in memory for `CatchupTxns`.
    pub catchup_window: usize,
    /// Committed transactions elapsed before requesting a background snapshot.
    pub snapshot_txn_threshold: u32,
    /// Accumulated payload bytes before requesting a background snapshot.
    pub snapshot_bytes_threshold: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_populated_cells: DEFAULT_MAX_POPULATED_CELLS,
            max_doc_bytes: DEFAULT_MAX_DOC_BYTES,
            max_push_batch: DEFAULT_MAX_PUSH_BATCH,
            max_push_bytes: DEFAULT_MAX_PUSH_BYTES,
            catchup_window: DEFAULT_CATCHUP_WINDOW,
            snapshot_txn_threshold: DEFAULT_SNAPSHOT_TXN_THRESHOLD,
            snapshot_bytes_threshold: DEFAULT_SNAPSHOT_BYTES_THRESHOLD,
        }
    }
}

/// Result of calling [`crate::ServerDocMachine::validate_and_try_apply`].
#[derive(Clone, Debug)]
pub struct StagedTxn {
    /// Authoritative committed transaction ready for database insertion and broadcast.
    pub committed: WireCommittedTxnV2,
    /// Inverses enabling atomic rollback if the batch transaction is aborted before persistence.
    pub(crate) inverses: Vec<InverseOp<BoundFormula>>,
    /// Encoded payload byte length counted against snapshot threshold and DB `log_bytes`.
    pub payload_bytes: usize,
    /// Prior `ReplicaWatermark` snapshot before this staged transaction advanced it.
    pub(crate) prior_watermark: ReplicaWatermark,
    /// New `max_id_counter` after this transaction.
    pub max_id_counter: u32,
}

/// Outcome of submitting one pending transaction to [`crate::ServerDocMachine`].
#[derive(Clone, Debug)]
pub enum TryApplyOutcome {
    /// Newly validated and staged onto the in-memory server workbook.
    Staged(StagedTxn),
    /// Idempotent resend of an already-committed `client_seq`.
    DuplicateAck {
        /// Originating replica ID.
        replica: u32,
        /// Already-committed client sequence.
        client_seq: u64,
        /// Assigned server sequence number.
        seq: u64,
        /// Original authoritative server commit timestamp.
        timestamp_ms: u64,
    },
}

/// Connection lifecycle phase inside [`crate::ClientSyncMachine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Disconnected / working strictly offline.
    Offline,
    /// WebSocket opened; `Hello` sent and awaiting `Welcome`.
    Handshaking,
    /// Server requested a snapshot before replaying live tail commits.
    AwaitingSnapshot {
        /// Sequence number covered by the requested snapshot.
        snapshot_seq: u64,
    },
    /// Caught up and online; pending writes may be pushed immediately.
    Online,
}

/// Pure side-effect commands emitted in deterministic order by [`crate::ClientSyncMachine`].
///
/// Hosts **must execute actions in order**: notably [`Self::PersistPending`]
/// always precedes [`Self::SendFrame`] (`Push`) to uphold the durable-before-send
/// invariant (`06-sync.md`).
#[derive(Clone, Debug, PartialEq)]
pub enum ClientAction {
    /// Append one [`WirePendingTxnV2`] to local `pending.wal` and `fsync` before network send.
    PersistPending(WirePendingTxnV2),
    /// Atomically rewrite/truncate local `pending.wal` to the exact slice given
    /// (after committing catchup pops, epoch remap, or rejection tail removal).
    PersistTruncatedPending(Vec<WirePendingTxnV2>),
    /// Transmit one sync frame over the active WebSocket connection.
    SendFrame(WireSyncFrameV1),
    /// Fetch and install the authoritative `RTCLSNAP` snapshot at `snapshot_seq`.
    RequestSnapshot {
        /// Sequence covered by the snapshot.
        snapshot_seq: u64,
        /// Server snapshot download endpoint path.
        url: String,
    },
    /// Save durably rejected tail transactions into a standalone local rescue
    /// document (`"未能同步的修改 - ..."`) so user edits are never silently lost.
    SaveUnsyncedLocal {
        /// Highest rejected client sequence (`pending` tail starting here was removed).
        from_client_seq: u64,
        /// Ordered transactions that failed server acceptance.
        txns: Vec<WirePendingTxnV2>,
    },
    /// Drop local undo entries authored at `client_seq >= from_client_seq` (`06-sync.md`).
    PruneUndoFrom(u64),
    /// Notify the user that the server restored from a backup incrementing `log_epoch`.
    EpochChanged {
        /// Previous known epoch.
        old_epoch: u8,
        /// Current authoritative server epoch.
        new_epoch: u8,
    },
    /// Confirmed state digest disagreed with a periodic server `Digest` frame at `seq`.
    DigestMismatch {
        /// Server sequence where divergence was observed.
        seq: u64,
        /// Server's reported [`reticula_core::state_digest::StateDigest`] components `(lo, hi, order)`.
        expected: reticula_core::state_digest::StateDigest,
        /// Local confirmed workbook digest at `seq`.
        actual: reticula_core::state_digest::StateDigest,
    },
    /// Reconciled `visible` workbook state changed (triggering formula recalculation & grid redraw).
    NotifyVisibleChanged,
    /// Close and re-establish the WebSocket connection to re-align state via `Hello`/`Welcome`.
    RequestReconnect,
}

/// Applies `ops` atomically onto `workbook` for any caller (`ServerDocMachine` or recovery),
/// rolling back partial ops via their inverses if an operation in the transaction fails.
pub(crate) fn apply_ops_atomic(
    workbook: &mut reticula_core::ops::Workbook<BoundFormula>,
    ops: &[Op<BoundFormula>],
) -> Result<Vec<InverseOp<BoundFormula>>, reticula_core::ops::OpError> {
    let mut inverses = Vec::with_capacity(ops.len());
    for op in ops {
        match workbook.apply(op.clone()) {
            Ok(inv) => inverses.push(inv),
            Err(err) => {
                while let Some(prev) = inverses.pop() {
                    let _ = workbook.apply_inverse(prev);
                }
                return Err(err);
            }
        }
    }
    Ok(inverses)
}

/// Rewinds previously recorded `inverses` in strict LIFO order.
pub(crate) fn rewind_inverses(
    workbook: &mut reticula_core::ops::Workbook<BoundFormula>,
    inverses: Vec<InverseOp<BoundFormula>>,
) {
    let _ = rewind_inverses_checked(workbook, inverses);
}

/// Rewinds `inverses` in strict LIFO order and reports whether every one of
/// them applied.
pub(crate) fn rewind_inverses_checked(
    workbook: &mut reticula_core::ops::Workbook<BoundFormula>,
    mut inverses: Vec<InverseOp<BoundFormula>>,
) -> bool {
    let mut exact = true;
    while let Some(inv) = inverses.pop() {
        exact &= workbook.apply_inverse(inv).is_ok();
    }
    exact
}
