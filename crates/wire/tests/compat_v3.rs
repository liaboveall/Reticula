//! Backward-compatibility tests for frozen `wire::v3`, `RTCLWAL3`,
//! `RTCLSNAP` sections `9/10/11`, and `SYNC_PROTOCOL_VERSION_V2 = 2` frames (`tests/compat/v3/`).

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use reticula_core::determinism::to_hex;
use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id};
use reticula_core::ops::NamedRangeKey;
use reticula_core::value::Value;
use reticula_formula::binder::BoundExpr;
use reticula_wire::snapshot::decode_snapshot_any;
use reticula_wire::sync_frame::{decode_sync_frame_any, decode_sync_frame_v2};
use reticula_wire::v3::{SYNC_PROTOCOL_VERSION_V2, WireSyncFrameV2};
use reticula_wire::wal::{replay_wal_records_v3, scan_wal_any_v3, scan_wal_v3};

const BASE_SNAP: &[u8] = include_bytes!("../../../tests/compat/v3/base.rtclsnap");
const REPLAY_WAL: &[u8] = include_bytes!("../../../tests/compat/v3/replay.rtclwal");
const EXPECTED_DIGEST: &str = include_str!("../../../tests/compat/v3/expected_digest.txt");
const SYNC_FRAMES_BIN: &[u8] = include_bytes!("../../../tests/compat/v3/sync_frames.bin");

#[test]
fn frozen_v3_snapshot_wal_and_sync_frames_decode_to_exact_expected_digests() {
    let mut decoded = decode_snapshot_any(BASE_SNAP).expect("frozen v3 base snapshot must decode");
    let replica1 = pack_replica_id(0, 1).unwrap();
    assert_eq!(decoded.watermarks.len(), 1);
    assert_eq!(decoded.watermarks[0].replica, replica1);
    assert_eq!(decoded.permutes.len(), 1);
    assert_eq!(decoded.permutes[0].seq, 4);

    let s0 = SheetId(0);
    {
        let base_wb = decoded.engine.workbook();
        assert!(
            base_wb
                .named_ranges()
                .contains_key(&NamedRangeKey::new(None, "Rate"))
        );
        assert!(
            base_wb
                .named_ranges()
                .contains_key(&NamedRangeKey::new(Some(s0), "LocalMax"))
        );
        assert!(matches!(
            base_wb.formula(s0, RowId(1), ColId(1)).map(|f| &f.root),
            Some(BoundExpr::Call { args, .. }) if matches!(args.first(), Some(BoundExpr::SpillRef(_)))
        ));
        assert!(matches!(
            base_wb.formula(s0, RowId(1), ColId(2)).map(|f| &f.root),
            Some(BoundExpr::ScopedName { .. })
        ));
        assert!(matches!(
            base_wb.formula(s0, RowId(0), ColId(3)).map(|f| &f.root),
            Some(BoundExpr::Call { args, .. }) if args.iter().any(|a| matches!(a, BoundExpr::MissingArg))
        ));
        assert!(matches!(
            base_wb.formula(s0, RowId(1), ColId(3)).map(|f| &f.root),
            Some(BoundExpr::PreservedCached { .. })
        ));
        assert!(matches!(
            base_wb.formula(s0, RowId(2), ColId(3)).map(|f| &f.root),
            Some(BoundExpr::Opaque { .. })
        ));
    }

    let scan_direct = scan_wal_v3(REPLAY_WAL, 4, 8).expect("frozen v3 WAL must scan directly");
    let scan = scan_wal_any_v3(REPLAY_WAL, 4, 8).expect("frozen v3 WAL must scan via any_v3");
    assert_eq!(scan_direct, scan);
    replay_wal_records_v3(&mut decoded.engine, &scan.records, 0, true)
        .expect("frozen v3 WAL must replay cleanly");

    let wb = decoded.engine.workbook();
    // Seq 5 PermuteContent([0,1,2], perm=[2,0,1]) moved old A1=10 -> A2, old A2=20 -> A3, old A3=30 -> A1.
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(0), ColId(0)),
        Value::Number(30.0)
    );
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(1), ColId(0)),
        Value::Number(10.0)
    );
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(2), ColId(0)),
        Value::Number(20.0)
    );
    // Seq 6 MoveCells moved A4=40 -> C4.
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(3), ColId(0)),
        Value::Empty
    );
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(3), ColId(2)),
        Value::Number(40.0)
    );
    // Seq 7 defined Sheet1!Tax, seq 8 deleted workbook Rate.
    assert!(
        !wb.named_ranges()
            .contains_key(&NamedRangeKey::new(None, "Rate"))
    );
    assert!(
        wb.named_ranges()
            .contains_key(&NamedRangeKey::new(Some(s0), "Tax"))
    );

    let mut lines = EXPECTED_DIGEST.lines();
    let expected_logical = lines.next().expect("logical digest line").trim();
    let expected_state = lines.next().expect("state digest line").trim();

    let logical_hex = String::from_utf8_lossy(&to_hex(wb.logical_digest())).into_owned();
    assert_eq!(logical_hex, expected_logical);

    let sd = wb.state_digest();
    let actual_state = format!(
        "v{}:{}:{}:{}",
        sd.version,
        String::from_utf8_lossy(&to_hex(sd.content_lo)),
        String::from_utf8_lossy(&to_hex(sd.content_hi)),
        String::from_utf8_lossy(&to_hex(sd.order)),
    );
    assert_eq!(actual_state, expected_state);

    let mut cursor = 0_usize;
    let mut count = 0_usize;
    while cursor < SYNC_FRAMES_BIN.len() {
        let len_bytes: [u8; 4] = SYNC_FRAMES_BIN[cursor..cursor + 4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        cursor += 4;
        let slice = &SYNC_FRAMES_BIN[cursor..cursor + len];
        let frame = decode_sync_frame_v2(slice).expect("frozen v2 sync frame must decode");
        let (ver, any_frame) =
            decode_sync_frame_any(slice).expect("frozen v2 sync frame must decode via any");
        assert_eq!(ver, SYNC_PROTOCOL_VERSION_V2);
        assert_eq!(frame, any_frame);
        assert!(matches!(
            frame,
            WireSyncFrameV2::Hello { .. }
                | WireSyncFrameV2::Welcome { .. }
                | WireSyncFrameV2::Push { .. }
                | WireSyncFrameV2::Ack { .. }
                | WireSyncFrameV2::Reject { .. }
                | WireSyncFrameV2::Commit { .. }
                | WireSyncFrameV2::Digest { .. }
                | WireSyncFrameV2::Presence(_)
                | WireSyncFrameV2::Ping(_)
                | WireSyncFrameV2::Pong(_)
        ));
        cursor += len;
        count += 1;
    }
    assert_eq!(count, 11);
}
