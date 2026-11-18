//! WebAssembly bindings: the entry point of the engine worker (`13-m1a.md` 6.1).
//!
//! The engine is driven like a state machine. A command goes in through
//! [`Engine::handle`], which may answer with an event; viewport pages the
//! command made due are then taken out one at a time with
//! [`Engine::take_page`]. Whenever [`Engine::has_pending_recalc`] remains true
//! after a command, the worker yields to the event loop (`setTimeout(0)`) so
//! incoming `Subscribe` or `EditCell` messages run without waiting, and then
//! sends [`EngineCommand::ContinueRecalc`] for the next ~4 ms slice.
//!
//! Everything that crosses the worker boundary as a structured object is
//! defined here and exported to TypeScript via `tsify`, so there is exactly one
//! definition of each type.

use std::collections::{HashSet, VecDeque};

use reticula_core::id::{ColId, IdRun, RowId, SheetId};
use reticula_core::ops::{Op, OpError};
use reticula_core::viewport::{self, PAGE_COLS, PAGE_ROWS, Page, PageRange};
use reticula_formula::determinism;
use reticula_formula::engine::{EditError, FormulaEngine, RecalcStatus};
use reticula_formula::parser::ParseError;
use serde::{Deserialize, Serialize};
use tsify::{Ts, Tsify};
use wasm_bindgen::prelude::*;

#[cfg(feature = "bench")]
pub mod bench;

/// Time budget per recalculation slice in milliseconds (`13-m1a.md` 5.6).
const RECALC_SLICE_BUDGET_MS: f64 = 4.0;
/// Formula count evaluated between monotonic clock checks inside a slice.
const RECALC_CHECK_INTERVAL: usize = 256;
/// Above this many dirty cell notifications in a single wave, re-enqueue all
/// currently subscribed pages directly instead of mapping each cell to a page.
const BULK_DIRTY_PAGE_THRESHOLD: usize = 2_048;

mod wire_u64 {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct U64Visitor;

        impl Visitor<'_> for U64Visitor {
            type Value = u64;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 64-bit unsigned integer as a decimal string or non-negative integer")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse::<u64>().map_err(E::custom)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(v)
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(U64Visitor)
    }
}

mod wire_opt_u64 {
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawOpt {
        Text(String),
        Num(u64),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        let opt = Option::<RawOpt>::deserialize(deserializer)?;
        match opt {
            None => Ok(None),
            Some(RawOpt::Num(n)) => Ok(Some(n)),
            Some(RawOpt::Text(s)) => s.parse::<u64>().map(Some).map_err(serde::de::Error::custom),
        }
    }
}

/// Contiguous run of row or column identifiers (`firstId .. firstId + len`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct WireIdRun {
    /// First 64-bit identifier encoded as an exact decimal string.
    #[serde(with = "wire_u64")]
    #[tsify(type = "string")]
    pub first_id: u64,
    /// Number of consecutive identifiers (`>= 1`).
    pub len: u32,
}

impl From<WireIdRun> for IdRun {
    fn from(w: WireIdRun) -> Self {
        Self {
            first_id: w.first_id,
            len: w.len,
        }
    }
}

/// A message from the main thread to the engine worker (`13-m1a.md` 6.1).
#[derive(Debug, Deserialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EngineCommand {
    /// Replace the document with the seeded 1M-row sample (including parameter
    /// panel `U1:V2` and 100,000 bound `M`-column formulas). Answered by
    /// [`EngineEvent::Ready`].
    OpenSample {
        /// Seed of the sample. The same seed gives the same sheet everywhere.
        seed: u32,
        /// Number of rows, the header row included.
        rows: u32,
    },
    /// Declare the range of cells the grid wants to hold: what it shows plus a
    /// margin around it. Replaces the previous subscription.
    Subscribe {
        /// Target sheet identifier (`0` for the primary sheet).
        #[serde(default)]
        sheet: u32,
        /// A number the grid picks; every page sent for this subscription
        /// carries it.
        epoch: u32,
        /// Position of the first row of the range.
        first_row: u32,
        /// Number of rows in the range.
        row_count: u32,
        /// Position of the first column of the range.
        first_col: u32,
        /// Number of columns in the range.
        col_count: u32,
    },
    /// Query the raw input / formula text and evaluation details of a single
    /// cell (selected cell, formula bar, or `F2`).
    InspectCell {
        /// Sheet identifier.
        sheet: u32,
        /// Row identifier (decimal `u64` string).
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        row_id: u64,
        /// Column identifier (decimal `u64` string).
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        col_id: u64,
    },
    /// Submit raw user input (`raw`) to `(sheet, row_id, col_id)` under E30
    /// type inference and trigger incremental recalculation.
    EditCell {
        /// Sheet identifier.
        sheet: u32,
        /// Row identifier (decimal `u64` string).
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        row_id: u64,
        /// Column identifier (decimal `u64` string).
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        col_id: u64,
        /// Raw input string (`""`, `"=A2*B2"`, `"5%"`, `"2026-11-18"`, ...).
        raw: String,
    },
    /// Clear all cells in the Cartesian product of `rows × cols`.
    ClearCells {
        /// Sheet identifier.
        sheet: u32,
        /// Contiguous row ID runs.
        rows: Vec<WireIdRun>,
        /// Contiguous column ID runs.
        cols: Vec<WireIdRun>,
    },
    /// Insert `count` new rows after `after_row_id` (`null` inserts at the very
    /// top of the sheet, before row 1).
    InsertRows {
        /// Sheet identifier.
        sheet: u32,
        /// Anchor row ID to insert after, or `null` to insert before the first row.
        #[serde(deserialize_with = "wire_opt_u64::deserialize")]
        #[tsify(type = "string | null")]
        after_row_id: Option<u64>,
        /// Number of rows to insert (`>= 1`).
        count: u32,
    },
    /// Delete the specified row ID runs (tombstoning them on the row axis).
    DeleteRows {
        /// Sheet identifier.
        sheet: u32,
        /// Contiguous row ID runs to delete.
        rows: Vec<WireIdRun>,
    },
    /// Advance an in-progress time-sliced recalculation by one ~4 ms budget
    /// slice.
    ContinueRecalc,
    /// Run the determinism self-check. Answered by [`EngineEvent::SelfChecked`].
    SelfCheck,
}

/// A message from the engine worker to the main thread (`13-m1a.md` 6.1).
#[derive(Debug, PartialEq, Serialize, Tsify)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EngineEvent {
    /// A document is open and its cells can be subscribed to.
    Ready {
        /// Facts about the engine.
        info: EngineInfo,
        /// The sheet that was opened.
        sheet: SheetInfo,
    },
    /// Structural edit (`InsertRows` / `DeleteRows`) completed.
    SheetChanged {
        /// Updated sheet dimensions.
        sheet: SheetInfo,
        /// Elapsed time of the structural operation (plus any immediate slice)
        /// in milliseconds.
        action_ms: f64,
        /// Formulas recalculated synchronously within the initial slice.
        recalculated: u32,
    },
    /// Raw formula/value inspection details for a cell.
    CellInspected {
        /// Sheet identifier.
        sheet: u32,
        /// Row identifier (decimal `u64` string).
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        row_id: u64,
        /// Column identifier (decimal `u64` string).
        #[serde(with = "wire_u64")]
        #[tsify(type = "string")]
        col_id: u64,
        /// Canonical A1 address (`"A1"`, `"V1"`).
        address: String,
        /// Raw authoring string (`"=SUM(M2:M100001)"`, `"0%"`, ...).
        raw: String,
        /// Formatted display string (`"123.45"`, `"0.00%"`, ...).
        display: String,
        /// Whether the cell currently holds a bound formula.
        is_formula: bool,
        /// Error literal (`"#DIV/0!"`, `#SPILL!`, `#CYCLE!`) if the cell value is an error.
        error: Option<String>,
    },
    /// Partial progress emitted when recalculation yields at a 4 ms slice boundary.
    RecalcProgress {
        /// Formulas evaluated so far in this recalculation wave.
        done: u32,
        /// Total formulas queued in this wave.
        total: u32,
    },
    /// Incremental recalculation (or value edit) finished.
    RecalcDone {
        /// Total formulas recalculated across the wave (`0` for pure value edits
        /// with no dependents).
        recalculated: u32,
        /// Engine time spent marking dirty cells and evaluating formulas, in ms.
        elapsed_ms: f64,
    },
    /// Command was atomically rejected without mutating workbook state.
    Rejected {
        /// Human-readable rejection reason.
        reason: String,
    },
    /// The determinism self-check has run.
    SelfChecked {
        /// Its result: 16 lowercase hexadecimal digits.
        digest: String,
    },
}

/// Facts about the running engine.
#[derive(Debug, PartialEq, Eq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Version of the engine kernel.
    pub kernel_version: String,
}

/// Facts about an open sheet.
#[derive(Clone, Debug, PartialEq, Serialize, Tsify)]
#[serde(rename_all = "camelCase")]
pub struct SheetInfo {
    /// Sheet identifier (`0` for primary sheet).
    pub sheet: u32,
    /// Number of live rows.
    pub rows: u32,
    /// Number of live columns.
    pub cols: u32,
    /// Total bound formulas in the workbook.
    pub formula_count: u32,
    /// Seed the sheet was generated from (`0` if not from a sample).
    pub seed: u32,
    /// How long generating it took, in milliseconds.
    pub generation_ms: f64,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);

    /// The worker's monotonic clock, in milliseconds.
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;
}

#[inline]
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        performance_now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
    }
}

/// Runs once when the module is instantiated.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
fn start() {
    // A panic in the engine is a bug. Report it where a developer will see it;
    // the worker glue turns the resulting trap into a prompt to reload.
    std::panic::set_hook(Box::new(|info| {
        console_error(&format!("engine panic: {info}"));
    }));
}

/// Reserves `bytes` of linear memory through the Rust global allocator in one
/// shot and immediately releases it into the allocator's free list (`13-m1a.md`
/// 2.3 & 5.2).
#[wasm_bindgen(js_name = reserveForBytes)]
pub fn reserve_for_bytes(bytes: usize) -> bool {
    reticula_core::memory::try_reserve_heap(bytes)
}

/// The engine: one open workbook (`FormulaEngine`) and the grid's viewport
/// subscription to it.
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Engine {
    engine: FormulaEngine,
    seed: u32,
    generation_ms: f64,
    /// Sheet currently subscribed by the grid.
    subscribed_sheet: SheetId,
    /// The pages the grid holds or is about to be sent.
    subscribed: PageRange,
    /// The subscription that pages are currently sent for.
    epoch: u32,
    /// Pages that are due, in the order they will be sent.
    due: VecDeque<Page>,
    /// O(1) membership set keeping `due` free of duplicates.
    due_set: HashSet<Page>,
    /// Accumulated engine milliseconds spent across the active recalculation wave.
    recalc_elapsed_ms: f64,
    /// Scratch space for encoding pages.
    text: String,
}

#[wasm_bindgen]
impl Engine {
    /// Creates an engine with a fresh empty workbook.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Carries out a command and returns the event that answers it, if the
    /// command has one. Afterwards, take pages until there are none left, and
    /// check [`Self::has_pending_recalc`].
    ///
    /// # Errors
    ///
    /// Fails if `command` is not a well-formed command; the engine is left
    /// unchanged.
    pub fn handle(
        &mut self,
        command: &Ts<EngineCommand>,
    ) -> Result<Option<Ts<EngineEvent>>, JsError> {
        let cmd = command.to_rust()?;
        let event = self.handle_command(cmd);
        Ok(event.as_ref().map(Ts::from_rust).transpose()?)
    }

    /// Whether dirty formulas remain queued for subsequent [`EngineCommand::ContinueRecalc`]
    /// slices.
    #[wasm_bindgen(js_name = hasPendingRecalc)]
    #[must_use]
    pub fn has_pending_recalc(&self) -> bool {
        self.engine.has_pending_recalc()
    }

    /// Encodes the next page that is due and hands it over, or returns
    /// nothing when the grid has been sent everything due.
    #[wasm_bindgen(js_name = takePage)]
    pub fn take_page(&mut self) -> Option<Vec<u8>> {
        let page = self.due.pop_front()?;
        self.due_set.remove(&page);
        let sheet_id = self.subscribed_sheet;
        let sheet = self.engine.workbook().sheet(sheet_id)?;
        let engine_ref = &self.engine;
        Some(viewport::encode_page_with_formulas(
            sheet,
            page,
            self.epoch,
            &mut self.text,
            |r, c| engine_ref.has_formula(sheet_id, r, c),
        ))
    }
}

impl Engine {
    /// Pure Rust command handler used both by WASM bindings and unit tests.
    pub fn handle_command(&mut self, command: EngineCommand) -> Option<EngineEvent> {
        match command {
            EngineCommand::OpenSample { seed, rows } => Some(self.open_sample(seed, rows)),
            EngineCommand::Subscribe {
                sheet,
                epoch,
                first_row,
                row_count,
                first_col,
                col_count,
            } => {
                self.subscribe(
                    SheetId(sheet),
                    epoch,
                    (first_row, row_count),
                    (first_col, col_count),
                );
                None
            }
            EngineCommand::InspectCell {
                sheet,
                row_id,
                col_id,
            } => self.inspect_cell(SheetId(sheet), RowId(row_id), ColId(col_id)),
            EngineCommand::EditCell {
                sheet,
                row_id,
                col_id,
                raw,
            } => Some(self.edit_cell(SheetId(sheet), RowId(row_id), ColId(col_id), &raw)),
            EngineCommand::ClearCells { sheet, rows, cols } => {
                Some(self.clear_cells(SheetId(sheet), rows, cols))
            }
            EngineCommand::InsertRows {
                sheet,
                after_row_id,
                count,
            } => Some(self.insert_rows(SheetId(sheet), after_row_id, count)),
            EngineCommand::DeleteRows { sheet, rows } => {
                Some(self.delete_rows(SheetId(sheet), rows))
            }
            EngineCommand::ContinueRecalc => self.continue_recalc(),
            EngineCommand::SelfCheck => {
                let digest = determinism::to_hex(determinism::digest());
                Some(EngineEvent::SelfChecked {
                    digest: String::from_utf8_lossy(&digest).into_owned(),
                })
            }
        }
    }

    fn open_sample(&mut self, seed: u32, rows: u32) -> EngineEvent {
        let started = now_ms();
        self.engine = FormulaEngine::open_sample(u64::from(seed), rows);
        let generation_ms = now_ms() - started;

        self.seed = seed;
        self.generation_ms = generation_ms;
        self.subscribed_sheet = SheetId(0);
        self.subscribed = PageRange::default();
        self.due.clear();
        self.due_set.clear();
        self.recalc_elapsed_ms = 0.0;
        let _ = self.engine.take_dirty_changes();

        EngineEvent::Ready {
            info: EngineInfo {
                kernel_version: reticula_core::KERNEL_VERSION.to_owned(),
            },
            sheet: self.sheet_info(SheetId(0)),
        }
    }

    fn sheet_info(&self, sheet_id: SheetId) -> SheetInfo {
        let (rows, cols) = self
            .engine
            .workbook()
            .sheet(sheet_id)
            .map_or((0, 0), |s| (s.rows(), s.cols()));
        let formula_count = u32::try_from(self.engine.workbook().formulas().len()).unwrap_or(0);
        SheetInfo {
            sheet: sheet_id.0,
            rows,
            cols,
            formula_count,
            seed: self.seed,
            generation_ms: self.generation_ms,
        }
    }

    fn subscribe(&mut self, sheet_id: SheetId, epoch: u32, rows: (u32, u32), cols: (u32, u32)) {
        let dims = self
            .engine
            .workbook()
            .sheet(sheet_id)
            .map_or((0, 0), |s| (s.rows(), s.cols()));
        let wanted = PageRange::covering(dims, rows, cols);
        let sheet_changed = sheet_id != self.subscribed_sheet;
        let held = if sheet_changed {
            PageRange::default()
        } else {
            self.subscribed
        };

        if sheet_changed {
            self.due.clear();
            self.due_set.clear();
        } else {
            self.due.retain(|page| {
                let keep = wanted.contains(*page);
                if !keep {
                    self.due_set.remove(page);
                }
                keep
            });
        }

        for page in wanted.pages() {
            if !held.contains(page) && self.due_set.insert(page) {
                self.due.push_back(page);
            }
        }
        self.subscribed_sheet = sheet_id;
        self.subscribed = wanted;
        self.epoch = epoch;
    }

    fn enqueue_page(&mut self, page: Page) {
        if self.subscribed.contains(page) && self.due_set.insert(page) {
            self.due.push_back(page);
        }
    }

    fn flush_dirty_pages(&mut self) {
        let dirty = self.engine.take_dirty_changes();
        if self.subscribed.is_empty() {
            return;
        }
        if dirty.structural || dirty.cells.len() > BULK_DIRTY_PAGE_THRESHOLD {
            let pages: Vec<Page> = self.subscribed.pages().collect();
            for page in pages {
                self.enqueue_page(page);
            }
            return;
        }
        let target_sheet = self.subscribed_sheet;
        let mut to_enqueue = Vec::new();
        if let Some(sheet) = self.engine.workbook().sheet(target_sheet) {
            for (s_id, row_id, col_id) in dirty.cells {
                if s_id != target_sheet {
                    continue;
                }
                if let (Some(r_pos), Some(c_pos)) = (
                    sheet.row_axis().id_to_pos(row_id.0),
                    sheet.col_axis().id_to_pos(col_id.0),
                ) {
                    let page = Page {
                        row: r_pos / PAGE_ROWS,
                        col: c_pos / PAGE_COLS,
                    };
                    if self.subscribed.contains(page) {
                        to_enqueue.push(page);
                    }
                }
            }
        }
        for page in to_enqueue {
            self.enqueue_page(page);
        }
    }

    fn inspect_cell(&self, sheet_id: SheetId, row_id: RowId, col_id: ColId) -> Option<EngineEvent> {
        let info = self.engine.inspect_cell(sheet_id, row_id, col_id)?;
        Some(EngineEvent::CellInspected {
            sheet: sheet_id.0,
            row_id: row_id.0,
            col_id: col_id.0,
            address: info.address,
            raw: info.raw,
            display: info.display,
            is_formula: info.is_formula,
            error: info.error,
        })
    }

    fn edit_cell(
        &mut self,
        sheet_id: SheetId,
        row_id: RowId,
        col_id: ColId,
        raw: &str,
    ) -> EngineEvent {
        let started = now_ms();
        if let Err(err) = self.engine.edit_cell_raw(sheet_id, row_id, col_id, raw) {
            return EngineEvent::Rejected {
                reason: format_edit_error(err),
            };
        }
        let apply_ms = now_ms() - started;
        self.recalc_elapsed_ms += apply_ms;
        self.flush_dirty_pages();

        if !self.engine.has_pending_recalc() {
            let elapsed_ms = core::mem::take(&mut self.recalc_elapsed_ms);
            return EngineEvent::RecalcDone {
                recalculated: 0,
                elapsed_ms,
            };
        }
        self.step_recalc_slice()
    }

    fn clear_cells(
        &mut self,
        sheet_id: SheetId,
        rows: Vec<WireIdRun>,
        cols: Vec<WireIdRun>,
    ) -> EngineEvent {
        let started = now_ms();
        let op = Op::ClearRange {
            sheet: sheet_id,
            rows: rows.into_iter().map(IdRun::from).collect(),
            cols: cols.into_iter().map(IdRun::from).collect(),
        };
        if let Err(err) = self.engine.apply_op(op) {
            return EngineEvent::Rejected {
                reason: format_op_error(err),
            };
        }
        self.recalc_elapsed_ms += now_ms() - started;
        self.flush_dirty_pages();

        if !self.engine.has_pending_recalc() {
            let elapsed_ms = core::mem::take(&mut self.recalc_elapsed_ms);
            return EngineEvent::RecalcDone {
                recalculated: 0,
                elapsed_ms,
            };
        }
        self.step_recalc_slice()
    }

    fn insert_rows(
        &mut self,
        sheet_id: SheetId,
        after_row_id: Option<u64>,
        count: u32,
    ) -> EngineEvent {
        if count == 0 {
            return EngineEvent::Rejected {
                reason: String::from("Row count must be at least 1."),
            };
        }
        let started = now_ms();
        let Some(ids) = self.engine.allocator_mut().alloc_run(count) else {
            return EngineEvent::Rejected {
                reason: String::from("Identifier counter exhausted."),
            };
        };
        let op = Op::InsertRows {
            sheet: sheet_id,
            after: after_row_id.map(RowId),
            ids,
        };
        if let Err(err) = self.engine.apply_op(op) {
            return EngineEvent::Rejected {
                reason: format_op_error(err),
            };
        }
        let mut recalculated = 0;
        if self.engine.has_pending_recalc() {
            let slice_start = now_ms();
            match self.engine.recalc_with_budget(RECALC_CHECK_INTERVAL, |_| {
                now_ms() - slice_start >= RECALC_SLICE_BUDGET_MS
            }) {
                RecalcStatus::Complete { recalculated: n } => {
                    recalculated = n;
                    self.recalc_elapsed_ms = 0.0;
                }
                RecalcStatus::InProgress { done, .. } => {
                    recalculated = done;
                    self.recalc_elapsed_ms += now_ms() - slice_start;
                }
            }
        }
        let action_ms = now_ms() - started;
        // Recompute clipped subscription bounds against updated row count and push dirty pages.
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn delete_rows(&mut self, sheet_id: SheetId, rows: Vec<WireIdRun>) -> EngineEvent {
        if rows.is_empty() {
            return EngineEvent::Rejected {
                reason: String::from("Select at least one row to delete."),
            };
        }
        let started = now_ms();
        let op = Op::DeleteRows {
            sheet: sheet_id,
            ids: rows.into_iter().map(IdRun::from).collect(),
        };
        if let Err(err) = self.engine.apply_op(op) {
            return EngineEvent::Rejected {
                reason: format_op_error(err),
            };
        }
        let mut recalculated = 0;
        if self.engine.has_pending_recalc() {
            let slice_start = now_ms();
            match self.engine.recalc_with_budget(RECALC_CHECK_INTERVAL, |_| {
                now_ms() - slice_start >= RECALC_SLICE_BUDGET_MS
            }) {
                RecalcStatus::Complete { recalculated: n } => {
                    recalculated = n;
                    self.recalc_elapsed_ms = 0.0;
                }
                RecalcStatus::InProgress { done, .. } => {
                    recalculated = done;
                    self.recalc_elapsed_ms += now_ms() - slice_start;
                }
            }
        }
        let action_ms = now_ms() - started;
        self.refresh_subscription_extent();
        self.flush_dirty_pages();
        EngineEvent::SheetChanged {
            sheet: self.sheet_info(sheet_id),
            action_ms,
            recalculated,
        }
    }

    fn refresh_subscription_extent(&mut self) {
        if self.subscribed.is_empty() {
            return;
        }
        let dims = self
            .engine
            .workbook()
            .sheet(self.subscribed_sheet)
            .map_or((0, 0), |s| (s.rows(), s.cols()));
        let pages: Vec<Page> = self.subscribed.pages().collect();
        if let (Some(first), Some(last)) = (pages.first(), pages.last()) {
            let first_row = first.row.saturating_mul(PAGE_ROWS);
            let row_count =
                (last.row.saturating_add(1).saturating_sub(first.row)).saturating_mul(PAGE_ROWS);
            let first_col = first.col.saturating_mul(PAGE_COLS);
            let col_count =
                (last.col.saturating_add(1).saturating_sub(first.col)).saturating_mul(PAGE_COLS);
            self.subscribed =
                PageRange::covering(dims, (first_row, row_count), (first_col, col_count));
        }
    }

    fn continue_recalc(&mut self) -> Option<EngineEvent> {
        if !self.engine.has_pending_recalc() {
            return None;
        }
        Some(self.step_recalc_slice())
    }

    fn step_recalc_slice(&mut self) -> EngineEvent {
        let slice_start = now_ms();
        let status = self.engine.recalc_with_budget(RECALC_CHECK_INTERVAL, |_| {
            now_ms() - slice_start >= RECALC_SLICE_BUDGET_MS
        });
        let slice_ms = now_ms() - slice_start;
        self.recalc_elapsed_ms += slice_ms;
        self.flush_dirty_pages();

        match status {
            RecalcStatus::InProgress { done, total } => EngineEvent::RecalcProgress { done, total },
            RecalcStatus::Complete { recalculated } => {
                let elapsed_ms = core::mem::take(&mut self.recalc_elapsed_ms);
                EngineEvent::RecalcDone {
                    recalculated,
                    elapsed_ms,
                }
            }
        }
    }
}

fn format_edit_error(err: EditError) -> String {
    match err {
        EditError::Parse(p) => match p {
            ParseError::MissingEquals => String::from("Formula cannot be empty after '='."),
            ParseError::FormulaTooLong => {
                String::from("Formula exceeds maximum length of 8,192 characters.")
            }
            ParseError::NestingTooDeep => {
                String::from("Formula exceeds maximum nesting depth of 64 levels.")
            }
            ParseError::TooManyArguments => {
                String::from("Function call exceeds maximum of 255 arguments.")
            }
            ParseError::Lex(lex) => format!("Invalid token in formula ({lex:?})."),
            ParseError::UnexpectedToken => String::from("Formula syntax error."),
            ParseError::InvalidRange => String::from("Invalid cell or range reference."),
            ParseError::InvalidArrayLiteral => {
                String::from("Invalid array literal (must be rectangular constant literals).")
            }
        },
        EditError::Op(op) => format_op_error(op),
    }
}

fn format_op_error(err: OpError) -> String {
    match err {
        OpError::SheetNotFound => String::from("Target sheet does not exist."),
        OpError::TooManySheets => String::from("Maximum sheet count (256) reached."),
        OpError::TooManyEdits => String::from("Single operation exceeds 10,000 cell edits."),
        OpError::DuplicateCellTarget => String::from("Operation contains duplicate target cells."),
        OpError::TargetNotFound => String::from("Target row or column no longer exists."),
        OpError::Axis(axis_err) => format!("Axis limit or target error ({axis_err:?})."),
        OpError::NonFiniteNumber => String::from("Numeric value must be finite."),
        OpError::TextTooLong => String::from("Cell text exceeds 32,767 UTF-16 code units."),
        OpError::FormulaTooLong => {
            String::from("Formula exceeds maximum length of 8,192 characters.")
        }
        OpError::InvalidFormula => String::from("Formula failed validation."),
        OpError::StringPoolExhausted => String::from("String table capacity exhausted."),
        OpError::MemoryLimitExceeded => {
            String::from("Local workbook memory limit (1 GiB) exceeded.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_state_machine_handles_sample_edit_inspect_insert_delete_and_dirty_pages() {
        let mut eng = Engine::new();
        let ready = eng
            .handle_command(EngineCommand::OpenSample {
                seed: 20261117,
                rows: 500,
            })
            .unwrap();
        let EngineEvent::Ready { sheet, .. } = ready else {
            panic!("expected Ready");
        };
        assert_eq!(sheet.rows, 500);
        assert_eq!(sheet.cols, 22);
        assert_eq!(sheet.formula_count, 500); // 499 M-col formulas + V2 SUM

        // Subscribe to top-left 64 rows x 22 cols (2 pages: (0,0) and (0,1)).
        assert!(
            eng.handle_command(EngineCommand::Subscribe {
                sheet: 0,
                epoch: 1,
                first_row: 0,
                row_count: 64,
                first_col: 0,
                col_count: 22,
            })
            .is_none()
        );
        let p0 = eng.take_page().expect("page (0,0)");
        let p1 = eng.take_page().expect("page (0,1)");
        assert!(eng.take_page().is_none());
        assert!(!p0.is_empty() && !p1.is_empty());

        // Inspect V2 (`row_id = 1, col_id = 21`).
        let inspected = eng
            .handle_command(EngineCommand::InspectCell {
                sheet: 0,
                row_id: 1,
                col_id: 21,
            })
            .unwrap();
        let EngineEvent::CellInspected {
            address,
            raw,
            is_formula,
            ..
        } = inspected
        else {
            panic!("expected CellInspected");
        };
        assert_eq!(address, "V2");
        assert_eq!(raw, "=SUM(M2:M500)");
        assert!(is_formula);

        // Edit V1 (`row_id = 0, col_id = 21`) to `5%` -> triggers recalculation & dirty pages!
        let mut ev = eng
            .handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: 0,
                col_id: 21,
                raw: String::from("5%"),
            })
            .unwrap();
        while matches!(ev, EngineEvent::RecalcProgress { .. }) {
            ev = eng.handle_command(EngineCommand::ContinueRecalc).unwrap();
        }
        let EngineEvent::RecalcDone { recalculated, .. } = ev else {
            panic!("expected RecalcDone, got {ev:?}");
        };
        assert_eq!(recalculated, 500);
        // Both subscribed pages ((0,0) has M2..M64; (0,1) has V1 & V2) must be re-queued!
        assert!(eng.take_page().is_some());
        assert!(eng.take_page().is_some());
        assert!(eng.take_page().is_none());

        // Insert 1 row at the very top (`after_row_id: None`) -> rows becomes 501, V2 becomes V3!
        let ins = eng
            .handle_command(EngineCommand::InsertRows {
                sheet: 0,
                after_row_id: None,
                count: 1,
            })
            .unwrap();
        let EngineEvent::SheetChanged { sheet: s2, .. } = ins else {
            panic!("expected SheetChanged");
        };
        assert_eq!(s2.rows, 501);
        while eng.take_page().is_some() {}

        let inspected_after = eng
            .handle_command(EngineCommand::InspectCell {
                sheet: 0,
                row_id: 1,
                col_id: 21,
            })
            .unwrap();
        let EngineEvent::CellInspected {
            address: addr2,
            raw: raw2,
            ..
        } = inspected_after
        else {
            panic!("expected CellInspected");
        };
        assert_eq!(addr2, "V3");
        assert_eq!(raw2, "=SUM(M3:M501)");

        // Malformed formula is rejected without mutating state.
        let rej = eng
            .handle_command(EngineCommand::EditCell {
                sheet: 0,
                row_id: 0,
                col_id: 0,
                raw: String::from("=SUM(1,"),
            })
            .unwrap();
        assert!(matches!(rej, EngineEvent::Rejected { .. }));
    }
}
