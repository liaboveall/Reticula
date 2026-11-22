use reticula_core::id::{ColId, RowId, SheetId, pack_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_server::docs::{DocRooms, RoomConnection};
use reticula_server::store::{BatchEntry, MAIN_BRANCH, Store, StoreError};
use reticula_sync::{MemberRole, ServerDocMachine, StagedTxn, TryApplyOutcome};
use reticula_wire::convert_v2::ops_to_wire_txn_v2;
use reticula_wire::sync_frame::decode_sync_frame;
use reticula_wire::v2::{SYNC_PROTOCOL_VERSION, WirePendingTxnV2, WireSyncFrameV1};
use sqlx::PgPool;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

fn pending_edit(replica: u32, client_seq: u64, base_server_seq: u64) -> WirePendingTxnV2 {
    let txn = ops_to_wire_txn_v2(
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
    WirePendingTxnV2 {
        replica,
        client_seq,
        base_server_seq,
        timestamp_ms: 0,
        txn,
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

async fn read_frame(connection: &mut RoomConnection) -> Option<WireSyncFrameV1> {
    timeout(Duration::from_secs(5), connection.recv())
        .await
        .unwrap()
        .map(|bytes| decode_sync_frame(&bytes).unwrap())
}

fn hello(doc: Uuid, replica: Option<u32>, last_applied_seq: u64) -> WireSyncFrameV1 {
    WireSyncFrameV1::Hello {
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
        Some(WireSyncFrameV1::Welcome { replica: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut observer).await,
        Some(WireSyncFrameV1::Welcome { replica: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut independent).await,
        Some(WireSyncFrameV1::Welcome { .. })
    ));

    sender
        .push(vec![pending_edit(1, 1, 0), pending_edit(1, 2, 0)], 128)
        .await
        .unwrap();
    let first_ack = read_frame(&mut sender).await.unwrap();
    assert!(matches!(
        first_ack,
        WireSyncFrameV1::Ack {
            seq: 1,
            client_seq: 1,
            ..
        }
    ));
    assert_eq!(store.load_machine(doc).await.unwrap().head_seq(), 2);
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV1::Commit { seq: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV1::Ack { seq: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut sender).await,
        Some(WireSyncFrameV1::Commit { seq: 2, .. })
    ));
    assert!(matches!(
        read_frame(&mut observer).await,
        Some(WireSyncFrameV1::Commit { seq: 1, .. })
    ));
    assert!(matches!(
        read_frame(&mut observer).await,
        Some(WireSyncFrameV1::Commit { seq: 2, .. })
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
        Some(WireSyncFrameV1::Ack { seq: 1, .. })
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
        Some(WireSyncFrameV1::Welcome {
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
