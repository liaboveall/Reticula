use reticula_core::id::{ColId, IdRun, RowId, pack_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, ScalarValue};
use reticula_core::value::Value;
use reticula_sync::{ApplyResult, MemberRole, ServerDocMachine, ServerLimits};
use reticula_wire::sync_frame::decode_sync_frame_v2;
use reticula_wire::v2::WireRejectReasonV1;
use reticula_wire::v2::WireStateDigestV1;
use reticula_wire::v3::{WireCommittedTxnV3, WirePendingTxnV3, WireSyncFrameV2};
use reticula_wire::wal::encode_pending_wal_init_v2;

use super::*;
use crate::{EngineCommand, WireIdRun};

const DOC: [u8; 16] = [8; 16];
const MAIN: [u8; 16] = [0; 16];
const ALICE: [u8; 16] = [1; 16];
const BOB: [u8; 16] = [2; 16];

fn open(info: Option<SyncCheckpointInfo>, snapshot: Vec<u8>, pending_wal: Vec<u8>) -> Engine {
    let mut engine = Engine::new();
    assert!(matches!(
        engine.handle_sync_command(SyncCommand::Open {
            doc_id: Uuid::from_bytes(DOC).to_string(),
            branch_id: Uuid::from_bytes(MAIN).to_string(),
            info,
            snapshot,
            pending_wal,
        }),
        Some(EngineEvent::Ready { .. })
    ));
    engine
}

fn connect() -> (Engine, ServerDocMachine) {
    let mut engine = open(None, Vec::new(), Vec::new());
    let mut server = ServerDocMachine::new(
        DOC,
        MAIN,
        "/api/workbooks/test".into(),
        ServerLimits::default(),
    );
    let _ = engine.handle_sync_command(SyncCommand::Connected);
    let hello = sent(events(&mut engine));
    let welcome = server
        .on_hello(ALICE, Some(MemberRole::Owner), &hello)
        .unwrap();
    receive(&mut engine, &welcome);
    events(&mut engine);
    (engine, server)
}

fn receive(engine: &mut Engine, frame: &WireSyncFrameV2) {
    let _ = engine.handle_sync_command(SyncCommand::Receive {
        frame: encode_sync_frame_v2(frame).unwrap(),
    });
}

fn events(engine: &mut Engine) -> Vec<CollabEvent> {
    std::iter::from_fn(|| engine.pop_collab_event()).collect()
}

fn sent(events: Vec<CollabEvent>) -> WireSyncFrameV2 {
    events
        .into_iter()
        .find_map(|event| match event {
            CollabEvent::Send { bytes } => Some(decode_sync_frame_v2(&bytes).unwrap()),
            _ => None,
        })
        .expect("expected an outbound frame")
}

fn pending(frame: WireSyncFrameV2) -> WirePendingTxnV3 {
    let WireSyncFrameV2::Push { mut txns } = frame else {
        panic!("expected Push")
    };
    assert_eq!(txns.len(), 1);
    txns.pop().unwrap()
}

fn commit(
    server: &mut ServerDocMachine,
    user: [u8; 16],
    txn: WirePendingTxnV3,
) -> WireCommittedTxnV3 {
    let accepted = server
        .apply_and_commit(user, Some(MemberRole::Editor), txn.replica, txn, 12)
        .unwrap();
    let ApplyResult::Committed(commit) = accepted else {
        panic!("expected a fresh commit")
    };
    commit
}

fn deliver(engine: &mut Engine, commit: WireCommittedTxnV3) {
    receive(
        engine,
        &WireSyncFrameV2::Commit {
            seq: commit.seq,
            replica: commit.replica,
            client_seq: commit.client_seq,
            user_id: commit.user_id,
            timestamp_ms: commit.timestamp_ms,
            txn: commit.txn,
        },
    );
}

fn edit_op(row: u64, value: f64) -> Op<BoundFormula> {
    Op::SetCells {
        sheet: SheetId(0),
        edits: vec![CellEdit {
            row: RowId(row),
            col: ColId(0),
            input: CellInput::Scalar(ScalarValue::Number(value)),
            format: FormatPatch::Keep,
        }],
    }
}

fn raw(engine: &mut Engine, row: u64, raw: &str) {
    assert!(!matches!(
        engine.handle_command(EngineCommand::EditCell {
            sheet: 0,
            row_id: row,
            col_id: 0,
            raw: raw.into(),
        }),
        Some(EngineEvent::Rejected { .. })
    ));
}

fn value(engine: &Engine, row: u64) -> Value {
    engine
        .engine
        .workbook()
        .sheet(SheetId(0))
        .unwrap()
        .value_by_id(RowId(row), ColId(0))
}

fn checkpoint(engine: &mut Engine) -> SyncCheckpoint {
    let _ = engine.handle_sync_command(SyncCommand::Checkpoint);
    events(engine)
        .into_iter()
        .find_map(|event| match event {
            CollabEvent::Checkpoint { checkpoint } => Some(checkpoint),
            _ => None,
        })
        .unwrap()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn pending_wal_precedes_send_and_checkpoint_tracks_commit_instead_of_ack() {
    let (mut engine, mut server) = connect();
    let _ = engine.handle_command(EngineCommand::DeleteRows {
        sheet: 0,
        rows: vec![WireIdRun {
            first_id: 2,
            len: 1,
        }],
    });
    let output = events(&mut engine);
    let CollabEvent::AppendPending { bytes } = &output[0] else {
        panic!("pending WAL must be first")
    };
    let mut original_pending = encode_pending_wal_init_v2();
    original_pending.extend_from_slice(bytes);
    let txn = pending(sent(output));
    let commit = commit(&mut server, ALICE, txn);
    receive(
        &mut engine,
        &WireSyncFrameV2::Ack {
            replica: commit.replica,
            client_seq: commit.client_seq,
            seq: commit.seq,
            timestamp_ms: commit.timestamp_ms,
        },
    );
    let before = checkpoint(&mut engine);
    assert_eq!(before.info.confirmed_seq, 0);
    assert_eq!(before.info.confirmed_client_seq, 0);
    assert_eq!(
        open(Some(before.info), before.snapshot, original_pending.clone())
            .collab
            .unwrap()
            .machine
            .pending_len(),
        1
    );

    deliver(&mut engine, commit);
    let after = checkpoint(&mut engine);
    assert_eq!(after.info.confirmed_seq, 1);
    assert_eq!(after.info.confirmed_client_seq, 1);
    let reopened = open(Some(after.info), after.snapshot, original_pending);
    let session = reopened.collab.unwrap();
    assert_eq!(session.machine.pending_len(), 0);
    assert_eq!(
        session.machine.visible().state_digest(),
        server.workbook().state_digest()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn collaborative_undo_and_redo_preserve_remote_cells_and_reject_saves_before_truncating() {
    let (mut engine, mut server) = connect();
    engine
        .execute_user_txn(
            "Compound edit".into(),
            &[edit_op(0, 10.0), edit_op(1, 20.0), edit_op(1, 21.0)],
        )
        .unwrap();
    let own = commit(&mut server, ALICE, pending(sent(events(&mut engine))));
    deliver(&mut engine, own);
    let bob = server.allocate_replica(BOB).unwrap();
    let overwrite = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 1,
            timestamp_ms: 11,
            txn: ops_to_wire_txn_v3("Bob edited A1".into(), &[edit_op(0, 77.0)]).unwrap(),
        },
    );
    deliver(&mut engine, overwrite);
    events(&mut engine);

    let _ = engine.handle_command(EngineCommand::Undo);
    assert_eq!(value(&engine, 0), Value::Number(77.0));
    assert_eq!(value(&engine, 1), Value::Empty);
    let undo = pending(sent(events(&mut engine)));
    let (_, ops) = ops_from_wire_txn_v3(undo.txn.clone()).unwrap();
    assert!(ops.iter().all(|op| matches!(op, Op::SetCellsIf { .. })));
    deliver(&mut engine, commit(&mut server, ALICE, undo));
    events(&mut engine);
    let _ = engine.handle_command(EngineCommand::Redo);
    assert_eq!(value(&engine, 0), Value::Number(77.0));
    assert_eq!(value(&engine, 1), Value::Number(21.0));
    let redo = pending(sent(events(&mut engine)));
    deliver(&mut engine, commit(&mut server, ALICE, redo));
    events(&mut engine);

    raw(&mut engine, 0, "99");
    let denied_seq = pending(sent(events(&mut engine))).client_seq;
    receive(
        &mut engine,
        &WireSyncFrameV2::Reject {
            client_seq: denied_seq,
            reason: WireRejectReasonV1::QuotaExceeded,
        },
    );
    let rejection = events(&mut engine);
    let [
        CollabEvent::SaveUnsynced {
            snapshot,
            pending_wal,
            ..
        },
        CollabEvent::ReplacePending { .. },
        ..,
    ] = rejection.as_slice()
    else {
        panic!("rescue must precede pending WAL rewrite")
    };
    let archived = decode_snapshot_any(snapshot).unwrap().engine;
    assert_eq!(
        archived
            .workbook()
            .sheet(SheetId(0))
            .unwrap()
            .value_by_id(RowId(0), ColId(0)),
        Value::Number(99.0)
    );
    assert_eq!(
        scan_pending_wal_any(pending_wal).unwrap().records[0].client_seq,
        denied_seq
    );
    assert_eq!(value(&engine, 0), Value::Number(77.0));
    assert_eq!(engine.undo_stack.undo_len(), 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn remote_presence_is_resolved_again_after_a_structural_commit() {
    let (mut engine, mut server) = connect();
    let bob = server.allocate_replica(BOB).unwrap();
    receive(
        &mut engine,
        &WireSyncFrameV2::Presence(WirePresenceV1 {
            replica: bob,
            user_id: BOB,
            display_name: "Bob".into(),
            color_slot: 2,
            sheet_id: 0,
            cursor_row: 2,
            cursor_col: 1,
            selection: None,
        }),
    );
    assert!(events(&mut engine).iter().any(|event| matches!(event, CollabEvent::Presence { overlays, .. } if overlays[0].cursor_row == 2)));
    let id = pack_id(0, bob, 1).unwrap();
    let insert = Op::InsertRows {
        sheet: SheetId(0),
        after: None,
        ids: IdRun::new(id, 1).unwrap(),
    };
    let inserted = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 0,
            txn: ops_to_wire_txn_v3("Insert".into(), &[insert]).unwrap(),
        },
    );
    deliver(&mut engine, inserted);
    assert!(events(&mut engine).iter().any(|event| matches!(event, CollabEvent::Presence { overlays, .. } if overlays[0].cursor_row == 3 && overlays[0].cursor_col == 1)));
    let _ = engine.handle_sync_command(SyncCommand::Disconnected);
    assert!(events(&mut engine).iter().any(
        |event| matches!(event, CollabEvent::Presence { overlays, .. } if overlays.is_empty())
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn a_commit_resends_the_pages_its_text_may_overflow_into() {
    let (mut engine, mut server) = connect();
    let _ = engine.handle_command(EngineCommand::Subscribe {
        sheet: 0,
        epoch: 1,
        first_row: 0,
        row_count: 64,
        first_col: 0,
        col_count: 160,
        visible: None,
        resend: false,
    });
    assert_eq!(std::iter::from_fn(|| engine.take_page()).count(), 10);
    let bob = server.allocate_replica(BOB).unwrap();
    let edited = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 0,
            txn: ops_to_wire_txn_v3("Edit".into(), &[edit_op(3, 7.0)]).unwrap(),
        },
    );
    deliver(&mut engine, edited);
    let columns: Vec<u32> = std::iter::from_fn(|| engine.take_page())
        .map(|page| u32::from_le_bytes(page[12..16].try_into().unwrap()) / 16)
        .collect();
    assert_eq!(columns, [0, 1, 2, 3, 4]);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn local_cursor_follows_rows_inserted_by_someone_else() {
    let (mut engine, mut server) = connect();
    let span = |first_row, first_col| PresenceSpan {
        first_row,
        end_row: first_row + 2,
        first_col,
        end_col: first_col + 1,
    };
    let _ = engine.handle_sync_command(SyncCommand::LocalPresence {
        sheet: 0,
        cursor_row: 2,
        cursor_col: 1,
        selection: Some(span(2, 1)),
    });
    events(&mut engine);

    let bob = server.allocate_replica(BOB).unwrap();
    let insert = Op::InsertRows {
        sheet: SheetId(0),
        after: None,
        ids: IdRun::new(pack_id(0, bob, 1).unwrap(), 1).unwrap(),
    };
    let inserted = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 0,
            txn: ops_to_wire_txn_v3("Insert".into(), &[insert]).unwrap(),
        },
    );
    deliver(&mut engine, inserted);
    let moved: Vec<_> = events(&mut engine)
        .into_iter()
        .filter_map(|event| match event {
            CollabEvent::LocalSelectionMoved { sheet, from, to } => Some((sheet, from, to)),
            _ => None,
        })
        .collect();
    assert_eq!(
        moved,
        vec![(
            0,
            LocalCursor {
                row: 2,
                col: 1,
                selection: Some(span(2, 1)),
            },
            LocalCursor {
                row: 3,
                col: 1,
                selection: Some(span(3, 1)),
            },
        )]
    );

    // A change that moves nothing leaves the cursor alone.
    let edit = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 2,
            base_server_seq: 1,
            timestamp_ms: 0,
            txn: ops_to_wire_txn_v3("Edit".into(), &[edit_op(5, 1.0)]).unwrap(),
        },
    );
    deliver(&mut engine, edit);
    assert!(
        !events(&mut engine)
            .iter()
            .any(|event| matches!(event, CollabEvent::LocalSelectionMoved { .. }))
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn digest_mismatch_requests_a_full_catchup_without_dropping_pending_edits() {
    let (mut engine, mut server) = connect();
    raw(&mut engine, 0, "10");
    let committed = commit(&mut server, ALICE, pending(sent(events(&mut engine))));
    deliver(&mut engine, committed);
    events(&mut engine);
    raw(&mut engine, 1, "20");
    events(&mut engine);

    let mut wrong: WireStateDigestV1 = server.workbook().state_digest().into();
    wrong.content_lo ^= 1;
    receive(
        &mut engine,
        &WireSyncFrameV2::Digest {
            seq: 1,
            state_digest: wrong,
        },
    );
    let effects = events(&mut engine);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, CollabEvent::DigestMismatch { .. }))
    );
    assert!(effects.contains(&CollabEvent::Reconnect));

    let _ = engine.handle_sync_command(SyncCommand::Connected);
    let hello = sent(events(&mut engine));
    assert!(matches!(
        hello,
        WireSyncFrameV2::Hello {
            last_applied_seq: 0,
            ..
        }
    ));
    receive(
        &mut engine,
        &server
            .on_hello(ALICE, Some(MemberRole::Owner), &hello)
            .unwrap(),
    );
    assert_eq!(pending(sent(events(&mut engine))).client_seq, 2);
    assert_eq!(value(&engine, 0), Value::Number(10.0));
    assert_eq!(value(&engine, 1), Value::Number(20.0));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn incremental_reconciliation_with_pending_edits_recalculates_only_affected_formulas() {
    let (mut engine, mut server) = connect();
    // Base workbook: A1=10, B1==A1*2 (row 0), A3=5, B3==A3*3 (row 2).
    raw(&mut engine, 0, "10");
    assert!(!matches!(
        engine.handle_command(EngineCommand::EditCell {
            sheet: 0,
            row_id: 0,
            col_id: 1,
            raw: "=A1*2".into(),
        }),
        Some(EngineEvent::Rejected { .. })
    ));
    raw(&mut engine, 2, "5");
    assert!(!matches!(
        engine.handle_command(EngineCommand::EditCell {
            sheet: 0,
            row_id: 2,
            col_id: 1,
            raw: "=A3*3".into(),
        }),
        Some(EngineEvent::Rejected { .. })
    ));
    engine.engine.recalc_all();

    let mut pending_txns = Vec::new();
    for event in events(&mut engine) {
        if let CollabEvent::Send { bytes } = event
            && let Ok(WireSyncFrameV2::Push { txns }) = decode_sync_frame_v2(&bytes)
        {
            pending_txns.extend(txns);
        }
    }
    for txn in pending_txns {
        let committed = commit(&mut server, ALICE, txn);
        deliver(&mut engine, committed);
        events(&mut engine);
    }
    engine.engine.recalc_all();
    assert!(!engine.engine.has_pending_recalc());

    // Local pending edit changes A1 -> 25 (only B1 depends on A1; B3 does not).
    raw(&mut engine, 0, "25");
    engine.engine.recalc_all();
    events(&mut engine);
    assert!(!engine.engine.has_pending_recalc());

    // Remote commit from Bob touches A2 -> 7 while Alice still has pending A1=25.
    let bob = server.allocate_replica(BOB).unwrap();
    let remote = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 4,
            timestamp_ms: 20,
            txn: ops_to_wire_txn_v3("Bob A2".into(), &[edit_op(1, 7.0)]).unwrap(),
        },
    );
    deliver(&mut engine, remote);
    events(&mut engine);

    assert!(engine.collab.as_ref().unwrap().engine_inverses_valid);
    // Only B1 (1 formula) was dirtied by rewind+replay; B3 stayed clean!
    assert_eq!(engine.engine.recalc_all(), 1);
    assert_eq!(
        engine
            .engine
            .workbook()
            .sheet(SheetId(0))
            .unwrap()
            .value_by_id(RowId(0), ColId(1)),
        Value::Number(50.0)
    );
    assert_eq!(
        engine
            .engine
            .workbook()
            .sheet(SheetId(0))
            .unwrap()
            .value_by_id(RowId(2), ColId(1)),
        Value::Number(15.0)
    );
    assert_eq!(
        engine.engine.workbook().state_digest(),
        engine
            .collab
            .as_ref()
            .unwrap()
            .machine
            .visible()
            .state_digest()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn reopened_pending_wal_falls_back_cleanly_then_re_enables_incremental_reconcile() {
    let (mut engine, mut server) = connect();
    raw(&mut engine, 0, "10");
    let c1 = commit(&mut server, ALICE, pending(sent(events(&mut engine))));
    deliver(&mut engine, c1);
    events(&mut engine);

    let cp = checkpoint(&mut engine);
    raw(&mut engine, 0, "40");
    let out = events(&mut engine);
    let CollabEvent::AppendPending { bytes } = &out[0] else {
        panic!("expected AppendPending")
    };
    let mut wal = encode_pending_wal_init_v2();
    wal.extend_from_slice(bytes);

    let mut reopened = open(Some(cp.info), cp.snapshot, wal);
    assert!(!reopened.collab.as_ref().unwrap().engine_inverses_valid);
    let _ = reopened.handle_sync_command(SyncCommand::Connected);
    let hello = sent(events(&mut reopened));
    let welcome = server
        .on_hello(ALICE, Some(MemberRole::Owner), &hello)
        .unwrap();
    receive(&mut reopened, &welcome);
    let pushed = pending(sent(events(&mut reopened)));
    assert_eq!(reopened.collab.as_ref().unwrap().machine.pending_len(), 1);
    assert!(!reopened.collab.as_ref().unwrap().engine_inverses_valid);

    let bob = server.allocate_replica(BOB).unwrap();
    let bob_commit = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 1,
            timestamp_ms: 30,
            txn: ops_to_wire_txn_v3("Bob A2".into(), &[edit_op(1, 9.0)]).unwrap(),
        },
    );
    deliver(&mut reopened, bob_commit);
    events(&mut reopened);
    assert_eq!(value(&reopened, 0), Value::Number(40.0));
    assert_eq!(value(&reopened, 1), Value::Number(9.0));

    // Once the reopened pending entry commits, incremental inverses become valid again.
    let alice_commit = commit(&mut server, ALICE, pushed);
    deliver(&mut reopened, alice_commit);
    events(&mut reopened);
    assert_eq!(reopened.collab.as_ref().unwrap().machine.pending_len(), 0);
    assert!(reopened.collab.as_ref().unwrap().engine_inverses_valid);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn viewer_receives_commits_reports_read_only_rejects_edits_and_skips_presence() {
    let mut server = ServerDocMachine::new(
        DOC,
        MAIN,
        "/api/workbooks/test".into(),
        ServerLimits::default(),
    );
    let alice = server.allocate_replica(ALICE).unwrap();
    let c1 = commit(
        &mut server,
        ALICE,
        WirePendingTxnV3 {
            replica: alice,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 10,
            txn: ops_to_wire_txn_v3("Init".into(), &[edit_op(0, 12.0)]).unwrap(),
        },
    );

    let viewer_user = [3; 16];
    let mut engine = open(None, Vec::new(), Vec::new());
    let _ = engine.handle_sync_command(SyncCommand::Connected);
    let hello = sent(events(&mut engine));
    let welcome = server
        .on_hello(viewer_user, Some(MemberRole::Viewer), &hello)
        .unwrap();
    receive(&mut engine, &welcome);
    let evs = events(&mut engine);
    assert!(evs.iter().any(|e| matches!(
        e,
        CollabEvent::State {
            connection: SyncConnection::Online,
            replica: None,
            pending: 0,
            read_only: true,
        }
    )));
    assert_eq!(value(&engine, 0), Value::Number(12.0));

    let err = engine
        .execute_user_txn("Blocked".into(), &[edit_op(0, 99.0)])
        .unwrap_err();
    assert!(err.contains("read-only"));

    let _ = engine.handle_sync_command(SyncCommand::LocalPresence {
        sheet: 0,
        cursor_row: 0,
        cursor_col: 0,
        selection: None,
    });
    assert!(
        !events(&mut engine)
            .iter()
            .any(|e| matches!(e, CollabEvent::Send { .. }))
    );
    let _ = c1;
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn e16_collab_sort_sync_rebuilds_engine_and_checkpoints_permutes() {
    let (mut engine, mut server) = connect();
    engine
        .execute_user_txn(
            "Seed rows".into(),
            &[edit_op(0, 10.0), edit_op(1, 20.0), edit_op(2, 30.0)],
        )
        .unwrap();
    let init = commit(&mut server, ALICE, pending(sent(events(&mut engine))));
    deliver(&mut engine, init);
    events(&mut engine);

    // Local pending edit targeting row 0 (value 10), before Bob's sort commits.
    raw(&mut engine, 0, "99");
    let pushed = pending(sent(events(&mut engine)));
    assert_eq!(value(&engine, 0), Value::Number(99.0));

    let bob = server.allocate_replica(BOB).unwrap();
    let bob_sort = commit(
        &mut server,
        BOB,
        WirePendingTxnV3 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 1,
            timestamp_ms: 40,
            txn: ops_to_wire_txn_v3(
                "Sort".into(),
                &[Op::PermuteContent {
                    sheet: SheetId(0),
                    rows: vec![RowId(0), RowId(1), RowId(2)],
                    cols: vec![ColId(0)],
                    row_perm: vec![2, 0, 1],
                }],
            )
            .unwrap(),
        },
    );
    deliver(&mut engine, bob_sort);
    events(&mut engine);

    // Old row 0 moved to row 1 via E16 while still pending -> Engine must match Session.
    assert_eq!(value(&engine, 0), Value::Number(30.0));
    assert_eq!(value(&engine, 1), Value::Number(99.0));
    assert_eq!(value(&engine, 2), Value::Number(20.0));
    assert_eq!(
        engine.engine.workbook().state_digest(),
        engine
            .collab
            .as_ref()
            .unwrap()
            .machine
            .visible()
            .state_digest()
    );

    // Commit Alice's pending edit -> server E16 maps to row 1, checkpoint preserves permutes.
    deliver(&mut engine, commit(&mut server, ALICE, pushed));
    events(&mut engine);
    let cp = checkpoint(&mut engine);
    let reopened = open(Some(cp.info), cp.snapshot, encode_pending_wal_init_v2());
    assert_eq!(
        reopened
            .collab
            .as_ref()
            .unwrap()
            .machine
            .confirmed_permutes()
            .len(),
        1
    );
    assert_eq!(value(&reopened, 1), Value::Number(99.0));
}
