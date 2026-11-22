//! Structured and byte-level fuzz harnesses shared by `cargo-fuzz` (`fuzz/`)
//! and `cargo xtask fuzz` (`09-quality.md` & `14-m1b.md` 8.1).
//!
//! Covers:
//! 1. `fuzz_one_parser`: arbitrary byte/UTF-8 inputs to lexer, parser, pretty
//!    printer, E30 input type inference, and RFC 4180 CSV parser;
//! 2. `fuzz_one_snapshot`: arbitrary byte slices fed into `RTCLSNAP` v0 & v1 decoders;
//! 3. `fuzz_one_wal`: arbitrary byte slices fed into `RTCLWAL1` scanner & replay;
//! 4. `fuzz_one_ops`: `arbitrary::Unstructured`-generated valid and adversarial
//!    M1b operation sequences + `WireOpV1` round-trip;
//! 5. `fuzz_one_eval`: arbitrary `BoundExpr` syntax trees + arbitrary cell
//!    contents evaluated in `FormulaEngine`.

use arbitrary::Unstructured;
use reticula_core::csv::import_csv_sheet;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId, pack_id};
use reticula_core::input::{infer_input, infer_input_scalar_only};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormatTarget, FormulaMode, Op, RawFormula, ScalarValue,
    Workbook,
};
use reticula_core::snapshot::decode_snapshot;
use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_core::value::CalcError;
use reticula_formula::ast::{BinaryOp, Literal, UnaryOp};
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef, templatize_formula};
use reticula_formula::engine::FormulaEngine;
use reticula_formula::lexer::tokenize;
use reticula_formula::parser::{parse_expr, parse_formula};
use reticula_wire::snapshot::decode_snapshot_any;
use reticula_wire::v1::WireOpV1;
use reticula_wire::wal::{replay_wal_records, scan_wal};

/// Target 1: formula lexer, Pratt parser, AST printer, E30 input inference, and CSV.
pub fn fuzz_one_parser(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let _ = infer_input(&text);
    let _ = infer_input_scalar_only(&text);
    if data.len() <= 2048 {
        let _ = import_csv_sheet(SheetId(0), String::from("CSV"), data, None);
    }
    let _ = tokenize(&text);
    if let Ok(ast) = parse_expr(&text) {
        let printed = ast.to_formula_string();
        let _ = parse_formula(&printed);
    }
    if let Ok(ast) = parse_formula(&text) {
        let printed = ast.to_formula_string();
        let _ = parse_formula(&printed);
    }
}

/// Target 2: untrusted `RTCLSNAP` v0 & v1 binary snapshot decoders.
pub fn fuzz_one_snapshot(data: &[u8]) {
    let _ = decode_snapshot::<RawFormula>(data);
    let _ = decode_snapshot_any(data);
}

/// Target 3: untrusted `RTCLWAL1` CRC-framed log scanner and replay.
pub fn fuzz_one_wal(data: &[u8]) {
    let _ = scan_wal(data, 0, 5);
    if let Ok(scan) = scan_wal(data, 0, 0) {
        let mut eng = FormulaEngine::new();
        let _ = replay_wal_records(&mut eng, &scan.records, 0, true);
    }
}

/// Target 4: structured valid + invalid M1b operation sequences on an engine.
#[allow(clippy::too_many_lines)]
pub fn fuzz_one_ops(data: &[u8]) {
    let mut u = Unstructured::new(data);
    let mut eng = FormulaEngine::new();
    let count: u8 = u.int_in_range(0..=24).unwrap_or(0);

    for _ in 0..count {
        let sheet = if u.ratio(7, 8).unwrap_or(true) {
            SheetId(0)
        } else {
            SheetId(u.int_in_range(0..=3).unwrap_or(1))
        };
        let kind: u8 = u.int_in_range(0..=15).unwrap_or(0);
        let op: Op<BoundFormula> = match kind {
            0 => {
                let n: usize = u.int_in_range(0..=8).unwrap_or(0);
                let mut edits = Vec::with_capacity(n);
                for _ in 0..n {
                    let row = arb_row_id(&mut u);
                    let col = arb_col_id(&mut u);
                    let input = arb_bound_cell_input(&mut u);
                    let format = match u.int_in_range(0_u8..=2).unwrap_or(0) {
                        0 => FormatPatch::Keep,
                        1 => FormatPatch::Set(NumberFormat::Fixed2),
                        _ => FormatPatch::Clear,
                    };
                    edits.push(CellEdit {
                        row,
                        col,
                        input,
                        format,
                    });
                }
                Op::SetCells { sheet, edits }
            }
            1 => Op::ClearRange {
                sheet,
                rows: vec![arb_id_run(&mut u)],
                cols: vec![arb_id_run(&mut u)],
            },
            2 => {
                let after = if u.arbitrary().unwrap_or(false) {
                    Some(arb_row_id(&mut u))
                } else {
                    None
                };
                let ids = if u.ratio(3, 4).unwrap_or(true) {
                    let len = u.int_in_range(1_u32..=4).unwrap_or(1);
                    eng.allocator_mut().alloc_run(len).unwrap_or(IdRun {
                        first_id: 0,
                        len: 1,
                    })
                } else {
                    arb_id_run(&mut u)
                };
                Op::InsertRows { sheet, after, ids }
            }
            3 => {
                let after = if u.arbitrary().unwrap_or(false) {
                    Some(arb_col_id(&mut u))
                } else {
                    None
                };
                let ids = arb_id_run(&mut u);
                Op::InsertCols { sheet, after, ids }
            }
            4 => Op::DeleteRows {
                sheet,
                ids: vec![arb_id_run(&mut u)],
            },
            5 => Op::DeleteCols {
                sheet,
                ids: vec![arb_id_run(&mut u)],
            },
            6 => Op::UndeleteRows {
                sheet,
                ids: vec![arb_id_run(&mut u)],
            },
            7 => Op::MoveRows {
                sheet,
                ids: vec![arb_id_run(&mut u)],
                after: if u.arbitrary().unwrap_or(false) {
                    Some(arb_row_id(&mut u))
                } else {
                    None
                },
            },
            8 => Op::MoveCols {
                sheet,
                ids: vec![arb_id_run(&mut u)],
                after: if u.arbitrary().unwrap_or(false) {
                    Some(arb_col_id(&mut u))
                } else {
                    None
                },
            },
            9 => Op::ResizeRows {
                sheet,
                sizes: vec![(arb_id_run(&mut u), u.arbitrary().unwrap_or(Some(30)))],
            },
            10 => Op::SetFreeze {
                sheet,
                frozen_rows: u.int_in_range(0_u32..=40).unwrap_or(0),
                frozen_cols: u.int_in_range(0_u32..=40).unwrap_or(0),
            },
            11 => Op::SetFormat {
                sheet,
                target: FormatTarget::Cells {
                    rows: vec![arb_id_run(&mut u)],
                    cols: vec![arb_id_run(&mut u)],
                },
                number_format: FormatPatch::Set(NumberFormat::Percent),
                style: StylePatch {
                    bold: Some(true),
                    align: Some(HorizontalAlign::Center),
                    text_color: ColorPatch::Set(ColorValue::Palette(1)),
                    border_top: Some(BorderWeight::Thin),
                    ..StylePatch::default()
                },
            },
            12 => {
                let f = BoundFormula {
                    mode: FormulaMode::Dynamic,
                    root: arb_bound_expr(&mut u, 2),
                };
                let tpl = templatize_formula(&f, RowId(0), ColId(0));
                Op::FillFormula {
                    sheet,
                    rows: vec![IdRun {
                        first_id: u.int_in_range(0_u64..=8).unwrap_or(0),
                        len: u.int_in_range(1_u32..=4).unwrap_or(1),
                    }],
                    cols: vec![IdRun {
                        first_id: u.int_in_range(0_u64..=4).unwrap_or(0),
                        len: 1,
                    }],
                    template: tpl,
                }
            }
            13 => Op::AddSheet {
                sheet: SheetId(u.int_in_range(1_u32..=4).unwrap_or(1)),
                name: String::from("FuzzTab"),
                after: Some(SheetId(0)),
            },
            14 => Op::DeleteSheet { sheet },
            _ => Op::UndeleteSheet { sheet },
        };

        if let Ok(wire) = WireOpV1::try_from(&op)
            && let Ok(bytes) = postcard::to_allocvec(&wire)
            && let Ok(decoded_w) = postcard::from_bytes::<WireOpV1>(&bytes)
        {
            let _ = Op::<BoundFormula>::try_from(decoded_w);
        }

        let before = eng.workbook().logical_digest();
        match eng.apply_op(op) {
            Ok(inv) => {
                if u.ratio(1, 4).unwrap_or(false) {
                    let _ = eng.apply_inverse(inv);
                }
            }
            Err(_) => {
                assert_eq!(eng.workbook().logical_digest(), before);
            }
        }
    }
    eng.recalc_all();
    let _ = Workbook::<RawFormula>::new();
}

/// Target 4: arbitrary `BoundExpr` trees + arbitrary sheet cell values fed into
/// `FormulaEngine` evaluation and incremental recalculation.
pub fn fuzz_one_eval(data: &[u8]) {
    let mut u = Unstructured::new(data);
    let mut eng = FormulaEngine::new();
    let sheet = SheetId(0);

    // Populate a 6x6 region with arbitrary constants and arbitrary BoundFormulas.
    let cell_count: u8 = u.int_in_range(1..=20).unwrap_or(1);
    for _ in 0..cell_count {
        let r = RowId(u.int_in_range(0_u64..=5).unwrap_or(0));
        let c = ColId(u.int_in_range(0_u64..=5).unwrap_or(0));
        let is_formula = u.ratio(2, 3).unwrap_or(true);
        let input = if is_formula {
            let mode = if u.arbitrary().unwrap_or(true) {
                FormulaMode::Dynamic
            } else {
                FormulaMode::Legacy
            };
            let max_depth = u.int_in_range(0_usize..=8).unwrap_or(3);
            let root = arb_bound_expr(&mut u, max_depth);
            CellInput::Formula {
                formula: BoundFormula { mode, root },
                cached: None,
            }
        } else {
            CellInput::Scalar(arb_scalar(&mut u))
        };
        let _ = eng.apply_op(Op::SetCells {
            sheet,
            edits: vec![CellEdit {
                row: r,
                col: c,
                input,
                format: FormatPatch::Keep,
            }],
        });
        if u.ratio(1, 3).unwrap_or(false) {
            let step = u.int_in_range(1_usize..=4).unwrap_or(1);
            let _ = eng.recalc_step(step);
        }
    }
    eng.recalc_all();
}

fn arb_row_id(u: &mut Unstructured<'_>) -> RowId {
    if u.ratio(4, 5).unwrap_or(true) {
        RowId(u.int_in_range(0_u64..=32).unwrap_or(0))
    } else {
        let ep: u8 = u.arbitrary().unwrap_or(0);
        let rep: u32 = u.int_in_range(0..=10).unwrap_or(1);
        let cnt: u32 = u.arbitrary().unwrap_or(1);
        RowId(pack_id(ep, rep, cnt).unwrap_or(u.arbitrary().unwrap_or(0)))
    }
}

fn arb_col_id(u: &mut Unstructured<'_>) -> ColId {
    if u.ratio(4, 5).unwrap_or(true) {
        ColId(u.int_in_range(0_u64..=16).unwrap_or(0))
    } else {
        ColId(u.arbitrary().unwrap_or(0))
    }
}

fn arb_id_run(u: &mut Unstructured<'_>) -> IdRun {
    let first_id = if u.ratio(3, 4).unwrap_or(true) {
        u.int_in_range(0_u64..=32).unwrap_or(0)
    } else {
        u.arbitrary().unwrap_or(0)
    };
    let len = if u.ratio(4, 5).unwrap_or(true) {
        u.int_in_range(1_u32..=8).unwrap_or(1)
    } else {
        u.arbitrary().unwrap_or(0)
    };
    IdRun { first_id, len }
}

fn arb_scalar(u: &mut Unstructured<'_>) -> ScalarValue {
    match u.int_in_range(0_u8..=4).unwrap_or(0) {
        0 => ScalarValue::Number(f64::from_bits(u.arbitrary().unwrap_or(0))),
        1 => ScalarValue::Number(f64::from(u.int_in_range(-1000_i32..=1000).unwrap_or(0)) / 10.0),
        2 => {
            let len = u.int_in_range(0_usize..=24).unwrap_or(0);
            let bytes = u.bytes(len).unwrap_or(&[]);
            ScalarValue::Text(String::from_utf8_lossy(bytes).into_owned())
        }
        3 => ScalarValue::Bool(u.arbitrary().unwrap_or(false)),
        _ => ScalarValue::Error(arb_calc_error(u)),
    }
}

fn arb_bound_cell_input(u: &mut Unstructured<'_>) -> CellInput<BoundFormula> {
    match u.int_in_range(0_u8..=2).unwrap_or(0) {
        0 => CellInput::Empty,
        1 => CellInput::Scalar(arb_scalar(u)),
        _ => CellInput::Formula {
            formula: BoundFormula {
                mode: FormulaMode::Dynamic,
                root: arb_bound_expr(u, 2),
            },
            cached: None,
        },
    }
}

fn arb_calc_error(u: &mut Unstructured<'_>) -> CalcError {
    match u.int_in_range(0_u8..=9).unwrap_or(0) {
        0 => CalcError::Div0,
        1 => CalcError::Value,
        2 => CalcError::Ref,
        3 => CalcError::Name,
        4 => CalcError::Num,
        5 => CalcError::Na,
        6 => CalcError::Null,
        7 => CalcError::Spill,
        8 => CalcError::Calc,
        _ => CalcError::Cycle,
    }
}

fn arb_literal_u(u: &mut Unstructured<'_>) -> Literal {
    match u.int_in_range(0_u8..=3).unwrap_or(0) {
        0 => {
            let n = f64::from(u.int_in_range(-500_i32..=500).unwrap_or(0)) / 4.0;
            Literal::Number(n)
        }
        1 => {
            let len = u.int_in_range(0_usize..=12).unwrap_or(0);
            let b = u.bytes(len).unwrap_or(&[]);
            Literal::Text(String::from_utf8_lossy(b).into_owned())
        }
        2 => Literal::Bool(u.arbitrary().unwrap_or(false)),
        _ => Literal::Error(arb_calc_error(u)),
    }
}

fn arb_bound_expr(u: &mut Unstructured<'_>, depth: usize) -> BoundExpr {
    if depth == 0 || u.is_empty() {
        return match u.int_in_range(0_u8..=2).unwrap_or(0) {
            0 => BoundExpr::Literal(arb_literal_u(u)),
            1 => BoundExpr::Reference(BoundRef::Cell {
                sheet: None,
                missing_sheet: None,
                row: RowId(u.int_in_range(0_u64..=7).unwrap_or(0)),
                col: ColId(u.int_in_range(0_u64..=7).unwrap_or(0)),
                abs_col: false,
                abs_row: false,
            }),
            _ => BoundExpr::Reference(BoundRef::Area {
                sheet: None,
                missing_sheet: None,
                first_row: RowId(u.int_in_range(0_u64..=5).unwrap_or(0)),
                first_col: ColId(u.int_in_range(0_u64..=5).unwrap_or(0)),
                last_row: RowId(u.int_in_range(0_u64..=7).unwrap_or(2)),
                last_col: ColId(u.int_in_range(0_u64..=5).unwrap_or(1)),
                abs_first_col: false,
                abs_first_row: false,
                abs_last_col: false,
                abs_last_row: false,
            }),
        };
    }

    let next = depth.saturating_sub(1);
    match u.int_in_range(0_u8..=5).unwrap_or(0) {
        0 => BoundExpr::Literal(arb_literal_u(u)),
        1 => {
            let rows = u.int_in_range(1_u32..=3).unwrap_or(1);
            let cols = u.int_in_range(1_u32..=3).unwrap_or(1);
            let elements = (0..rows * cols).map(|_| arb_literal_u(u)).collect();
            BoundExpr::ArrayLiteral {
                rows,
                cols,
                elements,
            }
        }
        2 => BoundExpr::Paren(Box::new(arb_bound_expr(u, next))),
        3 => {
            let op = match u.int_in_range(0_u8..=3).unwrap_or(0) {
                0 => UnaryOp::Plus,
                1 => UnaryOp::Neg,
                2 => UnaryOp::ImplicitIntersection,
                _ => UnaryOp::Percent,
            };
            BoundExpr::Unary {
                op,
                expr: Box::new(arb_bound_expr(u, next)),
            }
        }
        4 => {
            let op = match u.int_in_range(0_u8..=11).unwrap_or(0) {
                0 => BinaryOp::Pow,
                1 => BinaryOp::Mul,
                2 => BinaryOp::Div,
                3 => BinaryOp::Add,
                4 => BinaryOp::Sub,
                5 => BinaryOp::Concat,
                6 => BinaryOp::Eq,
                7 => BinaryOp::Ne,
                8 => BinaryOp::Lt,
                9 => BinaryOp::Gt,
                10 => BinaryOp::Le,
                _ => BinaryOp::Ge,
            };
            BoundExpr::Binary {
                op,
                left: Box::new(arb_bound_expr(u, next)),
                right: Box::new(arb_bound_expr(u, next)),
            }
        }
        _ => {
            const FUNCS: [&str; 38] = [
                "SUM",
                "AVERAGE",
                "COUNT",
                "COUNTA",
                "MIN",
                "MAX",
                "IF",
                "AND",
                "OR",
                "NOT",
                "IFERROR",
                "IFNA",
                "ABS",
                "ROUND",
                "INT",
                "TRUNC",
                "MOD",
                "SQRT",
                "POWER",
                "LEN",
                "TRIM",
                "MID",
                "ISBLANK",
                "ROW",
                "VLOOKUP",
                "HLOOKUP",
                "INDEX",
                "MATCH",
                "CHOOSE",
                "ROWS",
                "COUNTBLANK",
                "COUNTIF",
                "SUMIF",
                "DATE",
                "EDATE",
                "FLOOR",
                "SEARCH",
                "TEXT",
            ];
            let idx = u.int_in_range(0_usize..=FUNCS.len() - 1).unwrap_or(0);
            let argc = u.int_in_range(0_usize..=4).unwrap_or(1);
            let args = (0..argc).map(|_| arb_bound_expr(u, next)).collect();
            BoundExpr::Call {
                name: String::from(FUNCS[idx]),
                args,
            }
        }
    }
}

/// Target 6: arbitrary sync frames (`WireSyncFrameV1`), `RTCLPWAL`/`RTCLWAL2`
/// scanning, and arbitrary synchronized state-machine transitions across
/// `ServerDocMachine` and two `ClientSyncMachine` instances.
pub fn fuzz_one_sync(data: &[u8]) {
    use reticula_core::state_digest::StateDigest;
    use reticula_sync::{
        ApplyResult, ClientSyncMachine, MemberRole, ServerDocMachine, ServerLimits,
        replay_pending_onto,
    };
    use reticula_wire::convert_v2::ops_to_wire_txn_v2;
    use reticula_wire::sync_frame::{decode_sync_frame, encode_sync_frame};
    use reticula_wire::v2::{
        SYNC_PROTOCOL_VERSION, WireCatchupV1, WireCommittedTxnV2, WirePendingTxnV2,
        WireRejectReasonV1, WireStateDigestV1, WireSyncFrameV1,
    };
    use reticula_wire::wal::{scan_pending_wal, scan_wal_any};

    if let Ok(frame) = decode_sync_frame(data) {
        let _ = encode_sync_frame(&frame);
    }
    let _ = scan_pending_wal(data);
    let _ = scan_wal_any(data, 0, 8);

    const DOC: [u8; 16] = [0x11; 16];
    const BRANCH: [u8; 16] = [0x22; 16];
    const UA: [u8; 16] = [0xAA; 16];
    const UB: [u8; 16] = [0xBB; 16];

    let mut u = Unstructured::new(data);
    let mut server =
        ServerDocMachine::new(DOC, BRANCH, String::from("/s"), ServerLimits::default());
    let mut clients = [
        ClientSyncMachine::new(DOC, BRANCH),
        ClientSyncMachine::new(DOC, BRANCH),
    ];
    let users = [UA, UB];

    let steps = u.int_in_range(1_usize..=16).unwrap_or(1);
    for step in 0..steps {
        let cidx = usize::from(u.arbitrary::<bool>().unwrap_or(false));
        let user = users[cidx];
        let role = if u.ratio(7, 8).unwrap_or(true) {
            Some(if cidx == 0 {
                MemberRole::Owner
            } else {
                MemberRole::Editor
            })
        } else {
            None
        };

        match u.int_in_range(0_u8..=8).unwrap_or(0) {
            0 => {
                for act in clients[cidx].on_connected() {
                    if let reticula_sync::ClientAction::SendFrame(hf) = act
                        && let Ok(welcome) = server.on_hello(user, role, &hf)
                    {
                        let _ = clients[cidx].on_welcome(welcome);
                    }
                }
            }
            1 => {
                clients[cidx].on_disconnected();
            }
            2 => {
                let r = arb_row_id(&mut u);
                let c = arb_col_id(&mut u);
                let input = if u.arbitrary().unwrap_or(true) {
                    CellInput::Scalar(arb_scalar(&mut u))
                } else {
                    CellInput::Empty
                };
                if let Ok(actions) = clients[cidx].push_local(
                    String::from("edit"),
                    vec![Op::SetCells {
                        sheet: SheetId(0),
                        edits: vec![CellEdit {
                            row: r,
                            col: c,
                            input,
                            format: FormatPatch::Keep,
                        }],
                    }],
                    step as u64,
                ) {
                    for act in actions {
                        if let reticula_sync::ClientAction::SendFrame(WireSyncFrameV1::Push {
                            txns,
                        }) = act
                            && let Some(rep) = clients[cidx].replica()
                        {
                            for p in txns {
                                if let Ok(ApplyResult::Committed(commit)) =
                                    server.apply_and_commit(user, role, rep, p, step as u64)
                                {
                                    let _ = clients[0].on_commit(commit.clone());
                                    let _ = clients[1].on_commit(commit);
                                }
                            }
                        }
                    }
                }
            }
            3 => {
                if let Ok(frame) = decode_sync_frame(data) {
                    let _ = server.on_hello(user, role, &frame);
                    match &frame {
                        WireSyncFrameV1::Welcome { .. } => {
                            let _ = clients[cidx].on_welcome(frame);
                        }
                        WireSyncFrameV1::Commit {
                            seq,
                            replica,
                            client_seq,
                            user_id,
                            timestamp_ms,
                            txn,
                        } => {
                            let _ = clients[cidx].on_commit(WireCommittedTxnV2 {
                                seq: *seq,
                                replica: *replica,
                                client_seq: *client_seq,
                                user_id: *user_id,
                                timestamp_ms: *timestamp_ms,
                                txn: txn.clone(),
                            });
                        }
                        WireSyncFrameV1::Ack {
                            replica,
                            client_seq,
                            ..
                        } => clients[cidx].on_ack(*replica, *client_seq),
                        WireSyncFrameV1::Reject { client_seq, reason } => {
                            let _ = clients[cidx].on_reject(*client_seq, *reason);
                        }
                        WireSyncFrameV1::Digest { seq, state_digest } => {
                            let _ = clients[cidx].on_digest(*seq, *state_digest);
                        }
                        _ => {}
                    }
                }
            }
            4 => {
                let cs = u.int_in_range(0_u64..=8).unwrap_or(1);
                let rep = u.int_in_range(0_u32..=8).unwrap_or(1);
                if let Ok(txn) = ops_to_wire_txn_v2(String::from("arb"), &[]) {
                    let pending = WirePendingTxnV2 {
                        replica: rep,
                        client_seq: cs,
                        base_server_seq: u.arbitrary().unwrap_or(0),
                        timestamp_ms: step as u64,
                        txn,
                    };
                    let _ = server.apply_and_commit(user, role, rep, pending, step as u64);
                }
            }
            5 => {
                let cs = u.int_in_range(0_u64..=6).unwrap_or(1);
                let reason = match u.int_in_range(0_u8..=4).unwrap_or(0) {
                    0 => WireRejectReasonV1::Forbidden,
                    1 => WireRejectReasonV1::InvalidOp,
                    2 => WireRejectReasonV1::QuotaExceeded,
                    3 => WireRejectReasonV1::PayloadTooLarge,
                    _ => WireRejectReasonV1::NonContiguousClientSeq { expected: 1 },
                };
                let _ = clients[cidx].on_reject(cs, reason);
            }
            6 => {
                let df = server.digest_frame();
                if let WireSyncFrameV1::Digest { seq, state_digest } = df {
                    let _ = clients[cidx].on_digest(seq, state_digest);
                }
                let bad = WireStateDigestV1 {
                    version: 1,
                    content_lo: u.arbitrary().unwrap_or(0),
                    content_hi: u.arbitrary().unwrap_or(0),
                    order: u.arbitrary().unwrap_or(0),
                };
                let _ = clients[cidx].on_digest(clients[cidx].confirmed_seq(), bad);
            }
            7 => {
                let welcome = WireSyncFrameV1::Welcome {
                    epoch: 0,
                    head_seq: 0,
                    replica: 1,
                    last_client_seq: 0,
                    max_id_counter: 0,
                    catchup: WireCatchupV1::SnapshotRequired {
                        snapshot_seq: 0,
                        url: String::from("/snap/0"),
                    },
                };
                let _ = clients[cidx].on_welcome(welcome);
                let _ = clients[cidx].install_snapshot(0, Workbook::new(), &[], Vec::new());
            }
            _ => {
                let _ = SYNC_PROTOCOL_VERSION;
                let _ = StateDigest::try_from(server.state_digest());
            }
        }

        // Invariant #1 holds unconditionally on every client state transition.
        let expected = replay_pending_onto(
            clients[cidx].confirmed(),
            &clients[cidx].pending_wire_txns(),
        );
        assert_eq!(
            clients[cidx].visible().state_digest(),
            expected.state_digest(),
            "fuzz_one_sync violated client visible == replay(confirmed + pending)"
        );
    }
}
