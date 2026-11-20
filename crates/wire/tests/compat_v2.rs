//! Backward-compatibility tests for frozen `wire::v2`, `RTCLWAL2`, and
//! `SYNC_PROTOCOL_VERSION = 1` binary frames (`tests/compat/v2/`).

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use reticula_core::determinism::to_hex;
use reticula_core::id::{ColId, RowId, SheetId, pack_replica_id, pack_sheet_id};
use reticula_core::value::Value;
use reticula_wire::snapshot::decode_snapshot_any;
use reticula_wire::sync_frame::decode_sync_frame;
use reticula_wire::v2::WireSyncFrameV1;
use reticula_wire::wal::{replay_wal_records_v2, scan_wal_any};

const BASE_SNAP: &[u8] = include_bytes!("../../../tests/compat/v2/base.rtclsnap");
const REPLAY_WAL: &[u8] = include_bytes!("../../../tests/compat/v2/replay.rtclwal");
const EXPECTED_DIGEST: &str = include_str!("../../../tests/compat/v2/expected_digest.txt");
const SYNC_FRAMES_BIN: &[u8] = include_bytes!("../../../tests/compat/v2/sync_frames.bin");

#[test]
fn frozen_v2_wal_and_sync_frames_decode_to_exact_expected_digests() {
    let mut decoded = decode_snapshot_any(BASE_SNAP).expect("frozen v2 base snapshot must decode");
    let replica1 = pack_replica_id(0, 1).unwrap();
    assert_eq!(decoded.watermarks.len(), 1);
    assert_eq!(decoded.watermarks[0].replica, replica1);

    let scan = scan_wal_any(REPLAY_WAL, 5, 8).expect("frozen v2 WAL must scan");
    replay_wal_records_v2(&mut decoded.engine, &scan.records, 0, true)
        .expect("frozen v2 WAL must replay cleanly");

    let wb = decoded.engine.workbook();
    let s0 = SheetId(0);
    // Peer updated A2 to 50.0, so SetCellsIf skipped clearing A2; A3 matched formula
    // ignoring cached result and overwrote to 999.0.
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(1), ColId(0)),
        Value::Number(50.0)
    );
    assert_eq!(
        wb.sheet(s0).unwrap().value_by_id(RowId(2), ColId(0)),
        Value::Number(999.0)
    );

    let collab_sheet = pack_sheet_id(0, replica1, 1).unwrap();
    assert!(wb.sheet(collab_sheet).is_some());

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

    // Decode all frozen binary sync frames from `sync_frames.bin`.
    let mut cursor = 0_usize;
    let mut count = 0_usize;
    while cursor < SYNC_FRAMES_BIN.len() {
        let len_bytes: [u8; 4] = SYNC_FRAMES_BIN[cursor..cursor + 4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        cursor += 4;
        let frame = decode_sync_frame(&SYNC_FRAMES_BIN[cursor..cursor + len])
            .expect("frozen sync frame must decode");
        assert!(matches!(
            frame,
            WireSyncFrameV1::Hello { .. }
                | WireSyncFrameV1::Welcome { .. }
                | WireSyncFrameV1::Push { .. }
                | WireSyncFrameV1::Ack { .. }
                | WireSyncFrameV1::Reject { .. }
                | WireSyncFrameV1::Commit { .. }
                | WireSyncFrameV1::Digest { .. }
                | WireSyncFrameV1::Presence(_)
                | WireSyncFrameV1::Ping(_)
                | WireSyncFrameV1::Pong(_)
        ));
        cursor += len;
        count += 1;
    }
    assert_eq!(count, 11);
}
