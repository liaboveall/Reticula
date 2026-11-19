//! Property-based tests (`proptest`) for `reticula-core`, `reticula-formula`
//! and `reticula-wire` (`13-m1a.md` 7.1 & `14-m1b.md` 8.1).

use proptest::prelude::*;
use reticula_core::axis::Axis;
use reticula_core::format::{CurrencySymbol, NumberFormat};
use reticula_core::id::{ColId, IdAllocator, IdRun, RowId, SheetId, pack_id};
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormatTarget, FormulaMode, Op, ScalarValue, Workbook,
};
use reticula_core::sheet::Sheet;
use reticula_core::style::{BorderWeight, ColorPatch, ColorValue, HorizontalAlign, StylePatch};
use reticula_core::value::CalcError;
use reticula_formula::address::CellAddr;
use reticula_formula::ast::{BinaryOp, Expr, Literal, UnaryOp};
use reticula_formula::binder::{
    BoundFormula, FillTargetRect, FormulaShiftSpec, build_fill_plan, shift_formula_for_cell,
    templatize_formula,
};
use reticula_formula::engine::{FormulaEngine, RecalcStatus};
use reticula_formula::parser::parse_formula;
use reticula_wire::snapshot::{decode_snapshot_any, encode_snapshot_v1};
use reticula_wire::v1::{WireOpV1, WireReplicaWatermarkV1, WireWalRecordV1};
use reticula_wire::wal::{
    WalCorruption, WalTailDisposition, encode_wal_complete, encode_wal_record, replay_wal_records,
    scan_wal,
};

// ============================================================================
// 1. Axis vs naive Vec<(u64, u32)> model
// ============================================================================

fn arb_axis_steps() -> impl Strategy<Value = Vec<(u8, usize, u32)>> {
    prop::collection::vec((0_u8..7, 0_usize..200, 1_u32..8), 1..70)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    #[test]
    fn prop_axis_matches_naive_vec(steps in arb_axis_steps()) {
        let base_len = 24_u32;
        let max_len = 500_u32;
        let mut axis = Axis::with_base(base_len, max_len);
        let mut naive: Vec<(u64, u32)> = (0..u64::from(base_len)).map(|id| (id, 0)).collect();
        let mut alloc = IdAllocator::local();
        let mut inserted_history: Vec<IdRun> = Vec::new();

        for (kind, idx_seed, len_seed) in steps {
            match kind {
                0 => {
                    // Insert at top
                    let run = alloc.alloc_run(len_seed).unwrap();
                    if axis.insert_after(None, run).is_ok() {
                        let new_items: Vec<(u64, u32)> = run.iter().map(|id| (id, 0)).collect();
                        naive.splice(0..0, new_items);
                        inserted_history.push(run);
                    }
                }
                1 | 2 => {
                    // Insert after an existing entry (live or tombstoned)
                    if !naive.is_empty() {
                        let pos = idx_seed % naive.len();
                        let anchor_id = naive[pos].0;
                        let run = alloc.alloc_run(len_seed).unwrap();
                        if axis.insert_after(Some(anchor_id), run).is_ok() {
                            let new_items: Vec<(u64, u32)> = run.iter().map(|id| (id, 0)).collect();
                            naive.splice(pos + 1..pos + 1, new_items);
                            inserted_history.push(run);
                        }
                    }
                }
                3 | 4 => {
                    // Delete 1..=len_seed items (supports double-delete!)
                    if !naive.is_empty() {
                        let pos = idx_seed % naive.len();
                        let id = naive[pos].0;
                        let single = IdRun::new(id, 1).unwrap();
                        assert!(axis.delete_runs(&[single]).is_ok());
                        naive[pos].1 += 1;
                    }
                }
                5 => {
                    // Undelete a currently tombstoned item
                    let deleted_indices: Vec<usize> = naive
                        .iter()
                        .enumerate()
                        .filter(|(_, (_, count))| *count > 0)
                        .map(|(i, _)| i)
                        .collect();
                    if !deleted_indices.is_empty() {
                        let pos = deleted_indices[idx_seed % deleted_indices.len()];
                        let id = naive[pos].0;
                        let single = IdRun::new(id, 1).unwrap();
                        assert!(axis.undelete_runs(&[single]).is_ok());
                        naive[pos].1 -= 1;
                    }
                }
                _ => {
                    // Remove a previously inserted run if all its entries are consecutive & untombstoned
                    if let Some(run) = inserted_history.pop() {
                        let res = axis.remove_inserted(run);
                        if res.is_ok() {
                            naive.retain(|(id, _)| !run.contains(*id));
                        }
                    }
                }
            }

            // Verify invariants against `naive` after every single step!
            let naive_live: Vec<u64> = naive
                .iter()
                .filter(|(_, t)| *t == 0)
                .map(|(id, _)| *id)
                .collect();
            prop_assert_eq!(axis.total_len() as usize, naive.len());
            prop_assert_eq!(axis.live_len() as usize, naive_live.len());

            for (vis_pos, &expected_id) in naive_live.iter().enumerate() {
                let p = vis_pos as u32;
                prop_assert_eq!(axis.pos_to_id(p), Some(expected_id));
                prop_assert_eq!(axis.id_to_pos(expected_id), Some(p));
            }
            for &(id, tomb) in &naive {
                if tomb > 0 {
                    prop_assert_eq!(axis.id_to_pos(id), None);
                }
            }

            if naive.len() >= 2 {
                let i1 = idx_seed % naive.len();
                let i2 = (idx_seed.wrapping_mul(7) + 3) % naive.len();
                let (id_a, id_b) = (naive[i1].0, naive[i2].0);
                prop_assert_eq!(axis.compare_anchors(id_a, id_b), Some(i1.cmp(&i2)));

                // Forward and reversed anchor range iteration!
                let (lo, hi) = if i1 <= i2 { (i1, i2) } else { (i2, i1) };
                let expected_range: Vec<u64> = naive[lo..=hi]
                    .iter()
                    .filter(|(_, t)| *t == 0)
                    .map(|(id, _)| *id)
                    .collect();
                for (first_anchor, last_anchor) in [(id_a, id_b), (id_b, id_a)] {
                    let mut actual_range = Vec::new();
                    let ok = axis.for_each_live_in_range(first_anchor, last_anchor, |r| {
                        actual_range.extend(r.iter());
                    });
                    prop_assert!(ok);
                    prop_assert_eq!(&actual_range, &expected_range);
                }
            }

            if !naive_live.is_empty() {
                let first_pos = (idx_seed % naive_live.len()) as u32;
                let count = len_seed;
                let mut pos_ids = Vec::new();
                axis.for_each_live_positions(first_pos, count, |r| pos_ids.extend(r.iter()));
                let end = (first_pos as usize + count as usize).min(naive_live.len());
                prop_assert_eq!(pos_ids.as_slice(), &naive_live[first_pos as usize..end]);
            }
        }
    }
}

// ============================================================================
// 2 & 3. Op + InverseOp exact round-trip & Snapshot v0 round-trip
// ============================================================================

fn arb_valid_raw_input() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        (-10_000_i32..10_000_i32).prop_map(|n| format!("{n}.25")),
        prop::sample::select(vec![
            String::from("TRUE"),
            String::from("FALSE"),
            String::from("2026-11-18"),
            String::from("1900-02-29"),
            String::from("12.5%"),
            String::from("'0042"),
            String::from("华东-SKU"),
            String::from("#N/A"),
            String::from("=A1+B1"),
            String::from("=SUM(A1:B4)"),
            String::from("=ROW()*10+COLUMN()"),
            String::from("=IF(A1>0,ROUND(A1*1.13,2),0)"),
        ]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_op_and_inverse_restore_exact_digest_and_snapshot_round_trips(
        ops in prop::collection::vec((0_u8..6, 0_u32..8, 0_u32..5, arb_valid_raw_input()), 1..30)
    ) {
        let mut eng = FormulaEngine::new();
        let sheet = SheetId(0);
        let mut history = Vec::new();

        for (kind, r_idx, c_idx, raw) in ops {
            eng.recalc_all();
            let before_digest = eng.workbook().logical_digest();

            let sh = eng.workbook().sheet(sheet).unwrap();
            let live_r = sh.rows();
            let live_c = sh.cols();
            if live_r < 4 || live_c < 4 {
                continue;
            }
            let row = RowId(sh.row_id(r_idx % live_r.min(12)));
            let col = ColId(sh.col_id(c_idx % live_c.min(8)));

            let inv_res = match kind {
                0 | 1 => eng.edit_cell_raw(sheet, row, col, &raw).ok(),
                2 => {
                    let r_run = IdRun::new(row.0, 1).unwrap();
                    let c_run = IdRun::new(col.0, 1).unwrap();
                    eng.apply_op(Op::ClearRange {
                        sheet,
                        rows: vec![r_run],
                        cols: vec![c_run],
                    })
                    .ok()
                }
                3 => {
                    let ins = eng.allocator_mut().alloc_run(1).unwrap();
                    let after = if r_idx % 2 == 0 { None } else { Some(row) };
                    eng.apply_op(Op::InsertRows {
                        sheet,
                        after,
                        ids: ins,
                    })
                    .ok()
                }
                4 => {
                    let del = IdRun::new(row.0, 1).unwrap();
                    eng.apply_op(Op::DeleteRows {
                        sheet,
                        ids: vec![del],
                    })
                    .ok()
                }
                _ => {
                    let ins = eng.allocator_mut().alloc_run(1).unwrap();
                    eng.apply_op(Op::InsertCols {
                        sheet,
                        after: Some(col),
                        ids: ins,
                    })
                    .ok()
                }
            };

            if let Some(inv) = inv_res {
                eng.recalc_all();
                history.push((before_digest, inv));
            }
        }

        // Snapshot round-trip on the mutated workbook!
        eng.recalc_all();
        let peak_digest = eng.workbook().logical_digest();
        let snap_bytes = eng.encode_snapshot().expect("snapshot encode");
        let (decoded, _) = FormulaEngine::decode_snapshot(&snap_bytes).expect("snapshot decode");
        prop_assert_eq!(decoded.workbook().logical_digest(), peak_digest);

        // Unwind every inverse operation in LIFO order and verify exact restoration!
        while let Some((expected_digest, inv)) = history.pop() {
            eng.apply_inverse(inv).expect("inverse succeeds");
            eng.recalc_all();
            prop_assert_eq!(eng.workbook().logical_digest(), expected_digest);
        }
    }
}

// ============================================================================
// 4. Arbitrary valid + invalid ops never panic and reject atomically
// ============================================================================

fn arb_maybe_invalid_op() -> impl Strategy<Value = Op<BoundFormula>> {
    let sheet_id = prop_oneof![Just(SheetId(0)), Just(SheetId(999))];
    let row_id = prop_oneof![
        (0_u64..20).prop_map(RowId),
        Just(RowId(pack_id(7, 77, 1).unwrap())),
        Just(RowId(u64::MAX)),
    ];
    let col_id = prop_oneof![
        (0_u64..10).prop_map(ColId),
        Just(ColId(pack_id(7, 77, 2).unwrap())),
        Just(ColId(u64::MAX)),
    ];
    let scalar = prop_oneof![
        Just(ScalarValue::Number(12.5)),
        Just(ScalarValue::Number(f64::NAN)),
        Just(ScalarValue::Number(f64::INFINITY)),
        Just(ScalarValue::Bool(true)),
        Just(ScalarValue::Error(CalcError::Div0)),
        Just(ScalarValue::Text("ok".to_owned())),
        Just(ScalarValue::Text("x".repeat(32_769))),
    ];
    let edit = (row_id.clone(), col_id.clone(), scalar).prop_map(|(r, c, s)| CellEdit {
        row: r,
        col: c,
        input: CellInput::Scalar(s),
        format: FormatPatch::Set(NumberFormat::General),
    });

    prop_oneof![
        (sheet_id.clone(), prop::collection::vec(edit, 0..6))
            .prop_map(|(sheet, edits)| Op::SetCells { sheet, edits }),
        (sheet_id.clone(), row_id.clone(), 1_u32..4).prop_map(|(sheet, r, len)| {
            let ids = IdRun::new(r.0, len).unwrap_or(IdRun { first_id: r.0, len });
            Op::InsertRows {
                sheet,
                after: Some(r),
                ids,
            }
        }),
        (sheet_id, row_id, 1_u32..4).prop_map(|(sheet, r, len)| {
            let ids = IdRun::new(r.0, len).unwrap_or(IdRun { first_id: r.0, len });
            Op::DeleteRows {
                sheet,
                ids: vec![ids, ids],
            }
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn prop_arbitrary_ops_never_panic_and_invalid_ops_leave_state_unchanged(
        ops in prop::collection::vec(arb_maybe_invalid_op(), 1..25)
    ) {
        let mut eng = FormulaEngine::new();
        for op in ops {
            let before = eng.workbook().logical_digest();
            match eng.apply_op(op) {
                Ok(_) => {
                    eng.recalc_all();
                }
                Err(_) => {
                    prop_assert_eq!(eng.workbook().logical_digest(), before);
                }
            }
        }
    }
}

// ============================================================================
// 5. AST pretty-print -> re-parse round-trip
// ============================================================================

fn arb_literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        (0_u32..1_000_000).prop_map(|n| Literal::Number(f64::from(n) / 4.0)),
        "[A-Za-z0-9 _\"]{0,12}".prop_map(Literal::Text),
        any::<bool>().prop_map(Literal::Bool),
        prop::sample::select(vec![
            CalcError::Div0,
            CalcError::Value,
            CalcError::Ref,
            CalcError::Name,
            CalcError::Num,
            CalcError::Na,
            CalcError::Spill,
        ])
        .prop_map(Literal::Error),
    ]
}

fn arb_cell_addr() -> impl Strategy<Value = CellAddr> {
    (0_u32..200, 0_u32..5000, any::<bool>(), any::<bool>()).prop_map(
        |(col, row, abs_col, abs_row)| CellAddr {
            col,
            row,
            abs_col,
            abs_row,
        },
    )
}

fn arb_sheet_opt() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some(String::from("Sheet1"))),
        Just(Some(String::from("Sales 2026"))),
        Just(Some(String::from("Q1's Data"))),
    ]
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        arb_literal().prop_map(Expr::Literal),
        (arb_sheet_opt(), arb_cell_addr()).prop_map(|(sheet, addr)| Expr::CellRef { sheet, addr }),
        (arb_sheet_opt(), arb_cell_addr(), arb_cell_addr())
            .prop_map(|(sheet, first, last)| { Expr::AreaRef { sheet, first, last } }),
        (
            arb_sheet_opt(),
            0_u32..50,
            any::<bool>(),
            0_u32..50,
            any::<bool>()
        )
            .prop_map(
                |(sheet, first_col, abs_first, last_col, abs_last)| Expr::ColRange {
                    sheet,
                    first_col,
                    abs_first,
                    last_col,
                    abs_last,
                }
            ),
        (
            arb_sheet_opt(),
            0_u32..500,
            any::<bool>(),
            0_u32..500,
            any::<bool>()
        )
            .prop_map(
                |(sheet, first_row, abs_first, last_row, abs_last)| Expr::RowRange {
                    sheet,
                    first_row,
                    abs_first,
                    last_row,
                    abs_last,
                }
            ),
        (1_u32..=3, 1_u32..=3)
            .prop_flat_map(|(rows, cols)| {
                let len = (rows * cols) as usize;
                (
                    Just(rows),
                    Just(cols),
                    prop::collection::vec(arb_literal(), len),
                )
            })
            .prop_map(|(rows, cols, elements)| Expr::ArrayLiteral {
                rows,
                cols,
                elements,
            }),
    ];

    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            inner.clone().prop_map(|e| Expr::Paren(Box::new(e))),
            (
                prop::sample::select(vec![
                    UnaryOp::Plus,
                    UnaryOp::Neg,
                    UnaryOp::ImplicitIntersection,
                    UnaryOp::Percent,
                ]),
                inner.clone(),
            )
                .prop_map(|(op, expr)| Expr::Unary {
                    op,
                    expr: Box::new(expr),
                }),
            (
                prop::sample::select(vec![
                    BinaryOp::Pow,
                    BinaryOp::Mul,
                    BinaryOp::Div,
                    BinaryOp::Add,
                    BinaryOp::Sub,
                    BinaryOp::Concat,
                    BinaryOp::Eq,
                    BinaryOp::Ne,
                    BinaryOp::Lt,
                    BinaryOp::Gt,
                    BinaryOp::Le,
                    BinaryOp::Ge,
                ]),
                inner.clone(),
                inner.clone(),
            )
                .prop_map(|(op, left, right)| Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }),
            (
                prop::sample::select(vec![
                    String::from("SUM"),
                    String::from("IF"),
                    String::from("ROUND"),
                    String::from("CONCAT"),
                ]),
                prop::collection::vec(inner, 1..=4),
            )
                .prop_map(|(name, args)| Expr::Call { name, args }),
        ]
    })
}

fn strip_parens(expr: &Expr) -> Expr {
    match expr {
        Expr::Paren(inner) => strip_parens(inner),
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(strip_parens(expr)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(strip_parens(left)),
            right: Box::new(strip_parens(right)),
        },
        Expr::Call { name, args } => Expr::Call {
            name: name.clone(),
            args: args.iter().map(strip_parens).collect(),
        },
        other => other.clone(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(250))]

    #[test]
    fn prop_ast_pretty_print_and_reparse_round_trip(ast in arb_expr()) {
        let printed = ast.to_formula_string();
        let reparsed = parse_formula(&printed).expect("printed AST must parse");
        // Semantic tree (modulo explicit `Paren` wrappers inserted where child
        // precedence required grouping) is identical, and canonical printing is
        // strictly idempotent on re-parse!
        prop_assert_eq!(strip_parens(&reparsed), strip_parens(&ast), "printed: {}", printed);
        let reprinted = reparsed.to_formula_string();
        prop_assert_eq!(&reprinted, &printed);
        prop_assert_eq!(parse_formula(&reprinted).unwrap(), reparsed);
    }
}

// ============================================================================
// 6. Interleaved edits & time-sliced recalc == full recalc from scratch
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_interleaved_edits_and_sliced_recalc_match_full_recalc_from_scratch(
        actions in prop::collection::vec((0_u8..5, 0_u32..8, 0_u32..4, 1_usize..5, -50_i32..50_i32), 5..35)
    ) {
        let mut eng = FormulaEngine::new();
        let s = SheetId(0);

        // Seed an interconnected formula network on columns B, C, D (`cols 1..=3`)
        // referencing data in column A (`col 0`), ranges, `IF` and `ROW()`.
        for r in 0_u64..8 {
            eng.edit_cell_raw(s, RowId(r), ColId(0), &format!("{}", r + 1)).unwrap();
            eng.edit_cell_raw(s, RowId(r), ColId(1), &format!("=A{}*2+$A$1", r + 1)).unwrap();
            eng.edit_cell_raw(s, RowId(r), ColId(2), &format!("=SUM(B1:B{})+ROW()", r + 1)).unwrap();
            eng.edit_cell_raw(s, RowId(r), ColId(3), &format!("=IF(C{}>20,ROUND(C{}/3,2),B{})", r + 1, r + 1, r + 1)).unwrap();
        }
        // Partially advance recalc before starting interleaved mutations!
        let _ = eng.recalc_step(3);

        for (kind, r_seed, c_seed, slice_budget, val) in actions {
            let sh = eng.workbook().sheet(s).unwrap();
            let live_r = sh.rows();
            if live_r >= 3 {
                let row = RowId(sh.row_id(r_seed % live_r.min(10)));
                let col = ColId(sh.col_id(c_seed % 4));
                match kind {
                    0 | 1 => {
                        let raw = if c_seed == 0 {
                            format!("{val}")
                        } else if val % 3 == 0 {
                            String::from("=SUM(A1:A5)*2")
                        } else {
                            format!("=A1+({val})")
                        };
                        let _ = eng.edit_cell_raw(s, row, col, &raw);
                    }
                    2 => {
                        let ins = eng.allocator_mut().alloc_run(1).unwrap();
                        let _ = eng.apply_op(Op::InsertRows {
                            sheet: s,
                            after: if r_seed % 2 == 0 { None } else { Some(row) },
                            ids: ins,
                        });
                    }
                    3 => {
                        if live_r > 4 {
                            let del = IdRun::new(row.0, 1).unwrap();
                            let _ = eng.apply_op(Op::DeleteRows {
                                sheet: s,
                                ids: vec![del],
                            });
                        }
                    }
                    _ => {
                        let r_run = IdRun::new(row.0, 1).unwrap();
                        let c_run = IdRun::new(col.0, 1).unwrap();
                        let _ = eng.apply_op(Op::ClearRange {
                            sheet: s,
                            rows: vec![r_run],
                            cols: vec![c_run],
                        });
                    }
                }
            }
            // Run ONLY a small partial slice (`1..=4` cells), leaving the rest
            // dirty while the next mutation arrives!
            let _ = eng.recalc_step(slice_budget);
        }

        // Finally drain recalculation to quiescence.
        while matches!(eng.recalc_step(3), RecalcStatus::InProgress { .. }) {}

        // Build a brand-new engine from the resulting workbook and recalculate
        // every formula from scratch: results must match bit-for-bit!
        let scratch = FormulaEngine::from_workbook_recalc_all(eng.workbook().clone());
        prop_assert_eq!(
            eng.workbook().logical_digest(),
            scratch.workbook().logical_digest()
        );
    }
}

// ============================================================================
// 7. M1b full Op + InverseOp round-trip (MoveRows/Cols E25, Undelete*, SetBlock,
//    FillFormula, Resize*, SetFreeze, SetFormat, Sheet ops E20/E28)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(90))]

    #[test]
    fn prop_m1b_ops_and_inverse_restore_exact_digest(
        steps in prop::collection::vec((0_u8..16, 0_u32..10, 0_u32..6, 0_u32..10), 1..30)
    ) {
        let mut eng = FormulaEngine::new();
        let s0 = SheetId(0);
        // Seed base data & range/single-cell formulas on Sheet1 so E25 handoff
        // and cross-sheet E20 are exercised under random sequences.
        for r in 0_u64..8 {
            eng.edit_cell_raw(s0, RowId(r), ColId(0), &format!("{}", (r + 1) * 3)).unwrap();
        }
        eng.edit_cell_raw(s0, RowId(0), ColId(1), "=SUM(A2:A6)").unwrap();
        eng.edit_cell_raw(s0, RowId(1), ColId(1), "=A3+ROW()").unwrap();
        eng.edit_cell_raw(s0, RowId(2), ColId(1), "=SUM(A1:C4)").unwrap();
        eng.recalc_all();

        let mut history = Vec::new();
        let mut deleted_row_stack: Vec<(SheetId, IdRun)> = Vec::new();
        let mut deleted_sheet_stack: Vec<SheetId> = Vec::new();

        for (kind, r_seed, c_seed, dst_seed) in steps {
            eng.recalc_all();
            let before_digest = eng.workbook().logical_digest();

            let live_sheet_ids: Vec<SheetId> = eng
                .workbook()
                .live_sheets()
                .map(reticula_core::sheet::Sheet::id)
                .collect();
            let sheet = live_sheet_ids[(dst_seed as usize) % live_sheet_ids.len()];
            let sh = eng.workbook().sheet(sheet).unwrap();
            let live_r = sh.rows();
            let live_c = sh.cols();
            if live_r < 4 || live_c < 4 {
                continue;
            }
            let r_pos = r_seed % live_r.min(10);
            let c_pos = c_seed % live_c.min(6);
            let row = RowId(sh.row_id(r_pos));
            let col = ColId(sh.col_id(c_pos));

            let op_opt: Option<Op<BoundFormula>> = match kind {
                0 => {
                    // MoveRows (including E25 endpoint moves)
                    let dst_pos = dst_seed % live_r.min(10);
                    let after = if dst_seed % 5 == 0 {
                        None
                    } else {
                        let cand = RowId(sh.row_id(dst_pos));
                        if cand == row { None } else { Some(cand) }
                    };
                    Some(Op::MoveRows {
                        sheet,
                        ids: vec![IdRun::new(row.0, 1).unwrap()],
                        after,
                    })
                }
                1 => {
                    // MoveCols
                    let dst_pos = dst_seed % live_c.min(6);
                    let after = if dst_seed % 4 == 0 {
                        None
                    } else {
                        let cand = ColId(sh.col_id(dst_pos));
                        if cand == col { None } else { Some(cand) }
                    };
                    Some(Op::MoveCols {
                        sheet,
                        ids: vec![IdRun::new(col.0, 1).unwrap()],
                        after,
                    })
                }
                2 => {
                    // DeleteRows
                    let run = IdRun::new(row.0, 1).unwrap();
                    deleted_row_stack.push((sheet, run));
                    Some(Op::DeleteRows {
                        sheet,
                        ids: vec![run],
                    })
                }
                3 => {
                    // UndeleteRows
                    deleted_row_stack
                        .pop()
                        .map(|(s_id, run)| Op::UndeleteRows {
                            sheet: s_id,
                            ids: vec![run],
                        })
                }
                4 => {
                    // ResizeRows
                    let h = if dst_seed % 3 == 0 { None } else { Some(28 + (dst_seed as u16 % 40)) };
                    Some(Op::ResizeRows {
                        sheet,
                        sizes: vec![(IdRun::new(row.0, 1).unwrap(), h)],
                    })
                }
                5 => {
                    // ResizeCols
                    let w = if dst_seed % 3 == 0 { None } else { Some(80 + (dst_seed as u16 * 10)) };
                    Some(Op::ResizeCols {
                        sheet,
                        sizes: vec![(IdRun::new(col.0, 1).unwrap(), w)],
                    })
                }
                6 => Some(Op::SetFreeze {
                    sheet,
                    frozen_rows: dst_seed % 3,
                    frozen_cols: c_seed % 3,
                }),
                7 => {
                    // SetFormat on cells / rows / cols
                    let target = match dst_seed % 3 {
                        0 => FormatTarget::Cells {
                            rows: vec![IdRun::new(row.0, 1).unwrap()],
                            cols: vec![IdRun::new(col.0, 1).unwrap()],
                        },
                        1 => FormatTarget::Rows(vec![IdRun::new(row.0, 1).unwrap()]),
                        _ => FormatTarget::Cols(vec![IdRun::new(col.0, 1).unwrap()]),
                    };
                    let fmt = FormatPatch::Set(NumberFormat::Currency {
                        symbol: CurrencySymbol::Usd,
                        decimals: 2,
                    });
                    let style = StylePatch {
                        bold: Some(dst_seed % 2 == 0),
                        align: Some(HorizontalAlign::Right),
                        text_color: ColorPatch::Set(ColorValue::Palette(2)),
                        border_top: Some(BorderWeight::Thin),
                        border_bottom: Some(BorderWeight::Thin),
                        ..StylePatch::default()
                    };
                    Some(Op::SetFormat {
                        sheet,
                        target,
                        number_format: fmt,
                        style,
                    })
                }
                8 => {
                    // SetBlock (2x2)
                    let r2 = RowId(sh.row_id((r_pos + 1) % live_r.min(10)));
                    let c2 = ColId(sh.col_id((c_pos + 1) % live_c.min(6)));
                    if r2 != row && c2 != col {
                        let f_cell = BoundFormula::parse_and_bind(
                            "=A1+1",
                            FormulaMode::Dynamic,
                            eng.workbook(),
                            sheet,
                        )
                        .unwrap();
                        Some(Op::SetBlock {
                            sheet,
                            rows: vec![IdRun::new(row.0, 1).unwrap(), IdRun::new(r2.0, 1).unwrap()],
                            cols: vec![col, c2],
                            columns_data: vec![
                                vec![
                                    CellInput::Scalar(ScalarValue::Number(f64::from(dst_seed))),
                                    CellInput::Formula {
                                        formula: f_cell,
                                        cached: None,
                                    },
                                ],
                                vec![
                                    CellInput::Scalar(ScalarValue::Text(String::from("blk"))),
                                    CellInput::Empty,
                                ],
                            ],
                        })
                    } else {
                        None
                    }
                }
                9 => {
                    // FillFormula
                    let f = BoundFormula::parse_and_bind(
                        "=A1*2+$A$1",
                        FormulaMode::Dynamic,
                        eng.workbook(),
                        sheet,
                    )
                    .unwrap();
                    let tpl = templatize_formula(&f, row, col);
                    Some(Op::FillFormula {
                        sheet,
                        rows: vec![IdRun::new(row.0, 1).unwrap()],
                        cols: vec![IdRun::new(col.0, 1).unwrap()],
                        template: tpl,
                    })
                }
                10 => {
                    // AddSheet (E28 disambiguation exercised by repeating "Data")
                    let new_id = eng.alloc_sheet_id();
                    Some(Op::AddSheet {
                        sheet: new_id,
                        name: String::from("Data"),
                        after: Some(sheet),
                    })
                }
                11 => {
                    // Cross-sheet formula edit on s0 referencing another live sheet if available
                    if live_sheet_ids.len() >= 2 {
                        let other = live_sheet_ids[1];
                        let other_name = String::from(eng.workbook().sheet(other).unwrap().name());
                        let src = format!("='{other_name}'!A1+5");
                        if let Ok(inv) = eng.edit_cell_raw(s0, RowId(3), ColId(2), &src) {
                            eng.recalc_all();
                            history.push((before_digest, inv));
                        }
                    }
                    None
                }
                12 => Some(Op::RenameSheet {
                    sheet,
                    name: format!("Tab{}", dst_seed % 4),
                }),
                13 => {
                    if live_sheet_ids.len() >= 2 {
                        let victim = live_sheet_ids[live_sheet_ids.len() - 1];
                        deleted_sheet_stack.push(victim);
                        Some(Op::DeleteSheet { sheet: victim })
                    } else {
                        None
                    }
                }
                14 => deleted_sheet_stack
                    .pop()
                    .map(|s_id| Op::UndeleteSheet { sheet: s_id }),
                _ => {
                    let after = if dst_seed % 2 == 0 { None } else { Some(s0) };
                    Some(Op::MoveSheet { sheet, after })
                }
            };

            if let Some(op) = op_opt {
                match eng.apply_op(op) {
                    Ok(inv) => {
                        eng.recalc_all();
                        history.push((before_digest, inv));
                    }
                    Err(_) => {
                        // Note: `alloc_sheet_id` on rejected `AddSheet` doesn't enter `logical_digest`;
                        // verify workbook digest was untouched on rejection!
                        prop_assert_eq!(eng.workbook().logical_digest(), before_digest);
                    }
                }
            }
        }

        // Verify incremental engine matches from-scratch recalc at peak state!
        eng.recalc_all();
        let scratch = FormulaEngine::from_workbook_recalc_all(eng.workbook().clone());
        prop_assert_eq!(
            eng.workbook().logical_digest(),
            scratch.workbook().logical_digest()
        );

        // Unwind every InverseOp in LIFO order and verify bit-exact restoration!
        while let Some((expected_digest, inv)) = history.pop() {
            eng.apply_inverse(inv).expect("inverse succeeds");
            eng.recalc_all();
            prop_assert_eq!(eng.workbook().logical_digest(), expected_digest);
        }
    }
}

// ============================================================================
// 8. `build_fill_plan` expanded formulas == per-cell `shift_formula_for_cell`
// ============================================================================

fn arb_fill_source_formula() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "=A1+B2",
        "=$A$1+A1*B$2+$C3",
        "=SUM(A1:B3)+$D$1",
        "=SUM($A$4:A1)",
        "=SUM(A$4:D1)",
        "=SUM(2:4)+SUM($2:3)",
        "=SUM(A:B)+SUM($B:A)",
        "=Data!A1+SUM(Data!$A$3:B1)",
    ])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn prop_fill_formula_plan_matches_per_cell_shift_and_recalc(
        formula_src in arb_fill_source_formula(),
        src_r in 0_u32..5,
        src_c in 0_u32..4,
        dst_r in 0_u32..6,
        row_count in 1_u32..12,
        dst_c in 0_u32..4,
        col_count in 1_u32..4,
        axis_mutations in prop::collection::vec((0_u8..4, 0_u32..12), 0..6),
    ) {
        let mut eng_plan = FormulaEngine::from_workbook_clean(Workbook::from_sheet(
            Sheet::with_axes(SheetId(0), String::from("Sheet1"), 64, 16),
            None,
        ));
        let s0 = SheetId(0);
        let s1 = eng_plan.alloc_sheet_id();
        eng_plan
            .apply_op(Op::AddSheet {
                sheet: s1,
                name: String::from("Data"),
                after: Some(s0),
            })
            .unwrap();

        for r in 0_u64..14 {
            for c in 0_u64..6 {
                eng_plan
                    .edit_cell_raw(s0, RowId(r), ColId(c), &format!("{}", r * 10 + c + 1))
                    .unwrap();
                eng_plan
                    .edit_cell_raw(s1, RowId(r), ColId(c), &format!("{}", r * 5 + c + 2))
                    .unwrap();
            }
        }

        // Apply random inserts/deletes/moves on `s0` and `s1` so axes contain
        // tombstones and non-base replica runs!
        for (mut_kind, pos_seed) in axis_mutations {
            let target_s = if pos_seed % 2 == 0 { s0 } else { s1 };
            let (rid, cid) = {
                let sh = eng_plan.workbook().sheet(target_s).unwrap();
                let p = pos_seed % sh.rows().min(12);
                let cp = pos_seed % sh.cols().min(6);
                (RowId(sh.row_id(p)), ColId(sh.col_id(cp)))
            };
            match mut_kind {
                0 => {
                    let ins = eng_plan.allocator_mut().alloc_run(2).unwrap();
                    let _ = eng_plan.apply_op(Op::InsertRows {
                        sheet: target_s,
                        after: Some(rid),
                        ids: ins,
                    });
                }
                1 => {
                    let _ = eng_plan.apply_op(Op::DeleteRows {
                        sheet: target_s,
                        ids: vec![IdRun::new(rid.0, 1).unwrap()],
                    });
                }
                2 => {
                    let ins = eng_plan.allocator_mut().alloc_run(1).unwrap();
                    let _ = eng_plan.apply_op(Op::InsertCols {
                        sheet: target_s,
                        after: Some(cid),
                        ids: ins,
                    });
                }
                _ => {
                    let _ = eng_plan.apply_op(Op::MoveRows {
                        sheet: target_s,
                        ids: vec![IdRun::new(rid.0, 1).unwrap()],
                        after: None,
                    });
                }
            }
        }
        eng_plan.recalc_all();

        let source_bound = BoundFormula::parse_and_bind(
            formula_src,
            FormulaMode::Dynamic,
            eng_plan.workbook(),
            s0,
        )
        .unwrap();

        let mut eng_individual = eng_plan.clone();

        let ops = build_fill_plan(
            &source_bound,
            eng_plan.workbook(),
            s0,
            src_r,
            src_c,
            FillTargetRect {
                dst_sheet: s0,
                first_row_pos: dst_r,
                row_count,
                first_col_pos: dst_c,
                col_count,
            },
        );
        eng_plan.apply_txn(ops).unwrap();
        eng_plan.recalc_all();

        // Reference oracle: shift and write each cell individually!
        let mut edits = Vec::new();
        for c_off in 0..col_count {
            let cp = dst_c + c_off;
            let cid = ColId(eng_individual.workbook().sheet(s0).unwrap().col_id(cp));
            for r_off in 0..row_count {
                let rp = dst_r + r_off;
                let rid = RowId(eng_individual.workbook().sheet(s0).unwrap().row_id(rp));
                let shifted = shift_formula_for_cell(
                    &source_bound,
                    eng_individual.workbook(),
                    FormulaShiftSpec {
                        src_sheet: s0,
                        src_row_pos: src_r,
                        src_col_pos: src_c,
                        dst_sheet: s0,
                        dst_row_pos: rp,
                        dst_col_pos: cp,
                    },
                );
                edits.push(CellEdit {
                    row: rid,
                    col: cid,
                    input: CellInput::Formula {
                        formula: shifted,
                        cached: None,
                    },
                    format: FormatPatch::Keep,
                });
            }
        }
        eng_individual
            .apply_op(Op::SetCells { sheet: s0, edits })
            .unwrap();
        eng_individual.recalc_all();

        prop_assert_eq!(
            eng_plan.workbook().logical_digest(),
            eng_individual.workbook().logical_digest()
        );
    }
}

// ============================================================================
// 9. `wire::v1` operations & `RTCLSNAP` v1 snapshot round-trip (`14-m1b.md` 8.1 ③)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_wire_v1_ops_and_snapshot_round_trip(
        steps in prop::collection::vec((0_u8..10, 0_u32..10, 0_u32..5, -500_i32..500), 1..20),
        replica in 1_u32..100,
        last_client_seq in 0_u64..10_000,
        max_id_counter in 0_u32..100_000,
    ) {
        let mut eng_direct = FormulaEngine::from_workbook_clean(Workbook::from_sheet(
            Sheet::with_axes(SheetId(0), String::from("Sheet1"), 48, 12),
            None,
        ));
        let mut eng_replayed = eng_direct.clone();
        let s0 = SheetId(0);

        for (r, c) in [(0_u64, 0_u64), (1, 0), (2, 0), (0, 1), (1, 1)] {
            let raw = format!("{}", (r + 1) * 10 + c);
            eng_direct.edit_cell_raw(s0, RowId(r), ColId(c), &raw).unwrap();
            eng_replayed.edit_cell_raw(s0, RowId(r), ColId(c), &raw).unwrap();
        }
        eng_direct.edit_cell_raw(s0, RowId(0), ColId(2), "=SUM(A1:B3)").unwrap();
        eng_replayed.edit_cell_raw(s0, RowId(0), ColId(2), "=SUM(A1:B3)").unwrap();
        eng_direct.recalc_all();
        eng_replayed.recalc_all();

        for (kind, r_seed, c_seed, num_seed) in steps {
            let (row, col, r_after) = {
                let sh = eng_direct.workbook().sheet(s0).unwrap();
                let r_pos = r_seed % sh.rows().min(12);
                let c_pos = c_seed % sh.cols().min(6);
                let after_pos = (r_seed + 3) % sh.rows().min(12);
                (
                    RowId(sh.row_id(r_pos)),
                    ColId(sh.col_id(c_pos)),
                    RowId(sh.row_id(after_pos)),
                )
            };

            let op_opt: Option<Op<BoundFormula>> = match kind {
                0 => Some(Op::SetCells {
                    sheet: s0,
                    edits: vec![CellEdit {
                        row,
                        col,
                        input: CellInput::Scalar(ScalarValue::Number(f64::from(num_seed) / 4.0)),
                        format: FormatPatch::Set(NumberFormat::Fixed {
                            decimals: 2,
                            thousands: true,
                        }),
                    }],
                }),
                1 => {
                    let f = BoundFormula::parse_and_bind(
                        "=A1*2+$B$1",
                        FormulaMode::Dynamic,
                        eng_direct.workbook(),
                        s0,
                    )
                    .unwrap();
                    let tpl = templatize_formula(&f, row, col);
                    Some(Op::FillFormula {
                        sheet: s0,
                        rows: vec![IdRun::new(row.0, 1).unwrap()],
                        cols: vec![IdRun::new(col.0, 1).unwrap()],
                        template: tpl,
                    })
                }
                2 => {
                    let ins = eng_direct.allocator_mut().alloc_run(2).unwrap();
                    let _ = eng_replayed.allocator_mut().alloc_run(2);
                    Some(Op::InsertRows {
                        sheet: s0,
                        after: Some(row),
                        ids: ins,
                    })
                }
                3 => {
                    if row != r_after {
                        Some(Op::MoveRows {
                            sheet: s0,
                            ids: vec![IdRun::new(row.0, 1).unwrap()],
                            after: Some(r_after),
                        })
                    } else {
                        None
                    }
                }
                4 => Some(Op::ResizeRows {
                    sheet: s0,
                    sizes: vec![(IdRun::new(row.0, 1).unwrap(), Some(28 + (r_seed as u16 * 2)))],
                }),
                5 => Some(Op::SetFreeze {
                    sheet: s0,
                    frozen_rows: r_seed % 4,
                    frozen_cols: c_seed % 3,
                }),
                6 => Some(Op::SetFormat {
                    sheet: s0,
                    target: FormatTarget::Rows(vec![IdRun::new(row.0, 1).unwrap()]),
                    number_format: FormatPatch::Set(NumberFormat::Currency {
                        symbol: CurrencySymbol::Eur,
                        decimals: 2,
                    }),
                    style: StylePatch {
                        italic: Some(true),
                        fill_color: ColorPatch::Set(ColorValue::Rgb(0x112233)),
                        border_left: Some(BorderWeight::Thin),
                        ..StylePatch::default()
                    },
                }),
                7 => Some(Op::AddSheet {
                    sheet: eng_direct.alloc_sheet_id(),
                    name: String::from("Extra Sheet"),
                    after: Some(s0),
                }),
                8 => Some(Op::ClearRange {
                    sheet: s0,
                    rows: vec![IdRun::new(row.0, 1).unwrap()],
                    cols: vec![IdRun::new(col.0, 1).unwrap()],
                }),
                _ => Some(Op::SetCells {
                    sheet: s0,
                    edits: vec![CellEdit {
                        row,
                        col,
                        input: CellInput::Scalar(ScalarValue::Text(format!("s{num_seed}"))),
                        format: FormatPatch::Keep,
                    }],
                }),
            };

            if let Some(op) = op_opt {
                if matches!(op, Op::AddSheet { .. }) {
                    let _ = eng_replayed.alloc_sheet_id();
                }
                let wire_op = WireOpV1::from(&op);
                let encoded = postcard::to_allocvec(&wire_op).unwrap();
                let decoded_wire: WireOpV1 = postcard::from_bytes(&encoded).unwrap();
                let roundtripped_op = Op::<BoundFormula>::try_from(decoded_wire).unwrap();

                let r1 = eng_direct.apply_op(op);
                let r2 = eng_replayed.apply_op(roundtripped_op);
                prop_assert_eq!(r1.is_ok(), r2.is_ok());
            }
        }

        eng_direct.recalc_all();
        eng_replayed.recalc_all();
        prop_assert_eq!(
            eng_direct.workbook().logical_digest(),
            eng_replayed.workbook().logical_digest()
        );

        let wm = vec![WireReplicaWatermarkV1 {
            replica,
            last_client_seq,
            max_id_counter,
        }];
        let snap_bytes = encode_snapshot_v1(eng_direct.workbook(), &wm).unwrap();
        let decoded = decode_snapshot_any(&snap_bytes).unwrap();
        prop_assert_eq!(decoded.watermarks, wm);
        prop_assert_eq!(
            decoded.engine.workbook().logical_digest(),
            eng_direct.workbook().logical_digest()
        );
    }
}

// ============================================================================
// 10. `RTCLWAL1` tail truncation, zero-padding & mid-file corruption (`14-m1b.md` 8.1 ④)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_wal_v1_tail_truncation_zero_pad_and_mid_corruption(
        base_seq in 0_u64..500,
        values in prop::collection::vec(-10_000_i32..10_000, 2..8),
        extra_tail_cut_frac in 0_usize..1000,
        zero_pad_len in 0_usize..512,
        mid_flip_frac in 0_usize..1000,
        flip_mask in 1_u8..=255,
    ) {
        let mut records = Vec::with_capacity(values.len());
        let mut ref_eng = FormulaEngine::new();
        for (i, &v) in values.iter().enumerate() {
            let seq = base_seq + (i as u64) + 1;
            let op: Op<BoundFormula> = Op::SetCells {
                sheet: SheetId(0),
                edits: vec![CellEdit {
                    row: RowId(i as u64),
                    col: ColId(0),
                    input: CellInput::Scalar(ScalarValue::Number(f64::from(v) / 10.0)),
                    format: FormatPatch::Keep,
                }],
            };
            ref_eng.apply_op(op.clone()).unwrap();
            records.push(WireWalRecordV1 {
                seq,
                timestamp_ms: 1_760_000_000_000 + seq,
                label: format!("Txn {seq}"),
                ops: vec![WireOpV1::from(&op)],
            });
        }
        ref_eng.recalc_all();
        let expected_digest = ref_eng.workbook().logical_digest();
        let last_flushed_seq = base_seq + (records.len() as u64);

        let committed_bytes = encode_wal_complete(base_seq, &records).unwrap();

        // (a) Append an extra unflushed record (`last_flushed_seq + 1`), truncate at
        //     an arbitrary byte offset inside it, and optionally append `0x00` padding:
        //     all committed `records` MUST be recovered intact!
        let extra_op: Op<BoundFormula> = Op::SetCells {
            sheet: SheetId(0),
            edits: vec![CellEdit {
                row: RowId(99),
                col: ColId(0),
                input: CellInput::Scalar(ScalarValue::Number(999.0)),
                format: FormatPatch::Keep,
            }],
        };
        let extra_frame = encode_wal_record(&WireWalRecordV1 {
            seq: last_flushed_seq + 1,
            timestamp_ms: 1_760_000_999_999,
            label: String::from("Unflushed crash tail"),
            ops: vec![WireOpV1::from(&extra_op)],
        })
        .unwrap();

        let cut = extra_tail_cut_frac % extra_frame.len(); // `0..extra_frame.len() - 1`
        let mut with_tail = committed_bytes.clone();
        with_tail.extend_from_slice(&extra_frame[..cut]);
        if cut == 0 && zero_pad_len > 0 {
            with_tail.extend(std::iter::repeat_n(0_u8, zero_pad_len));
        }

        let scan = scan_wal(&with_tail, base_seq, last_flushed_seq).unwrap();
        prop_assert_eq!(scan.records.len(), records.len());
        prop_assert_eq!(scan.valid_bytes, committed_bytes.len());
        if cut == 0 && zero_pad_len == 0 {
            prop_assert_eq!(scan.tail, WalTailDisposition::Clean);
        } else if cut == 0 {
            prop_assert_eq!(
                scan.tail,
                WalTailDisposition::ZeroPaddedTail {
                    valid_bytes: committed_bytes.len()
                }
            );
        } else {
            let tail_ok = matches!(
                scan.tail,
                WalTailDisposition::TornFinalRecord { .. }
                    | WalTailDisposition::ZeroPaddedTail { .. }
            );
            prop_assert!(tail_ok);
        }

        let mut replayed = FormulaEngine::new();
        replay_wal_records(&mut replayed, &scan.records, 0, true).unwrap();
        prop_assert_eq!(replayed.workbook().logical_digest(), expected_digest);

        // (b) Flip any non-zero mask in any mid-file byte (header, meta, or any
        //     record `0 .. records.len() - 1`): MUST be detected as corruption!
        let prefix_before_last =
            encode_wal_complete(base_seq, &records[..records.len() - 1]).unwrap();
        let flip_pos = mid_flip_frac % prefix_before_last.len();
        let mut tampered = committed_bytes;
        tampered[flip_pos] ^= flip_mask;
        let err = scan_wal(&tampered, base_seq, last_flushed_seq).unwrap_err();
        let corrupt_ok = matches!(
            err,
            WalCorruption::InvalidHeader
                | WalCorruption::BaseSeqMismatch { .. }
                | WalCorruption::NonContiguousSeq { .. }
                | WalCorruption::MidFileCorruption { .. }
        );
        prop_assert!(corrupt_ok);
    }
}
