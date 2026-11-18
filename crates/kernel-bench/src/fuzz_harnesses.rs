//! Structured and byte-level fuzz harnesses shared by `cargo-fuzz` (`fuzz/`)
//! and `cargo xtask fuzz` (`09-quality.md` & `13-m1a.md` 7.3).
//!
//! Covers:
//! 1. `fuzz_one_parser`: arbitrary byte/UTF-8 inputs to lexer, parser, pretty
//!    printer and E30 input type inference;
//! 2. `fuzz_one_snapshot`: arbitrary byte slices fed into `decode_snapshot`;
//! 3. `fuzz_one_ops`: `arbitrary::Unstructured`-generated valid and adversarial
//!    operation sequences applied to a small workbook;
//! 4. `fuzz_one_eval`: arbitrary `BoundExpr` syntax trees + arbitrary cell
//!    contents evaluated in `FormulaEngine`.

use arbitrary::Unstructured;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId, pack_id};
use reticula_core::input::infer_input;
use reticula_core::ops::{
    CellEdit, CellInput, FormatPatch, FormulaMode, Op, RawFormula, ScalarValue, Workbook,
};
use reticula_core::snapshot::decode_snapshot;
use reticula_core::value::CalcError;
use reticula_formula::ast::{BinaryOp, Literal, UnaryOp};
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};
use reticula_formula::engine::FormulaEngine;
use reticula_formula::lexer::tokenize;
use reticula_formula::parser::{parse_expr, parse_formula};

/// Target 1: formula lexer, Pratt parser, AST printer, and E30 input inference.
pub fn fuzz_one_parser(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let _ = infer_input(&text);
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

/// Target 2: untrusted `RTCLSNAP` v0 binary snapshot decoder.
pub fn fuzz_one_snapshot(data: &[u8]) {
    let _ = decode_snapshot::<RawFormula>(data);
    let _ = FormulaEngine::decode_snapshot(data);
}

/// Target 3: structured valid + invalid operation sequences on a workbook.
pub fn fuzz_one_ops(data: &[u8]) {
    let mut u = Unstructured::new(data);
    let mut wb: Workbook<RawFormula> = Workbook::new();
    let count: u8 = u.int_in_range(0..=32).unwrap_or(0);

    for _ in 0..count {
        let sheet = if u.ratio(7, 8).unwrap_or(true) {
            SheetId(0)
        } else {
            SheetId(u.arbitrary().unwrap_or(1))
        };
        let kind: u8 = u.int_in_range(0..=5).unwrap_or(0);
        let op = match kind {
            0 => {
                let n: usize = u.int_in_range(0..=16).unwrap_or(0);
                let mut edits = Vec::with_capacity(n);
                for _ in 0..n {
                    let row = arb_row_id(&mut u);
                    let col = arb_col_id(&mut u);
                    let input = arb_raw_cell_input(&mut u);
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
                    let len = u.int_in_range(1_u32..=8).unwrap_or(1);
                    wb.allocator_mut().alloc_run(len).unwrap_or(IdRun {
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
            _ => Op::DeleteCols {
                sheet,
                ids: vec![arb_id_run(&mut u)],
            },
        };

        let before = wb.logical_digest();
        match wb.apply(op) {
            Ok(inv) => {
                if u.ratio(1, 4).unwrap_or(false) {
                    let _ = wb.apply_inverse(inv);
                }
            }
            Err(_) => {
                assert_eq!(wb.logical_digest(), before);
            }
        }
    }
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

fn arb_raw_cell_input(u: &mut Unstructured<'_>) -> CellInput<RawFormula> {
    match u.int_in_range(0_u8..=2).unwrap_or(0) {
        0 => CellInput::Empty,
        1 => CellInput::Scalar(arb_scalar(u)),
        _ => {
            let len = u.int_in_range(0_usize..=32).unwrap_or(0);
            let bytes = u.bytes(len).unwrap_or(&[]);
            let mut s = String::from("=");
            s.push_str(&String::from_utf8_lossy(bytes));
            CellInput::Formula {
                formula: RawFormula {
                    source: s,
                    mode: FormulaMode::Dynamic,
                },
                cached: None,
            }
        }
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
            const FUNCS: [&str; 24] = [
                "SUM", "AVERAGE", "COUNT", "COUNTA", "MIN", "MAX", "IF", "AND", "OR", "NOT",
                "IFERROR", "IFNA", "ABS", "ROUND", "INT", "TRUNC", "MOD", "SQRT", "POWER", "LEN",
                "TRIM", "MID", "ISBLANK", "ROW",
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
