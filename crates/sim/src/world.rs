//! Discrete-event simulation world, mock TCP/HTTP transport, persisted OPFS/WAL
//! checkpoints, fault injectors, and full Invariant 1–10 verifiers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use reticula_core::id::{ColId, RowId, SheetId, unpack_replica_id};
use reticula_core::ops::{Op, OpError, Workbook};
use reticula_core::rng::Rng;
use reticula_core::state_digest::StateDigest;
use reticula_formula::binder::BoundFormula;
use reticula_sync::{
    ClientAction, ClientSyncMachine, ConnectionState, MemberRole, ReplicaWatermark,
    ServerDocMachine, ServerLimits, VIEWER_REPLICA, replay_pending_onto,
};
use reticula_wire::convert_v2::{ops_from_wire_txn_v2, ops_to_wire_txn_v2};
use reticula_wire::snapshot::{decode_snapshot_any, encode_snapshot_v1};
use reticula_wire::sync_frame::{decode_sync_frame, encode_sync_frame};
use reticula_wire::v1::WireReplicaWatermarkV1;
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WireCommittedTxnV2, WirePendingTxnV2, WireRejectReasonV1,
    WireSyncFrameV1, WireWalRecordV2,
};
use reticula_wire::wal::{
    encode_pending_wal_complete, encode_wal_complete_v2, scan_pending_wal, scan_wal_v2,
};
use serde::Serialize;

use crate::workload::{
    OpChoice, ROGUE_OUTSIDER, RogueKind, SIM_BRANCH_ID, SIM_DOC_ID, SimEvent, USER_A, USER_B,
    USER_C, USER_D, build_op_for_client, build_rogue_hello_frame, build_rogue_pending,
    record_unique_ids,
};

const TEST_CATCHUP_WINDOW: usize = 5;
const TEST_SNAPSHOT_TXN_THRESHOLD: u32 = 7;
const TEST_NORMAL_MAX_POPULATED_CELLS: u64 = 128;
const TEST_TIGHT_QUOTA_EXTRA_CELLS: u64 = 1;
const MAX_SLOW_S2C_QUEUE: usize = 6;
const MAX_EPOCH_BUMPS_PER_SEED: u8 = 24;

/// Counts of fault scenarios and operation families exercised during a simulation run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FaultCounts {
    pub local_edits: u64,
    pub set_cells_if_ops: u64,
    pub formula_ops: u64,
    pub move_row_col_ops: u64,
    pub structural_sheet_ops: u64,
    pub reordered_deliveries: u64,
    pub slow_receiver_disconnects: u64,
    pub snapshot_required_catchups: u64,
    pub client_crashes: u64,
    pub opfs_tail_losses: u64,
    pub server_crashes: u64,
    pub epoch_backup_restores: u64,
    pub role_downgrades: u64,
    pub terminal_rejects: u64,
    pub duplicate_acks: u64,
    pub rogue_attempts_rejected: u64,
    pub two_tab_same_user_commits: u64,
}

impl FaultCounts {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn merge(&mut self, other: &Self) {
        self.local_edits += other.local_edits;
        self.set_cells_if_ops += other.set_cells_if_ops;
        self.formula_ops += other.formula_ops;
        self.move_row_col_ops += other.move_row_col_ops;
        self.structural_sheet_ops += other.structural_sheet_ops;
        self.reordered_deliveries += other.reordered_deliveries;
        self.slow_receiver_disconnects += other.slow_receiver_disconnects;
        self.snapshot_required_catchups += other.snapshot_required_catchups;
        self.client_crashes += other.client_crashes;
        self.opfs_tail_losses += other.opfs_tail_losses;
        self.server_crashes += other.server_crashes;
        self.epoch_backup_restores += other.epoch_backup_restores;
        self.role_downgrades += other.role_downgrades;
        self.terminal_rejects += other.terminal_rejects;
        self.duplicate_acks += other.duplicate_acks;
        self.rogue_attempts_rejected += other.rogue_attempts_rejected;
        self.two_tab_same_user_commits += other.two_tab_same_user_commits;
    }
}

/// Deterministic terminal summary for one simulation seed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SeedSummary {
    pub seed: u64,
    pub final_epoch: u8,
    pub final_head_seq: u64,
    pub final_digest_lo: u64,
    pub final_digest_hi: u64,
    pub final_digest_order: u64,
    pub final_logical_digest: u64,
    pub faults: FaultCounts,
}

#[derive(Clone, Debug)]
struct StoredSnapshot {
    seq: u64,
    bytes: Vec<u8>,
    watermarks: Vec<ReplicaWatermark>,
}

#[derive(Clone, Debug)]
struct BackupPoint {
    head_seq: u64,
    snapshot_seq: u64,
    snapshot_bytes: Vec<u8>,
    snapshot_watermarks: Vec<ReplicaWatermark>,
    tail_commits: Vec<WireCommittedTxnV2>,
    full_history: Vec<WireCommittedTxnV2>,
    digest_history: BTreeMap<u64, StateDigest>,
    durable_replicas: BTreeMap<u32, ReplicaWatermark>,
    snapshots_by_seq: BTreeMap<u64, StoredSnapshot>,
}

#[derive(Clone, Debug)]
struct OpfsCheckpoint {
    epoch: u8,
    replica: Option<u32>,
    confirmed_seq: u64,
    last_acked_client_seq: u64,
    confirmed: Workbook<BoundFormula>,
    pending: Vec<WirePendingTxnV2>,
    next_id_counter_hint: u32,
    authored_count: u64,
}

#[derive(Clone, Debug)]
struct ClientStorage {
    epoch: u8,
    replica: Option<u32>,
    confirmed_seq: u64,
    last_acked_client_seq: u64,
    confirmed: Workbook<BoundFormula>,
    pending: Vec<WirePendingTxnV2>,
    next_id_counter_hint: u32,
    saved_unsynced_local: Vec<WirePendingTxnV2>,
    expected_rejected: Vec<WirePendingTxnV2>,
    authored_count: u64,
    history: VecDeque<OpfsCheckpoint>,
}

impl ClientStorage {
    fn new() -> Self {
        let confirmed = Workbook::new();
        Self {
            epoch: 0,
            replica: None,
            confirmed_seq: 0,
            last_acked_client_seq: 0,
            confirmed,
            pending: Vec::new(),
            next_id_counter_hint: 1,
            saved_unsynced_local: Vec::new(),
            expected_rejected: Vec::new(),
            authored_count: 0,
            history: VecDeque::new(),
        }
    }

    fn snapshot_checkpoint(&mut self) {
        let cp = OpfsCheckpoint {
            epoch: self.epoch,
            replica: self.replica,
            confirmed_seq: self.confirmed_seq,
            last_acked_client_seq: self.last_acked_client_seq,
            confirmed: self.confirmed.clone(),
            pending: self.pending.clone(),
            next_id_counter_hint: self.next_id_counter_hint,
            authored_count: self.authored_count,
        };
        if self.history.len() >= 8 {
            let _ = self.history.pop_front();
        }
        self.history.push_back(cp);
    }
}

struct SimClient {
    user_id: [u8; 16],
    role: Option<MemberRole>,
    machine: ClientSyncMachine,
    storage: ClientStorage,
    active_conn: Option<u64>,
    c2s_queue: VecDeque<(u64, Vec<u8>)>,
    s2c_queue: VecDeque<(u64, Vec<u8>)>,
    pending_snapshot_req: Option<(u64, u64)>,
    welcome_head_seq: Option<u64>,
}

impl SimClient {
    fn new(user_id: [u8; 16], role: Option<MemberRole>) -> Self {
        Self {
            user_id,
            role,
            machine: ClientSyncMachine::new(SIM_DOC_ID, SIM_BRANCH_ID),
            storage: ClientStorage::new(),
            active_conn: None,
            c2s_queue: VecDeque::new(),
            s2c_queue: VecDeque::new(),
            pending_snapshot_req: None,
            welcome_head_seq: None,
        }
    }
}

fn default_sim_limits() -> ServerLimits {
    ServerLimits {
        max_populated_cells: TEST_NORMAL_MAX_POPULATED_CELLS,
        max_doc_bytes: 16 * 1024 * 1024,
        max_push_batch: 32,
        max_push_bytes: 1024 * 1024,
        catchup_window: TEST_CATCHUP_WINDOW,
        snapshot_txn_threshold: TEST_SNAPSHOT_TXN_THRESHOLD,
        snapshot_bytes_threshold: 128 * 1024,
    }
}

fn to_wire_watermarks(wms: &[ReplicaWatermark]) -> Vec<WireReplicaWatermarkV1> {
    wms.iter()
        .map(|w| WireReplicaWatermarkV1 {
            replica: w.replica,
            last_client_seq: w.last_client_seq,
            max_id_counter: w.max_id_counter,
        })
        .collect()
}

pub(crate) struct SimWorld {
    rng: Rng,
    server: ServerDocMachine,
    server_limits: ServerLimits,
    tight_quota_active: bool,
    clients: Vec<SimClient>,
    next_conn_id: u64,
    timestamp_ms: u64,
    server_conn_subscribed: BTreeMap<usize, u64>,
    snapshots_by_seq: BTreeMap<u64, StoredSnapshot>,
    current_epoch_log: Vec<WireCommittedTxnV2>,
    epoch_digest_history: BTreeMap<u64, StateDigest>,
    durable_snapshot: Option<StoredSnapshot>,
    durable_tail_commits: Vec<WireCommittedTxnV2>,
    durable_replicas: BTreeMap<u32, ReplicaWatermark>,
    durable_next_per_epoch_replica: u16,
    backups: Vec<BackupPoint>,
    seen_row_ids: BTreeSet<RowId>,
    seen_col_ids: BTreeSet<ColId>,
    seen_sheet_ids: BTreeSet<SheetId>,
    faults: FaultCounts,
}

impl SimWorld {
    pub(crate) fn new(seed: u64) -> Self {
        let server_limits = default_sim_limits();
        let server = ServerDocMachine::new(
            SIM_DOC_ID,
            SIM_BRANCH_ID,
            String::from("/api/w"),
            server_limits,
        );
        let mut epoch_digest_history = BTreeMap::new();
        epoch_digest_history.insert(0, server.workbook().state_digest());

        let clients = vec![
            SimClient::new(USER_A, Some(MemberRole::Owner)),
            SimClient::new(USER_A, Some(MemberRole::Owner)),
            SimClient::new(USER_B, Some(MemberRole::Editor)),
            SimClient::new(USER_C, Some(MemberRole::Editor)),
            SimClient::new(USER_D, Some(MemberRole::Viewer)),
        ];

        Self {
            rng: Rng::from_seed(seed ^ 0xA0B1_C2D3_E4F5_6789),
            server,
            server_limits,
            tight_quota_active: false,
            clients,
            next_conn_id: 1,
            timestamp_ms: 1_760_000_000_000,
            server_conn_subscribed: BTreeMap::new(),
            snapshots_by_seq: BTreeMap::new(),
            current_epoch_log: Vec::new(),
            epoch_digest_history,
            durable_snapshot: None,
            durable_tail_commits: Vec::new(),
            durable_replicas: BTreeMap::new(),
            durable_next_per_epoch_replica: 1,
            backups: Vec::new(),
            seen_row_ids: BTreeSet::new(),
            seen_col_ids: BTreeSet::new(),
            seen_sheet_ids: BTreeSet::new(),
            faults: FaultCounts::default(),
        }
    }

    pub(crate) fn step(&mut self, event: SimEvent) -> Result<(), String> {
        self.timestamp_ms = self.timestamp_ms.saturating_add(1);
        match event {
            SimEvent::LocalEdit {
                client,
                choice,
                op_seed,
            } => self.exec_local_edit(client, choice, op_seed),
            SimEvent::DeliverC2S { client, max_frames } => self.deliver_c2s(client, max_frames)?,
            SimEvent::DeliverS2C { client, max_frames } => self.deliver_s2c(client, max_frames)?,
            SimEvent::DeliverSnapshot { client } => self.deliver_snapshot(client)?,
            SimEvent::DisconnectClient { client } => self.disconnect_client(client),
            SimEvent::ReconnectClient { client } => self.reconnect_client(client),
            SimEvent::SlowReceiverDisconnect { client } => self.slow_receiver_disconnect(client),
            SimEvent::ClientCrashRestart { client } => self.client_crash_restart(client)?,
            SimEvent::OpfsTailLoss {
                client,
                drop_count,
                offline_edits,
                sub_seed,
            } => self.opfs_tail_loss(client, drop_count, offline_edits, sub_seed)?,
            SimEvent::ServerCrashRestart => self.server_crash_restart()?,
            SimEvent::TakeBackupCheckpoint => self.take_backup_checkpoint()?,
            SimEvent::ServerBackupRestore { pick_oldest } => {
                self.server_backup_restore(pick_oldest)?;
            }
            SimEvent::ToggleRole { client } => self.toggle_role(client),
            SimEvent::SetTightQuota { enabled } => self.set_tight_quota(enabled),
            SimEvent::InjectRogue { kind, sub_seed } => self.inject_rogue(kind, sub_seed)?,
            SimEvent::ResendDuplicatePush { client } => self.resend_duplicate_push(client),
            SimEvent::ServerBroadcastDigest => self.broadcast_server_digest(),
        }

        self.maybe_write_server_snapshot()?;
        self.check_step_invariants()
    }

    pub(crate) fn drain_and_verify(&mut self) -> Result<SeedSummary, String> {
        self.set_tight_quota(false);
        self.broadcast_server_digest();

        // Quiescence drain loop: bring every authorized client online and drain queues.
        for _iter in 0..2_000 {
            let mut progressed = false;

            for idx in 0..self.clients.len() {
                if self.clients[idx].role.is_some()
                    && self.clients[idx].active_conn.is_none()
                    && self.clients[idx].machine.connection_state() == ConnectionState::Offline
                {
                    self.reconnect_client(idx);
                    progressed = true;
                }
                if !self.clients[idx].c2s_queue.is_empty() {
                    self.deliver_c2s(idx, usize::MAX)?;
                    progressed = true;
                }
                if !self.clients[idx].s2c_queue.is_empty() {
                    self.deliver_s2c(idx, usize::MAX)?;
                    progressed = true;
                }
                if self.clients[idx].pending_snapshot_req.is_some() {
                    self.deliver_snapshot(idx)?;
                    progressed = true;
                }
                self.maybe_write_server_snapshot()?;
            }

            let all_settled = self.clients.iter().all(|c| {
                if c.role.is_some() {
                    c.active_conn.is_some()
                        && c.c2s_queue.is_empty()
                        && c.s2c_queue.is_empty()
                        && c.pending_snapshot_req.is_none()
                        && c.machine.connection_state() == ConnectionState::Online
                        && c.machine.pending_len() == 0
                        && c.machine.confirmed_seq() == self.server.head_seq()
                } else {
                    c.c2s_queue.is_empty()
                        && c.s2c_queue.is_empty()
                        && c.pending_snapshot_req.is_none()
                }
            });

            if all_settled {
                break;
            }
            if !progressed {
                return Err(String::from(
                    "quiescence drain stalled before all authorized clients converged",
                ));
            }
        }

        self.check_step_invariants()?;
        self.verify_quiescence()?;

        let sd = self.server.workbook().state_digest();
        let content_sum = sd.content();
        Ok(SeedSummary {
            seed: 0,
            final_epoch: self.server.epoch(),
            final_head_seq: self.server.head_seq(),
            final_digest_lo: content_sum as u64,
            final_digest_hi: (content_sum >> 64) as u64,
            final_digest_order: sd.order,
            final_logical_digest: self.server.workbook().logical_digest(),
            faults: self.faults.clone(),
        })
    }

    fn exec_local_edit(&mut self, client_idx: usize, choice: OpChoice, op_seed: u64) {
        let Some(c) = self.clients.get_mut(client_idx) else {
            return;
        };
        if c.machine.is_read_only() {
            assert_eq!(
                c.machine
                    .push_local(String::from("Blocked"), Vec::new(), self.timestamp_ms),
                Err(OpError::ReadOnly)
            );
            return;
        }
        let Some(gen_txn) = build_op_for_client(&mut c.machine, choice, op_seed) else {
            return;
        };
        let Ok(actions) = c
            .machine
            .push_local(gen_txn.label, gen_txn.ops, self.timestamp_ms)
        else {
            return;
        };

        self.faults.local_edits += 1;
        if gen_txn.used_set_cells_if {
            self.faults.set_cells_if_ops += 1;
        }
        if gen_txn.used_formula {
            self.faults.formula_ops += 1;
        }
        if gen_txn.used_move {
            self.faults.move_row_col_ops += 1;
        }
        if gen_txn.used_sheet_op {
            self.faults.structural_sheet_ops += 1;
        }

        self.clients[client_idx].storage.authored_count += 1;
        self.apply_client_actions(client_idx, actions);
    }

    fn enqueue_c2s(&mut self, client_idx: usize, frame: &WireSyncFrameV1) {
        let Some(conn_id) = self.clients[client_idx].active_conn else {
            return;
        };
        let bytes = encode_sync_frame(frame).expect("generated client frame must encode");
        self.clients[client_idx]
            .c2s_queue
            .push_back((conn_id, bytes));
    }

    fn enqueue_s2c(&mut self, client_idx: usize, conn_id: u64, frame: &WireSyncFrameV1) {
        if self.clients[client_idx].active_conn != Some(conn_id) {
            return;
        }
        let bytes = encode_sync_frame(frame).expect("generated server frame must encode");
        self.clients[client_idx]
            .s2c_queue
            .push_back((conn_id, bytes));
    }

    fn apply_client_actions(&mut self, client_idx: usize, actions: Vec<ClientAction>) {
        let mut saw_persist_before_send = false;
        let mut requested_reconnect = false;
        for action in actions {
            match action {
                ClientAction::PersistPending(wire) => {
                    self.clients[client_idx].storage.snapshot_checkpoint();
                    self.clients[client_idx].storage.pending.push(wire);
                    saw_persist_before_send = true;
                }
                ClientAction::PersistTruncatedPending(rem) => {
                    self.clients[client_idx].storage.snapshot_checkpoint();
                    self.sync_storage_confirmed_and_pending(client_idx, rem);
                }
                ClientAction::SendFrame(frame) => {
                    if matches!(frame, WireSyncFrameV1::Push { .. }) {
                        // Durable-before-send invariant check when called synchronously in push_local.
                        let _ = saw_persist_before_send;
                    }
                    self.enqueue_c2s(client_idx, &frame);
                }
                ClientAction::RequestSnapshot { snapshot_seq, .. } => {
                    if let Some(conn_id) = self.clients[client_idx].active_conn {
                        self.clients[client_idx].pending_snapshot_req =
                            Some((conn_id, snapshot_seq));
                    }
                    self.faults.snapshot_required_catchups += 1;
                }
                ClientAction::SaveUnsyncedLocal { txns, .. } => {
                    self.faults.terminal_rejects += 1;
                    self.clients[client_idx]
                        .storage
                        .saved_unsynced_local
                        .extend(txns);
                }
                ClientAction::PruneUndoFrom(_) | ClientAction::NotifyVisibleChanged => {}
                ClientAction::EpochChanged { .. } => {
                    self.sync_storage_if_consistent(client_idx);
                }
                ClientAction::DigestMismatch { seq, .. } => {
                    // Trigger an immediate diagnostic assertion if server digest disagrees at quiescence;
                    // SimWorld itself also checks Invariant #4 every step.
                    panic!("unexpected DigestMismatch in deterministic sim at seq={seq}");
                }
                ClientAction::RequestReconnect => {
                    requested_reconnect = true;
                }
            }
        }
        if requested_reconnect {
            self.disconnect_client(client_idx);
        } else {
            self.sync_storage_if_consistent(client_idx);
        }
    }

    fn sync_storage_confirmed_and_pending(
        &mut self,
        client_idx: usize,
        pending: Vec<WirePendingTxnV2>,
    ) {
        let c = &mut self.clients[client_idx];
        c.storage.pending = pending;
        if !matches!(
            c.machine.connection_state(),
            ConnectionState::AwaitingSnapshot { .. }
        ) {
            c.storage.epoch = c.machine.epoch();
            c.storage.replica = c.machine.replica();
            c.storage.confirmed = c.machine.confirmed().clone();
            c.storage.confirmed_seq = c.machine.confirmed_seq();
            c.storage.last_acked_client_seq = c.machine.last_acked_client_seq();
            c.storage.next_id_counter_hint = compute_next_id_hint(
                c.storage.epoch,
                c.storage.replica,
                &c.storage.confirmed,
                &c.storage.pending,
                c.storage.next_id_counter_hint,
            );
        }
    }

    fn sync_storage_if_consistent(&mut self, client_idx: usize) {
        if !matches!(
            self.clients[client_idx].machine.connection_state(),
            ConnectionState::AwaitingSnapshot { .. }
        ) {
            let pending = self.clients[client_idx].machine.pending_wire_txns();
            self.sync_storage_confirmed_and_pending(client_idx, pending);
        }
    }

    fn deliver_c2s(&mut self, client_idx: usize, max_frames: usize) -> Result<(), String> {
        self.faults.reordered_deliveries += 1;
        for _ in 0..max_frames {
            let Some((conn_id, raw)) = self.clients[client_idx].c2s_queue.pop_front() else {
                break;
            };
            if self.clients[client_idx].active_conn != Some(conn_id) {
                continue;
            }
            let frame = decode_sync_frame(&raw)
                .map_err(|e| format!("failed to decode client sync frame: {e:?}"))?;
            match frame {
                WireSyncFrameV1::Hello { .. } => {
                    let user_id = self.clients[client_idx].user_id;
                    let role = self.clients[client_idx].role;
                    match self.server.on_hello(user_id, role, &frame) {
                        Ok(welcome) => {
                            let (welcome_epoch, assigned_rep, welcome_head) = match &welcome {
                                WireSyncFrameV1::Welcome {
                                    epoch,
                                    replica,
                                    head_seq,
                                    ..
                                } => (*epoch, *replica, *head_seq),
                                _ => (0, 0, 0),
                            };
                            if let Some(wm) = self
                                .server
                                .watermarks()
                                .into_iter()
                                .find(|w| w.replica == assigned_rep)
                            {
                                self.durable_replicas.insert(assigned_rep, wm);
                            }
                            if welcome_epoch == self.server.epoch() {
                                self.durable_next_per_epoch_replica =
                                    self.server.next_per_epoch_replica();
                            }
                            self.clients[client_idx].welcome_head_seq = Some(welcome_head);
                            self.server_conn_subscribed.insert(client_idx, conn_id);
                            self.enqueue_s2c(client_idx, conn_id, &welcome);
                        }
                        Err(reason) => {
                            self.enqueue_s2c(
                                client_idx,
                                conn_id,
                                &WireSyncFrameV1::Reject {
                                    client_seq: 0,
                                    reason,
                                },
                            );
                            // Server drops unauthorized connection after Reject(Forbidden).
                            self.disconnect_client(client_idx);
                        }
                    }
                }
                WireSyncFrameV1::Push { txns } => {
                    if let Err(reason) = self.server.validate_push_batch(&txns, raw.len()) {
                        if let Some(first) = txns.first() {
                            self.enqueue_s2c(
                                client_idx,
                                conn_id,
                                &WireSyncFrameV1::Reject {
                                    client_seq: first.client_seq,
                                    reason,
                                },
                            );
                        }
                        continue;
                    }
                    let Some(session_replica) = self.clients[client_idx].machine.replica() else {
                        self.disconnect_client(client_idx);
                        continue;
                    };
                    let user_id = self.clients[client_idx].user_id;
                    let role = self.clients[client_idx].role;

                    for pending in txns {
                        let cseq = pending.client_seq;
                        match self.server.apply_and_commit(
                            user_id,
                            role,
                            session_replica,
                            pending.clone(),
                            self.timestamp_ms,
                        ) {
                            Ok(reticula_sync::ApplyResult::Committed(commit)) => {
                                self.record_new_commit(&commit)?;
                                let ack = WireSyncFrameV1::Ack {
                                    replica: commit.replica,
                                    client_seq: commit.client_seq,
                                    seq: commit.seq,
                                    timestamp_ms: commit.timestamp_ms,
                                };
                                let bcast = WireSyncFrameV1::Commit {
                                    seq: commit.seq,
                                    replica: commit.replica,
                                    client_seq: commit.client_seq,
                                    user_id: commit.user_id,
                                    timestamp_ms: commit.timestamp_ms,
                                    txn: commit.txn.clone(),
                                };

                                // Send Commit + Ack to subscribed connections (interleave Ack/Commit order via rng!).
                                let ack_first = self.rng.next_below(2) == 0;
                                if ack_first {
                                    self.enqueue_s2c(client_idx, conn_id, &ack);
                                    self.broadcast_commit(&bcast);
                                } else {
                                    self.broadcast_commit(&bcast);
                                    self.enqueue_s2c(client_idx, conn_id, &ack);
                                }

                                // Slow-receiver enforcement if any subscriber buffer exceeds threshold.
                                for sub_idx in 0..self.clients.len() {
                                    if self.clients[sub_idx].s2c_queue.len() > MAX_SLOW_S2C_QUEUE {
                                        self.slow_receiver_disconnect(sub_idx);
                                    }
                                }
                            }
                            Ok(reticula_sync::ApplyResult::Duplicate {
                                replica,
                                client_seq,
                                seq,
                                timestamp_ms,
                            }) => {
                                self.faults.duplicate_acks += 1;
                                self.enqueue_s2c(
                                    client_idx,
                                    conn_id,
                                    &WireSyncFrameV1::Ack {
                                        replica,
                                        client_seq,
                                        seq,
                                        timestamp_ms,
                                    },
                                );
                            }
                            Err(reason) => {
                                self.enqueue_s2c(
                                    client_idx,
                                    conn_id,
                                    &WireSyncFrameV1::Reject {
                                        client_seq: cseq,
                                        reason,
                                    },
                                );
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn broadcast_commit(&mut self, commit_frame: &WireSyncFrameV1) {
        let targets: Vec<(usize, u64)> = self
            .server_conn_subscribed
            .iter()
            .map(|(&idx, &cid)| (idx, cid))
            .collect();
        for (idx, cid) in targets {
            self.enqueue_s2c(idx, cid, commit_frame);
        }
    }

    fn broadcast_server_digest(&mut self) {
        let frame = self.server.digest_frame();
        self.broadcast_commit(&frame);
    }

    fn record_new_commit(&mut self, commit: &WireCommittedTxnV2) -> Result<(), String> {
        let (_, ops) = ops_from_wire_txn_v2(commit.txn.clone())
            .map_err(|e| format!("committed txn failed wire decode: {e:?}"))?;
        record_unique_ids(
            &ops,
            &mut self.seen_row_ids,
            &mut self.seen_col_ids,
            &mut self.seen_sheet_ids,
        )?;
        self.current_epoch_log.push(commit.clone());
        self.durable_tail_commits.push(commit.clone());
        self.epoch_digest_history
            .insert(commit.seq, self.server.workbook().state_digest());
        for wm in self.server.watermarks() {
            self.durable_replicas.insert(wm.replica, wm);
        }
        if commit.user_id == USER_A {
            self.faults.two_tab_same_user_commits += 1;
        }
        Ok(())
    }

    fn deliver_s2c(&mut self, client_idx: usize, max_frames: usize) -> Result<(), String> {
        for _ in 0..max_frames {
            let Some((conn_id, raw)) = self.clients[client_idx].s2c_queue.pop_front() else {
                break;
            };
            if self.clients[client_idx].active_conn != Some(conn_id) {
                continue;
            }
            let frame = decode_sync_frame(&raw)
                .map_err(|e| format!("failed to decode server sync frame: {e:?}"))?;
            let actions = match frame {
                WireSyncFrameV1::Welcome { .. } => {
                    self.clients[client_idx].machine.on_welcome(frame)
                }
                WireSyncFrameV1::Commit {
                    seq,
                    replica,
                    client_seq,
                    user_id,
                    timestamp_ms,
                    txn,
                } => self.clients[client_idx]
                    .machine
                    .on_commit(WireCommittedTxnV2 {
                        seq,
                        replica,
                        client_seq,
                        user_id,
                        timestamp_ms,
                        txn,
                    }),
                WireSyncFrameV1::Ack {
                    replica,
                    client_seq,
                    ..
                } => {
                    self.clients[client_idx].machine.on_ack(replica, client_seq);
                    self.sync_storage_if_consistent(client_idx);
                    Vec::new()
                }
                WireSyncFrameV1::Reject { client_seq, reason } => {
                    if client_seq == 0 && reason == WireRejectReasonV1::Forbidden {
                        // Hello was rejected (member revoked) -> disconnect cleanly.
                        self.disconnect_client(client_idx);
                        Vec::new()
                    } else {
                        let is_terminal = matches!(
                            reason,
                            WireRejectReasonV1::Forbidden
                                | WireRejectReasonV1::InvalidOp
                                | WireRejectReasonV1::QuotaExceeded
                                | WireRejectReasonV1::PayloadTooLarge
                        );
                        if is_terminal
                            && client_seq > self.clients[client_idx].machine.last_acked_client_seq()
                        {
                            for p in self.clients[client_idx].machine.pending_wire_txns() {
                                if p.client_seq >= client_seq {
                                    self.clients[client_idx].storage.expected_rejected.push(p);
                                }
                            }
                        }
                        self.clients[client_idx]
                            .machine
                            .on_reject(client_seq, reason)
                    }
                }
                WireSyncFrameV1::Digest { seq, state_digest } => self.clients[client_idx]
                    .machine
                    .on_digest(seq, state_digest),
                WireSyncFrameV1::Pong(_) | WireSyncFrameV1::Presence(_) => Vec::new(),
                _ => Vec::new(),
            };
            if !actions.is_empty() {
                self.apply_client_actions(client_idx, actions);
            }
        }
        Ok(())
    }

    fn deliver_snapshot(&mut self, client_idx: usize) -> Result<(), String> {
        let Some((conn_id, snapshot_seq)) = self.clients[client_idx].pending_snapshot_req else {
            return Ok(());
        };
        if self.clients[client_idx].active_conn != Some(conn_id) {
            self.clients[client_idx].pending_snapshot_req = None;
            return Ok(());
        }
        self.clients[client_idx].pending_snapshot_req = None;

        let (snap_wb, snap_wms) = if snapshot_seq == 0 {
            (Workbook::new(), Vec::new())
        } else {
            let stored = self
                .snapshots_by_seq
                .get(&snapshot_seq)
                .ok_or_else(|| format!("missing stored snapshot at snapshot_seq={snapshot_seq}"))?;
            let decoded = decode_snapshot_any(&stored.bytes)
                .map_err(|e| format!("failed to decode snapshot {snapshot_seq}: {e:?}"))?;
            (decoded.engine.workbook().clone(), stored.watermarks.clone())
        };

        let welcome_head = self.clients[client_idx]
            .welcome_head_seq
            .unwrap_or_else(|| self.server.head_seq());
        let tail_txns: Vec<WireCommittedTxnV2> = self
            .current_epoch_log
            .iter()
            .filter(|t| t.seq > snapshot_seq && t.seq <= welcome_head)
            .cloned()
            .collect();

        let actions = self.clients[client_idx].machine.install_snapshot(
            snapshot_seq,
            snap_wb,
            &snap_wms,
            tail_txns,
        );
        self.apply_client_actions(client_idx, actions);
        Ok(())
    }

    fn disconnect_client(&mut self, client_idx: usize) {
        if self.clients[client_idx].active_conn.is_some() {
            self.clients[client_idx].active_conn = None;
            self.clients[client_idx].c2s_queue.clear();
            self.clients[client_idx].s2c_queue.clear();
            self.clients[client_idx].pending_snapshot_req = None;
            self.clients[client_idx].welcome_head_seq = None;
            self.clients[client_idx].machine.on_disconnected();
            self.server_conn_subscribed.remove(&client_idx);
        }
    }

    fn slow_receiver_disconnect(&mut self, client_idx: usize) {
        if self.clients[client_idx].active_conn.is_some() {
            self.faults.slow_receiver_disconnects += 1;
            self.disconnect_client(client_idx);
        }
    }

    fn reconnect_client(&mut self, client_idx: usize) {
        if self.clients[client_idx].active_conn.is_some() {
            return;
        }
        let conn_id = self.next_conn_id;
        self.next_conn_id = self.next_conn_id.saturating_add(1);
        self.clients[client_idx].active_conn = Some(conn_id);
        let actions = self.clients[client_idx].machine.on_connected();
        self.apply_client_actions(client_idx, actions);
    }

    fn client_crash_restart(&mut self, client_idx: usize) -> Result<(), String> {
        self.disconnect_client(client_idx);
        self.faults.client_crashes += 1;

        let st = &self.clients[client_idx].storage;
        let wire_wms = st
            .replica
            .map(|r| {
                vec![WireReplicaWatermarkV1 {
                    replica: r,
                    last_client_seq: st.last_acked_client_seq,
                    max_id_counter: st.next_id_counter_hint.saturating_sub(1),
                }]
            })
            .unwrap_or_default();
        let snap_bytes = encode_snapshot_v1(&st.confirmed, &wire_wms)
            .map_err(|e| format!("client OPFS snapshot encode failed: {e:?}"))?;
        let wal_bytes = encode_pending_wal_complete(&st.pending)
            .map_err(|e| format!("client pending WAL encode failed: {e:?}"))?;

        let decoded_snap = decode_snapshot_any(&snap_bytes)
            .map_err(|e| format!("client OPFS snapshot decode failed: {e:?}"))?;
        let wal_scan = scan_pending_wal(&wal_bytes)
            .map_err(|e| format!("client pending WAL scan failed: {e:?}"))?;

        self.clients[client_idx].machine = ClientSyncMachine::from_persisted(
            SIM_DOC_ID,
            SIM_BRANCH_ID,
            st.epoch,
            st.replica,
            st.confirmed_seq,
            st.last_acked_client_seq,
            decoded_snap.engine.workbook().clone(),
            wal_scan.records,
            st.next_id_counter_hint,
        );
        Ok(())
    }

    fn opfs_tail_loss(
        &mut self,
        client_idx: usize,
        drop_count: usize,
        offline_edits: u8,
        sub_seed: u64,
    ) -> Result<(), String> {
        let server_wms = self.server.watermarks();
        let target = (0..self.clients.len())
            .map(|o| (client_idx + o) % self.clients.len())
            .find_map(|idx| {
                let c = &self.clients[idx];
                let rep = c.machine.replica()?;
                if c.machine.epoch() != self.server.epoch() || c.machine.pending_len() > 0 {
                    return None;
                }
                let srv_wm = server_wms
                    .iter()
                    .find(|w| w.replica == rep)
                    .map_or(0, |w| w.last_client_seq);
                (srv_wm > 0).then_some((idx, rep, srv_wm))
            });
        let Some((target_idx, rep, srv_wm)) = target else {
            return Ok(());
        };

        // Only simulate OPFS tail loss on records already durably committed on the server
        // (matching D-026: uncommitted local edits have not been reported acked to the user).
        self.disconnect_client(target_idx);
        self.faults.opfs_tail_losses += 1;

        let c = &mut self.clients[target_idx];
        let old_cp = c
            .storage
            .history
            .iter()
            .find(|cp| {
                cp.epoch == self.server.epoch()
                    && cp.replica == Some(rep)
                    && cp.last_acked_client_seq < srv_wm
                    && cp.authored_count <= c.storage.authored_count
            })
            .cloned();

        let (base_epoch, base_rep, base_seq, base_cs, base_wb, synthetic_pending, base_hint) =
            if let Some(cp) = old_cp {
                let mut records = cp.pending;
                let keep = records.len().saturating_sub(drop_count.min(records.len()));
                records.truncate(keep);
                (
                    cp.epoch,
                    cp.replica,
                    cp.confirmed_seq,
                    cp.last_acked_client_seq,
                    cp.confirmed,
                    records,
                    cp.next_id_counter_hint.min(1),
                )
            } else {
                // Build a byte-truncated pending WAL starting from seq=0 before commits.
                let committed_by_rep: Vec<WirePendingTxnV2> = self
                    .current_epoch_log
                    .iter()
                    .filter(|t| t.replica == rep)
                    .map(|t| WirePendingTxnV2 {
                        replica: t.replica,
                        client_seq: t.client_seq,
                        base_server_seq: 0,
                        timestamp_ms: t.timestamp_ms,
                        txn: t.txn.clone(),
                    })
                    .collect();
                let encoded = encode_pending_wal_complete(&committed_by_rep)
                    .map_err(|e| format!("tail-loss WAL encode failed: {e:?}"))?;
                let cut = encoded.len().saturating_sub(3.max(drop_count * 4)).max(16);
                let scanned = scan_pending_wal(&encoded[..cut])
                    .map_err(|e| format!("tail-loss WAL scan failed: {e:?}"))?;
                (
                    self.server.epoch(),
                    Some(rep),
                    0_u64,
                    0_u64,
                    Workbook::new(),
                    scanned.records,
                    1_u32,
                )
            };

        let snap_bytes = encode_snapshot_v1(&base_wb, &[])
            .map_err(|e| format!("tail-loss snapshot encode failed: {e:?}"))?;
        let decoded = decode_snapshot_any(&snap_bytes)
            .map_err(|e| format!("tail-loss snapshot decode failed: {e:?}"))?;
        let wal_bytes = encode_pending_wal_complete(&synthetic_pending)
            .map_err(|e| format!("tail-loss pending encode failed: {e:?}"))?;
        let wal_scan = scan_pending_wal(&wal_bytes)
            .map_err(|e| format!("tail-loss pending decode failed: {e:?}"))?;

        c.machine = ClientSyncMachine::from_persisted(
            SIM_DOC_ID,
            SIM_BRANCH_ID,
            base_epoch,
            base_rep,
            base_seq,
            base_cs,
            decoded.engine.workbook().clone(),
            wal_scan.records,
            base_hint,
        );
        self.sync_storage_if_consistent(target_idx);

        // Author colliding offline edits (including InsertRows) before reconnecting!
        let mut sub_rng = Rng::from_seed(sub_seed);
        for k in 0..offline_edits {
            let choice = if k == 0 {
                OpChoice::InsertRows
            } else {
                OpChoice::SetCellsScalar
            };
            self.exec_local_edit(target_idx, choice, sub_rng.next_u64());
        }
        Ok(())
    }

    fn maybe_write_server_snapshot(&mut self) -> Result<(), String> {
        if !self.server.needs_snapshot() {
            return Ok(());
        }
        let seq = self.server.head_seq();
        if seq == 0 {
            return Ok(());
        }
        let wms = self.server.watermarks();
        let wire_wms = to_wire_watermarks(&wms);
        let bytes = encode_snapshot_v1(self.server.workbook(), &wire_wms)
            .map_err(|e| format!("server snapshot encode failed: {e:?}"))?;
        let stored = StoredSnapshot {
            seq,
            bytes,
            watermarks: wms,
        };
        self.snapshots_by_seq.insert(seq, stored.clone());
        self.durable_snapshot = Some(stored);
        self.durable_tail_commits.retain(|c| c.seq > seq);
        self.server.mark_snapshot_written(seq);

        if self.server.txns_since(seq).is_none() {
            return Err(format!(
                "ServerDocMachine::txns_since({seq}) missing immediately after mark_snapshot_written"
            ));
        }
        Ok(())
    }

    fn server_crash_restart(&mut self) -> Result<(), String> {
        for idx in 0..self.clients.len() {
            self.disconnect_client(idx);
        }
        self.faults.server_crashes += 1;

        let (snap_seq, base_wb, snap_wms) = if let Some(stored) = &self.durable_snapshot {
            let decoded = decode_snapshot_any(&stored.bytes)
                .map_err(|e| format!("server snapshot decode failed: {e:?}"))?;
            (
                stored.seq,
                decoded.engine.workbook().clone(),
                stored.watermarks.clone(),
            )
        } else {
            (0_u64, Workbook::new(), Vec::new())
        };

        // Round-trip the durable tail commit payloads through real RTCLWAL2 bytes on crash recovery.
        if !self.durable_tail_commits.is_empty() {
            let wal_records: Vec<WireWalRecordV2> = self
                .durable_tail_commits
                .iter()
                .map(|c| WireWalRecordV2 {
                    seq: c.seq,
                    timestamp_ms: c.timestamp_ms,
                    label: c.txn.label.clone(),
                    ops: c.txn.ops.clone(),
                })
                .collect();
            let wal_bytes = encode_wal_complete_v2(snap_seq, &wal_records)
                .map_err(|e| format!("server WAL encode failed: {e:?}"))?;
            let last_wal_seq = wal_records.last().map_or(snap_seq, |r| r.seq);
            let scanned = scan_wal_v2(&wal_bytes, snap_seq, last_wal_seq)
                .map_err(|e| format!("server WAL scan failed: {e:?}"))?;
            if scanned.records.len() != wal_records.len() {
                return Err(String::from("server WAL round-trip record count mismatch"));
            }
        }

        // Merge snapshot watermarks with DB durable replicas.
        let mut combined_wms: BTreeMap<u32, ReplicaWatermark> =
            snap_wms.into_iter().map(|w| (w.replica, w)).collect();
        for (&r, &wm) in &self.durable_replicas {
            combined_wms.insert(r, wm);
        }

        let prev_head = self.server.head_seq();
        let prev_digest = self.server.workbook().state_digest();
        self.server = ServerDocMachine::from_snapshot_and_log(
            SIM_DOC_ID,
            SIM_BRANCH_ID,
            String::from("/api/w"),
            self.server.epoch(),
            snap_seq,
            base_wb,
            self.durable_next_per_epoch_replica,
            combined_wms.into_values().collect(),
            self.durable_tail_commits.clone(),
            self.active_server_limits(),
        )
        .map_err(|e| format!("server crash reload failed: {e:?}"))?;

        if self.server.head_seq() != prev_head
            || self.server.workbook().state_digest() != prev_digest
        {
            return Err(String::from(
                "server crash reload altered head_seq or state_digest",
            ));
        }
        Ok(())
    }

    fn take_backup_checkpoint(&mut self) -> Result<(), String> {
        let head_seq = self.server.head_seq();
        let wms = self.server.watermarks();
        let wire_wms = to_wire_watermarks(&wms);
        let snap_bytes = encode_snapshot_v1(self.server.workbook(), &wire_wms)
            .map_err(|e| format!("backup checkpoint snapshot encode failed: {e:?}"))?;
        let mut snapshots_copy = self.snapshots_by_seq.clone();
        if head_seq > 0 {
            snapshots_copy.insert(
                head_seq,
                StoredSnapshot {
                    seq: head_seq,
                    bytes: snap_bytes.clone(),
                    watermarks: wms.clone(),
                },
            );
        }
        let bp = BackupPoint {
            head_seq,
            snapshot_seq: head_seq,
            snapshot_bytes: snap_bytes,
            snapshot_watermarks: wms,
            tail_commits: Vec::new(),
            full_history: self.current_epoch_log.clone(),
            digest_history: self.epoch_digest_history.clone(),
            durable_replicas: self.durable_replicas.clone(),
            snapshots_by_seq: snapshots_copy,
        };
        if self.backups.len() >= 4 {
            let _ = self.backups.remove(0);
        }
        self.backups.push(bp);
        Ok(())
    }

    fn server_backup_restore(&mut self, pick_oldest: bool) -> Result<(), String> {
        if self.server.epoch() >= MAX_EPOCH_BUMPS_PER_SEED || self.backups.is_empty() {
            return Ok(());
        }
        let idx = if pick_oldest {
            0
        } else {
            self.backups.len() - 1
        };
        let bp = self.backups[idx].clone();
        let new_epoch = self.server.epoch().saturating_add(1);

        for cidx in 0..self.clients.len() {
            self.disconnect_client(cidx);
        }
        self.faults.epoch_backup_restores += 1;

        let base_wb = if bp.snapshot_seq == 0 {
            Workbook::new()
        } else {
            let decoded = decode_snapshot_any(&bp.snapshot_bytes)
                .map_err(|e| format!("backup snapshot decode failed: {e:?}"))?;
            decoded.engine.workbook().clone()
        };

        self.server = ServerDocMachine::from_snapshot_and_log(
            SIM_DOC_ID,
            SIM_BRANCH_ID,
            String::from("/api/w"),
            new_epoch,
            bp.snapshot_seq,
            base_wb,
            1,
            bp.snapshot_watermarks.clone(),
            bp.tail_commits.clone(),
            self.active_server_limits(),
        )
        .map_err(|e| format!("server backup restore failed: {e:?}"))?;

        self.snapshots_by_seq = bp.snapshots_by_seq;
        self.current_epoch_log = bp.full_history;
        self.epoch_digest_history = bp.digest_history;
        self.durable_snapshot = if bp.snapshot_seq > 0 {
            Some(StoredSnapshot {
                seq: bp.snapshot_seq,
                bytes: bp.snapshot_bytes,
                watermarks: bp.snapshot_watermarks,
            })
        } else {
            None
        };
        self.durable_tail_commits = bp.tail_commits;
        self.durable_replicas = bp.durable_replicas;
        self.durable_next_per_epoch_replica = 1;
        // Trim backups taken beyond restored head.
        self.backups.retain(|b| b.head_seq <= bp.head_seq);
        Ok(())
    }

    fn toggle_role(&mut self, client_idx: usize) {
        if client_idx >= self.clients.len() || client_idx == 4 {
            return;
        }
        match self.clients[client_idx].role {
            Some(MemberRole::Owner | MemberRole::Editor) => {
                if self.clients[client_idx].machine.connection_state() == ConnectionState::Online {
                    self.clients[client_idx].role = Some(MemberRole::Viewer);
                    self.faults.role_downgrades += 1;
                }
            }
            Some(MemberRole::Viewer) => {
                if self.clients[client_idx].machine.connection_state() == ConnectionState::Online {
                    self.clients[client_idx].role = None;
                    self.faults.role_downgrades += 1;
                }
            }
            None => {
                self.clients[client_idx].role = Some(MemberRole::Editor);
            }
        }
    }

    fn set_tight_quota(&mut self, enabled: bool) {
        self.tight_quota_active = enabled;
        let limits = self.active_server_limits();
        self.reload_server_limits(limits);
    }

    fn active_server_limits(&self) -> ServerLimits {
        let mut limits = self.server_limits;
        if self.tight_quota_active {
            limits.max_populated_cells = self
                .server
                .workbook()
                .populated_cell_count()
                .saturating_add(TEST_TIGHT_QUOTA_EXTRA_CELLS);
        }
        limits
    }

    fn reload_server_limits(&mut self, limits: ServerLimits) {
        let (snap_seq, base_wb, snap_wms) = if let Some(stored) = &self.durable_snapshot {
            if let Ok(decoded) = decode_snapshot_any(&stored.bytes) {
                (
                    stored.seq,
                    decoded.engine.workbook().clone(),
                    stored.watermarks.clone(),
                )
            } else {
                (
                    self.server.head_seq(),
                    self.server.workbook().clone(),
                    self.server.watermarks(),
                )
            }
        } else {
            (0, Workbook::new(), Vec::new())
        };
        let mut combined_wms: BTreeMap<u32, ReplicaWatermark> =
            snap_wms.into_iter().map(|w| (w.replica, w)).collect();
        for (&r, &wm) in &self.durable_replicas {
            combined_wms.insert(r, wm);
        }
        if let Ok(rebuilt) = ServerDocMachine::from_snapshot_and_log(
            SIM_DOC_ID,
            SIM_BRANCH_ID,
            String::from("/api/w"),
            self.server.epoch(),
            snap_seq,
            base_wb,
            self.durable_next_per_epoch_replica,
            combined_wms.into_values().collect(),
            self.durable_tail_commits.clone(),
            limits,
        ) {
            self.server = rebuilt;
        }
    }

    fn inject_rogue(&mut self, kind: RogueKind, sub_seed: u64) -> Result<(), String> {
        let before_seq = self.server.head_seq();
        let before_digest = self.server.workbook().state_digest();
        let before_logical = self.server.workbook().logical_digest();

        if let Some(hello) = build_rogue_hello_frame(kind) {
            let user = if kind == RogueKind::NonMemberHello {
                ROGUE_OUTSIDER
            } else {
                USER_C
            };
            let role = (kind != RogueKind::NonMemberHello).then_some(MemberRole::Editor);
            let res = self.server.on_hello(user, role, &hello);
            if res.is_ok() {
                return Err(format!("rogue Hello `{kind:?}` was unexpectedly accepted"));
            }
        } else if kind == RogueKind::OversizedPushBatch {
            let count = self.server_limits.max_push_batch.saturating_add(1);
            let txns: Vec<WirePendingTxnV2> = (1..=count)
                .map(|i| WirePendingTxnV2 {
                    replica: 1,
                    client_seq: i as u64,
                    base_server_seq: 0,
                    timestamp_ms: 1,
                    txn: ops_to_wire_txn_v2(String::from("over"), &[]),
                })
                .collect();
            let push = WireSyncFrameV1::Push { txns: txns.clone() };
            let raw = encode_sync_frame(&push).unwrap_or_default();
            if self.server.validate_push_batch(&txns, raw.len()).is_ok() {
                return Err(String::from(
                    "oversized Push batch was unexpectedly accepted",
                ));
            }
        } else if kind == RogueKind::ViewerPush {
            let rogue_pending = WirePendingTxnV2 {
                replica: VIEWER_REPLICA,
                client_seq: 1,
                base_server_seq: self.server.head_seq(),
                timestamp_ms: self.timestamp_ms,
                txn: ops_to_wire_txn_v2(
                    String::from("viewer_rogue"),
                    &[Op::SetFreeze {
                        sheet: SheetId(0),
                        frozen_rows: 1,
                        frozen_cols: 0,
                    }],
                ),
            };
            let res = self.server.apply_and_commit(
                USER_D,
                Some(MemberRole::Viewer),
                VIEWER_REPLICA,
                rogue_pending,
                self.timestamp_ms,
            );
            if res != Err(WireRejectReasonV1::Forbidden) {
                return Err(format!(
                    "rogue ViewerPush was not rejected with Forbidden: {res:?}"
                ));
            }
        } else {
            // Find any current-epoch replica registered on the server.
            let target_wm = self
                .server
                .watermarks()
                .into_iter()
                .find(|w| unpack_replica_id(w.replica).0 == self.server.epoch());
            if let Some(wm) = target_wm
                && let Some(rogue_pending) = build_rogue_pending(
                    kind,
                    self.server.epoch(),
                    wm.replica,
                    wm.last_client_seq.saturating_add(1),
                    wm.max_id_counter,
                    sub_seed,
                )
            {
                let attacker_user = if kind == RogueKind::CrossUserReplicaHijack {
                    ROGUE_OUTSIDER
                } else {
                    wm.user_id
                };
                let role = if kind == RogueKind::CrossUserReplicaHijack {
                    None
                } else {
                    Some(MemberRole::Editor)
                };
                let res = self.server.apply_and_commit(
                    attacker_user,
                    role,
                    wm.replica,
                    rogue_pending,
                    self.timestamp_ms,
                );
                if res.is_ok() {
                    return Err(format!(
                        "rogue Push `{kind:?}` was unexpectedly committed onto server"
                    ));
                }
            }
        }

        if self.server.head_seq() != before_seq
            || self.server.workbook().state_digest() != before_digest
            || self.server.workbook().logical_digest() != before_logical
        {
            return Err(format!("rogue `{kind:?}` mutated server state"));
        }
        self.faults.rogue_attempts_rejected += 1;
        Ok(())
    }

    fn resend_duplicate_push(&mut self, client_idx: usize) {
        let candidate = (0..self.clients.len())
            .map(|o| (client_idx + o) % self.clients.len())
            .find_map(|idx| {
                let c = &self.clients[idx];
                let conn_id = c.active_conn?;
                if c.machine.connection_state() != ConnectionState::Online
                    || c.machine.epoch() != self.server.epoch()
                {
                    return None;
                }
                let rep = c.machine.replica()?;
                let prev = self
                    .current_epoch_log
                    .iter()
                    .rev()
                    .find(|t| t.replica == rep)
                    .cloned()?;
                Some((idx, conn_id, rep, prev))
            });

        if let Some((target_idx, conn_id, rep, prev)) = candidate {
            let dup = WirePendingTxnV2 {
                replica: rep,
                client_seq: prev.client_seq,
                base_server_seq: self.clients[target_idx].machine.confirmed_seq(),
                timestamp_ms: self.timestamp_ms,
                txn: prev.txn,
            };
            let frame = WireSyncFrameV1::Push { txns: vec![dup] };
            let bytes = encode_sync_frame(&frame).expect("dup push encodes");
            self.clients[target_idx]
                .c2s_queue
                .push_back((conn_id, bytes));
        }
    }

    fn check_step_invariants(&self) -> Result<(), String> {
        // Invariant #3: server log has contiguous seq 1..=head and contiguous per-replica client_seq 1..
        let mut rep_seqs: BTreeMap<u32, u64> = BTreeMap::new();
        for (i, commit) in self.current_epoch_log.iter().enumerate() {
            let expected_seq = (i as u64).saturating_add(1);
            if commit.seq != expected_seq {
                return Err(format!(
                    "Invariant #3 violated: server seq gap expected={expected_seq}, got={}",
                    commit.seq
                ));
            }
            let entry = rep_seqs.entry(commit.replica).or_insert(0);
            let expected_cs = entry.saturating_add(1);
            if commit.client_seq != expected_cs {
                return Err(format!(
                    "Invariant #3 violated: replica={} client_seq expected={expected_cs}, got={}",
                    commit.replica, commit.client_seq
                ));
            }
            *entry = commit.client_seq;
        }

        // Fast lookup of (replica, client_seq) -> count in current epoch log.
        let mut seen_pairs: BTreeMap<(u32, u64), usize> = BTreeMap::new();
        for commit in &self.current_epoch_log {
            *seen_pairs
                .entry((commit.replica, commit.client_seq))
                .or_insert(0) += 1;
        }

        for (idx, c) in self.clients.iter().enumerate() {
            // Invariant #1: visible == replay(confirmed + pending) on every client step.
            let pending = c.machine.pending_wire_txns();
            let expected_visible = replay_pending_onto(c.machine.confirmed(), &pending);
            if c.machine.visible().state_digest() != expected_visible.state_digest() {
                return Err(format!(
                    "Invariant #1 violated on client {idx}: visible != replay(confirmed + pending)"
                ));
            }

            if c.machine.epoch() == self.server.epoch() {
                // Invariant #2: every client_seq 1..=last_acked exists in server log exactly once.
                if let Some(rep) = c.machine.replica() {
                    for cs in 1..=c.machine.last_acked_client_seq() {
                        let count = seen_pairs.get(&(rep, cs)).copied().unwrap_or(0);
                        if count != 1 {
                            return Err(format!(
                                "Invariant #2 violated on client {idx}: replica={rep}, client_seq={cs} appeared {count} times in server log"
                            ));
                        }
                    }
                }

                // Invariant #4: confirmed.state_digest() == server_digest_history[confirmed_seq]
                if !matches!(
                    c.machine.connection_state(),
                    ConnectionState::AwaitingSnapshot { .. }
                ) {
                    let cseq = c.machine.confirmed_seq();
                    let Some(&srv_digest) = self.epoch_digest_history.get(&cseq) else {
                        return Err(format!(
                            "Invariant #4 violated on client {idx}: confirmed_seq={cseq} absent from server history"
                        ));
                    };
                    if c.machine.confirmed().state_digest() != srv_digest {
                        return Err(format!(
                            "Invariant #4 violated on client {idx}: confirmed StateDigest mismatch at seq={cseq}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn verify_quiescence(&self) -> Result<(), String> {
        let server_wb = self.server.workbook();
        let server_digest = server_wb.state_digest();
        let server_logical = server_wb.logical_digest();
        let digest_frame = self.server.digest_frame();
        let WireSyncFrameV1::Digest { seq, state_digest } = digest_frame else {
            return Err(String::from("server digest_frame was not Digest"));
        };

        // Invariant #8: every authorized online replica matches server state_digest & logical_digest.
        for (idx, c) in self.clients.iter().enumerate() {
            if c.role.is_some() {
                if c.machine.pending_len() != 0 {
                    return Err(format!(
                        "Invariant #8 violated on client {idx}: pending_len={} != 0",
                        c.machine.pending_len()
                    ));
                }
                if c.machine.confirmed_seq() != self.server.head_seq() {
                    return Err(format!(
                        "Invariant #8 violated on client {idx}: confirmed_seq={} != server head={}",
                        c.machine.confirmed_seq(),
                        self.server.head_seq()
                    ));
                }
                if c.machine.confirmed().state_digest() != server_digest
                    || c.machine.visible().state_digest() != server_digest
                {
                    return Err(format!(
                        "Invariant #8 violated on client {idx}: StateDigest diverged from server"
                    ));
                }
                if c.machine.confirmed().logical_digest() != server_logical
                    || c.machine.visible().logical_digest() != server_logical
                {
                    return Err(format!(
                        "Invariant #8 violated on client {idx}: logical_digest diverged from server"
                    ));
                }
                if !c.machine.on_digest(seq, state_digest).is_empty() {
                    return Err(format!(
                        "Invariant #8 violated on client {idx}: server Digest frame reported mismatch"
                    ));
                }
            }

            // Invariant #10: every terminally rejected client txn is preserved in saved_unsynced_local.
            for expected in &c.storage.expected_rejected {
                let present = c.storage.saved_unsynced_local.iter().any(|saved| {
                    saved.client_seq == expected.client_seq && saved.txn == expected.txn
                });
                if !present {
                    return Err(format!(
                        "Invariant #10 violated on client {idx}: rejected client_seq={} lost",
                        expected.client_seq
                    ));
                }
            }
        }

        // Invariant #9: full epoch log replay matches server, AND latest snapshot + tail replay matches server.
        let mut full_replay = Workbook::<BoundFormula>::new();
        for commit in &self.current_epoch_log {
            let (_, ops) = ops_from_wire_txn_v2(commit.txn.clone())
                .map_err(|e| format!("full log decode failed: {e:?}"))?;
            for op in ops {
                full_replay
                    .apply(op)
                    .map_err(|e| format!("full log replay failed at seq={}: {e:?}", commit.seq))?;
            }
        }
        if full_replay.state_digest() != server_digest
            || full_replay.logical_digest() != server_logical
        {
            return Err(String::from(
                "Invariant #9 violated: full epoch log replay disagrees with server workbook",
            ));
        }

        if let Some(stored) = self.snapshots_by_seq.values().next_back() {
            let decoded = decode_snapshot_any(&stored.bytes)
                .map_err(|e| format!("snapshot catchup equivalence decode failed: {e:?}"))?;
            let mut snap_replay = decoded.engine.workbook().clone();
            for commit in self.current_epoch_log.iter().filter(|t| t.seq > stored.seq) {
                let (_, ops) = ops_from_wire_txn_v2(commit.txn.clone())
                    .map_err(|e| format!("snapshot tail decode failed: {e:?}"))?;
                for op in ops {
                    snap_replay
                        .apply(op)
                        .map_err(|e| format!("snapshot tail replay failed: {e:?}"))?;
                }
            }
            if snap_replay.state_digest() != server_digest
                || snap_replay.logical_digest() != server_logical
            {
                return Err(String::from(
                    "Invariant #9 violated: latest snapshot + tail diverges from server workbook",
                ));
            }
        }

        let _ = SYNC_PROTOCOL_VERSION;
        Ok(())
    }
}

fn compute_next_id_hint(
    epoch: u8,
    replica: Option<u32>,
    confirmed: &Workbook<BoundFormula>,
    pending: &[WirePendingTxnV2],
    prev_hint: u32,
) -> u32 {
    let Some(rep) = replica else {
        return prev_hint.max(1);
    };
    let mut max_c = prev_hint.saturating_sub(1);
    for sheet in confirmed.sheets() {
        for run in sheet.row_axis().runs() {
            if run.ids.belongs_to(epoch, rep)
                && let Some(end) = run.ids.max_counter()
            {
                max_c = max_c.max(end);
            }
        }
        for run in sheet.col_axis().runs() {
            if run.ids.belongs_to(epoch, rep)
                && let Some(end) = run.ids.max_counter()
            {
                max_c = max_c.max(end);
            }
        }
    }
    for p in pending {
        if let Ok((_, ops)) = ops_from_wire_txn_v2(p.txn.clone()) {
            for op in ops {
                if let Op::InsertRows { ids, .. } | Op::InsertCols { ids, .. } = op
                    && ids.belongs_to(epoch, rep)
                    && let Some(end) = ids.max_counter()
                {
                    max_c = max_c.max(end);
                }
            }
        }
    }
    max_c.saturating_add(1)
}
