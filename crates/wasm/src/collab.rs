//! Browser bindings for the pure collaborative sync machine.

use std::collections::{BTreeMap, VecDeque};

use reticula_core::id::{MAX_REPLICA, SheetId, unpack_replica_id};
use reticula_core::ops::{InverseOp, Op, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_sync::types::DEFAULT_MAX_PUSH_BYTES;
use reticula_sync::{
    ClientAction, ClientSyncMachine, CommittedPermute, ConnectionState, ReplicaWatermark,
};
use reticula_wire::convert_v3::{ops_from_wire_txn_v3, ops_to_wire_txn_v3};
use reticula_wire::snapshot::{
    decode_snapshot_any, encode_snapshot_v1, encode_snapshot_with_permutes,
};
use reticula_wire::sync_frame::{decode_sync_frame_any, encode_sync_frame_v2};
use reticula_wire::v1::WireReplicaWatermarkV1;
use reticula_wire::v2::{WirePresenceRangeV1, WirePresenceV1};
use reticula_wire::v3::{
    WireCommittedPermuteV3, WireCommittedTxnV3, WirePendingTxnV3, WireSyncFrameV2,
};
use reticula_wire::wal::{
    encode_pending_wal_complete_v2, encode_pending_wal_init_v2, encode_pending_wal_record_v2,
    scan_pending_wal_any,
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

/// Where the local cursor and its selection sit in display coordinates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct LocalCursor {
    pub row: u32,
    pub col: u32,
    pub selection: Option<PresenceSpan>,
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
        read_only: bool,
    },
    Presence {
        sheet: u32,
        overlays: Vec<RemotePresence>,
    },
    /// A remote or reconciled change moved the rows or columns under the
    /// local cursor. The host moves its selection from `from` to `to` unless
    /// the user has moved it since.
    LocalSelectionMoved {
        sheet: u32,
        from: LocalCursor,
        to: LocalCursor,
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

/// The local cursor by stable row and column IDs, with the display position
/// the host last reported for it.
struct LocalAnchor {
    sheet: SheetId,
    row: u64,
    col: u64,
    range: Option<WirePresenceRangeV1>,
    at: LocalCursor,
}

struct EnginePendingTxn {
    client_seq: u64,
    ops: Vec<Op<BoundFormula>>,
    inverses: Vec<InverseOp<BoundFormula>>,
}

pub(crate) struct CollabSession {
    pub(crate) machine: ClientSyncMachine,
    events: VecDeque<CollabEvent>,
    presences: BTreeMap<u32, PresenceEntry>,
    last_presence_sent: Option<f64>,
    local_anchor: Option<LocalAnchor>,
    engine_pending: VecDeque<EnginePendingTxn>,
    engine_inverses_valid: bool,
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
        let (confirmed, confirmed_permutes) = if snapshot.is_empty() && info.confirmed_seq == 0 {
            (Workbook::new(), Vec::new())
        } else {
            let decoded = decode_snapshot_any(snapshot)
                .map_err(|error| format!("Could not decode collaborative checkpoint: {error:?}"))?;
            let permutes = decoded
                .permutes
                .iter()
                .map(CommittedPermute::from)
                .collect();
            (decoded.engine.workbook().clone(), permutes)
        };
        let (mut pending_records, mut repair_pending) = if pending.is_empty() {
            (Vec::new(), false)
        } else {
            let scan = scan_pending_wal_any(pending)
                .map_err(|error| format!("Could not recover pending transactions: {error:?}"))?;
            (scan.records, scan.valid_bytes < pending.len())
        };
        if pending_records.iter().any(|record| {
            Some(record.replica) != info.replica
                || ops_from_wire_txn_v3(record.txn.clone()).is_err()
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
            confirmed_permutes,
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
        let engine_inverses_valid = machine.pending_len() == 0;
        Self {
            machine,
            events: VecDeque::new(),
            presences: BTreeMap::new(),
            last_presence_sent: None,
            local_anchor: None,
            engine_pending: VecDeque::new(),
            engine_inverses_valid,
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
        if self.machine.is_read_only() {
            return Err("This collaborative workbook is read-only.".into());
        }
        let Some(replica) = self
            .machine
            .replica()
            .filter(|_| self.initialized && !self.failed)
        else {
            return Err("Collaborative edits require a verified replica.".into());
        };
        let pending = WirePendingTxnV3 {
            replica,
            client_seq: self.machine.next_client_seq(),
            base_server_seq: self.machine.confirmed_seq(),
            timestamp_ms,
            txn: ops_to_wire_txn_v3(label.into(), ops)
                .map_err(|_| String::from("This transaction cannot be encoded for sync."))?,
        };
        if !encode_sync_frame_v2(&WireSyncFrameV2::Push {
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
        engine_inverses: Vec<InverseOp<BoundFormula>>,
    ) -> Result<u64, reticula_core::ops::OpError> {
        let seq = self.machine.next_client_seq();
        let actions = self
            .machine
            .push_local(label.into(), ops.clone(), timestamp_ms)?;
        if self.engine_inverses_valid {
            self.engine_pending.push_back(EnginePendingTxn {
                client_seq: seq,
                ops,
                inverses: engine_inverses,
            });
        }
        for action in actions {
            self.emit(action);
        }
        self.state();
        Ok(seq)
    }

    fn reset_engine_pending(&mut self) {
        self.engine_pending.clear();
        self.engine_inverses_valid = self.machine.pending_len() == 0;
    }

    fn pop_confirmed_engine_pending(&mut self) {
        if self.machine.pending_len() == 0 {
            self.engine_pending.clear();
            self.engine_inverses_valid = true;
            return;
        }
        let confirmed_cs = self.machine.confirmed_client_seq();
        while self
            .engine_pending
            .front()
            .is_some_and(|entry| entry.client_seq <= confirmed_cs)
        {
            let _ = self.engine_pending.pop_front();
        }
        if self.engine_pending.len() != self.machine.pending_len() {
            self.engine_pending.clear();
            self.engine_inverses_valid = false;
        }
    }

    fn reconcile_engine_incremental(
        &mut self,
        engine: &mut FormulaEngine,
        remote_ops: Vec<Op<BoundFormula>>,
    ) -> bool {
        if !self.engine_inverses_valid
            || self.engine_pending.is_empty()
            || self.engine_pending.len() != self.machine.pending_len()
            || remote_ops
                .iter()
                .any(|op| matches!(op, Op::PermuteContent { .. }))
            || self.engine_pending.iter().any(|e| {
                e.ops
                    .iter()
                    .any(|op| matches!(op, Op::PermuteContent { .. }))
            })
        {
            return false;
        }
        for entry in self.engine_pending.iter_mut().rev() {
            let invs = core::mem::take(&mut entry.inverses);
            for inv in invs.into_iter().rev() {
                if engine.apply_inverse(inv).is_err() {
                    return false;
                }
            }
        }
        if engine.apply_txn(remote_ops).is_err() {
            return false;
        }
        for entry in &mut self.engine_pending {
            let Ok(new_invs) = engine.apply_txn(entry.ops.clone()) else {
                return false;
            };
            entry.inverses = new_invs;
        }
        engine.workbook().state_digest() == self.machine.visible().state_digest()
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
            read_only: self.machine.is_read_only(),
        });
    }

    fn emit(&mut self, action: ClientAction) {
        if self.failed {
            return;
        }
        let event = match action {
            ClientAction::PersistPending(txn) => encode_pending_wal_record_v2(&txn)
                .map(|bytes| CollabEvent::AppendPending { bytes })
                .ok(),
            ClientAction::PersistTruncatedPending(txns) => encode_pending_wal_complete_v2(&txns)
                .map(|bytes| CollabEvent::ReplacePending {
                    info: self.info(),
                    bytes,
                })
                .ok(),
            ClientAction::SendFrame(frame) => encode_sync_frame_v2(&frame)
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
        let permutes: Vec<WireCommittedPermuteV3> = self
            .machine
            .confirmed_permutes()
            .iter()
            .map(WireCommittedPermuteV3::from)
            .collect();
        let snapshot = encode_snapshot_with_permutes(
            self.machine.confirmed(),
            watermarks.as_slice(),
            &permutes,
        )
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
        let Some(s) = self.machine.visible().live_sheet(sheet) else {
            self.local_anchor = None;
            return;
        };
        let (Some(cursor_row), Some(cursor_col)) =
            (s.row_axis().pos_to_id(row), s.col_axis().pos_to_id(col))
        else {
            self.local_anchor = None;
            return;
        };
        let range = selection.as_ref().and_then(|span| {
            Some(WirePresenceRangeV1 {
                start_row: s.row_axis().pos_to_id(span.first_row)?,
                end_row: s.row_axis().pos_to_id(span.end_row.checked_sub(1)?)?,
                start_col: s.col_axis().pos_to_id(span.first_col)?,
                end_col: s.col_axis().pos_to_id(span.end_col.checked_sub(1)?)?,
            })
        });
        self.local_anchor = Some(LocalAnchor {
            sheet,
            row: cursor_row,
            col: cursor_col,
            range,
            at: LocalCursor {
                row,
                col,
                selection: range.and(selection),
            },
        });
        let now = now_ms();
        if self.machine.connection_state() != ConnectionState::Online
            || self.machine.is_read_only()
            || self
                .last_presence_sent
                .is_some_and(|last| now - last < 50.0)
        {
            return;
        }
        let Some(replica) = self.machine.replica() else {
            return;
        };
        self.last_presence_sent = Some(now);
        self.emit(ClientAction::SendFrame(WireSyncFrameV2::Presence(
            WirePresenceV1 {
                replica,
                user_id: [0; 16],
                display_name: String::new(),
                color_slot: 0,
                sheet_id: sheet.0,
                cursor_row,
                cursor_col,
                selection: range,
            },
        )));
    }

    /// After a change the local user did not make, finds the local cursor by
    /// its stable IDs and asks the host to follow it if it moved. A cursor
    /// whose row or column was deleted stays where the host has it.
    fn relocate_local(&mut self, subscribed: SheetId) {
        let Some(anchor) = &mut self.local_anchor else {
            return;
        };
        if anchor.sheet != subscribed {
            return;
        }
        let Some(s) = self.machine.visible().live_sheet(anchor.sheet) else {
            return;
        };
        let (Some(row), Some(col)) = (
            s.row_axis().id_to_pos(anchor.row),
            s.col_axis().id_to_pos(anchor.col),
        ) else {
            return;
        };
        let selection = match anchor.range {
            None => None,
            Some(range) => {
                let (Some(r1), Some(r2), Some(c1), Some(c2)) = (
                    s.row_axis().id_to_pos(range.start_row),
                    s.row_axis().id_to_pos(range.end_row),
                    s.col_axis().id_to_pos(range.start_col),
                    s.col_axis().id_to_pos(range.end_col),
                ) else {
                    return;
                };
                Some(PresenceSpan {
                    first_row: r1.min(r2),
                    end_row: r1.max(r2).saturating_add(1),
                    first_col: c1.min(c2),
                    end_col: c1.max(c2).saturating_add(1),
                })
            }
        };
        let to = LocalCursor {
            row,
            col,
            selection,
        };
        if to == anchor.at {
            return;
        }
        let from = core::mem::replace(&mut anchor.at, to.clone());
        let sheet = anchor.sheet.0;
        self.events
            .push_back(CollabEvent::LocalSelectionMoved { sheet, from, to });
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

/// Encodes the 16-byte `RTCLPWAL` header (v2) for collaborative `pending.wal` logs.
#[wasm_bindgen(js_name = encodePendingWalInit)]
#[must_use]
pub fn wasm_encode_pending_wal_init() -> Vec<u8> {
    encode_pending_wal_init_v2()
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
        let mut was_pending = None;
        let actions = match command {
            SyncCommand::Connected => session.machine.on_connected(),
            SyncCommand::Disconnected => {
                session.machine.on_disconnected();
                session.presences.clear();
                session.last_presence_sent = None;
                session.presence(self.subscribed_sheet);
                Vec::new()
            }
            SyncCommand::Receive { frame } => match decode_sync_frame_any(&frame) {
                Ok((_, WireSyncFrameV2::Ping(nonce))) => {
                    vec![ClientAction::SendFrame(WireSyncFrameV2::Pong(nonce))]
                }
                Ok((_, welcome @ WireSyncFrameV2::Welcome { .. }))
                    if session.machine.connection_state() == ConnectionState::Handshaking =>
                {
                    session.machine.on_welcome(welcome)
                }
                Ok((
                    _,
                    WireSyncFrameV2::Commit {
                        seq,
                        replica,
                        client_seq,
                        user_id,
                        timestamp_ms,
                        txn,
                    },
                )) => {
                    if session.machine.connection_state() == ConnectionState::Online {
                        was_pending = Some(session.machine.pending_len());
                        incremental_ops =
                            ops_from_wire_txn_v3(txn.clone()).ok().map(|(_, ops)| ops);
                    }
                    session.machine.on_commit(WireCommittedTxnV3 {
                        seq,
                        replica,
                        client_seq,
                        user_id,
                        timestamp_ms,
                        txn,
                    })
                }
                Ok((
                    _,
                    WireSyncFrameV2::Ack {
                        replica,
                        client_seq,
                        ..
                    },
                )) => {
                    session.machine.on_ack(replica, client_seq);
                    Vec::new()
                }
                Ok((
                    _,
                    WireSyncFrameV2::Reject {
                        reason: reticula_wire::v2::WireRejectReasonV1::UpgradeRequired,
                        ..
                    },
                )) => {
                    session.events.push_back(CollabEvent::UpgradeRequired);
                    Vec::new()
                }
                Ok((_, WireSyncFrameV2::Reject { client_seq: 0, .. })) => {
                    vec![ClientAction::RequestReconnect]
                }
                Ok((_, WireSyncFrameV2::Reject { client_seq, reason })) => {
                    session.machine.on_reject(client_seq, reason)
                }
                Ok((_, WireSyncFrameV2::Digest { seq, state_digest })) => {
                    session.machine.on_digest(seq, state_digest)
                }
                Ok((_, WireSyncFrameV2::Presence(value))) => {
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
                Ok((_, WireSyncFrameV2::Pong(_))) => Vec::new(),
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
                            let permutes = decoded
                                .permutes
                                .iter()
                                .map(CommittedPermute::from)
                                .collect::<Vec<_>>();
                            session.machine.install_snapshot(
                                seq,
                                decoded.engine.workbook().clone(),
                                &watermarks,
                                permutes,
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
                    encode_pending_wal_complete_v2(&txns),
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
                    session.reset_engine_pending();
                    session.emit(action);
                }
                action @ ClientAction::DigestMismatch { .. } => {
                    session.emit(action);
                    session.machine.reset_confirmed_for_resync();
                    session.reset_engine_pending();
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
        if !changed
            && (session.engine_pending.len() != session.machine.pending_len()
                || !session.engine_inverses_valid)
        {
            session.pop_confirmed_engine_pending();
        }
        let event = if changed {
            let reconciled = match (was_pending, incremental_ops) {
                (Some(0), Some(ops)) if session.machine.pending_len() == 0 => {
                    self.engine.apply_txn(ops).is_ok()
                        && self.engine.workbook().state_digest()
                            == session.machine.visible().state_digest()
                }
                (Some(prev_pending), Some(ops))
                    if prev_pending > 0 && session.machine.pending_len() == prev_pending =>
                {
                    session.reconcile_engine_incremental(&mut self.engine, ops)
                }
                _ => false,
            };
            if !reconciled {
                self.engine
                    .replace_reconciled_workbook(session.machine.visible().clone());
                session.reset_engine_pending();
                self.pending_aux_event = None;
                self.pages.mark_all();
            }
            self.ensure_subscribed_sheet_live();
            self.refresh_subscription_extent();
            self.flush_dirty_pages();
            session.presence(self.subscribed_sheet);
            session.relocate_local(self.subscribed_sheet);
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
