//! `#![no_std]` collaborative synchronization state machines (`ClientSyncMachine`
//! and `ServerDocMachine`, `06-sync.md` & `15-m2a.md` 5.3).
//!
//! Pure input/output transition functions with zero I/O, clock, network, or
//! randomness dependencies so the exact same kernel runs in Tokio services,
//! WebAssembly workers, and the deterministic single-thread simulator.

#![no_std]
// --- kernel lint block: identical in every kernel crate, verified by `cargo xtask check` ---
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::exit,
    clippy::expect_used,
    clippy::get_unwrap,
    clippy::indexing_slicing,
    clippy::iter_over_hash_type,
    clippy::panic,
    clippy::string_slice,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]
// --- end kernel lint block ---

extern crate alloc;

pub mod client;
pub mod server;
pub mod types;
pub mod validate;

pub use client::{ClientSyncMachine, replay_pending_onto};
pub use server::{ApplyResult, ServerDocMachine};
pub use types::{
    ClientAction, CommittedPermute, ConnectionState, MemberRole, ReplicaWatermark, ServerLimits,
    StagedTxn, TryApplyOutcome, VIEWER_REPLICA,
};
pub use validate::transform_ops_through_permute;

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id};
    use reticula_core::ops::{CellEdit, CellInput, FormatPatch, FormulaMode, Op, ScalarValue};
    use reticula_core::value::Value;
    use reticula_formula::binder::BoundFormula;
    use reticula_wire::convert_v3::ops_to_wire_txn_v3;
    use reticula_wire::v2::{WireRejectReasonV1, WireStateDigestV1};
    use reticula_wire::v3::{
        SYNC_PROTOCOL_VERSION_V2, WireCatchupV2, WireCommittedTxnV3, WirePendingTxnV3,
        WireSyncFrameV2,
    };

    use crate::{
        ClientAction, ClientSyncMachine, ConnectionState, MemberRole, ReplicaWatermark,
        ServerDocMachine, ServerLimits, VIEWER_REPLICA, replay_pending_onto,
        transform_ops_through_permute,
    };

    const DOC_ID: [u8; 16] = [1; 16];
    const BRANCH_ID: [u8; 16] = [0; 16];
    const USER_A: [u8; 16] = [0xAA; 16];
    const USER_B: [u8; 16] = [0xBB; 16];

    fn connect_client(
        server: &mut ServerDocMachine,
        client: &mut ClientSyncMachine,
        user_id: [u8; 16],
        role: Option<MemberRole>,
    ) {
        let out = client.on_connected();
        let ClientAction::SendFrame(hello) = &out[0] else {
            panic!("expected Hello");
        };
        let welcome = server.on_hello(user_id, role, hello).unwrap();
        let welcome_actions = client.on_welcome(welcome);
        if let Some(ClientAction::RequestSnapshot { snapshot_seq, .. }) = welcome_actions
            .iter()
            .find(|a| matches!(a, ClientAction::RequestSnapshot { .. }))
        {
            let tail = server.txns_since(*snapshot_seq).unwrap_or_default();
            // Simulate exact snapshot at `snapshot_seq` via empty base replay when tests
            // cover explicit snapshot installs directly.
            let _ = tail;
        }
    }

    fn commit_ok(
        server: &mut ServerDocMachine,
        user_id: [u8; 16],
        role: Option<MemberRole>,
        rep: u32,
        txn: WirePendingTxnV3,
        ts: u64,
    ) -> WireCommittedTxnV3 {
        let crate::server::ApplyResult::Committed(c) = server
            .apply_and_commit(user_id, role, rep, txn, ts)
            .unwrap()
        else {
            panic!("expected committed transaction");
        };
        c
    }

    fn set_num(row: u32, col: u32, val: f64) -> Op<BoundFormula> {
        Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(u64::from(row)),
                col: ColId(u64::from(col)),
                input: CellInput::Scalar(ScalarValue::Number(val)),
                format: FormatPatch::Keep,
            }],
        }
    }

    #[test]
    fn slow_path_rewind_replay_converges_and_preserves_concurrent_edits() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/workbooks/1"),
            ServerLimits::default(),
        );
        let mut ca = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let mut cb = ClientSyncMachine::new(DOC_ID, BRANCH_ID);

        connect_client(&mut server, &mut ca, USER_A, Some(MemberRole::Owner));
        connect_client(&mut server, &mut cb, USER_B, Some(MemberRole::Editor));
        assert_eq!(ca.connection_state(), ConnectionState::Online);
        assert_eq!(cb.connection_state(), ConnectionState::Online);

        // Both edit offline / concurrently before receiving each other's commits.
        ca.on_disconnected();
        cb.on_disconnected();

        let r_ins = ca.alloc_row_run(1).unwrap();
        ca.push_local(
            String::from("A inserts row"),
            vec![
                Op::InsertRows {
                    sheet: SheetId(0),
                    after: None,
                    ids: r_ins,
                },
                Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(r_ins.first_id),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(11.0)),
                        format: FormatPatch::Keep,
                    }],
                },
            ],
            100,
        )
        .unwrap();

        cb.push_local(
            String::from("B writes row 0"),
            vec![Op::SetCells {
                sheet: SheetId(0),
                edits: vec![CellEdit {
                    row: RowId(0),
                    col: ColId(1),
                    input: CellInput::Scalar(ScalarValue::Number(22.0)),
                    format: FormatPatch::Keep,
                }],
            }],
            101,
        )
        .unwrap();

        // Client B connects first and commits at seq 1.
        let hello_b = cb.on_connected();
        let ClientAction::SendFrame(hb) = &hello_b[0] else {
            panic!();
        };
        let wb_welcome = server
            .on_hello(USER_B, Some(MemberRole::Editor), hb)
            .unwrap();
        let b_actions = cb.on_welcome(wb_welcome);
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: b_txns }) =
            b_actions.last().unwrap()
        else {
            panic!("expected Push from B");
        };
        let c1 = commit_ok(
            &mut server,
            USER_B,
            Some(MemberRole::Editor),
            cb.replica().unwrap(),
            b_txns[0].clone(),
            1000,
        );

        // Deliver c1 to B out-of-order AFTER Ack:
        cb.on_ack(c1.replica, c1.client_seq);
        cb.on_commit(c1.clone());
        assert_eq!(cb.pending_len(), 0);

        // Now A connects, receives catchup c1, rewinds A's edit -> applies c1 -> replays A's edit!
        let hello_a = ca.on_connected();
        let ClientAction::SendFrame(ha) = &hello_a[0] else {
            panic!();
        };
        let wa_welcome = server
            .on_hello(USER_A, Some(MemberRole::Owner), ha)
            .unwrap();
        let a_actions = ca.on_welcome(wa_welcome);
        assert!(a_actions.contains(&ClientAction::NotifyVisibleChanged));
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: a_txns }) =
            a_actions.last().unwrap()
        else {
            panic!("expected Push from A");
        };

        // Also test idempotent resend when A retries Push twice.
        let c2 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            ca.replica().unwrap(),
            a_txns[0].clone(),
            1001,
        );
        assert!(matches!(
            server.apply_and_commit(
                USER_A,
                Some(MemberRole::Owner),
                ca.replica().unwrap(),
                a_txns[0].clone(),
                9999,
            ),
            Ok(crate::server::ApplyResult::Duplicate {
                seq: 2,
                timestamp_ms: 1001,
                ..
            })
        ));

        ca.on_commit(c2.clone());
        cb.on_commit(c2);
        assert_eq!(
            ca.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            cb.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            ca.visible().logical_digest(),
            server.workbook().logical_digest()
        );
        assert_eq!(
            cb.visible().logical_digest(),
            server.workbook().logical_digest()
        );
        assert_eq!(
            ca.visible()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(r_ins.first_id), ColId(0)),
            Value::Number(11.0)
        );
        assert_eq!(
            ca.visible()
                .sheet(SheetId(0))
                .unwrap()
                .value_by_id(RowId(0), ColId(1)),
            Value::Number(22.0)
        );
    }

    #[test]
    fn opfs_tail_loss_with_offline_edit_collision_aligns_watermarks_without_losing_edit() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        let mut client = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut client, USER_A, Some(MemberRole::Owner));
        let rep = client.replica().unwrap();

        // Commit client_seq = 1 (with InsertRows counter 1).
        let r1 = client.alloc_row_run(1).unwrap();
        let a1 = client
            .push_local(
                String::from("r1"),
                vec![Op::InsertRows {
                    sheet: SheetId(0),
                    after: None,
                    ids: r1,
                }],
                10,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: t1 }) = &a1[2] else {
            panic!();
        };
        let c1 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            rep,
            t1[0].clone(),
            10,
        );
        client.on_commit(c1.clone());

        // Commit client_seq = 2 on server too (using counter 2 + collaborative SheetId counter 1),
        // then simulate crash losing local OPFS tail for seq = 2!
        let r2 = client.alloc_row_run(1).unwrap();
        let s_local = client.alloc_sheet_id().unwrap();
        let a2 = client
            .push_local(
                String::from("r2+sheet"),
                vec![
                    Op::InsertRows {
                        sheet: SheetId(0),
                        after: None,
                        ids: r2,
                    },
                    Op::AddSheet {
                        sheet: s_local,
                        name: String::from("AddedBeforeCrash"),
                        after: Some(SheetId(0)),
                    },
                ],
                20,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: t2 }) = &a2[2] else {
            panic!();
        };
        let _c2 = server
            .apply_and_commit(USER_A, Some(MemberRole::Owner), rep, t2[0].clone(), 20)
            .unwrap();

        // Reload client from truncated state (only knows confirmed_seq=1, client_seq=1, id=1!).
        let mut crashed = ClientSyncMachine::from_persisted(
            DOC_ID,
            BRANCH_ID,
            0,
            Some(rep),
            1,
            1,
            client.confirmed().clone(),
            vec![],
            vec![],
            2,
        );
        // Before reconnecting, offline user creates an edit + allocates row run + sheet ID,
        // which naively reuse client_seq=2, id=2, sheet_counter=1!
        let r_offline = crashed.alloc_row_run(1).unwrap();
        let s_offline = crashed.alloc_sheet_id().unwrap();
        assert_eq!(r_offline, r2);
        assert_eq!(s_offline, s_local);
        crashed
            .push_local(
                String::from("Offline after tail loss"),
                vec![
                    Op::InsertRows {
                        sheet: SheetId(0),
                        after: None,
                        ids: r_offline,
                    },
                    Op::AddSheet {
                        sheet: s_offline,
                        name: String::from("OfflineTab"),
                        after: Some(SheetId(0)),
                    },
                ],
                30,
            )
            .unwrap();

        // Reconnect: Welcome detects client_seq <= 2 and remaps offline edit to client_seq=3,
        // id=3, sheet_counter=2!
        let hello = crashed.on_connected();
        let ClientAction::SendFrame(h) = &hello[0] else {
            panic!();
        };
        let welcome = server.on_hello(USER_A, Some(MemberRole::Owner), h).unwrap();
        let w_actions = crashed.on_welcome(welcome);
        assert_eq!(crashed.next_client_seq(), 4);
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: remapped }) =
            w_actions.last().unwrap()
        else {
            panic!();
        };
        assert_eq!(remapped.len(), 1);
        assert_eq!(remapped[0].client_seq, 3);
        let c3 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            rep,
            remapped[0].clone(),
            30,
        );
        crashed.on_commit(c3);
        assert_eq!(
            crashed.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(server.workbook().sheets().len(), 3);
    }

    #[test]
    fn reject_and_epoch_backup_restore_archive_unreconciled_ops_cleanly() {
        // 1. Rejection -> SaveUnsyncedLocal + PruneUndoFrom
        let mut client = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        connect_client(&mut server, &mut client, USER_A, Some(MemberRole::Editor));
        client
            .push_local(
                String::from("Edit 1"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(0),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(5.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                1,
            )
            .unwrap();
        client
            .push_local(
                String::from("Edit 2"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(1),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(6.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                2,
            )
            .unwrap();

        let rej_actions = client.on_reject(1, WireRejectReasonV1::Forbidden);
        assert!(rej_actions.iter().any(|a| matches!(
            a,
            ClientAction::SaveUnsyncedLocal {
                from_client_seq: 1,
                txns
            } if txns.len() == 2
        )));
        assert!(rej_actions.contains(&ClientAction::PruneUndoFrom(1)));
        assert_eq!(client.pending_len(), 0);
        assert_eq!(
            client.visible().state_digest(),
            client.confirmed().state_digest()
        );

        // 2. Server restored at old backup (`epoch = 1`) -> client resets epoch,
        // remaps pending formulas/inserted IDs, and replays!
        let old_rep = pack_replica_id(0, 1).unwrap();
        let mut cl_epoch = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let welcome_e0 = WireSyncFrameV2::Welcome {
            epoch: 0,
            head_seq: 0,
            replica: old_rep,
            last_client_seq: 0,
            max_id_counter: 0,
            catchup: WireCatchupV2::Txns {
                from_seq: 1,
                txns: vec![],
            },
        };
        cl_epoch.on_connected();
        cl_epoch.on_welcome(welcome_e0);
        cl_epoch.on_disconnected();

        let offline_r = cl_epoch.alloc_row_run(1).unwrap();
        let f_src = "=SUM(A1:A1)";
        let f_bound = BoundFormula::parse_and_bind(
            f_src,
            FormulaMode::Dynamic,
            cl_epoch.visible(),
            SheetId(0),
        )
        .unwrap();
        cl_epoch
            .push_local(
                String::from("Insert + Formula offline"),
                vec![
                    Op::InsertRows {
                        sheet: SheetId(0),
                        after: None,
                        ids: offline_r,
                    },
                    Op::SetCells {
                        sheet: SheetId(0),
                        edits: vec![CellEdit {
                            row: RowId(offline_r.first_id),
                            col: ColId(0),
                            input: CellInput::Formula {
                                formula: f_bound,
                                cached: None,
                            },
                            format: FormatPatch::Keep,
                        }],
                    },
                ],
                50,
            )
            .unwrap();

        let restored_server_wb = reticula_core::ops::Workbook::new();
        let mut restored_server = ServerDocMachine::from_snapshot_and_log(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            1,
            0,
            restored_server_wb,
            1,
            vec![],
            vec![],
            vec![],
            ServerLimits::default(),
        )
        .unwrap();

        let hello_e1 = cl_epoch.on_connected();
        let ClientAction::SendFrame(he1) = &hello_e1[0] else {
            panic!();
        };
        let w_e1 = restored_server
            .on_hello(USER_A, Some(MemberRole::Owner), he1)
            .unwrap();
        let actions_e1 = cl_epoch.on_welcome(w_e1);
        assert!(actions_e1.contains(&ClientAction::EpochChanged {
            old_epoch: 0,
            new_epoch: 1
        }));
        assert_eq!(cl_epoch.epoch(), 1);
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: e1_push }) =
            actions_e1.last().unwrap()
        else {
            panic!();
        };
        let commit_e1 = commit_ok(
            &mut restored_server,
            USER_A,
            Some(MemberRole::Owner),
            cl_epoch.replica().unwrap(),
            e1_push[0].clone(),
            50,
        );
        cl_epoch.on_commit(commit_e1);
        assert_eq!(
            cl_epoch.confirmed().state_digest(),
            restored_server.workbook().state_digest()
        );
        assert_eq!(
            replay_pending_onto(
                cl_epoch.confirmed(),
                &cl_epoch.pending_wire_txns(),
                cl_epoch.confirmed_permutes()
            )
            .state_digest(),
            cl_epoch.visible().state_digest()
        );

        // Digest mismatch detection check
        let bad = WireStateDigestV1 {
            version: 1,
            content_lo: 123,
            content_hi: 456,
            order: 789,
        };
        let mut dm_server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/w"),
            ServerLimits::default(),
        );
        let r_dm = dm_server.allocate_replica(USER_A).unwrap();
        let c_dm = WireCommittedTxnV3 {
            seq: 1,
            replica: r_dm,
            client_seq: 1,
            user_id: USER_A,
            timestamp_ms: 1,
            txn: ops_to_wire_txn_v3(String::new(), &[]).unwrap(),
        };
        let mut dm_client = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        dm_client.on_connected();
        dm_client.on_welcome(
            dm_server
                .on_hello(
                    USER_A,
                    Some(MemberRole::Owner),
                    &WireSyncFrameV2::Hello {
                        protocol_version: SYNC_PROTOCOL_VERSION_V2,
                        doc_id: DOC_ID,
                        branch_id: BRANCH_ID,
                        replica: Some(r_dm),
                        last_applied_seq: 0,
                        known_epoch: 0,
                    },
                )
                .unwrap(),
        );
        dm_client.on_commit(c_dm);
        let mismatch = dm_client.on_digest(1, bad);
        assert!(matches!(
            mismatch.as_slice(),
            [ClientAction::DigestMismatch { seq: 1, .. }]
        ));
    }

    #[test]
    fn snapshot_catchup_buffers_live_commits_and_merges_cleanly() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits {
                catchup_window: 2,
                snapshot_txn_threshold: 2,
                ..ServerLimits::default()
            },
        );
        let mut alice = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut alice, USER_A, Some(MemberRole::Owner));

        let mut snap_wb_at_2 = None;
        for i in 1..=3_u64 {
            let act = alice
                .push_local(
                    String::from("step"),
                    vec![Op::SetCells {
                        sheet: SheetId(0),
                        edits: vec![CellEdit {
                            row: RowId(i),
                            col: ColId(0),
                            input: CellInput::Scalar(ScalarValue::Number(i as f64)),
                            format: FormatPatch::Keep,
                        }],
                    }],
                    i,
                )
                .unwrap();
            let ClientAction::SendFrame(WireSyncFrameV2::Push { txns }) = act.last().unwrap()
            else {
                panic!();
            };
            let c = commit_ok(
                &mut server,
                USER_A,
                Some(MemberRole::Owner),
                alice.replica().unwrap(),
                txns[0].clone(),
                i,
            );
            alice.on_commit(c);
            if i == 2 {
                assert!(server.needs_snapshot());
                snap_wb_at_2 = Some(server.workbook().clone());
            }
        }
        // Seq 3 committed while the snapshot at seq 2 was being written.
        server.mark_snapshot_written(2);
        assert!(!server.needs_snapshot());
        let snap_wb_at_2 = snap_wb_at_2.unwrap();

        // Bob connects late -> gets SnapshotRequired(snapshot_seq = 2), while commit 4 arrives live!
        let mut bob = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let h = bob.on_connected();
        let ClientAction::SendFrame(hf) = &h[0] else {
            panic!();
        };
        let w = server
            .on_hello(USER_B, Some(MemberRole::Editor), hf)
            .unwrap();
        let bob_actions = bob.on_welcome(w);
        assert!(matches!(
            bob_actions[0],
            ClientAction::RequestSnapshot {
                snapshot_seq: 2,
                ..
            }
        ));

        // Server commits seq 4 while Bob is downloading snapshot 2:
        let act4 = alice
            .push_local(
                String::from("4"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(4),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(4.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                4,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: t4 }) = act4.last().unwrap()
        else {
            panic!();
        };
        let c4 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            alice.replica().unwrap(),
            t4[0].clone(),
            4,
        );
        assert!(server.needs_snapshot());
        bob.on_commit(c4);

        let wms: Vec<ReplicaWatermark> = server.watermarks();
        let tail = server.txns_since(2).unwrap();
        bob.install_snapshot(2, snap_wb_at_2, &wms, Vec::new(), tail);
        assert_eq!(bob.connection_state(), ConnectionState::Online);
        assert_eq!(bob.confirmed_seq(), 4);
        assert_eq!(
            bob.confirmed().state_digest(),
            server.workbook().state_digest()
        );
    }

    #[test]
    fn reject_reuses_client_seq_and_reloaded_server_deduplicates_snapshotted_seq() {
        use reticula_wire::v2::WireRejectReasonV1;

        use crate::server::ApplyResult;

        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits {
                max_populated_cells: 1,
                ..ServerLimits::default()
            },
        );
        let mut alice = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut alice, USER_A, Some(MemberRole::Owner));

        // 1. Push and commit txn 1 (populated cell count = 1, hits cap).
        let act1 = alice
            .push_local(
                String::from("cell1"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(0),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(10.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                1,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: t1 }) = act1.last().unwrap()
        else {
            panic!();
        };
        let p1 = t1[0].clone();
        let c1 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            alice.replica().unwrap(),
            p1.clone(),
            1,
        );
        alice.on_ack(c1.replica, c1.client_seq);
        alice.on_commit(c1);
        assert_eq!(alice.last_acked_client_seq(), 1);
        assert_eq!(alice.next_client_seq(), 2);

        // 2. Push txn 2 exceeding quota -> server rejects -> client archives and reuses seq 2.
        let act2 = alice
            .push_local(
                String::from("cell2"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(1),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(20.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                2,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: t2 }) = act2.last().unwrap()
        else {
            panic!();
        };
        assert_eq!(t2[0].client_seq, 2);
        let rej_err = server
            .apply_and_commit(
                USER_A,
                Some(MemberRole::Owner),
                alice.replica().unwrap(),
                t2[0].clone(),
                2,
            )
            .unwrap_err();
        assert_eq!(rej_err, WireRejectReasonV1::QuotaExceeded);

        let rej_actions = alice.on_reject(2, rej_err);
        assert!(matches!(
            rej_actions.as_slice(),
            [
                ClientAction::SaveUnsyncedLocal { from_client_seq: 2, txns },
                ClientAction::PruneUndoFrom(2),
                ClientAction::PersistTruncatedPending(rem),
                ClientAction::NotifyVisibleChanged,
            ] if rem.is_empty() && txns.len() == 1
        ));
        assert_eq!(alice.next_client_seq(), 2);

        // 3. Subsequent local edit overwrites existing cell (count stays 1) with client_seq = 2.
        let act3 = alice
            .push_local(
                String::from("cell1_update"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(0),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(30.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                3,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: t3 }) = act3.last().unwrap()
        else {
            panic!();
        };
        assert_eq!(t3[0].client_seq, 2);
        let c2 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            alice.replica().unwrap(),
            t3[0].clone(),
            3,
        );
        alice.on_commit(c2);
        assert_eq!(alice.last_acked_client_seq(), 2);

        // Stale terminal reject for an already-acked client_seq must be ignored.
        assert!(
            alice
                .on_reject(1, WireRejectReasonV1::QuotaExceeded)
                .is_empty()
        );

        // 4. Reload server from snapshot at seq=2 with empty tail and verify resending p1 yields Duplicate.
        let mut reloaded = ServerDocMachine::from_snapshot_and_log(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            server.epoch(),
            server.head_seq(),
            server.workbook().clone(),
            server.next_per_epoch_replica(),
            server.watermarks(),
            server.committed_permutes().to_vec(),
            Vec::new(),
            ServerLimits::default(),
        )
        .unwrap();
        let dup = reloaded
            .apply_and_commit(
                USER_A,
                Some(MemberRole::Owner),
                alice.replica().unwrap(),
                p1,
                4,
            )
            .unwrap();
        assert_eq!(
            dup,
            ApplyResult::Duplicate {
                replica: alice.replica().unwrap(),
                client_seq: 1,
                seq: 2,
                timestamp_ms: 0,
            }
        );
    }

    #[test]
    fn epoch_change_snapshot_required_preserves_visible_pending_invariant() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits {
                catchup_window: 1,
                snapshot_txn_threshold: 1,
                ..ServerLimits::default()
            },
        );
        let mut alice = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut alice, USER_A, Some(MemberRole::Owner));

        // Epoch 0 commits seq 1, captures snapshot at 1, then server restores backup bumping epoch to 1.
        let act1 = alice
            .push_local(
                String::from("seed"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(0),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(10.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                1,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns }) = act1.last().unwrap() else {
            panic!();
        };
        let c1 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            alice.replica().unwrap(),
            txns[0].clone(),
            1,
        );
        alice.on_commit(c1);
        let snap1 = server.workbook().clone();
        let wms1 = server.watermarks();

        // Reload server at epoch=1 with snapshot_seq=1 and head_seq=1 + a second commit so gap > catchup_window.
        let mut backup_srv = ServerDocMachine::from_snapshot_and_log(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            1,
            1,
            snap1.clone(),
            1,
            wms1.clone(),
            Vec::new(),
            Vec::new(),
            ServerLimits {
                catchup_window: 1,
                snapshot_txn_threshold: 1,
                ..ServerLimits::default()
            },
        )
        .unwrap();
        let bob_rep = backup_srv.allocate_replica(USER_B).unwrap();
        backup_srv
            .apply_and_commit(
                USER_B,
                Some(MemberRole::Editor),
                bob_rep,
                WirePendingTxnV3 {
                    replica: bob_rep,
                    client_seq: 1,
                    base_server_seq: 1,
                    timestamp_ms: 2,
                    txn: ops_to_wire_txn_v3(
                        String::from("b1"),
                        &[Op::SetCells {
                            sheet: SheetId(0),
                            edits: vec![CellEdit {
                                row: RowId(1),
                                col: ColId(0),
                                input: CellInput::Scalar(ScalarValue::Number(20.0)),
                                format: FormatPatch::Keep,
                            }],
                        }],
                    )
                    .unwrap(),
                },
                2,
            )
            .unwrap();
        backup_srv
            .apply_and_commit(
                USER_B,
                Some(MemberRole::Editor),
                bob_rep,
                WirePendingTxnV3 {
                    replica: bob_rep,
                    client_seq: 2,
                    base_server_seq: 1,
                    timestamp_ms: 3,
                    txn: ops_to_wire_txn_v3(
                        String::from("b2"),
                        &[Op::SetCells {
                            sheet: SheetId(0),
                            edits: vec![CellEdit {
                                row: RowId(2),
                                col: ColId(0),
                                input: CellInput::Scalar(ScalarValue::Number(30.0)),
                                format: FormatPatch::Keep,
                            }],
                        }],
                    )
                    .unwrap(),
                },
                3,
            )
            .unwrap();

        // Alice disconnects, reconnects (starts Handshaking), pushes a local edit BEFORE Welcome arrives.
        alice.on_disconnected();
        let hello_act = alice.on_connected();
        let ClientAction::SendFrame(hf) = &hello_act[0] else {
            panic!();
        };
        let _ = alice
            .push_local(
                String::from("local_offline"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(3),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(40.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                4,
            )
            .unwrap();

        let welcome = backup_srv
            .on_hello(USER_A, Some(MemberRole::Owner), hf)
            .unwrap();
        let actions = alice.on_welcome(welcome);
        assert!(matches!(
            actions.as_slice(),
            [
                ClientAction::EpochChanged {
                    old_epoch: 0,
                    new_epoch: 1
                },
                ClientAction::RequestSnapshot {
                    snapshot_seq: 1,
                    ..
                }
            ]
        ));
        assert_eq!(
            alice.visible().state_digest(),
            replay_pending_onto(
                alice.confirmed(),
                &alice.pending_wire_txns(),
                alice.confirmed_permutes()
            )
            .state_digest()
        );

        let tail = backup_srv.txns_since(1).unwrap();
        let snap_actions = alice.install_snapshot(1, snap1, &wms1, Vec::new(), tail);
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: remapped }) =
            snap_actions.last().unwrap()
        else {
            panic!();
        };
        let c_final = commit_ok(
            &mut backup_srv,
            USER_A,
            Some(MemberRole::Owner),
            alice.replica().unwrap(),
            remapped[0].clone(),
            5,
        );
        alice.on_commit(c_final);
        assert_eq!(
            alice.confirmed().state_digest(),
            backup_srv.workbook().state_digest()
        );
    }

    #[test]
    fn awaiting_snapshot_after_epoch_change_keeps_pending_wal_monotonic() {
        use reticula_wire::wal::{encode_pending_wal_complete_v2, scan_pending_wal_v2};

        let mut srv0 = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits {
                catchup_window: 1,
                snapshot_txn_threshold: 1,
                ..ServerLimits::default()
            },
        );
        let mut bob = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut srv0, &mut bob, USER_B, Some(MemberRole::Editor));
        for i in 1..=2_u64 {
            let act = bob
                .push_local(
                    String::from("b"),
                    vec![Op::SetCells {
                        sheet: SheetId(0),
                        edits: vec![CellEdit {
                            row: RowId(i),
                            col: ColId(0),
                            input: CellInput::Scalar(ScalarValue::Number(i as f64)),
                            format: FormatPatch::Keep,
                        }],
                    }],
                    i,
                )
                .unwrap();
            let ClientAction::SendFrame(WireSyncFrameV2::Push { txns }) = act.last().unwrap()
            else {
                panic!();
            };
            let c = commit_ok(
                &mut srv0,
                USER_B,
                Some(MemberRole::Editor),
                bob.replica().unwrap(),
                txns[0].clone(),
                i,
            );
            bob.on_commit(c);
        }
        let snap1 = srv0.workbook().clone();
        let tail_after_1 = srv0.txns_since(1).unwrap();
        let wms = srv0.watermarks();

        // Reload server bumped to epoch=1 at snapshot_seq=1 + 1 tail commit, then push one more
        // commit so head_seq=3 (gap from 0 = 3 > catchup_window=1).
        let mut srv1 = ServerDocMachine::from_snapshot_and_log(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            1,
            1,
            snap1.clone(),
            1,
            wms.clone(),
            Vec::new(),
            tail_after_1,
            ServerLimits {
                catchup_window: 1,
                snapshot_txn_threshold: 1,
                ..ServerLimits::default()
            },
        )
        .unwrap();
        let rep = srv1.allocate_replica(USER_B).unwrap();
        srv1.apply_and_commit(
            USER_B,
            Some(MemberRole::Editor),
            rep,
            WirePendingTxnV3 {
                replica: rep,
                client_seq: 1,
                base_server_seq: 2,
                timestamp_ms: 3,
                txn: ops_to_wire_txn_v3(
                    String::from("b3"),
                    &[Op::SetCells {
                        sheet: SheetId(0),
                        edits: vec![CellEdit {
                            row: RowId(3),
                            col: ColId(0),
                            input: CellInput::Scalar(ScalarValue::Number(3.0)),
                            format: FormatPatch::Keep,
                        }],
                    }],
                )
                .unwrap(),
            },
            3,
        )
        .unwrap();

        // Alice authors 2 offline edits (cs=1,2), connects & gets EpochChanged + SnapshotRequired,
        // then authors a 3rd edit while AwaitingSnapshot -> must write cs=3, NOT cs=1!
        let mut alice = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        for cs_row in 10..=11_u64 {
            let _ = alice
                .push_local(
                    String::from("off"),
                    vec![Op::SetCells {
                        sheet: SheetId(0),
                        edits: vec![CellEdit {
                            row: RowId(cs_row),
                            col: ColId(1),
                            input: CellInput::Scalar(ScalarValue::Number(cs_row as f64)),
                            format: FormatPatch::Keep,
                        }],
                    }],
                    cs_row,
                )
                .unwrap();
        }
        let h = alice.on_connected();
        let ClientAction::SendFrame(hf) = &h[0] else {
            panic!();
        };
        let w = srv1.on_hello(USER_A, Some(MemberRole::Owner), hf).unwrap();
        let _ = alice.on_welcome(w);
        assert_eq!(
            alice.connection_state(),
            ConnectionState::AwaitingSnapshot { snapshot_seq: 1 }
        );

        let _ = alice
            .push_local(
                String::from("during_snap"),
                vec![Op::SetCells {
                    sheet: SheetId(0),
                    edits: vec![CellEdit {
                        row: RowId(12),
                        col: ColId(1),
                        input: CellInput::Scalar(ScalarValue::Number(12.0)),
                        format: FormatPatch::Keep,
                    }],
                }],
                12,
            )
            .unwrap();
        let pending = alice.pending_wire_txns();
        let seqs: Vec<u64> = pending.iter().map(|p| p.client_seq).collect();
        assert_eq!(seqs, [1, 2, 3]);

        let wal_bytes = encode_pending_wal_complete_v2(&pending).unwrap();
        let scan = scan_pending_wal_v2(&wal_bytes).unwrap();
        assert_eq!(scan.records.len(), 3);

        // Reload from persisted OPFS state and complete install_snapshot -> full convergence!
        let mut reloaded = ClientSyncMachine::from_persisted(
            DOC_ID,
            BRANCH_ID,
            alice.epoch(),
            alice.replica(),
            alice.confirmed_seq(),
            alice.last_acked_client_seq(),
            alice.confirmed().clone(),
            alice.confirmed_permutes().to_vec(),
            scan.records,
            1,
        );
        let h2 = reloaded.on_connected();
        let ClientAction::SendFrame(hf2) = &h2[0] else {
            panic!();
        };
        let w2 = srv1.on_hello(USER_A, Some(MemberRole::Owner), hf2).unwrap();
        let _ = reloaded.on_welcome(w2);
        let tail = srv1.txns_since(1).unwrap();
        let snap_acts = reloaded.install_snapshot(1, snap1, &wms, Vec::new(), tail);
        for act in snap_acts {
            if let ClientAction::SendFrame(WireSyncFrameV2::Push { txns }) = act {
                for p in txns {
                    let c = commit_ok(
                        &mut srv1,
                        USER_A,
                        Some(MemberRole::Owner),
                        reloaded.replica().unwrap(),
                        p,
                        99,
                    );
                    reloaded.on_commit(c);
                }
            }
        }
        assert_eq!(
            reloaded.confirmed().state_digest(),
            srv1.workbook().state_digest()
        );
    }

    #[test]
    fn viewer_handshake_uses_sentinel_replica_blocks_edits_and_converges_via_snapshot() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits {
                catchup_window: 2,
                ..ServerLimits::default()
            },
        );
        let mut owner = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut owner, USER_A, Some(MemberRole::Owner));

        for step in 1..=5_u32 {
            let acts = owner
                .push_local(
                    String::from("Owner edit"),
                    vec![set_num(step, 0, f64::from(step))],
                    u64::from(step),
                )
                .unwrap();
            for act in acts {
                if let ClientAction::SendFrame(WireSyncFrameV2::Push { txns }) = act {
                    for p in txns {
                        let c = commit_ok(
                            &mut server,
                            USER_A,
                            Some(MemberRole::Owner),
                            owner.replica().unwrap(),
                            p,
                            u64::from(step),
                        );
                        owner.on_commit(c);
                    }
                }
            }
        }
        let snap_seq = server.head_seq();
        server.mark_snapshot_written(snap_seq);
        let snap_wb = server.workbook().clone();
        let snap_wms = server.watermarks();

        let owner_rep = owner.replica().unwrap();
        let acts6 = owner
            .push_local(String::from("Tail"), vec![set_num(6, 0, 6.0)], 60)
            .unwrap();
        for act in acts6 {
            if let ClientAction::SendFrame(WireSyncFrameV2::Push { txns }) = act {
                for p in txns {
                    let c = commit_ok(
                        &mut server,
                        USER_A,
                        Some(MemberRole::Owner),
                        owner_rep,
                        p,
                        60,
                    );
                    owner.on_commit(c);
                }
            }
        }

        let viewer_user = [0xCC; 16];
        let mut viewer = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let out = viewer.on_connected();
        let ClientAction::SendFrame(hello) = &out[0] else {
            panic!("expected Hello");
        };
        let welcome = server
            .on_hello(viewer_user, Some(MemberRole::Viewer), hello)
            .unwrap();
        assert!(matches!(
            welcome,
            WireSyncFrameV2::Welcome {
                replica: VIEWER_REPLICA,
                last_client_seq: 0,
                max_id_counter: 0,
                catchup: WireCatchupV2::SnapshotRequired { .. },
                ..
            }
        ));
        let welcome_acts = viewer.on_welcome(welcome);
        assert!(
            welcome_acts
                .iter()
                .any(|a| matches!(a, ClientAction::RequestSnapshot { .. }))
        );
        let tail = server.txns_since(snap_seq).unwrap();
        let snap_acts = viewer.install_snapshot(snap_seq, snap_wb, &snap_wms, Vec::new(), tail);
        assert!(
            !snap_acts
                .iter()
                .any(|a| matches!(a, ClientAction::SendFrame(WireSyncFrameV2::Push { .. })))
        );
        assert!(viewer.is_read_only());
        assert_eq!(viewer.replica(), None);
        assert_eq!(
            viewer.visible().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            viewer.push_local(String::from("Blocked"), vec![set_num(0, 0, 99.0)], 99),
            Err(reticula_core::ops::OpError::ReadOnly)
        );
        // Viewer Push rejected by server even with sentinel replica 0.
        let forged = WirePendingTxnV3 {
            replica: VIEWER_REPLICA,
            client_seq: 1,
            base_server_seq: snap_seq,
            timestamp_ms: 99,
            txn: ops_to_wire_txn_v3(String::from("Rogue"), &[set_num(0, 0, 99.0)]).unwrap(),
        };
        assert!(matches!(
            server.validate_and_try_apply(
                viewer_user,
                Some(MemberRole::Viewer),
                VIEWER_REPLICA,
                forged,
                99
            ),
            Err(WireRejectReasonV1::Forbidden)
        ));
    }

    #[test]
    fn editor_downgraded_to_viewer_rescues_pending_edits_and_converges() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        let mut bob = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut bob, USER_B, Some(MemberRole::Editor));
        assert!(!bob.is_read_only());

        bob.on_disconnected();
        bob.push_local(String::from("Offline"), vec![set_num(0, 0, 77.0)], 10)
            .unwrap();
        assert_eq!(bob.pending_len(), 1);

        let h = bob.on_connected();
        let ClientAction::SendFrame(hello) = &h[0] else {
            panic!();
        };
        let welcome = server
            .on_hello(USER_B, Some(MemberRole::Viewer), hello)
            .unwrap();
        let acts = bob.on_welcome(welcome);

        assert!(bob.is_read_only());
        assert_eq!(bob.replica(), None);
        assert_eq!(bob.pending_len(), 0);
        assert_eq!(
            bob.visible().state_digest(),
            server.workbook().state_digest()
        );
        assert!(acts.iter().any(|a| matches!(
            a,
            ClientAction::SaveUnsyncedLocal {
                from_client_seq: 1,
                txns
            } if txns.len() == 1
        )));
        assert!(
            acts.iter()
                .any(|a| matches!(a, ClientAction::PruneUndoFrom(1)))
        );
        assert!(
            acts.iter()
                .any(|a| matches!(a, ClientAction::PersistTruncatedPending(rem) if rem.is_empty()))
        );
    }

    #[test]
    fn imported_non_empty_base_requires_snapshot_on_seq_zero_hello() {
        let mut base = reticula_core::ops::Workbook::new();
        base.apply(set_num(0, 0, 42.0)).unwrap();

        let mut server = ServerDocMachine::from_snapshot_and_log(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            0,
            0,
            base.clone(),
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            ServerLimits::default(),
        )
        .unwrap();

        let hello = WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: DOC_ID,
            branch_id: BRANCH_ID,
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        };
        let welcome = server
            .on_hello(USER_A, Some(MemberRole::Owner), &hello)
            .unwrap();
        assert!(matches!(
            welcome,
            WireSyncFrameV2::Welcome {
                head_seq: 0,
                catchup: WireCatchupV2::SnapshotRequired {
                    snapshot_seq: 0,
                    ..
                },
                ..
            }
        ));

        let mut alice = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let acts = alice.on_welcome(welcome);
        assert!(acts.iter().any(|a| matches!(
            a,
            ClientAction::RequestSnapshot {
                snapshot_seq: 0,
                ..
            }
        )));
        alice.install_snapshot(0, base, &[], Vec::new(), Vec::new());
        assert_eq!(
            alice.confirmed().state_digest(),
            server.workbook().state_digest()
        );

        let mut empty_server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        let empty_welcome = empty_server
            .on_hello(USER_A, Some(MemberRole::Owner), &hello)
            .unwrap();
        assert!(matches!(
            empty_welcome,
            WireSyncFrameV2::Welcome {
                catchup: WireCatchupV2::Txns { txns, .. },
                ..
            } if txns.is_empty()
        ));
    }

    #[test]
    fn e16_concurrent_sort_transforms_all_content_ops_and_fill_formula() {
        use alloc::boxed::Box;
        use reticula_core::format::NumberFormat;
        use reticula_core::id::IdRun;
        use reticula_core::ops::{CellInput, ConditionalCellEdit, FormatTarget};
        use reticula_core::style::StylePatch;
        use reticula_formula::ast::BinaryOp;
        use reticula_formula::binder::{
            AxisTarget, BoundExpr, BoundRef, TemplateExpr, TemplateFormula, TemplateRef,
        };

        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        let mut a = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let mut b = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut a, USER_A, Some(MemberRole::Owner));
        connect_client(&mut server, &mut b, USER_B, Some(MemberRole::Editor));

        // Seed rows 0..3, cols 0..=1.
        let seed_ops = vec![Op::SetBlock {
            sheet: SheetId(0),
            rows: vec![IdRun::new(0, 4).unwrap()],
            cols: vec![ColId(0), ColId(1)],
            columns_data: vec![
                vec![
                    CellInput::Scalar(ScalarValue::Number(30.0)),
                    CellInput::Scalar(ScalarValue::Number(10.0)),
                    CellInput::Scalar(ScalarValue::Number(20.0)),
                    CellInput::Scalar(ScalarValue::Number(99.0)),
                ],
                vec![
                    CellInput::Scalar(ScalarValue::Text(String::from("row0_b"))),
                    CellInput::Scalar(ScalarValue::Text(String::from("row1_b"))),
                    CellInput::Scalar(ScalarValue::Text(String::from("row2_b"))),
                    CellInput::Scalar(ScalarValue::Text(String::from("row3_b"))),
                ],
            ],
        }];
        let a0 = a.push_local(String::from("seed"), seed_ops, 1).unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: seed_push }) = a0.last().unwrap()
        else {
            panic!();
        };
        let c_seed = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            seed_push[0].clone(),
            1,
        );
        a.on_commit(c_seed.clone());
        b.on_commit(c_seed);
        assert_eq!(a.confirmed_seq(), 1);
        assert_eq!(b.confirmed_seq(), 1);

        // Now A sorts col 0 only across rows [0, 1, 2] ascending -> row_perm = [1, 2, 0]
        // Meaning: dst0(Row0) <- Row1(10), dst1(Row1) <- Row2(20), dst2(Row2) <- Row0(30).
        // Therefore forward record map across col 0 is: Row1 -> Row0, Row2 -> Row1, Row0 -> Row2.
        let sort_op = Op::PermuteContent {
            sheet: SheetId(0),
            rows: vec![RowId(0), RowId(1), RowId(2)],
            cols: vec![ColId(0)],
            row_perm: vec![1, 2, 0],
        };
        let a_sort = a
            .push_local(String::from("sort"), vec![sort_op], 2)
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: sort_push }) =
            a_sort.last().unwrap()
        else {
            panic!();
        };

        // Concurrently BEFORE B receives sort (B still at confirmed_seq = 1):
        // B edits Row0/Col0 (30 -> 35), Row0/Col1 ("row0_b" -> "still_row0_b"),
        // conditionally updates Row1/Col0 (10 -> 15), SetFormat on Row2/Col0,
        // and FillFormula across rows 0..=3, cols 0..=1!
        let b_txns_ops = [
            Op::SetCells {
                sheet: SheetId(0),
                edits: vec![
                    CellEdit {
                        row: RowId(0),
                        col: ColId(0),
                        input: CellInput::Scalar(ScalarValue::Number(35.0)),
                        format: FormatPatch::Keep,
                    },
                    CellEdit {
                        row: RowId(0),
                        col: ColId(1),
                        input: CellInput::Scalar(ScalarValue::Text(String::from("still_row0_b"))),
                        format: FormatPatch::Keep,
                    },
                ],
            },
            Op::SetCellsIf {
                sheet: SheetId(0),
                edits: vec![ConditionalCellEdit {
                    row: RowId(1),
                    col: ColId(0),
                    expected_input: CellInput::Scalar(ScalarValue::Number(10.0)),
                    expected_format: None,
                    new_input: CellInput::Scalar(ScalarValue::Number(15.0)),
                    new_format: FormatPatch::Keep,
                }],
            },
            Op::SetFormat {
                sheet: SheetId(0),
                target: FormatTarget::Cells {
                    rows: vec![IdRun::new(2, 1).unwrap()],
                    cols: vec![IdRun::new(0, 1).unwrap()],
                },
                number_format: FormatPatch::Set(NumberFormat::Fixed2),
                style: StylePatch::default(),
            },
            Op::FillFormula {
                sheet: SheetId(0),
                rows: vec![IdRun::new(0, 4).unwrap()],
                cols: vec![IdRun::new(0, 2).unwrap()],
                template: TemplateFormula {
                    mode: FormulaMode::Dynamic,
                    root: TemplateExpr::Binary {
                        op: BinaryOp::Add,
                        left: Box::new(TemplateExpr::Reference(TemplateRef::Cell {
                            sheet: None,
                            missing_sheet: None,
                            row: AxisTarget::IdDelta(0),
                            col: AxisTarget::Fixed(2),
                            abs_col: true,
                            abs_row: false,
                        })),
                        right: Box::new(TemplateExpr::Literal(
                            reticula_formula::ast::Literal::Number(1.0),
                        )),
                    },
                },
            },
        ];
        let b1 = b
            .push_local(String::from("b_edits"), b_txns_ops[..3].to_vec(), 3)
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: b1_push }) = b1.last().unwrap()
        else {
            panic!();
        };

        let c_sort = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            sort_push[0].clone(),
            2,
        );
        let c_b1 = commit_ok(
            &mut server,
            USER_B,
            Some(MemberRole::Editor),
            b.replica().unwrap(),
            b1_push[0].clone(),
            3,
        );
        a.on_commit(c_sort.clone());
        b.on_commit(c_sort);
        assert_eq!(
            b.visible().state_digest(),
            replay_pending_onto(
                b.confirmed(),
                &b.pending_wire_txns(),
                b.confirmed_permutes()
            )
            .state_digest()
        );
        assert_eq!(
            b.visible().sheet(SheetId(0)).unwrap().value(2, 0),
            Value::Number(35.0)
        );
        assert_eq!(
            b.visible().sheet(SheetId(0)).unwrap().value(0, 0),
            Value::Number(15.0)
        );

        a.on_commit(c_b1.clone());
        b.on_commit(c_b1);
        assert_eq!(b.pending_len(), 0);
        assert_eq!(
            a.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            b.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            server.workbook().sheet(SheetId(0)).unwrap().value(0, 0),
            Value::Number(15.0)
        );
        assert_eq!(
            server.workbook().sheet(SheetId(0)).unwrap().value(2, 0),
            Value::Number(35.0)
        );
        assert_eq!(
            server
                .workbook()
                .sheet(SheetId(0))
                .unwrap()
                .cell_format_by_id(RowId(1), ColId(0)),
            Some(NumberFormat::Fixed2)
        );

        // Now test FillFormula & ClearRange at pre-sort base_server_seq = 1:
        let fill_ops = core::slice::from_ref(&b_txns_ops[3]);
        let transformed_fill = transform_ops_through_permute(
            fill_ops,
            server.committed_permutes(),
            1,
            b.replica().unwrap(),
            b.next_client_seq(),
        );
        assert_eq!(transformed_fill.len(), 3);
        let fill_pending = WirePendingTxnV3 {
            replica: b.replica().unwrap(),
            client_seq: b.next_client_seq(),
            base_server_seq: 1,
            timestamp_ms: 4,
            txn: ops_to_wire_txn_v3(String::from("late_fill"), fill_ops).unwrap(),
        };
        let _c_fill = commit_ok(
            &mut server,
            USER_B,
            Some(MemberRole::Editor),
            b.replica().unwrap(),
            fill_pending,
            4,
        );
        let f_at_dst2 = server
            .workbook()
            .formulas()
            .get(&(SheetId(0), RowId(2), ColId(0)))
            .unwrap();
        let BoundExpr::Binary { left, .. } = &f_at_dst2.root else {
            panic!("expected binary formula");
        };
        assert_eq!(
            **left,
            BoundExpr::Reference(BoundRef::Cell {
                sheet: None,
                missing_sheet: None,
                row: RowId(0),
                col: ColId(2),
                abs_col: true,
                abs_row: false,
            })
        );
    }

    #[test]
    fn e16_own_offline_sort_followed_by_own_edit_is_not_double_permuted() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        let mut a = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let mut b = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut a, USER_A, Some(MemberRole::Owner));
        connect_client(&mut server, &mut b, USER_B, Some(MemberRole::Editor));

        let s_acts = a
            .push_local(
                String::from("seed"),
                vec![set_num(0, 0, 1.0), set_num(1, 0, 2.0)],
                1,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: s_push }) =
            s_acts.last().unwrap()
        else {
            panic!();
        };
        let c1 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            s_push[0].clone(),
            1,
        );
        a.on_commit(c1.clone());
        b.on_commit(c1);

        a.on_disconnected();
        b.on_disconnected();

        a.push_local(
            String::from("a_sort"),
            vec![Op::PermuteContent {
                sheet: SheetId(0),
                rows: vec![RowId(0), RowId(1)],
                cols: vec![ColId(0)],
                row_perm: vec![1, 0],
            }],
            2,
        )
        .unwrap();
        a.push_local(
            String::from("a_edit_post_sort"),
            vec![set_num(0, 0, 200.0)],
            3,
        )
        .unwrap();

        b.push_local(
            String::from("b_edit_pre_sort"),
            vec![set_num(0, 0, 100.0)],
            4,
        )
        .unwrap();

        let ah = a.on_connected();
        let ClientAction::SendFrame(ref hf_a) = ah[0] else {
            panic!();
        };
        let aw = server
            .on_hello(USER_A, Some(MemberRole::Owner), hf_a)
            .unwrap();
        let a_w_acts = a.on_welcome(aw);
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: a_pushes }) =
            a_w_acts.last().unwrap()
        else {
            panic!();
        };
        assert_eq!(a_pushes.len(), 2);
        let ca2 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            a_pushes[0].clone(),
            5,
        );
        let ca3 = commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            a_pushes[1].clone(),
            6,
        );
        a.on_commit(ca2);
        a.on_commit(ca3);

        let bh = b.on_connected();
        let ClientAction::SendFrame(ref hf_b) = bh[0] else {
            panic!();
        };
        let bw = server
            .on_hello(USER_B, Some(MemberRole::Editor), hf_b)
            .unwrap();
        let b_w_acts = b.on_welcome(bw);
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: b_pushes }) =
            b_w_acts.last().unwrap()
        else {
            panic!();
        };
        let cb4 = commit_ok(
            &mut server,
            USER_B,
            Some(MemberRole::Editor),
            b.replica().unwrap(),
            b_pushes[0].clone(),
            7,
        );
        b.on_commit(cb4.clone());
        a.on_commit(cb4);

        assert_eq!(
            a.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            b.confirmed().state_digest(),
            server.workbook().state_digest()
        );
        assert_eq!(
            server.workbook().sheet(SheetId(0)).unwrap().value(0, 0),
            Value::Number(200.0)
        );
        assert_eq!(
            server.workbook().sheet(SheetId(0)).unwrap().value(1, 0),
            Value::Number(100.0)
        );
    }

    #[test]
    fn e16_server_cold_restart_with_persisted_permutes_and_v1_upgrade_required() {
        let mut server = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits {
                catchup_window: 1,
                snapshot_txn_threshold: 1,
                ..ServerLimits::default()
            },
        );
        let mut a = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        let mut b = ClientSyncMachine::new(DOC_ID, BRANCH_ID);
        connect_client(&mut server, &mut a, USER_A, Some(MemberRole::Owner));
        connect_client(&mut server, &mut b, USER_B, Some(MemberRole::Editor));
        b.on_disconnected();

        let a1 = a
            .push_local(
                String::from("sort"),
                vec![Op::PermuteContent {
                    sheet: SheetId(0),
                    rows: vec![RowId(0), RowId(1)],
                    cols: vec![ColId(0)],
                    row_perm: vec![1, 0],
                }],
                1,
            )
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: ap1 }) = a1.last().unwrap()
        else {
            panic!();
        };
        commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            ap1[0].clone(),
            1,
        );
        let a2 = a
            .push_local(String::from("extra"), vec![set_num(5, 0, 5.0)], 2)
            .unwrap();
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: ap2 }) = a2.last().unwrap()
        else {
            panic!();
        };
        commit_ok(
            &mut server,
            USER_A,
            Some(MemberRole::Owner),
            a.replica().unwrap(),
            ap2[0].clone(),
            2,
        );

        let v1_hello = WireSyncFrameV2::Hello {
            protocol_version: 1,
            doc_id: DOC_ID,
            branch_id: BRANCH_ID,
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        };
        let permutes_snap = server.committed_permutes().to_vec();
        let snap_wb = server.workbook().clone();
        let wms = server.watermarks();

        let mut cold_srv = ServerDocMachine::from_snapshot_and_log(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            server.epoch(),
            server.head_seq(),
            snap_wb.clone(),
            server.next_per_epoch_replica(),
            wms.clone(),
            permutes_snap.clone(),
            Vec::new(),
            ServerLimits::default(),
        )
        .unwrap();

        let mut srv_v1_test = ServerDocMachine::new(
            DOC_ID,
            BRANCH_ID,
            String::from("/api/w"),
            ServerLimits::default(),
        );
        let rep_v1 = srv_v1_test.allocate_replica(USER_A).unwrap();
        srv_v1_test
            .apply_and_commit(USER_A, Some(MemberRole::Owner), rep_v1, ap1[0].clone(), 1)
            .unwrap();
        assert_eq!(
            srv_v1_test.on_hello(USER_B, Some(MemberRole::Editor), &v1_hello),
            Err(WireRejectReasonV1::UpgradeRequired)
        );

        b.push_local(String::from("b_offline"), vec![set_num(0, 0, 777.0)], 3)
            .unwrap();
        let bh = b.on_connected();
        let ClientAction::SendFrame(ref hfb) = bh[0] else {
            panic!();
        };
        let bw = cold_srv
            .on_hello(USER_B, Some(MemberRole::Editor), hfb)
            .unwrap();
        let _ = b.on_welcome(bw);
        let b_snap_acts =
            b.install_snapshot(server.head_seq(), snap_wb, &wms, permutes_snap, Vec::new());
        assert_eq!(
            b.visible().sheet(SheetId(0)).unwrap().value(1, 0),
            Value::Number(777.0)
        );
        let ClientAction::SendFrame(WireSyncFrameV2::Push { txns: bp }) =
            b_snap_acts.last().unwrap()
        else {
            panic!();
        };
        let cb = commit_ok(
            &mut cold_srv,
            USER_B,
            Some(MemberRole::Editor),
            b.replica().unwrap(),
            bp[0].clone(),
            4,
        );
        b.on_commit(cb);
        assert_eq!(
            b.confirmed().state_digest(),
            cold_srv.workbook().state_digest()
        );
        assert_eq!(
            cold_srv.workbook().sheet(SheetId(0)).unwrap().value(1, 0),
            Value::Number(777.0)
        );
    }
}
