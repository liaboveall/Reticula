//! Browser bindings for the pure collaborative sync machine.

use std::collections::{BTreeMap, VecDeque};

use reticula_core::id::{MAX_REPLICA, SheetId, unpack_replica_id};
use reticula_core::ops::{Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_sync::types::DEFAULT_MAX_PUSH_BYTES;
use reticula_sync::{ClientAction, ClientSyncMachine, ConnectionState, ReplicaWatermark};
use reticula_wire::convert_v2::{ops_from_wire_txn_v2, ops_to_wire_txn_v2};
use reticula_wire::snapshot::{decode_snapshot_any, encode_snapshot_v1};
use reticula_wire::sync_frame::{decode_sync_frame, encode_sync_frame};
use reticula_wire::v1::WireReplicaWatermarkV1;
use reticula_wire::v2::{
    WireCommittedTxnV2, WirePendingTxnV2, WirePresenceRangeV1, WirePresenceV1, WireSyncFrameV1,
};
use reticula_wire::wal::{
    encode_pending_wal_complete, encode_pending_wal_record, scan_pending_wal,
};
use serde::{Deserialize, Serialize};
use tsify::{Ts, Tsify};
use uuid::Uuid;
use wasm_bindgen::prelude::*;

use crate::{Engine, EngineEvent, check_not_panicked, now_ms};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SyncCheckpointInfo {
    pub epoch: u8,
    pub replica: Option<u32>,
    #[serde(with = "crate::wire_u64")]
    #[tsify(type = "string")]
    pub confirmed_seq: u64,
    #[serde(with = "crate::wire_u64")]
    #[tsify(type = "string")]
    pub confirmed_client_seq: u64,
    pub next_id_counter: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SyncCheckpoint {
    pub info: SyncCheckpointInfo,
    #[serde(with = "serde_bytes")]
    #[tsify(type = "Uint8Array")]
    pub snapshot: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct PresenceSpan {
    pub first_row: u32,
    pub end_row: u32,
    pub first_col: u32,
    pub end_col: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct RemotePresence {
    pub replica: u32,
    pub display_name: String,
    pub color_slot: u8,
    pub cursor_row: u32,
    pub cursor_col: u32,
    pub selection: Option<PresenceSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SyncCommand {
    Open {
        doc_id: String,
        branch_id: String,
        info: Option<SyncCheckpointInfo>,
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        snapshot: Vec<u8>,
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        pending_wal: Vec<u8>,
    },
    Connected,
    Disconnected,
    Receive {
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        frame: Vec<u8>,
    },
    InstallSnapshot {
        #[serde(with = "crate::wire_u64")]
        #[tsify(type = "string")]
        seq: u64,
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        snapshot: Vec<u8>,
    },
    Checkpoint,
    LocalPresence {
        sheet: u32,
        cursor_row: u32,
        cursor_col: u32,
        selection: Option<PresenceSpan>,
    },
    ExpirePresence,
}

/// Process events in order. A pending WAL write (or rescue snapshot) must be
/// durable before the host executes a following send or truncation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CollabEvent {
    AppendPending {
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        bytes: Vec<u8>,
    },
    ReplacePending {
        info: SyncCheckpointInfo,
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        bytes: Vec<u8>,
    },
    Send {
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        bytes: Vec<u8>,
    },
    FetchSnapshot {
        #[serde(with = "crate::wire_u64")]
        #[tsify(type = "string")]
        seq: u64,
        url: String,
    },
    SaveUnsynced {
        #[serde(with = "crate::wire_u64")]
        #[tsify(type = "string")]
        from_client_seq: u64,
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        snapshot: Vec<u8>,
        #[serde(with = "serde_bytes")]
        #[tsify(type = "Uint8Array")]
        pending_wal: Vec<u8>,
    },
    Checkpoint {
        checkpoint: SyncCheckpoint,
    },
    CheckpointUnavailable {
        message: String,
    },
    State {
        connection: SyncConnection,
        replica: Option<u32>,
        pending: u32,
    },
    Presence {
        sheet: u32,
        overlays: Vec<RemotePresence>,
    },
    EpochChanged {
        old_epoch: u8,
        new_epoch: u8,
    },
    DigestMismatch {
        seq: String,
        expected: String,
        actual: String,
    },
    Reconnect,
    UpgradeRequired,
    Failed {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub enum SyncConnection {
    Offline,
    Connecting,
    CatchingUp,
    Online,
}

struct PresenceEntry {
    frame: WirePresenceV1,
    received_at: f64,
}

pub(crate) struct CollabSession {
    pub(crate) machine: ClientSyncMachine,
    events: VecDeque<CollabEvent>,
    presences: BTreeMap<u32, PresenceEntry>,
    last_presence_sent: Option<f64>,
    initialized: bool,
    failed: bool,
}

impl CollabSession {
    fn open(
        doc_id: &str,
        branch_id: &str,
        info: Option<SyncCheckpointInfo>,
        snapshot: &[u8],
        pending: &[u8],
    ) -> Result<Self, String> {
        let doc = parse_uuid(doc_id)?;
        let branch = parse_uuid(branch_id)?;
        let Some(info) = info else {
            if !snapshot.is_empty() || !pending.is_empty() {
                return Err("Collaborative checkpoint metadata is missing.".into());
            }
            return Ok(Self::new(ClientSyncMachine::new(doc, branch), false));
        };
        if info.confirmed_client_seq > info.confirmed_seq
            || info.replica.is_some_and(|replica| {
                let (epoch, counter) = unpack_replica_id(replica);
                replica > MAX_REPLICA || epoch != info.epoch || counter == 0 || counter == u16::MAX
            })
        {
            return Err("Invalid collaborative replica metadata.".into());
        }
        let confirmed = if snapshot.is_empty() && info.confirmed_seq == 0 {
            Workbook::new()
        } else {
            decode_snapshot_any(snapshot)
                .map_err(|error| format!("Could not decode collaborative checkpoint: {error:?}"))?
                .engine
                .workbook()
                .clone()
        };
        let (mut pending_records, mut repair_pending) = if pending.is_empty() {
            (Vec::new(), false)
        } else {
            let scan = scan_pending_wal(pending)
                .map_err(|error| format!("Could not recover pending transactions: {error:?}"))?;
            (scan.records, scan.valid_bytes < pending.len())
        };
        if pending_records.iter().any(|record| {
            Some(record.replica) != info.replica
                || ops_from_wire_txn_v2(record.txn.clone()).is_err()
        }) {
            return Err("Pending transactions do not belong to this collaborative replica.".into());
        }
        pending_records.retain(|record| {
            let keep = record.client_seq > info.confirmed_client_seq;
            repair_pending |= !keep;
            keep
        });
        let initialized = info.replica.is_some();
        let machine = ClientSyncMachine::from_persisted(
            doc,
            branch,
            info.epoch,
            info.replica,
            info.confirmed_seq,
            info.confirmed_client_seq,
            confirmed,
            pending_records,
            info.next_id_counter,
        );
        let mut session = Self::new(machine, initialized);
        if repair_pending {
            session.emit(ClientAction::PersistTruncatedPending(
                session.machine.pending_wire_txns(),
            ));
        }
        Ok(session)
    }

    fn new(machine: ClientSyncMachine, initialized: bool) -> Self {
        Self {
            machine,
            events: VecDeque::new(),
            presences: BTreeMap::new(),
            last_presence_sent: None,
            initialized,
            failed: false,
        }
    }

    pub(crate) fn preflight(
        &self,
        label: &str,
        ops: &[Op<BoundFormula>],
        timestamp_ms: u64,
    ) -> Result<(), String> {
        let Some(replica) = self
            .machine
            .replica()
            .filter(|_| self.initialized && !self.failed)
        else {
            return Err("Collaborative edits require a verified replica.".into());
        };
        let pending = WirePendingTxnV2 {
            replica,
            client_seq: self.machine.next_client_seq(),
            base_server_seq: self.machine.confirmed_seq(),
            timestamp_ms,
            txn: ops_to_wire_txn_v2(label.into(), ops),
        };
        if !encode_sync_frame(&WireSyncFrameV1::Push {
            txns: vec![pending],
        })
        .is_ok_and(|bytes| bytes.len() <= DEFAULT_MAX_PUSH_BYTES)
        {
            return Err("This collaborative transaction exceeds the sync frame limit.".into());
        }
        Ok(())
    }

    pub(crate) fn queue(
        &mut self,
        label: &str,
        ops: Vec<Op<BoundFormula>>,
        timestamp_ms: u64,
    ) -> Result<u64, reticula_core::ops::OpError> {
        let seq = self.machine.next_client_seq();
        let actions = self.machine.push_local(label.into(), ops, timestamp_ms)?;
        for action in actions {
            self.emit(action);
        }
        self.state();
        Ok(seq)
    }

    fn info(&self) -> SyncCheckpointInfo {
        SyncCheckpointInfo {
            epoch: self.machine.epoch(),
            replica: self.machine.replica(),
            confirmed_seq: self.machine.confirmed_seq(),
            confirmed_client_seq: self.machine.confirmed_client_seq(),
            next_id_counter: self.machine.next_id_counter(),
        }
    }

    fn state(&mut self) {
        let connection = match self.machine.connection_state() {
            ConnectionState::Offline => SyncConnection::Offline,
            ConnectionState::Handshaking => SyncConnection::Connecting,
            ConnectionState::AwaitingSnapshot { .. } => SyncConnection::CatchingUp,
            ConnectionState::Online => SyncConnection::Online,
        };
        self.events.push_back(CollabEvent::State {
            connection,
            replica: self.machine.replica(),
            pending: u32::try_from(self.machine.pending_len()).unwrap_or(u32::MAX),
        });
    }

    fn emit(&mut self, action: ClientAction) {
        if self.failed {
            return;
        }
        let event = match action {
            ClientAction::PersistPending(txn) => encode_pending_wal_record(&txn)
                .map(|bytes| CollabEvent::AppendPending { bytes })
                .ok(),
            ClientAction::PersistTruncatedPending(txns) => encode_pending_wal_complete(&txns)
                .map(|bytes| CollabEvent::ReplacePending {
                    info: self.info(),
                    bytes,
                })
                .ok(),
            ClientAction::SendFrame(frame) => encode_sync_frame(&frame)
                .map(|bytes| CollabEvent::Send { bytes })
                .ok(),
            ClientAction::RequestSnapshot { snapshot_seq, url } => {
                Some(CollabEvent::FetchSnapshot {
                    seq: snapshot_seq,
                    url,
                })
            }
            ClientAction::EpochChanged {
                old_epoch,
                new_epoch,
            } => Some(CollabEvent::EpochChanged {
                old_epoch,
                new_epoch,
            }),
            ClientAction::DigestMismatch {
                seq,
                expected,
                actual,
            } => Some(CollabEvent::DigestMismatch {
                seq: seq.to_string(),
                expected: format!("{expected:?}"),
                actual: format!("{actual:?}"),
            }),
            ClientAction::RequestReconnect => Some(CollabEvent::Reconnect),
            ClientAction::NotifyVisibleChanged
            | ClientAction::PruneUndoFrom(_)
            | ClientAction::SaveUnsyncedLocal { .. } => return,
        };
        if let Some(event) = event {
            self.events.push_back(event);
        } else {
            self.fail("Could not encode collaborative output.".into());
        }
    }

    fn fail(&mut self, message: String) {
        self.failed = true;
        self.events.push_back(CollabEvent::Failed { message });
    }

    fn checkpoint(&mut self) -> Result<(), String> {
        if !self.initialized
            || matches!(
                self.machine.connection_state(),
                ConnectionState::AwaitingSnapshot { .. }
            )
        {
            return Err("The collaborative workbook is still catching up.".into());
        }
        let info = self.info();
        let watermarks = info.replica.map(|replica| WireReplicaWatermarkV1 {
            replica,
            last_client_seq: info.confirmed_client_seq,
            max_id_counter: info.next_id_counter.saturating_sub(1),
        });
        let snapshot = encode_snapshot_v1(self.machine.confirmed(), watermarks.as_slice())
            .map_err(|error| format!("Could not encode collaborative checkpoint: {error:?}"))?;
        self.events.push_back(CollabEvent::Checkpoint {
            checkpoint: SyncCheckpoint { info, snapshot },
        });
        Ok(())
    }

    fn local_presence(
        &mut self,
        sheet: SheetId,
        row: u32,
        col: u32,
        selection: Option<PresenceSpan>,
    ) {
        let now = now_ms();
        if self.machine.connection_state() != ConnectionState::Online
            || self
                .last_presence_sent
                .is_some_and(|last| now - last < 50.0)
        {
            return;
        }
        let Some(replica) = self.machine.replica() else {
            return;
        };
        let Some(s) = self.machine.visible().live_sheet(sheet) else {
            return;
        };
        let (Some(cursor_row), Some(cursor_col)) =
            (s.row_axis().pos_to_id(row), s.col_axis().pos_to_id(col))
        else {
            return;
        };
        let selection = selection.and_then(|span| {
            Some(WirePresenceRangeV1 {
                start_row: s.row_axis().pos_to_id(span.first_row)?,
                end_row: s.row_axis().pos_to_id(span.end_row.checked_sub(1)?)?,
                start_col: s.col_axis().pos_to_id(span.first_col)?,
                end_col: s.col_axis().pos_to_id(span.end_col.checked_sub(1)?)?,
            })
        });
        self.last_presence_sent = Some(now);
        self.emit(ClientAction::SendFrame(WireSyncFrameV1::Presence(
            WirePresenceV1 {
                replica,
                user_id: [0; 16],
                display_name: String::new(),
                color_slot: 0,
                sheet_id: sheet.0,
                cursor_row,
                cursor_col,
                selection,
            },
        )));
    }

    pub(crate) fn presence(&mut self, sheet: SheetId) {
        let now = now_ms();
        self.presences
            .retain(|_, entry| now - entry.received_at < 60_000.0);
        let overlays = self
            .machine
            .visible()
            .live_sheet(sheet)
            .map_or_else(Vec::new, |s| {
                self.presences
                    .values()
                    .filter_map(|entry| {
                        let value = &entry.frame;
                        if value.sheet_id != sheet.0 {
                            return None;
                        }
                        let selection = value.selection.and_then(|range| {
                            let r1 = s.row_axis().id_to_pos(range.start_row)?;
                            let r2 = s.row_axis().id_to_pos(range.end_row)?;
                            let c1 = s.col_axis().id_to_pos(range.start_col)?;
                            let c2 = s.col_axis().id_to_pos(range.end_col)?;
                            Some(PresenceSpan {
                                first_row: r1.min(r2),
                                end_row: r1.max(r2).saturating_add(1),
                                first_col: c1.min(c2),
                                end_col: c1.max(c2).saturating_add(1),
                            })
                        });
                        Some(RemotePresence {
                            replica: value.replica,
                            display_name: value.display_name.clone(),
                            color_slot: value.color_slot,
                            cursor_row: s.row_axis().id_to_pos(value.cursor_row)?,
                            cursor_col: s.col_axis().id_to_pos(value.cursor_col)?,
                            selection,
                        })
                    })
                    .collect()
            });
        self.events.push_back(CollabEvent::Presence {
            sheet: sheet.0,
            overlays,
        });
    }
}

fn parse_uuid(value: &str) -> Result<[u8; 16], String> {
    Uuid::parse_str(value)
        .map(Uuid::into_bytes)
        .map_err(|_| "Invalid collaborative document or branch identifier.".into())
}

#[wasm_bindgen]
impl Engine {
    #[wasm_bindgen(js_name = handleSync)]
    pub fn handle_sync(
        &mut self,
        command: &Ts<SyncCommand>,
    ) -> Result<Option<Ts<EngineEvent>>, JsError> {
        check_not_panicked()?;
        let command = command.to_rust()?;
        let event = self.handle_sync_command(command);
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }

    #[wasm_bindgen(js_name = takeCollabEvent)]
    pub fn take_collab_event(&mut self) -> Result<Option<Ts<CollabEvent>>, JsError> {
        let event = self.pop_collab_event();
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }
}

impl Engine {
    pub fn pop_collab_event(&mut self) -> Option<CollabEvent> {
        self.collab.as_mut()?.events.pop_front()
    }

    pub fn handle_sync_command(&mut self, command: SyncCommand) -> Option<EngineEvent> {
        if let SyncCommand::Open {
            doc_id,
            branch_id,
            info,
            snapshot,
            pending_wal,
        } = command
        {
            return match CollabSession::open(&doc_id, &branch_id, info, &snapshot, &pending_wal) {
                Ok(mut session) => {
                    self.open_empty();
                    self.engine
                        .replace_reconciled_workbook(session.machine.visible().clone());
                    self.ensure_subscribed_sheet_live();
                    session.state();
                    self.collab = Some(session);
                    Some(EngineEvent::Ready {
                        info: crate::EngineInfo {
                            kernel_version: reticula_core::KERNEL_VERSION.into(),
                        },
                        sheet: self.sheet_info(self.subscribed_sheet),
                    })
                }
                Err(reason) => Some(EngineEvent::Rejected { reason }),
            };
        }
        let Some(mut session) = self.collab.take() else {
            return Some(EngineEvent::Rejected {
                reason: "No collaborative workbook is open.".into(),
            });
        };
        if session.failed {
            if matches!(command, SyncCommand::Checkpoint) {
                session
                    .events
                    .push_back(CollabEvent::CheckpointUnavailable {
                        message: "Collaborative session has failed.".into(),
                    });
            }
            self.collab = Some(session);
            return None;
        }
        let mut incremental_ops = None;
        let actions = match command {
            SyncCommand::Connected => session.machine.on_connected(),
            SyncCommand::Disconnected => {
                session.machine.on_disconnected();
                session.presences.clear();
                session.last_presence_sent = None;
                session.presence(self.subscribed_sheet);
                Vec::new()
            }
            SyncCommand::Receive { frame } => match decode_sync_frame(&frame) {
                Ok(WireSyncFrameV1::Ping(nonce)) => {
                    vec![ClientAction::SendFrame(WireSyncFrameV1::Pong(nonce))]
                }
                Ok(welcome @ WireSyncFrameV1::Welcome { .. })
                    if session.machine.connection_state() == ConnectionState::Handshaking =>
                {
                    session.machine.on_welcome(welcome)
                }
                Ok(WireSyncFrameV1::Commit {
                    seq,
                    replica,
                    client_seq,
                    user_id,
                    timestamp_ms,
                    txn,
                }) => {
                    if session.machine.connection_state() == ConnectionState::Online
                        && session.machine.pending_len() == 0
                    {
                        incremental_ops =
                            ops_from_wire_txn_v2(txn.clone()).ok().map(|(_, ops)| ops);
                    }
                    session.machine.on_commit(WireCommittedTxnV2 {
                        seq,
                        replica,
                        client_seq,
                        user_id,
                        timestamp_ms,
                        txn,
                    })
                }
                Ok(WireSyncFrameV1::Ack {
                    replica,
                    client_seq,
                    ..
                }) => {
                    session.machine.on_ack(replica, client_seq);
                    Vec::new()
                }
                Ok(WireSyncFrameV1::Reject {
                    reason: reticula_wire::v2::WireRejectReasonV1::UpgradeRequired,
                    ..
                }) => {
                    session.events.push_back(CollabEvent::UpgradeRequired);
                    Vec::new()
                }
                Ok(WireSyncFrameV1::Reject { client_seq: 0, .. }) => {
                    vec![ClientAction::RequestReconnect]
                }
                Ok(WireSyncFrameV1::Reject { client_seq, reason }) => {
                    session.machine.on_reject(client_seq, reason)
                }
                Ok(WireSyncFrameV1::Digest { seq, state_digest }) => {
                    session.machine.on_digest(seq, state_digest)
                }
                Ok(WireSyncFrameV1::Presence(value)) => {
                    if session.machine.replica() != Some(value.replica) {
                        session.presences.insert(
                            value.replica,
                            PresenceEntry {
                                frame: value,
                                received_at: now_ms(),
                            },
                        );
                        session.presence(self.subscribed_sheet);
                    }
                    Vec::new()
                }
                Ok(WireSyncFrameV1::Pong(_)) => Vec::new(),
                _ => vec![ClientAction::RequestReconnect],
            },
            SyncCommand::InstallSnapshot { seq, snapshot } => {
                if session.machine.connection_state()
                    == (ConnectionState::AwaitingSnapshot { snapshot_seq: seq })
                {
                    match decode_snapshot_any(&snapshot) {
                        Ok(decoded) => {
                            let watermarks = decoded
                                .watermarks
                                .into_iter()
                                .map(|wm| ReplicaWatermark {
                                    replica: wm.replica,
                                    user_id: [0; 16],
                                    last_client_seq: wm.last_client_seq,
                                    max_id_counter: wm.max_id_counter,
                                })
                                .collect::<Vec<_>>();
                            session.machine.install_snapshot(
                                seq,
                                decoded.engine.workbook().clone(),
                                &watermarks,
                                Vec::new(),
                            )
                        }
                        Err(_) => vec![ClientAction::RequestReconnect],
                    }
                } else {
                    Vec::new()
                }
            }
            SyncCommand::Checkpoint => {
                if let Err(message) = session.checkpoint() {
                    session
                        .events
                        .push_back(CollabEvent::CheckpointUnavailable { message });
                }
                Vec::new()
            }
            SyncCommand::LocalPresence {
                sheet,
                cursor_row,
                cursor_col,
                selection,
            } => {
                session.local_presence(SheetId(sheet), cursor_row, cursor_col, selection);
                Vec::new()
            }
            SyncCommand::ExpirePresence => {
                session.presence(self.subscribed_sheet);
                Vec::new()
            }
            SyncCommand::Open { .. } => unreachable!(),
        };
        let mut changed = false;
        for action in actions {
            match action {
                ClientAction::SaveUnsyncedLocal {
                    from_client_seq,
                    txns,
                } => match (
                    encode_snapshot_v1(self.engine.workbook(), &[]),
                    encode_pending_wal_complete(&txns),
                ) {
                    (Ok(snapshot), Ok(pending_wal)) => {
                        session.events.push_back(CollabEvent::SaveUnsynced {
                            from_client_seq,
                            snapshot,
                            pending_wal,
                        });
                    }
                    _ => {
                        session.fail("Could not preserve rejected collaborative edits.".into());
                        break;
                    }
                },
                ClientAction::PruneUndoFrom(seq) => self.undo_stack.prune_collab(seq),
                ClientAction::NotifyVisibleChanged => changed = true,
                action @ ClientAction::EpochChanged { .. } => {
                    self.undo_stack.clear();
                    session.presences.clear();
                    session.initialized = false;
                    session.emit(action);
                }
                action @ ClientAction::DigestMismatch { .. } => {
                    session.emit(action);
                    session.machine.reset_confirmed_for_resync();
                    session.initialized = false;
                    session.presences.clear();
                    session.presence(self.subscribed_sheet);
                    session.emit(ClientAction::RequestReconnect);
                }
                action => session.emit(action),
            }
        }
        if session.machine.connection_state() == ConnectionState::Online {
            session.initialized = true;
        }
        let event = if changed {
            if !incremental_ops.is_some_and(|ops| self.engine.apply_txn(ops).is_ok()) {
                self.engine
                    .replace_reconciled_workbook(session.machine.visible().clone());
                self.pending_aux_event = None;
                for page in self.subscribed_pages.clone() {
                    self.enqueue_page(page);
                }
            }
            self.ensure_subscribed_sheet_live();
            self.refresh_subscription_extent();
            self.flush_dirty_pages();
            session.presence(self.subscribed_sheet);
            Some(EngineEvent::SheetChanged {
                sheet: self.sheet_info(self.subscribed_sheet),
                action_ms: 0.0,
                recalculated: 0,
            })
        } else {
            None
        };
        session.state();
        self.collab = Some(session);
        event
    }
}

#[cfg(test)]
mod tests;
