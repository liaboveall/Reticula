//! Frozen wire schema & persistence compatibility gate (`D-020`, `14-m1b.md` 5.5 & 8.1).
//!
//! `cargo xtask compat --freeze v1` generates deterministic `RTCLSNAP` v1 and
//! `RTCLWAL1` fixtures under `tests/compat/v1/` and records the SHA-256 hashes
//! of `crates/wire/src/v1.rs` and every fixture in `tests/compat/hashes.txt`.
//! `cargo xtask check` verifies those hashes on every commit and CI run so an
//! accidental breaking change to a frozen wire version is caught immediately.

use std::fs;
use std::path::Path;

use reticula_core::determinism::to_hex;
use reticula_core::format::{CurrencySymbol, NumberFormat};
use reticula_core::id::{ColId, IdRun, RowId, SheetId, pack_replica_id, pack_sheet_id};
use reticula_core::ops::{
    CellEdit, CellInput, ConditionalCellEdit, FormatPatch, FormatTarget, FormulaMode, Op,
    ScalarValue, Workbook,
};
use reticula_core::sheet::Sheet;
use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_formula::binder::{BoundExpr, BoundFormula, templatize_formula};
use reticula_formula::engine::FormulaEngine;
use reticula_wire::snapshot::{
    decode_snapshot_any, encode_snapshot_v1, encode_snapshot_with_permutes,
};
use reticula_wire::sync_frame::{
    decode_sync_frame, decode_sync_frame_any, decode_sync_frame_v2, encode_sync_frame,
    encode_sync_frame_v2,
};
use reticula_wire::v1::{WireOpV1, WireReplicaWatermarkV1, WireWalRecordV1};
use reticula_wire::v2::{
    SYNC_PROTOCOL_VERSION, WireCatchupV1, WireCommittedTxnV2, WireOpV2, WirePendingTxnV2,
    WirePresenceRangeV1, WirePresenceV1, WireRejectReasonV1, WireStateDigestV1, WireSyncFrameV1,
    WireTxnV2, WireWalRecordV2,
};
use reticula_wire::v3::{
    SYNC_PROTOCOL_VERSION_V2, WireCatchupV2, WireCommittedPermuteV3, WireCommittedTxnV3, WireOpV3,
    WirePendingTxnV3, WireSyncFrameV2, WireTxnV3, WireWalRecordV3,
};
use reticula_wire::wal::{
    encode_wal_complete, encode_wal_complete_v2, encode_wal_complete_v3, replay_wal_records,
    replay_wal_records_v2, replay_wal_records_v3, scan_wal, scan_wal_any, scan_wal_any_v3,
    scan_wal_v3,
};

use crate::util::{self, Flags, Result};

const HASHES_FILE: &str = "tests/compat/hashes.txt";

const FROZEN_V1_FILES: [&str; 4] = [
    "crates/wire/src/v1.rs",
    "tests/compat/v1/base.rtclsnap",
    "tests/compat/v1/expected_digest.txt",
    "tests/compat/v1/replay.rtclwal",
];

const FROZEN_V2_FILES: [&str; 5] = [
    "crates/wire/src/v2.rs",
    "tests/compat/v2/base.rtclsnap",
    "tests/compat/v2/expected_digest.txt",
    "tests/compat/v2/replay.rtclwal",
    "tests/compat/v2/sync_frames.bin",
];

const FROZEN_V3_FILES: [&str; 5] = [
    "crates/wire/src/v3.rs",
    "tests/compat/v3/base.rtclsnap",
    "tests/compat/v3/expected_digest.txt",
    "tests/compat/v3/replay.rtclwal",
    "tests/compat/v3/sync_frames.bin",
];

pub fn run(flags: &Flags) -> Result {
    if let Some(ver) = flags.get("freeze") {
        return match ver {
            "v1" => freeze_v1(),
            "v2" => freeze_v2(),
            "v3" => freeze_v3(),
            other => Err(format!(
                "unknown wire version `{other}` (supported: `v1`, `v2`, `v3`)"
            )),
        };
    }
    check()
}

pub fn check() -> Result {
    util::step("frozen wire compatibility hashes (v1 + v2 + v3)");
    let root = util::root();
    let hashes_path = root.join(HASHES_FILE);
    let content = fs::read_to_string(&hashes_path).map_err(|e| {
        format!(
            "cannot read {}: {e} (run `cargo xtask compat --freeze v1` first)",
            hashes_path.display()
        )
    })?;

    let mut entries = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((hex, rel_path)) = trimmed.split_once("  ") else {
            return Err(format!(
                "{}:{}: malformed hash entry `{trimmed}`",
                HASHES_FILE,
                idx + 1
            ));
        };
        entries.push((hex.trim().to_owned(), rel_path.trim().to_owned()));
    }

    for required in FROZEN_V1_FILES
        .iter()
        .chain(FROZEN_V2_FILES.iter())
        .chain(FROZEN_V3_FILES.iter())
    {
        if !entries.iter().any(|(_, p)| p == required) {
            return Err(format!(
                "{HASHES_FILE} is missing required frozen file `{required}`"
            ));
        }
    }

    for (expected_hex, rel_path) in &entries {
        let full_path = root.join(rel_path);
        let actual_hex = hash_repo_file(&full_path)?;
        if &actual_hex != expected_hex {
            return Err(format!(
                "SHA-256 mismatch on frozen wire file `{rel_path}`:\n  expected: {expected_hex}\n  actual:   {actual_hex}\nFrozen wire modules and fixtures are immutable (D-020); introduce a new wire version instead of editing `{rel_path}`."
            ));
        }
    }

    println!("{} frozen wire file(s) verified", entries.len());
    Ok(())
}

fn freeze_v1() -> Result {
    util::step("generating tests/compat/v1 fixtures and hashes.txt");
    let root = util::root();
    let dir = root.join("tests/compat/v1");
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    // 1. Construct deterministic 2-sheet base workbook and encode `base.rtclsnap`.
    let base_sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), 64, 12);
    let mut eng = FormulaEngine::from_workbook_clean(Workbook::from_sheet(base_sheet, None));
    let s0 = SheetId(0);
    let s1 = eng.alloc_sheet_id();
    eng.apply_op(Op::AddSheet {
        sheet: s1,
        name: String::from("Rates"),
        after: Some(s0),
    })
    .map_err(|e| format!("AddSheet: {e:?}"))?;

    for (r, raw) in ["10", "20", "30", "40", "50"].iter().enumerate() {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(0), raw)
            .map_err(|e| format!("seed A: {e:?}"))?;
    }
    eng.edit_cell_raw(s0, RowId(0), ColId(1), "=SUM(A1:A5)")
        .map_err(|e| format!("seed B1: {e:?}"))?;

    for (r, (sku, rate)) in [("SKU-A", "15.5"), ("SKU-B", "28.0"), ("SKU-C", "42.25")]
        .iter()
        .enumerate()
    {
        eng.edit_cell_raw(s1, RowId(r as u64), ColId(0), &format!("'{sku}"))
            .map_err(|e| format!("Rates sku: {e:?}"))?;
        eng.edit_cell_raw(s1, RowId(r as u64), ColId(1), rate)
            .map_err(|e| format!("Rates val: {e:?}"))?;
    }
    eng.edit_cell_raw(
        s0,
        RowId(1),
        ColId(1),
        "=VLOOKUP(\"SKU-B\",Rates!A1:B3,2,FALSE)",
    )
    .map_err(|e| format!("seed B2: {e:?}"))?;
    eng.apply_op(Op::SetFreeze {
        sheet: s0,
        frozen_rows: 1,
        frozen_cols: 1,
    })
    .map_err(|e| format!("SetFreeze: {e:?}"))?;
    eng.recalc_all();

    let watermarks = vec![WireReplicaWatermarkV1 {
        replica: 1,
        last_client_seq: 10,
        max_id_counter: 50,
    }];
    let snap_bytes = encode_snapshot_v1(eng.workbook(), &watermarks)
        .map_err(|e| format!("encode_snapshot_v1: {e:?}"))?;

    // 2. Build 6 WAL transactions exercising InsertRows, DeleteRows, MoveRows
    //    (E25 anchor handoff on `=SUM(A1:A5)`!), FillFormula, SetFormat/Resize,
    //    and cross-sheet edit.
    let mut replay_eng = decode_snapshot_any(&snap_bytes)
        .map_err(|e| format!("decode base: {e:?}"))?
        .engine;

    let mut wal_records = Vec::new();
    let ins_run = replay_eng.allocator_mut().alloc_run(2).unwrap();
    let txns: Vec<(&str, Vec<Op<BoundFormula>>)> = {
        let op1 = vec![
            Op::InsertRows {
                sheet: s0,
                after: Some(RowId(1)),
                ids: ins_run,
            },
            Op::SetCells {
                sheet: s0,
                edits: vec![CellEdit {
                    row: RowId(ins_run.first_id),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(99.0)),
                    format: FormatPatch::Keep,
                }],
            },
        ];
        let op2 = vec![Op::DeleteRows {
            sheet: s0,
            ids: vec![IdRun::new(2, 1).unwrap()],
        }];
        // Move top anchor `RowId(0)` downward into the interior of `A1:A5` -> E25 handoff!
        let op3 = vec![Op::MoveRows {
            sheet: s0,
            ids: vec![IdRun::new(0, 1).unwrap()],
            after: Some(RowId(3)),
        }];
        let tpl_base = BoundFormula::parse_and_bind(
            "=A1*10+$B$2",
            FormulaMode::Dynamic,
            replay_eng.workbook(),
            s0,
        )
        .unwrap();
        let tpl = templatize_formula(&tpl_base, RowId(0), ColId(2));
        let op4 = vec![Op::FillFormula {
            sheet: s0,
            rows: vec![IdRun::new(5, 8).unwrap()],
            cols: vec![IdRun::new(2, 1).unwrap()],
            template: tpl,
        }];
        let op5 = vec![
            Op::SetFormat {
                sheet: s0,
                target: FormatTarget::Cols(vec![IdRun::new(2, 1).unwrap()]),
                number_format: FormatPatch::Set(NumberFormat::Currency {
                    symbol: CurrencySymbol::Usd,
                    decimals: 2,
                }),
                style: StylePatch {
                    bold: Some(true),
                    align: Some(HorizontalAlign::Right),
                    fill_color: ColorPatch::Set(ColorValue::Palette(3)),
                    border_bottom: Some(BorderWeight::Thin),
                    ..StylePatch::default()
                },
            },
            Op::ResizeRows {
                sheet: s0,
                sizes: vec![(IdRun::new(0, 1).unwrap(), Some(32))],
            },
        ];
        let cross_f = BoundFormula::parse_and_bind(
            "=B1+Rates!B3",
            FormulaMode::Dynamic,
            replay_eng.workbook(),
            s0,
        )
        .unwrap();
        let op6 = vec![Op::SetCells {
            sheet: s0,
            edits: vec![CellEdit {
                row: RowId(2),
                col: ColId(1),
                input: CellInput::Formula {
                    formula: cross_f,
                    cached: None,
                },
                format: FormatPatch::Keep,
            }],
        }];
        vec![
            ("Insert 2 rows", op1),
            ("Delete row", op2),
            ("Move row with E25 handoff", op3),
            ("Fill formula C6:C13", op4),
            ("Format column C & resize row", op5),
            ("Add cross-sheet formula", op6),
        ]
    };

    for (idx, (label, ops)) in txns.into_iter().enumerate() {
        let seq = 11 + (idx as u64);
        let wire_ops: Vec<WireOpV1> = ops
            .iter()
            .map(|op| WireOpV1::try_from(op).map_err(|e| format!("WireOpV1::try_from: {e:?}")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        wal_records.push(WireWalRecordV1 {
            seq,
            timestamp_ms: 1_763_500_000_000 + seq,
            label: String::from(label),
            ops: wire_ops,
        });
    }

    let wal_bytes =
        encode_wal_complete(10, &wal_records).map_err(|e| format!("encode_wal_complete: {e:?}"))?;

    // Verify cold decode + WAL scan + replay produces deterministic digest.
    let mut verify_eng = decode_snapshot_any(&snap_bytes)
        .map_err(|e| format!("verify decode: {e:?}"))?
        .engine;
    let scan = scan_wal(&wal_bytes, 10, 16).map_err(|e| format!("verify scan_wal: {e:?}"))?;
    replay_wal_records(&mut verify_eng, &scan.records, 0, true)
        .map_err(|e| format!("verify replay: {e:?}"))?;
    let digest_hex_bytes = to_hex(verify_eng.workbook().logical_digest());
    let digest_hex = String::from_utf8_lossy(&digest_hex_bytes).into_owned();

    fs::write(dir.join("base.rtclsnap"), &snap_bytes)
        .map_err(|e| format!("write base.rtclsnap: {e}"))?;
    fs::write(dir.join("replay.rtclwal"), &wal_bytes)
        .map_err(|e| format!("write replay.rtclwal: {e}"))?;
    fs::write(dir.join("expected_digest.txt"), format!("{digest_hex}\n"))
        .map_err(|e| format!("write expected_digest.txt: {e}"))?;

    let mut hash_lines = String::from(
        "# Frozen SHA-256 manifest for wire v1 schema and compatibility fixtures (D-020).\n",
    );
    for rel in FROZEN_V1_FILES {
        let h = hash_repo_file(&root.join(rel))?;
        hash_lines.push_str(&format!("{h}  {rel}\n"));
    }
    fs::write(root.join(HASHES_FILE), hash_lines)
        .map_err(|e| format!("write {HASHES_FILE}: {e}"))?;

    println!(
        "wrote tests/compat/v1/ (base={}B, wal={}B, expected_digest={digest_hex})",
        snap_bytes.len(),
        wal_bytes.len()
    );
    Ok(())
}

fn freeze_v2() -> Result {
    util::step("generating tests/compat/v2 fixtures and updating hashes.txt");
    let root = util::root();
    let hashes_path = root.join(HASHES_FILE);
    let existing_hashes = fs::read_to_string(&hashes_path).map_err(|e| {
        format!(
            "cannot read {}: {e} (v1 must be frozen before freezing v2)",
            hashes_path.display()
        )
    })?;

    // Collect existing v1 block untouched and verify every v1 SHA-256 still matches.
    let mut v1_section = String::new();
    for line in existing_hashes.lines() {
        let trimmed = line.trim();
        if trimmed.contains("wire v2") || FROZEN_V2_FILES.iter().any(|p| trimmed.ends_with(p)) {
            continue;
        }
        if let Some((hex, rel)) = trimmed.split_once("  ")
            && FROZEN_V1_FILES.contains(&rel.trim())
        {
            let actual = hash_repo_file(&root.join(rel.trim()))?;
            if actual != hex.trim() {
                return Err(format!(
                    "v1 frozen file `{}` hash drifted before freeze v2 (expected {}, got {actual})",
                    rel.trim(),
                    hex.trim()
                ));
            }
        }
        v1_section.push_str(line);
        v1_section.push('\n');
    }

    let dir = root.join("tests/compat/v2");
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let s0 = SheetId(0);
    let replica1 = pack_replica_id(0, 1).ok_or_else(|| "pack_replica_id failed".to_owned())?;
    let base_sheet = Sheet::with_axes(s0, String::from("Sheet1"), 32, 8);
    let mut eng = FormulaEngine::from_workbook_clean(Workbook::from_sheet(base_sheet, None));
    eng.edit_cell_raw(s0, RowId(0), ColId(0), "100")
        .map_err(|e| format!("seed A1: {e:?}"))?;
    eng.edit_cell_raw(s0, RowId(1), ColId(0), "25")
        .map_err(|e| format!("seed A2: {e:?}"))?;
    eng.edit_cell_raw(s0, RowId(2), ColId(0), "=A1+A2")
        .map_err(|e| format!("seed A3: {e:?}"))?;
    eng.recalc_all();

    let watermarks = [WireReplicaWatermarkV1 {
        replica: replica1,
        last_client_seq: 5,
        max_id_counter: 1,
    }];
    let snap_bytes = encode_snapshot_v1(eng.workbook(), &watermarks)
        .map_err(|e| format!("encode_snapshot_v1: {e:?}"))?;

    let collab_sheet =
        pack_sheet_id(0, replica1, 1).ok_or_else(|| "pack_sheet_id failed".to_owned())?;
    let f_a3 = eng
        .workbook()
        .formula(s0, RowId(2), ColId(0))
        .map(std::borrow::Cow::into_owned)
        .ok_or_else(|| "missing formula A3".to_owned())?;

    let peer_edit_a2 = Op::SetCells {
        sheet: s0,
        edits: vec![CellEdit {
            row: RowId(1),
            col: ColId(0),
            input: CellInput::Scalar(ScalarValue::Number(50.0)),
            format: FormatPatch::Set(NumberFormat::Fixed2),
        }],
    };
    // Undo expects A2=25 (stale -> skipped!) and A3=A1+A2 with stale cached: None (matches!).
    let cond_undo = Op::SetCellsIf {
        sheet: s0,
        edits: vec![
            ConditionalCellEdit {
                row: RowId(1),
                col: ColId(0),
                expected_input: CellInput::Scalar(ScalarValue::Number(25.0)),
                expected_format: None,
                new_input: CellInput::Empty,
                new_format: FormatPatch::Keep,
            },
            ConditionalCellEdit {
                row: RowId(2),
                col: ColId(0),
                expected_input: CellInput::Formula {
                    formula: f_a3,
                    cached: None,
                },
                expected_format: None,
                new_input: CellInput::Scalar(ScalarValue::Number(999.0)),
                new_format: FormatPatch::Set(NumberFormat::Integer),
            },
        ],
    };
    let add_collab_sheet = Op::AddSheet {
        sheet: collab_sheet,
        name: String::from("Collab"),
        after: Some(s0),
    };
    let set_collab_cell = Op::SetCells {
        sheet: collab_sheet,
        edits: vec![CellEdit {
            row: RowId(0),
            col: ColId(0),
            input: CellInput::Scalar(ScalarValue::Text(String::from("synced"))),
            format: FormatPatch::Keep,
        }],
    };

    let wal_records = vec![
        WireWalRecordV2 {
            seq: 6,
            timestamp_ms: 1_763_600_000_006,
            label: String::from("Peer update A2"),
            ops: vec![WireOpV2::try_from(&peer_edit_a2).map_err(|e| format!("WireOpV2: {e:?}"))?],
        },
        WireWalRecordV2 {
            seq: 7,
            timestamp_ms: 1_763_600_000_007,
            label: String::from("Conditional undo"),
            ops: vec![WireOpV2::try_from(&cond_undo).map_err(|e| format!("WireOpV2: {e:?}"))?],
        },
        WireWalRecordV2 {
            seq: 8,
            timestamp_ms: 1_763_600_000_008,
            label: String::from("Add collaborative sheet"),
            ops: vec![
                WireOpV2::try_from(&add_collab_sheet).map_err(|e| format!("WireOpV2: {e:?}"))?,
                WireOpV2::try_from(&set_collab_cell).map_err(|e| format!("WireOpV2: {e:?}"))?,
            ],
        },
    ];
    let wal_bytes = encode_wal_complete_v2(5, &wal_records)
        .map_err(|e| format!("encode_wal_complete_v2: {e:?}"))?;

    let mut verify_eng = decode_snapshot_any(&snap_bytes)
        .map_err(|e| format!("verify decode: {e:?}"))?
        .engine;
    let scan = scan_wal_any(&wal_bytes, 5, 8).map_err(|e| format!("verify scan_wal_any: {e:?}"))?;
    replay_wal_records_v2(&mut verify_eng, &scan.records, 0, true)
        .map_err(|e| format!("verify replay v2: {e:?}"))?;

    let logical_hex =
        String::from_utf8_lossy(&to_hex(verify_eng.workbook().logical_digest())).into_owned();
    let sd = verify_eng.workbook().state_digest();
    let lo_hex = String::from_utf8_lossy(&to_hex(sd.content_lo)).into_owned();
    let hi_hex = String::from_utf8_lossy(&to_hex(sd.content_hi)).into_owned();
    let ord_hex = String::from_utf8_lossy(&to_hex(sd.order)).into_owned();
    let expected_txt = format!(
        "{logical_hex}\nv{}:{lo_hex}:{hi_hex}:{ord_hex}\n",
        sd.version
    );

    let frames = vec![
        WireSyncFrameV1::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION,
            doc_id: [0x11; 16],
            branch_id: [0_u8; 16],
            replica: Some(replica1),
            last_applied_seq: 5,
            known_epoch: 0,
        },
        WireSyncFrameV1::Welcome {
            epoch: 0,
            head_seq: 8,
            replica: replica1,
            last_client_seq: 5,
            max_id_counter: 1,
            catchup: WireCatchupV1::Txns {
                from_seq: 6,
                txns: vec![WireCommittedTxnV2 {
                    seq: 6,
                    replica: replica1,
                    client_seq: 6,
                    user_id: [0x22; 16],
                    timestamp_ms: 1_763_600_000_006,
                    txn: WireTxnV2 {
                        label: String::from("Peer"),
                        ops: vec![
                            WireOpV2::try_from(&peer_edit_a2)
                                .map_err(|e| format!("WireOpV2: {e:?}"))?,
                        ],
                    },
                }],
            },
        },
        WireSyncFrameV1::Welcome {
            epoch: 1,
            head_seq: 8,
            replica: pack_replica_id(1, 2).unwrap_or(1),
            last_client_seq: 0,
            max_id_counter: 0,
            catchup: WireCatchupV1::SnapshotRequired {
                snapshot_seq: 5,
                url: String::from("/api/workbooks/11111111/snapshots/5"),
            },
        },
        WireSyncFrameV1::Push {
            txns: vec![WirePendingTxnV2 {
                replica: replica1,
                client_seq: 6,
                base_server_seq: 5,
                timestamp_ms: 1_763_600_000_007,
                txn: WireTxnV2 {
                    label: String::from("Conditional undo"),
                    ops: vec![
                        WireOpV2::try_from(&cond_undo).map_err(|e| format!("WireOpV2: {e:?}"))?,
                    ],
                },
            }],
        },
        WireSyncFrameV1::Ack {
            replica: replica1,
            client_seq: 6,
            seq: 7,
            timestamp_ms: 1_763_600_000_007,
        },
        WireSyncFrameV1::Reject {
            client_seq: 7,
            reason: WireRejectReasonV1::NonContiguousClientSeq { expected: 6 },
        },
        WireSyncFrameV1::Commit {
            seq: 8,
            replica: replica1,
            client_seq: 7,
            user_id: [0x22; 16],
            timestamp_ms: 1_763_600_000_008,
            txn: WireTxnV2 {
                label: String::from("Add collaborative sheet"),
                ops: vec![
                    WireOpV2::try_from(&add_collab_sheet)
                        .map_err(|e| format!("WireOpV2: {e:?}"))?,
                ],
            },
        },
        WireSyncFrameV1::Digest {
            seq: 8,
            state_digest: WireStateDigestV1::from(sd),
        },
        WireSyncFrameV1::Presence(WirePresenceV1 {
            replica: replica1,
            user_id: [0x22; 16],
            display_name: String::from("Alice"),
            color_slot: 2,
            sheet_id: s0.0,
            cursor_row: 1,
            cursor_col: 0,
            selection: Some(WirePresenceRangeV1 {
                start_row: 0,
                start_col: 0,
                end_row: 2,
                end_col: 0,
            }),
        }),
        WireSyncFrameV1::Ping(12345),
        WireSyncFrameV1::Pong(12345),
    ];

    let mut sync_bin = Vec::new();
    for frame in &frames {
        let packet = encode_sync_frame(frame).map_err(|e| format!("encode_sync_frame: {e:?}"))?;
        let roundtrip =
            decode_sync_frame(&packet).map_err(|e| format!("decode_sync_frame: {e:?}"))?;
        if &roundtrip != frame {
            return Err("sync frame round-trip mismatch".to_owned());
        }
        let len_u32 = u32::try_from(packet.len()).map_err(|_| "frame len overflow".to_owned())?;
        sync_bin.extend_from_slice(&len_u32.to_le_bytes());
        sync_bin.extend_from_slice(&packet);
    }

    fs::write(dir.join("base.rtclsnap"), &snap_bytes)
        .map_err(|e| format!("write v2 base.rtclsnap: {e}"))?;
    fs::write(dir.join("replay.rtclwal"), &wal_bytes)
        .map_err(|e| format!("write v2 replay.rtclwal: {e}"))?;
    fs::write(dir.join("expected_digest.txt"), &expected_txt)
        .map_err(|e| format!("write v2 expected_digest.txt: {e}"))?;
    fs::write(dir.join("sync_frames.bin"), &sync_bin)
        .map_err(|e| format!("write v2 sync_frames.bin: {e}"))?;

    let mut out_hashes = v1_section;
    out_hashes.push_str(
        "# Frozen SHA-256 manifest for wire v2 schema, sync protocol v1 frames and fixtures (D-020).\n",
    );
    for rel in FROZEN_V2_FILES {
        let h = hash_repo_file(&root.join(rel))?;
        out_hashes.push_str(&format!("{h}  {rel}\n"));
    }
    fs::write(&hashes_path, out_hashes).map_err(|e| format!("write {HASHES_FILE}: {e}"))?;

    println!(
        "wrote tests/compat/v2/ (base={}B, wal={}B, sync_frames={}B, digest={logical_hex})",
        snap_bytes.len(),
        wal_bytes.len(),
        sync_bin.len()
    );
    Ok(())
}

fn freeze_v3() -> Result {
    util::step("generating tests/compat/v3 fixtures and updating hashes.txt");
    let root = util::root();
    let hashes_path = root.join(HASHES_FILE);
    let existing_hashes = fs::read_to_string(&hashes_path).map_err(|e| {
        format!(
            "cannot read {}: {e} (v1 and v2 must be frozen before freezing v3)",
            hashes_path.display()
        )
    })?;

    let mut preserved_section = String::new();
    for line in existing_hashes.lines() {
        let trimmed = line.trim();
        if trimmed.contains("wire v3") || FROZEN_V3_FILES.iter().any(|p| trimmed.ends_with(p)) {
            continue;
        }
        if let Some((hex, rel)) = trimmed.split_once("  ")
            && (FROZEN_V1_FILES.contains(&rel.trim()) || FROZEN_V2_FILES.contains(&rel.trim()))
        {
            let actual = hash_repo_file(&root.join(rel.trim()))?;
            if actual != hex.trim() {
                return Err(format!(
                    "frozen file `{}` hash drifted before freeze v3 (expected {}, got {actual})",
                    rel.trim(),
                    hex.trim()
                ));
            }
        }
        preserved_section.push_str(line);
        preserved_section.push('\n');
    }

    let dir = root.join("tests/compat/v3");
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let s0 = SheetId(0);
    let replica1 = pack_replica_id(0, 1).ok_or_else(|| "pack_replica_id failed".to_owned())?;
    let base_sheet = Sheet::with_axes(s0, String::from("Sheet1"), 32, 8);
    let mut eng = FormulaEngine::from_workbook_clean(Workbook::from_sheet(base_sheet, None));
    for (r, raw) in ["10", "20", "30", "40"].iter().enumerate() {
        eng.edit_cell_raw(s0, RowId(r as u64), ColId(0), raw)
            .map_err(|e| format!("seed A: {e:?}"))?;
    }
    let f_rate = BoundFormula::parse_and_bind("=0.15", FormulaMode::Dynamic, eng.workbook(), s0)
        .map_err(|e| format!("Rate formula: {e:?}"))?;
    let f_local_max =
        BoundFormula::parse_and_bind("=MAX(A1:A4)", FormulaMode::Dynamic, eng.workbook(), s0)
            .map_err(|e| format!("LocalMax formula: {e:?}"))?;
    eng.apply_op(Op::DefineNamedRange {
        scope: None,
        name: String::from("Rate"),
        formula: f_rate,
        comment: Some(String::from("Workbook rate")),
        hidden: false,
    })
    .map_err(|e| format!("DefineNamedRange Rate: {e:?}"))?;
    eng.apply_op(Op::DefineNamedRange {
        scope: Some(s0),
        name: String::from("LocalMax"),
        formula: f_local_max,
        comment: None,
        hidden: true,
    })
    .map_err(|e| format!("DefineNamedRange LocalMax: {e:?}"))?;

    eng.edit_cell_raw(s0, RowId(0), ColId(1), "=SEQUENCE(3,1,1,1)")
        .map_err(|e| format!("seed B1: {e:?}"))?;
    eng.edit_cell_raw(s0, RowId(1), ColId(1), "=SUM(B1#)")
        .map_err(|e| format!("seed B2: {e:?}"))?;
    eng.edit_cell_raw(s0, RowId(0), ColId(2), "=Rate")
        .map_err(|e| format!("seed C1: {e:?}"))?;
    eng.edit_cell_raw(s0, RowId(1), ColId(2), "=Sheet1!LocalMax")
        .map_err(|e| format!("seed C2: {e:?}"))?;
    eng.edit_cell_raw(s0, RowId(0), ColId(3), "=IF(A1>0,,99)")
        .map_err(|e| format!("seed D1: {e:?}"))?;
    eng.apply_op(Op::SetCells {
        sheet: s0,
        edits: vec![
            CellEdit {
                row: RowId(1),
                col: ColId(3),
                input: CellInput::Formula {
                    formula: BoundFormula {
                        mode: FormulaMode::Dynamic,
                        root: BoundExpr::PreservedCached {
                            raw: String::from("=FUTURE_FN(1)"),
                            cached: ScalarValue::Number(42.5),
                        },
                    },
                    cached: Some(ScalarValue::Number(42.5)),
                },
                format: FormatPatch::Keep,
            },
            CellEdit {
                row: RowId(2),
                col: ColId(3),
                input: CellInput::Formula {
                    formula: BoundFormula {
                        mode: FormulaMode::Dynamic,
                        root: BoundExpr::Opaque {
                            raw: String::from("=Table1[Amount]"),
                        },
                    },
                    cached: Some(ScalarValue::Number(7.5)),
                },
                format: FormatPatch::Keep,
            },
        ],
    })
    .map_err(|e| format!("seed D2/D3: {e:?}"))?;
    eng.recalc_all();

    let watermarks = [WireReplicaWatermarkV1 {
        replica: replica1,
        last_client_seq: 4,
        max_id_counter: 1,
    }];
    let permutes = [WireCommittedPermuteV3 {
        seq: 4,
        replica: replica1,
        client_seq: 4,
        sheet: s0.0,
        cols: vec![0],
        row_pairs: vec![(0, 1), (1, 0)],
    }];
    let snap_bytes = encode_snapshot_with_permutes(eng.workbook(), &watermarks, &permutes)
        .map_err(|e| format!("encode_snapshot_with_permutes: {e:?}"))?;

    let permute_op = Op::PermuteContent {
        sheet: s0,
        rows: vec![RowId(0), RowId(1), RowId(2)],
        cols: vec![ColId(0)],
        row_perm: vec![2, 0, 1],
    };
    let move_op = Op::MoveCells {
        src_sheet: s0,
        src_rows: vec![RowId(3)],
        src_cols: vec![ColId(0)],
        dst_sheet: s0,
        dst_rows: vec![RowId(3)],
        dst_cols: vec![ColId(2)],
    };
    let f_tax = BoundFormula::parse_and_bind("=C4*0.2", FormulaMode::Dynamic, eng.workbook(), s0)
        .map_err(|e| format!("Tax formula: {e:?}"))?;
    let define_tax_op = Op::DefineNamedRange {
        scope: Some(s0),
        name: String::from("Tax"),
        formula: f_tax,
        comment: Some(String::from("Local tax")),
        hidden: false,
    };
    let f_spill = BoundFormula::parse_and_bind("=B1#", FormulaMode::Dynamic, eng.workbook(), s0)
        .map_err(|e| format!("Spill formula: {e:?}"))?;
    let delete_and_spill_ops = [
        Op::DeleteNamedRange {
            scope: None,
            name: String::from("Rate"),
        },
        Op::SetCells {
            sheet: s0,
            edits: vec![CellEdit {
                row: RowId(0),
                col: ColId(4),
                input: CellInput::Formula {
                    formula: f_spill,
                    cached: None,
                },
                format: FormatPatch::Set(NumberFormat::Fixed2),
            }],
        },
    ];

    let wal_records = vec![
        WireWalRecordV3 {
            seq: 5,
            timestamp_ms: 1_765_000_000_005,
            label: String::from("Sort A1:A3"),
            ops: vec![WireOpV3::try_from(&permute_op).map_err(|e| format!("WireOpV3: {e:?}"))?],
        },
        WireWalRecordV3 {
            seq: 6,
            timestamp_ms: 1_765_000_000_006,
            label: String::from("Move A4 to C4"),
            ops: vec![WireOpV3::try_from(&move_op).map_err(|e| format!("WireOpV3: {e:?}"))?],
        },
        WireWalRecordV3 {
            seq: 7,
            timestamp_ms: 1_765_000_000_007,
            label: String::from("Define Tax"),
            ops: vec![WireOpV3::try_from(&define_tax_op).map_err(|e| format!("WireOpV3: {e:?}"))?],
        },
        WireWalRecordV3 {
            seq: 8,
            timestamp_ms: 1_765_000_000_008,
            label: String::from("Delete Rate & spill ref"),
            ops: delete_and_spill_ops
                .iter()
                .map(WireOpV3::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| format!("WireOpV3: {e:?}"))?,
        },
    ];
    let wal_bytes = encode_wal_complete_v3(4, &wal_records)
        .map_err(|e| format!("encode_wal_complete_v3: {e:?}"))?;

    let mut verify_decoded =
        decode_snapshot_any(&snap_bytes).map_err(|e| format!("decode v3 snap: {e:?}"))?;
    let scan = scan_wal_v3(&wal_bytes, 4, 8).map_err(|e| format!("scan_wal_v3: {e:?}"))?;
    let scan_any =
        scan_wal_any_v3(&wal_bytes, 4, 8).map_err(|e| format!("scan_wal_any_v3: {e:?}"))?;
    if scan_any != scan {
        return Err("scan_wal_any_v3 mismatch on v3 fixture".to_owned());
    }
    replay_wal_records_v3(&mut verify_decoded.engine, &scan.records, 0, true)
        .map_err(|e| format!("replay_wal_records_v3: {e:?}"))?;

    let logical_hex =
        String::from_utf8_lossy(&to_hex(verify_decoded.engine.workbook().logical_digest()))
            .into_owned();
    let sd = verify_decoded.engine.workbook().state_digest();
    let lo_hex = String::from_utf8_lossy(&to_hex(sd.content_lo)).into_owned();
    let hi_hex = String::from_utf8_lossy(&to_hex(sd.content_hi)).into_owned();
    let ord_hex = String::from_utf8_lossy(&to_hex(sd.order)).into_owned();
    let expected_txt = format!(
        "{logical_hex}\nv{}:{lo_hex}:{hi_hex}:{ord_hex}\n",
        sd.version
    );

    let frames = vec![
        WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: [0x33; 16],
            branch_id: [0_u8; 16],
            replica: Some(replica1),
            last_applied_seq: 4,
            known_epoch: 0,
        },
        WireSyncFrameV2::Welcome {
            epoch: 0,
            head_seq: 8,
            replica: replica1,
            last_client_seq: 4,
            max_id_counter: 1,
            catchup: WireCatchupV2::Txns {
                from_seq: 5,
                txns: vec![WireCommittedTxnV3 {
                    seq: 5,
                    replica: replica1,
                    client_seq: 5,
                    user_id: [0x44; 16],
                    timestamp_ms: 1_765_000_000_005,
                    txn: WireTxnV3 {
                        label: String::from("Sort A1:A3"),
                        ops: vec![
                            WireOpV3::try_from(&permute_op)
                                .map_err(|e| format!("WireOpV3: {e:?}"))?,
                        ],
                    },
                }],
            },
        },
        WireSyncFrameV2::Welcome {
            epoch: 1,
            head_seq: 8,
            replica: pack_replica_id(1, 2).unwrap_or(1),
            last_client_seq: 0,
            max_id_counter: 0,
            catchup: WireCatchupV2::SnapshotRequired {
                snapshot_seq: 4,
                url: String::from("/api/workbooks/33333333/snapshots/4"),
            },
        },
        WireSyncFrameV2::Push {
            txns: vec![WirePendingTxnV3 {
                replica: replica1,
                client_seq: 6,
                base_server_seq: 5,
                timestamp_ms: 1_765_000_000_006,
                txn: WireTxnV3 {
                    label: String::from("Move A4 to C4"),
                    ops: vec![
                        WireOpV3::try_from(&move_op).map_err(|e| format!("WireOpV3: {e:?}"))?,
                    ],
                },
            }],
        },
        WireSyncFrameV2::Ack {
            replica: replica1,
            client_seq: 6,
            seq: 6,
            timestamp_ms: 1_765_000_000_006,
        },
        WireSyncFrameV2::Reject {
            client_seq: 7,
            reason: WireRejectReasonV1::UpgradeRequired,
        },
        WireSyncFrameV2::Commit {
            seq: 7,
            replica: replica1,
            client_seq: 7,
            user_id: [0x44; 16],
            timestamp_ms: 1_765_000_000_007,
            txn: WireTxnV3 {
                label: String::from("Define Tax"),
                ops: vec![
                    WireOpV3::try_from(&define_tax_op).map_err(|e| format!("WireOpV3: {e:?}"))?,
                ],
            },
        },
        WireSyncFrameV2::Digest {
            seq: 8,
            state_digest: WireStateDigestV1::from(sd),
        },
        WireSyncFrameV2::Presence(WirePresenceV1 {
            replica: replica1,
            user_id: [0x44; 16],
            display_name: String::from("Alice"),
            color_slot: 1,
            sheet_id: s0.0,
            cursor_row: 0,
            cursor_col: 4,
            selection: Some(WirePresenceRangeV1 {
                start_row: 0,
                start_col: 0,
                end_row: 2,
                end_col: 4,
            }),
        }),
        WireSyncFrameV2::Ping(67890),
        WireSyncFrameV2::Pong(67890),
    ];

    let mut sync_bin = Vec::new();
    for frame in &frames {
        let packet =
            encode_sync_frame_v2(frame).map_err(|e| format!("encode_sync_frame_v2: {e:?}"))?;
        let roundtrip =
            decode_sync_frame_v2(&packet).map_err(|e| format!("decode_sync_frame_v2: {e:?}"))?;
        if &roundtrip != frame {
            return Err("sync v2 frame round-trip mismatch".to_owned());
        }
        let (_, any_rt) =
            decode_sync_frame_any(&packet).map_err(|e| format!("decode_sync_frame_any: {e:?}"))?;
        if &any_rt != frame {
            return Err("sync v2 any decode mismatch".to_owned());
        }
        let len_u32 = u32::try_from(packet.len()).map_err(|_| "frame len overflow".to_owned())?;
        sync_bin.extend_from_slice(&len_u32.to_le_bytes());
        sync_bin.extend_from_slice(&packet);
    }

    fs::write(dir.join("base.rtclsnap"), &snap_bytes)
        .map_err(|e| format!("write v3 base.rtclsnap: {e}"))?;
    fs::write(dir.join("replay.rtclwal"), &wal_bytes)
        .map_err(|e| format!("write v3 replay.rtclwal: {e}"))?;
    fs::write(dir.join("expected_digest.txt"), &expected_txt)
        .map_err(|e| format!("write v3 expected_digest.txt: {e}"))?;
    fs::write(dir.join("sync_frames.bin"), &sync_bin)
        .map_err(|e| format!("write v3 sync_frames.bin: {e}"))?;

    let mut out_hashes = preserved_section;
    out_hashes.push_str(
        "# Frozen SHA-256 manifest for wire v3 schema, sync protocol v2 frames and fixtures (D-020).\n",
    );
    for rel in FROZEN_V3_FILES {
        let h = hash_repo_file(&root.join(rel))?;
        out_hashes.push_str(&format!("{h}  {rel}\n"));
    }
    fs::write(&hashes_path, out_hashes).map_err(|e| format!("write {HASHES_FILE}: {e}"))?;

    println!(
        "wrote tests/compat/v3/ (base={}B, wal={}B, sync_frames={}B, digest={logical_hex})",
        snap_bytes.len(),
        wal_bytes.len(),
        sync_bin.len()
    );
    Ok(())
}

fn hash_repo_file(path: &Path) -> Result<String> {
    let raw = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let is_text = path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| matches!(ext, "rs" | "txt"));
    if is_text {
        let normalised: Vec<u8> = raw.into_iter().filter(|&b| b != b'\r').collect();
        Ok(sha256_hex(&normalised))
    } else {
        Ok(sha256_hex(&raw))
    }
}

// ------------------------------------------------------------ FIPS 180-4 SHA-256

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0_u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K256[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{word:08x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn fips_180_4_sha256_test_vectors_hold() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
