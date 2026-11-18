//! Dedicated WebAssembly benchmark entrypoints (`--features bench`).
//!
//! Compiled only for `cargo xtask bench` and `s9-builds`; excluded from the
//! production `reticula-wasm` bundle so it adds zero bytes to shipped assets.

use reticula_core::id::{ColId, RowId, SheetId};
use reticula_core::ops::{FormulaMode, Op, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::value::Value;
use reticula_formula::ast::BinaryOp;
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};
use reticula_formula::engine::FormulaEngine;
use wasm_bindgen::prelude::*;

/// Stateful benchmark fixture driven from a Cross-Origin-Isolated browser page.
#[wasm_bindgen]
pub struct WasmBench {
    engine: FormulaEngine,
    step_counter: u32,
}

#[wasm_bindgen]
impl WasmBench {
    /// Prepares B2: 1,000,000 rows in column `A` + `=SUM(A1:A1000000)` in `B1`.
    #[must_use]
    pub fn new_b2() -> Self {
        let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
        for r in 0_u32..1_000_000 {
            sheet.set_value_by_id(
                RowId(u64::from(r)),
                ColId(0),
                Value::Number(f64::from(r % 97) * 0.5),
            );
        }
        let wb = Workbook::from_sheet(sheet, None);
        let mut engine = FormulaEngine::from_workbook_clean(wb);
        let _ = engine.edit_cell_raw(SheetId(0), RowId(0), ColId(1), "=SUM(A1:A1000000)");
        engine.recalc_all();
        Self {
            engine,
            step_counter: 0,
        }
    }

    /// Runs one B2 iteration (mutates `A500001` and recalculates `=SUM(A1:A1000000)`).
    pub fn step_b2(&mut self) -> f64 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let v = f64::from(self.step_counter);
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), RowId(500_000), ColId(0), &format!("{v}"));
        self.engine.recalc_all();
        read_num(&self.engine, RowId(0), ColId(1))
    }

    /// Prepares B3: 1,000,000 formulas `=B{r}*C{r}+$Z$1`.
    #[must_use]
    pub fn new_b3() -> Self {
        Self {
            engine: build_1m_row_local(),
            step_counter: 0,
        }
    }

    /// Runs one B3 iteration (mutates `Z1` and recalculates all 1,000,000 formulas).
    pub fn step_b3(&mut self) -> u32 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let v = f64::from(self.step_counter) + 1.5;
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), RowId(0), ColId(25), &format!("{v}"));
        self.engine.recalc_all()
    }

    /// Prepares B4: 100,000 formulas `=A{r}*$Z$1`.
    #[must_use]
    pub fn new_b4() -> Self {
        let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
        sheet.set_value_by_id(RowId(0), ColId(25), Value::Number(1.0));
        for r in 1_u32..=100_000 {
            sheet.set_value_by_id(
                RowId(u64::from(r)),
                ColId(0),
                Value::Number(f64::from(r) * 0.25),
            );
        }
        let mut wb = Workbook::from_sheet(sheet, None);
        for r in 1_u32..=100_000 {
            let row = RowId(u64::from(r));
            let f = BoundFormula {
                mode: FormulaMode::Dynamic,
                root: BoundExpr::Binary {
                    op: BinaryOp::Mul,
                    left: Box::new(cell_expr(row, ColId(0), false)),
                    right: Box::new(cell_expr(RowId(0), ColId(25), true)),
                },
            };
            wb.insert_seeded_formula(
                SheetId(0),
                row,
                ColId(1),
                f,
                Value::Number(f64::from(r) * 0.25),
            );
        }
        Self {
            engine: FormulaEngine::from_workbook_clean(wb),
            step_counter: 0,
        }
    }

    /// Runs one B4 iteration (mutates `Z1`, propagates dirty to 100,000 formulas,
    /// and recalculates all 100,000).
    pub fn step_b4(&mut self) -> u32 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let factor = 1.0 + f64::from(self.step_counter) * 0.01;
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), RowId(0), ColId(25), &format!("{factor}"));
        self.engine.recalc_all()
    }

    /// Prepares B5: 1,000,000-row running balance chain `C1 = B1`, `C{r} = C{r-1} + B{r}`.
    #[must_use]
    pub fn new_b5() -> Self {
        let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
        for r in 0_u32..1_000_000 {
            sheet.set_value_by_id(RowId(u64::from(r)), ColId(1), Value::Number(1.0));
            sheet.set_value_by_id(
                RowId(u64::from(r)),
                ColId(2),
                Value::Number(f64::from(r + 1)),
            );
        }
        let mut wb = Workbook::from_sheet(sheet, None);
        wb.insert_seeded_formula(
            SheetId(0),
            RowId(0),
            ColId(2),
            BoundFormula {
                mode: FormulaMode::Dynamic,
                root: cell_expr(RowId(0), ColId(1), false),
            },
            Value::Number(1.0),
        );
        for r in 1_u32..1_000_000 {
            let prev = RowId(u64::from(r - 1));
            let cur = RowId(u64::from(r));
            let f = BoundFormula {
                mode: FormulaMode::Dynamic,
                root: BoundExpr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(cell_expr(prev, ColId(2), false)),
                    right: Box::new(cell_expr(cur, ColId(1), false)),
                },
            };
            wb.insert_seeded_formula(
                SheetId(0),
                cur,
                ColId(2),
                f,
                Value::Number(f64::from(r + 1)),
            );
        }
        Self {
            engine: FormulaEngine::from_workbook_clean(wb),
            step_counter: 0,
        }
    }

    /// Runs one B5 iteration (mutates `B1` and evaluates the 1,000,000-cell chain
    /// without recursion).
    pub fn step_b5(&mut self) -> u32 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let v = f64::from(self.step_counter) + 2.0;
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), RowId(0), ColId(1), &format!("{v}"));
        self.engine.recalc_all()
    }

    /// Prepares B7 (`group = 1`: 1M row-local `=B*C+$Z$1` + `=SUM(B2:B100001)`;
    /// `group = 2`: 1M row-local + cross-insertion-point `=SUM(B1:B1000000)`;
    /// `group = 3`: 1M rows with 100,000 structure-sensitive `=ROW()+B{r}` formulas).
    #[must_use]
    pub fn new_b7(group: u8) -> Self {
        let mut engine = if group == 3 {
            let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
            for r in 0_u32..1_000_000 {
                sheet.set_value_by_id(RowId(u64::from(r)), ColId(1), Value::Number(1.0));
            }
            let mut wb = Workbook::from_sheet(sheet, None);
            for r in 1_u32..=100_000 {
                let row = RowId(u64::from(r));
                let f = BoundFormula {
                    mode: FormulaMode::Dynamic,
                    root: BoundExpr::Binary {
                        op: BinaryOp::Add,
                        left: Box::new(BoundExpr::Call {
                            name: String::from("ROW"),
                            args: Vec::new(),
                        }),
                        right: Box::new(cell_expr(row, ColId(1), false)),
                    },
                };
                wb.insert_seeded_formula(
                    SheetId(0),
                    row,
                    ColId(3),
                    f,
                    Value::Number(f64::from(r + 2)),
                );
            }
            let mut eng = FormulaEngine::from_workbook_clean(wb);
            let _ = eng.edit_cell_raw(SheetId(0), RowId(0), ColId(4), "=SUM(B2:B100001)");
            eng.recalc_all();
            eng
        } else {
            let mut eng = build_1m_row_local();
            let sum_expr = if group == 2 {
                "=SUM(B1:B1000000)"
            } else {
                "=SUM(B2:B100001)"
            };
            let _ = eng.edit_cell_raw(SheetId(0), RowId(0), ColId(4), sum_expr);
            eng.recalc_all();
            eng
        };
        let _ = engine.take_dirty_changes();
        Self {
            engine,
            step_counter: 0,
        }
    }

    /// B7a: inserts `batch` rows one by one (`group == 2` inserts after `RowId(0)`
    /// inside `B1:B1000000`; `group == 1` and `3` insert at the very top `after: None`)
    /// and recalculates any dirtied formulas.
    pub fn step_b7a(&mut self, group: u8, batch: u32) -> u32 {
        let after = if group == 2 { Some(RowId(0)) } else { None };
        let mut total_recalc = 0_u32;
        for _ in 0..batch.max(1) {
            if let Some(ids) = self.engine.allocator_mut().alloc_run(1) {
                let _ = self.engine.apply_op(Op::InsertRows {
                    sheet: SheetId(0),
                    after,
                    ids,
                });
                total_recalc = total_recalc.saturating_add(self.engine.recalc_all());
            }
        }
        total_recalc
    }

    /// B7b: edits a range-referenced cell (`RowId(50), ColId(1)`) immediately
    /// following B7a and recalculates.
    pub fn step_b7b(&mut self) -> u32 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let v = 10.0 + f64::from(self.step_counter);
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), RowId(50), ColId(1), &format!("{v}"));
        self.engine.recalc_all()
    }

    /// Decodes an `RTCLSNAP` v0 snapshot buffer into a live `FormulaEngine` and
    /// returns the primary sheet's live row count (used for real-dataset load
    /// benchmarks in `13-m1a.md` 8.3).
    pub fn decode_snapshot_rows(bytes: &[u8]) -> Result<u32, JsError> {
        let _ = reticula_core::memory::try_reserve_heap(bytes.len().saturating_mul(16));
        let (eng, _) = FormulaEngine::decode_snapshot(bytes)
            .map_err(|e| JsError::new(&format!("decode_snapshot failed: {e:?}")))?;
        let rows = eng.workbook().sheet(SheetId(0)).map_or(0, Sheet::rows);
        Ok(rows)
    }
}

fn build_1m_row_local() -> FormulaEngine {
    let mut sheet = Sheet::standard(SheetId(0), String::from("Sheet1"));
    sheet.set_value_by_id(RowId(0), ColId(25), Value::Number(1.0));
    for r in 0_u32..1_000_000 {
        let row = RowId(u64::from(r));
        sheet.set_value_by_id(row, ColId(1), Value::Number(2.0));
        sheet.set_value_by_id(row, ColId(2), Value::Number(3.0));
    }
    let mut wb = Workbook::from_sheet(sheet, None);
    for r in 0_u32..1_000_000 {
        let row = RowId(u64::from(r));
        let bc = BoundExpr::Binary {
            op: BinaryOp::Mul,
            left: Box::new(cell_expr(row, ColId(1), false)),
            right: Box::new(cell_expr(row, ColId(2), false)),
        };
        let f = BoundFormula {
            mode: FormulaMode::Dynamic,
            root: BoundExpr::Binary {
                op: BinaryOp::Add,
                left: Box::new(bc),
                right: Box::new(cell_expr(RowId(0), ColId(25), true)),
            },
        };
        wb.insert_seeded_formula(SheetId(0), row, ColId(3), f, Value::Number(7.0));
    }
    FormulaEngine::from_workbook_clean(wb)
}

fn cell_expr(row: RowId, col: ColId, abs: bool) -> BoundExpr {
    BoundExpr::Reference(BoundRef::Cell {
        sheet: None,
        missing_sheet: None,
        row,
        col,
        abs_col: abs,
        abs_row: abs,
    })
}

fn read_num(engine: &FormulaEngine, row: RowId, col: ColId) -> f64 {
    match engine
        .workbook()
        .sheet(SheetId(0))
        .map(|s| s.value_by_id(row, col))
    {
        Some(Value::Number(n)) => n,
        _ => 0.0,
    }
}
