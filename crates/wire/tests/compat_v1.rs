//! Frozen `wire::v1` snapshot + WAL compatibility & replay gate (`D-020`,
//! `14-m1b.md` 5.5 & 8.1), executed on both native and `wasm32-unknown-unknown`.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use reticula_core::determinism::to_hex;
use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::value::Value;
use reticula_wire::snapshot::{SNAPSHOT_VERSION_V1, decode_snapshot_any};
use reticula_wire::wal::{WalTailDisposition, replay_wal_records, scan_wal};

const BASE_SNAP: &[u8] = include_bytes!("../../../tests/compat/v1/base.rtclsnap");
const REPLAY_WAL: &[u8] = include_bytes!("../../../tests/compat/v1/replay.rtclwal");
const EXPECTED_DIGEST: &str = include_str!("../../../tests/compat/v1/expected_digest.txt");

#[test]
fn frozen_v1_snapshot_and_wal_replay_to_exact_expected_digest() {
    let mut decoded = decode_snapshot_any(BASE_SNAP).expect("decode frozen v1 base.rtclsnap");
    assert_eq!(decoded.format_version, SNAPSHOT_VERSION_V1);
    assert_eq!(decoded.watermarks.len(), 1);
    assert_eq!(decoded.watermarks[0].replica, 1);
    assert_eq!(decoded.watermarks[0].last_client_seq, 10);
    assert_eq!(decoded.watermarks[0].max_id_counter, 50);

    let scan = scan_wal(REPLAY_WAL, 10, 16).expect("scan frozen v1 replay.rtclwal");
    assert_eq!(scan.tail, WalTailDisposition::Clean);
    assert_eq!(scan.records.len(), 6);
    assert_eq!(scan.last_seq(), 16);

    let applied = replay_wal_records(&mut decoded.engine, &scan.records, 0, true)
        .expect("replay frozen v1 WAL");
    assert_eq!(applied, 6);

    // Verify E25 range-anchor handoff took effect on `B1` (`RowId(0), ColId(1)`):
    // Originally `=SUM(A1:A5)` (`RowId(0)..=RowId(4)`), then `RowId(0)` moved down
    // after `RowId(3)`, so the top anchor handed off to the first staying live row!
    let b1 = decoded
        .engine
        .inspect_cell(SheetId(0), RowId(0), ColId(1))
        .unwrap();
    assert!(b1.is_formula);
    assert!(matches!(
        decoded
            .engine
            .workbook()
            .sheet(SheetId(0))
            .unwrap()
            .value_by_id(RowId(0), ColId(1)),
        Value::Number(_)
    ));

    let hex_bytes = to_hex(decoded.engine.workbook().logical_digest());
    let actual_hex = core::str::from_utf8(&hex_bytes).unwrap();
    assert_eq!(actual_hex, EXPECTED_DIGEST.trim());
}
