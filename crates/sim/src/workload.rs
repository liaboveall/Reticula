//! Deterministic event-script and operation workload generation for `reticula-sim`.
//!
//! Uses only [`reticula_core::rng::Rng`] (never `rand` or `HashMap`/`HashSet`) so
//! every `u64` seed drives identical work on all host platforms.

use std::collections::BTreeSet;

use reticula_core::axis::Axis;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId, pack_id, pack_sheet_id};
use reticula_core::ops::{
    CellEdit, CellInput, ConditionalCellEdit, FormatPatch, FormatTarget, FormulaMode, Op,
    ScalarValue, Workbook,
};
use reticula_core::rng::Rng;
use reticula_core::style::StylePatch;
use reticula_core::value::CalcError;
use reticula_formula::binder::{BoundFormula, templatize_formula};
use reticula_sync::ClientSyncMachine;
use reticula_wire::v3::{
    SYNC_PROTOCOL_VERSION_V2, WireOpV3, WirePendingTxnV3, WireSyncFrameV2, WireTxnV3,
};

/// Fixed simulated collaborator count:
/// - `0`: Alice Tab 1 (`USER_A`, Owner)
/// - `1`: Alice Tab 2 (`USER_A`, Owner) — two-tab concurrency
/// - `2`: Bob (`USER_B`, Editor, revocable/downgradeable in permission tests)
/// - `3`: Carol (`USER_C`, Editor)
/// - `4`: Dave (`USER_D`, permanent Viewer with sentinel replica `0`)
pub const NUM_CLIENTS: usize = 5;

/// Simulated document UUID.
pub const SIM_DOC_ID: [u8; 16] = [0xD0; 16];
/// Simulated branch UUID.
pub const SIM_BRANCH_ID: [u8; 16] = [0xB1; 16];

/// Member UUIDs indexed cleanly by collaborator identity.
pub const USER_A: [u8; 16] = [0xA1; 16];
pub const USER_B: [u8; 16] = [0xB2; 16];
pub const USER_C: [u8; 16] = [0xC3; 16];
pub const USER_D: [u8; 16] = [0xD4; 16];
pub const ROGUE_OUTSIDER: [u8; 16] = [0xEE; 16];

/// Every M3a operation family exercised in deterministic simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpChoice {
    SetCellsScalar,
    SetCellsFormula,
    SetCellsIfMatching,
    SetCellsIfStale,
    ClearRange,
    InsertRows,
    InsertCols,
    DeleteRows,
    DeleteCols,
    UndeleteRows,
    UndeleteCols,
    MoveRows,
    MoveCols,
    PermuteContent,
    MoveCells,
    DefineNamedRange,
    DeleteNamedRange,
    ResizeRows,
    ResizeCols,
    SetFreeze,
    SetFormat,
    SetBlock,
    FillFormula,
    AddSheet,
    RenameSheet,
    DeleteSheet,
    UndeleteSheet,
    MoveSheet,
}

/// Invalid/malicious frames injected by rogue clients to verify strict rejection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RogueKind {
    NonMemberHello,
    BadProtocolVersion,
    WrongDocOrBranch,
    ClientSeqZero,
    ClientSeqGap,
    ForgedForeignRowId,
    ForgedForeignSheetId,
    NonMonotonicIdCounter,
    CrossUserReplicaHijack,
    DuplicateMoveRows,
    OversizedPushBatch,
    ViewerPush,
}

/// Discrete event in a shrinkable simulation schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimEvent {
    LocalEdit {
        client: usize,
        choice: OpChoice,
        op_seed: u64,
    },
    DeliverC2S {
        client: usize,
        max_frames: usize,
    },
    DeliverS2C {
        client: usize,
        max_frames: usize,
    },
    DeliverSnapshot {
        client: usize,
    },
    DisconnectClient {
        client: usize,
    },
    ReconnectClient {
        client: usize,
    },
    SlowReceiverDisconnect {
        client: usize,
    },
    ClientCrashRestart {
        client: usize,
    },
    OpfsTailLoss {
        client: usize,
        drop_count: usize,
        offline_edits: u8,
        sub_seed: u64,
    },
    ServerCrashRestart,
    TakeBackupCheckpoint,
    ServerBackupRestore {
        pick_oldest: bool,
    },
    ToggleRole {
        client: usize,
    },
    SetTightQuota {
        enabled: bool,
    },
    InjectRogue {
        kind: RogueKind,
        sub_seed: u64,
    },
    ResendDuplicatePush {
        client: usize,
    },
    ServerBroadcastDigest,
}

/// Result of constructing a local transaction for a simulated client.
pub(crate) struct GeneratedLocalTxn {
    pub(crate) label: String,
    pub(crate) ops: Vec<Op<BoundFormula>>,
    pub(crate) used_set_cells_if: bool,
    pub(crate) used_formula: bool,
    pub(crate) used_move: bool,
    pub(crate) used_permute: bool,
    pub(crate) used_move_cells: bool,
    pub(crate) used_named_range: bool,
    pub(crate) used_sheet_op: bool,
}

/// Generates a deterministic event schedule from `seed`.
#[must_use]
pub fn generate_script(seed: u64) -> Vec<SimEvent> {
    let mut rng = Rng::from_seed(seed ^ 0x51A7_E026_9E37_79B9);
    let len = 55 + (rng.next_below(45) as usize);
    let mut events = Vec::with_capacity(len + 8);

    // Connect initial quorum (including Alice Tab 1 & Alice Tab 2) early.
    for client in 0..NUM_CLIENTS {
        events.push(SimEvent::ReconnectClient { client });
    }
    for client in 0..NUM_CLIENTS {
        events.push(SimEvent::DeliverC2S {
            client,
            max_frames: 2,
        });
        events.push(SimEvent::DeliverS2C {
            client,
            max_frames: 2,
        });
    }

    for step in 0..len {
        let pick = rng.next_below(100);
        let client = rng.next_below(NUM_CLIENTS as u64) as usize;
        let ev = if pick < 36 {
            let choice = pick_op_choice(&mut rng);
            SimEvent::LocalEdit {
                client,
                choice,
                op_seed: rng.next_u64(),
            }
        } else if pick < 52 {
            SimEvent::DeliverC2S {
                client,
                max_frames: 1 + rng.next_below(3) as usize,
            }
        } else if pick < 68 {
            SimEvent::DeliverS2C {
                client,
                max_frames: 1 + rng.next_below(4) as usize,
            }
        } else if pick < 73 {
            SimEvent::DeliverSnapshot { client }
        } else if pick < 78 {
            if rng.next_below(2) == 0 {
                SimEvent::DisconnectClient { client }
            } else {
                SimEvent::ReconnectClient { client }
            }
        } else if pick < 82 {
            SimEvent::SlowReceiverDisconnect { client }
        } else if pick < 86 {
            SimEvent::ClientCrashRestart { client }
        } else if pick < 90 {
            SimEvent::OpfsTailLoss {
                client,
                drop_count: 1 + rng.next_below(2) as usize,
                offline_edits: 1 + rng.next_below(2) as u8,
                sub_seed: rng.next_u64(),
            }
        } else if pick < 93 {
            SimEvent::ServerCrashRestart
        } else if pick < 96 {
            if rng.next_below(2) == 0 || step < 18 {
                SimEvent::TakeBackupCheckpoint
            } else {
                SimEvent::ServerBackupRestore {
                    pick_oldest: rng.next_below(2) == 0,
                }
            }
        } else if pick == 96 {
            SimEvent::ToggleRole { client: 2 }
        } else if pick == 97 {
            SimEvent::SetTightQuota {
                enabled: rng.next_below(2) == 0,
            }
        } else if pick == 98 {
            SimEvent::InjectRogue {
                kind: pick_rogue_kind(&mut rng),
                sub_seed: rng.next_u64(),
            }
        } else if rng.next_below(2) == 0 {
            SimEvent::ResendDuplicatePush { client }
        } else {
            SimEvent::ServerBroadcastDigest
        };
        events.push(ev);

        // Take a backup point early on so backup-restore triggers cleanly even in short seeds.
        if step == 14 {
            events.push(SimEvent::TakeBackupCheckpoint);
        }
    }

    // Ensure tight quota toggles back off at the end of active phase if still active, while leaving
    // any archived rejections verified in Invariant #10.
    events.push(SimEvent::SetTightQuota { enabled: false });
    events
}

fn pick_op_choice(rng: &mut Rng) -> OpChoice {
    match rng.next_below(105) {
        0..=17 => OpChoice::SetCellsScalar,
        18..=26 => OpChoice::SetCellsFormula,
        27..=32 => OpChoice::SetCellsIfMatching,
        33..=35 => OpChoice::SetCellsIfStale,
        36..=38 => OpChoice::ClearRange,
        39..=43 => OpChoice::InsertRows,
        44..=46 => OpChoice::InsertCols,
        47..=49 => OpChoice::DeleteRows,
        50..=51 => OpChoice::DeleteCols,
        52..=53 => OpChoice::UndeleteRows,
        54..=55 => OpChoice::UndeleteCols,
        56..=59 => OpChoice::MoveRows,
        60..=62 => OpChoice::MoveCols,
        63..=71 => OpChoice::PermuteContent,
        72..=77 => OpChoice::MoveCells,
        78..=81 => OpChoice::DefineNamedRange,
        82..=83 => OpChoice::DeleteNamedRange,
        84..=85 => OpChoice::ResizeRows,
        86..=87 => OpChoice::ResizeCols,
        88..=89 => OpChoice::SetFreeze,
        90..=92 => OpChoice::SetFormat,
        93..=95 => OpChoice::SetBlock,
        96..=98 => OpChoice::FillFormula,
        99 => OpChoice::AddSheet,
        100 => OpChoice::RenameSheet,
        101 => OpChoice::DeleteSheet,
        102 => OpChoice::UndeleteSheet,
        103 => OpChoice::MoveSheet,
        _ => OpChoice::PermuteContent,
    }
}

fn pick_rogue_kind(rng: &mut Rng) -> RogueKind {
    match rng.next_below(12) {
        0 => RogueKind::NonMemberHello,
        1 => RogueKind::BadProtocolVersion,
        2 => RogueKind::WrongDocOrBranch,
        3 => RogueKind::ClientSeqZero,
        4 => RogueKind::ClientSeqGap,
        5 => RogueKind::ForgedForeignRowId,
        6 => RogueKind::ForgedForeignSheetId,
        7 => RogueKind::NonMonotonicIdCounter,
        8 => RogueKind::CrossUserReplicaHijack,
        9 => RogueKind::DuplicateMoveRows,
        10 => RogueKind::OversizedPushBatch,
        _ => RogueKind::ViewerPush,
    }
}

pub(crate) fn collect_axis_samples(
    axis: &Axis,
    live_limit: usize,
    tomb_limit: usize,
) -> (Vec<u64>, Vec<u64>) {
    let mut live = Vec::with_capacity(live_limit);
    let mut tomb = Vec::with_capacity(tomb_limit);

    for run in axis.runs() {
        let len = run.ids.len;
        if let Some(ts) = &run.tombstones {
            let take = len.min(64);
            for offset in 0..take {
                let Some(raw) = run.ids.id_at(offset) else {
                    break;
                };
                if ts[offset as usize] == 0 {
                    if live.len() < live_limit {
                        live.push(raw);
                    }
                } else if tomb.len() < tomb_limit {
                    tomb.push(raw);
                }
                if live.len() >= live_limit && tomb.len() >= tomb_limit {
                    return (live, tomb);
                }
            }
        } else {
            let need = live_limit.saturating_sub(live.len());
            let take = len.min(need as u32);
            for offset in 0..take {
                if let Some(raw) = run.ids.id_at(offset) {
                    live.push(raw);
                }
            }
        }
        if live.len() >= live_limit && tomb.len() >= tomb_limit {
            break;
        }
    }
    (live, tomb)
}

fn build_scalar(rng: &mut Rng) -> ScalarValue {
    match rng.next_below(5) {
        0..=1 => ScalarValue::Number(((rng.next_below(200) as i64) - 50) as f64 * 0.5),
        2 => ScalarValue::Text(format!("t{}", rng.next_below(64))),
        3 => ScalarValue::Bool(rng.next_below(2) == 0),
        _ => match rng.next_below(3) {
            0 => ScalarValue::Error(CalcError::Div0),
            1 => ScalarValue::Error(CalcError::Value),
            _ => ScalarValue::Error(CalcError::Na),
        },
    }
}

fn build_format(rng: &mut Rng) -> NumberFormat {
    match rng.next_below(5) {
        0 => NumberFormat::General,
        1 => NumberFormat::Fixed2,
        2 => NumberFormat::Percent,
        3 => NumberFormat::Date,
        _ => NumberFormat::Integer,
    }
}

fn build_formula_for_sheet(
    rng: &mut Rng,
    wb: &Workbook<BoundFormula>,
    sheet_id: SheetId,
) -> Option<BoundFormula> {
    let sources = [
        "=SUM(A1:C4)",
        "=A1+B1",
        "=IF(A1>0,B2,C1)",
        "=AVERAGE(A1:B3)",
        "=COUNT(A1:D4)",
        "=A2*2-B1",
    ];
    let src = sources[rng.next_below(sources.len() as u64) as usize];
    BoundFormula::parse_and_bind(src, FormulaMode::Dynamic, wb, sheet_id).ok()
}

pub(crate) fn build_op_for_client(
    client: &mut ClientSyncMachine,
    choice: OpChoice,
    op_seed: u64,
) -> Option<GeneratedLocalTxn> {
    let mut rng = Rng::from_seed(op_seed);
    let wb = client.visible().clone();
    let live_sheets: Vec<SheetId> = wb
        .sheets()
        .iter()
        .filter(|s| s.deleted_count() == 0)
        .map(reticula_core::sheet::Sheet::id)
        .collect();
    let deleted_sheets: Vec<SheetId> = wb
        .sheets()
        .iter()
        .filter(|s| s.deleted_count() > 0)
        .map(reticula_core::sheet::Sheet::id)
        .collect();
    let sheet_id = if live_sheets.is_empty() {
        wb.sheets().first()?.id()
    } else {
        live_sheets[rng.next_below(live_sheets.len() as u64) as usize]
    };

    let sheet = wb.sheet(sheet_id)?;
    let (live_rows, tomb_rows) = collect_axis_samples(sheet.row_axis(), 10, 6);
    let (live_cols, tomb_cols) = collect_axis_samples(sheet.col_axis(), 8, 6);

    let pick_row = |rng: &mut Rng| -> Option<RowId> {
        (!live_rows.is_empty())
            .then(|| RowId(live_rows[rng.next_below(live_rows.len() as u64) as usize]))
    };
    let pick_col = |rng: &mut Rng| -> Option<ColId> {
        (!live_cols.is_empty())
            .then(|| ColId(live_cols[rng.next_below(live_cols.len() as u64) as usize]))
    };

    let mut used_set_cells_if = false;
    let mut used_formula = false;
    let mut used_move = false;
    let mut used_permute = false;
    let mut used_move_cells = false;
    let mut used_named_range = false;
    let mut used_sheet_op = false;

    let op = match choice {
        OpChoice::SetCellsScalar => {
            let r = pick_row(&mut rng)?;
            let c = pick_col(&mut rng)?;
            let input = if rng.next_below(6) == 0 {
                CellInput::Empty
            } else {
                CellInput::Scalar(build_scalar(&mut rng))
            };
            Op::SetCells {
                sheet: sheet_id,
                edits: vec![CellEdit {
                    row: r,
                    col: c,
                    input,
                    format: if rng.next_below(4) == 0 {
                        FormatPatch::Set(build_format(&mut rng))
                    } else {
                        FormatPatch::Keep
                    },
                }],
            }
        }
        OpChoice::SetCellsFormula => {
            let r = pick_row(&mut rng)?;
            let c = pick_col(&mut rng)?;
            used_formula = true;
            let formula = build_formula_for_sheet(&mut rng, &wb, sheet_id)?;
            Op::SetCells {
                sheet: sheet_id,
                edits: vec![CellEdit {
                    row: r,
                    col: c,
                    input: CellInput::Formula {
                        formula,
                        cached: None,
                    },
                    format: FormatPatch::Keep,
                }],
            }
        }
        OpChoice::SetCellsIfMatching | OpChoice::SetCellsIfStale => {
            let r = pick_row(&mut rng)?;
            let c = pick_col(&mut rng)?;
            let snap = wb.cell_snapshot(sheet_id, r, c)?;
            let expected_input = if choice == OpChoice::SetCellsIfStale {
                CellInput::Scalar(ScalarValue::Text(String::from("__mismatch__")))
            } else {
                snap.input
            };
            used_set_cells_if = true;
            Op::SetCellsIf {
                sheet: sheet_id,
                edits: vec![ConditionalCellEdit {
                    row: r,
                    col: c,
                    expected_input,
                    expected_format: snap.explicit_format,
                    new_input: CellInput::Scalar(build_scalar(&mut rng)),
                    new_format: FormatPatch::Keep,
                }],
            }
        }
        OpChoice::ClearRange => {
            let r = pick_row(&mut rng)?;
            let c = pick_col(&mut rng)?;
            Op::ClearRange {
                sheet: sheet_id,
                rows: vec![IdRun::new(r.0, 1)?],
                cols: vec![IdRun::new(c.0, 1)?],
            }
        }
        OpChoice::InsertRows => {
            let after = if rng.next_below(4) == 0 {
                None
            } else {
                pick_row(&mut rng)
            };
            let len = 1 + rng.next_below(2) as u32;
            let ids = client.alloc_row_run(len)?;
            Op::InsertRows {
                sheet: sheet_id,
                after,
                ids,
            }
        }
        OpChoice::InsertCols => {
            let after = if rng.next_below(4) == 0 {
                None
            } else {
                pick_col(&mut rng)
            };
            let len = 1 + rng.next_below(2) as u32;
            let ids = client.alloc_col_run(len)?;
            Op::InsertCols {
                sheet: sheet_id,
                after,
                ids,
            }
        }
        OpChoice::DeleteRows => {
            if live_rows.len() <= 2 {
                let ids = client.alloc_row_run(1)?;
                Op::InsertRows {
                    sheet: sheet_id,
                    after: live_rows.first().copied().map(RowId),
                    ids,
                }
            } else {
                let r = pick_row(&mut rng)?;
                Op::DeleteRows {
                    sheet: sheet_id,
                    ids: vec![IdRun::new(r.0, 1)?],
                }
            }
        }
        OpChoice::DeleteCols => {
            if live_cols.len() <= 2 {
                let ids = client.alloc_col_run(1)?;
                Op::InsertCols {
                    sheet: sheet_id,
                    after: live_cols.first().copied().map(ColId),
                    ids,
                }
            } else {
                let c = pick_col(&mut rng)?;
                Op::DeleteCols {
                    sheet: sheet_id,
                    ids: vec![IdRun::new(c.0, 1)?],
                }
            }
        }
        OpChoice::UndeleteRows => {
            if let Some(&tomb) =
                tomb_rows.get(rng.next_below(tomb_rows.len().max(1) as u64) as usize)
            {
                Op::UndeleteRows {
                    sheet: sheet_id,
                    ids: vec![IdRun::new(tomb, 1)?],
                }
            } else {
                let r = pick_row(&mut rng)?;
                Op::DeleteRows {
                    sheet: sheet_id,
                    ids: vec![IdRun::new(r.0, 1)?],
                }
            }
        }
        OpChoice::UndeleteCols => {
            if let Some(&tomb) =
                tomb_cols.get(rng.next_below(tomb_cols.len().max(1) as u64) as usize)
            {
                Op::UndeleteCols {
                    sheet: sheet_id,
                    ids: vec![IdRun::new(tomb, 1)?],
                }
            } else {
                let c = pick_col(&mut rng)?;
                Op::DeleteCols {
                    sheet: sheet_id,
                    ids: vec![IdRun::new(c.0, 1)?],
                }
            }
        }
        OpChoice::MoveRows => {
            if live_rows.len() < 3 {
                return None;
            }
            let src_idx = rng.next_below(live_rows.len() as u64) as usize;
            let moved = live_rows[src_idx];
            let mut dst_choices: Vec<Option<RowId>> = vec![None];
            for &cand in &live_rows {
                if cand != moved {
                    dst_choices.push(Some(RowId(cand)));
                }
            }
            let after = dst_choices[rng.next_below(dst_choices.len() as u64) as usize];
            used_move = true;
            Op::MoveRows {
                sheet: sheet_id,
                ids: vec![IdRun::new(moved, 1)?],
                after,
            }
        }
        OpChoice::MoveCols => {
            if live_cols.len() < 3 {
                return None;
            }
            let src_idx = rng.next_below(live_cols.len() as u64) as usize;
            let moved = live_cols[src_idx];
            let mut dst_choices: Vec<Option<ColId>> = vec![None];
            for &cand in &live_cols {
                if cand != moved {
                    dst_choices.push(Some(ColId(cand)));
                }
            }
            let after = dst_choices[rng.next_below(dst_choices.len() as u64) as usize];
            used_move = true;
            Op::MoveCols {
                sheet: sheet_id,
                ids: vec![IdRun::new(moved, 1)?],
                after,
            }
        }
        OpChoice::ResizeRows => {
            let r = pick_row(&mut rng)?;
            let size = if rng.next_below(4) == 0 {
                None
            } else {
                Some(18 + rng.next_below(40) as u16)
            };
            Op::ResizeRows {
                sheet: sheet_id,
                sizes: vec![(IdRun::new(r.0, 1)?, size)],
            }
        }
        OpChoice::ResizeCols => {
            let c = pick_col(&mut rng)?;
            let size = if rng.next_below(4) == 0 {
                None
            } else {
                Some(48 + rng.next_below(80) as u16)
            };
            Op::ResizeCols {
                sheet: sheet_id,
                sizes: vec![(IdRun::new(c.0, 1)?, size)],
            }
        }
        OpChoice::SetFreeze => Op::SetFreeze {
            sheet: sheet_id,
            frozen_rows: rng.next_below(3) as u32,
            frozen_cols: rng.next_below(3) as u32,
        },
        OpChoice::SetFormat => {
            let r = pick_row(&mut rng)?;
            let c = pick_col(&mut rng)?;
            let target = match rng.next_below(3) {
                0 => FormatTarget::Cells {
                    rows: vec![IdRun::new(r.0, 1)?],
                    cols: vec![IdRun::new(c.0, 1)?],
                },
                1 => FormatTarget::Rows(vec![IdRun::new(r.0, 1)?]),
                _ => FormatTarget::Cols(vec![IdRun::new(c.0, 1)?]),
            };
            Op::SetFormat {
                sheet: sheet_id,
                target,
                number_format: FormatPatch::Set(build_format(&mut rng)),
                style: StylePatch {
                    bold: Some(rng.next_below(2) == 0),
                    italic: Some(rng.next_below(2) == 0),
                    ..StylePatch::default()
                },
            }
        }
        OpChoice::SetBlock => {
            if live_rows.len() < 2 || live_cols.len() < 2 {
                return None;
            }
            let r0 = live_rows[0];
            let r1 = live_rows[1];
            let c0 = live_cols[0];
            let c1 = live_cols[1];
            let col0 = vec![
                CellInput::Scalar(build_scalar(&mut rng)),
                CellInput::Scalar(build_scalar(&mut rng)),
            ];
            let col1 = vec![
                CellInput::Scalar(build_scalar(&mut rng)),
                CellInput::Scalar(build_scalar(&mut rng)),
            ];
            Op::SetBlock {
                sheet: sheet_id,
                rows: vec![IdRun::new(r0, 1)?, IdRun::new(r1, 1)?],
                cols: vec![ColId(c0), ColId(c1)],
                columns_data: vec![col0, col1],
            }
        }
        OpChoice::FillFormula => {
            if live_rows.len() < 2 || live_cols.len() < 2 {
                return None;
            }
            used_formula = true;
            let bound = build_formula_for_sheet(&mut rng, &wb, sheet_id)?;
            let r0 = RowId(live_rows[0]);
            let r1 = RowId(live_rows[1]);
            let c0 = ColId(live_cols[0]);
            let c1 = ColId(live_cols[1]);
            let template = templatize_formula(&bound, r0, c0);
            Op::FillFormula {
                sheet: sheet_id,
                rows: vec![IdRun::new(r0.0, 1)?, IdRun::new(r1.0, 1)?],
                cols: vec![IdRun::new(c0.0, 1)?, IdRun::new(c1.0, 1)?],
                template,
            }
        }
        OpChoice::AddSheet => {
            used_sheet_op = true;
            if live_sheets.len() >= 4 {
                Op::RenameSheet {
                    sheet: sheet_id,
                    name: format!("R_{}", rng.next_below(1000)),
                }
            } else {
                let fresh = client.alloc_sheet_id()?;
                Op::AddSheet {
                    sheet: fresh,
                    name: format!("Tab_{}", rng.next_below(1000)),
                    after: Some(sheet_id),
                }
            }
        }
        OpChoice::RenameSheet => {
            used_sheet_op = true;
            Op::RenameSheet {
                sheet: sheet_id,
                name: format!("Sheet_{}", rng.next_below(500)),
            }
        }
        OpChoice::DeleteSheet => {
            used_sheet_op = true;
            if live_sheets.len() >= 2 {
                let target =
                    live_sheets[1 + rng.next_below((live_sheets.len() - 1) as u64) as usize];
                Op::DeleteSheet { sheet: target }
            } else {
                let fresh = client.alloc_sheet_id()?;
                Op::AddSheet {
                    sheet: fresh,
                    name: format!("Extra_{}", rng.next_below(500)),
                    after: Some(sheet_id),
                }
            }
        }
        OpChoice::UndeleteSheet => {
            used_sheet_op = true;
            if let Some(&target) = deleted_sheets.first() {
                Op::UndeleteSheet { sheet: target }
            } else if live_sheets.len() >= 2 {
                Op::DeleteSheet {
                    sheet: live_sheets[1],
                }
            } else {
                Op::RenameSheet {
                    sheet: sheet_id,
                    name: format!("Keep_{}", rng.next_below(500)),
                }
            }
        }
        OpChoice::MoveSheet => {
            used_sheet_op = true;
            if wb.sheets().len() < 2 {
                Op::RenameSheet {
                    sheet: sheet_id,
                    name: format!("M_{}", rng.next_below(500)),
                }
            } else {
                let idx = rng.next_below(wb.sheets().len() as u64) as usize;
                let s = wb.sheets()[idx].id();
                let mut candidates: Vec<Option<SheetId>> = vec![None];
                for other in wb.sheets() {
                    if other.id() != s {
                        candidates.push(Some(other.id()));
                    }
                }
                let after = candidates[rng.next_below(candidates.len() as u64) as usize];
                Op::MoveSheet { sheet: s, after }
            }
        }
        OpChoice::PermuteContent => {
            if live_rows.len() < 2 || live_cols.is_empty() {
                return None;
            }
            used_permute = true;
            let n_rows = 2 + (rng.next_below((live_rows.len() - 2).min(4) as u64 + 1) as usize);
            let n_cols = 1 + (rng.next_below(live_cols.len().min(3) as u64) as usize);
            let rows: Vec<RowId> = live_rows[..n_rows].iter().copied().map(RowId).collect();
            let cols: Vec<ColId> = live_cols[..n_cols].iter().copied().map(ColId).collect();
            let mut row_perm: Vec<u32> = (0..n_rows as u32).collect();
            for i in (1..n_rows).rev() {
                let j = rng.next_below((i + 1) as u64) as usize;
                row_perm.swap(i, j);
            }
            if row_perm.iter().enumerate().all(|(i, &p)| p as usize == i) {
                row_perm.swap(0, 1);
            }
            Op::PermuteContent {
                sheet: sheet_id,
                rows,
                cols,
                row_perm,
            }
        }
        OpChoice::MoveCells => {
            if sheet.rows() < 2 || sheet.cols() < 2 {
                return None;
            }
            used_move_cells = true;
            let dst_sheet_id = if live_sheets.len() >= 2 && rng.next_below(3) == 0 {
                live_sheets[rng.next_below(live_sheets.len() as u64) as usize]
            } else {
                sheet_id
            };
            let dst_sheet = wb.sheet(dst_sheet_id)?;
            let max_h = sheet.rows().min(dst_sheet.rows()).min(3);
            let max_w = sheet.cols().min(dst_sheet.cols()).min(3);
            if max_h == 0 || max_w == 0 {
                return None;
            }
            let h = 1 + (rng.next_below(u64::from(max_h)) as u32);
            let w = 1 + (rng.next_below(u64::from(max_w)) as u32);
            let src_r0 = rng.next_below(u64::from((sheet.rows() - h).min(8) + 1)) as u32;
            let src_c0 = rng.next_below(u64::from((sheet.cols() - w).min(6) + 1)) as u32;
            let dst_r0 = rng.next_below(u64::from((dst_sheet.rows() - h).min(8) + 1)) as u32;
            let dst_c0 = rng.next_below(u64::from((dst_sheet.cols() - w).min(6) + 1)) as u32;
            let src_rows: Vec<RowId> = (src_r0..src_r0 + h)
                .filter_map(|p| sheet.row_axis().pos_to_id(p).map(RowId))
                .collect();
            let src_cols: Vec<ColId> = (src_c0..src_c0 + w)
                .filter_map(|p| sheet.col_axis().pos_to_id(p).map(ColId))
                .collect();
            let dst_rows: Vec<RowId> = (dst_r0..dst_r0 + h)
                .filter_map(|p| dst_sheet.row_axis().pos_to_id(p).map(RowId))
                .collect();
            let dst_cols: Vec<ColId> = (dst_c0..dst_c0 + w)
                .filter_map(|p| dst_sheet.col_axis().pos_to_id(p).map(ColId))
                .collect();
            if src_rows.len() != h as usize
                || dst_rows.len() != h as usize
                || src_cols.len() != w as usize
                || dst_cols.len() != w as usize
            {
                return None;
            }
            Op::MoveCells {
                src_sheet: sheet_id,
                src_rows,
                src_cols,
                dst_sheet: dst_sheet_id,
                dst_rows,
                dst_cols,
            }
        }
        OpChoice::DefineNamedRange => {
            used_named_range = true;
            let scope = (rng.next_below(2) == 0).then_some(sheet_id);
            let names = ["TaxRate", "Sales_Q1", "Total_A", "Ref_B"];
            let name = String::from(names[rng.next_below(names.len() as u64) as usize]);
            let formula = build_formula_for_sheet(&mut rng, &wb, sheet_id)?;
            Op::DefineNamedRange {
                scope,
                name,
                formula,
                comment: (rng.next_below(2) == 0).then(|| String::from("sim note")),
                hidden: rng.next_below(4) == 0,
            }
        }
        OpChoice::DeleteNamedRange => {
            used_named_range = true;
            if let Some((key, def)) = wb.named_ranges().iter().next() {
                Op::DeleteNamedRange {
                    scope: key.scope,
                    name: def.name.clone(),
                }
            } else {
                let formula = build_formula_for_sheet(&mut rng, &wb, sheet_id)?;
                Op::DefineNamedRange {
                    scope: None,
                    name: String::from("TaxRate"),
                    formula,
                    comment: None,
                    hidden: false,
                }
            }
        }
    };

    Some(GeneratedLocalTxn {
        label: format!("{choice:?}"),
        ops: vec![op],
        used_set_cells_if,
        used_formula,
        used_move,
        used_permute,
        used_move_cells,
        used_named_range,
        used_sheet_op,
    })
}

/// Extracts all newly allocated row, column, and sheet IDs from `ops`.
pub(crate) fn extract_allocated_ids(
    ops: &[Op<BoundFormula>],
    rows_out: &mut Vec<RowId>,
    cols_out: &mut Vec<ColId>,
    sheets_out: &mut Vec<SheetId>,
) {
    for op in ops {
        match op {
            Op::InsertRows { ids, .. } => {
                for raw in ids.iter() {
                    rows_out.push(RowId(raw));
                }
            }
            Op::InsertCols { ids, .. } => {
                for raw in ids.iter() {
                    cols_out.push(ColId(raw));
                }
            }
            Op::AddSheet { sheet, .. } => sheets_out.push(*sheet),
            _ => {}
        }
    }
}

/// Builds a dummy distinct forged row/sheet rogue pending transaction against `target_replica`.
pub(crate) fn build_rogue_pending(
    kind: RogueKind,
    target_epoch: u8,
    target_replica: u32,
    expected_next_cs: u64,
    current_max_id: u32,
    sub_seed: u64,
) -> Option<WirePendingTxnV3> {
    let mut rng = Rng::from_seed(sub_seed);
    let (replica, client_seq, op) = match kind {
        RogueKind::ClientSeqZero => (
            target_replica,
            0,
            WireOpV3::SetFreeze {
                sheet: 0,
                frozen_rows: 1,
                frozen_cols: 0,
            },
        ),
        RogueKind::ClientSeqGap => (
            target_replica,
            expected_next_cs.saturating_add(5),
            WireOpV3::SetFreeze {
                sheet: 0,
                frozen_rows: 1,
                frozen_cols: 0,
            },
        ),
        RogueKind::ForgedForeignRowId => {
            let foreign_rep = target_replica.wrapping_add(1).max(2);
            let forged_first =
                pack_id(target_epoch, foreign_rep, current_max_id.saturating_add(99))?;
            (
                target_replica,
                expected_next_cs,
                WireOpV3::InsertRows {
                    sheet: 0,
                    after: None,
                    ids: IdRun::new(forged_first, 1)?.into(),
                },
            )
        }
        RogueKind::ForgedForeignSheetId => {
            let foreign_rep = target_replica.wrapping_add(1).max(2);
            let forged_sheet = pack_sheet_id(target_epoch, foreign_rep, 7)?;
            (
                target_replica,
                expected_next_cs,
                WireOpV3::AddSheet {
                    sheet: forged_sheet.0,
                    name: String::from("Forged"),
                    after: Some(0),
                },
            )
        }
        RogueKind::NonMonotonicIdCounter => {
            let stale_first = pack_id(target_epoch, target_replica, current_max_id)?;
            (
                target_replica,
                expected_next_cs,
                WireOpV3::InsertRows {
                    sheet: 0,
                    after: None,
                    ids: IdRun::new(stale_first, 1)?.into(),
                },
            )
        }
        RogueKind::CrossUserReplicaHijack => (
            target_replica.saturating_add(1),
            expected_next_cs,
            WireOpV3::SetFreeze {
                sheet: 0,
                frozen_rows: 2,
                frozen_cols: 0,
            },
        ),
        RogueKind::DuplicateMoveRows => (
            target_replica,
            expected_next_cs,
            WireOpV3::MoveRows {
                sheet: 0,
                ids: vec![IdRun::new(0, 1)?.into(), IdRun::new(0, 1)?.into()],
                after: Some(1),
            },
        ),
        RogueKind::NonMemberHello
        | RogueKind::BadProtocolVersion
        | RogueKind::WrongDocOrBranch
        | RogueKind::OversizedPushBatch
        | RogueKind::ViewerPush => {
            let _ = rng.next_u64();
            return None;
        }
    };

    Some(WirePendingTxnV3 {
        replica,
        client_seq,
        base_server_seq: 0,
        timestamp_ms: 999,
        txn: WireTxnV3 {
            label: format!("rogue_{}", rng.next_below(100)),
            ops: vec![op],
        },
    })
}

/// Checks if any ID in `rows` or `cols` or `sheets` collides with `seen_*` sets.
pub(crate) fn record_unique_ids(
    ops: &[Op<BoundFormula>],
    seen_rows: &mut BTreeSet<RowId>,
    seen_cols: &mut BTreeSet<ColId>,
    seen_sheets: &mut BTreeSet<SheetId>,
) -> Result<(), String> {
    let mut rows = Vec::new();
    let mut cols = Vec::new();
    let mut sheets = Vec::new();
    extract_allocated_ids(ops, &mut rows, &mut cols, &mut sheets);
    for r in rows {
        if !seen_rows.insert(r) {
            return Err(format!("duplicate RowId committed across history: {r:?}"));
        }
    }
    for c in cols {
        if !seen_cols.insert(c) {
            return Err(format!("duplicate ColId committed across history: {c:?}"));
        }
    }
    for s in sheets {
        if !seen_sheets.insert(s) {
            return Err(format!("duplicate SheetId committed across history: {s:?}"));
        }
    }
    Ok(())
}

/// Builds a malformed handshake frame for non-Push rogue variants.
#[must_use]
pub fn build_rogue_hello_frame(kind: RogueKind) -> Option<WireSyncFrameV2> {
    match kind {
        RogueKind::NonMemberHello => Some(WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: SIM_DOC_ID,
            branch_id: SIM_BRANCH_ID,
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        }),
        RogueKind::BadProtocolVersion => Some(WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2.wrapping_add(7),
            doc_id: SIM_DOC_ID,
            branch_id: SIM_BRANCH_ID,
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        }),
        RogueKind::WrongDocOrBranch => Some(WireSyncFrameV2::Hello {
            protocol_version: SYNC_PROTOCOL_VERSION_V2,
            doc_id: [0x99; 16],
            branch_id: SIM_BRANCH_ID,
            replica: None,
            last_applied_seq: 0,
            known_epoch: 0,
        }),
        _ => None,
    }
}
