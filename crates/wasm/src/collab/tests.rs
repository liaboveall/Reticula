use reticula_core::id::{ColId, IdRun, RowId, pack_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, ScalarValue};
use reticula_core::value::Value;
use reticula_sync::{ApplyResult, MemberRole, ServerDocMachine, ServerLimits};
use reticula_wire::v2::WireRejectReasonV1;
use reticula_wire::v2::WireStateDigestV1;
use reticula_wire::wal::encode_pending_wal_init;

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

fn receive(engine: &mut Engine, frame: &WireSyncFrameV1) {
    let _ = engine.handle_sync_command(SyncCommand::Receive {
        frame: encode_sync_frame(frame).unwrap(),
    });
}

fn events(engine: &mut Engine) -> Vec<CollabEvent> {
    std::iter::from_fn(|| engine.pop_collab_event()).collect()
}

fn sent(events: Vec<CollabEvent>) -> WireSyncFrameV1 {
    events
        .into_iter()
        .find_map(|event| match event {
            CollabEvent::Send { bytes } => Some(decode_sync_frame(&bytes).unwrap()),
            _ => None,
        })
        .expect("expected an outbound frame")
}

fn pending(frame: WireSyncFrameV1) -> WirePendingTxnV2 {
    let WireSyncFrameV1::Push { mut txns } = frame else {
        panic!("expected Push")
    };
    assert_eq!(txns.len(), 1);
    txns.pop().unwrap()
}

fn commit(
    server: &mut ServerDocMachine,
    user: [u8; 16],
    txn: WirePendingTxnV2,
) -> WireCommittedTxnV2 {
    let accepted = server
        .apply_and_commit(user, Some(MemberRole::Editor), txn.replica, txn, 12)
        .unwrap();
    let ApplyResult::Committed(commit) = accepted else {
        panic!("expected a fresh commit")
    };
    commit
}

fn deliver(engine: &mut Engine, commit: WireCommittedTxnV2) {
    receive(
        engine,
        &WireSyncFrameV1::Commit {
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
    let mut original_pending = encode_pending_wal_init();
    original_pending.extend_from_slice(bytes);
    let txn = pending(sent(output));
    let commit = commit(&mut server, ALICE, txn);
    receive(
        &mut engine,
        &WireSyncFrameV1::Ack {
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
        WirePendingTxnV2 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 1,
            timestamp_ms: 11,
            txn: ops_to_wire_txn_v2("Bob edited A1".into(), &[edit_op(0, 77.0)]),
        },
    );
    deliver(&mut engine, overwrite);
    events(&mut engine);

    let _ = engine.handle_command(EngineCommand::Undo);
    assert_eq!(value(&engine, 0), Value::Number(77.0));
    assert_eq!(value(&engine, 1), Value::Empty);
    let undo = pending(sent(events(&mut engine)));
    let (_, ops) = ops_from_wire_txn_v2(undo.txn.clone()).unwrap();
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
        &WireSyncFrameV1::Reject {
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
        scan_pending_wal(pending_wal).unwrap().records[0].client_seq,
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
        &WireSyncFrameV1::Presence(WirePresenceV1 {
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
        WirePendingTxnV2 {
            replica: bob,
            client_seq: 1,
            base_server_seq: 0,
            timestamp_ms: 0,
            txn: ops_to_wire_txn_v2("Insert".into(), &[insert]),
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
        &WireSyncFrameV1::Digest {
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
        WireSyncFrameV1::Hello {
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
