//! Pure client synchronization state machine (`ClientSyncMachine`).
//!
//! Maintains:
//! - `confirmed`: authoritative server-acknowledged workbook at `confirmed_seq`;
//! - `visible`: optimistic local view (`confirmed` + unconfirmed `pending`);
//! - `pending`: local queue rewound and replayed cleanly in a single atomic batch
//!   whenever remote commits or snapshot catch-ups arrive (`06-sync.md`).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;
use alloc::vec::Vec;

use reticula_core::id::{
    IdAllocator, IdRun, SheetId, pack_sheet_id, sheet_id_belongs_to, unpack_sheet_id,
};
use reticula_core::ops::{InverseOp, Op, OpError, Workbook};
use reticula_core::state_digest::StateDigest;
use reticula_formula::binder::BoundFormula;
use reticula_wire::convert_v2::{ops_from_wire_txn_v2, ops_to_wire_txn_v2};
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WireCatchupV1, WireCommittedTxnV2, WirePendingTxnV2, WireRejectReasonV1,
    WireStateDigestV1, WireSyncFrameV1,
};

use crate::types::{
    ClientAction, ConnectionState, DEFAULT_MAX_PUSH_BATCH, DEFAULT_MAX_PUSH_BYTES,
    ReplicaWatermark, apply_ops_atomic, rewind_inverses,
};
use crate::validate::{RemapState, max_id_counter_in_ops};

// Version byte, Push variant tag and the length of a batch of at most 64 entries.
const PUSH_FRAME_OVERHEAD: usize = 3;

#[derive(Clone, Debug)]
struct PendingEntry {
    wire: WirePendingTxnV2,
    ops: Vec<Op<BoundFormula>>,
    inverses: Vec<InverseOp<BoundFormula>>,
    applied_on_visible: bool,
    /// Set for edits authored offline after loading persisted state and before
    /// the server confirms our high-watermark in `Welcome` (guards against
    /// losing offline edits that reused a `client_seq` after OPFS tail loss).
    unverified_offline_edit: bool,
}

#[derive(Clone, Copy, Debug)]
struct WelcomeProgress {
    epoch: u8,
    replica: u32,
    last_client_seq: u64,
    max_id_counter: u32,
    epoch_changed: bool,
}

/// Pure `#![no_std]` client synchronization state machine (`15-m2a.md` 5.3 & `06-sync.md`).
#[derive(Clone, Debug)]
pub struct ClientSyncMachine {
    doc_id: [u8; 16],
    branch_id: [u8; 16],
    epoch: u8,
    replica: Option<u32>,
    confirmed: Workbook<BoundFormula>,
    visible: Workbook<BoundFormula>,
    pending: VecDeque<PendingEntry>,
    buffered_commits: Vec<WireCommittedTxnV2>,
    pending_welcome: Option<WelcomeProgress>,
    confirmed_seq: u64,
    last_acked_client_seq: u64,
    next_client_seq: u64,
    last_pushed_client_seq: u64,
    id_allocator: IdAllocator,
    next_sheet_counter: u8,
    connection_state: ConnectionState,
    watermarks_verified: bool,
}

impl ClientSyncMachine {
    /// Creates a brand-new offline/connecting client sync state machine starting
    /// at an empty `Sheet1` (`seq = 0`, `epoch = 0`, `replica = None`).
    #[must_use]
    pub fn new(doc_id: [u8; 16], branch_id: [u8; 16]) -> Self {
        let confirmed = Workbook::new();
        let visible = confirmed.clone();
        Self {
            doc_id,
            branch_id,
            epoch: 0,
            replica: None,
            confirmed,
            visible,
            pending: VecDeque::new(),
            buffered_commits: Vec::new(),
            pending_welcome: None,
            confirmed_seq: 0,
            last_acked_client_seq: 0,
            next_client_seq: 1,
            last_pushed_client_seq: 0,
            id_allocator: IdAllocator::local(),
            next_sheet_counter: 1,
            connection_state: ConnectionState::Offline,
            watermarks_verified: false,
        }
    }

    /// Reconstructs client state after tab restart / OPFS reload (`base` snapshot
    /// at `confirmed_seq`, restored `pending.wal` records, and saved replica identity).
    #[expect(
        clippy::too_many_arguments,
        reason = "rehydrates full client state from OPFS snapshot + pending WAL without I/O"
    )]
    #[must_use]
    pub fn from_persisted(
        doc_id: [u8; 16],
        branch_id: [u8; 16],
        epoch: u8,
        replica: Option<u32>,
        confirmed_seq: u64,
        last_acked_client_seq: u64,
        confirmed: Workbook<BoundFormula>,
        persisted_pending: Vec<WirePendingTxnV2>,
        next_id_counter_hint: u32,
    ) -> Self {
        let mut visible = confirmed.clone();
        let mut queue = VecDeque::with_capacity(persisted_pending.len());
        let mut highest_seq = last_acked_client_seq;
        let mut highest_id = next_id_counter_hint.saturating_sub(1);
        let mut max_sheet_c = 0_u8;

        if let Some(r) = replica {
            max_sheet_c = max_sheet_c.max(max_sheet_counter_in_wb(&confirmed, epoch, r));
        }

        for wire in persisted_pending {
            highest_seq = highest_seq.max(wire.client_seq);
            let Ok((_, ops)) = ops_from_wire_txn_v2(wire.txn.clone()) else {
                continue;
            };
            if let Some(r) = replica {
                highest_id = highest_id.max(max_id_counter_in_ops(epoch, r, &ops));
                max_sheet_c = max_sheet_c.max(max_sheet_counter_in_ops(&ops, epoch, r));
            }
            let (inverses, applied) = match apply_ops_atomic(&mut visible, &ops) {
                Ok(invs) => (invs, true),
                Err(_) => (Vec::new(), false),
            };
            queue.push_back(PendingEntry {
                wire,
                ops,
                inverses,
                applied_on_visible: applied,
                unverified_offline_edit: false,
            });
        }

        let alloc = if let Some(r) = replica {
            IdAllocator::new(epoch, r, highest_id.saturating_add(1))
                .unwrap_or_else(IdAllocator::local)
        } else {
            IdAllocator::local()
        };

        Self {
            doc_id,
            branch_id,
            epoch,
            replica,
            confirmed,
            visible,
            pending: queue,
            buffered_commits: Vec::new(),
            pending_welcome: None,
            confirmed_seq,
            last_acked_client_seq,
            next_client_seq: highest_seq.saturating_add(1),
            last_pushed_client_seq: 0,
            id_allocator: alloc,
            next_sheet_counter: max_sheet_c.saturating_add(1),
            connection_state: ConnectionState::Offline,
            watermarks_verified: false,
        }
    }

    /// Current connection lifecycle state.
    #[must_use]
    pub const fn connection_state(&self) -> ConnectionState {
        self.connection_state
    }

    /// Current known server epoch.
    #[must_use]
    pub const fn epoch(&self) -> u8 {
        self.epoch
    }

    /// Assigned collaborative replica ID, if any.
    #[must_use]
    pub const fn replica(&self) -> Option<u32> {
        self.replica
    }

    /// Last contiguous server `seq` durably applied to `confirmed`.
    #[must_use]
    pub const fn confirmed_seq(&self) -> u64 {
        self.confirmed_seq
    }

    /// Highest `client_seq` acknowledged or matched from server commit log.
    #[must_use]
    pub const fn last_acked_client_seq(&self) -> u64 {
        self.last_acked_client_seq
    }

    /// Next `client_seq` to be assigned on local push.
    #[must_use]
    pub const fn next_client_seq(&self) -> u64 {
        self.next_client_seq
    }

    /// Own client sequence already represented in an aligned confirmed workbook.
    /// An Ack arriving before its Commit must not advance durable checkpoints.
    #[must_use]
    pub fn confirmed_client_seq(&self) -> u64 {
        self.pending
            .front()
            .map_or(self.last_acked_client_seq, |entry| {
                self.last_acked_client_seq
                    .min(entry.wire.client_seq.saturating_sub(1))
            })
    }

    /// Next identifier counter for persistence alongside a collaborative checkpoint.
    #[must_use]
    pub const fn next_id_counter(&self) -> u32 {
        self.id_allocator.next_counter()
    }

    /// Number of unconfirmed pending transactions in the queue.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Snapshot of currently pending [`WirePendingTxnV2`] entries in FIFO order.
    #[must_use]
    pub fn pending_wire_txns(&self) -> Vec<WirePendingTxnV2> {
        self.pending.iter().map(|e| e.wire.clone()).collect()
    }

    /// Read-only view of the optimistic `visible` workbook.
    #[must_use]
    pub const fn visible(&self) -> &Workbook<BoundFormula> {
        &self.visible
    }

    /// Mutable access to `visible` ONLY for cached evaluation updates or styling
    /// preparation without altering logical state or `StateDigest`.
    pub const fn visible_mut(&mut self) -> &mut Workbook<BoundFormula> {
        &mut self.visible
    }

    /// Read-only view of the confirmed baseline workbook.
    #[must_use]
    pub const fn confirmed(&self) -> &Workbook<BoundFormula> {
        &self.confirmed
    }

    /// Allocates a fresh row [`IdRun`] from this client's synchronized stream.
    pub fn alloc_row_run(&mut self, len: u32) -> Option<IdRun> {
        self.id_allocator.alloc_run(len)
    }

    /// Allocates a fresh column [`IdRun`] from this client's synchronized stream.
    pub fn alloc_col_run(&mut self, len: u32) -> Option<IdRun> {
        self.id_allocator.alloc_run(len)
    }

    /// Allocates the next collision-free collaborative [`SheetId`] for `(epoch, replica)`.
    pub fn alloc_sheet_id(&mut self) -> Option<SheetId> {
        let rep = self.replica.unwrap_or(1);
        let mut c = self.next_sheet_counter.max(1);
        loop {
            let candidate = pack_sheet_id(self.epoch, rep, c)?;
            if self.confirmed.sheet(candidate).is_none() && self.visible.sheet(candidate).is_none()
            {
                self.next_sheet_counter = c.saturating_add(1);
                return Some(candidate);
            }
            if c == u8::MAX {
                return None;
            }
            c = c.saturating_add(1);
        }
    }

    /// Initiates a WebSocket handshake (`Hello`).
    pub fn on_connected(&mut self) -> Vec<ClientAction> {
        self.connection_state = ConnectionState::Handshaking;
        self.pending_welcome = None;
        self.buffered_commits.clear();
        vec![ClientAction::SendFrame(WireSyncFrameV1::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION,
            doc_id: self.doc_id,
            branch_id: self.branch_id,
            replica: self.replica,
            last_applied_seq: self.confirmed_seq,
            known_epoch: self.epoch,
        })]
    }

    /// Transitions to [`ConnectionState::Offline`], preserving all pending edits.
    pub fn on_disconnected(&mut self) {
        self.connection_state = ConnectionState::Offline;
        self.pending_welcome = None;
        self.buffered_commits.clear();
    }

    /// Discards a divergent confirmed revision while retaining all optimistic transactions.
    /// The next Hello replays the server log or installs a fresh snapshot from sequence zero.
    pub fn reset_confirmed_for_resync(&mut self) {
        self.on_disconnected();
        self.confirmed = Workbook::new();
        self.confirmed_seq = 0;
        self.replay_all_pending_fresh();
    }

    /// Applies a local transaction optimistically to `visible`, enqueues it for
    /// persistence + sync, and emits [`ClientAction::PersistPending`] strictly
    /// before [`ClientAction::SendFrame`].
    pub fn push_local(
        &mut self,
        label: alloc::string::String,
        ops: Vec<Op<BoundFormula>>,
        timestamp_ms: u64,
    ) -> Result<Vec<ClientAction>, OpError> {
        let inverses = apply_ops_atomic(&mut self.visible, &ops)?;
        let client_seq = self.next_client_seq;
        self.next_client_seq = client_seq.saturating_add(1);
        let rep = self.replica.unwrap_or(1);
        let wire = WirePendingTxnV2 {
            replica: rep,
            client_seq,
            base_server_seq: self.confirmed_seq,
            timestamp_ms,
            txn: ops_to_wire_txn_v2(label, &ops),
        };
        let entry = PendingEntry {
            wire: wire.clone(),
            ops,
            inverses,
            applied_on_visible: true,
            unverified_offline_edit: !self.watermarks_verified,
        };
        self.pending.push_back(entry);

        let mut actions = vec![
            ClientAction::PersistPending(wire.clone()),
            ClientAction::NotifyVisibleChanged,
        ];
        if self.connection_state == ConnectionState::Online {
            self.last_pushed_client_seq = client_seq;
            actions.push(ClientAction::SendFrame(WireSyncFrameV1::Push {
                txns: vec![wire],
            }));
        }
        Ok(actions)
    }

    /// Handles a server `Welcome` frame (`epoch`, watermarks, and inline txns vs snapshot).
    pub fn on_welcome(&mut self, frame: WireSyncFrameV1) -> Vec<ClientAction> {
        let WireSyncFrameV1::Welcome {
            epoch,
            head_seq: _,
            replica,
            last_client_seq,
            max_id_counter,
            catchup,
        } = frame
        else {
            return Vec::new();
        };

        let mut actions = Vec::new();
        let epoch_changed = epoch != self.epoch;
        let replica_changed = self.replica != Some(replica);
        let old_epoch = self.epoch;

        if epoch_changed {
            actions.push(ClientAction::EpochChanged {
                old_epoch,
                new_epoch: epoch,
            });
            self.rewind_all_pending();
            self.confirmed = Workbook::new();
            self.visible = self.confirmed.clone();
            self.confirmed_seq = 0;
            self.last_acked_client_seq = 0;
            self.next_sheet_counter = 1;
        }

        self.epoch = epoch;
        self.replica = Some(replica);
        self.last_pushed_client_seq = 0;

        match catchup {
            WireCatchupV1::SnapshotRequired { snapshot_seq, url } => {
                let pending_max = self.pending.back().map_or(0, |e| e.wire.client_seq);
                if epoch_changed || replica_changed {
                    self.last_acked_client_seq = last_client_seq;
                    self.next_client_seq = last_client_seq.max(pending_max).saturating_add(1);
                    self.id_allocator =
                        IdAllocator::new(epoch, replica, max_id_counter.saturating_add(1))
                            .unwrap_or_else(IdAllocator::local);
                } else {
                    self.last_acked_client_seq = self.last_acked_client_seq.max(last_client_seq);
                    self.next_client_seq = self
                        .next_client_seq
                        .max(last_client_seq.max(pending_max).saturating_add(1));
                }
                self.pending_welcome = Some(WelcomeProgress {
                    epoch,
                    replica,
                    last_client_seq,
                    max_id_counter,
                    epoch_changed: epoch_changed || replica_changed,
                });
                self.buffered_commits.clear();
                self.connection_state = ConnectionState::AwaitingSnapshot { snapshot_seq };
                if epoch_changed {
                    self.replay_all_pending_fresh();
                }
                actions.push(ClientAction::RequestSnapshot { snapshot_seq, url });
                actions
            }
            WireCatchupV1::Txns { from_seq, txns } => {
                let expected_from = self.confirmed_seq.saturating_add(1);
                if from_seq != expected_from {
                    if epoch_changed {
                        self.replay_all_pending_fresh();
                    }
                    actions.push(ClientAction::RequestReconnect);
                    return actions;
                }
                if !epoch_changed {
                    self.rewind_all_pending();
                }

                let mut pending_modified = false;
                for c in txns {
                    if !self.apply_confirmed_commit(&c, &mut pending_modified) {
                        self.replay_all_pending_fresh();
                        actions.push(ClientAction::RequestReconnect);
                        return actions;
                    }
                }

                self.align_watermarks_and_remap(
                    epoch,
                    replica,
                    last_client_seq,
                    max_id_counter,
                    epoch_changed || replica_changed,
                    &mut pending_modified,
                );
                self.replay_all_pending_fresh();
                self.connection_state = ConnectionState::Online;
                self.watermarks_verified = true;

                if pending_modified {
                    actions.push(ClientAction::PersistTruncatedPending(
                        self.pending_wire_txns(),
                    ));
                }
                actions.push(ClientAction::NotifyVisibleChanged);
                self.push_pending_batches(&mut actions);
                actions
            }
        }
    }

    /// Completes snapshot catch-up after downloading `snapshot_wb` at `snapshot_seq`
    /// plus `watermarks` and `tail_txns` (`seq > snapshot_seq`).
    pub fn install_snapshot(
        &mut self,
        snapshot_seq: u64,
        snapshot_wb: Workbook<BoundFormula>,
        watermarks: &[ReplicaWatermark],
        tail_txns: Vec<WireCommittedTxnV2>,
    ) -> Vec<ClientAction> {
        let Some(progress) = self.pending_welcome.take() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        self.rewind_all_pending();

        let snap_wm = watermarks
            .iter()
            .find(|w| w.replica == progress.replica)
            .copied();
        let wm_cs = progress
            .last_client_seq
            .max(snap_wm.map_or(0, |w| w.last_client_seq));
        let wm_id = progress
            .max_id_counter
            .max(snap_wm.map_or(0, |w| w.max_id_counter));

        self.confirmed = snapshot_wb;
        self.confirmed_seq = snapshot_seq;
        self.epoch = progress.epoch;
        self.replica = Some(progress.replica);
        self.last_acked_client_seq = self
            .last_acked_client_seq
            .max(snap_wm.map_or(0, |w| w.last_client_seq));

        let mut pending_modified = false;
        if !progress.epoch_changed {
            // Drop any pre-crash persisted pending entries covered by snapshot watermarks.
            while let Some(front) = self.pending.front() {
                if !front.unverified_offline_edit
                    && front.wire.replica == progress.replica
                    && front.wire.client_seq <= snap_wm.map_or(0, |w| w.last_client_seq)
                {
                    let _ = self.pending.pop_front();
                    pending_modified = true;
                } else {
                    break;
                }
            }
        }

        let mut buffered = core::mem::take(&mut self.buffered_commits);
        let mut combined = tail_txns;
        combined.append(&mut buffered);
        combined.sort_unstable_by_key(|c| c.seq);
        combined.dedup_by_key(|c| c.seq);

        for c in combined {
            if c.seq <= self.confirmed_seq {
                continue;
            }
            if c.seq != self.confirmed_seq.saturating_add(1) {
                self.replay_all_pending_fresh();
                actions.push(ClientAction::RequestReconnect);
                return actions;
            }
            if !self.apply_confirmed_commit(&c, &mut pending_modified) {
                self.replay_all_pending_fresh();
                actions.push(ClientAction::RequestReconnect);
                return actions;
            }
        }

        self.align_watermarks_and_remap(
            progress.epoch,
            progress.replica,
            wm_cs,
            wm_id,
            progress.epoch_changed,
            &mut pending_modified,
        );
        self.replay_all_pending_fresh();
        self.connection_state = ConnectionState::Online;
        self.watermarks_verified = true;
        self.last_pushed_client_seq = 0;

        if pending_modified {
            actions.push(ClientAction::PersistTruncatedPending(
                self.pending_wire_txns(),
            ));
        }
        actions.push(ClientAction::NotifyVisibleChanged);
        self.push_pending_batches(&mut actions);
        actions
    }

    /// Processes an incoming live `Commit` frame from the server.
    pub fn on_commit(&mut self, commit: WireCommittedTxnV2) -> Vec<ClientAction> {
        if matches!(
            self.connection_state,
            ConnectionState::Offline | ConnectionState::Handshaking
        ) {
            return Vec::new();
        }
        if let ConnectionState::AwaitingSnapshot { snapshot_seq } = self.connection_state {
            if commit.seq > snapshot_seq {
                self.buffered_commits.push(commit);
            }
            return Vec::new();
        }
        if commit.seq <= self.confirmed_seq {
            return Vec::new();
        }
        if commit.seq != self.confirmed_seq.saturating_add(1) {
            return vec![ClientAction::RequestReconnect];
        }

        let is_own_front = self.replica == Some(commit.replica)
            && self.pending.front().is_some_and(|front| {
                !front.unverified_offline_edit
                    && front.wire.client_seq == commit.client_seq
                    && front.wire.txn == commit.txn
            });

        if is_own_front {
            let Ok((_, ops)) = ops_from_wire_txn_v2(commit.txn) else {
                return vec![ClientAction::RequestReconnect];
            };
            if apply_ops_atomic(&mut self.confirmed, &ops).is_err() {
                return vec![ClientAction::RequestReconnect];
            }
            self.confirmed_seq = commit.seq;
            self.last_acked_client_seq = self.last_acked_client_seq.max(commit.client_seq);
            let popped = self.pending.pop_front();
            let mut actions = vec![ClientAction::PersistTruncatedPending(
                self.pending_wire_txns(),
            )];
            if popped.is_some_and(|p| !p.applied_on_visible) {
                self.replay_all_pending_fresh();
                actions.push(ClientAction::NotifyVisibleChanged);
            }
            return actions;
        }

        // Foreign (or mismatched tail-loss) commit -> slow path rewind/apply/replay!
        self.rewind_all_pending();
        let Ok((_, ops)) = ops_from_wire_txn_v2(commit.txn) else {
            self.replay_all_pending_fresh();
            return vec![ClientAction::RequestReconnect];
        };
        if apply_ops_atomic(&mut self.confirmed, &ops).is_err() {
            self.replay_all_pending_fresh();
            return vec![ClientAction::RequestReconnect];
        }
        self.confirmed_seq = commit.seq;
        self.replay_all_pending_fresh();
        vec![ClientAction::NotifyVisibleChanged]
    }

    /// Processes an incoming `Ack` frame (idempotent with `Commit` arrival order).
    pub fn on_ack(&mut self, replica: u32, client_seq: u64) {
        if self.replica == Some(replica) {
            self.last_acked_client_seq = self.last_acked_client_seq.max(client_seq);
        }
    }

    /// Handles a server `Reject` frame cleanly (`06-sync.md` & D-026).
    pub fn on_reject(
        &mut self,
        rejected_client_seq: u64,
        reason: WireRejectReasonV1,
    ) -> Vec<ClientAction> {
        match reason {
            WireRejectReasonV1::NonContiguousClientSeq { .. }
            | WireRejectReasonV1::EpochMismatch { .. }
            | WireRejectReasonV1::UpgradeRequired => {
                vec![ClientAction::RequestReconnect]
            }
            WireRejectReasonV1::Forbidden
            | WireRejectReasonV1::InvalidOp
            | WireRejectReasonV1::QuotaExceeded
            | WireRejectReasonV1::PayloadTooLarge => {
                if rejected_client_seq <= self.last_acked_client_seq {
                    return Vec::new();
                }
                self.rewind_all_pending();

                let mut retained = VecDeque::new();
                let mut archived = Vec::new();
                while let Some(mut entry) = self.pending.pop_front() {
                    entry.inverses.clear();
                    entry.applied_on_visible = false;
                    if entry.wire.client_seq >= rejected_client_seq {
                        archived.push(entry.wire);
                    } else {
                        retained.push_back(entry);
                    }
                }
                self.pending = retained;
                self.replay_all_pending_fresh();
                self.last_pushed_client_seq = self
                    .pending
                    .back()
                    .map_or(self.last_acked_client_seq, |e| e.wire.client_seq);
                self.next_client_seq = self.last_pushed_client_seq.saturating_add(1);

                let mut actions = Vec::new();
                if !archived.is_empty() {
                    actions.push(ClientAction::SaveUnsyncedLocal {
                        from_client_seq: rejected_client_seq,
                        txns: archived,
                    });
                    actions.push(ClientAction::PruneUndoFrom(rejected_client_seq));
                }
                actions.push(ClientAction::PersistTruncatedPending(
                    self.pending_wire_txns(),
                ));
                actions.push(ClientAction::NotifyVisibleChanged);
                actions
            }
        }
    }

    /// Compares a periodic server `Digest` frame against local `confirmed` when
    /// `self.confirmed_seq == seq`, emitting [`ClientAction::DigestMismatch`] on
    /// divergence.
    #[must_use]
    pub fn on_digest(&self, seq: u64, wire_digest: WireStateDigestV1) -> Vec<ClientAction> {
        if self.confirmed_seq != seq {
            return Vec::new();
        }
        let Ok(expected) = StateDigest::try_from(wire_digest) else {
            return Vec::new();
        };
        let actual = self.confirmed.state_digest();
        if actual != expected {
            return vec![ClientAction::DigestMismatch {
                seq,
                expected,
                actual,
            }];
        }
        Vec::new()
    }

    // ------------------------------------------------------------ internal helpers

    fn apply_confirmed_commit(
        &mut self,
        commit: &WireCommittedTxnV2,
        pending_modified: &mut bool,
    ) -> bool {
        if commit.seq <= self.confirmed_seq {
            return true;
        }
        if commit.seq != self.confirmed_seq.saturating_add(1) {
            return false;
        }
        let Ok((_, ops)) = ops_from_wire_txn_v2(commit.txn.clone()) else {
            return false;
        };
        if apply_ops_atomic(&mut self.confirmed, &ops).is_err() {
            return false;
        }
        self.confirmed_seq = commit.seq;

        let is_own_front = self.replica == Some(commit.replica)
            && self.pending.front().is_some_and(|front| {
                !front.unverified_offline_edit
                    && front.wire.client_seq == commit.client_seq
                    && front.wire.txn == commit.txn
            });
        if is_own_front {
            let _ = self.pending.pop_front();
            self.last_acked_client_seq = self.last_acked_client_seq.max(commit.client_seq);
            *pending_modified = true;
        }
        true
    }

    fn align_watermarks_and_remap(
        &mut self,
        epoch: u8,
        replica: u32,
        server_last_cs: u64,
        server_max_id_counter: u32,
        force_remap_all: bool,
        pending_modified: &mut bool,
    ) {
        self.last_acked_client_seq = self.last_acked_client_seq.max(server_last_cs);
        self.next_client_seq = server_last_cs.saturating_add(1);

        let cur_next_id = if self.id_allocator.epoch() == epoch
            && self.id_allocator.replica() == replica
            && !force_remap_all
        {
            self.id_allocator
                .next_counter()
                .max(server_max_id_counter.saturating_add(1))
        } else {
            server_max_id_counter.saturating_add(1)
        };
        self.id_allocator =
            IdAllocator::new(epoch, replica, cur_next_id).unwrap_or_else(IdAllocator::local);

        let wb_sheet_max = max_sheet_counter_in_wb(&self.confirmed, epoch, replica);
        if force_remap_all {
            self.next_sheet_counter = wb_sheet_max.saturating_add(1);
        } else {
            self.next_sheet_counter = self.next_sheet_counter.max(wb_sheet_max.saturating_add(1));
        }

        let non_contiguous = self.pending.iter().enumerate().any(|(i, e)| {
            e.wire.client_seq != server_last_cs.saturating_add(1).saturating_add(i as u64)
        });
        let needs_remap = force_remap_all
            || non_contiguous
            || self.pending.iter().any(|e| {
                e.wire.replica != replica
                    || e.wire.client_seq <= server_last_cs
                    || e.unverified_offline_edit
            });

        if needs_remap && !self.pending.is_empty() {
            *pending_modified = true;
            // Rebuild ID allocator strictly above confirmed watermark when remapping so
            // existing confirmed allocations stay untouched.
            self.id_allocator =
                IdAllocator::new(epoch, replica, server_max_id_counter.saturating_add(1))
                    .unwrap_or_else(IdAllocator::local);
            self.next_sheet_counter = wb_sheet_max.saturating_add(1);
            let mut remap = RemapState {
                epoch,
                replica,
                id_alloc: &mut self.id_allocator,
                next_sheet_counter: &mut self.next_sheet_counter,
                confirmed: &self.confirmed,
                sheets: BTreeMap::new(),
                rows: BTreeMap::new(),
                cols: BTreeMap::new(),
            };
            let mut old_entries = core::mem::take(&mut self.pending);
            let mut cs = server_last_cs.saturating_add(1);
            while let Some(entry) = old_entries.pop_front() {
                let mut remapped_ops = Vec::with_capacity(entry.ops.len());
                for op in entry.ops {
                    if let Some(m) = remap.remap_op(op) {
                        remapped_ops.push(m);
                    }
                }
                let wire = WirePendingTxnV2 {
                    replica,
                    client_seq: cs,
                    base_server_seq: self.confirmed_seq,
                    timestamp_ms: entry.wire.timestamp_ms,
                    txn: ops_to_wire_txn_v2(entry.wire.txn.label, &remapped_ops),
                };
                cs = cs.saturating_add(1);
                self.pending.push_back(PendingEntry {
                    wire,
                    ops: remapped_ops,
                    inverses: Vec::new(),
                    applied_on_visible: false,
                    unverified_offline_edit: false,
                });
            }
            self.next_client_seq = cs;
        } else {
            let last_cs = self.pending.back().map(|e| e.wire.client_seq);
            if let Some(cs) = last_cs {
                self.next_client_seq = self.next_client_seq.max(cs.saturating_add(1));
            }
            for entry in &mut self.pending {
                entry.unverified_offline_edit = false;
            }
            // Also keep id_allocator & sheet counter strictly ahead of retained unremapped pending ops!
            let mut max_pending_id = server_max_id_counter;
            let mut max_pending_sc = wb_sheet_max;
            for entry in &self.pending {
                max_pending_id =
                    max_pending_id.max(max_id_counter_in_ops(epoch, replica, &entry.ops));
                max_pending_sc =
                    max_pending_sc.max(max_sheet_counter_in_ops(&entry.ops, epoch, replica));
            }
            self.id_allocator = IdAllocator::new(epoch, replica, max_pending_id.saturating_add(1))
                .unwrap_or_else(IdAllocator::local);
            self.next_sheet_counter = self
                .next_sheet_counter
                .max(max_pending_sc.saturating_add(1));
        }
    }

    fn rewind_all_pending(&mut self) {
        let mut idx = self.pending.len();
        while idx > 0 {
            idx = idx.saturating_sub(1);
            if let Some(entry) = self.pending.get_mut(idx)
                && entry.applied_on_visible
            {
                let invs = core::mem::take(&mut entry.inverses);
                rewind_inverses(&mut self.visible, invs);
                entry.applied_on_visible = false;
            }
        }
    }

    fn replay_all_pending_fresh(&mut self) {
        self.visible = self.confirmed.clone();
        for entry in &mut self.pending {
            match apply_ops_atomic(&mut self.visible, &entry.ops) {
                Ok(invs) => {
                    entry.inverses = invs;
                    entry.applied_on_visible = true;
                }
                Err(_) => {
                    entry.inverses.clear();
                    entry.applied_on_visible = false;
                }
            }
        }
    }

    fn push_pending_batches(&mut self, actions: &mut Vec<ClientAction>) {
        if self.pending.is_empty() {
            return;
        }
        let mut batch = Vec::new();
        let mut batch_bytes = 0_usize;
        for entry in &self.pending {
            let entry_bytes = postcard::to_allocvec(&entry.wire).map_or(64, |v| v.len());
            if !batch.is_empty()
                && (batch.len() >= DEFAULT_MAX_PUSH_BATCH
                    || batch_bytes.saturating_add(entry_bytes)
                        > DEFAULT_MAX_PUSH_BYTES.saturating_sub(PUSH_FRAME_OVERHEAD))
            {
                actions.push(ClientAction::SendFrame(WireSyncFrameV1::Push {
                    txns: core::mem::take(&mut batch),
                }));
                batch_bytes = 0;
            }
            self.last_pushed_client_seq = self.last_pushed_client_seq.max(entry.wire.client_seq);
            batch.push(entry.wire.clone());
            batch_bytes = batch_bytes.saturating_add(entry_bytes);
        }
        if !batch.is_empty() {
            actions.push(ClientAction::SendFrame(WireSyncFrameV1::Push {
                txns: batch,
            }));
        }
    }
}

/// Computes `replay(confirmed, pending)` cleanly onto a fresh clone of
/// `confirmed` (used by tests and deterministic simulation to verify Invariant #1).
#[must_use]
pub fn replay_pending_onto(
    confirmed: &Workbook<BoundFormula>,
    pending: &[WirePendingTxnV2],
) -> Workbook<BoundFormula> {
    let mut wb = confirmed.clone();
    for item in pending {
        if let Ok((_, ops)) = ops_from_wire_txn_v2(item.txn.clone()) {
            let _ = apply_ops_atomic(&mut wb, &ops);
        }
    }
    wb
}

fn max_sheet_counter_in_wb(wb: &Workbook<BoundFormula>, epoch: u8, replica: u32) -> u8 {
    let mut max_c = 0_u8;
    for sheet in wb.sheets() {
        if sheet_id_belongs_to(sheet.id(), epoch, replica) {
            let (_, _, counter) = unpack_sheet_id(sheet.id());
            max_c = max_c.max(counter);
        }
    }
    max_c
}

fn max_sheet_counter_in_ops(ops: &[Op<BoundFormula>], epoch: u8, replica: u32) -> u8 {
    let mut max_c = 0_u8;
    for op in ops {
        if let Op::AddSheet { sheet, .. } = op
            && sheet_id_belongs_to(*sheet, epoch, replica)
        {
            let (_, _, counter) = unpack_sheet_id(*sheet);
            max_c = max_c.max(counter);
        }
    }
    max_c
}
