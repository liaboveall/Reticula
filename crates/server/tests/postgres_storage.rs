use reticula_core::id::{ColId, RowId, SheetId, pack_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_core::value::Value;
use reticula_server::docs::{DocRooms, RoomConnection, RoomError};
use reticula_server::store::{BatchEntry, MAIN_BRANCH, Store, StoreError};
use reticula_sync::{MemberRole, ServerDocMachine, StagedTxn, TryApplyOutcome};
use reticula_wire::convert_v3::ops_to_wire_txn_v3;
use reticula_wire::sync_frame::{decode_sync_frame, decode_sync_frame_v2};
use reticula_wire::v2::{SYNC_PROTOCOL_VERSION, WireRejectReasonV1, WireSyncFrameV1};
use reticula_wire::v3::{SYNC_PROTOCOL_VERSION_V2, WirePendingTxnV3, WireSyncFrameV2};
use sqlx::PgPool;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

fn pending_edit(replica: u32, client_seq: u64, base_server_seq: u64) -> WirePendingTxnV3 {
    let txn = ops_to_wire_txn_v3(
        format!("Edit {client_seq}"),
        &[Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(pack_id(0, 0, 1).unwrap()),
                col: ColId(pack_id(0, 0, 1).unwrap()),
                input: CellInput::Scalar(ScalarValue::Number(42.0 + f64::from(replica))),
                format: FormatPatch::Keep,
            }],
        }],
    )
    .unwrap();
    WirePendingTxnV3 {
        replica,
        client_seq,
        base_server_seq,
        timestamp_ms: 0,
        txn,
    }
}

fn pending_ops_txn(
    replica: u32,
    client_seq: u64,
    base_server_seq: u64,
    label: &str,
    ops: &[Op<reticula_formula::binder::BoundFormula>],
) -> WirePendingTxnV3 {
    WirePendingTxnV3 {
        replica,
        client_seq,
        base_server_seq,
        timestamp_ms: 0,
        txn: ops_to_wire_txn_v3(label.to_owned(), ops).unwrap(),
    }
}

fn stage(machine: &mut ServerDocMachine, user: Uuid, replica: u32, client_seq: u64) -> StagedTxn {
    let input = pending_edit(replica, client_seq, 0);
    match machine
        .validate_and_try_apply(
            user.into_bytes(),
            Some(MemberRole::Owner),
            replica,
            input,
            1_795_000_000_123,
        )
        .unwrap()
    {
        TryApplyOutcome::Staged(staged) => staged,
        outcome => panic!("transaction was not staged: {outcome:?}"),
    }
}

async fn read_frame(connection: &mut RoomConnection) -> Option<WireSyncFrameV2> {
    timeout(Duration::from_secs(5), connection.recv())
        .await
        .unwrap()
        .map(|bytes| decode_sync_frame_v2(&bytes).unwrap())
}

async fn read_v1_frame(connection: &mut RoomConnection) -> Option<WireSyncFrameV1> {
    timeout(Duration::from_secs(5), connection.recv())
        .await
        .unwrap()
        .map(|bytes| decode_sync_frame(&bytes).unwrap())
}

fn hello(doc: Uuid, replica: Option<u32>, last_applied_seq: u64) -> WireSyncFrameV2 {
    WireSyncFrameV2::Hello {
        protocol_version: SYNC_PROTOCOL_VERSION_V2,
        doc_id: doc.into_bytes(),
        branch_id: MAIN_BRANCH.into_bytes(),
        replica,
        last_applied_seq,
        known_epoch: 0,
    }
}

fn hello_v1(doc: Uuid, replica: Option<u32>, last_applied_seq: u64) -> WireSyncFrameV2 {
    WireSyncFrameV2::Hello {
        protocol_version: SYNC_PROTOCOL_VERSION,
        doc_id: doc.into_bytes(),
        branch_id: MAIN_BRANCH.into_bytes(),
        replica,
        last_applied_seq,
        known_epoch: 0,
    }
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn actor_acks_committed_rows_and_isolates_a_failed_room() {
    let url = std::env::var("RETICULA_TEST_DATABASE_URL").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::connect(&url, dir.path().to_path_buf())
        .await
        .unwrap();
    let db = PgPool::connect(&url).await.unwrap();
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, name, avatar_url) VALUES ($1, 'Owner', '')")
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
    let doc = store.create_workbook(owner, "Shared sheet").await.unwrap();
    let independent_doc = store.create_workbook(owner, "Other sheet").await.unwrap();
    let registry = DocRooms::new(store.clone());
    let mut sender = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Owner".into(),
            hello(doc, Some(1), 0),
        )
        .await
        .unwrap();
    let mut observer = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Owner".into(),
            hello(doc, None, 0),
        )
        .await
        .unwrap();
    let mut independent = registry
        .join(
            independent_doc,
            owner,
            MemberRole::Owner,
            "Owner".into(),
            hello(independent_doc, Some(1), 0),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV2::Welcome { replica: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut observer).await,
        Some(WireSyncFrameV2::Welcome { replica: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut independent).await,
        Some(WireSyncFrameV2::Welcome { .. })
    ));

    sender
        .push(vec![pending_edit(1, 1, 0), pending_edit(1, 2, 0)], 128)
        .await
        .unwrap();
    let first_ack = read_frame(&mut sender).await.unwrap();
    assert!(matches!(
        first_ack,
        WireSyncFrameV2::Ack {
            seq: 1,
            client_seq: 1,
            ..
        }
    ));
    assert_eq!(store.load_machine(doc).await.unwrap().head_seq(), 2);
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV2::Commit { seq: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV2::Ack { seq: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV2::Commit { seq: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut observer).await,
        Some(WireSyncFrameV2::Commit { seq: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut observer).await,
        Some(WireSyncFrameV2::Commit { seq: 2, .. })
    ));

    let mut other_writer = store.load_machine(doc).await.unwrap();
    store.write_snapshot(&other_writer).await.unwrap();
    let competing = stage(&mut other_writer, owner, 2, 1);
    store
        .commit_batch(
            doc,
            0,
            &[BatchEntry {
                staged: &competing,
                base_seq: 2,
            }],
            other_writer.workbook().estimated_bytes(),
        )
        .await
        .unwrap();
    sender.push(vec![pending_edit(1, 3, 2)], 64).await.unwrap();
    assert_eq!(read_frame(&mut sender).await, None);
    assert_eq!(read_frame(&mut observer).await, None);

    independent
        .push(vec![pending_edit(1, 1, 0)], 64)
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut independent).await,
        Some(WireSyncFrameV2::Ack { seq: 1, .. })
    ));
    let mut reconnected = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Owner".into(),
            hello(doc, Some(1), 3),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut reconnected).await,
        Some(WireSyncFrameV2::Welcome {
            head_seq: 3,
            last_client_seq: 2,
            ..
        })
    ));
    reconnected
        .push(vec![pending_edit(1, 1, 0)], 64)
        .await
        .unwrap();
    assert_eq!(read_frame(&mut reconnected).await, Some(first_ack));
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn commits_multiple_replicas_atomically_and_recovers_snapshot_plus_log() {
    let url = std::env::var("RETICULA_TEST_DATABASE_URL").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::connect(&url, dir.path().to_path_buf())
        .await
        .unwrap();
    let db = PgPool::connect(&url).await.unwrap();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, avatar_url) VALUES ($1, 'Alice', ''), ($2, 'Bob', '')",
    )
    .bind(alice)
    .bind(bob)
    .execute(&db)
    .await
    .unwrap();
    let doc = store
        .create_workbook(alice, "Transactional sheet")
        .await
        .unwrap();
    let mut machine = store.load_machine(doc).await.unwrap();
    assert_eq!(machine.head_seq(), 0);
    assert_eq!(machine.next_per_epoch_replica(), 2);
    let base = store.write_snapshot(&machine).await.unwrap();
    assert_eq!(base.seq(), 0);
    assert_eq!(base, store.write_snapshot(&machine).await.unwrap());

    let bob_replica = machine.allocate_replica(bob.into_bytes()).unwrap();
    assert_eq!(bob_replica, 2);
    store.register_replica(doc, bob, bob_replica).await.unwrap();
    assert!(matches!(
        store.register_replica(doc, bob, bob_replica).await,
        Err(StoreError::StaleActor)
    ));

    let first = stage(&mut machine, alice, 1, 1);
    let second = stage(&mut machine, bob, bob_replica, 1);
    let mut wrong_watermark = second.clone();
    wrong_watermark.committed.client_seq = 2;
    let invalid = [
        BatchEntry {
            staged: &first,
            base_seq: 0,
        },
        BatchEntry {
            staged: &wrong_watermark,
            base_seq: 0,
        },
    ];
    let est = machine.workbook().estimated_bytes();
    assert!(matches!(
        store.commit_batch(doc, 0, &invalid, est).await,
        Err(StoreError::StaleActor)
    ));
    let rolled_back = store.load_machine(doc).await.unwrap();
    assert_eq!(rolled_back.head_seq(), 0);
    assert!(
        rolled_back
            .watermarks()
            .iter()
            .all(|watermark| watermark.last_client_seq == 0)
    );

    let valid = [
        BatchEntry {
            staged: &first,
            base_seq: 0,
        },
        BatchEntry {
            staged: &second,
            base_seq: 0,
        },
    ];
    store.commit_batch(doc, 0, &valid, est).await.unwrap();
    assert!(matches!(
        store.commit_batch(doc, 0, &valid, est).await,
        Err(StoreError::StaleActor)
    ));
    machine.commit_staged(first);
    machine.commit_staged(second);
    let snapshot = store.write_snapshot(&machine).await.unwrap();
    assert_eq!(snapshot.seq(), 2);
    assert_eq!(
        store.snapshot(doc, 2).await.unwrap(),
        Some(snapshot.clone())
    );
    assert_eq!(
        snapshot
            .open(store.data_dir())
            .unwrap()
            .metadata()
            .unwrap()
            .len(),
        snapshot.size()
    );

    let third = stage(&mut machine, alice, 1, 2);
    store
        .commit_batch(
            doc,
            0,
            &[BatchEntry {
                staged: &third,
                base_seq: 2,
            }],
            machine.workbook().estimated_bytes(),
        )
        .await
        .unwrap();
    let third = machine.commit_staged(third);
    let restored = store.load_machine(doc).await.unwrap();
    assert_eq!(restored.head_seq(), 3);
    assert_eq!(restored.last_snapshot_seq(), 2);
    assert_eq!(restored.state_digest(), machine.state_digest());
    assert_eq!(restored.watermarks(), machine.watermarks());
    assert_eq!(restored.txns_since(2).unwrap(), vec![third]);
    let base_seq: i64 =
        sqlx::query_scalar("SELECT base_seq FROM txns WHERE workbook_id = $1 AND seq = 3")
            .bind(doc)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(base_seq, 2);

    let path = dir
        .path()
        .join("snapshots")
        .join(doc.to_string())
        .join("2.rtclsnap");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(matches!(
        store.load_machine(doc).await,
        Err(StoreError::Invalid("snapshot checksum or length"))
    ));
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn e16_permute_survives_snapshot_and_cold_restart() {
    let url = std::env::var("RETICULA_TEST_DATABASE_URL").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::connect(&url, dir.path().to_path_buf())
        .await
        .unwrap();
    let db = PgPool::connect(&url).await.unwrap();
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, avatar_url) VALUES ($1, 'Alice', ''), ($2, 'Bob', '')",
    )
    .bind(alice)
    .bind(bob)
    .execute(&db)
    .await
    .unwrap();
    let doc = store
        .create_workbook(alice, "Sort restart sheet")
        .await
        .unwrap();
    let mut machine = store.load_machine(doc).await.unwrap();
    let bob_replica = machine.allocate_replica(bob.into_bytes()).unwrap();
    assert_eq!(bob_replica, 2);
    store.register_replica(doc, bob, bob_replica).await.unwrap();

    let rows = vec![RowId(1), RowId(2), RowId(3)];
    let cols = vec![ColId(1)];
    let init_txn = pending_ops_txn(
        1,
        1,
        0,
        "Seed rows",
        &[Op::SetCells {
            sheet: SheetId(0),
            edits: vec![
                CellEdit {
                    row: RowId(1),
                    col: ColId(1),
                    input: CellInput::Scalar(ScalarValue::Number(10.0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(2),
                    col: ColId(1),
                    input: CellInput::Scalar(ScalarValue::Number(20.0)),
                    format: FormatPatch::Keep,
                },
                CellEdit {
                    row: RowId(3),
                    col: ColId(1),
                    input: CellInput::Scalar(ScalarValue::Number(30.0)),
                    format: FormatPatch::Keep,
                },
            ],
        }],
    );
    let TryApplyOutcome::Staged(s1) = machine
        .validate_and_try_apply(
            alice.into_bytes(),
            Some(MemberRole::Owner),
            1,
            init_txn,
            100,
        )
        .unwrap()
    else {
        panic!("expected Staged s1");
    };
    let sort_txn = pending_ops_txn(
        1,
        2,
        1,
        "Permute",
        &[Op::PermuteContent {
            sheet: SheetId(0),
            rows,
            cols,
            row_perm: vec![2, 0, 1],
        }],
    );
    let TryApplyOutcome::Staged(s2) = machine
        .validate_and_try_apply(
            alice.into_bytes(),
            Some(MemberRole::Owner),
            1,
            sort_txn,
            200,
        )
        .unwrap()
    else {
        panic!("expected Staged s2");
    };
    store
        .commit_batch(
            doc,
            0,
            &[
                BatchEntry {
                    staged: &s1,
                    base_seq: 0,
                },
                BatchEntry {
                    staged: &s2,
                    base_seq: 1,
                },
            ],
            machine.workbook().estimated_bytes(),
        )
        .await
        .unwrap();
    machine.commit_staged(s1);
    machine.commit_staged(s2);
    let snap = store.write_snapshot(&machine).await.unwrap();
    assert_eq!(snap.seq(), 2);

    // Cold restart from snapshot with SECTION_PERMUTE_LOG_V1 (no WAL tail needed).
    let mut cold = store.load_machine(doc).await.unwrap();
    assert_eq!(cold.committed_permutes().len(), 1);

    // Late offline Bob (base_server_seq = 1 < permute seq 2) targets old RowId(1) (value 10).
    let late_bob = pending_ops_txn(
        bob_replica,
        1,
        1,
        "Offline edit on original A1",
        &[Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(1),
                col: ColId(1),
                input: CellInput::Scalar(ScalarValue::Number(99.0)),
                format: FormatPatch::Keep,
            }],
        }],
    );
    let TryApplyOutcome::Staged(s3) = cold
        .validate_and_try_apply(
            bob.into_bytes(),
            Some(MemberRole::Editor),
            bob_replica,
            late_bob,
            300,
        )
        .unwrap()
    else {
        panic!("expected Staged s3");
    };
    store
        .commit_batch(
            doc,
            0,
            &[BatchEntry {
                staged: &s3,
                base_seq: 1,
            }],
            cold.workbook().estimated_bytes(),
        )
        .await
        .unwrap();
    cold.commit_staged(s3);

    let final_machine = store.load_machine(doc).await.unwrap();
    let sheet = final_machine.workbook().sheet(SheetId(0)).unwrap();
    // perm[0] = 2 -> old RowId(3)=30 moved to RowId(1)
    // perm[1] = 0 -> old RowId(1)=10 moved to RowId(2) -> Bob's edit lands here!
    assert_eq!(sheet.value_by_id(RowId(1), ColId(1)), Value::Number(30.0));
    assert_eq!(sheet.value_by_id(RowId(2), ColId(1)), Value::Number(99.0));
    assert_eq!(sheet.value_by_id(RowId(3), ColId(1)), Value::Number(20.0));
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
async fn legacy_v1_client_receives_v1_commits_and_upgrade_required_on_v3_op() {
    let url = std::env::var("RETICULA_TEST_DATABASE_URL").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::connect(&url, dir.path().to_path_buf())
        .await
        .unwrap();
    let db = PgPool::connect(&url).await.unwrap();
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, name, avatar_url) VALUES ($1, 'Owner', '')")
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
    let doc = store.create_workbook(owner, "Mixed v1/v2").await.unwrap();
    let registry = DocRooms::new(store.clone());

    let mut v2_conn = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Alice v2".into(),
            hello(doc, Some(1), 0),
        )
        .await
        .unwrap();
    let mut v1_conn = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Alice v1".into(),
            hello_v1(doc, None, 0),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut v2_conn).await,
        Some(WireSyncFrameV2::Welcome { replica: 1, .. })
    ));
    assert!(matches!(
        read_v1_frame(&mut v1_conn).await,
        Some(WireSyncFrameV1::Welcome { replica: 2, .. })
    ));

    // Commit a v1-compatible SetCells op: both v2 and v1 receive it cleanly.
    v2_conn
        .push(vec![pending_edit(1, 1, 0)], 128)
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut v2_conn).await,
        Some(WireSyncFrameV2::Ack { seq: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut v2_conn).await,
        Some(WireSyncFrameV2::Commit { seq: 1, .. })
    ));
    assert!(matches!(
        read_v1_frame(&mut v1_conn).await,
        Some(WireSyncFrameV1::Commit { seq: 1, .. })
    ));

    // Commit a v3-only PermuteContent op: v2 succeeds, v1 gets UpgradeRequired & disconnects.
    let permute_txn = pending_ops_txn(
        1,
        2,
        1,
        "Sort",
        &[Op::PermuteContent {
            sheet: SheetId(0),
            rows: vec![RowId(1), RowId(2)],
            cols: vec![ColId(1)],
            row_perm: vec![1, 0],
        }],
    );
    v2_conn.push(vec![permute_txn], 128).await.unwrap();
    assert!(matches!(
        read_frame(&mut v2_conn).await,
        Some(WireSyncFrameV2::Ack { seq: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut v2_conn).await,
        Some(WireSyncFrameV2::Commit { seq: 2, .. })
    ));
    assert_eq!(
        read_v1_frame(&mut v1_conn).await,
        Some(WireSyncFrameV1::Reject {
            client_seq: 0,
            reason: WireRejectReasonV1::UpgradeRequired,
        })
    );
    assert_eq!(read_v1_frame(&mut v1_conn).await, None);

    // Reconnecting as v1 when inline catchup crosses PermuteContent rejects with UpgradeRequired.
    let err = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Alice v1".into(),
            hello_v1(doc, Some(2), 1),
        )
        .await;
    assert!(matches!(
        err,
        Err(RoomError::Rejected(WireRejectReasonV1::UpgradeRequired))
    ));

    // Upgrading to v2 catches up across seq 2 cleanly.
    let mut upgraded = registry
        .join(
            doc,
            owner,
            MemberRole::Owner,
            "Alice v2".into(),
            hello(doc, Some(2), 1),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut upgraded).await,
        Some(WireSyncFrameV2::Welcome { head_seq: 2, .. })
    ));
}
