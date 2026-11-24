//! Benchmark B9 (`--features bench`): what the slow path of collaborative sync
//! costs when a remote commit arrives while local edits are still pending.
//!
//! The slow path rewinds every pending transaction, applies the remote one and
//! replays the pending ones on a fresh copy of the confirmed state. The worker
//! then rebuilds the formula engine from the reconciled workbook and
//! recalculates its formulas in time slices. Each fixture measures both: the
//! sync machine alone, and the whole worker from the incoming frame to the
//! last recalculation slice.
//!
//! The document is the app's sample cut to 100,000 data rows: 22 columns and
//! 100,000 formulas `=ROUND(J*K*(1-L-$V$1), 2)`. Pending local edits change
//! column J, which those formulas read; the remote edits change column A of
//! rows no local edit touches.

use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue, Workbook};
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::FormulaEngine;
use reticula_sync::{ClientAction, ClientSyncMachine};
use reticula_wire::convert_v3::ops_to_wire_txn_v3;
use reticula_wire::snapshot::encode_snapshot_v1;
use reticula_wire::sync_frame::encode_sync_frame_v2;
use reticula_wire::v3::{WireCatchupV2, WireCommittedTxnV3, WireSyncFrameV2};
use wasm_bindgen::prelude::*;

use crate::collab::{SyncCheckpointInfo, SyncCommand};
use crate::{Engine, EngineCommand, now_ms};

const DOC: [u8; 16] = [9; 16];
const BRANCH: [u8; 16] = [0; 16];
const REMOTE_USER: [u8; 16] = [2; 16];
/// Data rows below the header row.
const DATA_ROWS: u32 = 100_000;
const SHEET: SheetId = SheetId(0);
const COL_A: ColId = ColId(0);
const COL_J: ColId = ColId(9);
/// Columns J..L and N..T: everything from J to T except the formulas in M.
const BLOCK_COLS: [u64; 10] = [9, 10, 11, 13, 14, 15, 16, 17, 18, 19];

/// What is waiting to be confirmed when the remote commits arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[wasm_bindgen]
pub enum B9Case {
    /// B9a: 1,000 single-cell edits on separate rows.
    ThousandSingleCells = 0,
    /// B9b: one 100,000-cell transaction (a pasted block) followed by 9
    /// single-cell edits.
    BlockOf100kCells = 1,
    /// Control: nothing pending, so the worker applies the commit in place.
    NothingPending = 2,
}

fn number(row: u32, col: ColId, value: f64) -> CellEdit<BoundFormula> {
    CellEdit {
        row: RowId(u64::from(row)),
        col,
        input: CellInput::Scalar(ScalarValue::Number(value)),
        format: FormatPatch::Keep,
    }
}

fn set_cells(edits: Vec<CellEdit<BoundFormula>>) -> Op<BoundFormula> {
    Op::SetCells {
        sheet: SHEET,
        edits,
    }
}

/// The pending transactions of `case`, in order.
fn pending_ops(case: B9Case) -> Vec<Vec<Op<BoundFormula>>> {
    match case {
        B9Case::ThousandSingleCells => (1..=1_000)
            .map(|row| {
                vec![set_cells(vec![number(
                    row,
                    COL_J,
                    f64::from(row % 7) + 1.0,
                )])]
            })
            .collect(),
        B9Case::BlockOf100kCells => {
            // Rows 1..=10,000 of ten data columns around the formula column M:
            // a pasted block of 100,000 cells that leaves the formulas alone.
            // One transaction; each column is one operation, since an
            // operation writes at most `MAX_EDITS_PER_OP` (10,000) cells.
            let block = BLOCK_COLS
                .iter()
                .map(|&col| {
                    set_cells(
                        (1..=10_000)
                            .map(|row| number(row, ColId(col), f64::from(row % 13) + 0.5))
                            .collect(),
                    )
                })
                .collect();
            let mut txns = vec![block];
            txns.extend((20_001..20_010).map(|row| vec![set_cells(vec![number(row, COL_J, 3.0)])]));
            txns
        }
        B9Case::NothingPending => Vec::new(),
    }
}

fn base_workbook() -> Workbook<BoundFormula> {
    FormulaEngine::open_sample(42, DATA_ROWS + 1)
        .workbook()
        .clone()
}

/// The `n`-th remote commit: one cell of column A, below every pending edit.
fn remote_commit(seq: u64, n: u64) -> WireCommittedTxnV3 {
    let row = 50_000 + u32::try_from(n % 40_000).unwrap_or(0);
    let value = f64::from(u32::try_from(n % 1_000).unwrap_or(0));
    WireCommittedTxnV3 {
        seq,
        replica: REMOTE_REPLICA,
        client_seq: n,
        user_id: REMOTE_USER,
        timestamp_ms: 0,
        txn: ops_to_wire_txn_v3(
            "Remote edit".into(),
            &[set_cells(vec![number(row, COL_A, value)])],
        )
        .expect("bench op fits wire v3"),
    }
}

const LOCAL_REPLICA: u32 = 2;
const REMOTE_REPLICA: u32 = 3;

/// One B9 fixture, stepped by the native runner or by the benchmark page.
#[wasm_bindgen]
pub struct SyncBench {
    machine: ClientSyncMachine,
    machine_seq: u64,
    engine: Engine,
    engine_seq: u64,
    remote_count: u64,
    pending: usize,
    pending_cells: usize,
}

#[wasm_bindgen]
impl SyncBench {
    /// Builds both the bare sync machine and a collaborative worker engine on
    /// the 100,000-row document, with `case`'s edits pending in each.
    ///
    /// # Panics
    ///
    /// Panics if the fixture cannot be set up, which is a bug in the benchmark.
    #[must_use]
    pub fn new(case: B9Case) -> Self {
        let workbook = base_workbook();
        let txns = pending_ops(case);
        let pending_cells = txns
            .iter()
            .flatten()
            .map(|op| match op {
                Op::SetCells { edits, .. } => edits.len(),
                _ => 0,
            })
            .sum();

        // The sync machine on its own, online, with the edits pushed but not
        // yet committed.
        let mut machine = ClientSyncMachine::from_persisted(
            DOC,
            BRANCH,
            0,
            Some(LOCAL_REPLICA),
            1,
            0,
            workbook.clone(),
            Vec::new(),
            Vec::new(),
            0,
        );
        let _ = machine.on_connected();
        let _ = machine.on_welcome(welcome(1));
        for ops in &txns {
            machine
                .push_local("Local edit".into(), ops.clone(), 0)
                .expect("pending edit applies");
        }
        assert_eq!(machine.pending_len(), txns.len());

        // The worker engine, opened from a checkpoint the way the page opens
        // a collaborative workbook, subscribed to a first screen of cells.
        let snapshot = encode_snapshot_v1(&workbook, &[]).expect("snapshot encodes");
        drop(workbook);
        let mut engine = Engine::new();
        let opened = engine.handle_sync_command(SyncCommand::Open {
            doc_id: uuid(DOC),
            branch_id: uuid(BRANCH),
            info: Some(SyncCheckpointInfo {
                epoch: 0,
                replica: Some(LOCAL_REPLICA),
                confirmed_seq: 1,
                confirmed_client_seq: 0,
                next_id_counter: 0,
            }),
            snapshot,
            pending_wal: Vec::new(),
        });
        assert!(matches!(opened, Some(crate::EngineEvent::Ready { .. })));
        let _ = engine.handle_command(EngineCommand::Subscribe {
            sheet: 0,
            epoch: 1,
            first_row: 0,
            row_count: 64,
            first_col: 0,
            col_count: 22,
            visible: None,
            resend: false,
        });
        let _ = engine.handle_sync_command(SyncCommand::Connected);
        let _ = engine.handle_sync_command(SyncCommand::Receive {
            frame: encode_sync_frame_v2(&welcome(1)).expect("welcome encodes"),
        });
        for ops in &txns {
            engine
                .execute_user_txn("Local edit".into(), ops)
                .expect("pending edit applies");
        }
        let mut bench = Self {
            machine,
            machine_seq: 1,
            engine,
            engine_seq: 1,
            remote_count: 0,
            pending: txns.len(),
            pending_cells,
        };
        bench.drain();
        assert_eq!(
            bench
                .engine
                .collab
                .as_ref()
                .map(|session| session.machine.pending_len()),
            Some(bench.pending)
        );
        bench
    }

    /// Pending transactions in this fixture.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Cells written by the pending transactions.
    #[must_use]
    pub fn pending_cells(&self) -> usize {
        self.pending_cells
    }

    /// Delivers the next remote commit to the bare sync machine: the rewind,
    /// apply and replay of the slow path, with the incremental state digest.
    ///
    /// # Panics
    ///
    /// Panics if the machine asks to reconnect, which would mean the fixture
    /// fell out of step.
    pub fn step_machine(&mut self) {
        self.machine_seq += 1;
        self.remote_count += 1;
        let actions = self
            .machine
            .on_commit(remote_commit(self.machine_seq, self.remote_count));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, ClientAction::RequestReconnect)),
            "the sync machine fell out of step"
        );
    }

    /// Delivers the next remote commit to the worker engine as an encoded
    /// frame. Returns the milliseconds the call itself took; the caller times
    /// [`Self::finish_engine`] after it for the total.
    pub fn step_engine(&mut self) -> f64 {
        self.engine_seq += 1;
        self.remote_count += 1;
        let frame = encode_sync_frame_v2(&WireSyncFrameV2::Commit {
            seq: self.engine_seq,
            replica: REMOTE_REPLICA,
            client_seq: self.remote_count,
            user_id: REMOTE_USER,
            timestamp_ms: 0,
            txn: remote_commit(self.engine_seq, self.remote_count).txn,
        })
        .expect("commit encodes");
        let started = now_ms();
        let _ = self
            .engine
            .handle_sync_command(SyncCommand::Receive { frame });
        now_ms() - started
    }

    /// Runs the worker's time-sliced recalculation to the end and takes the
    /// pages and events it produced, as the worker loop does. Returns the
    /// number of recalculation slices.
    pub fn finish_engine(&mut self) -> u32 {
        let mut slices = 0;
        while self
            .engine
            .handle_command(EngineCommand::ContinueRecalc)
            .is_some()
        {
            slices += 1;
        }
        self.drain();
        slices
    }

    /// Breakdown: one clone of the sync machine's visible workbook, the copy
    /// the slow path makes of the confirmed state on every remote commit.
    pub fn step_clone(&self) {
        drop(core::hint::black_box(self.machine.visible().clone()));
    }

    /// Breakdown: rebuilds the worker's formula engine from a copy of its own
    /// workbook, as it does after a reconciled commit, and marks every formula
    /// for recalculation. Returns the milliseconds of the rebuild alone, the
    /// copy excluded; run [`Self::finish_engine`] after it for the
    /// recalculation.
    pub fn step_rebuild(&mut self) -> f64 {
        let workbook = self.engine.engine.workbook().clone();
        let started = now_ms();
        self.engine.engine.replace_reconciled_workbook(workbook);
        now_ms() - started
    }

    fn drain(&mut self) {
        while self.engine.take_page().is_some() {}
        while self.engine.pop_collab_event().is_some() {}
        while self.engine.take_pending_wal_frame().is_some() {}
    }
}

fn welcome(head_seq: u64) -> WireSyncFrameV2 {
    WireSyncFrameV2::Welcome {
        epoch: 0,
        head_seq,
        replica: LOCAL_REPLICA,
        last_client_seq: 0,
        max_id_counter: 0,
        catchup: WireCatchupV2::Txns {
            from_seq: head_seq + 1,
            txns: Vec::new(),
        },
    }
}

fn uuid(bytes: [u8; 16]) -> String {
    uuid::Uuid::from_bytes(bytes).to_string()
}

// The replica IDs above are `pack_replica_id(0, 2)` and `pack_replica_id(0, 3)`.
const _: () = {
    assert!(matches!(pack_replica_id(0, 2), Some(LOCAL_REPLICA)));
    assert!(matches!(pack_replica_id(0, 3), Some(REMOTE_REPLICA)));
};
