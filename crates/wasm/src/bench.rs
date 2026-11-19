//! Dedicated WebAssembly benchmark entrypoints (`--features bench`).
//!
//! Compiled only for `cargo xtask bench` and `s9-builds`; excluded from the
//! production `reticula-wasm` bundle so it adds zero bytes to shipped assets.

use std::collections::BTreeMap;

use reticula_core::axis::AxisRun;
use reticula_core::format::NumberFormat;
use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{FormulaMode, Op, Workbook};
use reticula_core::sheet::Sheet;
use reticula_core::store::{BLOCK_ROWS, Block, Column, SlotMap};
use reticula_core::strings::StringTable;
use reticula_core::value::Value;
use reticula_core::viewport::{self, Page};
use reticula_formula::ast::{BinaryOp, Literal};
use reticula_formula::binder::{BoundExpr, BoundFormula, BoundRef};
use reticula_formula::engine::FormulaEngine;
use reticula_wire::snapshot::{SnapshotEncoder, SnapshotStep, decode_snapshot_any};
use wasm_bindgen::prelude::*;

/// Stateful benchmark fixture driven from a Cross-Origin-Isolated browser page
/// or dedicated Web Worker (`09-quality.md`, `13-m1a.md` 7.4, `14-m1b.md` 8.3).
#[wasm_bindgen]
pub struct WasmBench {
    engine: FormulaEngine,
    step_counter: u32,
    snapshot_encoder: Option<SnapshotEncoder>,
    pending_chunk: Option<Vec<u8>>,
    text_scratch: String,
}

#[wasm_bindgen]
impl WasmBench {
    fn from_engine(engine: FormulaEngine) -> Self {
        Self {
            engine,
            step_counter: 0,
            snapshot_encoder: None,
            pending_chunk: None,
            text_scratch: String::with_capacity(64),
        }
    }

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
        Self::from_engine(engine)
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
        Self::from_engine(build_1m_row_local())
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
        Self::from_engine(FormulaEngine::from_workbook_clean(wb))
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
        Self::from_engine(FormulaEngine::from_workbook_clean(wb))
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

    /// Prepares B6 (`14-m1b.md` 8.3): `formula_count` (default `100,000`) exact
    /// bitwise `=VLOOKUP(D{r}, $A$2:$B$100001, 2, FALSE)` formulas over a
    /// `table_rows` (default `100,000`) lookup table.
    #[must_use]
    pub fn new_b6(table_rows: u32, formula_count: u32) -> Self {
        let rows = table_rows.max(10);
        let formulas = formula_count.clamp(1, rows);
        let mut sheet = Sheet::with_axes(SheetId(0), String::from("Sheet1"), rows + 2, 6);
        for r in 1_u32..=rows {
            let row = RowId(u64::from(r));
            sheet.set_value_by_id(row, ColId(0), Value::Number(f64::from(r)));
            sheet.set_value_by_id(row, ColId(1), Value::Number(f64::from(r) * 1.5));
        }
        for r in 1_u32..=formulas {
            let row = RowId(u64::from(r));
            let key = ((r.wrapping_mul(7919)) % rows) + 1;
            sheet.set_value_by_id(row, ColId(3), Value::Number(f64::from(key)));
            sheet.set_value_by_id(row, ColId(4), Value::Number(f64::from(key) * 1.5));
        }
        let mut wb = Workbook::from_sheet(sheet, None);
        let last_row = RowId(u64::from(rows));
        for r in 1_u32..=formulas {
            let row = RowId(u64::from(r));
            let f = BoundFormula {
                mode: FormulaMode::Dynamic,
                root: BoundExpr::Call {
                    name: String::from("VLOOKUP"),
                    args: vec![
                        cell_expr(row, ColId(3), false),
                        BoundExpr::Reference(BoundRef::Area {
                            sheet: None,
                            missing_sheet: None,
                            first_row: RowId(1),
                            first_col: ColId(0),
                            last_row,
                            last_col: ColId(1),
                            abs_first_col: true,
                            abs_first_row: true,
                            abs_last_col: true,
                            abs_last_row: true,
                        }),
                        BoundExpr::Literal(Literal::Number(2.0)),
                        BoundExpr::Literal(Literal::Bool(false)),
                    ],
                },
            };
            let key = ((r.wrapping_mul(7919)) % rows) + 1;
            wb.insert_seeded_formula(
                SheetId(0),
                row,
                ColId(4),
                f,
                Value::Number(f64::from(key) * 1.5),
            );
        }
        Self::from_engine(FormulaEngine::from_workbook_clean(wb))
    }

    /// Runs one B6 full evaluation by mutating `B2` inside `$A$2:$B$100001` and
    /// recalculating every dependent `VLOOKUP`.
    pub fn step_b6(&mut self) -> u32 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let v = 1.5 + f64::from(self.step_counter) * 0.25;
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), RowId(1), ColId(1), &format!("{v}"));
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
        Self::from_engine(engine)
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

    /// Builds a B8 memory fixture (`14-m1b.md` 8.3):
    /// - `case = 1`: `10,000,000` numeric cells (`1M × 10`);
    /// - `case = 2`: `1,000,000` string cells (500 distinct repeated strings);
    /// - `case = 3`: `1,000,000` string cells (1,000,000 unique short strings);
    /// - `case = 4`: `1,000,000` formulas `=B{r}*C{r}+$Z$1`.
    #[must_use]
    pub fn new_b8(case: u8) -> Self {
        let engine = match case {
            1 => build_dense_grid(1_000_000, 10, 0),
            2 => build_string_grid(1_000_000, 500),
            3 => build_string_grid(1_000_000, 1_000_000),
            _ => build_1m_row_local(),
        };
        Self::from_engine(engine)
    }

    /// Kernel `estimated_bytes` of the active workbook (for B8 reporting).
    #[must_use]
    pub fn estimated_bytes(&self) -> f64 {
        self.engine.workbook().estimated_bytes() as f64
    }

    /// Unique interned strings in Sheet 0 (for B8 reporting).
    #[must_use]
    pub fn unique_strings(&self) -> u32 {
        self.engine
            .workbook()
            .sheet(SheetId(0))
            .map_or(0, |s| u32::try_from(s.strings().len()).unwrap_or(u32::MAX))
    }

    /// Total formula count in the workbook.
    #[must_use]
    pub fn formula_count(&self) -> u32 {
        u32::try_from(self.engine.workbook().formulas().len()).unwrap_or(u32::MAX)
    }

    /// Prepares a B1 / B11 (`1M × 20`, with `0` or `1M` formulas) or B12
    /// (`1M × 50`, with `100k` formulas) dense workbook (`14-m1b.md` 8.3).
    #[must_use]
    pub fn new_storage_fixture(rows: u32, cols: u32, formula_rows: u32) -> Self {
        Self::from_engine(build_dense_grid(rows, cols, formula_rows))
    }

    /// Begins a Copy-on-Write `RTCLSNAP` v1 background snapshot stream (B11 & B12).
    pub fn start_snapshot_stream(&mut self) {
        self.pending_chunk = None;
        self.snapshot_encoder = Some(SnapshotEncoder::new(self.engine.workbook(), Vec::new()));
    }

    /// Advances the active CoW snapshot stream by at most `max_blocks` column blocks.
    /// Returns `[status, offsetOrTotalLen]` where `status`:
    /// - `0` = progress only (no bytes to write);
    /// - `1` = chunk ready (`take_snapshot_chunk()`, write at `offset`);
    /// - `2` = finished (`take_snapshot_chunk()` holds header for offset `0`, truncate to `totalLen`).
    pub fn step_snapshot_stream(&mut self, max_blocks: u32) -> Result<Vec<f64>, JsError> {
        let Some(enc) = self.snapshot_encoder.as_mut() else {
            return Err(JsError::new("No active snapshot stream"));
        };
        let step = enc
            .encode_step(self.engine.workbook(), max_blocks as usize)
            .map_err(|e| JsError::new(&format!("encode_step failed: {e:?}")))?;
        match step {
            SnapshotStep::Progress => Ok(vec![0.0, 0.0]),
            SnapshotStep::Chunk { offset, bytes } => {
                self.pending_chunk = Some(bytes);
                Ok(vec![1.0, offset as f64])
            }
            SnapshotStep::Finished {
                header_bytes,
                total_len,
            } => {
                self.pending_chunk = Some(header_bytes);
                self.snapshot_encoder = None;
                Ok(vec![2.0, total_len as f64])
            }
        }
    }

    /// Transfers out the bytes produced by the most recent `step_snapshot_stream`.
    pub fn take_snapshot_chunk(&mut self) -> Option<Vec<u8>> {
        self.pending_chunk.take()
    }

    /// Concurrent 25 ms input probe during B11 / B12 snapshot streaming: mutates
    /// one cell (triggering single-block CoW split if shared) and encodes the
    /// first `64 × 16` viewport page.
    pub fn probe_edit_and_page(&mut self) -> usize {
        self.step_counter = self.step_counter.wrapping_add(1);
        let row = RowId(u64::from((self.step_counter % 32) + 1));
        let v = 100.0 + f64::from(self.step_counter);
        let _ = self
            .engine
            .edit_cell_raw(SheetId(0), row, ColId(0), &format!("{v}"));
        let wb = self.engine.workbook();
        let Some(sheet) = wb.sheet(SheetId(0)) else {
            return 0;
        };
        let bytes = viewport::encode_page_with_formulas(
            sheet,
            Page { row: 0, col: 0 },
            self.step_counter,
            &mut self.text_scratch,
            |r, c| wb.formula(SheetId(0), r, c).is_some(),
        );
        bytes.len()
    }

    /// Decodes an `RTCLSNAP` v0/v1 snapshot, builds the full dependency graph,
    /// and encodes the first interactive `64 × 16` viewport page (B1 & B12 load).
    pub fn decode_v1_and_first_page(bytes: &[u8]) -> Result<Self, JsError> {
        let decoded = decode_snapshot_any(bytes)
            .map_err(|e| JsError::new(&format!("decode_snapshot_any failed: {e:?}")))?;
        let mut bench = Self::from_engine(decoded.engine);
        let wb = bench.engine.workbook();
        if let Some(sheet) = wb.sheet(SheetId(0)) {
            let page = viewport::encode_page_with_formulas(
                sheet,
                Page { row: 0, col: 0 },
                1,
                &mut bench.text_scratch,
                |r, c| wb.formula(SheetId(0), r, c).is_some(),
            );
            assert!(!page.is_empty());
        }
        Ok(bench)
    }

    /// B12 #3: mutates the shared parameter cell (`$Z$1` at col 25 for 50-col
    /// B12, or `$S$1` at col 18 for 20-col B1) and recalculates all dependent
    /// formulas.
    pub fn step_storage_recalc(&mut self, param_col: u32) -> u32 {
        self.step_counter = self.step_counter.wrapping_add(1);
        let v = 1.0 + f64::from(self.step_counter) * 0.05;
        let _ = self.engine.edit_cell_raw(
            SheetId(0),
            RowId(0),
            ColId(u64::from(param_col)),
            &format!("{v}"),
        );
        self.engine.recalc_all()
    }

    /// Decodes an `RTCLSNAP` snapshot buffer into a live `FormulaEngine` and
    /// returns the primary sheet's live row count.
    pub fn decode_snapshot_rows(bytes: &[u8]) -> Result<u32, JsError> {
        let _ = reticula_core::memory::try_reserve_heap(bytes.len().saturating_mul(16));
        let decoded = decode_snapshot_any(bytes)
            .map_err(|e| JsError::new(&format!("decode_snapshot_any failed: {e:?}")))?;
        let rows = decoded
            .engine
            .workbook()
            .sheet(SheetId(0))
            .map_or(0, Sheet::rows);
        Ok(rows)
    }
}

fn build_dense_grid(rows: u32, cols: u32, formula_rows: u32) -> FormulaEngine {
    let blocks_per_col = (rows as usize).div_ceil(BLOCK_ROWS as usize);
    let param_col = if cols >= 26 { 25_u32 } else { cols - 2 };
    let formula_col = cols - 1;

    let mut columns = Vec::with_capacity(cols as usize);
    for c in 0..cols {
        let mut col_blocks = Vec::with_capacity(blocks_per_col);
        for b in 0..blocks_per_col {
            let base_r = (b as u32) * BLOCK_ROWS;
            let count = (rows - base_r).min(BLOCK_ROWS) as usize;
            let mut words = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
            for (off, word) in words.iter_mut().enumerate().take(count) {
                let r = base_r + (off as u32);
                let num = if c == param_col && r == 0 {
                    1.0
                } else if c == formula_col && r < formula_rows {
                    let bv = f64::from(((r.wrapping_mul(31) ^ 17) % 1000) + 1) * 0.25;
                    let cv = f64::from(((r.wrapping_mul(31) ^ 34) % 1000) + 1) * 0.25;
                    bv * cv + 1.0
                } else {
                    f64::from(((r.wrapping_mul(31) ^ c.wrapping_mul(17)) % 1000) + 1) * 0.25
                };
                *word = Value::Number(num).pack();
            }
            col_blocks.push(Block::dense(words).unwrap());
        }
        columns.push(Column::from_blocks(col_blocks, rows));
    }

    let sheet = Sheet::from_parts(
        SheetId(0),
        String::from("Sheet1"),
        vec![AxisRun::live(IdRun::new(0, rows).unwrap())],
        vec![AxisRun::live(IdRun::new(0, cols).unwrap())],
        SlotMap::with_base(rows),
        SlotMap::with_base(cols),
        columns,
        vec![NumberFormat::General; cols as usize],
        BTreeMap::new(),
        StringTable::new(),
    )
    .expect("valid dense benchmark sheet");

    let mut wb = Workbook::from_sheet(sheet, None);
    for r in 0..formula_rows {
        let row = RowId(u64::from(r));
        let bv = f64::from(((r.wrapping_mul(31) ^ 17) % 1000) + 1) * 0.25;
        let cv = f64::from(((r.wrapping_mul(31) ^ 34) % 1000) + 1) * 0.25;
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
                right: Box::new(cell_expr(RowId(0), ColId(u64::from(param_col)), true)),
            },
        };
        wb.insert_seeded_formula(
            SheetId(0),
            row,
            ColId(u64::from(formula_col)),
            f,
            Value::Number(bv * cv + 1.0),
        );
    }
    FormulaEngine::from_workbook_clean(wb)
}

fn build_string_grid(rows: u32, distinct: u32) -> FormulaEngine {
    let mut strings = StringTable::new();
    let distinct = distinct.max(1);
    let mut ids = Vec::with_capacity(distinct as usize);
    for i in 0..distinct {
        let s = format!("S{i:07}");
        ids.push(strings.intern(&s).expect("intern benchmark string"));
    }

    let blocks_per_col = (rows as usize).div_ceil(BLOCK_ROWS as usize);
    let mut col_blocks = Vec::with_capacity(blocks_per_col);
    for b in 0..blocks_per_col {
        let base_r = (b as u32) * BLOCK_ROWS;
        let count = (rows - base_r).min(BLOCK_ROWS) as usize;
        let mut words = vec![Value::EMPTY_BITS; BLOCK_ROWS as usize];
        for (off, word) in words.iter_mut().enumerate().take(count) {
            let r = base_r + (off as u32);
            let sid = ids[(r % distinct) as usize];
            *word = Value::Text(sid).pack();
        }
        col_blocks.push(Block::dense(words).unwrap());
    }
    let columns = vec![Column::from_blocks(col_blocks, rows)];
    let sheet = Sheet::from_parts(
        SheetId(0),
        String::from("Sheet1"),
        vec![AxisRun::live(IdRun::new(0, rows).unwrap())],
        vec![AxisRun::live(IdRun::new(0, 1).unwrap())],
        SlotMap::with_base(rows),
        SlotMap::with_base(1),
        columns,
        vec![NumberFormat::General],
        BTreeMap::new(),
        strings,
    )
    .expect("valid string benchmark sheet");

    FormulaEngine::from_workbook_clean(Workbook::from_sheet(sheet, None))
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
