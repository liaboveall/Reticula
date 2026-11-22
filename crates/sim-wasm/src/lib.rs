//! On-demand WebAssembly bindings for the interactive `/sim` lab and 5-replica
//! deterministic fault simulator (`16-m2b.md`).

use std::collections::{BTreeMap, VecDeque};

use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{CellInput, ConditionalCellEdit, FormatPatch, InverseOp, Op, Workbook};
use reticula_core::state_digest::StateDigest;
use reticula_core::viewport::Page;
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_sim::{
    FaultCounts, SeedSummary, SimEvent, SimReplicaState, SimWorld, generate_script,
};
use reticula_sync::{
    ClientAction, ClientSyncMachine, CommittedPermute, ConnectionState, MemberRole,
    ReplicaWatermark, ServerDocMachine, ServerLimits,
};
use reticula_wire::snapshot::{decode_snapshot_any, encode_snapshot_with_permutes};
use reticula_wire::sync_frame::{decode_sync_frame_v2, encode_sync_frame_v2};
use reticula_wire::v1::WireReplicaWatermarkV1;
use reticula_wire::v3::{
    WireCatchupV2, WireCommittedPermuteV3, WireCommittedTxnV3, WirePendingTxnV3, WireSyncFrameV2,
};
use reticula_wire::wal::{encode_pending_wal_complete_v2, scan_pending_wal_any};
use serde::{Deserialize, Serialize};
use tsify::{Ts, Tsify};
use wasm_bindgen::prelude::*;

const LAB_DOC_ID: [u8; 16] = [0xD0; 16];
const LAB_BRANCH_ID: [u8; 16] = [0xB1; 16];
const LAB_USERS: [([u8; 16], &str, MemberRole); 3] = [
    ([0xA1; 16], "Alice", MemberRole::Owner),
    ([0xB2; 16], "Bob", MemberRole::Editor),
    ([0xC3; 16], "Carol", MemberRole::Editor),
];

/// Network link emulation mode for a replica in [`InteractiveSimLab`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkMode {
    Online,
    Delayed,
    Dropped,
}

impl LinkMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "online" => Ok(Self::Online),
            "delayed" => Ok(Self::Delayed),
            "dropped" => Ok(Self::Dropped),
            _ => Err(format!("unsupported link mode `{raw}`")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Delayed => "delayed",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Clone, Debug)]
struct QueuedFrame {
    conn_id: u64,
    summary: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct ReplicaCheckpoint {
    epoch: u8,
    replica: Option<u32>,
    confirmed_seq: u64,
    last_acked_client_seq: u64,
    confirmed: Workbook<BoundFormula>,
    confirmed_permutes: Vec<CommittedPermute>,
    pending: Vec<WirePendingTxnV3>,
    next_id_counter_hint: u32,
}

impl ReplicaCheckpoint {
    fn empty() -> Self {
        Self {
            epoch: 0,
            replica: None,
            confirmed_seq: 0,
            last_acked_client_seq: 0,
            confirmed: Workbook::new(),
            confirmed_permutes: Vec::new(),
            pending: Vec::new(),
            next_id_counter_hint: 1,
        }
    }
}

#[derive(Clone, Debug)]
struct LabUndoEntry {
    client_seq: Option<u64>,
    label: String,
    undo_ops: Vec<Op<BoundFormula>>,
}

struct LabReplica {
    name: String,
    user_id: [u8; 16],
    role: MemberRole,
    machine: ClientSyncMachine,
    engine: FormulaEngine,
    checkpoint: ReplicaCheckpoint,
    undo_stack: Vec<LabUndoEntry>,
    link_mode: LinkMode,
    active_conn: Option<u64>,
    c2s_queue: VecDeque<QueuedFrame>,
    s2c_queue: VecDeque<QueuedFrame>,
    pending_snapshot_req: Option<(u64, u64)>,
    welcome_head_seq: Option<u64>,
    last_notice: Option<String>,
    text_scratch: String,
}

impl LabReplica {
    fn new(name: &str, user_id: [u8; 16], role: MemberRole) -> Self {
        Self {
            name: String::from(name),
            user_id,
            role,
            machine: ClientSyncMachine::new(LAB_DOC_ID, LAB_BRANCH_ID),
            engine: FormulaEngine::new(),
            checkpoint: ReplicaCheckpoint::empty(),
            undo_stack: Vec::new(),
            link_mode: LinkMode::Online,
            active_conn: None,
            c2s_queue: VecDeque::new(),
            s2c_queue: VecDeque::new(),
            pending_snapshot_req: None,
            welcome_head_seq: None,
            last_notice: None,
            text_scratch: String::with_capacity(512),
        }
    }

    fn sync_engine_visible(&mut self) {
        self.engine
            .replace_reconciled_workbook(self.machine.visible().clone());
        self.engine.recalc_all();
    }

    fn sync_checkpoint(&mut self) {
        if matches!(
            self.machine.connection_state(),
            ConnectionState::AwaitingSnapshot { .. }
        ) {
            return;
        }
        self.checkpoint.epoch = self.machine.epoch();
        self.checkpoint.replica = self.machine.replica();
        self.checkpoint.confirmed = self.machine.confirmed().clone();
        self.checkpoint.confirmed_permutes = self.machine.confirmed_permutes().to_vec();
        self.checkpoint.confirmed_seq = self.machine.confirmed_seq();
        self.checkpoint.last_acked_client_seq = self.machine.last_acked_client_seq();
        self.checkpoint.pending = self.machine.pending_wire_txns();
        self.checkpoint.next_id_counter_hint = self.machine.next_id_counter();
    }
}

/// Minimal `@reticula/grid`-compatible sheet configuration returned by [`InteractiveSimLab`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SimSheetConfig {
    pub rows: u32,
    pub cols: u32,
    pub frozen_rows: u32,
    pub frozen_cols: u32,
    pub custom_rows: Vec<(u32, u16)>,
    pub custom_cols: Vec<(u32, u16)>,
}

/// One queued network frame waiting between a simulated replica and the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SimWireQueueItem {
    pub replica_index: usize,
    pub replica_name: String,
    pub direction: String,
    pub summary: String,
}

/// Snapshot of one replica inside [`InteractiveSimLab`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SimLabReplicaView {
    pub index: usize,
    pub name: String,
    pub role: String,
    pub replica_id: Option<u32>,
    pub link_mode: String,
    pub connection: String,
    pub confirmed_seq: u64,
    pub pending_len: usize,
    pub can_undo: bool,
    pub digest: String,
    pub converged: bool,
    pub last_notice: Option<String>,
}

/// Full inspectable state of [`InteractiveSimLab`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SimLabState {
    pub server_head_seq: u64,
    pub server_epoch: u8,
    pub server_digest: String,
    pub all_converged: bool,
    pub replicas: Vec<SimLabReplicaView>,
    pub queue: Vec<SimWireQueueItem>,
}

/// Interactive 2–3 replica offline-merge & fault sandbox.
#[wasm_bindgen]
pub struct InteractiveSimLab {
    server: ServerDocMachine,
    replicas: Vec<LabReplica>,
    epoch_log: Vec<WireCommittedTxnV3>,
    snapshots_by_seq: BTreeMap<u64, (Vec<u8>, Vec<ReplicaWatermark>)>,
    next_conn_id: u64,
    timestamp_ms: u64,
}

#[wasm_bindgen]
impl InteractiveSimLab {
    #[wasm_bindgen(constructor)]
    pub fn new(replica_count: usize) -> Result<Self, JsError> {
        Self::new_inner(replica_count).map_err(|msg| JsError::new(&msg))
    }

    #[wasm_bindgen(js_name = replicaCount)]
    #[must_use]
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    #[wasm_bindgen(js_name = sheetInfo)]
    pub fn sheet_info(&self, replica: usize) -> Result<Ts<SimSheetConfig>, JsError> {
        let rep = self
            .replicas
            .get(replica)
            .ok_or_else(|| JsError::new("replica index out of range"))?;
        let sheet = rep
            .engine
            .workbook()
            .sheet(SheetId(0))
            .ok_or_else(|| JsError::new("missing SheetId(0)"))?;
        Ts::from_rust(&SimSheetConfig {
            rows: sheet.rows(),
            cols: sheet.cols(),
            frozen_rows: sheet.frozen_rows(),
            frozen_cols: sheet.frozen_cols(),
            custom_rows: Vec::new(),
            custom_cols: Vec::new(),
        })
        .map_err(Into::into)
    }

    #[wasm_bindgen(js_name = encodePage)]
    pub fn encode_page(
        &mut self,
        replica: usize,
        page_row: u32,
        page_col: u32,
        epoch: u32,
    ) -> Result<Option<Vec<u8>>, JsError> {
        let rep = self
            .replicas
            .get_mut(replica)
            .ok_or_else(|| JsError::new("replica index out of range"))?;
        let page = Page {
            row: page_row,
            col: page_col,
        };
        rep.engine.prepare_page_styles(SheetId(0), page);
        Ok(rep
            .engine
            .encode_viewport_page(SheetId(0), page, epoch, &mut rep.text_scratch))
    }

    #[wasm_bindgen(js_name = editCell)]
    pub fn edit_cell(
        &mut self,
        replica: usize,
        row: u32,
        col: u32,
        raw: &str,
    ) -> Result<Ts<SimLabState>, JsError> {
        self.edit_cell_inner(replica, row, col, raw)
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = insertRow)]
    pub fn insert_row(
        &mut self,
        replica: usize,
        after_row_idx: i32,
    ) -> Result<Ts<SimLabState>, JsError> {
        self.insert_row_inner(replica, after_row_idx)
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = deleteRow)]
    pub fn delete_row(&mut self, replica: usize, row_idx: u32) -> Result<Ts<SimLabState>, JsError> {
        self.delete_row_inner(replica, row_idx)
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    pub fn undo(&mut self, replica: usize) -> Result<Ts<SimLabState>, JsError> {
        self.undo_inner(replica).map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = setLinkState)]
    pub fn set_link_state(
        &mut self,
        replica: usize,
        mode: &str,
    ) -> Result<Ts<SimLabState>, JsError> {
        self.set_link_state_inner(replica, mode)
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = crashAndRestart)]
    pub fn crash_and_restart(&mut self, replica: usize) -> Result<Ts<SimLabState>, JsError> {
        self.crash_and_restart_inner(replica)
            .map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = deliverNext)]
    pub fn deliver_next(&mut self) -> Result<Ts<SimLabState>, JsError> {
        self.deliver_one_inner().map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = deliverAll)]
    pub fn deliver_all(&mut self) -> Result<Ts<SimLabState>, JsError> {
        self.deliver_all_inner().map_err(|msg| JsError::new(&msg))?;
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }

    #[wasm_bindgen(js_name = inspectState)]
    pub fn inspect_state(&self) -> Result<Ts<SimLabState>, JsError> {
        Ts::from_rust(&self.state_snapshot()).map_err(Into::into)
    }
}

impl InteractiveSimLab {
    pub fn new_inner(replica_count: usize) -> Result<Self, String> {
        if !(2..=3).contains(&replica_count) {
            return Err(format!("replica count must be 2 or 3, got {replica_count}"));
        }
        let limits = ServerLimits {
            catchup_window: 16,
            snapshot_txn_threshold: 8,
            ..ServerLimits::default()
        };
        let server =
            ServerDocMachine::new(LAB_DOC_ID, LAB_BRANCH_ID, String::from("/api/w"), limits);
        let replicas: Vec<LabReplica> = LAB_USERS[..replica_count]
            .iter()
            .map(|(uid, name, role)| LabReplica::new(name, *uid, *role))
            .collect();

        let mut lab = Self {
            server,
            replicas,
            epoch_log: Vec::new(),
            snapshots_by_seq: BTreeMap::new(),
            next_conn_id: 1,
            timestamp_ms: 1_760_000_000_000,
        };
        for idx in 0..lab.replicas.len() {
            lab.connect_replica(idx);
        }
        lab.drain_online_replicas()?;
        Ok(lab)
    }

    pub fn edit_cell_inner(
        &mut self,
        replica: usize,
        row: u32,
        col: u32,
        raw: &str,
    ) -> Result<(), String> {
        let (sheet, row_id, col_id, op, prev_cell) = {
            let rep = self
                .replicas
                .get(replica)
                .ok_or_else(|| String::from("replica index out of range"))?;
            let sheet_id = SheetId(0);
            let sheet = rep
                .engine
                .workbook()
                .sheet(sheet_id)
                .ok_or_else(|| String::from("missing SheetId(0)"))?;
            let row_id = RowId(
                sheet
                    .row_axis()
                    .pos_to_id(row)
                    .ok_or_else(|| format!("row index {row} out of range"))?,
            );
            let col_id = ColId(
                sheet
                    .col_axis()
                    .pos_to_id(col)
                    .ok_or_else(|| format!("column index {col} out of range"))?,
            );
            let prev_cell = rep
                .machine
                .visible()
                .cell_snapshot(sheet_id, row_id, col_id)
                .ok_or_else(|| String::from("failed to inspect prior cell"))?;
            let op = rep
                .engine
                .build_cell_edit(sheet_id, row_id, col_id, raw)
                .map_err(|err| format!("invalid cell input: {err:?}"))?;
            (sheet_id, row_id, col_id, op, prev_cell)
        };

        let expected_after = match &op {
            Op::SetCells { edits, .. } => edits
                .first()
                .map(|e| {
                    (
                        discard_cache(e.input.clone()),
                        resolve_patch_fmt(prev_cell.explicit_format, e.format),
                    )
                })
                .unwrap_or((CellInput::Empty, prev_cell.explicit_format)),
            _ => (CellInput::Empty, prev_cell.explicit_format),
        };
        let undo_op = Op::SetCellsIf {
            sheet,
            edits: vec![ConditionalCellEdit {
                row: row_id,
                col: col_id,
                expected_input: expected_after.0,
                expected_format: expected_after.1,
                new_input: discard_cache(prev_cell.input),
                new_format: match prev_cell.explicit_format {
                    Some(fmt) => FormatPatch::Set(fmt),
                    None => FormatPatch::Clear,
                },
            }],
        };

        self.push_local_tx(
            replica,
            format!("Edit R{}C{}", row + 1, col + 1),
            vec![op],
            vec![undo_op],
        )
    }

    pub fn insert_row_inner(&mut self, replica: usize, after_row_idx: i32) -> Result<(), String> {
        let (after_id, run) = {
            let rep = self
                .replicas
                .get_mut(replica)
                .ok_or_else(|| String::from("replica index out of range"))?;
            let sheet = rep
                .machine
                .visible()
                .sheet(SheetId(0))
                .ok_or_else(|| String::from("missing SheetId(0)"))?;
            let after_id = if after_row_idx < 0 {
                None
            } else {
                let idx = u32::try_from(after_row_idx)
                    .map_err(|_| String::from("invalid row position"))?;
                Some(RowId(
                    sheet
                        .row_axis()
                        .pos_to_id(idx)
                        .ok_or_else(|| format!("row index {idx} out of range"))?,
                ))
            };
            let run = rep
                .machine
                .alloc_row_run(1)
                .ok_or_else(|| String::from("row ID space exhausted"))?;
            (after_id, run)
        };
        let op = Op::InsertRows {
            sheet: SheetId(0),
            after: after_id,
            ids: run,
        };
        let undo_op = Op::DeleteRows {
            sheet: SheetId(0),
            ids: vec![run],
        };
        self.push_local_tx(
            replica,
            format!("Insert Row {}", after_row_idx + 2),
            vec![op],
            vec![undo_op],
        )
    }

    pub fn delete_row_inner(&mut self, replica: usize, row_idx: u32) -> Result<(), String> {
        let row_id = {
            let rep = self
                .replicas
                .get(replica)
                .ok_or_else(|| String::from("replica index out of range"))?;
            let sheet = rep
                .machine
                .visible()
                .sheet(SheetId(0))
                .ok_or_else(|| String::from("missing SheetId(0)"))?;
            sheet
                .row_axis()
                .pos_to_id(row_idx)
                .ok_or_else(|| format!("row index {row_idx} out of range"))?
        };
        let run = IdRun {
            first_id: row_id,
            len: 1,
        };
        let op = Op::DeleteRows {
            sheet: SheetId(0),
            ids: vec![run],
        };
        let undo_op = Op::UndeleteRows {
            sheet: SheetId(0),
            ids: vec![run],
        };
        self.push_local_tx(
            replica,
            format!("Delete Row {}", row_idx + 1),
            vec![op],
            vec![undo_op],
        )
    }

    pub fn undo_inner(&mut self, replica: usize) -> Result<(), String> {
        let entry = {
            let rep = self
                .replicas
                .get_mut(replica)
                .ok_or_else(|| String::from("replica index out of range"))?;
            rep.undo_stack
                .pop()
                .ok_or_else(|| String::from("nothing to undo on this replica"))?
        };
        self.push_local_tx_without_undo(replica, format!("Undo {}", entry.label), entry.undo_ops)
    }

    pub fn set_link_state_inner(&mut self, replica: usize, mode: &str) -> Result<(), String> {
        let parsed = LinkMode::parse(mode)?;
        if replica >= self.replicas.len() {
            return Err(String::from("replica index out of range"));
        }
        self.replicas[replica].link_mode = parsed;
        match parsed {
            LinkMode::Dropped => {
                self.disconnect_replica(replica);
            }
            LinkMode::Delayed => {
                if self.replicas[replica].active_conn.is_none() {
                    self.connect_replica(replica);
                }
            }
            LinkMode::Online => {
                if self.replicas[replica].active_conn.is_none() {
                    self.connect_replica(replica);
                }
                self.drain_online_replicas()?;
            }
        }
        Ok(())
    }

    pub fn crash_and_restart_inner(&mut self, replica: usize) -> Result<(), String> {
        if replica >= self.replicas.len() {
            return Err(String::from("replica index out of range"));
        }
        self.disconnect_replica(replica);

        let cp = &self.replicas[replica].checkpoint;
        let wire_wms = cp
            .replica
            .map(|r| {
                vec![WireReplicaWatermarkV1 {
                    replica: r,
                    last_client_seq: cp.last_acked_client_seq,
                    max_id_counter: cp.next_id_counter_hint.saturating_sub(1),
                }]
            })
            .unwrap_or_default();
        let wire_permutes: Vec<WireCommittedPermuteV3> = cp
            .confirmed_permutes
            .iter()
            .map(WireCommittedPermuteV3::from)
            .collect();
        let snap_bytes = encode_snapshot_with_permutes(&cp.confirmed, &wire_wms, &wire_permutes)
            .map_err(|err| format!("OPFS snapshot encode failed: {err:?}"))?;
        let wal_bytes = encode_pending_wal_complete_v2(&cp.pending)
            .map_err(|err| format!("OPFS pending WAL encode failed: {err:?}"))?;

        let decoded = decode_snapshot_any(&snap_bytes)
            .map_err(|err| format!("OPFS snapshot decode failed: {err:?}"))?;
        let wal_scan = scan_pending_wal_any(&wal_bytes)
            .map_err(|err| format!("OPFS pending WAL scan failed: {err:?}"))?;
        let confirmed_permutes = decoded
            .permutes
            .iter()
            .map(CommittedPermute::from)
            .collect();

        let restored = ClientSyncMachine::from_persisted(
            LAB_DOC_ID,
            LAB_BRANCH_ID,
            cp.epoch,
            cp.replica,
            cp.confirmed_seq,
            cp.last_acked_client_seq,
            decoded.engine.workbook().clone(),
            confirmed_permutes,
            wal_scan.records,
            cp.next_id_counter_hint,
        );
        self.replicas[replica].machine = restored;
        self.replicas[replica].last_notice =
            Some(String::from("Restarted from persisted OPFS state"));
        self.replicas[replica].sync_engine_visible();

        if self.replicas[replica].link_mode != LinkMode::Dropped {
            self.connect_replica(replica);
            if self.replicas[replica].link_mode == LinkMode::Online {
                self.drain_online_replicas()?;
            }
        }
        Ok(())
    }

    pub fn deliver_one_inner(&mut self) -> Result<bool, String> {
        for idx in 0..self.replicas.len() {
            if self.replicas[idx].link_mode == LinkMode::Dropped {
                continue;
            }
            if !self.replicas[idx].c2s_queue.is_empty() {
                self.deliver_c2s_one(idx)?;
                self.drain_online_replicas()?;
                return Ok(true);
            }
            if self.replicas[idx].pending_snapshot_req.is_some() {
                self.deliver_snapshot(idx)?;
                self.drain_online_replicas()?;
                return Ok(true);
            }
            if !self.replicas[idx].s2c_queue.is_empty() {
                self.deliver_s2c_one(idx)?;
                self.drain_online_replicas()?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn deliver_all_inner(&mut self) -> Result<(), String> {
        for _ in 0..2_000 {
            if !self.deliver_one_inner()? {
                return Ok(());
            }
        }
        Err(String::from("deliverAll exceeded step bound"))
    }

    #[must_use]
    pub fn state_snapshot(&self) -> SimLabState {
        let server_digest = self.server.workbook().state_digest();
        let server_digest_hex = format_digest(server_digest);
        let server_head_seq = self.server.head_seq();
        let mut queue = Vec::new();
        let mut replicas = Vec::with_capacity(self.replicas.len());
        let mut all_converged = true;

        for (index, rep) in self.replicas.iter().enumerate() {
            let visible_digest = rep.machine.visible().state_digest();
            let converged = visible_digest == server_digest
                && rep.machine.confirmed_seq() == server_head_seq
                && rep.machine.pending_len() == 0;
            if !converged {
                all_converged = false;
            }
            replicas.push(SimLabReplicaView {
                index,
                name: rep.name.clone(),
                role: String::from(match rep.role {
                    MemberRole::Owner => "owner",
                    MemberRole::Editor => "editor",
                    MemberRole::Viewer => "viewer",
                }),
                replica_id: rep.machine.replica(),
                link_mode: String::from(rep.link_mode.as_str()),
                connection: String::from(connection_label(rep.machine.connection_state())),
                confirmed_seq: rep.machine.confirmed_seq(),
                pending_len: rep.machine.pending_len(),
                can_undo: !rep.undo_stack.is_empty(),
                digest: format_digest(visible_digest),
                converged,
                last_notice: rep.last_notice.clone(),
            });

            for item in &rep.c2s_queue {
                queue.push(SimWireQueueItem {
                    replica_index: index,
                    replica_name: rep.name.clone(),
                    direction: String::from("c2s"),
                    summary: item.summary.clone(),
                });
            }
            if let Some((_, snap_seq)) = rep.pending_snapshot_req {
                queue.push(SimWireQueueItem {
                    replica_index: index,
                    replica_name: rep.name.clone(),
                    direction: String::from("snapshot"),
                    summary: format!("Fetch snapshot #{snap_seq}"),
                });
            }
            for item in &rep.s2c_queue {
                queue.push(SimWireQueueItem {
                    replica_index: index,
                    replica_name: rep.name.clone(),
                    direction: String::from("s2c"),
                    summary: item.summary.clone(),
                });
            }
        }

        SimLabState {
            server_head_seq,
            server_epoch: self.server.epoch(),
            server_digest: server_digest_hex,
            all_converged,
            replicas,
            queue,
        }
    }

    fn push_local_tx(
        &mut self,
        replica: usize,
        label: String,
        ops: Vec<Op<BoundFormula>>,
        undo_ops: Vec<Op<BoundFormula>>,
    ) -> Result<(), String> {
        self.push_local_with_undo(replica, label, ops, Some(undo_ops))
    }

    fn push_local_tx_without_undo(
        &mut self,
        replica: usize,
        label: String,
        ops: Vec<Op<BoundFormula>>,
    ) -> Result<(), String> {
        self.push_local_with_undo(replica, label, ops, None)
    }

    fn push_local_with_undo(
        &mut self,
        replica: usize,
        label: String,
        ops: Vec<Op<BoundFormula>>,
        undo_ops: Option<Vec<Op<BoundFormula>>>,
    ) -> Result<(), String> {
        let rep = self
            .replicas
            .get_mut(replica)
            .ok_or_else(|| String::from("replica index out of range"))?;
        self.timestamp_ms = self.timestamp_ms.saturating_add(1);
        let client_seq_before = rep.machine.next_client_seq();
        let actions = rep
            .machine
            .push_local(label.clone(), ops, self.timestamp_ms)
            .map_err(|err| format!("local edit rejected: {err:?}"))?;
        if let Some(undo_ops) = undo_ops {
            rep.undo_stack.push(LabUndoEntry {
                client_seq: Some(client_seq_before),
                label,
                undo_ops,
            });
        }
        self.apply_client_actions(replica, actions);
        self.replicas[replica].sync_engine_visible();
        self.drain_online_replicas()?;
        Ok(())
    }

    fn connect_replica(&mut self, replica: usize) {
        if self.replicas[replica].active_conn.is_some() {
            return;
        }
        let conn_id = self.next_conn_id;
        self.next_conn_id = self.next_conn_id.saturating_add(1);
        self.replicas[replica].active_conn = Some(conn_id);
        let actions = self.replicas[replica].machine.on_connected();
        self.apply_client_actions(replica, actions);
    }

    fn disconnect_replica(&mut self, replica: usize) {
        if self.replicas[replica].active_conn.is_some() {
            self.replicas[replica].active_conn = None;
            self.replicas[replica].c2s_queue.clear();
            self.replicas[replica].s2c_queue.clear();
            self.replicas[replica].pending_snapshot_req = None;
            self.replicas[replica].welcome_head_seq = None;
            self.replicas[replica].machine.on_disconnected();
        }
    }

    fn drain_online_replicas(&mut self) -> Result<(), String> {
        for _ in 0..2_000 {
            let mut progressed = false;
            for idx in 0..self.replicas.len() {
                if self.replicas[idx].link_mode != LinkMode::Online {
                    continue;
                }
                if self.replicas[idx].active_conn.is_none() {
                    self.connect_replica(idx);
                    progressed = true;
                }
                while !self.replicas[idx].c2s_queue.is_empty() {
                    self.deliver_c2s_one(idx)?;
                    progressed = true;
                }
                if self.replicas[idx].pending_snapshot_req.is_some() {
                    self.deliver_snapshot(idx)?;
                    progressed = true;
                }
                while !self.replicas[idx].s2c_queue.is_empty() {
                    self.deliver_s2c_one(idx)?;
                    progressed = true;
                }
            }
            if !progressed {
                return Ok(());
            }
        }
        Err(String::from("online drain exceeded step bound"))
    }

    fn apply_client_actions(&mut self, replica: usize, actions: Vec<ClientAction>) {
        let mut reconnect = false;
        for action in actions {
            match action {
                ClientAction::PersistPending(_) | ClientAction::PersistTruncatedPending(_) => {
                    self.replicas[replica].sync_checkpoint();
                }
                ClientAction::SendFrame(frame) => {
                    if let Some(conn_id) = self.replicas[replica].active_conn
                        && let Ok(bytes) = encode_sync_frame_v2(&frame)
                    {
                        self.replicas[replica].c2s_queue.push_back(QueuedFrame {
                            conn_id,
                            summary: summarize_frame(&frame),
                            bytes,
                        });
                    }
                }
                ClientAction::RequestSnapshot { snapshot_seq, .. } => {
                    if let Some(conn_id) = self.replicas[replica].active_conn {
                        self.replicas[replica].pending_snapshot_req = Some((conn_id, snapshot_seq));
                    }
                }
                ClientAction::SaveUnsyncedLocal { txns, .. } => {
                    self.replicas[replica].last_notice = Some(format!(
                        "Rescued {} terminally rejected edit(s)",
                        txns.len()
                    ));
                }
                ClientAction::PruneUndoFrom(from_seq) => {
                    self.replicas[replica]
                        .undo_stack
                        .retain(|entry| entry.client_seq.is_none_or(|seq| seq < from_seq));
                }
                ClientAction::EpochChanged {
                    old_epoch,
                    new_epoch,
                } => {
                    self.replicas[replica].last_notice =
                        Some(format!("Server epoch changed {old_epoch} -> {new_epoch}"));
                }
                ClientAction::DigestMismatch { seq, .. } => {
                    self.replicas[replica].last_notice =
                        Some(format!("Digest mismatch detected at #{seq}; resyncing"));
                }
                ClientAction::NotifyVisibleChanged => {
                    self.replicas[replica].sync_engine_visible();
                }
                ClientAction::RequestReconnect => {
                    reconnect = true;
                }
            }
        }
        self.replicas[replica].sync_checkpoint();
        self.replicas[replica].sync_engine_visible();
        if reconnect {
            self.disconnect_replica(replica);
            if self.replicas[replica].link_mode != LinkMode::Dropped {
                self.connect_replica(replica);
            }
        }
    }

    fn enqueue_s2c(&mut self, replica: usize, conn_id: u64, frame: &WireSyncFrameV2) {
        if self.replicas[replica].active_conn != Some(conn_id) {
            return;
        }
        if let Ok(bytes) = encode_sync_frame_v2(frame) {
            self.replicas[replica].s2c_queue.push_back(QueuedFrame {
                conn_id,
                summary: summarize_frame(frame),
                bytes,
            });
        }
    }

    fn deliver_c2s_one(&mut self, replica: usize) -> Result<(), String> {
        let Some(queued) = self.replicas[replica].c2s_queue.pop_front() else {
            return Ok(());
        };
        if self.replicas[replica].active_conn != Some(queued.conn_id) {
            return Ok(());
        }
        let frame = decode_sync_frame_v2(&queued.bytes)
            .map_err(|err| format!("failed to decode client frame: {err:?}"))?;
        match frame {
            WireSyncFrameV2::Hello { .. } => {
                let user_id = self.replicas[replica].user_id;
                let role = Some(self.replicas[replica].role);
                match self.server.on_hello(user_id, role, &frame) {
                    Ok(welcome) => {
                        if let WireSyncFrameV2::Welcome { head_seq, .. } = &welcome {
                            self.replicas[replica].welcome_head_seq = Some(*head_seq);
                        }
                        self.enqueue_s2c(replica, queued.conn_id, &welcome);
                    }
                    Err(reason) => {
                        self.enqueue_s2c(
                            replica,
                            queued.conn_id,
                            &WireSyncFrameV2::Reject {
                                client_seq: 0,
                                reason,
                            },
                        );
                    }
                }
            }
            WireSyncFrameV2::Push { txns } => {
                if let Err(reason) = self.server.validate_push_batch(&txns, queued.bytes.len()) {
                    if let Some(first) = txns.first() {
                        self.enqueue_s2c(
                            replica,
                            queued.conn_id,
                            &WireSyncFrameV2::Reject {
                                client_seq: first.client_seq,
                                reason,
                            },
                        );
                    }
                    return Ok(());
                }
                let Some(session_rep) = self.replicas[replica].machine.replica() else {
                    self.disconnect_replica(replica);
                    return Ok(());
                };
                let user_id = self.replicas[replica].user_id;
                let role = Some(self.replicas[replica].role);
                self.timestamp_ms = self.timestamp_ms.saturating_add(1);
                for pending in txns {
                    let cseq = pending.client_seq;
                    match self.server.apply_and_commit(
                        user_id,
                        role,
                        session_rep,
                        pending,
                        self.timestamp_ms,
                    ) {
                        Ok(reticula_sync::ApplyResult::Committed(commit)) => {
                            self.epoch_log.push(commit.clone());
                            let ack = WireSyncFrameV2::Ack {
                                replica: commit.replica,
                                client_seq: commit.client_seq,
                                seq: commit.seq,
                                timestamp_ms: commit.timestamp_ms,
                            };
                            self.enqueue_s2c(replica, queued.conn_id, &ack);

                            let bcast = WireSyncFrameV2::Commit {
                                seq: commit.seq,
                                replica: commit.replica,
                                client_seq: commit.client_seq,
                                user_id: commit.user_id,
                                timestamp_ms: commit.timestamp_ms,
                                txn: commit.txn,
                            };
                            for idx in 0..self.replicas.len() {
                                if let Some(cid) = self.replicas[idx].active_conn {
                                    self.enqueue_s2c(idx, cid, &bcast);
                                }
                            }
                            self.maybe_write_server_snapshot()?;
                        }
                        Ok(reticula_sync::ApplyResult::Duplicate {
                            replica: ack_rep,
                            client_seq,
                            seq,
                            timestamp_ms,
                        }) => {
                            self.enqueue_s2c(
                                replica,
                                queued.conn_id,
                                &WireSyncFrameV2::Ack {
                                    replica: ack_rep,
                                    client_seq,
                                    seq,
                                    timestamp_ms,
                                },
                            );
                        }
                        Err(reason) => {
                            self.enqueue_s2c(
                                replica,
                                queued.conn_id,
                                &WireSyncFrameV2::Reject {
                                    client_seq: cseq,
                                    reason,
                                },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn deliver_s2c_one(&mut self, replica: usize) -> Result<(), String> {
        let Some(queued) = self.replicas[replica].s2c_queue.pop_front() else {
            return Ok(());
        };
        if self.replicas[replica].active_conn != Some(queued.conn_id) {
            return Ok(());
        }
        let frame = decode_sync_frame_v2(&queued.bytes)
            .map_err(|err| format!("failed to decode server frame: {err:?}"))?;
        let actions = match frame {
            WireSyncFrameV2::Welcome { .. } => self.replicas[replica].machine.on_welcome(frame),
            WireSyncFrameV2::Commit {
                seq,
                replica: commit_rep,
                client_seq,
                user_id,
                timestamp_ms,
                txn,
            } => self.replicas[replica]
                .machine
                .on_commit(WireCommittedTxnV3 {
                    seq,
                    replica: commit_rep,
                    client_seq,
                    user_id,
                    timestamp_ms,
                    txn,
                }),
            WireSyncFrameV2::Ack {
                replica: ack_rep,
                client_seq,
                ..
            } => {
                self.replicas[replica].machine.on_ack(ack_rep, client_seq);
                self.replicas[replica].sync_checkpoint();
                Vec::new()
            }
            WireSyncFrameV2::Reject { client_seq, reason } => {
                self.replicas[replica].machine.on_reject(client_seq, reason)
            }
            WireSyncFrameV2::Digest { seq, state_digest } => {
                self.replicas[replica].machine.on_digest(seq, state_digest)
            }
            _ => Vec::new(),
        };
        self.apply_client_actions(replica, actions);
        Ok(())
    }

    fn deliver_snapshot(&mut self, replica: usize) -> Result<(), String> {
        let Some((conn_id, snap_seq)) = self.replicas[replica].pending_snapshot_req else {
            return Ok(());
        };
        self.replicas[replica].pending_snapshot_req = None;
        if self.replicas[replica].active_conn != Some(conn_id) {
            return Ok(());
        }
        let (snap_wb, snap_wms, snap_permutes) = if snap_seq == 0 {
            (Workbook::new(), Vec::new(), Vec::new())
        } else {
            let (bytes, wms) = self
                .snapshots_by_seq
                .get(&snap_seq)
                .ok_or_else(|| format!("missing snapshot #{snap_seq}"))?;
            let decoded = decode_snapshot_any(bytes)
                .map_err(|err| format!("failed to decode snapshot #{snap_seq}: {err:?}"))?;
            let permutes = decoded
                .permutes
                .iter()
                .map(CommittedPermute::from)
                .collect();
            (decoded.engine.workbook().clone(), wms.clone(), permutes)
        };
        let welcome_head = self.replicas[replica]
            .welcome_head_seq
            .unwrap_or_else(|| self.server.head_seq());
        let tail: Vec<WireCommittedTxnV3> = self
            .epoch_log
            .iter()
            .filter(|txn| txn.seq > snap_seq && txn.seq <= welcome_head)
            .cloned()
            .collect();
        let actions = self.replicas[replica].machine.install_snapshot(
            snap_seq,
            snap_wb,
            &snap_wms,
            snap_permutes,
            tail,
        );
        self.apply_client_actions(replica, actions);
        Ok(())
    }

    fn maybe_write_server_snapshot(&mut self) -> Result<(), String> {
        let head = self.server.head_seq();
        if head > 0 && head.is_multiple_of(8) && !self.snapshots_by_seq.contains_key(&head) {
            let wms = self.server.watermarks();
            let wire_wms: Vec<WireReplicaWatermarkV1> = wms
                .iter()
                .map(|w| WireReplicaWatermarkV1 {
                    replica: w.replica,
                    last_client_seq: w.last_client_seq,
                    max_id_counter: w.max_id_counter,
                })
                .collect();
            let wire_permutes: Vec<WireCommittedPermuteV3> = self
                .server
                .committed_permutes()
                .iter()
                .map(WireCommittedPermuteV3::from)
                .collect();
            let bytes =
                encode_snapshot_with_permutes(self.server.workbook(), &wire_wms, &wire_permutes)
                    .map_err(|err| format!("server snapshot encode failed: {err:?}"))?;
            self.snapshots_by_seq.insert(head, (bytes, wms));
            self.server.mark_snapshot_written(head);
        }
        Ok(())
    }
}

/// Fault counts returned across 5-replica simulation runs (`Tsify`-friendly DTO).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct AutoSimFaultCounts {
    pub local_edits: u64,
    pub set_cells_if_ops: u64,
    pub formula_ops: u64,
    pub move_row_col_ops: u64,
    pub permute_content_ops: u64,
    pub move_cells_ops: u64,
    pub named_range_ops: u64,
    pub e16_transformed_edits: u64,
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

impl From<&FaultCounts> for AutoSimFaultCounts {
    fn from(f: &FaultCounts) -> Self {
        Self {
            local_edits: f.local_edits,
            set_cells_if_ops: f.set_cells_if_ops,
            formula_ops: f.formula_ops,
            move_row_col_ops: f.move_row_col_ops,
            permute_content_ops: f.permute_content_ops,
            move_cells_ops: f.move_cells_ops,
            named_range_ops: f.named_range_ops,
            e16_transformed_edits: f.e16_transformed_edits,
            structural_sheet_ops: f.structural_sheet_ops,
            reordered_deliveries: f.reordered_deliveries,
            slow_receiver_disconnects: f.slow_receiver_disconnects,
            snapshot_required_catchups: f.snapshot_required_catchups,
            client_crashes: f.client_crashes,
            opfs_tail_losses: f.opfs_tail_losses,
            server_crashes: f.server_crashes,
            epoch_backup_restores: f.epoch_backup_restores,
            role_downgrades: f.role_downgrades,
            terminal_rejects: f.terminal_rejects,
            duplicate_acks: f.duplicate_acks,
            rogue_attempts_rejected: f.rogue_attempts_rejected,
            two_tab_same_user_commits: f.two_tab_same_user_commits,
        }
    }
}

/// Terminal digest summary matching `reticula_sim::SeedSummary`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct AutoSeedSummary {
    pub seed: String,
    pub final_epoch: u8,
    pub final_head_seq: u64,
    pub final_digest: String,
    pub final_digest_lo: String,
    pub final_digest_hi: String,
    pub final_digest_order: String,
    pub final_logical_digest: String,
    pub faults: AutoSimFaultCounts,
}

impl From<SeedSummary> for AutoSeedSummary {
    fn from(s: SeedSummary) -> Self {
        Self {
            seed: s.seed.to_string(),
            final_epoch: s.final_epoch,
            final_head_seq: s.final_head_seq,
            final_digest: format!(
                "{:016x}{:016x}:{:016x}",
                s.final_digest_hi, s.final_digest_lo, s.final_digest_order
            ),
            final_digest_lo: s.final_digest_lo.to_string(),
            final_digest_hi: s.final_digest_hi.to_string(),
            final_digest_order: s.final_digest_order.to_string(),
            final_logical_digest: s.final_logical_digest.to_string(),
            faults: AutoSimFaultCounts::from(&s.faults),
        }
    }
}

/// Replica state row inside [`AutoSimProgress`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct AutoReplicaState {
    pub index: usize,
    pub role: String,
    pub replica: Option<u32>,
    pub connection: String,
    pub confirmed_seq: u64,
    pub pending_len: usize,
    pub queued_c2s: usize,
    pub queued_s2c: usize,
}

impl From<SimReplicaState> for AutoReplicaState {
    fn from(r: SimReplicaState) -> Self {
        Self {
            index: r.index,
            role: r.role,
            replica: r.replica,
            connection: r.connection,
            confirmed_seq: r.confirmed_seq,
            pending_len: r.pending_len,
            queued_c2s: r.queued_c2s,
            queued_s2c: r.queued_s2c,
        }
    }
}

/// Incremental progress returned by [`AutoSimRunner::step_batch`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct AutoSimProgress {
    pub seed: String,
    pub total_steps: usize,
    pub completed_steps: usize,
    pub finished: bool,
    pub passed: bool,
    pub error: Option<String>,
    pub head_seq: u64,
    pub epoch: u8,
    pub server_digest: String,
    pub faults: AutoSimFaultCounts,
    pub replicas: Vec<AutoReplicaState>,
    pub summary: Option<AutoSeedSummary>,
}

/// Stepwise 5-replica deterministic fault-injection runner for the `/sim` lab.
#[wasm_bindgen]
pub struct AutoSimRunner {
    seed: u64,
    script: Vec<SimEvent>,
    cursor: usize,
    world: SimWorld,
    finished: bool,
    summary: Option<SeedSummary>,
    error: Option<String>,
}

#[wasm_bindgen]
impl AutoSimRunner {
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            script: generate_script(seed),
            cursor: 0,
            world: SimWorld::new(seed),
            finished: false,
            summary: None,
            error: None,
        }
    }

    #[wasm_bindgen(js_name = totalSteps)]
    #[must_use]
    pub fn total_steps(&self) -> usize {
        self.script.len()
    }

    #[wasm_bindgen(js_name = completedSteps)]
    #[must_use]
    pub fn completed_steps(&self) -> usize {
        self.cursor
    }

    #[wasm_bindgen(js_name = isFinished)]
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    #[wasm_bindgen(js_name = stepBatch)]
    pub fn step_batch(&mut self, count: usize) -> Result<Ts<AutoSimProgress>, JsError> {
        let progress = self.step_batch_inner(count);
        Ts::from_rust(&progress).map_err(Into::into)
    }
}

impl AutoSimRunner {
    pub fn step_batch_inner(&mut self, count: usize) -> AutoSimProgress {
        if !self.finished && count > 0 {
            let limit = self.cursor.saturating_add(count).min(self.script.len());
            while self.cursor < limit {
                let event = self.script[self.cursor];
                if let Err(err) = self.world.step(event) {
                    self.error = Some(format!("step {} ({event:?}): {err}", self.cursor));
                    self.finished = true;
                    break;
                }
                self.cursor += 1;
            }
            if !self.finished && self.cursor >= self.script.len() {
                match self.world.drain_and_verify() {
                    Ok(mut sum) => {
                        sum.seed = self.seed;
                        self.summary = Some(sum);
                    }
                    Err(err) => {
                        self.error = Some(err);
                    }
                }
                self.finished = true;
            }
        }

        AutoSimProgress {
            seed: self.seed.to_string(),
            total_steps: self.script.len(),
            completed_steps: self.cursor,
            finished: self.finished,
            passed: self.finished && self.error.is_none(),
            error: self.error.clone(),
            head_seq: self.world.head_seq(),
            epoch: self.world.epoch(),
            server_digest: format_digest(self.world.server_digest()),
            faults: AutoSimFaultCounts::from(self.world.faults()),
            replicas: self
                .world
                .replica_states()
                .into_iter()
                .map(AutoReplicaState::from)
                .collect(),
            summary: self.summary.clone().map(AutoSeedSummary::from),
        }
    }
}

fn format_digest(sd: StateDigest) -> String {
    format!(
        "{:016x}{:016x}:{:016x}",
        sd.content_hi, sd.content_lo, sd.order
    )
}

fn connection_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Offline => "offline",
        ConnectionState::Handshaking => "handshaking",
        ConnectionState::AwaitingSnapshot { .. } => "awaitingSnapshot",
        ConnectionState::Online => "online",
    }
}

fn summarize_frame(frame: &WireSyncFrameV2) -> String {
    match frame {
        WireSyncFrameV2::Hello {
            last_applied_seq, ..
        } => format!("Hello (seq #{last_applied_seq})"),
        WireSyncFrameV2::Welcome {
            replica,
            head_seq,
            catchup,
            ..
        } => match catchup {
            WireCatchupV2::SnapshotRequired { snapshot_seq, .. } => {
                format!("Welcome (r{replica}, head #{head_seq}, snap #{snapshot_seq})")
            }
            WireCatchupV2::Txns { txns, .. } if !txns.is_empty() => {
                format!("Welcome (r{replica}, head #{head_seq}, {} txn)", txns.len())
            }
            WireCatchupV2::Txns { .. } => format!("Welcome (r{replica}, head #{head_seq})"),
        },
        WireSyncFrameV2::Push { txns } => match txns.first() {
            Some(first) => format!("Push {} txn(s) from #{}", txns.len(), first.client_seq),
            None => String::from("Push (empty)"),
        },
        WireSyncFrameV2::Commit { seq, replica, .. } => format!("Commit #{seq} (r{replica})"),
        WireSyncFrameV2::Ack {
            replica,
            client_seq,
            seq,
            ..
        } => format!("Ack r{replica} c#{client_seq} -> #{seq}"),
        WireSyncFrameV2::Reject { client_seq, reason } => {
            format!("Reject c#{client_seq}: {reason:?}")
        }
        WireSyncFrameV2::Digest { seq, .. } => format!("Digest #{seq}"),
        WireSyncFrameV2::Presence(_) => String::from("Presence"),
        WireSyncFrameV2::Ping(n) => format!("Ping #{n}"),
        WireSyncFrameV2::Pong(n) => format!("Pong #{n}"),
    }
}

fn discard_cache<F>(input: CellInput<F>) -> CellInput<F> {
    match input {
        CellInput::Formula { formula, .. } => CellInput::Formula {
            formula,
            cached: None,
        },
        other => other,
    }
}

fn resolve_patch_fmt(
    prev: Option<reticula_core::format::NumberFormat>,
    patch: FormatPatch,
) -> Option<reticula_core::format::NumberFormat> {
    match patch {
        FormatPatch::Keep => prev,
        FormatPatch::Clear => None,
        FormatPatch::Set(fmt) => Some(fmt),
    }
}

/// Sanity helper ensuring any idle rollback helper remains consistent with `InverseOp` usage.
#[allow(dead_code)]
fn _check_inverse_unused(_: &InverseOp<BoundFormula>, _: &[InverseOp<BoundFormula>]) {}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use reticula_core::id::{ColId, RowId, SheetId};
    use reticula_core::value::Value;
    use reticula_core::viewport::{HEADER_LEN, PAGE_ROWS, VERSION};
    use reticula_sim::run_seed;

    use super::{AutoSimRunner, InteractiveSimLab};

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn auto_sim_runner_matches_run_seed_42_bit_for_bit() {
        let expected = run_seed(42).expect("seed 42 passes");
        let mut runner = AutoSimRunner::new(42);
        let mut last = None;
        while !runner.is_finished() {
            last = Some(runner.step_batch_inner(17));
        }
        let progress = last.expect("at least one batch executed");
        assert!(progress.finished);
        assert!(progress.passed, "unexpected error: {:?}", progress.error);
        let summary = progress.summary.expect("terminal summary produced");
        assert_eq!(summary.seed, "42");
        assert_eq!(summary.final_epoch, expected.final_epoch);
        assert_eq!(summary.final_head_seq, expected.final_head_seq);
        assert_eq!(
            summary.final_digest_lo,
            expected.final_digest_lo.to_string()
        );
        assert_eq!(
            summary.final_digest_hi,
            expected.final_digest_hi.to_string()
        );
        assert_eq!(
            summary.final_digest_order,
            expected.final_digest_order.to_string()
        );
        assert_eq!(
            summary.final_logical_digest,
            expected.final_logical_digest.to_string()
        );
        assert_eq!(summary.faults.local_edits, expected.faults.local_edits);
        assert_eq!(
            summary.faults.server_crashes,
            expected.faults.server_crashes
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn interactive_lab_converges_after_offline_delayed_edits_crash_and_undo() {
        let mut lab = InteractiveSimLab::new_inner(3).expect("3-replica lab constructs");
        lab.set_link_state_inner(1, "dropped").unwrap();
        lab.set_link_state_inner(2, "delayed").unwrap();

        // Replica 0 edits online; replica 1 offline; replica 2 delayed with formula + undo.
        lab.edit_cell_inner(0, 0, 0, "10").unwrap();
        lab.edit_cell_inner(1, 1, 0, "20").unwrap();
        lab.edit_cell_inner(2, 2, 0, "=SUM(A1:A2)").unwrap();
        lab.insert_row_inner(2, 0).unwrap();
        lab.undo_inner(2).unwrap();

        // Simulate browser tab crash & restart from OPFS on dropped replica 1 while pending edits exist.
        lab.crash_and_restart_inner(1).unwrap();
        assert!(!lab.state_snapshot().all_converged);

        // Bring all replicas online/deliver queues and verify bit-identical convergence.
        lab.set_link_state_inner(1, "online").unwrap();
        lab.deliver_all_inner().unwrap();

        let final_state = lab.state_snapshot();
        assert!(
            final_state.all_converged,
            "expected all replicas to converge, got {final_state:?}"
        );
        for rep in &final_state.replicas {
            assert_eq!(rep.pending_len, 0);
            assert_eq!(rep.digest, final_state.server_digest);
        }

        // Verify formula evaluation on SheetId(0) C1/A3.
        let sheet0 = lab.replicas[0].engine.workbook().sheet(SheetId(0)).unwrap();
        let r2 = sheet0.row_axis().pos_to_id(2).unwrap();
        let c0 = sheet0.col_axis().pos_to_id(0).unwrap();
        assert_eq!(
            sheet0.value_by_id(RowId(r2), ColId(c0)),
            Value::Number(30.0)
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn interactive_lab_encodes_valid_viewport_v2_pages_for_grid() {
        let mut lab = InteractiveSimLab::new_inner(2).expect("2-replica lab constructs");
        lab.edit_cell_inner(0, 0, 0, "42").unwrap();
        lab.edit_cell_inner(0, 0, 1, "=A1*2").unwrap();
        let sheet1 = lab.replicas[1].engine.workbook().sheet(SheetId(0)).unwrap();
        let r0 = sheet1.row_axis().pos_to_id(0).unwrap();
        let c0 = sheet1.col_axis().pos_to_id(0).unwrap();
        let c1 = sheet1.col_axis().pos_to_id(1).unwrap();
        assert_eq!(
            sheet1.value_by_id(RowId(r0), ColId(c0)),
            Value::Number(42.0)
        );
        assert_eq!(
            sheet1.value_by_id(RowId(r0), ColId(c1)),
            Value::Number(84.0)
        );

        let page_bytes = lab
            .encode_page(1, 0, 0, 1)
            .expect("encode_page succeeds")
            .expect("page exists");
        assert!(page_bytes.len() > HEADER_LEN + (PAGE_ROWS as usize) * 8);
        assert_eq!(&page_bytes[..4], &VERSION.to_le_bytes());
        assert!(page_bytes.windows(2).any(|w| w == b"42"));
        assert!(page_bytes.windows(2).any(|w| w == b"84"));
    }
}
