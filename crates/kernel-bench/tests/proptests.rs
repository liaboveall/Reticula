//! Property-based tests (`proptest`) for `reticula-core` and `reticula-formula`
//! (`13-m1a.md` 7.1).

use proptest::prelude::*;
use reticula_core::axis::Axis;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdAllocator, IdRun, RowId, SheetId, pack_id};
use reticula_core::ops::{CellEdit, CellInput, FormatPatch, Op, ScalarValue};
use reticula_core::value::CalcError;
use reticula_formula::address::CellAddr;
use reticula_formula::ast::{BinaryOp, Expr, Literal, UnaryOp};
use reticula_formula::binder::BoundFormula;
use reticula_formula::engine::{FormulaEngine, RecalcStatus};
use reticula_formula::parser::parse_formula;

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
